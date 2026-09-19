//! The `.class` file reader: `jadx.plugins.input.java.JavaClassReader` +
//! `data/JavaClassData` + `data/JavaFieldData` + `data/JavaMethodData` +
//! `data/ClassOffsets`.
//!
//! jadx keeps a `DataReader` over the whole class file and remembers byte offsets
//! (`ClassOffsets`) so field/method bodies are parsed on demand. A shared library
//! that hands out pointers into its own buffers is better off owning everything,
//! so this port parses eagerly: one pass over the file, all data owned.

use crate::error::{JadxError, Res};
use crate::io::BinReader;
use crate::java::attrs::{AttrLevel, Attrs, CodeAttr};
use crate::java::const_pool::{ConstPool, Cst};
use crate::java::insn::{self, Insn};
use crate::types::{JType, MethodProto};

/// `0xCAFEBABE` (JVMS 4.1).
pub const MAGIC: u32 = 0xcafe_babe;

#[derive(Debug, Clone)]
pub struct FieldData {
	pub access_flags: u16,
	pub name: String,
	/// raw field descriptor, e.g. `I`, `[Ljava/lang/String;` (as jadx keeps it)
	pub desc: String,
	pub ty: JType,
	pub attrs: Attrs,
}

impl FieldData {
	pub const CONST_VALUE_TAG: &str = "ConstantValue";

	/// `ConstantValue` attribute, used for `static final` initialisers.
	pub fn constant_value(&self) -> Option<&Cst> {
		self.attrs.const_value.as_ref()
	}
}

#[derive(Debug, Clone)]
pub struct MethodData {
	pub access_flags: u16,
	pub name: String,
	/// raw method descriptor, e.g. `([Ljava/lang/String;)V`
	pub desc: String,
	pub proto: MethodProto,
	pub attrs: Attrs,
}

impl MethodData {
	pub fn is_constructor(&self) -> bool {
		self.name == "<init>"
	}

	pub fn is_class_initializer(&self) -> bool {
		self.name == "<clinit>"
	}

	pub fn is_abstract(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::ABSTRACT)
	}

	pub fn is_native(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::NATIVE)
	}

	pub fn code(&self) -> Option<&CodeAttr> {
		self.attrs.code.as_ref()
	}

	/// jadx: `JavaCodeReader.read()`.
	///
	/// Errors are returned rather than swallowed so the caller can attach a
	/// `/* decompiled with error */` comment, as jadx does for inconsistent code.
	/// Takes `&self` (not `&mut self`) so a caller can hold the constant pool and
	/// this method at the same time: `cls.methods[0].decode_insns(&cls.pool)`.
	pub fn decode_insns(&self, pool: &ConstPool) -> Res<Vec<Insn>> {
		let code = self.attrs.code.as_ref().ok_or_else(|| {
			JadxError::format(format!("method {}{} has no Code attribute", self.name, self.desc))
		})?;
		insn::decode_code(&code.code, pool)
	}

	/// Decoding for display paths (disassembly), where a broken method must still
	/// produce output: an error becomes an empty instruction list.
	pub fn decode_insns_lossy(&self, pool: &ConstPool) -> Vec<Insn> {
		match self.decode_insns(pool) {
			Ok(v) => v,
			Err(_) => Vec::new(),
		}
	}

	/// Slots taken by parameters plus `this` (JVMS 2.6.1, used for stack map
	/// checks and synthetic temp allocation).
	pub fn params_slots(&self, is_static: bool) -> u32 {
		let mut slots = if is_static { 0 } else { 1 };
		for a in &self.proto.args {
			slots += if a.is_wide() { 2 } else { 1 };
		}
		slots
	}
}

#[derive(Debug, Clone)]
pub struct JavaClassFile {
	pub minor_version: u16,
	pub major_version: u16,
	pub access_flags: u16,
	/// internal name of this class, `java/lang/String`
	pub this_class: String,
	/// `None` only for `java/lang/Object`
	pub super_class: Option<String>,
	pub interfaces: Vec<String>,
	pub pool: ConstPool,
	pub fields: Vec<FieldData>,
	pub methods: Vec<MethodData>,
	pub attrs: Attrs,
	/// where the class came from (jar path or directory), set by the loader
	pub origin: String,
}

