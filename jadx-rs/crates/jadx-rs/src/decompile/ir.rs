//! Intermediate representation for decompiled method bodies.
//!
//! jadx builds `JInst`/`JRoot`/`SSAVar` trees after a long chain of IR passes
//! (`JadxCodeReader` -> SSA -> `RegionMaker` -> `RegionProcessors` ->
//! `CodeGen`). Those passes are ~40k lines of Java on their own; this port keeps
//! the same *shape* -- expressions, statements, structured regions -- but
//! produces them directly from the operand stack (see [`super::emulate`]) so the
//! printer below stays identical to what jadx would emit for the constructs that
//! are supported.

use crate::java::const_pool::{Cst, RefInfo};
use crate::java::insn::{BinOp, BitOp, CmpKind, Cond, InvokeKind, ShiftOp};
use crate::types::JType;

/// An expression node. Boxes are used for exactly the children a Java printer
/// needs to recurse into.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
	Const(Cst),
	/// Text this port cannot model as a real expression (invokedynamic bootstrap,
	/// `monitorenter`, ...). Printed verbatim, always inside a `/* */` marker by
	/// the writer so the output still compiles.
	Raw { text: String, ty: JType },
	/// a local variable (including synthetic temps used for stack spilling)
	Local { index: u32, name: String, ty: JType },
	/// read of a field; `obj` is `None` for static fields
	Field {
		obj: Option<Box<Expr>>,
		owner: String,
		name: String,
		ty: JType,
		is_static: bool,
	},
	Array {
		arr: Box<Expr>,
		index: Box<Expr>,
		ty: JType,
	},
	Invoke {
		kind: InvokeKind,
		obj: Option<Box<Expr>>,
		args: Vec<Expr>,
		target: RefInfo,
		ret: JType,
	},
	/// `new T(args)`, or `new T[d0][d1]` when `dims` is non-empty.
	///
	/// `id` is the handle of a `new X()` / `dup` / `invokespecial <init>` triple:
	/// all copies of the same allocation share it, and `super_init` records the
	/// constructor arguments once the `invokespecial` has been seen (`id` ==
	/// `u32::MAX` for a plain or already-resolved allocation).
	New {
		id: u32,
		ty: JType,
		args: Vec<Expr>,
		dims: Vec<Expr>,
		/// `super(...)`/`this(...)` calls, produced from `invokespecial <init>`
		kind: NewKind,
	},
	Bin {
		op: BinOp,
		a: Box<Expr>,
		b: Box<Expr>,
		ty: JType,
	},
	Shift {
		op: ShiftOp,
		a: Box<Expr>,
		b: Box<Expr>,
		ty: JType,
	},
	Bit {
		op: BitOp,
		a: Box<Expr>,
		b: Box<Expr>,
		ty: JType,
	},
	/// comparison producing `boolean`
	Cmp {
		op: Cond,
		a: Box<Expr>,
		b: Box<Expr>,
	},
	/// `lcmp`/`fcmpl`/`dcmpg` producing `int`
	Cmp3 {
		kind: CmpKind,
		a: Box<Expr>,
		b: Box<Expr>,
	},
	Not {
		a: Box<Expr>,
	},
	Neg {
		a: Box<Expr>,
		ty: JType,
	},
	Cast {
		to: JType,
		a: Box<Expr>,
	},
	InstanceOf {
		a: Box<Expr>,
		ty: JType,
	},
	/// `a.length`
	Length {
		a: Box<Expr>,
	},
	/// `x++`, `++x`, `x += 3`
	Inc {
		index: u32,
		name: String,
		delta: i32,
		pre: bool,
		ty: JType,
	},
	/// `(cond) ? a : b` -- produced by the `?:` peephole on if/else with an
	/// assignment on both sides, as jadx's `RegionExtractHelper` does.
	Ternary {
		cond: Box<Expr>,
		a: Box<Expr>,
		b: Box<Expr>,
		ty: JType,
	},
}

/// Distinguishes `new X(...)` from the `super(...)`/`this(...)` forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
	Class,
	Array,
	Super,
	This,
}

impl Expr {
	pub fn ty(&self) -> JType {
		match self {
			Expr::Const(c) => c.type_of(),
			Expr::Raw { ty, .. } => ty.clone(),
			Expr::Local { ty, .. } => ty.clone(),
			Expr::Field { ty, .. } => ty.clone(),
			Expr::Array { ty, .. } => ty.clone(),
			Expr::Invoke { ret, .. } => ret.clone(),
			Expr::New { ty, .. } => ty.clone(),
			Expr::Bin { ty, .. } => ty.clone(),
			Expr::Shift { ty, .. } => ty.clone(),
			Expr::Bit { ty, .. } => ty.clone(),
			Expr::Cmp { .. } => JType::Boolean,
			Expr::Cmp3 { .. } => JType::Int,
			Expr::Not { .. } => JType::Boolean,
			Expr::Neg { ty, .. } => ty.clone(),
			Expr::Cast { to, .. } => to.clone(),
			Expr::InstanceOf { .. } => JType::Boolean,
			Expr::Length { .. } => JType::Int,
			Expr::Inc { ty, .. } => ty.clone(),
			Expr::Ternary { ty, .. } => ty.clone(),
		}
	}

