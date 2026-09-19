//! CFG structuring: basic blocks and edges -> Java control-flow statements.
//!
//! jadx builds `IRegion` trees in `RegionMaker` and then simplifies them through
//! ~30 `*Processors` (`IfRegionProcessors`, `LoopsRegionProcessors`, ...). This
//! port reaches the same result with a recursive descent over the reverse post
//! order of the CFG -- the classic JVM technique -- where a region is accepted
//! only when its shape is provably expressible as one Java statement. Everything
//! else stays as labelled blocks plus `goto`, which is what jadx prints for the
//! methods it cannot structure either.
//!
//! Two invariants keep the output compilable:
//!
//! * a region is accepted only if every successor of every block inside it is
//!   either inside the region or one of its exits ([`Str::region_ok`]);
//! * every emitted block and every accepted region is preceded by a `LBL_n:`
//!   marker, so a printed `goto` can never miss its target. Dead markers are
//!   removed afterwards by [`super::passes::elide_dead_labels`].

use std::collections::{HashMap, HashSet};

use crate::args::Args;
use crate::java::insn::Cond;
use crate::types::JType;

use super::cfg::Cfg;
use super::emulate::{LocalInfo, MethodCode};
use super::ir::{ends, CatchClause, Expr, Stmt, SwitchCase};

/// A structured method body.
#[derive(Debug, Clone)]
pub struct Structured {
	/// the statements of the body, in printing order
	pub body: Vec<Stmt>,
	/// shapes the structurer refused to fold into Java syntax, one message each;
	/// printed above the method like jadx's `/* JADX WARN: ... */` comments
	pub notes: Vec<String>,
	/// `false` when no region at all was recognised; the writer then adds the
	/// `/* failed to restore code structure */` marker jadx uses
	pub structured: bool,
}

/// Per-block input, detached from the borrow of [`MethodCode`] so that the
/// recursive descent only has to keep `&mut self`.
#[derive(Debug, Clone, Default)]
struct BlockOut {
	stmts: Vec<Stmt>,
	cond: Option<Expr>,
	targets: Vec<usize>,
	switch_keys: Vec<Option<i32>>,
	subject: Option<Expr>,
	catch_var: Option<u32>,
}

/// Where the region currently being emitted leaves to.
#[derive(Debug, Clone, Copy)]
struct Ctx {
	/// the successor that means "this region ended": nothing is printed for it,
	/// because the enclosing statement already continues there
	exit: Option<usize>,
	/// the enclosing loop, when inside one
	loop_: Option<LoopCtx>,
	/// the enclosing `switch` arm, when inside one
	switch: Option<SwitchCtx>,
	/// nesting guard against pathological CFGs
	depth: usize,
}

#[derive(Debug, Clone, Copy)]
struct LoopCtx {
	/// first block of the loop
	hdr: usize,
	/// block holding the test: the header for `while`, the last block for `do`
	cond: usize,
	/// the block the loop continues with
	exit: Option<usize>,
	do_while: bool,
}

#[derive(Debug, Clone, Copy)]
struct SwitchCtx {
	/// the block the `switch` continues with; reaching it needs a `break`
	exit: Option<usize>,
	/// `true` for the last arm, whose fall out of the switch is implicit
	last: bool,
}

impl Ctx {
	fn root() -> Ctx {
		Ctx { exit: None, loop_: None, switch: None, depth: 0 }
	}
}

/// Structure one method body from its CFG and the emulation result.
pub fn decompile_method_body(cfg: &Cfg, code: &MethodCode, args: &Args) -> Structured {
	let mut blocks: Vec<BlockOut> = Vec::with_capacity(cfg.blocks.len());
	for b in &cfg.blocks {
		let r = code.blocks.get(b.id);
		blocks.push(BlockOut {
			stmts: r.map(|r| r.stmts.clone()).unwrap_or_default(),
			cond: r.and_then(|r| r.cond.clone()),
			targets: b.successors.clone(),
			switch_keys: r.map(|r| r.switch_keys.clone()).unwrap_or_default(),
			subject: r.and_then(|r| r.switch_subject.clone()),
			catch_var: r.and_then(|r| r.catch_var),
		});
	}
	let mut s = Str {
		cfg,
		args,
		blocks,
		locals: code.locals.clone(),
		rpo: Vec::new(),
		pos: HashMap::new(),
		consumed: vec![false; cfg.blocks.len()],
		notes: Vec::new(),
		structured: false,
	};
	s.init_order();
	let body = s.run();
	Structured { body, notes: s.notes, structured: s.structured }
}