impl JavaClassFile {
	pub fn is_interface(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::INTERFACE)
	}

	pub fn is_abstract(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::ABSTRACT)
	}

	pub fn is_enum(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::ENUM)
	}

	pub fn is_record(&self) -> bool {
		!self.attrs.record_components.is_empty()
	}

	pub fn is_annotation(&self) -> bool {
		crate::access_flags::has(self.access_flags as u32, crate::access_flags::flag::ANNOTATION)
	}

	pub fn source_file(&self) -> Option<&str> {
		self.attrs.source_file.as_deref()
	}

	/// `pkg/Class#method(Ljava/lang/String;)V` -- the id jadx uses in code refs
	/// and rename mappings.
	pub fn method_id(&self, m: &MethodData) -> String {
		format!("{}#{}{}", self.this_class, m.name, m.desc)
	}

	/// Parse a class file buffer.
	pub fn parse(data: &[u8]) -> Res<JavaClassFile> {
		let mut r = BinReader::new(data);
		let magic = r.u32()?;
		if magic != MAGIC {
			return Err(JadxError::format(format!(
				"bad class file magic 0x{:08x} (expected 0x{:08x})",
				magic, MAGIC
			)));
		}
		let minor_version = r.u16()?;
		let major_version = r.u16()?;
		let pool = ConstPool::parse(&mut r)?;
		let access_flags = r.u16()?;
		let this_class = pool.class_name(r.u16()?)?;
		let super_index = r.u16()?;
		let super_class = if super_index == 0 { None } else { Some(pool.class_name(super_index)?) };
		let if_count = r.u16()?;
		let mut interfaces = Vec::with_capacity(if_count as usize);
		for _ in 0..if_count {
			interfaces.push(pool.class_name(r.u16()?)?);
		}

		let fields_count = r.u16()?;
		let mut fields = Vec::with_capacity(fields_count as usize);
		for _ in 0..fields_count {
			let f_access = r.u16()?;
			let f_name = pool.utf8(r.u16()?)?.to_string();
			let f_desc = pool.utf8(r.u16()?)?.to_string();
			let f_attrs = crate::java::attrs::read_attrs(&mut r, &pool, AttrLevel::Field)?;
			fields.push(FieldData {
				access_flags: f_access,
				name: f_name,
				desc: f_desc.clone(),
				ty: JType::from_descriptor(&f_desc),
				attrs: f_attrs,
			});
		}

		let methods_count = r.u16()?;
		let mut methods = Vec::with_capacity(methods_count as usize);
		for _ in 0..methods_count {
			let m_access = r.u16()?;
			let m_name = pool.utf8(r.u16()?)?.to_string();
			let m_desc = pool.utf8(r.u16()?)?.to_string();
			let m_attrs = crate::java::attrs::read_attrs(&mut r, &pool, AttrLevel::Method)?;
			methods.push(MethodData {
				access_flags: m_access,
				name: m_name,
				desc: m_desc.clone(),
				proto: JType::parse_method_descriptor(&m_desc),
				attrs: m_attrs,
			});
		}

		let attrs = crate::java::attrs::read_attrs(&mut r, &pool, AttrLevel::Class)?;

		Ok(JavaClassFile {
			minor_version,
			major_version,
			access_flags,
			this_class,
			super_class,
			interfaces,
			pool,
			fields,
			methods,
			attrs,
			origin: String::new(),
		})
	}
}

/// Quick magic-number probe used by the input loader to decide between `.class`
/// and other container formats.
pub fn looks_like_class_file(data: &[u8]) -> bool {
	if data.len() < 4 {
		return false;
	}
	let v = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
	v == MAGIC
}

