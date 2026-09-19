//! Control flow graph construction and analysis.
//!
//! jadx builds its regions from an SSA graph (`RegionMaker` after dozens of IR
//! passes). This port structures the plain CFG instead, so all it needs is: block
//! boundaries from branch targets and exception ranges, reverse post order, DFS
//! back edges and natural loop bodies. Dominator trees are deliberately not
//! computed -- the structuring patterns in `super::structure` use contiguity in
//! RPO plus reachability, which is enough for the shapes that real compilers
//! emit and much less code to get wrong.

use crate::error::{JadxError, Res};
use crate::java::attrs::{ExceptionHandler, LineNumber};
use crate::java::insn::{Cond, Insn, Sem};
use crate::types::JType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
	Normal,
	/// entry point of a `catch` handler
	Catch,
	/// entry point of a catch-all entry, i.e. `finally` in Java
	Finally,
}

#[derive(Debug, Clone)]
pub struct Block {
	pub id: usize,
	/// byte range in the `Code` array, `[start, end)`
	pub start: u32,
	pub end: u32,
	/// index range into the decoded instruction vector, `[first, last)`
	pub first: usize,
	pub last: usize,
	pub successors: Vec<usize>,
	pub predecessors: Vec<usize>,
	pub kind: BlockKind,
	/// for handler blocks: the caught type(s); `None` means catch-all
	pub catch_types: Vec<Option<JType>>,
	/// at least one edge jumps here, so the block cannot be merged with its
	/// predecessor
	pub is_target: bool,
	/// first line number reported for this block, when a LineNumberTable exists
	pub line: Option<u32>,
	/// reachable from the entry block, or a handler entry
	pub reachable: bool,
}

impl Block {
	pub fn insns<'a>(&self, all: &'a [Insn]) -> &'a [Insn] {
		&all[self.first..self.last]
	}

	pub fn last_insn<'a>(&self, all: &'a [Insn]) -> Option<&'a Insn> {
		if self.last > self.first {
			Some(&all[self.last - 1])
		} else {
			None
		}
	}

	/// true when the block ends with a conditional branch, so it can become the
	/// header of an `if`
	pub fn ends_with_condition(&self, all: &[Insn]) -> bool {
		match self.last_insn(all) {
			Some(i) => matches!(
				i.sem(),
				Sem::If(_) | Sem::IfCmp(_) | Sem::IfACmp(_) | Sem::IfNull | Sem::IfNonNull
			),
			None => false,
		}
	}

	/// the two outcomes of a conditional block: `(taken, fallthrough)` offsets
	pub fn cond_targets(&self, all: &[Insn]) -> Option<(u32, u32)> {
		let insn = self.last_insn(all)?;
		let taken = insn.branch()?;
		Some((taken, self.end))
	}

	/// a block that only falls through and is only fallen into can be merged into
	/// its single predecessor
	pub fn is_mergeable(&self) -> bool {
		!self.is_target && self.predecessors.len() == 1 && self.kind == BlockKind::Normal
	}
}

#[derive(Debug, Clone)]
pub struct Cfg {
	pub blocks: Vec<Block>,
	pub code_len: u32,
	/// `(block start offset, block id)`, sorted
	by_offset: Vec<(u32, usize)>,
	/// `exception_table` rows merged per protected range
	pub try_regions: Vec<TryRegion>,
	/// handler entry blocks, in exception table order
	pub handler_blocks: Vec<usize>,
}

/// Merged `Code.exception_table` rows for one protected range: same
/// `[start, end)`, one or more handlers (one per caught type, plus a catch-all).
#[derive(Debug, Clone)]
pub struct TryRegion {
	pub start: u32,
	pub end: u32,
	pub handlers: Vec<ExceptionHandler>,
}

impl Cfg {
	pub fn block_at(&self, offset: u32) -> Option<usize> {
		self.by_offset.iter().find(|(o, _)| *o == offset).map(|(_, b)| *b)
	}

	pub fn entry(&self) -> usize {
		0
	}

	/// Reverse post order of the CFG rooted at the entry block. Handler blocks
	/// are not part of it: they are entered through the exception table, and
	/// `handler_blocks` walks them separately.
	pub fn rpo_from(&self, root: usize) -> Vec<usize> {
		let n = self.blocks.len();
		if root >= n {
			return Vec::new();
		}
		let mut visited = vec![false; n];
		let mut order: Vec<usize> = Vec::with_capacity(n);
		let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
		visited[root] = true;
		while let Some((b, idx)) = stack.pop() {
			let succs = self.blocks[b].successors.clone();
			if idx < succs.len() {
				stack.push((b, idx + 1));
				let s = succs[idx];
				if s < n && !visited[s] {
					visited[s] = true;
					stack.push((s, 0));
				}
			} else {
				order.push(b);
			}
		}
		order.reverse();
		order
	}