struct Str<'a> {
	cfg: &'a Cfg,
	args: &'a Args,
	blocks: Vec<BlockOut>,
	locals: Vec<LocalInfo>,
	/// the blocks that will be emitted, in order
	rpo: Vec<usize>,
	/// block -> index in `rpo`
	pos: HashMap<usize, usize>,
	consumed: Vec<bool>,
	notes: Vec<String>,
	structured: bool,
}

impl<'a> Str<'a> {
	fn init_order(&mut self) {
		let mut rpo = self.cfg.reachable_rpo();
		// handler blocks are not reachable from the entry block: append their own
		// reverse post order so their statements are printed even when no `try`
		// region could wrap them
		for hb in self.cfg.handler_blocks.clone() {
			for b in self.cfg.rpo_from(hb) {
				if !rpo.contains(&b) {
					rpo.push(b);
				}
			}
		}
		rpo.retain(|b| self.cfg.blocks[*b].reachable);
		self.pos = rpo.iter().copied().enumerate().map(|(i, b)| (b, i)).collect();
		self.rpo = rpo;
	}

	fn run(&mut self) -> Vec<Stmt> {
		let to = self.rpo.len();
		if to == 0 {
			return Vec::new();
		}
		if self.args.decompilation_mode.is_special() {
			self.notes
				.push(format!("decompilation mode {:?}: control flow is printed as labelled blocks", self.args.decompilation_mode));
			return self.emit_linear(0, to, Ctx::root());
		}
		let body = self.emit(0, to, Ctx::root());
		// blocks that no region claimed are appended in order so nothing is lost
		let mut rest: Vec<Stmt> = Vec::new();
		for i in 0..to {
			let b = self.rpo[i];
			if self.consumed[b] {
				continue;
			}
			let ctx = Ctx { exit: self.at(i + 1), ..Ctx::root() };
			rest.extend(self.emit_block(b, &ctx));
			self.consumed[b] = true;
		}
		if rest.is_empty() {
			body
		} else {
			self.notes.push("some blocks did not fit any region and are printed with labels and goto".to_string());
			body.into_iter().chain(rest).collect()
		}
	}

	/// Emit the region covering `rpo[from..to]`.
	fn emit(&mut self, from: usize, to: usize, ctx: Ctx) -> Vec<Stmt> {
		if ctx.depth > 150 {
			self.notes.push("nesting limit reached; the rest of the method is printed with labels and goto".to_string());
			return self.emit_linear(from, to, ctx);
		}
		let mut out: Vec<Stmt> = Vec::new();
		let mut i = from;
		while i < to {
			let b = self.rpo[i];
			if self.consumed[b] {
				i += 1;
				continue;
			}
			if let Some((stmts, next)) = self.try_try(i, to, &ctx) {
				out.extend(stmts);
				i = next;
				self.structured = true;
				continue;
			}
			if let Some((stmts, next)) = self.try_loop(i, to, &ctx) {
				out.extend(stmts);
				i = next;
				self.structured = true;
				continue;
			}
			if let Some((stmts, next)) = self.try_switch(i, to, &ctx) {
				out.extend(stmts);
				i = next;
				self.structured = true;
				continue;
			}
			if let Some((stmts, next)) = self.try_if(i, to, &ctx) {
				out.extend(stmts);
				i = next;
				self.structured = true;
				continue;
			}
			out.extend(self.emit_block(b, &ctx));
			self.consumed[b] = true;
			i += 1;
		}
		out
	}

