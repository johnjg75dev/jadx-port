//! Post-structuring clean-up passes.
//!
//! jadx runs an equivalent set of rewrites inside its region processors
//! (`RegionExtractHelper` for ternaries, `InlineSingleVariableUsageVisitor` for
//! single-use variables, `ProcessVarAssigns` for `x = x + 1`,
//! `CodeWriter.removeExtraLabels` for labels). Here they are plain recursive
//! rewrites over the statement tree, applied in a fixed order:
//!
//! 1. [`strip_trailing_return`] -- a `return;` at the end of a void body is noise.
//! 2. [`fold_increment`] -- `x = x + 1` becomes `x++`.
//! 3. [`inline_temps`] -- a spilled stack temporary used once is inlined back.
//! 4. [`elide_dead_labels`] -- labels nothing jumps to, and `goto`s that only skip
//!    to the next statement, are dropped.
//!
//! The passes work on *names* rather than slot indices, because the structurer
//! may duplicate statements and `Stmt::LocalDef` carries no index; local names are
//! unique per slot after `emulate::make_names_unique`.

use std::collections::HashSet;

use crate::java::insn::BinOp;

use super::ir::{Expr, Stmt};

/// What the passes changed; surfaced through the FFI and printed as a comment by
/// `--comments-level=debug`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassReport {
	pub returns_stripped: usize,
	pub increments_folded: usize,
	pub temps_inlined: usize,
	pub labels_removed: usize,
	pub gotos_removed: usize,
}

impl PassReport {
	pub fn is_empty(&self) -> bool {
		self.returns_stripped + self.increments_folded + self.temps_inlined + self.labels_removed + self.gotos_removed == 0
	}

	pub fn summary(&self) -> String {
		format!(
			"returns-stripped={} increments={} inlined-temps={} labels={} gotos={}",
			self.returns_stripped, self.increments_folded, self.temps_inlined, self.labels_removed, self.gotos_removed
		)
	}
}

/// Run every pass in order.
pub fn optimize_body(body: &mut Vec<Stmt>, synthetic_names: &HashSet<String>) -> PassReport {
	let mut rep = PassReport::default();
	rep.returns_stripped = strip_trailing_return(body);
	rep.increments_folded = fold_increment(body);
	rep.temps_inlined = inline_temps(body, synthetic_names);
	let (l, g) = elide_dead_labels(body);
	rep.labels_removed = l;
	rep.gotos_removed = g;
	rep
}

/// Remove a `return;` that is the last statement of a body.
pub fn strip_trailing_return(body: &mut Vec<Stmt>) -> usize {
	let mut removed = 0usize;
	walk_lists(body, &mut |list: &mut Vec<Stmt>| {
		if let Some(i) = list.iter().rposition(|s| !is_decoration(s)) {
			if matches!(list[i], Stmt::Return { value: None }) {
				list.remove(i);
				removed += 1;
			}
		}
	});
	removed
}

/// `x = x + 1;` -> `x++;`, `x = x + k;` -> `x += k;` (jadx: `ProcessVarAssigns`).
pub fn fold_increment(body: &mut Vec<Stmt>) -> usize {
	let mut folded = 0usize;
	fold_list(body, &mut folded);
	folded
}

fn fold_list(list: &mut Vec<Stmt>, folded: &mut usize) {
	for i in 0..list.len() {
		if let Some(repl) = fold_candidate(&list[i]) {
			list[i] = repl;
			*folded += 1;
			continue;
		}
		descend_lists(&mut list[i], &mut |inner| fold_list(inner, folded));
	}
}

fn fold_candidate(s: &Stmt) -> Option<Stmt> {
	if let Stmt::Assign { target, value } = s {
		if let (Expr::Local { index, name, ty }, Expr::Bin { op: BinOp::Add, a, b }) = (&**target, &**value) {
			if matches!(**a, Expr::Local { index: i, .. } if i == *index) {
				let delta = b.as_int_const()?;
				return Some(Stmt::ExprStmt { expr: Expr::Inc { index: *index, name: name.clone(), delta, pre: false, ty: ty.clone() } });
			}
		}
	}
	None
}

