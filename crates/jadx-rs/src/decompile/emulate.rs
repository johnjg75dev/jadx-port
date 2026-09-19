//! Operand-stack emulation: bytecode -> expressions and straight-line statements.
//!
//! This replaces the longest part of jadx's pipeline (`JadxCodeReader` ->
//! `SsaBuildAndCheckPass` -> `TypeInfoCollector`/`TypeOfVisitor` ->
//! `RegistersMapper` -> `ProcessVarAssigns`), which turns Dalvik registers into
//! SSA `JVar`s and then out of SSA again. The approach used here is the classic
//! JVM one (Androvich et al., "Parsing and Pattern Recognition" -- the technique
//! behind JDC and jad): walk the CFG in reverse post order emulating the operand
//! stack, and build an expression tree for every value instead of executing it.
//!
//! A stack value that has to survive a control-flow join is spilled into a
//! synthetic temporary (`v$1`), which the structuring pass later inlines back via
//! [`super::passes::inline_temps`] -- the mirror image of jadx removing phi nodes
//! by inserting assignments.

use std::collections::HashMap;

use crate::args::Args;
use crate::error::{JadxError, Res};
use crate::java::attrs::{CodeAttr, LocalVar};
use crate::java::class_file::MethodData;
use crate::java::const_pool::{Cst, RefInfo};
use crate::java::insn::{CmpKind, Cond, Insn, InvokeKind, Sem, SlotTy};
use crate::types::JType;

use super::cfg::{BlockKind, Cfg};
use super::ir::{body_uses, Expr, NewKind, Stmt};

/// Info about one local variable slot (jadx: `JVar` plus its `IVarUse` record).
#[derive(Debug, Clone)]
pub struct LocalInfo {
	pub index: u32,
	pub name: String,
	pub ty: JType,
	/// `true` for the temporaries this pass invents for stack spills
	pub synthetic: bool,
	/// declared at the top of the body instead of at its first assignment
	pub hoist_decl: bool,
	/// parameter slot: already declared by the method signature
	pub is_param: bool,
	/// slot 0 of an instance method
	pub is_this: bool,
}

/// What emulating one basic block produced.
#[derive(Debug, Clone, Default)]
pub struct BlockResult {
	/// straight-line statements, without the trailing branch
	pub stmts: Vec<Stmt>,
	/// condition of a conditional branch, as executed
	pub cond: Option<Expr>,
	/// successor blocks, in CFG order (taken first for `if`, default first for switch)
	pub targets: Vec<usize>,
	/// switch key per successor; `None` marks the default arm
	pub switch_keys: Vec<Option<i32>>,
	/// the value switched on
	pub switch_subject: Option<Expr>,
	/// what is still on the operand stack when the block ends
	pub leftover: Vec<Expr>,
	/// for handler blocks: the slot the exception is stored into, i.e. the
	/// `catch` parameter (`None` when the handler ignores the exception)
	pub catch_var: Option<u32>,
	/// `LineNumberTable` entry for the block start
	pub line: Option<u32>,
}

/// Result of emulating a whole method.
#[derive(Debug, Clone, Default)]
pub struct MethodCode {
	pub locals: Vec<LocalInfo>,
	pub blocks: Vec<BlockResult>,
	/// locals that need a declaration at the top of the body
	pub declarations: Vec<LocalInfo>,
	/// `this` is not included; one entry per declared argument, in argument order
	/// (a `long`/`double` argument occupies two slots, so the slot indices are not
	/// contiguous)
	pub params: Vec<LocalInfo>,
	/// first free local slot, i.e. `this` plus the parameters (wide types count
	/// twice); everything from this index on is a body local
	pub params_slots: u32,
	/// set when the method could not be fully decoded; the caller renders it as
	/// an error comment, like jadx's `showInconsistentCode` fallback
	pub error: Option<String>,
}

/// What one instruction produced besides its stack/statement effect.
enum Outcome {
	None,
	Cond(Expr),
	Switch(Expr),
}

/// Emulates a single method. `cfg` must have been built from the same `insns`.
pub struct Emulator<'a> {
	insns: &'a [Insn],
	cfg: &'a Cfg,
	cls_name: &'a str,
	args: &'a Args,

	locals: Vec<LocalInfo>,
	params_slots: u32,
	/// how many `astore`-style writes each slot receives in the whole method
	assign_count: HashMap<u32, usize>,
	/// (block, slot) -> type, from the `StackMapTable` frame of that block
	frame_types: HashMap<(usize, u32), JType>,
	temp_prefix: &'static str,
	next_temp: u32,
	/// `new X()` handles: id -> constructor arguments, completed on `invokespecial`
	pending_inits: HashMap<u32, Vec<Expr>>,
	next_new_id: u32,
	is_ctor: bool,
	cur_block: usize,
}

impl<'a> Emulator<'a> {
	/// `code` and `method` are only read while building the local variable table;
	/// `cfg` must describe `insns` (same `Code` attribute).
	pub fn new(
		insns: &'a [Insn],
		cfg: &'a Cfg,
		code: &'a CodeAttr,
		method: &'a MethodData,
		is_static: bool,
		cls_name: &'a str,
		args: &'a Args,
	) -> Emulator<'a> {
		// --- pre-pass over the instruction list: how often is each slot written?
		let mut assign_count: HashMap<u32, usize> = HashMap::new();
		let mut written: HashMap<u32, bool> = HashMap::new();
		let mut read_early: HashMap<u32, bool> = HashMap::new();
		for i in insns {
			match i.sem() {
				Sem::Store(_) | Sem::Iinc => {
					let l = i.local();
					*assign_count.entry(l).or_insert(0) += 1;
					written.insert(l, true);
				}
				Sem::Load(_) => {
					let l = i.local();
					if !written.get(&l).copied().unwrap_or(false) {
						read_early.insert(l, true);
					}
				}
				_ => {}
			}
		}

		// --- types announced by the StackMapTable, per block
		let mut frame_types: HashMap<(usize, u32), JType> = HashMap::new();
		if let Some(map) = code.attrs.stack_map.as_ref() {
			for b in &cfg.blocks {
				if let Some(f) = map.get_for(b.start) {
					for slot in 0..(code.max_locals as u32) {
						if let Some(t) = f.local_type(slot) {
							frame_types.insert((b.id, slot), t);
						}
					}
				}
			}
		}