	/// One block: its statements, a label, and the transfer of control out of it.
	fn emit_block(&mut self, b: usize, ctx: &Ctx) -> Vec<Stmt> {
		let mut out: Vec<Stmt> = Vec::with_capacity(self.blocks[b].stmts.len() + 2);
		out.push(Stmt::Label { id: b as u32 });
		out.extend(self.blocks[b].stmts.iter().cloned());
		if ends(&out) {
			return out;
		}
		let targets = self.blocks[b].targets.clone();
		match targets.as_slice() {
			[] => {
				// no successor and no return/throw: the block leaves through the
				// region exit (e.g. the end of a `finally` handler) or ends the method
				if let Some(e) = ctx.exit {
					if e != b {
						out.push(Stmt::Goto { id: e as u32 });
					}
				}
			}
			[&s] => {
				if !self.transfer(b, s, ctx, &mut out) {
					out.push(Stmt::Goto { id: s as u32 });
				}
			}
			_ => {
				// an unfolded branch: print the test with a goto, and record the
				// remaining edges so the shape stays visible
				for (idx, &s) in targets.iter().enumerate() {
					if idx == 0 {
						match self.blocks[b].cond.clone() {
							Some(c) => out.push(Stmt::If { cond: c, then_body: vec![Stmt::Goto { id: s as u32 }], else_body: Vec::new(), has_else: false }),
							None => out.push(Stmt::Goto { id: s as u32 }),
						}
					} else {
						out.push(Stmt::Comment { text: format!("bytecode edge to block {} cannot be reached from the printed code", s) });
					}
				}
			}
		}
		out
	}

	/// `true` when the edge `b -> s` needs no statement.
	fn transfer(&mut self, b: usize, s: usize, ctx: &Ctx, out: &mut Vec<Stmt>) -> bool {
		if Some(s) == ctx.exit {
			return true;
		}
		if let Some(sw) = ctx.switch {
			if Some(s) == sw.exit {
				if !sw.last {
					out.push(Stmt::Break { id: None });
				}
				return true;
			}
			// `break`/`continue` would bind to the switch instead of the loop, so a
			// labelled goto is the faithful translation
			return false;
		}
		if let Some(l) = ctx.loop_ {
			if s == l.cond && l.do_while {
				// falling into the test of a `do { } while (c)` is a `continue`
				return true;
			}
			if s == l.hdr && !l.do_while {
				// the natural back edge of a `while` loop
				return true;
			}
			if Some(s) == l.exit {
				out.push(Stmt::Break { id: None });
				return true;
			}
			if s == l.cond {
				out.push(Stmt::Continue { id: None });
				return true;
			}
		}
		// straight into the next emitted block: nothing to print
		match self.next_live_after(b) {
			Some(x) if x == s => true,
			_ => false,
		}
	}

	fn emit_linear(&mut self, from: usize, to: usize, ctx: Ctx) -> Vec<Stmt> {
		let mut out = Vec::new();
		for i in from..to {
			let b = self.rpo[i];
			if self.consumed[b] {
				continue;
			}
			self.consumed[b] = true;
			let c = Ctx { exit: self.at(i + 1).or(ctx.exit), ..ctx };
			out.extend(self.emit_block(b, &c));
		}
		out
	}

	// --- region recognisers --------------------------------------------------

	/// `if (c) {...}` and `if (c) {...} else {...}`.
	///
	/// javac emits `if<false> else_label`, so the *fallthrough* block is the
	/// `then` branch and the printed condition is the inverse of the bytecode
	/// test; ecj sometimes emits `if<true> then_label`, where the condition is
	/// kept as it is. jadx performs the same swap in its `IfRegionProcessors`.
	fn try_if(&mut self, i: usize, to: usize, ctx: &Ctx) -> Option<(Vec<Stmt>, usize)> {
		let b = self.rpo[i];
		if self.blocks[b].cond.is_none() || self.blocks[b].subject.is_some() {
			return None;
		}
		let targets = self.blocks[b].targets.clone();
		if targets.len() != 2 {
			return None;
		}
		let ti = *self.pos.get(&targets[0])?;
		let fi = *self.pos.get(&targets[1])?;
		// the near arm must be the block that follows the test
		let near_is_fallthrough = fi == i + 1 && ti > fi;
		let near_is_taken = ti == i + 1 && fi > ti;
		if !near_is_fallthrough && !near_is_taken {
			return None;
		}
		let far = if near_is_fallthrough { ti } else { fi };
		if far <= i + 1 || far > to {
			return None;
		}
		// where the near arm leaves decides whether an `else` exists
		let merge = match self.last_target_of(i + 1, far) {
			Some(t) => *self.pos.get(&t)?,
			None => far,
		};
		if merge < far || merge > to {
			return None;
		}
		if !self.range_unconsumed(i + 1, far) || !self.region_ok(i + 1, far, merge) {
			return None;
		}
		let has_else = merge > far;
		if has_else && (!self.range_unconsumed(far, merge) || !self.region_ok(far, merge, merge)) {
			return None;
		}
		let cond0 = self.blocks[b].cond.clone()?;
		let cond = if near_is_fallthrough { invert(cond0) } else { cond0 };
		let then_ctx = Ctx { exit: Some(self.exit_block(merge, ctx)), depth: ctx.depth + 1, ..*ctx };
		let then_body = self.emit(i + 1, far, then_ctx);
		let else_body = if has_else {
			let else_ctx = Ctx { exit: Some(self.exit_block(merge, ctx)), depth: ctx.depth + 1, ..*ctx };
			self.emit(far, merge, else_ctx)
		} else {
			Vec::new()
		};
		for x in (i + 1)..merge {
			self.consumed[self.rpo[x]] = true;
		}
		self.consumed[b] = true;
		Some((self.label(b, vec![Stmt::If { cond, then_body, else_body, has_else }]), merge))
	}