	/// True when evaluating the expression can change program state, i.e. it may
	/// not be duplicated, reordered or dropped. jadx computes the same property
	/// with `JInsn.contains(AFlag.DONT_INLINE)`/`InvocationMerger`.
	pub fn has_side_effects(&self) -> bool {
		match self {
			Expr::Const(_) | Expr::Local { .. } | Expr::Raw { .. } => false,
			Expr::Field { .. } | Expr::Length { .. } | Expr::InstanceOf { .. } | Expr::Cmp { .. } | Expr::Cmp3 { .. } => false,
			Expr::Not { a } | Expr::Neg { a, .. } | Expr::Cast { a, .. } => a.has_side_effects(),
			Expr::Array { arr, index, .. } => arr.has_side_effects() || index.has_side_effects(),
			Expr::Bin { a, b, .. } | Expr::Shift { a, b, .. } | Expr::Bit { a, b, .. } => {
				a.has_side_effects() || b.has_side_effects()
			}
			Expr::Invoke { .. } | Expr::New { .. } | Expr::Inc { .. } => true,
			Expr::Ternary { cond, a, b, .. } => {
				cond.has_side_effects() || a.has_side_effects() || b.has_side_effects()
			}
		}
	}

	/// `true` when the expression is a bare reference to local `index`.
	pub fn is_local(&self, index: u32) -> bool {
		matches!(self, Expr::Local { index: i, .. } if *i == index)
	}

	pub fn local_index(&self) -> Option<u32> {
		match self {
			Expr::Local { index, .. } => Some(*index),
			_ => None,
		}
	}

	/// Used by the `x = x + 1` -> `x++` peephole.
	pub fn as_int_const(&self) -> Option<i32> {
		match self {
			Expr::Const(Cst::Int(v)) => Some(*v),
			Expr::Const(Cst::Long(v)) => Some(*v as i32),
			_ => None,
		}
	}

	pub fn box_self(self) -> Box<Expr> {
		Box::new(self)
	}
}

/// A `case`/`default` arm of a switch.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
	pub keys: Vec<i32>,
	pub body: Vec<Stmt>,
	/// true when control falls through into the next case (no `break`)
	pub falls_through: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
	/// `None` means the `finally` form
	pub ty: Option<JType>,
	pub var_name: String,
	pub body: Vec<Stmt>,
}

/// A statement node.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
	/// `T name = init;` (or `T name;` when `init` is `None`)
	LocalDef {
		name: String,
		ty: JType,
		init: Option<Expr>,
	},
	Assign {
		target: Box<Expr>,
		value: Box<Expr>,
	},
	ExprStmt {
		expr: Expr,
	},
	If {
		cond: Expr,
		then_body: Vec<Stmt>,
		else_body: Vec<Stmt>,
		has_else: bool,
	},
	While {
		cond: Option<Expr>,
		body: Vec<Stmt>,
		do_while: bool,
		/// label id used to resolve `break`/`continue`
		label: u32,
	},
	Switch {
		subject: Expr,
		cases: Vec<SwitchCase>,
		has_default: bool,
		default_body: Vec<Stmt>,
	},
	Return {
		value: Option<Expr>,
	},
	Throw {
		value: Expr,
	},
	TryCatch {
		try_body: Vec<Stmt>,
		catches: Vec<CatchClause>,
		finally_body: Option<Vec<Stmt>>,
	},
	/// `LBL_3:` -- jump target of a `goto`
	Label {
		id: u32,
	},
	Goto {
		id: u32,
	},
	Break {
		id: Option<u32>,
	},
	Continue {
		id: Option<u32>,
	},
	Comment {
		text: String,
	},
	/// `// [line: 42]` style marker, enabled by `Args::print_line_numbers`
	LineNumber {
		line: u32,
	},
}