		// --- locals: `this`, the parameters, then the remaining slots
		let mut locals: Vec<LocalInfo> = Vec::new();
		let mut slot: u32 = 0;
		if !is_static {
			locals.push(LocalInfo {
				index: slot,
				name: "this".to_string(),
				ty: JType::Class(cls_name.to_string()),
				synthetic: false,
				hoist_decl: false,
				is_param: false,
				is_this: true,
			});
			slot = 1;
		}
		let params_slots = slot + method.proto.args.len() as u32;
		for (i, a) in method.proto.args.iter().enumerate() {
			let named = match method.attrs.method_params.get(i).and_then(|p| p.name.clone()) {
				Some(n) if is_java_identifier(&n) => Some(n),
				_ => None,
			};
			let name = named.or_else(|| debug_name(&code.attrs.local_vars, slot)).unwrap_or_else(|| default_local_name(a, slot));
			locals.push(LocalInfo {
				index: slot,
				name,
				ty: a.clone(),
				synthetic: false,
				hoist_decl: false,
				is_param: true,
				is_this: false,
			});
			slot += if a.is_wide() { 2 } else { 1 };
		}
		let max_locals = (code.max_locals as usize).max(slot as usize);
		while locals.len() < max_locals {
			let idx = locals.len() as u32;
			let ty = frame_types
				.get(&(0usize, idx))
				.cloned()
				.or_else(|| debug_type(&code.attrs.local_vars, idx))
				.unwrap_or(JType::Void);
			let dbg = debug_name(&code.attrs.local_vars, idx).filter(|n| is_java_identifier(n));
			let name = dbg.unwrap_or_else(|| default_local_name(&ty, idx));
			// Decide up front whether the declaration has to be hoisted: a slot
			// written more than once, or read before its first write, cannot use
			// `T x = e;` at its assignment site.
			let count = assign_count.get(&idx).copied().unwrap_or(0);
			let early = read_early.get(&idx).copied().unwrap_or(false);
			let hoist = count != 1 || early;
			locals.push(LocalInfo {
				index: idx,
				name,
				ty,
				synthetic: false,
				hoist_decl: hoist,
				is_param: false,
				is_this: false,
			});
		}
		// `LocalVariableTable` scopes legitimately reuse one name for several
		// slots; jadx fixes that in `VarNamesCollector`, here in `make_names_unique`
		make_names_unique(&mut locals);
		let temp_prefix = pick_temp_prefix(&locals);