	pub fn rpo(&self) -> Vec<usize> {
		self.rpo_from(self.entry())
	}

	/// RPO of the reachable blocks only (dead code -- e.g. the bytes that follow a
	/// `return` -- is dropped by the caller and reported as a comment).
	pub fn reachable_rpo(&self) -> Vec<usize> {
		self.rpo().into_iter().filter(|b| self.blocks[*b].reachable).collect()
	}

	/// Depth first search from the entry, returning back edges `(from, to)` where
	/// `to` is still on the DFS stack; `to` is a loop header.
	pub fn back_edges(&self) -> Vec<(usize, usize)> {
		let n = self.blocks.len();
		if n == 0 {
			return Vec::new();
		}
		let mut state = vec![0u8; n]; // 0 new, 1 on stack, 2 finished
		let mut out: Vec<(usize, usize)> = Vec::new();
		let mut stack: Vec<(usize, usize)> = vec![(self.entry(), 0)];
		state[self.entry()] = 1;
		while let Some((b, idx)) = stack.pop() {
			let succs = self.blocks[b].successors.clone();
			if idx < succs.len() {
				stack.push((b, idx + 1));
				let s = succs[idx];
				if s >= n {
					continue;
				}
				match state[s] {
					1 => out.push((b, s)),
					0 => {
						state[s] = 1;
						stack.push((s, 0));
					}
					_ => {}
				}
			} else {
				state[b] = 2;
			}
		}
		out.sort();
		out.dedup();
		out
	}

	/// The natural loop of one back edge: every block that can reach `back_from`
	/// without passing through `header`, plus the header itself.
	pub fn loop_body(&self, header: usize, back_from: usize) -> Vec<usize> {
		let mut in_loop = vec![false; self.blocks.len()];
		in_loop[header] = true;
		let mut stack = vec![back_from];
		while let Some(b) = stack.pop() {
			if b >= in_loop.len() || in_loop[b] {
				continue;
			}
			in_loop[b] = true;
			for p in &self.blocks[b].predecessors {
				if !in_loop[*p] {
					stack.push(*p);
				}
			}
		}
		in_loop.iter().enumerate().filter(|(_, v)| **v).map(|(i, _)| i).collect()
	}

	/// `true` when `to` is reachable from `from` following successors.
	pub fn reaches(&self, from: usize, to: usize) -> bool {
		reaches(from, to, &self.blocks)
	}

	/// Every successor of `b` inside `region` must stay inside it, and edges that
	/// leave the region must all land on `exit` -- the single-exit requirement an
	/// `if`/`loop` region needs (jadx enforces the same property when it builds
	/// `Region` nodes).
	pub fn is_clean_region(&self, region: &[usize], exit: Option<usize>) -> bool {
		let mut inside = vec![false; self.blocks.len()];
		for b in region {
			if *b < inside.len() {
				inside[*b] = true;
			}
		}
		for b in region {
			for s in &self.blocks[*b].successors {
				if inside[*s] {
					continue;
				}
				match exit {
					Some(e) if e == *s => {}
					_ => return false,
				}
			}
		}
		true
	}
}

/// `true` when `to` is reachable from `from`.
pub fn reaches(from: usize, to: usize, blocks: &[Block]) -> bool {
	if from >= blocks.len() || to >= blocks.len() {
		return false;
	}
	let mut seen = vec![false; blocks.len()];
	let mut stack = vec![from];
	while let Some(b) = stack.pop() {
		if b == to {
			return true;
		}
		if seen[b] {
			continue;
		}
		seen[b] = true;
		for s in &blocks[b].successors {
			if *s < blocks.len() && !seen[*s] {
				stack.push(*s);
			}
		}
	}
	false
}

