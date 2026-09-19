//! JVM instruction table and decoder.
//!
//! Port of `jadx.plugins.input.java.data.code.JavaInsnsRegister` (the static
//! opcode table) together with the per-format decoders (`LoadConstDecoder`,
//! `InvokeDecoder`, `TableSwitchDecoder`, `LookupSwitchDecoder`, `WideDecoder`).
//! jadx splits these across one class per decoder family with a lambda per
//! opcode; the information that actually matters downstream -- opcode name,
//! operand layout and the semantic class of the instruction -- is folded into a
//! single const table here so both the disassembler and the decompiler read the
//! same description.

use crate::error::{JadxError, Res};
use crate::io::BinReader;
use crate::java::const_pool::{ConstPool, Cst, RefInfo};
use crate::types::JType;

/// Element type a JVM stack slot carries (JVMS 2.6.2 runtime value set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotTy {
	Int,
	Long,
	Float,
	Double,
	/// reference (object or array)
	Obj,
	/// `void`, used by the return opcode
	Void,
}

impl SlotTy {
	pub fn jtype(self) -> JType {
		match self {
			SlotTy::Int => JType::Int,
			SlotTy::Long => JType::Long,
			SlotTy::Float => JType::Float,
			SlotTy::Double => JType::Double,
			SlotTy::Obj => JType::object(),
			SlotTy::Void => JType::Void,
		}
	}

	pub fn is_wide(self) -> bool {
		matches!(self, SlotTy::Long | SlotTy::Double)
	}
}

/// Arithmetic operations on a `SlotTy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
	Add,
	Sub,
	Mul,
	Div,
	Rem,
}