/// Inline temporaries that are written once and read once, when both happen in the
/// same statement list and the value is side-effect free.
///
/// Spill temps that survive a control-flow join are deliberately *not* inlined:
/// that would mean duplicating the value expression into every predecessor, which
/// needs the reaching-definition dataflow jadx gets from its SSA form. They stay
/// as `int v$1;` plus one assignment per branch -- valid, if slightly verbose.
pub fn inline_temps(body: &mut Vec<Stmt>, names: &HashSet<String>) -> usize {
	if names.is_empty() {
		return 0;
	}
	let mut inlined = 0usize;
	inline_list(body, names, &mut inlined);
	inlined
}

fn inline_list(list: &mut Vec<Stmt>, names: &HashSet<String>, inlined: &mut usize) {
	for s in list.iter_mut() {
		descend_lists(s, &mut |inner| inline_list(inner, names, inlined));
	}
	while try_inline_once(list, names) {
		*inlined += 1;
	}
}

fn try_inline_once(list: &mut Vec<Stmt>, names: &HashSet<String>) -> bool {
	let mut found: Option<(usize, String, Expr)> = None;
	for (idx, s) in list.iter().enumerate() {
		let (name, value) = match s {
			Stmt::Assign { target, value } => match &**target {
				Expr::Local { name, .. } if names.contains(name) => (name.clone(), (**value).clone()),
				_ => continue,
			},
			_ => continue,
		};
		if value.has_side_effects() {
			continue;
		}
		// one write and one read in the whole body, the read after the write
		let reads_before = list[..=idx].iter().map(|o| occurrences(o, &name).0).sum::<usize>();
		let (reads, writes) = occurrences_all(list, &name);
		if writes != 1 || reads != 1 || reads_before != 0 {
			continue;
		}
		let use_at = match (idx + 1..list.len()).find(|j| occurrences(&list[*j], &name).0 > 0) {
			Some(j) => j,
			None => continue,
		};
		// nothing the value reads may be overwritten in between
		let touched = local_names_read(&value);
		let blocked = list[idx + 1..use_at].iter().any(|o| touched.iter().any(|t| occurrences(o, t).1 > 0));
		if blocked {
			continue;
		}
		found = Some((idx, name, value));
		break;
	}
	if let Some((idx, name, value)) = found {
		substitute(&mut list[idx + 1..], &name, &value);
		list.remove(idx);
		return true;
	}
	false
}

/// Drop `LBL_n:` markers nothing jumps to and `goto LBL_n;` that only skips to
/// the next statement. Iterated until stable: removing a `goto` can make its
/// target label dead, and vice versa.
pub fn elide_dead_labels(body: &mut Vec<Stmt>) -> (usize, usize) {
	let mut labels_removed = 0usize;
	let mut gotos_removed = 0usize;
	for _ in 0..16 {
		let used = label_targets(body);
		let mut changed = false;
		drop_dead_labels(body, &used, &mut labels_removed, &mut changed);
		drop_fallthrough_gotos(body, &mut gotos_removed, &mut changed);
		if !changed {
			break;
		}
	}
	(labels_removed, gotos_removed)
}

fn label_targets(list: &[Stmt]) -> HashSet<u32> {
	let mut out: HashSet<u32> = HashSet::new();
	collect_targets(list, &mut out);
	out
}

fn collect_targets(list: &[Stmt], out: &mut HashSet<u32>) {
	for s in list {
		match s {
			Stmt::Goto { id } => {
				out.insert(*id);
			}
			Stmt::Break { id: Some(id) } | Stmt::Continue { id: Some(id) } => {
				out.insert(*id);
			}
			_ => {}
		}
		descend_lists_ref(s, &mut |inner| collect_targets(inner, out));
	}
}