impl Stmt {
	/// Collect every local index read by this statement, used by the inlining
	/// pass and by "does this region touch variable x" queries.
	pub fn uses_local(&self, index: u32) -> bool {
		match self {
			Stmt::LocalDef { init, .. } => init.as_ref().map(|e| e.reads_local(index)).unwrap_or(false),
			Stmt::Assign { target, value } => target.reads_local(index) || value.reads_local(index),
			Stmt::ExprStmt { expr } | Stmt::Throw { value: expr } => expr.reads_local(index),
			Stmt::If { cond, then_body, else_body, .. } => {
				cond.reads_local(index) || body_uses(then_body, index) || body_uses(else_body, index)
			}
			Stmt::While { cond, body, .. } => {
				cond.as_ref().map(|c| c.reads_local(index)).unwrap_or(false) || body_uses(body, index)
			}
			Stmt::Switch { subject, cases, default_body, .. } => {
				subject.reads_local(index)
					|| cases.iter().any(|c| body_uses(&c.body, index))
					|| body_uses(default_body, index)
			}
			Stmt::Return { value } => value.as_ref().map(|e| e.reads_local(index)).unwrap_or(false),
			Stmt::TryCatch { try_body, catches, finally_body } => {
				body_uses(try_body, index)
					|| catches.iter().any(|c| body_uses(&c.body, index))
					|| finally_body.as_ref().map(|b| body_uses(b, index)).unwrap_or(false)
			}
			Stmt::Label { .. } | Stmt::Goto { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => false,
			Stmt::Comment { .. } | Stmt::LineNumber { .. } => false,
		}
	}

	/// True when the statement can end control flow (nothing after it runs).
	pub fn terminates(&self) -> bool {
		match self {
			Stmt::Return { .. } | Stmt::Throw { .. } | Stmt::Goto { .. } | Stmt::Break { .. } => true,
			Stmt::If { then_body, else_body, has_else, .. } => {
				*has_else && ends(then_body) && ends(else_body)
			}
			_ => false,
		}
	}

	pub fn is_comment(&self) -> bool {
		matches!(self, Stmt::Comment { .. })
	}
}

pub fn body_uses(body: &[Stmt], index: u32) -> bool {
	body.iter().any(|s| s.uses_local(index))
}

/// True when `body` cannot fall through to the next statement.
pub fn ends(body: &[Stmt]) -> bool {
	for s in body.iter().rev() {
		if matches!(s, Stmt::Comment { .. } | Stmt::LineNumber { .. }) {
			continue;
		}
		return s.terminates();
	}
	false
}

impl Expr {
	/// recursive helper, defined here to keep `Stmt` and `Expr` in one module
	pub fn reads_local(&self, index: u32) -> bool {
		match self {
			Expr::Local { index: i, .. } => *i == index,
			Expr::Const(_) | Expr::Raw { .. } => false,
			Expr::Field { obj, .. } => obj.as_ref().map(|e| e.reads_local(index)).unwrap_or(false),
			Expr::Length { a } | Expr::Not { a } => a.reads_local(index),
			Expr::Neg { a, .. } | Expr::Cast { a, .. } | Expr::InstanceOf { a, .. } => a.reads_local(index),
			Expr::Array { arr, index: i2, .. } => arr.reads_local(index) || i2.reads_local(index),
			Expr::Bin { a, b, .. } | Expr::Shift { a, b, .. } | Expr::Bit { a, b, .. } | Expr::Cmp { a, b, .. } | Expr::Cmp3 { a, b, .. } => {
				a.reads_local(index) || b.reads_local(index)
			}
			Expr::Invoke { obj, args, .. } => {
				obj.as_ref().map(|e| e.reads_local(index)).unwrap_or(false) || args.iter().any(|a| a.reads_local(index))
			}
			Expr::New { args, dims, .. } => {
				args.iter().any(|a| a.reads_local(index)) || dims.iter().any(|a| a.reads_local(index))
			}
			Expr::Inc { index: i, .. } => *i == index,
			Expr::Ternary { cond, a, b, .. } => {
				cond.reads_local(index) || a.reads_local(index) || b.reads_local(index)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn local(i: u32) -> Expr {
		Expr::Local { index: i, name: format!("v{}", i), ty: JType::Int }
	}

	#[test]
	fn side_effect_classification() {
		assert!(!local(1).has_side_effects());
		let call = Expr::Invoke {
			kind: InvokeKind::Static,
			obj: None,
			args: vec![],
			target: RefInfo { class: "pkg/A".to_string(), name: "f".to_string(), desc: "()I".to_string() },
			ret: JType::Int,
		};
		assert!(call.has_side_effects());
		let bin = Expr::Bin { op: BinOp::Add, a: Box::new(local(1)), b: Box::new(call), ty: JType::Int };
		assert!(bin.has_side_effects());
		let cmp = Expr::Cmp { op: Cond::Eq, a: Box::new(local(1)), b: Box::new(Expr::Const(Cst::Int(0))) };
		assert!(!cmp.has_side_effects());
	}

	#[test]
	fn local_use_scans_nested_bodies() {
		let s = Stmt::If {
			cond: Expr::Const(Cst::Int(1)),
			then_body: vec![Stmt::ExprStmt { expr: local(3) }],
			else_body: vec![],
			has_else: false,
		};
		assert!(s.uses_local(3));
		assert!(!s.uses_local(4));
	}

	#[test]
	fn terminator_detection() {
		let body = vec![Stmt::Return { value: None }, Stmt::Comment { text: "x".to_string() }];
		assert!(ends(&body));
		let body2 = vec![Stmt::Comment { text: "x".to_string() }];
		assert!(!ends(&body2));
		let ret_then_comment = vec![Stmt::ExprStmt { expr: local(1) }, Stmt::Return { value: None }];
		assert!(ends(&ret_then_comment));
	}
}