		Emulator {
			insns,
			cfg,
			cls_name,
			args,
			locals,
			params_slots,
			assign_count,
			frame_types,
			temp_prefix,
			next_temp: 1,
			pending_inits: HashMap::new(),
			next_new_id: 1,
			is_ctor: method.name == "<init>",
			cur_block: 0,
		}
	}

	fn frame_type(&self, index: u32) -> Option<JType> {
		self.frame_types.get(&(self.cur_block, index)).cloned()
	}

	/// Register a slot and refine its type as soon as one is known (jadx:
	/// `VarType` refinement in `TypeOfVisitor`).
	fn ensure_local(&mut self, index: u32, hint: Option<JType>) {
		while self.locals.len() <= index as usize {
			let idx = self.locals.len() as u32;
			self.locals.push(LocalInfo {
				index: idx,
				name: format!("{}{}", self.temp_prefix, idx),
				ty: JType::Void,
				synthetic: true,
				hoist_decl: true,
				is_param: false,
				is_this: false,
			});
		}
		let t = match hint {
			Some(t) if t != JType::Void => t,
			_ => return,
		};
		if self.locals[index as usize].ty == JType::Void {
			self.locals[index as usize].ty = t;
		}
	}

	fn local_expr(&mut self, index: u32) -> Expr {
		self.ensure_local(index, self.frame_type(index));
		let info = &self.locals[index as usize];
		Expr::Local { index, name: info.name.clone(), ty: info.ty.clone() }
	}

	/// A fresh slot for spilling one stack value across a control-flow join.
	fn new_temp(&mut self, ty: JType) -> u32 {
		let index = self.locals.len() as u32;
		let name = loop {
			let cand = format!("{}{}", self.temp_prefix, self.next_temp);
			self.next_temp += 1;
			if !self.locals.iter().any(|l| l.name == cand) {
				break cand;
			}
		};
		self.locals.push(LocalInfo {
			index,
			name,
			ty,
			synthetic: true,
			hoist_decl: true,
			is_param: false,
			is_this: false,
		});
		index
	}

	/// `T x = e;` for the unique definition of a slot, `x = e;` otherwise.
	fn store_local(&mut self, index: u32, value: Expr, stmts: &mut Vec<Stmt>) {
		self.ensure_local(index, Some(value.ty()));
		let info = self.locals[index as usize].clone();
		let target = Expr::Local { index, name: info.name.clone(), ty: info.ty.clone() };
		if info.is_this {
			// `this` is final, so the store cannot be expressed in Java. The value
			// has already been consumed; leave a note about the impossible bytecode.
			stmts.push(Stmt::Comment { text: "cannot assign to `this` (bytecode writes slot 0)".to_string() });
			return;
		}
		if info.is_param || info.hoist_decl {
			stmts.push(Stmt::Assign { target: Box::new(target), value: Box::new(value) });
		} else {
			stmts.push(Stmt::LocalDef { name: info.name, ty: info.ty, init: Some(value) });
		}
	}

	/// Emulate every reachable block in reverse post order, spilling the stack at
	/// joins. Handler blocks are emulated from their own root, mirroring jadx's
	/// `RegionMaker` which starts regions at `catch` blocks as well.
	pub fn run(mut self) -> MethodCode {
		let cfg: &'a Cfg = self.cfg;
		let insns: &'a [Insn] = self.insns;
		let n = cfg.blocks.len();
		let mut result: Vec<BlockResult> = (0..n).map(|_| BlockResult::default()).collect();
		// temps a block must load on entry, decided by its predecessors
		let mut incoming: Vec<Option<Vec<u32>>> = vec![None; n];
		// a block with a single predecessor inherits that predecessor's stack
		// directly, so no spill is needed
		let mut direct: Vec<Option<Vec<Expr>>> = vec![None; n];
		let mut error: Option<String> = None;

		for bi in self.emulation_order() {
			self.cur_block = bi;
			let b = &cfg.blocks[bi];
			let is_handler = !matches!(b.kind, BlockKind::Normal);
			let block_insns = &insns[b.first..b.last];
			// At a handler entry the exception object sits on the (otherwise empty)
			// stack and javac stores it immediately: consume that store and remember
			// the slot as the `catch` parameter.
			let catch_var = if is_handler {
				match block_insns.first().map(|i| i.sem()) {
					Some(Sem::Store(_)) => Some(block_insns[0].local()),
					_ => None,
				}
			} else {
				None
			};
			if let Some(cv) = catch_var {
				let ty = b.catch_types.iter().flatten().next().cloned();
				self.ensure_local(cv, ty);
			}

			let mut stack: Vec<Expr> = if let Some(v) = direct[bi].clone() {
				v
			} else if let Some(temps) = incoming[bi].clone() {
				let mut v: Vec<Expr> = Vec::with_capacity(temps.len());
				for t in temps {
					v.push(self.local_expr(t));
				}
				v
			} else {
				Vec::new()
			};
			let mut stmts: Vec<Stmt> = Vec::new();
			if self.args.print_line_numbers {
				if let Some(l) = b.line {
					stmts.push(Stmt::LineNumber { line: l });
				}
			}

			let mut cond: Option<Expr> = None;
			let mut switch_subject: Option<Expr> = None;
			let mut switch_keys: Vec<Option<i32>> = Vec::new();
			let mut failed = false;
			for (ii, insn) in block_insns.iter().enumerate() {
				if ii == 0 && catch_var == Some(insn.local()) {
					continue;
				}
				match self.emulate_insn(insn, &mut stack, &mut stmts) {
					Ok(Outcome::None) => {}
					Ok(Outcome::Cond(c)) => cond = Some(c),
					Ok(Outcome::Switch(e)) => switch_subject = Some(e),
					Err(e) => {
						error = Some(e.message);
						failed = true;
						break;
					}
				}
			}
			if failed {
				result[bi] = BlockResult {
					stmts,
					cond,
					targets: b.successors.clone(),
					switch_keys,
					switch_subject,
					leftover: Vec::new(),
					catch_var,
					line: b.line,
				};
				break;
			}

			// `catch (X e) { }` and the trailing rethrow of a `finally` block are
			// implicit in the bytecode: drop the rethrow, the Java `finally` does it.
			if is_handler {
				if let Some(cv) = catch_var {
					loop {
						let drop_last = match stmts.last() {
							Some(Stmt::Throw { value }) => value.local_index() == Some(cv),
							_ => false,
						};
						if !drop_last {
							break;
						}
						stmts.pop();
					}
				}
			}

			if switch_subject.is_some() {
				if let Some((_, pairs)) = block_insns.last().and_then(|i| i.switch_data()) {
					let mut key_of: HashMap<usize, i32> = HashMap::new();
					for (key, t) in &pairs {
						if let Some(tb) = cfg.block_at(*t) {
							key_of.entry(tb).or_insert(*key);
						}
					}
					switch_keys = b.successors.iter().map(|t| key_of.get(t).copied()).collect();
				}
			}

			// propagate what is left on the stack to the successors
			let leftover = stack.clone();
			let mut shape_error: Option<String> = None;
			if !leftover.is_empty() {
				for &s in &b.successors {
					if s >= n || !matches!(cfg.blocks[s].kind, BlockKind::Normal) {
						continue;
					}
					let single_pred = {
						let sb = &cfg.blocks[s];
						sb.predecessors.len() == 1 && sb.predecessors[0] == bi
					};
					if single_pred && direct[s].is_none() {
						direct[s] = Some(leftover.clone());
						continue;
					}
					let temps: Vec<u32> = match incoming[s].clone() {
						Some(t) => t,
						None => {
							let mut v: Vec<u32> = Vec::with_capacity(leftover.len());
							for item in leftover.iter() {
								v.push(self.new_temp(item.ty()));
							}
							incoming[s] = Some(v.clone());
							v
						}
					};
					if temps.len() != leftover.len() {
						shape_error = Some(format!(
							"stack shape mismatch entering block {}: {} value(s) pushed, {} expected",
							s,
							leftover.len(),
							temps.len()
						));
						break;
					}
					for (t, item) in temps.iter().zip(leftover.iter()) {
						self.ensure_local(*t, Some(item.ty()));
						let info = self.locals[*t as usize].clone();
						stmts.push(Stmt::Assign {
							target: Box::new(Expr::Local { index: *t, name: info.name, ty: info.ty }),
							value: Box::new(item.clone()),
						});
					}
				}
			}

			result[bi] = BlockResult {
				stmts,
				cond,
				targets: b.successors.clone(),
				switch_keys,
				switch_subject,
				leftover,
				catch_var,
				line: b.line,
			};
			if shape_error.is_some() {
				error = shape_error;
				break;
			}
		}

		// attach the arguments collected by `invokespecial` to every `new` handle
		let inits = self.pending_inits.clone();
		for r in result.iter_mut() {
			fix_new_stmts(&mut r.stmts, &inits);
			if let Some(c) = r.cond.as_mut() {
				fix_new_expr(c, &inits);
			}
			if let Some(s) = r.switch_subject.as_mut() {
				fix_new_expr(s, &inits);
			}
			for l in r.leftover.iter_mut() {
				fix_new_expr(l, &inits);
			}
		}

		self.finish(result, error)
	}

	/// Blocks to emulate, in an order where each block follows a predecessor:
	/// RPO from the entry block, then RPO from each handler entry.
	fn emulation_order(&self) -> Vec<usize> {
		let cfg = self.cfg;
		let n = cfg.blocks.len();
		let mut seen = vec![false; n];
		let mut out: Vec<usize> = Vec::with_capacity(n);
		for root in std::iter::once(cfg.entry()).chain(cfg.handler_blocks.iter().copied()) {
			for b in cfg.rpo_from(root) {
				if b < n && !seen[b] {
					seen[b] = true;
					if cfg.blocks[b].reachable {
						out.push(b);
					}
				}
			}
		}
		out
	}

	/// Finalize local types and compute the hoisted declaration list.
	fn finish(&mut self, blocks: Vec<BlockResult>, error: Option<String>) -> MethodCode {
		let all: Vec<Stmt> = blocks.iter().flat_map(|b| b.stmts.clone()).collect();
		let params: Vec<LocalInfo> = self.locals.iter().filter(|l| l.is_param).cloned().collect();
		let mut declarations: Vec<LocalInfo> = Vec::new();
		for i in 0..self.locals.len() {
			let idx = i as u32;
			let mut info = self.locals[i].clone();
			if info.is_param || info.is_this {
				continue;
			}
			let used = body_uses(&all, idx);
			if info.ty == JType::Void && (used || self.assign_count.contains_key(&idx)) {
				info.ty = JType::object();
			}
			let count = self.assign_count.get(&idx).copied().unwrap_or(0);
			let needs_decl = if info.synthetic { used } else { count > 0 && info.hoist_decl && used };
			if needs_decl {
				declarations.push(info.clone());
			}
			self.locals[i] = info;
		}
		MethodCode {
			locals: std::mem::take(&mut self.locals),
			blocks,
			declarations,
			params,
			params_slots: self.params_slots,
			error,
		}
	}

	/// One instruction, expressed on the emulated stack.
	fn emulate_insn(
		&mut self,
		insn: &Insn,
		stack: &mut Vec<Expr>,
		stmts: &mut Vec<Stmt>,
	) -> Res<Outcome> {
		let sem = insn.sem();
		match sem {
			Sem::Nop => {}
			Sem::Invalid => {
				return Err(JadxError::format(format!("illegal opcode 0x{:02x} at offset {}", insn.code, insn.off)));
			}
			Sem::AconstNull => stack.push(Expr::Const(Cst::Null)),
			Sem::Iconst | Sem::Bipush | Sem::Sipush => {
				let v = insn.int_const().unwrap_or(0);
				stack.push(Expr::Const(Cst::Int(v)));
			}
			Sem::Lconst => {
				let v = match insn.cst() {
					Some(Cst::Long(v)) => *v,
					_ => insn.int_const().unwrap_or(0) as i64,
				};
				stack.push(Expr::Const(Cst::Long(v)));
			}
			Sem::Fconst => {
				let v = match insn.cst() {
					Some(Cst::Float(v)) => *v,
					_ => 0.0f32,
				};
				stack.push(Expr::Const(Cst::Float(v)));
			}
			Sem::Dconst => {
				let v = match insn.cst() {
					Some(Cst::Double(v)) => *v,
					_ => 0.0f64,
				};
				stack.push(Expr::Const(Cst::Double(v)));
			}
			Sem::Ldc | Sem::LdcWide => {
				let c = insn.cst().cloned().unwrap_or(Cst::Null);
				stack.push(Expr::Const(c));
			}

			Sem::Load(_) => {
				let idx = insn.local();
				let e = self.local_expr(idx);
				stack.push(e);
			}
			Sem::Store(_) => {
				let idx = insn.local();
				let v = pop(stack, insn)?;
				self.store_local(idx, v, stmts);
			}
			Sem::ArrayLoad(ty) => {
				let index = pop(stack, insn)?;
				let arr = pop(stack, insn)?;
				let el = match arr.ty() {
					JType::Array(inner) => *inner,
					_ => ty.jtype(),
				};
				stack.push(Expr::Array { arr: Box::new(arr), index: Box::new(index), ty: el });
			}
			Sem::ArrayStore(_) => {
				let value = pop(stack, insn)?;
				let index = pop(stack, insn)?;
				let arr = pop(stack, insn)?;
				let ty = value.ty();
				stmts.push(Stmt::Assign {
					target: Box::new(Expr::Array { arr: Box::new(arr), index: Box::new(index), ty }),
					value: Box::new(value),
				});
			}

			Sem::Pop => {
				let v = pop(stack, insn)?;
				if v.has_side_effects() {
					stmts.push(Stmt::ExprStmt { expr: v });
				}
			}
			Sem::Pop2 => {
				// `pop2` removes two ints or one category-2 value
				let top = pop(stack, insn)?;
				if !top.ty().is_wide() {
					let second = pop(stack, insn)?;
					if second.has_side_effects() {
						stmts.push(Stmt::ExprStmt { expr: second });
					}
				}
				if top.has_side_effects() {
					stmts.push(Stmt::ExprStmt { expr: top });
				}
			}
			Sem::Dup => {
				let top = match stack.last() {
					Some(v) => v.clone(),
					None => return Err(JadxError::format(format!("dup on empty stack at offset {}", insn.off))),
				};
				stack.push(top);
			}
			Sem::DupX1 => {
				let v1 = pop(stack, insn)?;
				let v2 = pop(stack, insn)?;
				stack.push(v1.clone());
				stack.push(v2);
				stack.push(v1);
			}
			Sem::DupX2 => {
				if stack.len() < 3 {
					return Err(JadxError::format(format!(
						"dup_x2 with only {} stack value(s) at offset {}",
						stack.len(),
						insn.off
					)));
				}
				// ..., v3, v2, v1  ->  ..., v1, v3, v2, v1
				let v1 = pop(stack, insn)?;
				let v2 = pop(stack, insn)?;
				let v3 = pop(stack, insn)?;
				stack.push(v1.clone());
				stack.push(v3);
				stack.push(v2);
				stack.push(v1);
			}
			Sem::Dup2 => {
				let top = pop(stack, insn)?;
				if top.ty().is_wide() {
					// one category-2 value: duplicate it
					stack.push(top.clone());
					stack.push(top);
				} else {
					// two ints v1, v2 -> v2, v1, v2 -- here: push v1 again below v2
					let below = pop(stack, insn)?;
					stack.push(below.clone());
					stack.push(top.clone());
					stack.push(below);
					stack.push(top);
				}
			}
			Sem::Dup2X1 => {
				if stack.len() < 3 {
					return Err(JadxError::format(format!(
						"dup2_x1 with only {} stack value(s) at offset {}",
						stack.len(),
						insn.off
					)));
				}
				// ..., v3, v2, v1  ->  ..., v2, v1, v3, v2, v1
				let v1 = pop(stack, insn)?;
				let v2 = pop(stack, insn)?;
				let v3 = pop(stack, insn)?;
				stack.push(v2.clone());
				stack.push(v1.clone());
				stack.push(v3);
				stack.push(v2);
				stack.push(v1);
			}
			Sem::Dup2X2 => {
				if stack.len() < 4 {
					return Err(JadxError::format(format!(
						"dup2_x2 with only {} stack value(s) at offset {}",
						stack.len(),
						insn.off
					)));
				}
				// form 2 (four single values): ..., v4, v3, v2, v1 -> v2, v1, v4, v3, v2, v1
				let v1 = pop(stack, insn)?;
				let v2 = pop(stack, insn)?;
				let v3 = pop(stack, insn)?;
				let v4 = pop(stack, insn)?;
				stack.push(v2.clone());
				stack.push(v1.clone());
				stack.push(v4);
				stack.push(v3);
				stack.push(v2);
				stack.push(v1);
			}
			Sem::Swap => {
				let v1 = pop(stack, insn)?;
				let v2 = pop(stack, insn)?;
				stack.push(v1);
				stack.push(v2);
			}

			Sem::Bin(op, ty) => {
				let b = pop(stack, insn)?;
				let a = pop(stack, insn)?;
				// JVMS 2.11.1: the result type is the operand type of the opcode, so
				// `iadd` on bytes/chars/shorts yields `int`
				stack.push(Expr::Bin { op, a: Box::new(a), b: Box::new(b), ty: ty.jtype() });
			}
			Sem::Neg(ty) => {
				let a = pop(stack, insn)?;
				stack.push(Expr::Neg { a: Box::new(a), ty: ty.jtype() });
			}
			Sem::Shift(op, long) => {
				let b = pop(stack, insn)?;
				let a = pop(stack, insn)?;
				let ty = if long { JType::Long } else { JType::Int };
				stack.push(Expr::Shift { op, a: Box::new(a), b: Box::new(b), ty });
			}
			Sem::Bit(op, long) => {
				let b = pop(stack, insn)?;
				let a = pop(stack, insn)?;
				let ty = if long { JType::Long } else { JType::Int };
				stack.push(Expr::Bit { op, a: Box::new(a), b: Box::new(b), ty });
			}
			Sem::Iinc => {
				let index = insn.local();
				let delta = insn.inc_delta().unwrap_or(1);
				self.ensure_local(index, Some(JType::Int));
				let info = self.locals[index as usize].clone();
				stack.push(Expr::Inc { index, name: info.name, delta, pre: false, ty: JType::Int });
			}
			Sem::Conv(c) => {
				let a = pop(stack, insn)?;
				let to = c.cast_type();
				if a.ty() == to {
					stack.push(a);
				} else {
					stack.push(Expr::Cast { to, a: Box::new(a) });
				}
			}
			Sem::Cmp(kind) => {
				let b = pop(stack, insn)?;
				let a = pop(stack, insn)?;
				stack.push(Expr::Cmp3 { kind, a: Box::new(a), b: Box::new(b) });
			}

			Sem::If(c) => {
				let v = pop(stack, insn)?;
				// `lcmp` followed by `iflt` reads as a direct comparison of the two
				// longs (jadx: `IfTester` + `RegionProcessors` do the same rewrite)
				let out = match v {
					Expr::Cmp3 { kind: CmpKind::Long, a, b } => Expr::Cmp { op: c, a, b },
					other => Expr::Cmp { op: c, a: Box::new(other), b: Box::new(Expr::Const(Cst::Int(0))) },
				};
				return Ok(Outcome::Cond(out));
			}
			Sem::IfCmp(c) | Sem::IfACmp(c) => {
				let b = pop(stack, insn)?;
				let a = pop(stack, insn)?;
				return Ok(Outcome::Cond(Expr::Cmp { op: c, a: Box::new(a), b: Box::new(b) }));
			}
			Sem::IfNull => {
				let a = pop(stack, insn)?;
				return Ok(Outcome::Cond(Expr::Cmp { op: Cond::Eq, a: Box::new(a), b: Box::new(Expr::Const(Cst::Null)) }));
			}
			Sem::IfNonNull => {
				let a = pop(stack, insn)?;
				return Ok(Outcome::Cond(Expr::Cmp { op: Cond::Ne, a: Box::new(a), b: Box::new(Expr::Const(Cst::Null)) }));
			}
			Sem::Goto => {}
			Sem::Jsr => {
				// `jsr`/`ret` (pre-1.6 `finally`) needs a second return edge per
				// subroutine; this port reports it instead of miscompiling it.
				stmts.push(Stmt::Comment {
					text: format!("jsr to offset {} not supported (jadx: inconsistent code)", insn.branch().unwrap_or(0)),
				});
			}
			Sem::Ret => {
				stmts.push(Stmt::Comment { text: "ret instruction not supported".to_string() });
			}
			Sem::TableSwitch | Sem::LookupSwitch => {
				let v = pop(stack, insn)?;
				return Ok(Outcome::Switch(v));
			}

			Sem::Return(ty) => {
				if ty == SlotTy::Void {
					stmts.push(Stmt::Return { value: None });
				} else {
					let v = pop(stack, insn)?;
					stmts.push(Stmt::Return { value: Some(v) });
				}
			}
			Sem::Athrow => {
				let v = pop(stack, insn)?;
				stmts.push(Stmt::Throw { value: v });
			}

			Sem::GetStatic | Sem::GetField => {
				let is_static = sem == Sem::GetStatic;
				let ri = insn.field_ref().cloned().ok_or_else(|| JadxError::format(format!("unresolved field reference at offset {}", insn.off)))?;
				let obj = if is_static { None } else { Some(Box::new(pop(stack, insn)?)) };
				let ty = JType::from_descriptor(&ri.desc);
				stack.push(Expr::Field { obj, owner: ri.class, name: ri.name, ty, is_static });
			}
			Sem::PutStatic | Sem::PutField => {
				let is_static = sem == Sem::PutStatic;
				let ri = insn.field_ref().cloned().ok_or_else(|| JadxError::format(format!("unresolved field reference at offset {}", insn.off)))?;
				let value = pop(stack, insn)?;
				let obj = if is_static { None } else { Some(Box::new(pop(stack, insn)?)) };
				let ty = JType::from_descriptor(&ri.desc);
				stmts.push(Stmt::Assign {
					target: Box::new(Expr::Field { obj, owner: ri.class, name: ri.name, ty, is_static }),
					value: Box::new(value),
				});
			}
			Sem::Invoke(kind) => {
				let ri: RefInfo = insn
					.method_ref()
					.cloned()
					.ok_or_else(|| JadxError::format(format!("unresolved method reference at offset {}", insn.off)))?;
				let proto = JType::parse_method_descriptor(&ri.desc);
				// a `long`/`double` occupies two stack slots but counts as one value
				let mut args: Vec<Expr> = Vec::with_capacity(proto.args.len());
				for _ in 0..proto.args.len() {
					args.insert(0, pop(stack, insn)?);
				}
				let mut obj: Option<Box<Expr>> = match kind {
					InvokeKind::Static | InvokeKind::Dynamic => None,
					_ => Some(Box::new(pop(stack, insn)?)),
				};
				let ret = proto.ret.clone();

				if kind == InvokeKind::Special && ri.name == "<init>" {
					let receiver_is_this = obj.as_ref().map(|e| matches!(**e, Expr::Local { index: 0, .. })).unwrap_or(false);
					if receiver_is_this && self.is_ctor {
						// `invokespecial super.<init>` / `this.<init>` at the top of a
						// constructor body: jadx prints it as `super(...)`/`this(...)`
						let new_kind = if ri.class == self.cls_name { NewKind::This } else { NewKind::Super };
						stmts.push(Stmt::ExprStmt {
							expr: Expr::New {
								id: u32::MAX,
								ty: JType::Class(ri.class.clone()),
								args,
								dims: Vec::new(),
								kind: new_kind,
							},
						});
						return Ok(Outcome::None);
					}
					// `new X(); dup; <args>; invokespecial X.<init>()`: the receiver is
					// one copy of the allocation, the other copy stays on the stack.
					let new_id = match obj.as_deref() {
						Some(Expr::New { id, .. }) if *id != u32::MAX => Some(*id),
						_ => None,
					};
					if let Some(id) = new_id {
						self.pending_inits.insert(id, args);
						let dup_left = matches!(stack.last(), Some(Expr::New { id: other, .. }) if *other == id);
						if !dup_left {
							// no `dup` to keep alive: the allocation *is* the value
							let receiver = obj.take().expect("checked above");
							stack.push(*receiver);
						}
						return Ok(Outcome::None);
					}
				}

				if ret == JType::Void {
					stmts.push(Stmt::ExprStmt { expr: Expr::Invoke { kind, obj, args, target: ri, ret } });
				} else {
					stack.push(Expr::Invoke { kind, obj, args, target: ri, ret });
				}
			}

			Sem::New => {
				let ty = insn.ty().cloned().unwrap_or_else(JType::object);
				let id = self.next_new_id;
				self.next_new_id += 1;
				stack.push(Expr::New { id, ty, args: Vec::new(), dims: Vec::new(), kind: NewKind::Class });
			}
			Sem::NewArray | Sem::ANewArray => {
				let len = pop(stack, insn)?;
				let ty = insn.ty().cloned().unwrap_or_else(JType::object);
				stack.push(Expr::New { id: u32::MAX, ty: JType::array(ty), args: Vec::new(), dims: vec![len], kind: NewKind::Array });
			}
			Sem::MultiANewArray => {
				let dims_n = insn.dims().unwrap_or(1) as usize;
				let ty = insn.ty().cloned().unwrap_or_else(JType::object);
				let mut dims: Vec<Expr> = Vec::with_capacity(dims_n);
				for _ in 0..dims_n {
					dims.insert(0, pop(stack, insn)?);
				}
				stack.push(Expr::New { id: u32::MAX, ty, args: Vec::new(), dims, kind: NewKind::Array });
			}
			Sem::ArrayLength => {
				let a = pop(stack, insn)?;
				stack.push(Expr::Length { a: Box::new(a) });
			}
			Sem::CheckCast => {
				let a = pop(stack, insn)?;
				let to = insn.ty().cloned().unwrap_or_else(JType::object);
				if a.ty() == to {
					stack.push(a);
				} else {
					stack.push(Expr::Cast { to, a: Box::new(a) });
				}
			}
			Sem::InstanceOf => {
				let a = pop(stack, insn)?;
				let ty = insn.ty().cloned().unwrap_or_else(JType::object);
				stack.push(Expr::InstanceOf { a: Box::new(a), ty });
			}
			Sem::MonitorEnter | Sem::MonitorExit => {
				// jadx rebuilds `synchronized` regions from these opcodes in its
				// `RegionsMonitorExtractionHelper`; this port keeps the monitored
				// expression and marks the site so the output still compiles.
				let a = pop(stack, insn)?;
				let what = if sem == Sem::MonitorEnter { "monitorenter" } else { "monitorexit" };
				stmts.push(Stmt::Comment {
					text: format!("{}: synchronized block reconstruction is not ported", what),
				});
				if a.has_side_effects() {
					stmts.push(Stmt::ExprStmt { expr: a });
				}
			}
		}
		Ok(Outcome::None)
	}
}