	/// `while (c) { ... }` and `do { ... } while (c);`, found through the back
	/// edges that lead into the first block of the range.
	fn try_loop(&mut self, i: usize, to: usize, ctx: &Ctx) -> Option<(Vec<Stmt>, usize)> {
		let hdr = self.rpo[i];
		let back: Vec<usize> = self
			.cfg
			.back_edges()
			.into_iter()
			.filter(|(from, h)| *h == hdr && self.pos.get(from).copied().map(|p| p >= i && p < to).unwrap_or(false))
			.map(|(from, _)| from)
			.collect();
		if back.is_empty() {
			return None;
		}
		let mut body: HashSet<usize> = HashSet::new();
		body.insert(hdr);
		for f in &back {
			for x in self.cfg.loop_body(hdr, *f) {
				body.insert(x);
			}
		}
		let idx: Vec<usize> = body.iter().filter_map(|x| self.pos.get(x).copied()).collect();
		if idx.len() != body.len() {
			return None; // part of the loop is not in the emission order
		}
		let lo = *idx.iter().min()?;
		let hi = *idx.iter().max()?;
		if lo != i || hi >= to || hi - lo + 1 != idx.len() {
			return None; // not contiguous in the emission order
		}
		if !self.range_unconsumed(lo, hi + 1) {
			return None;
		}
		// exits of the loop, ignoring the edges that stay inside it
		let mut exits: Vec<usize> = Vec::new();
		for x in lo..=hi {
			let xb = self.rpo[x];
			for s in self.blocks[xb].targets.clone() {
				if self.is_in(s, lo, hi) || exits.contains(&s) {
					continue;
				}
				exits.push(s);
			}
		}
		if exits.len() > 1 {
			return None; // several exits: keep labels and goto
		}
		let exit = exits.first().copied();
		let next = match exit {
			Some(e) => match self.pos.get(&e).copied() {
				Some(p) if p > hi => p,
				Some(_) => return None,
				None => to,
			},
			None => hi + 1,
		};
		if next > to {
			return None;
		}

		let hdr_cond = self.blocks[hdr].cond.clone();
		let hdr_targets = self.blocks[hdr].targets.clone();
		let last = self.rpo[hi];
		let last_cond = self.blocks[last].cond.clone();
		let last_targets = self.blocks[last].targets.clone();
		// `while` when the test sits in the header, `do-while` when the last block
		// of the body jumps back to it
		let (cond_expr, body_from, body_to, cond_block, do_while) = if hdr_cond.is_some() && hdr_targets.len() == 2 {
			let (a_in, b_in) = (self.is_in(hdr_targets[0], lo, hi), self.is_in(hdr_targets[1], lo, hi));
			if a_in == b_in {
				return None;
			}
			let c = hdr_cond?;
			// the taken edge leaving the loop means the test reads as the body
			// condition, entering it means the opposite
			(let_cond(a_in, c), lo + 1, hi + 1, hdr, false)
		} else if last_cond.is_some() && last_targets.len() == 2 {
			let (a, bb) = (last_targets[0], last_targets[1]);
			let back_from_taken = a == hdr && bb != hdr;
			let back_from_fallthrough = bb == hdr && a != hdr;
			if !(back_from_taken || back_from_fallthrough) {
				return None;
			}
			let c = last_cond?;
			(let_cond(back_from_taken, c), lo, hi, last, true)
		} else {
			return None;
		};
		if body_from > body_to {
			return None;
		}
		// the body may leave through the test, the loop exit, or the range end
		let mut allowed: Vec<usize> = vec![self.pos.get(&cond_block).copied().unwrap_or(lo), next];
		if let Some(e) = exit {
			if let Some(p) = self.pos.get(&e).copied() {
				if !allowed.contains(&p) {
					allowed.push(p);
				}
			}
		}
		if !self.region_ok_multi(body_from, body_to, &allowed) {
			return None;
		}
		let loop_ctx = Ctx {
			exit: Some(cond_block),
			loop_: Some(LoopCtx { hdr, cond: cond_block, exit, do_while }),
			switch: None,
			depth: ctx.depth + 1,
		};
		let body_stmts = self.emit(body_from, body_to, loop_ctx);
		for x in lo..=hi {
			self.consumed[self.rpo[x]] = true;
		}
		Some((self.label(hdr, vec![Stmt::While { cond: Some(cond_expr), body: body_stmts, do_while, label: u32::MAX }]), next))
	}