fn drop_dead_labels(list: &mut Vec<Stmt>, used: &HashSet<u32>, removed: &mut usize, changed: &mut bool) {
	let mut i = 0usize;
	while i < list.len() {
		if let Stmt::Label { id } = list[i] {
			if !used.contains(&id) {
				list.remove(i);
				*removed += 1;
				*changed = true;
				continue;
			}
		}
		descend_lists(&mut list[i], &mut |inner| drop_dead_labels(inner, used, removed, changed));
		i += 1;
	}
}

fn drop_fallthrough_gotos(list: &mut Vec<Stmt>, removed: &mut usize, changed: &mut bool) {
	let mut i = 0usize;
	while i + 1 < list.len() {
		let target = match &list[i] {
			Stmt::Goto { id } => Some(*id),
			_ => None,
		};
		if let Some(id) = target {
			if matches!(list[i + 1], Stmt::Label { id: l } if l == id) {
				list.remove(i);
				*removed += 1;
				*changed = true;
				continue;
			}
		}
		descend_lists(&mut list[i], &mut |inner| drop_fallthrough_gotos(inner, removed, changed));
		i += 1;
	}
}

// --- generic helpers ---------------------------------------------------------

/// Statements that carry no control flow and can be skipped when looking for the
/// last real statement of a region.
fn is_decoration(s: &Stmt) -> bool {
	matches!(s, Stmt::Comment { .. } | Stmt::LineNumber { .. } | Stmt::Label { .. })
}

/// `(reads, writes)` of `name` inside one statement, including nested regions.
fn occurrences(s: &Stmt, name: &str) -> (usize, usize) {
	let mut r = 0usize;
	let mut w = 0usize;
	occ_stmt(s, name, &mut r, &mut w);
	(r, w)
}

fn occurrences_all(list: &[Stmt], name: &str) -> (usize, usize) {
	let mut r = 0usize;
	let mut w = 0usize;
	for s in list {
		occ_stmt(s, name, &mut r, &mut w);
	}
	(r, w)
}