/// Free function so the caller can keep `&mut self` for the locals table.
fn pop(stack: &mut Vec<Expr>, insn: &Insn) -> Res<Expr> {
	match stack.pop() {
		Some(v) => Ok(v),
		None => Err(JadxError::format(format!(
			"operand stack underflow at offset {} (`{}`)",
			insn.off,
			insn.name()
		))),
	}
}

/// `LocalVariableTable` name for a slot, if any (jadx: `useDebugVarsNames`).
fn debug_name(vars: &[LocalVar], slot: u32) -> Option<String> {
	vars.iter().find(|v| v.index == slot).map(|v| v.name.clone())
}

fn debug_type(vars: &[LocalVar], slot: u32) -> Option<JType> {
	vars.iter().find(|v| v.index == slot).map(|v| JType::from_descriptor(&v.desc))
}

/// Fallback names, mirroring jadx's `r3$`/`i2$` scheme: the letter describes the
/// type, the number is the slot index.
pub fn default_local_name(ty: &JType, index: u32) -> String {
	let c = match ty {
		JType::Long => 'j',
		JType::Float => 'f',
		JType::Double => 'd',
		JType::Boolean | JType::Byte | JType::Short | JType::Int | JType::Char => 'i',
		_ if ty.is_reference() => 'r',
		_ => 'v',
	};
	format!("{}{}$", c, index)
}