	/// `switch (x) { case ..: ... }`, one arm per group of successors.
	fn try_switch(&mut self, i: usize, to: usize, ctx: &Ctx) -> Option<(Vec<Stmt>, usize)> {
		let b = self.rpo[i];
		let subject = self.blocks[b].subject.clone()?;
		let targets = self.blocks[b].targets.clone();
		let keys = self.blocks[b].switch_keys.clone();
		if targets.len() < 2 || targets.len() != keys.len() {
			return None;
		}
		let mut by_start: HashMap<usize, Vec<i32>> = HashMap::new();
		let mut starts: Vec<usize> = Vec::new();
		let mut default_pos: Option<usize> = None;
		for (t, k) in targets.iter().zip(keys.iter()) {
			let p = *self.pos.get(t)?;
			if p <= i {
				return None; // a case body that starts before the switch
			}
			if !starts.contains(&p) {
				starts.push(p);
			}
			match k {
				Some(key) => by_start.entry(p).or_default().push(*key),
				None => default_pos = Some(p),
			}
		}
		let default_pos = default_pos?;
		starts.sort_unstable();
		if starts.first().copied() != Some(i + 1) {
			return None; // dead code between the switch and its first case
		}
		let last_start = *starts.last()?;
		// the switch continues at the region exit, or at the end of the range
		let merge = match ctx.exit.and_then(|e| self.pos.get(&e).copied()) {
			Some(p) if p > last_start => p,
			_ => to,
		};
		if merge <= last_start || merge > to {
			return None;
		}
		for x in (i + 1)..merge {
			if self.consumed[self.rpo[x]] {
				return None;
			}
		}
		if !self.region_ok(i + 1, merge, merge) {
			return None;
		}
		let exit_block = self.at(merge);
		let mut cases: Vec<SwitchCase> = Vec::new();
		let mut default_body: Vec<Stmt> = Vec::new();
		let mut has_default = false;
		for w in 0..starts.len() {
			let s = starts[w];
			let e = if w + 1 < starts.len() { starts[w + 1] } else { merge };
			if s >= e {
				continue;
			}
			let arm_ctx = Ctx {
				exit: exit_block,
				loop_: ctx.loop_,
				switch: Some(SwitchCtx { exit: exit_block, last: e == merge }),
				depth: ctx.depth + 1,
			};
			let body = self.emit(s, e, arm_ctx);
			match by_start.get(&s) {
				Some(ks) => {
					let falls = self.last_target_of(s, e) == self.at(e);
					cases.push(SwitchCase { keys: ks.clone(), body, falls_through: falls });
				}
				None => {
					if s == default_pos {
						default_body = body;
						has_default = true;
					}
				}
			}
		}
		for x in (i + 1)..merge {
			self.consumed[self.rpo[x]] = true;
		}
		self.consumed[b] = true;
		Some((self.label(b, vec![Stmt::Switch { subject, cases, has_default, default_body }]), merge))
	}