fn occ_stmt(s: &Stmt, name: &str, r: &mut usize, w: &mut usize) {
	match s {
		Stmt::LocalDef { name: n, init, .. } => {
			if n == name {
				*w += 1;
			}
			if let Some(e) = init {
				occ_expr(e, name, r, w);
			}
		}
		Stmt::Assign { target, value } => {
			match &**target {
				Expr::Local { name: n, .. } if n == name => *w += 1,
				other => occ_expr(other, name, r, w),
			}
			occ_expr(value, name, r, w);
		}
		Stmt::ExprStmt { expr } | Stmt::Throw { value: expr } => occ_expr(expr, name, r, w),
		Stmt::Return { value } => {
			if let Some(v) = value {
				occ_expr(v, name, r, w);
			}
		}
		Stmt::If { cond, then_body, else_body, .. } => {
			occ_expr(cond, name, r, w);
			for b in then_body.iter().chain(else_body.iter()) {
				occ_stmt(b, name, r, w);
			}
		}
		Stmt::While { cond, body, .. } => {
			if let Some(c) = cond {
				occ_expr(c, name, r, w);
			}
			for b in body {
				occ_stmt(b, name, r, w);
			}
		}
		Stmt::Switch { subject, cases, default_body, .. } => {
			occ_expr(subject, name, r, w);
			for c in cases {
				for b in &c.body {
					occ_stmt(b, name, r, w);
				}
			}
			for b in default_body {
				occ_stmt(b, name, r, w);
			}
		}
		Stmt::TryCatch { try_body, catches, finally_body } => {
			for b in try_body.iter().chain(finally_body.iter().flatten()).chain(catches.iter().flat_map(|c| c.body.iter())) {
				occ_stmt(b, name, r, w);
			}
		}
		Stmt::Label { .. } | Stmt::Goto { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
		Stmt::Comment { .. } | Stmt::LineNumber { .. } => {}
	}
}

fn occ_expr(e: &Expr, name: &str, r: &mut usize, w: &mut usize) {
	if matches!(e, Expr::Inc { name: n, .. } if n == name) {
		*w += 1;
	}
	for n in local_names_read(e) {
		if n == name {
			*r += 1;
		}
	}
}

/// The distinct local names an expression reads.
fn local_names_read(e: &Expr) -> Vec<String> {
	let mut out: Vec<String> = Vec::new();
	collect_locals(e, &mut out);
	out.sort();
	out.dedup();
	out
}

fn collect_locals(e: &Expr, out: &mut Vec<String>) {
	match e {
		Expr::Local { name, .. } | Expr::Inc { name, .. } => out.push(name.clone()),
		Expr::Const(_) | Expr::Raw { .. } => {}
		Expr::Field { obj: Some(o), .. } => collect_locals(o, out),
		Expr::Field { obj: None, .. } => {}
		Expr::Length { a } | Expr::Not { a } | Expr::Neg { a, .. } | Expr::Cast { a, .. } | Expr::InstanceOf { a, .. } => {
			collect_locals(a, out)
		}
		Expr::Array { arr, index, .. } => {
			collect_locals(arr, out);
			collect_locals(index, out);
		}
		Expr::Bin { a, b, .. } | Expr::Shift { a, b, .. } | Expr::Bit { a, b, .. } | Expr::Cmp { a, b, .. } | Expr::Cmp3 { a, b, .. } => {
			collect_locals(a, out);
			collect_locals(b, out);
		}
		Expr::Invoke { obj, args, .. } => {
			if let Some(o) = obj {
				collect_locals(o, out);
			}
			for a in args {
				collect_locals(a, out);
			}
		}
		Expr::New { args, dims, .. } => {
			for a in args.iter().chain(dims.iter()) {
				collect_locals(a, out);
			}
		}
		Expr::Ternary { cond, a, b, .. } => {
			collect_locals(cond, out);
			collect_locals(a, out);
			collect_locals(b, out);
		}
	}
}

/// Replace reads of `name` by `value` in a statement list.
fn substitute(list: &mut [Stmt], name: &str, value: &Expr) {
	for s in list.iter_mut() {
		walk_expr_mut(s, &mut |e| {
			if let Expr::Local { name: n, .. } = e {
				if n == name {
					*e = value.clone();
				}
			}
		});
	}
}

/// Apply `f` to every nested statement list of `s`.
fn descend_lists(s: &mut Stmt, f: &mut dyn FnMut(&mut Vec<Stmt>)) {
	match s {
		Stmt::If { then_body, else_body, .. } => {
			f(then_body);
			f(else_body);
		}
		Stmt::While { body, .. } => f(body),
		Stmt::Switch { cases, default_body, .. } => {
			for c in cases.iter_mut() {
				f(&mut c.body);
			}
			f(default_body);
		}
		Stmt::TryCatch { try_body, catches, finally_body } => {
			f(try_body);
			for c in catches.iter_mut() {
				f(&mut c.body);
			}
			if let Some(fb) = finally_body {
				f(fb);
			}
		}
		_ => {}
	}
}

fn descend_lists_ref(s: &Stmt, f: &mut dyn FnMut(&Vec<Stmt>)) {
	match s {
		Stmt::If { then_body, else_body, .. } => {
			f(then_body);
			f(else_body);
		}
		Stmt::While { body, .. } => f(body),
		Stmt::Switch { cases, default_body, .. } => {
			for c in cases {
				f(&c.body);
			}
			f(default_body);
		}
		Stmt::TryCatch { try_body, catches, finally_body } => {
			f(try_body);
			for c in catches {
				f(&c.body);
			}
			if let Some(fb) = finally_body {
				f(fb);
			}
		}
		_ => {}
	}
}

/// Apply `f` to `list` and to every list nested inside it.
fn walk_lists(list: &mut Vec<Stmt>, f: &mut dyn FnMut(&mut Vec<Stmt>)) {
	f(list);
	for s in list.iter_mut() {
		descend_lists(s, &mut |inner| walk_lists(inner, f));
	}
}

/// Apply `f` to every expression reachable from `s`, parents before children.
fn walk_expr_mut(s: &mut Stmt, f: &mut dyn FnMut(&mut Expr)) {
	fn top(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
		f(e);
		walk_expr_in(e, f);
	}
	match s {
		Stmt::LocalDef { init, .. } => {
			if let Some(e) = init.as_mut() {
				top(e, f);
			}
		}
		Stmt::Assign { target, value } => {
			top(target, f);
			top(value, f);
		}
		Stmt::ExprStmt { expr } => top(expr, f),
		Stmt::Throw { value } => top(value, f),
		Stmt::Return { value } => {
			if let Some(v) = value.as_mut() {
				top(v, f);
			}
		}
		Stmt::If { cond, then_body, else_body, .. } => {
			top(cond, f);
			for b in then_body.iter_mut().chain(else_body.iter_mut()) {
				walk_expr_mut(b, f);
			}
		}
		Stmt::While { cond, body, .. } => {
			if let Some(c) = cond.as_mut() {
				top(c, f);
			}
			for b in body.iter_mut() {
				walk_expr_mut(b, f);
			}
		}
		Stmt::Switch { subject, cases, default_body, .. } => {
			top(subject, f);
			for c in cases.iter_mut() {
				for b in c.body.iter_mut() {
					walk_expr_mut(b, f);
				}
			}
			for b in default_body.iter_mut() {
				walk_expr_mut(b, f);
			}
		}
		Stmt::TryCatch { try_body, catches, finally_body } => {
			for b in try_body.iter_mut().chain(finally_body.iter_mut().flatten()) {
				walk_expr_mut(b, f);
			}
			for c in catches.iter_mut() {
				for b in c.body.iter_mut() {
					walk_expr_mut(b, f);
				}
			}
		}
		Stmt::Label { .. } | Stmt::Goto { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
		Stmt::Comment { .. } | Stmt::LineNumber { .. } => {}
	}
}

fn walk_expr_in(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
	fn top(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
		f(e);
		walk_expr_in(e, f);
	}
	match e {
		Expr::Field { obj: Some(o), .. } => top(o, f),
		Expr::Field { obj: None, .. } => {}
		Expr::Length { a } | Expr::Not { a } | Expr::Neg { a, .. } | Expr::Cast { a, .. } | Expr::InstanceOf { a, .. } => top(a, f),
		Expr::Array { arr, index, .. } => {
			top(arr, f);
			top(index, f);
		}
		Expr::Bin { a, b, .. } | Expr::Shift { a, b, .. } | Expr::Bit { a, b, .. } | Expr::Cmp { a, b, .. } | Expr::Cmp3 { a, b, .. } => {
			top(a, f);
			top(b, f);
		}
		Expr::Invoke { obj, args, .. } => {
			if let Some(o) = obj.as_mut() {
				top(o, f);
			}
			for a in args.iter_mut() {
				top(a, f);
			}
		}
		Expr::New { args, dims, .. } => {
			for a in args.iter_mut().chain(dims.iter_mut()) {
				top(a, f);
			}
		}
		Expr::Ternary { cond, a, b, .. } => {
			top(cond, f);
			top(a, f);
			top(b, f);
		}
		_ => {}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::java::const_pool::Cst;
	use crate::types::JType;

	fn local(name: &str, index: u32) -> Expr {
		Expr::Local { index, name: name.to_string(), ty: JType::Int }
	}

	fn assign(name: &str, index: u32, value: Expr) -> Stmt {
		Stmt::Assign { target: Box::new(local(name, index)), value: Box::new(value) }
	}

	#[test]
	fn trailing_return_is_removed_everywhere() {
		let mut body = vec![
			Stmt::ExprStmt { expr: local("a", 1) },
			Stmt::If { cond: local("a", 1), then_body: vec![Stmt::Return { value: None }], else_body: Vec::new(), has_else: false },
			Stmt::Return { value: None },
		];
		let n = strip_trailing_return(&mut body);
		assert_eq!(n, 2, "both the nested and the outer trailing return go away");
		assert!(matches!(body[1], Stmt::If { ref then_body, .. } if then_body.is_empty()));
	}

	#[test]
	fn plus_one_folds_into_increment() {
		let mut body = vec![assign("i", 1, Expr::Bin { op: BinOp::Add, a: Box::new(local("i", 1)), b: Box::new(Expr::Const(Cst::Int(1))), ty: JType::Int })];
		assert_eq!(fold_increment(&mut body), 1);
		match &body[0] {
			Stmt::ExprStmt { expr: Expr::Inc { name, delta, pre, .. } } => {
				assert_eq!(name, "i");
				assert_eq!(*delta, 1);
				assert!(!pre);
			}
			other => panic!("expected i++, got {:?}", other),
		}
		// `x = x + y` is left alone
		let mut keep = vec![assign("i", 1, Expr::Bin { op: BinOp::Add, a: Box::new(local("i", 1)), b: Box::new(local("y", 2)), ty: JType::Int })];
		assert_eq!(fold_increment(&mut keep), 0);
	}

	#[test]
	fn temp_used_once_in_the_same_block_is_inlined() {
		let names: HashSet<String> = ["v$1".to_string()].into_iter().collect();
		let mut body = vec![
			assign("v$1", 5, local("a", 1)),
			Stmt::LocalDef { name: "sum".to_string(), ty: JType::Int, init: Some(Expr::Bin { op: BinOp::Add, a: Box::new(local("v$1", 5)), b: Box::new(local("b", 2)), ty: JType::Int }) },
		];
		assert_eq!(inline_temps(&mut body, &names), 1);
		assert_eq!(body.len(), 1);
		let text = format!("{:?}", body);
		assert!(text.contains("\"a\"") && text.contains("\"b\""), "expected a + b, got {}", text);
		assert!(!text.contains("v$1"), "temp must be gone: {}", text);
	}

	#[test]
	fn temp_used_across_a_join_is_kept() {
		let names: HashSet<String> = ["v$1".to_string()].into_iter().collect();
		let mut body = vec![
			assign("v$1", 5, local("a", 1)),
			Stmt::If {
				cond: local("c", 2),
				then_body: vec![assign("v$1", 5, local("b", 3))],
				else_body: Vec::new(),
				has_else: false,
			},
			Stmt::Return { value: Some(local("v$1", 5)) },
		];
		// two writes (one inside the `if`) -> no inlining, the temp stays declared
		assert_eq!(inline_temps(&mut body, &names), 0);
		assert_eq!(body.len(), 3);
	}

	#[test]
	fn dead_labels_and_fallthrough_gotos_are_dropped() {
		let mut body = vec![
			Stmt::Label { id: 1 },
			Stmt::Goto { id: 2 },
			Stmt::Label { id: 2 },
			Stmt::ExprStmt { expr: local("a", 1) },
			Stmt::Label { id: 3 },
		];
		let (labels, gotos) = elide_dead_labels(&mut body);
		assert_eq!(gotos, 1, "the goto only skips to the next label");
		// label 2 becomes dead once the goto that referenced it is gone
		assert_eq!(labels, 3, "all three labels are unreferenced in the end, got {:?}", body);
		assert_eq!(body.len(), 1, "{:?}", body);
		assert!(matches!(body[0], Stmt::ExprStmt { .. }));
	}

	#[test]
	fn label_referenced_by_a_backward_goto_survives() {
		let mut body = vec![
			Stmt::Label { id: 4 },
			Stmt::ExprStmt { expr: local("a", 1) },
			Stmt::Goto { id: 4 },
		];
		let (labels, gotos) = elide_dead_labels(&mut body);
		assert_eq!((labels, gotos), (0, 0), "{:?}", body);
		assert_eq!(body.len(), 3);
	}
}