impl BinOp {
	pub fn java_op(self) -> &'static str {
		match self {
			BinOp::Add => "+",
			BinOp::Sub => "-",
			BinOp::Mul => "*",
			BinOp::Div => "/",
			BinOp::Rem => "%",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftOp {
	Shl,
	Shr,
	Ushr,
}

impl ShiftOp {
	pub fn java_op(self) -> &'static str {
		match self {
			ShiftOp::Shl => "<<",
			ShiftOp::Shr => ">>",
			ShiftOp::Ushr => ">>>",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOp {
	And,
	Or,
	Xor,
}

impl BitOp {
	pub fn java_op(self) -> &'static str {
		match self {
			BitOp::And => "&",
			BitOp::Or => "|",
			BitOp::Xor => "^",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
	Eq,
	Ne,
	Lt,
	Ge,
	Gt,
	Le,
}

impl Cond {
	/// The condition with the branches swapped, used when a `goto` follows an
	/// `if` (jadx: `IfNode.replace`/`invertCondition`).
	pub fn invert(self) -> Cond {
		match self {
			Cond::Eq => Cond::Ne,
			Cond::Ne => Cond::Eq,
			Cond::Lt => Cond::Ge,
			Cond::Ge => Cond::Lt,
			Cond::Gt => Cond::Le,
			Cond::Le => Cond::Gt,
		}
	}

	pub fn java_op(self) -> &'static str {
		match self {
			Cond::Eq => "==",
			Cond::Ne => "!=",
			Cond::Lt => "<",
			Cond::Ge => ">=",
			Cond::Gt => ">",
			Cond::Le => "<=",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conv {
	I2L,
	I2F,
	I2D,
	L2I,
	L2F,
	L2D,
	F2I,
	F2L,
	F2D,
	D2I,
	D2L,
	D2F,
	I2B,
	I2C,
	I2S,
}

impl Conv {
	/// Java cast type to emit, `None` when the conversion is a no-op for the
	/// decompiler's integer model (`i2b`, `i2c`, `i2s` still need the cast).
	pub fn cast_type(self) -> JType {
		match self {
			Conv::I2L | Conv::F2L | Conv::D2L => JType::Long,
			Conv::I2F | Conv::L2F | Conv::D2F => JType::Float,
			Conv::I2D | Conv::L2D | Conv::F2D => JType::Double,
			Conv::L2I | Conv::F2I | Conv::D2I => JType::Int,
			Conv::I2B => JType::Byte,
			Conv::I2C => JType::Char,
			Conv::I2S => JType::Short,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpKind {
	Long,
	Fmpl,
	Fmpg,
	Dmpl,
	Dmpg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeKind {
	Virtual,
	Special,
	Static,
	Interface,
	Dynamic,
}

impl InvokeKind {
	pub fn as_str(self) -> &'static str {
		match self {
			InvokeKind::Virtual => "invokevirtual",
			InvokeKind::Special => "invokespecial",
			InvokeKind::Static => "invokestatic",
			InvokeKind::Interface => "invokeinterface",
			InvokeKind::Dynamic => "invokedynamic",
		}
	}
}

/// Semantic class of an instruction: what it *means*, used by the decompiler
/// instead of matching on raw opcode numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sem {
	Nop,
	Invalid,
	AconstNull,
	Iconst,
	Lconst,
	Fconst,
	Dconst,
	Bipush,
	Sipush,
	Ldc,
	LdcWide,
	Load(SlotTy),
	Store(SlotTy),
	ArrayLoad(SlotTy),
	ArrayStore(SlotTy),
	Pop,
	Pop2,
	Dup,
	DupX1,
	DupX2,
	Dup2,
	Dup2X1,
	Dup2X2,
	Swap,
	Bin(BinOp, SlotTy),
	Neg(SlotTy),
	Shift(ShiftOp, bool),
	Bit(BitOp, bool),
	Iinc,
	Conv(Conv),
	Cmp(CmpKind),
	/// `if<cond>` on the value stack: pops one int
	If(Cond),
	/// `if_icmp<cond>`: pops two ints
	IfCmp(Cond),
	/// `if_acmp<cond>`: pops two references (only Eq/Ne exist)
	IfACmp(Cond),
	IfNull,
	IfNonNull,
	Goto,
	Jsr,
	Ret,
	TableSwitch,
	LookupSwitch,
	Return(SlotTy),
	GetStatic,
	PutStatic,
	GetField,
	PutField,
	Invoke(InvokeKind),
	New,
	NewArray,
	ANewArray,
	MultiANewArray,
	ArrayLength,
	Athrow,
	CheckCast,
	InstanceOf,
	MonitorEnter,
	MonitorExit,
}

impl Sem {
	/// True for instructions that end a block without falling through.
	pub fn is_unconditional_branch(self) -> bool {
		matches!(
			self,
			Sem::Goto | Sem::Jsr | Sem::Ret | Sem::Athrow | Sem::Return(_)
		)
	}

	pub fn is_conditional_branch(self) -> bool {
		matches!(
			self,
			Sem::If(_) | Sem::IfCmp(_) | Sem::IfACmp(_) | Sem::IfNull | Sem::IfNonNull | Sem::TableSwitch | Sem::LookupSwitch
		)
	}

	pub fn is_const_load(self) -> bool {
		matches!(
			self,
			Sem::AconstNull | Sem::Iconst | Sem::Lconst | Sem::Fconst | Sem::Dconst | Sem::Bipush | Sem::Sipush | Sem::Ldc | Sem::LdcWide
		)
	}

	pub fn is_field(self) -> bool {
		matches!(
			self,
			Sem::GetStatic | Sem::PutStatic | Sem::GetField | Sem::PutField
		)
	}
}

/// Operand layout of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFmt {
	/// opcode only
	None,
	/// `u1` local variable index
	Local,
	/// `u1` index + `s1` const (iinc)
	Inc,
	/// `s1`
	Const1,
	/// `s2`
	Const2,
	/// `u1` constant pool index (ldc)
	Index1,
	/// `u2` constant pool index
	Index2,
	/// `u1` array type code (newarray)
	ArrayType,
	/// `s2` branch offset
	Branch,
	/// `s4` branch offset (goto_w)
	BranchWide,
	/// `u2` field/methodref
	Ref,
	/// `u2` methodref + `u1` count + `u1` 0 (invokeinterface)
	IfaceRef,
	/// `u2` indy + `u1` 0 + `u1` 0 (invokedynamic)
	IndyRef,
	/// `u2` class index (anewarray/checkcast/instanceof)
	Class,
	/// `u2` class index + `u1` dimensions
	MultiClass,
	/// `u1` return address index (ret)
	RetIndex,
	/// variable length, decoded by switch handling
	TableSwitch,
	LookupSwitch,
	/// `wide` prefix, re-dispatched
	Wide,
}

impl OpFmt {
	/// Fixed byte length, or 0 when the length depends on the payload.
	pub const fn fixed_len(self) -> u16 {
		match self {
			OpFmt::None => 1,
			OpFmt::Local => 2,
			OpFmt::Inc => 3,
			OpFmt::Const1 => 2,
			OpFmt::Const2 => 3,
			OpFmt::Index1 => 2,
			OpFmt::Index2 => 3,
			OpFmt::ArrayType => 2,
			OpFmt::Branch => 3,
			OpFmt::BranchWide => 5,
			OpFmt::Ref => 3,
			OpFmt::IfaceRef => 5,
			OpFmt::IndyRef => 5,
			OpFmt::Class => 3,
			OpFmt::MultiClass => 4,
			OpFmt::RetIndex => 2,
			OpFmt::TableSwitch | OpFmt::LookupSwitch | OpFmt::Wide => 0,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpInfo {
	pub code: u8,
	pub name: &'static str,
	pub fmt: OpFmt,
	pub sem: Sem,
}

const TABLE_SIZE: usize = 0xca;

/// Unregistered opcodes (`impdep1`, `impdep2`, reserved 0xb3..0xba).
const INVALID: OpInfo = OpInfo { code: 0, name: "unknown", fmt: OpFmt::None, sem: Sem::Invalid };

macro_rules! put {
	($t:expr, $code:expr, $name:expr, $fmt:expr, $sem:expr) => {
		$t[$code as usize] = OpInfo { code: $code, name: $name, fmt: $fmt, sem: $sem };
	};
}

const fn build_table() -> [OpInfo; TABLE_SIZE] {
	let mut t = [INVALID; TABLE_SIZE];

	put!(t, 0x00, "nop", OpFmt::None, Sem::Nop);
	put!(t, 0x01, "aconst_null", OpFmt::None, Sem::AconstNull);
	put!(t, 0x02, "iconst_m1", OpFmt::None, Sem::Iconst);
	put!(t, 0x03, "iconst_0", OpFmt::None, Sem::Iconst);
	put!(t, 0x04, "iconst_1", OpFmt::None, Sem::Iconst);
	put!(t, 0x05, "iconst_2", OpFmt::None, Sem::Iconst);
	put!(t, 0x06, "iconst_3", OpFmt::None, Sem::Iconst);
	put!(t, 0x07, "iconst_4", OpFmt::None, Sem::Iconst);
	put!(t, 0x08, "iconst_5", OpFmt::None, Sem::Iconst);
	put!(t, 0x09, "lconst_0", OpFmt::None, Sem::Lconst);
	put!(t, 0x0a, "lconst_1", OpFmt::None, Sem::Lconst);
	put!(t, 0x0b, "fconst_0", OpFmt::None, Sem::Fconst);
	put!(t, 0x0c, "fconst_1", OpFmt::None, Sem::Fconst);
	put!(t, 0x0d, "fconst_2", OpFmt::None, Sem::Fconst);
	put!(t, 0x0e, "dconst_0", OpFmt::None, Sem::Dconst);
	put!(t, 0x0f, "dconst_1", OpFmt::None, Sem::Dconst);
	put!(t, 0x10, "bipush", OpFmt::Const1, Sem::Bipush);
	put!(t, 0x11, "sipush", OpFmt::Const2, Sem::Sipush);
	put!(t, 0x12, "ldc", OpFmt::Index1, Sem::Ldc);
	put!(t, 0x13, "ldc_w", OpFmt::Index2, Sem::Ldc);
	put!(t, 0x14, "ldc2_w", OpFmt::Index2, Sem::LdcWide);

	// loads: iload, lload, fload, dload, aload and the _0.._3 shorthands
	put!(t, 0x15, "iload", OpFmt::Local, Sem::Load(SlotTy::Int));
	put!(t, 0x16, "lload", OpFmt::Local, Sem::Load(SlotTy::Long));
	put!(t, 0x17, "fload", OpFmt::Local, Sem::Load(SlotTy::Float));
	put!(t, 0x18, "dload", OpFmt::Local, Sem::Load(SlotTy::Double));
	put!(t, 0x19, "aload", OpFmt::Local, Sem::Load(SlotTy::Obj));
	put!(t, 0x1a, "iload_0", OpFmt::None, Sem::Load(SlotTy::Int));
	put!(t, 0x1b, "iload_1", OpFmt::None, Sem::Load(SlotTy::Int));
	put!(t, 0x1c, "iload_2", OpFmt::None, Sem::Load(SlotTy::Int));
	put!(t, 0x1d, "iload_3", OpFmt::None, Sem::Load(SlotTy::Int));
	put!(t, 0x1e, "lload_0", OpFmt::None, Sem::Load(SlotTy::Long));
	put!(t, 0x1f, "lload_1", OpFmt::None, Sem::Load(SlotTy::Long));
	put!(t, 0x20, "lload_2", OpFmt::None, Sem::Load(SlotTy::Long));
	put!(t, 0x21, "lload_3", OpFmt::None, Sem::Load(SlotTy::Long));
	put!(t, 0x22, "fload_0", OpFmt::None, Sem::Load(SlotTy::Float));
	put!(t, 0x23, "fload_1", OpFmt::None, Sem::Load(SlotTy::Float));
	put!(t, 0x24, "fload_2", OpFmt::None, Sem::Load(SlotTy::Float));
	put!(t, 0x25, "fload_3", OpFmt::None, Sem::Load(SlotTy::Float));
	put!(t, 0x26, "dload_0", OpFmt::None, Sem::Load(SlotTy::Double));
	put!(t, 0x27, "dload_1", OpFmt::None, Sem::Load(SlotTy::Double));
	put!(t, 0x28, "dload_2", OpFmt::None, Sem::Load(SlotTy::Double));
	put!(t, 0x29, "dload_3", OpFmt::None, Sem::Load(SlotTy::Double));
	put!(t, 0x2a, "aload_0", OpFmt::None, Sem::Load(SlotTy::Obj));
	put!(t, 0x2b, "aload_1", OpFmt::None, Sem::Load(SlotTy::Obj));
	put!(t, 0x2c, "aload_2", OpFmt::None, Sem::Load(SlotTy::Obj));
	put!(t, 0x2d, "aload_3", OpFmt::None, Sem::Load(SlotTy::Obj));

	// array loads
	put!(t, 0x2e, "iaload", OpFmt::None, Sem::ArrayLoad(SlotTy::Int));
	put!(t, 0x2f, "laload", OpFmt::None, Sem::ArrayLoad(SlotTy::Long));
	put!(t, 0x30, "faload", OpFmt::None, Sem::ArrayLoad(SlotTy::Float));
	put!(t, 0x31, "daload", OpFmt::None, Sem::ArrayLoad(SlotTy::Double));
	put!(t, 0x32, "aaload", OpFmt::None, Sem::ArrayLoad(SlotTy::Obj));
	put!(t, 0x33, "baload", OpFmt::None, Sem::ArrayLoad(SlotTy::Int));
	put!(t, 0x34, "caload", OpFmt::None, Sem::ArrayLoad(SlotTy::Int));
	put!(t, 0x35, "saload", OpFmt::None, Sem::ArrayLoad(SlotTy::Int));

	// stores
	put!(t, 0x36, "istore", OpFmt::Local, Sem::Store(SlotTy::Int));
	put!(t, 0x37, "lstore", OpFmt::Local, Sem::Store(SlotTy::Long));
	put!(t, 0x38, "fstore", OpFmt::Local, Sem::Store(SlotTy::Float));
	put!(t, 0x39, "dstore", OpFmt::Local, Sem::Store(SlotTy::Double));
	put!(t, 0x3a, "astore", OpFmt::Local, Sem::Store(SlotTy::Obj));
	put!(t, 0x3b, "istore_0", OpFmt::None, Sem::Store(SlotTy::Int));
	put!(t, 0x3c, "istore_1", OpFmt::None, Sem::Store(SlotTy::Int));
	put!(t, 0x3d, "istore_2", OpFmt::None, Sem::Store(SlotTy::Int));
	put!(t, 0x3e, "istore_3", OpFmt::None, Sem::Store(SlotTy::Int));
	put!(t, 0x3f, "lstore_0", OpFmt::None, Sem::Store(SlotTy::Long));
	put!(t, 0x40, "lstore_1", OpFmt::None, Sem::Store(SlotTy::Long));
	put!(t, 0x41, "lstore_2", OpFmt::None, Sem::Store(SlotTy::Long));
	put!(t, 0x42, "lstore_3", OpFmt::None, Sem::Store(SlotTy::Long));
	put!(t, 0x43, "fstore_0", OpFmt::None, Sem::Store(SlotTy::Float));
	put!(t, 0x44, "fstore_1", OpFmt::None, Sem::Store(SlotTy::Float));
	put!(t, 0x45, "fstore_2", OpFmt::None, Sem::Store(SlotTy::Float));
	put!(t, 0x46, "fstore_3", OpFmt::None, Sem::Store(SlotTy::Float));
	put!(t, 0x47, "dstore_0", OpFmt::None, Sem::Store(SlotTy::Double));
	put!(t, 0x48, "dstore_1", OpFmt::None, Sem::Store(SlotTy::Double));
	put!(t, 0x49, "dstore_2", OpFmt::None, Sem::Store(SlotTy::Double));
	put!(t, 0x4a, "dstore_3", OpFmt::None, Sem::Store(SlotTy::Double));
	put!(t, 0x4b, "astore_0", OpFmt::None, Sem::Store(SlotTy::Obj));
	put!(t, 0x4c, "astore_1", OpFmt::None, Sem::Store(SlotTy::Obj));
	put!(t, 0x4d, "astore_2", OpFmt::None, Sem::Store(SlotTy::Obj));
	put!(t, 0x4e, "astore_3", OpFmt::None, Sem::Store(SlotTy::Obj));

	// array stores
	put!(t, 0x4f, "iastore", OpFmt::None, Sem::ArrayStore(SlotTy::Int));
	put!(t, 0x50, "lastore", OpFmt::None, Sem::ArrayStore(SlotTy::Long));
	put!(t, 0x51, "fastore", OpFmt::None, Sem::ArrayStore(SlotTy::Float));
	put!(t, 0x52, "dastore", OpFmt::None, Sem::ArrayStore(SlotTy::Double));
	put!(t, 0x53, "aastore", OpFmt::None, Sem::ArrayStore(SlotTy::Obj));
	put!(t, 0x54, "bastore", OpFmt::None, Sem::ArrayStore(SlotTy::Int));
	put!(t, 0x55, "castore", OpFmt::None, Sem::ArrayStore(SlotTy::Int));
	put!(t, 0x56, "sastore", OpFmt::None, Sem::ArrayStore(SlotTy::Int));

	// stack manipulation
	put!(t, 0x57, "pop", OpFmt::None, Sem::Pop);
	put!(t, 0x58, "pop2", OpFmt::None, Sem::Pop2);
	put!(t, 0x59, "dup", OpFmt::None, Sem::Dup);
	put!(t, 0x5a, "dup_x1", OpFmt::None, Sem::DupX1);
	put!(t, 0x5b, "dup_x2", OpFmt::None, Sem::DupX2);
	put!(t, 0x5c, "dup2", OpFmt::None, Sem::Dup2);
	put!(t, 0x5d, "dup2_x1", OpFmt::None, Sem::Dup2X1);
	put!(t, 0x5e, "dup2_x2", OpFmt::None, Sem::Dup2X2);
	put!(t, 0x5f, "swap", OpFmt::None, Sem::Swap);

	// arithmetic
	put!(t, 0x60, "iadd", OpFmt::None, Sem::Bin(BinOp::Add, SlotTy::Int));
	put!(t, 0x61, "ladd", OpFmt::None, Sem::Bin(BinOp::Add, SlotTy::Long));
	put!(t, 0x62, "fadd", OpFmt::None, Sem::Bin(BinOp::Add, SlotTy::Float));
	put!(t, 0x63, "dadd", OpFmt::None, Sem::Bin(BinOp::Add, SlotTy::Double));
	put!(t, 0x64, "isub", OpFmt::None, Sem::Bin(BinOp::Sub, SlotTy::Int));
	put!(t, 0x65, "lsub", OpFmt::None, Sem::Bin(BinOp::Sub, SlotTy::Long));
	put!(t, 0x66, "fsub", OpFmt::None, Sem::Bin(BinOp::Sub, SlotTy::Float));
	put!(t, 0x67, "dsub", OpFmt::None, Sem::Bin(BinOp::Sub, SlotTy::Double));
	put!(t, 0x68, "imul", OpFmt::None, Sem::Bin(BinOp::Mul, SlotTy::Int));
	put!(t, 0x69, "lmul", OpFmt::None, Sem::Bin(BinOp::Mul, SlotTy::Long));
	put!(t, 0x6a, "fmul", OpFmt::None, Sem::Bin(BinOp::Mul, SlotTy::Float));
	put!(t, 0x6b, "dmul", OpFmt::None, Sem::Bin(BinOp::Mul, SlotTy::Double));
	put!(t, 0x6c, "idiv", OpFmt::None, Sem::Bin(BinOp::Div, SlotTy::Int));
	put!(t, 0x6d, "ldiv", OpFmt::None, Sem::Bin(BinOp::Div, SlotTy::Long));
	put!(t, 0x6e, "fdiv", OpFmt::None, Sem::Bin(BinOp::Div, SlotTy::Float));
	put!(t, 0x6f, "ddiv", OpFmt::None, Sem::Bin(BinOp::Div, SlotTy::Double));
	put!(t, 0x70, "irem", OpFmt::None, Sem::Bin(BinOp::Rem, SlotTy::Int));
	put!(t, 0x71, "lrem", OpFmt::None, Sem::Bin(BinOp::Rem, SlotTy::Long));
	put!(t, 0x72, "frem", OpFmt::None, Sem::Bin(BinOp::Rem, SlotTy::Float));
	put!(t, 0x73, "drem", OpFmt::None, Sem::Bin(BinOp::Rem, SlotTy::Double));
	put!(t, 0x74, "ineg", OpFmt::None, Sem::Neg(SlotTy::Int));
	put!(t, 0x75, "lneg", OpFmt::None, Sem::Neg(SlotTy::Long));
	put!(t, 0x76, "fneg", OpFmt::None, Sem::Neg(SlotTy::Float));
	put!(t, 0x77, "dneg", OpFmt::None, Sem::Neg(SlotTy::Double));
	put!(t, 0x78, "ishl", OpFmt::None, Sem::Shift(ShiftOp::Shl, false));
	put!(t, 0x79, "lshl", OpFmt::None, Sem::Shift(ShiftOp::Shl, true));
	put!(t, 0x7a, "ishr", OpFmt::None, Sem::Shift(ShiftOp::Shr, false));
	put!(t, 0x7b, "lshr", OpFmt::None, Sem::Shift(ShiftOp::Shr, true));
	put!(t, 0x7c, "iushr", OpFmt::None, Sem::Shift(ShiftOp::Ushr, false));
	put!(t, 0x7d, "lushr", OpFmt::None, Sem::Shift(ShiftOp::Ushr, true));
	put!(t, 0x7e, "iand", OpFmt::None, Sem::Bit(BitOp::And, false));
	put!(t, 0x7f, "land", OpFmt::None, Sem::Bit(BitOp::And, true));
	put!(t, 0x80, "ior", OpFmt::None, Sem::Bit(BitOp::Or, false));
	put!(t, 0x81, "lor", OpFmt::None, Sem::Bit(BitOp::Or, true));
	put!(t, 0x82, "ixor", OpFmt::None, Sem::Bit(BitOp::Xor, false));
	put!(t, 0x83, "lxor", OpFmt::None, Sem::Bit(BitOp::Xor, true));
	put!(t, 0x84, "iinc", OpFmt::Inc, Sem::Iinc);

	// conversions
	put!(t, 0x85, "i2l", OpFmt::None, Sem::Conv(Conv::I2L));
	put!(t, 0x86, "i2f", OpFmt::None, Sem::Conv(Conv::I2F));
	put!(t, 0x87, "i2d", OpFmt::None, Sem::Conv(Conv::I2D));
	put!(t, 0x88, "l2i", OpFmt::None, Sem::Conv(Conv::L2I));
	put!(t, 0x89, "l2f", OpFmt::None, Sem::Conv(Conv::L2F));
	put!(t, 0x8a, "l2d", OpFmt::None, Sem::Conv(Conv::L2D));
	put!(t, 0x8b, "f2i", OpFmt::None, Sem::Conv(Conv::F2I));
	put!(t, 0x8c, "f2l", OpFmt::None, Sem::Conv(Conv::F2L));
	put!(t, 0x8d, "f2d", OpFmt::None, Sem::Conv(Conv::F2D));
	put!(t, 0x8e, "d2i", OpFmt::None, Sem::Conv(Conv::D2I));
	put!(t, 0x8f, "d2l", OpFmt::None, Sem::Conv(Conv::D2L));
	put!(t, 0x90, "d2f", OpFmt::None, Sem::Conv(Conv::D2F));
	put!(t, 0x91, "i2b", OpFmt::None, Sem::Conv(Conv::I2B));
	put!(t, 0x92, "i2c", OpFmt::None, Sem::Conv(Conv::I2C));
	put!(t, 0x93, "i2s", OpFmt::None, Sem::Conv(Conv::I2S));

	// comparisons
	put!(t, 0x94, "lcmp", OpFmt::None, Sem::Cmp(CmpKind::Long));
	put!(t, 0x95, "fcmpl", OpFmt::None, Sem::Cmp(CmpKind::Fmpl));
	put!(t, 0x96, "fcmpg", OpFmt::None, Sem::Cmp(CmpKind::Fmpg));
	put!(t, 0x97, "dcmpl", OpFmt::None, Sem::Cmp(CmpKind::Dmpl));
	put!(t, 0x98, "dcmpg", OpFmt::None, Sem::Cmp(CmpKind::Dmpg));

	// conditional branches
	put!(t, 0x99, "ifeq", OpFmt::Branch, Sem::If(Cond::Eq));
	put!(t, 0x9a, "ifne", OpFmt::Branch, Sem::If(Cond::Ne));
	put!(t, 0x9b, "iflt", OpFmt::Branch, Sem::If(Cond::Lt));
	put!(t, 0x9c, "ifge", OpFmt::Branch, Sem::If(Cond::Ge));
	put!(t, 0x9d, "ifgt", OpFmt::Branch, Sem::If(Cond::Gt));
	put!(t, 0x9e, "ifle", OpFmt::Branch, Sem::If(Cond::Le));
	put!(t, 0x9f, "if_icmpeq", OpFmt::Branch, Sem::IfCmp(Cond::Eq));
	put!(t, 0xa0, "if_icmpne", OpFmt::Branch, Sem::IfCmp(Cond::Ne));
	put!(t, 0xa1, "if_icmplt", OpFmt::Branch, Sem::IfCmp(Cond::Lt));
	put!(t, 0xa2, "if_icmpge", OpFmt::Branch, Sem::IfCmp(Cond::Ge));
	put!(t, 0xa3, "if_icmpgt", OpFmt::Branch, Sem::IfCmp(Cond::Gt));
	put!(t, 0xa4, "if_icmple", OpFmt::Branch, Sem::IfCmp(Cond::Le));
	put!(t, 0xa5, "if_acmpeq", OpFmt::Branch, Sem::IfACmp(Cond::Eq));
	put!(t, 0xa6, "if_acmpne", OpFmt::Branch, Sem::IfACmp(Cond::Ne));
	put!(t, 0xa7, "goto", OpFmt::Branch, Sem::Goto);
	put!(t, 0xa8, "jsr", OpFmt::Branch, Sem::Jsr);
	put!(t, 0xa9, "ret", OpFmt::RetIndex, Sem::Ret);
	put!(t, 0xaa, "tableswitch", OpFmt::TableSwitch, Sem::TableSwitch);
	put!(t, 0xab, "lookupswitch", OpFmt::LookupSwitch, Sem::LookupSwitch);
	put!(t, 0xac, "ireturn", OpFmt::None, Sem::Return(SlotTy::Int));
	put!(t, 0xad, "lreturn", OpFmt::None, Sem::Return(SlotTy::Long));
	put!(t, 0xae, "freturn", OpFmt::None, Sem::Return(SlotTy::Float));
	put!(t, 0xaf, "dreturn", OpFmt::None, Sem::Return(SlotTy::Double));
	put!(t, 0xb0, "areturn", OpFmt::None, Sem::Return(SlotTy::Obj));
	put!(t, 0xb1, "return", OpFmt::None, Sem::Return(SlotTy::Void));

	// fields and invocations
	put!(t, 0xb2, "getstatic", OpFmt::Ref, Sem::GetStatic);
	put!(t, 0xb3, "putstatic", OpFmt::Ref, Sem::PutStatic);
	put!(t, 0xb4, "getfield", OpFmt::Ref, Sem::GetField);
	put!(t, 0xb5, "putfield", OpFmt::Ref, Sem::PutField);
	put!(t, 0xb6, "invokevirtual", OpFmt::Ref, Sem::Invoke(InvokeKind::Virtual));
	put!(t, 0xb7, "invokespecial", OpFmt::Ref, Sem::Invoke(InvokeKind::Special));
	put!(t, 0xb8, "invokestatic", OpFmt::Ref, Sem::Invoke(InvokeKind::Static));
	put!(t, 0xb9, "invokeinterface", OpFmt::IfaceRef, Sem::Invoke(InvokeKind::Interface));
	put!(t, 0xba, "invokedynamic", OpFmt::IndyRef, Sem::Invoke(InvokeKind::Dynamic));

	// objects, arrays, exceptions, monitors
	put!(t, 0xbb, "new", OpFmt::Class, Sem::New);
	put!(t, 0xbc, "newarray", OpFmt::ArrayType, Sem::NewArray);
	put!(t, 0xbd, "anewarray", OpFmt::Class, Sem::ANewArray);
	put!(t, 0xbe, "arraylength", OpFmt::None, Sem::ArrayLength);
	put!(t, 0xbf, "athrow", OpFmt::None, Sem::Athrow);
	put!(t, 0xc0, "checkcast", OpFmt::Class, Sem::CheckCast);
	put!(t, 0xc1, "instanceof", OpFmt::Class, Sem::InstanceOf);
	put!(t, 0xc2, "monitorenter", OpFmt::None, Sem::MonitorEnter);
	put!(t, 0xc3, "monitorexit", OpFmt::None, Sem::MonitorExit);
	put!(t, 0xc4, "wide", OpFmt::Wide, Sem::Nop);
	put!(t, 0xc5, "multianewarray", OpFmt::MultiClass, Sem::MultiANewArray);
	put!(t, 0xc6, "ifnull", OpFmt::Branch, Sem::IfNull);
	put!(t, 0xc7, "ifnonnull", OpFmt::Branch, Sem::IfNonNull);
	put!(t, 0xc8, "goto_w", OpFmt::BranchWide, Sem::Goto);
	put!(t, 0xc9, "jsr_w", OpFmt::BranchWide, Sem::Jsr);

	t
}

/// The static opcode table (jadx: `INSN_INFO`).
const TABLE: [OpInfo; TABLE_SIZE] = build_table();

/// jadx: `JavaInsnsRegister.get(int)`
pub fn info(code: u8) -> &'static OpInfo {
	let i = code as usize;
	if i < TABLE_SIZE {
		&TABLE[i]
	} else {
		&INVALID
	}
}

fn sem_of(code: u8) -> Sem {
	info(code).sem
}

/// `newarray` operand type codes (JVMS 6.5 newarray).
pub fn array_type_code(atype: u8) -> Res<JType> {
	Ok(match atype {
		4 => JType::Boolean,
		5 => JType::Char,
		6 => JType::Float,
		7 => JType::Double,
		8 => JType::Byte,
		9 => JType::Short,
		10 => JType::Int,
		11 => JType::Long,
		other => return Err(JadxError::format(format!("bad newarray type code {}", other))),
	})
}

/// Decoded operand of an instruction, with constant pool references already
/// resolved (jadx stores raw indices in `JavaInsnData` and resolves lazily).
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
	Local(u32),
	Inc { index: u32, delta: i32 },
	Const(i32),
	Cst(Cst),
	Branch(u32),
	Switch { default: u32, pairs: Vec<(i32, u32)> },
	Field(RefInfo),
	Method(RefInfo),
	Type(JType),
	MultiArray { ty: JType, dims: u32 },
}

/// One decoded instruction.
#[derive(Debug, Clone)]
pub struct Insn {
	/// offset from the start of the `Code` array
	pub off: u32,
	pub code: u8,
	pub len: u16,
	pub ops: Vec<Op>,
	/// true when the instruction was prefixed by `wide`
	pub wide: bool,
}

impl Insn {
	pub fn info(&self) -> &'static OpInfo {
		info(self.code)
	}

	pub fn sem(&self) -> Sem {
		self.info().sem
	}

	pub fn name(&self) -> &'static str {
		self.info().name
	}

	/// first `Local` operand, 0 when absent (mirrors `JavaInsnData::getReg`)
	pub fn local(&self) -> u32 {
		for op in &self.ops {
			match op {
				Op::Local(i) => return *i,
				Op::Inc { index, .. } => return *index,
				_ => {}
			}
		}
		0
	}

	pub fn inc_delta(&self) -> Option<i32> {
		for op in &self.ops {
			if let Op::Inc { delta, .. } = op {
				return Some(*delta);
			}
		}
		None
	}

	pub fn cst(&self) -> Option<&Cst> {
		for op in &self.ops {
			if let Op::Cst(c) = op {
				return Some(c);
			}
		}
		None
	}

	pub fn int_const(&self) -> Option<i32> {
		for op in &self.ops {
			match op {
				Op::Const(v) => return Some(*v),
				Op::Cst(Cst::Int(v)) => return Some(*v),
				_ => {}
			}
		}
		None
	}

	pub fn branch(&self) -> Option<u32> {
		for op in &self.ops {
			if let Op::Branch(t) = op {
				return Some(*t);
			}
		}
		None
	}

	pub fn switch_data(&self) -> Option<(u32, Vec<(i32, u32)>)> {
		for op in &self.ops {
			if let Op::Switch { default, pairs } = op {
				return Some((*default, pairs.iter().map(|(k, t)| (*k, *t)).collect()));
			}
		}
		None
	}

	/// every jump target of this instruction (jadx: `getAllTargets`)
	pub fn targets(&self) -> Vec<u32> {
		let mut out = Vec::new();
		for op in &self.ops {
			match op {
				Op::Branch(t) => out.push(*t),
				Op::Switch { default, pairs } => {
					out.push(*default);
					for (_, t) in pairs {
						out.push(*t);
					}
				}
				_ => {}
			}
		}
		out
	}

	pub fn field_ref(&self) -> Option<&RefInfo> {
		for op in &self.ops {
			if let Op::Field(r) = op {
				return Some(r);
			}
		}
		None
	}

	pub fn method_ref(&self) -> Option<&RefInfo> {
		for op in &self.ops {
			if let Op::Method(r) = op {
				return Some(r);
			}
		}
		None
	}

	pub fn ty(&self) -> Option<&JType> {
		for op in &self.ops {
			match op {
				Op::Type(t) => return Some(t),
				Op::MultiArray { ty, .. } => return Some(ty),
				_ => {}
			}
		}
		None
	}

	pub fn dims(&self) -> Option<u32> {
		for op in &self.ops {
			if let Op::MultiArray { dims, .. } = op {
				return Some(*dims);
			}
		}
		None
	}
}

/// Decode a `Code` array into instructions.
///
/// jadx: `JavaCodeReader.read()`. Branch targets are absolute offsets inside the
/// code array (jadx uses the same convention for `reg_data`/target offsets).
pub fn decode_code(code: &[u8], pool: &ConstPool) -> Res<Vec<Insn>> {
	let mut out: Vec<Insn> = Vec::new();
	let mut r = BinReader::new(code);
	let code_len = code.len() as u32;
	while (r.pos() as u32) < code_len {
		let off = r.pos() as u32;
		let opcode = r.u8()?;
		if opcode == 0xc4 {
			// jadx: `WideDecoder` -- only iload/istore/ret and iinc may follow
			let inner = r.u8()?;
			let sem = sem_of(inner);
			let index = r.u16()? as u32;
			let allowed = matches!(sem, Sem::Load(_) | Sem::Store(_) | Sem::Ret | Sem::Iinc);
			if !allowed {
				return Err(JadxError::format(format!(
					"illegal wide instruction {} at code offset {}",
					info(inner).name,
					off
				)));
			}
			if sem == Sem::Iinc {
				let delta = r.i16()? as i32;
				out.push(Insn { off, code: inner, len: 6, ops: vec![Op::Inc { index, delta }], wide: true });
			} else {
				out.push(Insn { off, code: inner, len: 4, ops: vec![Op::Local(index)], wide: true });
			}
			continue;
		}
		let op_info = info(opcode);
		if op_info.sem == Sem::Invalid {
			return Err(JadxError::format(format!(
				"illegal opcode 0x{:02x} at code offset {}",
				opcode, off
			)));
		}
		let fmt = op_info.fmt;
		let mut ops: Vec<Op> = Vec::new();
		let len: u16 = match fmt {
			OpFmt::None => {
				// the `*_0`..`*_3` shorthands carry the local index in the opcode itself
				if matches!(op_info.sem, Sem::Load(_) | Sem::Store(_)) && (0x1a..=0x2d).contains(&opcode) {
					ops.push(Op::Local(((opcode - 0x1a) % 4) as u32));
				} else if matches!(op_info.sem, Sem::Store(_)) && (0x3b..=0x4e).contains(&opcode) {
					ops.push(Op::Local(((opcode - 0x3b) % 4) as u32));
				}
				1
			}
			OpFmt::Local => {
				let idx = r.u8()? as u32;
				ops.push(Op::Local(idx));
				2
			}
			OpFmt::RetIndex => {
				let idx = r.u8()? as u32;
				ops.push(Op::Local(idx));
				2
			}
			OpFmt::Inc => {
				let idx = r.u8()? as u32;
				let delta = r.i8()? as i32;
				ops.push(Op::Inc { index: idx, delta });
				3
			}
			OpFmt::Const1 => {
				let v = r.i8()? as i32;
				ops.push(Op::Const(v));
				2
			}
			OpFmt::Const2 => {
				let v = r.i16()? as i32;
				ops.push(Op::Const(v));
				3
			}
			OpFmt::Index1 => {
				let idx = r.u8()?;
				ops.push(Op::Cst(pool.resolve_const(idx)?));
				2
			}
			OpFmt::Index2 => {
				let idx = r.u16()?;
				ops.push(Op::Cst(pool.resolve_const(idx)?));
				3
			}
			OpFmt::ArrayType => {
				let atype = r.u8()?;
				ops.push(Op::Type(array_type_code(atype)?));
				2
			}
			OpFmt::Branch => {
				let delta = r.i16()? as i32;
				let target = (off as i64 + delta as i64) as u32;
				ops.push(Op::Branch(target));
				3
			}
			OpFmt::BranchWide => {
				let delta = r.i32()?;
				let target = (off as i64 + delta as i64) as u32;
				ops.push(Op::Branch(target));
				5
			}
			OpFmt::Ref => {
				let idx = r.u16()?;
				let target = match op_info.sem {
					Sem::GetStatic | Sem::PutStatic | Sem::GetField | Sem::PutField => pool.field_ref(idx)?,
					_ => pool.method_ref(idx)?,
				};
				match op_info.sem {
					Sem::GetStatic | Sem::PutStatic | Sem::GetField | Sem::PutField => ops.push(Op::Field(target)),
					_ => ops.push(Op::Method(target)),
				}
				3
			}
			OpFmt::IfaceRef => {
				let idx = r.u16()?;
				let _count = r.u8()?;
				r.skip(1)?;
				ops.push(Op::Method(pool.method_ref(idx)?));
				5
			}
			OpFmt::IndyRef => {
				let idx = r.u16()?;
				r.skip(2)?;
				ops.push(Op::Method(pool.method_ref(idx)?));
				5
			}
			OpFmt::Class => {
				let idx = r.u16()?;
				ops.push(Op::Type(pool.class_type(idx)?));
				3
			}
			OpFmt::MultiClass => {
				let idx = r.u16()?;
				let dims = r.u8()? as u32;
				ops.push(Op::MultiArray { ty: pool.class_type(idx)?, dims });
				4
			}
			OpFmt::TableSwitch | OpFmt::LookupSwitch => {
				// padding is relative to the start of the code array (JVMS 6.5)
				let pad = (4 - (off % 4)) % 4;
				r.skip(pad as usize)?;
				let default = (r.i32()? as i64 + off as i64) as u32;
				let npairs = r.i32()?;
				let n = if npairs < 0 { 0usize } else { npairs as usize };
				let mut pairs = Vec::with_capacity(n);
				for _ in 0..n {
					let key = r.i32()?;
					let target = (r.i32()? as i64 + off as i64) as u32;
					pairs.push((key, target));
				}
				ops.push(Op::Switch { default, pairs });
				// both switch forms are variable length: measured from the cursor
				(r.pos() as u32 - off) as u16
			}
			OpFmt::Wide => 1,
		};
		if (off + len as u32) > code_len {
			return Err(JadxError::format(format!(
				"truncated instruction {} at code offset {} (needs {} bytes, {} left)",
				op_info.name,
				off,
				len,
				code_len - off
			)));
		}
		out.push(Insn { off, code: opcode, len, ops, wide: false });
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn table_covers_all_opcodes() {
		assert_eq!(TABLE_SIZE, 0xca);
		assert_eq!(info(0x00).name, "nop");
		assert_eq!(info(0xb1).name, "return");
		assert_eq!(info(0xbb).name, "new");
		assert_eq!(info(0xc4).name, "wide");
		assert_eq!(info(0xca).name, "unknown");
		assert_eq!(info(0xff).sem, Sem::Invalid);
		assert_eq!(info(0x9f).sem, Sem::IfCmp(Cond::Eq));
		assert_eq!(info(0x1a).sem, Sem::Load(SlotTy::Int));
		assert_eq!(info(0x2d).sem, Sem::Load(SlotTy::Obj));
		assert_eq!(info(0x4e).sem, Sem::Store(SlotTy::Obj));
		assert_eq!(info(0x4b).sem, Sem::Store(SlotTy::Obj));
		assert_eq!(info(0x3b).sem, Sem::Store(SlotTy::Int));
	}

	#[test]
	fn fixed_lengths_are_consistent_with_jadx() {
		// jadx payloadSize + 1 for the opcode byte
		assert_eq!(OpFmt::Local.fixed_len(), 2); // iload
		assert_eq!(OpFmt::Inc.fixed_len(), 3); // iinc
		assert_eq!(OpFmt::Branch.fixed_len(), 3); // ifeq
		assert_eq!(OpFmt::Ref.fixed_len(), 3); // getstatic
		assert_eq!(OpFmt::IfaceRef.fixed_len(), 5); // invokeinterface
		assert_eq!(OpFmt::IndyRef.fixed_len(), 5); // invokedynamic
		assert_eq!(OpFmt::MultiClass.fixed_len(), 4); // multianewarray
		assert_eq!(OpFmt::BranchWide.fixed_len(), 5); // goto_w
	}

	#[test]
	fn cond_inversion() {
		assert_eq!(Cond::Lt.invert(), Cond::Ge);
		assert_eq!(Cond::Ne.invert(), Cond::Eq);
	}

	#[test]
	fn newarray_types() {
		assert_eq!(array_type_code(10).unwrap(), JType::Int);
		assert_eq!(array_type_code(4).unwrap(), JType::Boolean);
		assert!(array_type_code(0).is_err());
	}

	#[test]
	fn decode_simple_sequence() {
		// iconst_1; istore_1; iload_1; ireturn
		let code = [0x04u8, 0x3c, 0x1b, 0xac];
		let pool = empty_pool();
		let insns = decode_code(&code, &pool).unwrap();
		assert_eq!(insns.len(), 4);
		assert_eq!(insns[0].sem(), Sem::Iconst);
		assert_eq!(insns[1].local(), 1);
		assert_eq!(insns[2].local(), 1);
		assert_eq!(insns[3].name(), "ireturn");
	}

	#[test]
	fn load_and_store_shorthands_encode_index_in_opcode() {
		for code in [0x1au8, 0x1b, 0x1c, 0x1d] {
			let insns = decode_code(&[code], &empty_pool()).unwrap();
			assert_eq!(insns[0].local(), (code - 0x1a) as u32);
		}
		for code in [0x4bu8, 0x4c, 0x4d, 0x4e] {
			let insns = decode_code(&[code], &empty_pool()).unwrap();
			assert_eq!(insns[0].local(), (code - 0x4b) as u32);
			assert_eq!(insns[0].sem(), Sem::Store(SlotTy::Obj));
		}
	}

	#[test]
	fn decode_illegal_opcode() {
		let code = [0xfeu8];
		assert!(decode_code(&code, &empty_pool()).is_err());
	}

	#[test]
	fn decode_wide_iinc() {
		// wide iinc #3, delta 2  =>  c4 84 00 03 00 02
		let code = [0xc4u8, 0x84, 0x00, 0x03, 0x00, 0x02];
		let insns = decode_code(&code, &empty_pool()).unwrap();
		assert_eq!(insns.len(), 1);
		assert!(insns[0].wide);
		assert_eq!(insns[0].local(), 3);
		assert_eq!(insns[0].inc_delta(), Some(2));
		assert_eq!(insns[0].len, 6);
	}

	#[test]
	fn decode_rejects_wide_of_non_widable_insn() {
		// wide iload is legal, wide iadd (0x60) is not
		let code = [0xc4u8, 0x60, 0x00, 0x01];
		assert!(decode_code(&code, &empty_pool()).is_err());
	}

	#[test]
	fn decode_tableswitch_padding() {
		// iload_0 at 0, tableswitch at 1 -> 3 pad bytes, fields start at 5
		let mut code: Vec<u8> = vec![0x1a, 0xaa, 0, 0, 0];
		code.extend_from_slice(&20i32.to_be_bytes()); // default: 1 + 20 = 21
		code.extend_from_slice(&1i32.to_be_bytes()); // 1 pair
		code.extend_from_slice(&7i32.to_be_bytes()); // key 7
		code.extend_from_slice(&3i32.to_be_bytes()); // target: 1 + 3 = 4
		code.push(0xb1); // return at 21
		assert_eq!(code.len(), 22);
		let insns = decode_code(&code, &empty_pool()).unwrap();
		assert_eq!(insns.len(), 3);
		assert_eq!(insns[1].sem(), Sem::TableSwitch);
		assert_eq!(insns[1].len, 20);
		let (default, pairs) = insns[1].switch_data().unwrap();
		assert_eq!(default, 21);
		assert_eq!(pairs, vec![(7, 4)]);
		assert_eq!(insns[1].targets(), vec![21, 4]);
		assert_eq!(insns[2].off, 21);
	}

	#[test]
	fn decode_lookupswitch_and_goto() {
		// goto 0 (self loop) at offset 0
		let code = [0xa7u8, 0xff, 0xfc];
		let insns = decode_code(&code, &empty_pool()).unwrap();
		assert_eq!(insns[0].sem(), Sem::Goto);
		assert_eq!(insns[0].branch(), Some(0));
	}

	fn empty_pool() -> ConstPool {
		// constant_pool_count = 1 => empty pool
		ConstPool::parse(&mut BinReader::new(&[0u8, 0x01])).unwrap()
	}
}