/// jadx: `JavaClassReader.getClassVersion` -- the `major` field maps to a JDK
/// release (45 = 1.1 ... 52 = 8 ... 68 = 24).
pub fn jdk_version(major: u16) -> u16 {
	if major >= 45 {
		major - 44
	} else {
		0
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testutil::{ClassBuilder, CodeBuilder};

	fn hello_class() -> Vec<u8> {
		let mut b = ClassBuilder::new("pkg/Hello", "java/lang/Object");
		b.add_interface("java/lang/Runnable");
		b.source_file("Hello.java");
		b.field_const(0x0019, "MAX", "I", b.int_const(0x7fffffff));
		let mut c = CodeBuilder::new(2, 2);
		c.op(0x1a); // iload_0
		c.op_u1(0x3c, 1); // istore_1... (istore_1 is 0x3c with no operand)
		c.op(0xb1); // return
		b.method(0x0001, "run", "()V", Some(c));
		b.to_bytes()
	}

	#[test]
	fn parses_a_complete_class() {
		let cls = JavaClassFile::parse(&hello_class()).unwrap();
		assert_eq!(cls.major_version, 52);
		assert_eq!(jdk_version(cls.major_version), 8);
		assert_eq!(cls.this_class, "pkg/Hello");
		assert_eq!(cls.interfaces, vec!["java/lang/Runnable".to_string()]);
		assert_eq!(cls.fields.len(), 1);
		assert_eq!(cls.fields[0].ty, JType::Int);
		match cls.fields[0].constant_value() {
			Some(Cst::Int(v)) => assert_eq!(*v, 0x7fffffff),
			other => panic!("unexpected const value {:?}", other),
		}
		assert_eq!(cls.methods.len(), 1);
		let m = &cls.methods[0];
		assert_eq!(m.name, "run");
		assert_eq!(m.proto.ret, JType::Void);
		assert!(m.code().is_some());
	}

	#[test]
	fn bad_magic_is_rejected() {
		let mut data = hello_class();
		data[0] = 0x00;
		let e = JavaClassFile::parse(&data).unwrap_err();
		assert_eq!(e.kind, crate::error::ErrorKind::InputFormat);
	}

	#[test]
	fn truncated_class_is_rejected_not_panicking() {
		let data = hello_class();
		for cut in 1..data.len() {
			// every truncation must produce an error, never a panic
			let _ = JavaClassFile::parse(&data[..cut]);
		}
	}

	#[test]
	fn params_slots_account_for_wide_types() {
		let mut b = ClassBuilder::new("pkg/A", "java/lang/Object");
		let mut c = CodeBuilder::new(1, 1);
		c.op(0xb1);
		b.method(0x0001, "m", "(IJ)V", Some(c));
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		// static: I(1) + J(2) = 3 slots
		assert_eq!(cls.methods[0].params_slots(true), 3);
		// instance: +1 for `this`
		assert_eq!(cls.methods[0].params_slots(false), 4);
		assert_eq!(cls.methods[0].proto.args, vec![JType::Int, JType::Long]);
	}

	#[test]
	fn method_id_format() {
		let mut b = ClassBuilder::new("pkg/A", "java/lang/Object");
		let mut c = CodeBuilder::new(1, 1);
		c.op(0xb1);
		b.method(0x0001, "m", "()V", Some(c));
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		assert_eq!(cls.method_id(&cls.methods[0]), "pkg/A#m()V");
	}

	#[test]
	fn insns_are_decoded() {
		let mut b = ClassBuilder::new("pkg/A", "java/lang/Object");
		let mut c = CodeBuilder::new(1, 1);
		c.op(0x1a); // iload_0
		c.iinc(0, 1);
		c.op(0xb1);
		b.method(0x0001, "m", "()V", Some(c));
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		let insns = cls.methods[0].decode_insns(&cls.pool).unwrap();
		assert_eq!(insns.len(), 3);
		assert_eq!(insns[1].sem(), insn::Sem::Iinc);
		assert_eq!(insns[1].local(), 0);
		assert_eq!(insns[1].inc_delta(), Some(1));
		// a method without a Code attribute reports an error, and yields nothing in
		// the lossy form used by the disassembler
		let mut nb = ClassBuilder::new("pkg/A", "java/lang/Object");
		nb.method(0x0401, "abs", "()V", None);
		let ncls = JavaClassFile::parse(&nb.to_bytes()).unwrap();
		assert!(ncls.methods[0].decode_insns(&ncls.pool).is_err());
		assert!(ncls.methods[0].decode_insns_lossy(&ncls.pool).is_empty());
	}
}