	/// `try { } catch () { } finally { }`, built from the exception table.
	///
	/// Only ranges that are contiguous in the emission order and whose handlers
	/// follow the protected code directly are accepted; anything else keeps its
	/// labels and gotos (jadx reports `use raw instructions` for such methods).
	fn try_try(&mut self, i: usize, to: usize, ctx: &Ctx) -> Option<(Vec<Stmt>, usize)> {
		if self.cfg.try_regions.is_empty() {
			return None;
		}
		let b = self.rpo[i];
		let start = self.cfg.blocks[b].start;
		let end_off = self.cfg.blocks[b].end;
		let region = self.cfg.try_regions.iter().find(|r| start >= r.start && end_off <= r.end)?;
		let mut try_end = i;
		while try_end < to {
			let x = self.rpo[try_end];
			let bx = &self.cfg.blocks[x];
			if bx.start >= region.start && bx.end <= region.end && !self.consumed[x] {
				try_end += 1;
			} else {
				break;
			}
		}
		if try_end <= i {
			return None;
		}
		let mut arms: Vec<(usize, bool, Option<JType>)> = Vec::new(); // (block, is_finally, type)
		for h in &region.handlers {
			let hb = self.cfg.block_at(h.handler_pc)?;
			arms.push((hb, h.catch_type.is_none(), h.catch_type.clone()));
		}
		if arms.is_empty() {
			return None;
		}
		// handler code has to follow the protected range without a gap
		let mut runs: Vec<(usize, usize, usize, bool, Option<JType>)> = Vec::new();
		let mut cursor = try_end;
		for (ai, (hb, is_final, ty)) in arms.iter().enumerate() {
			let hp = *self.pos.get(hb)?;
			if hp != cursor {
				return None;
			}
			let order = self.cfg.rpo_from(*hb);
			let mut n = 0usize;
			while n < order.len() {
				match self.pos.get(&order[n]).copied() {
					Some(p) if p == hp + n => n += 1,
					_ => break,
				}
			}
			if n == 0 || hp + n > to || !self.range_unconsumed(hp, hp + n) {
				return None;
			}
			runs.push((ai, hp, hp + n, *is_final, ty.clone()));
			cursor = hp + n;
		}
		// a `finally` handler must be the last one, otherwise the rethrow cannot be
		// expressed as one Java statement
		let has_finally = runs.iter().any(|(_, _, _, f, _)| *f);
		let finally_is_last = runs.last().map(|r| r.3).unwrap_or(false);
		if has_finally && !finally_is_last {
			return None;
		}
		let region_end = runs.last()?.2;
		let try_ctx = Ctx { exit: self.at(runs.first()?.1), depth: ctx.depth + 1, ..*ctx };
		let try_body = self.emit(i, try_end, try_ctx);
		let mut catches: Vec<CatchClause> = Vec::new();
		let mut finally_body: Option<Vec<Stmt>> = None;
		for (_, f, t, is_final, ty) in runs {
			let arm_block = self.rpo[f];
			let after_exit = self.at(t).or(ctx.exit);
			let arm_ctx = Ctx { exit: after_exit, depth: ctx.depth + 1, ..*ctx };
			let body = self.emit(f, t, arm_ctx);
			if is_final {
				finally_body = Some(match finally_body.take() {
					Some(mut prev) => {
						prev.extend(body);
						prev
					}
					None => body,
				});
			} else {
				// several exception table rows may share one handler (javac emits that
				// for `catch (A | B e)`); printing one clause loses the union type
				let n_types = region.handlers.iter().filter(|h| self.cfg.block_at(h.handler_pc) == Some(arm_block) && h.catch_type.is_some()).count();
				if n_types > 1 {
					self.notes.push(format!("handler at block {} catches {} types; merged into one catch clause", arm_block, n_types));
				}
				let ty = ty.unwrap_or_else(|| JType::class("java/lang/Throwable"));
				catches.push(CatchClause { ty: Some(ty), var_name: self.catch_var_name(arm_block), body });
			}
		}
		for x in i..region_end {
			self.consumed[self.rpo[x]] = true;
		}
		Some((self.label(b, vec![Stmt::TryCatch { try_body, catches, finally_body }]), region_end))
	}

	// --- helpers -------------------------------------------------------------