/// jadx: `RegionMaker` starts from these blocks; the offset-to-block map is
/// rebuilt here because branch targets are byte offsets.
pub fn build_cfg(
	insns: &[Insn],
	code_len: u32,
	handlers: &[ExceptionHandler],
	line_numbers: &[LineNumber],
) -> Res<Cfg> {
	if code_len == 0 {
		return Ok(Cfg { blocks: Vec::new(), code_len: 0, by_offset: Vec::new(), try_regions: Vec::new(), handler_blocks: Vec::new() });
	}
	// 1. leaders: every offset that must start a block
	let mut leaders: Vec<u32> = vec![0];
	for i in insns {
		let sem = i.sem();
		let is_branch = sem.is_conditional_branch() || matches!(sem, Sem::Goto | Sem::Jsr);
		if is_branch {
			for t in i.targets() {
				leaders.push(t);
			}
			leaders.push(i.off + i.len as u32);
		}
		if matches!(sem, Sem::Return(_) | Sem::Athrow | Sem::Ret) {
			leaders.push(i.off + i.len as u32);
		}
	}
	for h in handlers {
		leaders.push(h.handler_pc);
		leaders.push(h.start);
		if h.end < code_len {
			leaders.push(h.end);
		}
	}
	leaders.retain(|o| *o < code_len);
	leaders.sort_unstable();
	leaders.dedup();
	if leaders.is_empty() {
	 leaders.push(0);
	}

	// 2. split the instruction list at the leaders
	let mut blocks: Vec<Block> = Vec::new();
	let mut insn_index = 0usize;
	for bi in 0..leaders.len() {
		let start = leaders[bi];
		let end = if bi + 1 < leaders.len() { leaders[bi + 1] } else { code_len };
		while insn_index < insns.len() && insns[insn_index].off < start {
			insn_index += 1;
		}
		let first = insn_index;
		while insn_index < insns.len() && insns[insn_index].off < end {
			insn_index += 1;
		}
		let line = line_numbers.iter().find(|l| l.start_pc == start).map(|l| l.line);
		blocks.push(Block {
			id: bi,
			start,
			end,
			first,
			last: insn_index,
			successors: Vec::new(),
			predecessors: Vec::new(),
			kind: BlockKind::Normal,
			catch_types: Vec::new(),
			is_target: false,
			line,
			reachable: false,
		});
	}
	let offset_to_block: Vec<(u32, usize)> = blocks.iter().map(|b| (b.start, b.id)).collect();
	let find = |off: u32| -> Option<usize> { offset_to_block.iter().find(|(o, _)| *o == off).map(|(_, b)| *b) };

	// 3. edges, decided by the last instruction of each block
	for bi in 0..blocks.len() {
		let (first, last, end) = (blocks[bi].first, blocks[bi].last, blocks[bi].end);
		let mut succ: Vec<usize> = Vec::new();
		let last_insn = if last > first { Some(&insns[last - 1]) } else { None };
		match last_insn.map(|i| i.sem()) {
			Some(sem) if matches!(sem, Sem::Return(_) | Sem::Athrow | Sem::Ret) => {
				// the block ends the flow: no successors
			}
			Some(Sem::Goto) | Some(Sem::Jsr) => {
				if let Some(t) = last_insn.and_then(|i| i.branch()) {
					if let Some(tb) = find(t) {
						succ.push(tb);
					}
				}
			}
			Some(Sem::TableSwitch) | Some(Sem::LookupSwitch) => {
				if let Some((default, pairs)) = last_insn.and_then(|i| i.switch_data()) {
					if let Some(db) = find(default) {
						succ.push(db);
					}
					for (_, target) in pairs {
						if let Some(tb) = find(target) {
							if !succ.contains(&tb) {
								succ.push(tb);
							}
						}
					}
				}
			}
			Some(sem) if sem.is_conditional_branch() => {
				if let Some(t) = last_insn.and_then(|i| i.branch()) {
					if let Some(tb) = find(t) {
						succ.push(tb);
					}
				}
				if let Some(fb) = find(end) {
					if !succ.contains(&fb) {
						succ.push(fb);
					}
				}
			}
			_ => {
				if let Some(fb) = find(end) {
					succ.push(fb);
				}
			}
		}
		blocks[bi].successors = succ;
	}

	// 4. predecessors and jump-target flags
	let n = blocks.len();
	for bi in 0..n {
		let succ = blocks[bi].successors.clone();
		for s in succ {
			if !blocks[s].predecessors.contains(&bi) {
				blocks[s].predecessors.push(bi);
			}
			blocks[s].is_target = true;
		}
	}

	// 5. classify handler entry blocks
	let mut handler_blocks: Vec<usize> = Vec::new();
	for h in handlers {
		if let Some(hb) = find(h.handler_pc) {
			if !handler_blocks.contains(&hb) {
				handler_blocks.push(hb);
			}
			let ty = h.catch_type.clone();
			blocks[hb].kind = if ty.is_none() { BlockKind::Finally } else { BlockKind::Catch };
			if !blocks[hb].catch_types.contains(&ty) {
				blocks[hb].catch_types.push(ty);
			}
		}
	}

	// 6. reachability from the entry and from each handler entry
	let mut reachable = vec![false; n];
	let mut stack: Vec<usize> = Vec::new();
	for hb in &handler_blocks {
		if !reachable[*hb] {
			reachable[*hb] = true;
			stack.push(*hb);
		}
	}
	if n > 0 {
		if !reachable[0] {
			reachable[0] = true;
			stack.push(0);
		}
	}
	while let Some(b) = stack.pop() {
		let succ = blocks[b].successors.clone();
		for s in succ {
			if !reachable[s] {
				reachable[s] = true;
				stack.push(s);
			}
		}
	}
	for b in 0..n {
		blocks[b].reachable = reachable[b];
	}

	// 7. merge exception table rows that protect the same range
	let mut try_regions: Vec<TryRegion> = Vec::new();
	for h in handlers {
		match try_regions.iter_mut().find(|r| r.start == h.start && r.end == h.end) {
			Some(r) => {
				if !r.handlers.iter().any(|e| e.handler_pc == h.handler_pc && e.catch_type == h.catch_type) {
					r.handlers.push(h.clone());
				}
			}
			None => try_regions.push(TryRegion { start: h.start, end: h.end, handlers: vec![h.clone()] }),
		}
	}
	try_regions.sort_by_key(|r| (r.start, r.end));

	Ok(Cfg { blocks, code_len, by_offset: offset_to_block, try_regions, handler_blocks })
}

