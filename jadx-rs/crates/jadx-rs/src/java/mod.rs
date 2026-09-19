//! JVM class file reading -- the Rust counterpart of the `jadx-java-input`
//! plugin (`jadx.plugins.input.java`).
//!
//! jadx's java input is organised as:
//!
//! | jadx Java class                        | this module              |
//! |----------------------------------------|--------------------------|
//! | `data.JavaClassReader`                 | [`class_file`]           |
//! | `data.ConstPoolReader`                 | [`const_pool`]           |
//! | `data.JavaCodeReader` (+ `decoders/*`) | [`insn`]                 |
//! | `data.attributes.AttributesReader`     | [`attrs`]                |
//! | `utils.DescriptorParser`               | [`crate::types`] + [`signature`] |
//! | `utils.DisasmUtils` / `JavaCodeDumper` | [`disasm`]               |
//! | `data.ClassLoaderData`                 | [`crate::input`]         |
//!
//! The reader is *lenient by default*: anything it cannot understand becomes an
//! error at the smallest possible scope (one attribute, one method), matching
//! `JadxArgs.getClsOutput`/`JavaClassReader` behaviour where a class with a
//! broken attribute still lists its fields and methods.

pub mod attrs;
pub mod class_file;
pub mod const_pool;
pub mod disasm;
pub mod insn;
pub mod signature;

pub use attrs::{
	Annotation, AttrLevel, Attrs, BootstrapMethod, CodeAttr, EncodedValue, EnclosingMethod,
	ExceptionHandler, InnerClassInfo, LineNumber, LocalVar, LocalVarType, MethodParam,
	RecordComponent, StackFrame, StackMapTable, StackVal, Visibility, read_attrs,
};
pub use class_file::{jdk_version, looks_like_class_file, FieldData, JavaClassFile, MethodData, MAGIC};
pub use const_pool::{Cst, ConstPool, HandleKind, MethodHandle, RefInfo};
pub use disasm::{disasm_class, disasm_method, insn_line};
pub use insn::{
	array_type_code, decode_code, info, BinOp, BitOp, CmpKind, Cond, Conv, Insn, InvokeKind, Op,
	Sem, ShiftOp, SlotTy,
};
pub use signature::{field_type_to_source, parse as parse_signature, NameResolver, ParsedSignature};

/// Parse a class file buffer, the entry point used by [`crate::input`] and by
/// `jadx_class_get_*`.
pub fn parse(data: &[u8]) -> crate::error::Res<JavaClassFile> {
	JavaClassFile::parse(data)
}

/// `true` when the buffer starts with `0xCAFEBABE` and therefore belongs to the
/// JVM pipeline rather than to DEX (jadx decides this in `ClassLoaderData` by
/// trying each registered input in turn).
pub fn accepts(data: &[u8]) -> bool {
	looks_like_class_file(data)
}