	/// `LBL_n:` for a block that other code may jump to.
	fn label(&self, b: usize, mut stmts: Vec<Stmt>) -> Vec<Stmt> {
		let mut out = Vec::with_capacity(stmts.len() + 1);
		out.push(Stmt::Label { id: b as u32 });
		out.append(&mut stmts);
		out
	}

	/// The block at position `p`, `None` past the end of the emission order.
	fn at(&self, p: usize) -> Option<usize> {
		self.rpo.get(p).copied()
	}

	/// The block control continues with after position `p` of the emission order,
	/// falling back to the enclosing context's exit.
	fn exit_block(&self, p: usize, ctx: &Ctx) -> usize {
		match self.at(p) {
			Some(b) => b,
			None => ctx.exit.unwrap_or(usize::MAX),
		}
	}

	fn range_unconsumed(&self, from: usize, to: usize) -> bool {
		from < to && (from..to).all(|x| !self.consumed[self.rpo[x]])
	}

	/// Every successor of every block in `[from, to)` must be inside the range or
	/// land on `exit_pos` -- the single-exit requirement jadx enforces when it
	/// builds `Region` nodes.
	fn region_ok(&self, from: usize, to: usize, exit_pos: usize) -> bool {
		self.region_ok_multi(from, to, &[exit_pos])
	}

	fn region_ok_multi(&self, from: usize, to: usize, allowed: &[usize]) -> bool {
		let mut allowed: Vec<usize> = allowed.to_vec();
		allowed.push(to);
		for x in from..to {
			let b = self.rpo[x];
			for s in self.blocks[b].targets.clone() {
				match self.pos.get(&s).copied() {
					Some(p) if p >= from && p < to => {}
					Some(p) if allowed.contains(&p) => {}
					// an edge to a block outside the emission order (unreachable code)
					// is only fine when the block cannot fall through to it
					None => {
						if !ends(&self.blocks[b].stmts) {
							return false;
						}
					}
					Some(_) => return false,
				}
			}
		}
		true
	}

	/// The last not-yet-consumed block of a range: its successor is where the
	/// region leaves to.
	fn last_live_block(&self, from: usize, to: usize) -> Option<usize> {
		let mut last = None;
		for x in from..to {
			let b = self.rpo[x];
			if !self.consumed[b] {
				last = Some(b);
			}
		}
		last
	}

	fn last_target_of(&self, from: usize, to: usize) -> Option<usize> {
		let last = self.last_live_block(from, to)?;
		self.blocks[last].targets.first().copied()
	}

	fn is_in(&self, block: usize, lo: usize, hi: usize) -> bool {
		match self.pos.get(&block).copied() {
			Some(p) => p >= lo && p <= hi,
			None => false,
		}
	}

	fn next_live_after(&self, b: usize) -> Option<usize> {
		let mut p = *self.pos.get(&b)?;
		while p + 1 < self.rpo.len() {
			p += 1;
			let x = self.rpo[p];
			if !self.consumed[x] {
				return Some(x);
			}
		}
		None
	}

	/// The `catch` parameter name: the slot javac stored the exception into, so a
	/// `LocalVariableTable` name wins, like jadx's `useDebugVarsNames`.
	fn catch_var_name(&self, hb: usize) -> String {
		match self.blocks.get(hb).and_then(|b| b.catch_var) {
			Some(idx) => self
				.locals
				.iter()
				.find(|l| l.index == idx)
				.map(|l| l.name.clone())
				.unwrap_or_else(|| format!("e{}", idx)),
			None => "ignored".to_string(),
		}
	}
}

/// The condition to print for a loop test: kept as it is when the taken edge is
/// the back edge (the test says "stay in the loop"), inverted when the taken edge
/// leaves (the test says "break").
fn let_cond(taken_enters: bool, c: Expr) -> Expr {
	if taken_enters {
		c
	} else {
		invert(c)
	}
}

/// Invert a comparison for `if (!c)` (jadx: `IfNode.invertCondition`).
fn invert(e: Expr) -> Expr {
	match e {
		Expr::Cmp { op, a, b } => Expr::Cmp { op: invert_cond(op), a, b },
		Expr::Not { a } => *a,
		other => Expr::Not { a: Box::new(other) },
	}
}

fn invert_cond(op: Cond) -> Cond {
	op.invert()
}