/// Invert a comparison condition, for the "branch over a goto" rewrite
/// (jadx: `IfTester` / `region` simplifications).
pub fn inverted_cond(insn: &Insn) -> Option<Cond> {
	let sem = insn.sem();
	Some(match sem {
		Sem::If(c) | Sem::IfCmp(c) | Sem::IfACmp(c) => c.invert(),
		Sem::IfNull => Cond::Ne,
		Sem::IfNonNull => Cond::Eq,
		_ => return None,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::io::BinReader;
	use crate::java::const_pool::ConstPool;
	use crate::java::insn::decode_code;
	use crate::testutil::{ClassBuilder, CodeBuilder};

	/// Assemble a code array out of a fixture builder (branch offsets are patched
	/// for us), then build the CFG over it.
	fn cfg_of_code(code: &[u8]) -> Cfg {
		let pool = ConstPool::parse(&mut BinReader::new(&[0u8, 0x01])).unwrap();
		let insns = decode_code(code, &pool).unwrap();
		build_cfg(&insns, code.len() as u32, &[], &[]).unwrap()
	}

	/// same, from a `CodeBuilder`
	fn cfg_of(c: CodeBuilder) -> Cfg {
		let body = c.build();
		let code: Vec<u8> = body[8..].to_vec();
		cfg_of_code(&code)
	}

	fn empty_method() -> ClassBuilder {
		ClassBuilder::new("pkg/A", "java/lang/Object")
	}

	#[test]
	fn trivial_builder_helper_works() {
		let mut c = CodeBuilder::new(1, 1);
		c.op(0xb1);
		let cfg = cfg_of(c);
		assert_eq!(cfg.blocks.len(), 1);
		assert_eq!(cfg.code_len, 1);
		let _ = empty_method();
	}

	#[test]
	fn straight_line_code_is_one_block() {
		let mut c = CodeBuilder::new(1, 1);
		c.op(0x1a); // iload_0
		c.op(0xac); // ireturn
		let cfg = cfg_of(c);
		assert_eq!(cfg.blocks.len(), 1);
		assert!(cfg.blocks[0].successors.is_empty());
	}

	#[test]
	fn if_then_target_equal_to_fallthrough_collapses() {
		let mut c = CodeBuilder::new(2, 2);
		let end = c.new_label();
		c.op(0x1a); // iload_0
		c.branch(0x9a, end); // ifne -> end
		c.mark(end);
		c.op(0xb1); // return
		let cfg = cfg_of(c);
		assert_eq!(cfg.blocks.len(), 2);
		// the taken and fallthrough edges are the same block, so one successor
		assert_eq!(cfg.blocks[0].successors, vec![1]);
		assert_eq!(cfg.blocks[1].predecessors, vec![0]);
	}

	#[test]
	fn if_else_diamond_has_four_blocks() {
		let mut c = CodeBuilder::new(2, 2);
		let else_part = c.new_label();
		let end = c.new_label();
		c.op(0x1a); // iload_0
		c.branch(0x99, else_part); // ifeq -> else
		c.op(0x1a); // then: iload_0
		c.op(0xac); //   ireturn? use goto instead
		c.branch(0xa7, end); // goto end
		c.mark(else_part);
		c.op(0x03); // else: iconst_0
		c.mark(end);
		c.op(0xac); // ireturn
		let cfg = cfg_of(c);
		assert_eq!(cfg.blocks.len(), 4, "starts: {:?}", block_starts(&cfg));
		assert_eq!(cfg.blocks[0].successors, vec![2, 1]);
		assert_eq!(cfg.blocks[2].successors, vec![3]);
		assert_eq!(cfg.blocks[1].successors, vec![3]);
		assert_eq!(cfg.blocks[3].predecessors, vec![1, 2]);
	}

	fn block_starts(cfg: &Cfg) -> Vec<u32> {
		cfg.blocks.iter().map(|b| b.start).collect()
	}

	#[test]
	fn while_loop_back_edge_is_found() {
		let mut c = CodeBuilder::new(2, 2);
		let header = c.new_label();
		let end = c.new_label();
		c.mark(header);
		c.op(0x1a); // iload_0
		c.branch(0x99, end); // ifeq -> end
		c.iinc(0, 1); // iinc 0 by 1
		c.branch(0xa7, header); // goto header
		c.mark(end);
		c.op(0xb1); // return
		let cfg = cfg_of(c);
		let back = cfg.back_edges();
		assert_eq!(back.len(), 1);
		assert_eq!(back[0].1, 0, "loop header must be block 0");
		let body = cfg.loop_body(0, back[0].0);
		assert!(body.contains(&0));
		assert!(body.contains(&back[0].0));
		assert!(!body.contains(&2), "the block after the loop is not part of it");
	}

	#[test]
	fn dead_code_after_return_is_unreachable() {
		let mut c = CodeBuilder::new(1, 1);
		c.op(0xb1); // return
		c.op(0x00); // nop, unreachable
		let cfg = cfg_of(c);
		assert_eq!(cfg.blocks.len(), 2);
		assert!(cfg.blocks[0].reachable);
		assert!(!cfg.blocks[1].reachable);
	}

	#[test]
	fn handler_starts_create_blocks_and_regions() {
		let mut c = CodeBuilder::new(2, 2);
		let h = c.new_label();
		c.op(0xbf); // athrow
		c.mark(h);
		c.op(0x4d); // astore_1
		c.op(0xb1); // return
		let body = c.build();
		let code: Vec<u8> = body[8..].to_vec();
		let pool = ConstPool::parse(&mut BinReader::new(&[0u8, 0x01])).unwrap();
		let insns = decode_code(&code, &pool).unwrap();
		let handler = ExceptionHandler { start: 0, end: 1, handler_pc: 1, catch_type: None };
		let cfg = build_cfg(&insns, code.len() as u32, &[handler], &[]).unwrap();
		assert_eq!(cfg.blocks[1].kind, BlockKind::Finally);
		assert_eq!(cfg.try_regions.len(), 1);
		assert_eq!(cfg.blocks[1].catch_types, vec![None]);
		assert_eq!(cfg.handler_blocks, vec![1]);
	}

	#[test]
	fn clean_region_check_rejects_side_exits() {
		let mut c = CodeBuilder::new(2, 2);
		let l1 = c.new_label();
		let end = c.new_label();
		c.op(0x1a);
		c.branch(0x99, l1); // ifeq -> l1
		c.branch(0xa7, end); // goto end
		c.mark(l1);
		c.op(0xb1); // return (exits the region)
		c.mark(end);
		c.op(0xb1);
		let cfg = cfg_of(c);
		// blocks 1 (the goto) only reaches 3, so region [1] with exit 3 is clean
		assert!(cfg.is_clean_region(&[1], Some(3)));
		// block 2 returns: no exit block, so it is not clean
		assert!(!cfg.is_clean_region(&[2], Some(3)));
	}

	#[test]
	fn inverted_conditions() {
		let pool = ConstPool::parse(&mut BinReader::new(&[0u8, 0x01])).unwrap();
		let insns = decode_code(&[0x99u8, 0x00, 0x01], &pool).unwrap();
		assert_eq!(inverted_cond(&insns[0]), Some(Cond::Ne));
		let insns = decode_code(&[0xc6u8, 0x00, 0x01], &pool).unwrap();
		assert_eq!(inverted_cond(&insns[0]), Some(Cond::Ne));
		let insns = decode_code(&[0xb1u8], &pool).unwrap();
		assert_eq!(inverted_cond(&insns[0]), None);
	}
}