/// jadx: `VarNamesCollector` -- two slots may share a debug name when the scopes
/// do not overlap, but the decompiled output needs one declaration per name.
fn make_names_unique(locals: &mut [LocalInfo]) {
	let mut used: HashMap<String, ()> = HashMap::new();
	for l in locals.iter_mut() {
		if l.name.is_empty() {
			l.name = default_local_name(&l.ty, l.index);
		}
		if used.contains_key(&l.name) {
			let base = l.name.clone();
			let mut n = 1usize;
			loop {
				let cand = format!("{}${}", base, n);
				n += 1;
				if !used.contains_key(&cand) {
					l.name = cand;
					break;
				}
			}
		}
		used.insert(l.name.clone(), ());
	}
}

/// Pick a temp prefix that cannot collide with a real variable name.
fn pick_temp_prefix(locals: &[LocalInfo]) -> &'static str {
	const CANDIDATES: [&str; 5] = ["v$", "tmp$", "t$", "x$", "y$"];
	for c in CANDIDATES.iter() {
		if !locals.iter().any(|l| l.name.starts_with(c)) {
			return c;
		}
	}
	CANDIDATES[CANDIDATES.len() - 1]
}

fn is_java_identifier(s: &str) -> bool {
	let mut it = s.chars();
	match it.next() {
		Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
		_ => return false,
	}
	it.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Deep rewrite: give every `new` handle the constructor arguments that
/// `invokespecial` supplied.
pub fn fix_new_stmts(stmts: &mut Vec<Stmt>, inits: &HashMap<u32, Vec<Expr>>) {
	for s in stmts.iter_mut() {
		fix_new_stmt(s, inits);
	}
}

fn fix_new_stmt(s: &mut Stmt, inits: &HashMap<u32, Vec<Expr>>) {
	match s {
		Stmt::LocalDef { init, .. } => {
			if let Some(e) = init.as_mut() {
				fix_new_expr(e, inits);
			}
		}
		Stmt::Assign { target, value } => {
			fix_new_expr(target, inits);
			fix_new_expr(value, inits);
		}
		Stmt::ExprStmt { expr } => fix_new_expr(expr, inits),
		Stmt::Throw { value } => fix_new_expr(value, inits),
		Stmt::If { cond, then_body, else_body, .. } => {
			fix_new_expr(cond, inits);
			fix_new_stmts(then_body, inits);
			fix_new_stmts(else_body, inits);
		}
		Stmt::While { cond, body, .. } => {
			if let Some(c) = cond.as_mut() {
				fix_new_expr(c, inits);
			}
			fix_new_stmts(body, inits);
		}
		Stmt::Switch { subject, cases, default_body, .. } => {
			fix_new_expr(subject, inits);
			for c in cases.iter_mut() {
				fix_new_stmts(&mut c.body, inits);
			}
			fix_new_stmts(default_body, inits);
		}
		Stmt::Return { value } => {
			if let Some(v) = value.as_mut() {
				fix_new_expr(v, inits);
			}
		}
		Stmt::TryCatch { try_body, catches, finally_body } => {
			fix_new_stmts(try_body, inits);
			for c in catches.iter_mut() {
				fix_new_stmts(&mut c.body, inits);
			}
			if let Some(f) = finally_body.as_mut() {
				fix_new_stmts(f, inits);
			}
		}
		Stmt::Label { .. } | Stmt::Goto { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
		Stmt::Comment { .. } | Stmt::LineNumber { .. } => {}
	}
}

pub fn fix_new_expr(e: &mut Expr, inits: &HashMap<u32, Vec<Expr>>) {
	match e {
		Expr::New { id, args, dims, .. } => {
			if *id != u32::MAX {
				if let Some(a) = inits.get(id) {
					*args = a.clone();
				}
				*id = u32::MAX;
			}
			for d in dims.iter_mut() {
				fix_new_expr(d, inits);
			}
		}
		Expr::Invoke { obj, args, .. } => {
			if let Some(o) = obj.as_mut() {
				fix_new_expr(o, inits);
			}
			for a in args.iter_mut() {
				fix_new_expr(a, inits);
			}
		}
		Expr::Bin { a, b, .. }
		| Expr::Shift { a, b, .. }
		| Expr::Bit { a, b, .. }
		| Expr::Cmp { a, b, .. }
		| Expr::Cmp3 { a, b, .. } => {
			fix_new_expr(a, inits);
			fix_new_expr(b, inits);
		}
		Expr::Field { obj: Some(o), .. } => fix_new_expr(o, inits),
		Expr::Array { arr, index, .. } => {
			fix_new_expr(arr, inits);
			fix_new_expr(index, inits);
		}
		Expr::Ternary { cond, a, b, .. } => {
			fix_new_expr(cond, inits);
			fix_new_expr(a, inits);
			fix_new_expr(b, inits);
		}
		Expr::Neg { a, .. } | Expr::Cast { a, .. } | Expr::Not { a } | Expr::InstanceOf { a, .. } | Expr::Length { a } => {
			fix_new_expr(a, inits);
		}
		_ => {}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::java::attrs::CodeAttr;
	use crate::java::class_file::JavaClassFile;
	use crate::java::insn::BinOp;
	use crate::testutil::{ClassBuilder, CodeBuilder, Handler};

	/// Assemble a `pkg/T` class around `code`, parse it back, build the CFG and
	/// emulate: the whole front end of this port, without a JVM in sight.
	fn run_method(b: &mut ClassBuilder) -> MethodCode {
		let bytes = b.to_bytes();
		let cls = JavaClassFile::parse(&bytes).unwrap();
		let is_static = cls.methods[0].access_flags & 0x0008 != 0;
		let code_attr: &CodeAttr = cls.methods[0].attrs.code.as_ref().unwrap();
		let insns = cls.methods[0].decode_insns(&cls.pool).unwrap();
		let cfg = super::super::cfg::build_cfg(
			&insns,
			code_attr.code.len() as u32,
			&code_attr.handlers,
			&code_attr.attrs.line_numbers,
		)
		.unwrap();
		let args = Args::default();
		Emulator::new(&insns, &cfg, code_attr, &cls.methods[0], is_static, "pkg/T", &args).run()
	}

	fn simple(desc: &str, c: CodeBuilder) -> MethodCode {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.method(0x0008, "m", desc, Some(c)); // ACC_STATIC
		run_method(&mut b)
	}

	fn first_handler(mc: &MethodCode) -> usize {
		(0..mc.blocks.len()).find(|i| mc.blocks[*i].catch_var.is_some()).expect("a handler block was expected")
	}

	#[test]
	fn single_definition_becomes_a_local_declaration() {
		let mut c = CodeBuilder::new(2, 2);
		c.op(0x04); // iconst_1
		c.op(0x3c); // istore_1
		c.op(0x1b); // iload_1
		c.op(0xac); // ireturn
		let mc = simple("()I", c);
		assert!(mc.error.is_none(), "emulation failed: {:?}", mc.error);
		assert_eq!(mc.blocks[0].stmts.len(), 2, "{:?}", mc.blocks[0].stmts);
		match &mc.blocks[0].stmts[0] {
			Stmt::LocalDef { name, ty, init } => {
				assert_eq!(ty, &JType::Int);
				assert_eq!(name, &default_local_name(&JType::Int, 1));
				assert_eq!(init.as_ref().map(|e| e.ty()), Some(JType::Int));
			}
			other => panic!("expected a local definition, got {:?}", other),
		}
		// the trailing `iload_1` is not a statement, it is the return value
		match &mc.blocks[0].stmts[1] {
			Stmt::Return { value: Some(Expr::Local { index: 1, .. }) } => {}
			other => panic!("expected `return v1;`, got {:?}", other),
		}
		assert!(mc.declarations.is_empty(), "{:?}", mc.declarations);
	}

	#[test]
	fn arithmetic_builds_an_expression_tree() {
		let mut c = CodeBuilder::new(4, 4);
		c.op(0x1b); // iload_1
		c.op(0x1c); // iload_2
		c.op(0x60); // iadd
		c.op(0xac); // ireturn
		let mc = simple("(III)I", c);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		match &mc.blocks[0].stmts[0] {
			Stmt::Return { value: Some(Expr::Bin { op, a, b, ty }) } => {
				assert_eq!(*op, BinOp::Add);
				assert_eq!(*ty, JType::Int);
				assert!(matches!(**a, Expr::Local { index: 1, .. }), "{:?}", a);
				assert!(matches!(**b, Expr::Local { index: 2, .. }), "{:?}", b);
			}
			other => panic!("expected `return v1 + v2;`, got {:?}", other),
		}
	}

	#[test]
	fn reused_slot_is_declared_at_the_top() {
		let mut c = CodeBuilder::new(2, 2);
		c.op(0x04); // iconst_1
		c.op(0x3c); // istore_1
		c.op(0x05); // iconst_2
		c.op(0x3c); // istore_1 again
		c.op(0xb1); // return
		let mc = simple("()V", c);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		assert!(mc.declarations.iter().any(|l| l.index == 1), "slot 1 must be hoisted: {:?}", mc.declarations);
		let assigns = mc.blocks[0].stmts.iter().filter(|s| matches!(s, Stmt::Assign { .. })).count();
		assert_eq!(assigns, 2, "{:?}", mc.blocks[0].stmts);
		assert!(mc.blocks[0].stmts.iter().all(|s| !matches!(s, Stmt::LocalDef { .. })));
	}

	#[test]
	fn values_live_across_a_join_via_a_temporary() {
		// if (p0 != 0) { v = p1 } else { v = 0 }; return v
		let mut c = CodeBuilder::new(2, 3);
		let join = c.new_label();
		let else_part = c.new_label();
		c.op(0x1a); // iload_0
		c.branch(0x9a, else_part); // ifne else_part
		c.op(0x1b); // iload_1 (then value)
		c.branch(0xa7, join); // goto join
		c.mark(else_part);
		c.op(0x03); // iconst_0 (else value)
		c.mark(join);
		c.op(0x3d); // istore_2
		c.op(0x1c); // iload_2
		c.op(0xac); // ireturn
		let mc = simple("(II)I", c);
		assert!(mc.error.is_none(), "emulation failed: {:?}", mc.error);
		let temps: Vec<&LocalInfo> = mc.locals.iter().filter(|l| l.synthetic).collect();
		assert_eq!(temps.len(), 1, "exactly one spill temp expected: {:?}", mc.locals);
		assert!(mc.declarations.iter().any(|l| l.synthetic), "the temp must be declared: {:?}", mc.declarations);
		let temp_idx = temps[0].index;
		// both predecessors assign to it ...
		let writes = mc.blocks.iter().filter(|b| b.stmts.iter().any(|s| is_assign_to(s, temp_idx))).count();
		assert_eq!(writes, 2, "both branches must spill: {:?}", mc.blocks);
		// ... and the join block initialises the real local from it
		let join_block = mc.blocks.iter().find(|b| b.stmts.iter().any(|s| matches!(s, Stmt::LocalDef { .. }))).expect("join block");
		match &join_block.stmts[0] {
			Stmt::LocalDef { init: Some(Expr::Local { index, .. }), .. } => assert_eq!(*index, temp_idx),
			other => panic!("expected `int v2$ = v$1;`, got {:?}", other),
		}
	}

	fn is_assign_to(s: &Stmt, index: u32) -> bool {
		match s {
			Stmt::Assign { target, .. } => matches!(**target, Expr::Local { index: i, .. } if i == index),
			_ => false,
		}
	}

	#[test]
	fn lcmp_before_if_becomes_a_direct_comparison() {
		let mut c = CodeBuilder::new(6, 4);
		let yes = c.new_label();
		c.op(0x1e); // lload_0
		c.op(0x20); // lload_2
		c.op(0x94); // lcmp
		c.branch(0x9b, yes); // iflt yes
		c.op(0x03); // iconst_0
		c.op(0xac); // ireturn
		c.mark(yes);
		c.op(0x04); // iconst_1
		c.op(0xac); // ireturn
		let mc = simple("(JJ)I", c);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		match mc.blocks[0].cond.as_ref() {
			Some(Expr::Cmp { op, a, b }) => {
				assert_eq!(*op, Cond::Lt);
				assert!(matches!(**a, Expr::Local { index: 0, .. }), "{:?}", a);
				assert!(matches!(**b, Expr::Local { index: 2, .. }), "{:?}", b);
			}
			other => panic!("expected a collapsed long comparison, got {:?}", other),
		}
	}

	#[test]
	fn new_dup_init_is_folded_into_one_allocation() {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		let ty_idx = b.class("pkg/Other");
		let init_idx = b.method_ref(ty_idx, "<init>", "(I)V");
		let mut c = CodeBuilder::new(3, 2);
		c.op_u2(0xbb, ty_idx); // new pkg/Other
		c.op(0x59); // dup
		c.op(0x04); // iconst_1
		c.op_u2(0xb7, init_idx); // invokespecial pkg/Other.<init>(I)V
		c.op(0x4c); // astore_1
		c.op(0xb1); // return
		b.method(0x0008, "m", "()V", Some(c));
		let mc = run_method(&mut b);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		match &mc.blocks[0].stmts[0] {
			Stmt::LocalDef { init: Some(Expr::New { ty, args, kind, dims, .. }), .. } => {
				assert_eq!(ty, &JType::Class("pkg/Other".to_string()));
				assert_eq!(*kind, NewKind::Class);
				assert!(dims.is_empty());
				assert_eq!(args.len(), 1, "constructor arguments must be attached: {:?}", args);
			}
			other => panic!("expected `new Other(1)`, got {:?}", other),
		}
	}

	#[test]
	fn super_call_is_recognised_in_a_constructor() {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		let sup_idx = b.class("java/lang/Object");
		let init_idx = b.method_ref(sup_idx, "<init>", "()V");
		let mut c = CodeBuilder::new(1, 1);
		c.op(0x2a); // aload_0
		c.op_u2(0xb7, init_idx); // invokespecial java/lang/Object.<init>()V
		c.op(0xb1); // return
		b.method(0x0000, "<init>", "()V", Some(c));
		let mc = run_method(&mut b);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		match &mc.blocks[0].stmts[0] {
			Stmt::ExprStmt { expr: Expr::New { kind, args, ty, .. } } => {
				assert_eq!(*kind, NewKind::Super);
				assert!(args.is_empty());
				assert_eq!(ty, &JType::Class("java/lang/Object".to_string()));
			}
			other => panic!("expected `super();`, got {:?}", other),
		}
		// `this` is never declared in the body
		assert!(!mc.declarations.iter().any(|l| l.is_this));
	}

	#[test]
	fn stack_underflow_is_reported_as_an_error() {
		let mut c = CodeBuilder::new(1, 1);
		c.op(0x60); // iadd on an empty stack
		c.op(0xac); // ireturn
		let mc = simple("()I", c);
		let err = mc.error.expect("stack underflow must be reported");
		assert!(err.contains("underflow"), "unexpected message: {}", err);
	}

	#[test]
	fn handler_block_names_its_catch_parameter() {
		// try { p0 -> v1 } catch (Exception e) { return 0 }  return e.hashCode()?
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		let exc_idx = b.class("java/lang/Exception");
		let mut c = CodeBuilder::new(3, 4);
		let after = c.new_label();
		c.op(0x1a); // iload_0
		let try_start = c.pc() as u32 - 1;
		c.op(0x3c); // istore_1
		let try_end = c.pc() as u32;
		c.branch(0xa7, after); // goto after
		let handler_pc = c.pc() as u32;
		c.op(0x3d); // istore_2 -- the exception object
		c.op(0x03); // iconst_0
		c.op(0xac); // ireturn
		c.mark(after);
		c.op(0x1c); // iload_2
		c.op(0xac); // ireturn
		c.handler(Handler { start: try_start, end: try_end, handler: handler_pc, catch_type: exc_idx });
		b.method(0x0008, "m", "(I)I", Some(c));
		let mc = run_method(&mut b);
		assert!(mc.error.is_none(), "{:?}", mc.error);
		let hb = first_handler(&mc);
		assert_eq!(mc.blocks[hb].catch_var, Some(2), "the exception slot is the catch parameter");
		// the store itself is not printed: the `catch (T x)` header replaces it
		assert!(mc.blocks[hb].stmts.iter().all(|s| !is_assign_to(s, 2) && !matches!(s, Stmt::LocalDef { .. })), "{:?}", mc.blocks[hb].stmts);
		assert_eq!(mc.locals[2].ty, JType::Class("java/lang/Exception".to_string()));
	}

	#[test]
	fn name_collisions_from_the_local_variable_table_are_resolved() {
		// two slots named `x` (legal for javac in sibling scopes) must not produce
		// two declarations with the same name
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		let x_idx = b.utf8("x");
		let i_idx = b.utf8("I");
		let mut c = CodeBuilder::new(2, 4);
		c.op(0x04); // iconst_1
		c.op(0x3c); // istore_1
		c.op(0x05); // iconst_2
		c.op(0x3d); // istore_2
		c.op(0xb1); // return
		c.local_var(crate::testutil::LocalVar { start: 0, length: 6, name: x_idx, desc: i_idx, index: 1 });
		c.local_var(crate::testutil::LocalVar { start: 0, length: 6, name: x_idx, desc: i_idx, index: 2 });
		b.method(0x0008, "m", "()V", Some(c));
		let mc = run_method(&mut b);
		let names: Vec<&str> = mc.locals.iter().filter(|l| l.index == 1 || l.index == 2).map(|l| l.name.as_str()).collect();
		assert_eq!(names.len(), 2);
		assert_ne!(names[0], names[1], "duplicate local names must be disambiguated: {:?}", names);
		assert_eq!(names[0], "x");
		assert!(names[1].starts_with("x$"), "{:?}", names[1]);
	}
}
