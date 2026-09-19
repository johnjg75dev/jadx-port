//! Class file attribute readers.
//!
//! Port of `jadx.plugins.input.java.data.attributes.*`. jadx dispatches on the
//! attribute name through a static `JavaAttrType` map into one reader per
//! attribute, storing payloads in a string-keyed `JavaAttrStorage`. This port
//! uses a flat struct with one typed field per attribute instead: same parsing,
//! no lookup tables, and the compiler checks that every consumer asks for a real
//! attribute.

use crate::error::{JadxError, Res};
use crate::io::BinReader;
use crate::java::const_pool::{ConstPool, Cst};
use crate::types::JType;

/// Which table the attributes belong to (jadx: `IJavaAttributeParent`). `Code`
/// is only meaningful inside a method, and a nested `Code` table is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrLevel {
	Class,
	Field,
	Method,
	Code,
}

/// `LineNumberTable` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineNumber {
	pub start_pc: u32,
	pub line: u32,
}

/// `LocalVariableTable` entry (jadx: `JavaLocalVar`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVar {
	pub start_pc: u32,
	pub length: u32,
	pub name: String,
	pub desc: String,
	pub index: u32,
}

/// `LocalVariableTypeTable` entry: same slots, generic signature instead of a
/// descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVarType {
	pub start_pc: u32,
	pub length: u32,
	pub name: String,
	pub signature: String,
	pub index: u32,
}

/// `MethodParameters` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodParam {
	pub name: Option<String>,
	pub flags: u16,
}

/// `InnerClasses` entry (jadx: `JavaInnerClsAttr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerClassInfo {
	pub inner: Option<String>,
	pub outer: Option<String>,
	pub name: Option<String>,
	pub flags: u16,
}

/// `BootstrapMethods` entry (jadx: `RawBootstrapMethod`). Indices are constant
/// pool slots, as in jadx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapMethod {
	pub method_ref_index: u16,
	pub args: Vec<u16>,
}

/// `EnclosingMethod` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnclosingMethod {
	pub class_name: String,
	pub method_name: Option<String>,
	pub method_desc: Option<String>,
}

/// `Record` attribute component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordComponent {
	pub name: String,
	pub desc: String,
}

/// Annotation visibility (jadx: `AnnotationVisibility`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
	/// `RuntimeVisibleAnnotations`
	Runtime,
	/// `RuntimeInvisibleAnnotations`
	Build,
}

/// An `element_value` payload (jadx: `EncodedValue`).
#[derive(Debug, Clone, PartialEq)]
pub enum EncodedValue {
	Bool(bool),
	Byte(i8),
	Char(char),
	Short(i16),
	Int(i32),
	Long(i64),
	Float(f32),
	Double(f64),
	Str(String),
	Class(JType),
	Enum { class: String, name: String },
	Array(Vec<EncodedValue>),
	Nested(Annotation),
	MethodType(String),
	MethodHandle(crate::java::const_pool::MethodHandle),
}

impl EncodedValue {
	pub fn type_of(&self) -> JType {
		match self {
			EncodedValue::Bool(_) => JType::Boolean,
			EncodedValue::Byte(_) => JType::Byte,
			EncodedValue::Char(_) => JType::Char,
			EncodedValue::Short(_) => JType::Short,
			EncodedValue::Int(_) => JType::Int,
			EncodedValue::Long(_) => JType::Long,
			EncodedValue::Float(_) => JType::Float,
			EncodedValue::Double(_) => JType::Double,
			EncodedValue::Str(_) => JType::class("java/lang/String"),
			EncodedValue::Class(_) | EncodedValue::MethodType(_) => JType::class("java/lang/Class"),
			EncodedValue::Enum { class, .. } => JType::Class(class.clone()),
			EncodedValue::Array(_) | EncodedValue::Nested(_) | EncodedValue::MethodHandle(_) => JType::object(),
		}
	}
}

/// `@Foo(...)` (jadx: `JadxAnnotation`).
#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
	/// internal class name of the annotation type, e.g. `java/lang/Override`
	pub class_name: String,
	pub values: Vec<(String, EncodedValue)>,
	pub visibility: Visibility,
}

impl Annotation {
	pub fn is_override(&self) -> bool {
		self.class_name == "java/lang/Override"
	}
}

/// One `Code.exception_table` entry (jadx: `JavaTryData` + `JavaSingleCatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionHandler {
	/// inclusive start of the protected region
	pub start: u32,
	/// exclusive end of the protected region
	pub end: u32,
	/// handler entry offset
	pub handler_pc: u32,
	/// `None` marks a catch-all (`catch_type == 0`), i.e. `finally` in Java
	pub catch_type: Option<JType>,
}

/// The parsed `Code` attribute. jadx's `CodeAttr` keeps only the offset into the
/// class file and decodes lazily; here the bytes are copied out so the parsed
/// class owns its data.
#[derive(Debug, Clone)]
pub struct CodeAttr {
	pub max_stack: u16,
	pub max_locals: u16,
	pub code: Vec<u8>,
	pub handlers: Vec<ExceptionHandler>,
	/// nested attributes: LineNumberTable, LocalVariableTable, StackMapTable
	pub attrs: Box<Attrs>,
}

/// `StackMapTable` verification type (JVMS 4.7.4, jadx: `StackValueType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackVal {
	Int,
	Float,
	Double,
	Long,
	/// `top`: a slot whose value was category-2 and got overwritten
	Top,
	Null,
	UninitThis,
	Obj(JType),
	Uninit(u32),
}

impl StackVal {
	/// jadx: `StackFrame.getVarType` -- the java type a slot holds, if known.
	pub fn jtype(&self) -> Option<JType> {
		match self {
			StackVal::Int => Some(JType::Int),
			StackVal::Float => Some(JType::Float),
			StackVal::Double => Some(JType::Double),
			StackVal::Long => Some(JType::Long),
			StackVal::Obj(t) => Some(t.clone()),
			StackVal::Null => Some(JType::class("java/lang/Void")),
			StackVal::UninitThis | StackVal::Uninit(_) => Some(JType::object()),
			StackVal::Top => None,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
	pub locals: Vec<StackVal>,
	pub stack: Vec<StackVal>,
}

impl StackFrame {
	/// Type of local variable slot `index`, accounting for the two slots used by
	/// `long`/`double`.
	pub fn local_type(&self, index: u32) -> Option<JType> {
		let mut slot = 0u32;
		for lv in &self.locals {
			if slot == index {
				return lv.jtype();
			}
			slot += if lv == &StackVal::Double || lv == &StackVal::Long { 2 } else { 1 };
			if slot > index {
				return None;
			}
		}
		None
	}
}

/// Frames keyed by the offset of the first instruction they apply to
/// (jadx: `StackMapTableAttr.getFor(int)`).
#[derive(Debug, Clone, Default)]
pub struct StackMapTable {
	pub frames: Vec<(u32, StackFrame)>,
}

impl StackMapTable {
	pub fn get_for(&self, offset: u32) -> Option<&StackFrame> {
		self.frames.iter().find(|(o, _)| *o == offset).map(|(_, f)| f)
	}
}

/// Everything the rest of the crate reads out of an attribute table.
#[derive(Debug, Clone, Default)]
pub struct Attrs {
	pub source_file: Option<String>,
	/// generic `Signature` of a class, field, method or code-less attribute owner
	pub signature: Option<String>,
	/// `Exceptions` attribute: internal names listed in a `throws` clause
	pub exceptions: Vec<String>,
	pub inner_classes: Vec<InnerClassInfo>,
	pub bootstrap_methods: Vec<BootstrapMethod>,
	pub const_value: Option<Cst>,
	pub code: Option<CodeAttr>,
	pub line_numbers: Vec<LineNumber>,
	pub local_vars: Vec<LocalVar>,
	pub local_var_types: Vec<LocalVarType>,
	pub method_params: Vec<MethodParam>,
	pub annotations: Vec<Annotation>,
	/// one list of annotations per method parameter
	pub param_annotations: Vec<Vec<Annotation>>,
	/// `AnnotationDefault` (annotation type members only)
	pub annotation_default: Option<EncodedValue>,
	pub stack_map: Option<StackMapTable>,
	pub enclosing_method: Option<EnclosingMethod>,
	pub nest_host: Option<String>,
	pub nest_members: Vec<String>,
	pub permitted_subclasses: Vec<String>,
	pub record_components: Vec<RecordComponent>,
	/// `Deprecated` attribute present (jadx also honours ACC_DEPRECATED)
	pub deprecated_attr: bool,
	pub synthetic_attr: bool,
}

/// Read `attributes_count` attributes (jadx: `AttributesReader.readAttributes`).
/// Unknown attributes are skipped using their 4-byte length prefix.
pub fn read_attrs(r: &mut BinReader, pool: &ConstPool, level: AttrLevel) -> Res<Attrs> {
	let count = r.u16()?;
	let mut attrs = Attrs::default();
	for _ in 0..count {
		let name_index = r.u16()?;
		let len = r.u32()? as usize;
		let start = r.pos();
		let name = pool.utf8(name_index)?.to_string();
		read_one(r, pool, level, name.as_str(), &mut attrs)?;
		// re-sync: a reader that consumed a different amount than the attribute
		// length must not desynchronise the rest of the table
		if r.pos() != start + len {
			r.abs_pos(start + len)?;
		}
	}
	Ok(attrs)
}

fn read_one(r: &mut BinReader, pool: &ConstPool, level: AttrLevel, name: &str, out: &mut Attrs) -> Res<()> {
	match name {
		"ConstantValue" => {
			let idx = r.u16()?;
			out.const_value = Some(pool.resolve_const(idx)?);
		}
		"Code" => {
			if level == AttrLevel::Method {
				out.code = Some(read_code(r, pool)?);
			}
		}
		"Exceptions" => {
			let n = r.u16()?;
			for _ in 0..n {
				let idx = r.u16()?;
				out.exceptions.push(pool.class_name(idx)?);
			}
		}
		"Signature" => {
			let idx = r.u16()?;
			out.signature = Some(pool.utf8(idx)?.to_string());
		}
		"SourceFile" => {
			let idx = r.u16()?;
			out.source_file = Some(pool.utf8(idx)?.to_string());
		}
		"InnerClasses" => {
			let n = r.u16()?;
			for _ in 0..n {
				let inner_idx = r.u16()?;
				let outer_idx = r.u16()?;
				let name_idx = r.u16()?;
				let flags = r.u16()?;
				out.inner_classes.push(InnerClassInfo {
					inner: read_class_opt(pool, inner_idx)?,
					outer: read_class_opt(pool, outer_idx)?,
					name: pool.utf8_or_null(name_idx).map(|s| s.to_string()),
					flags,
				});
			}
		}
		"BootstrapMethods" => {
			let n = r.u16()?;
			for _ in 0..n {
				let mref = r.u16()?;
				let args_count = r.u16()?;
				let mut args = Vec::with_capacity(args_count as usize);
				for _ in 0..args_count {
					args.push(r.u16()?);
				}
				out.bootstrap_methods.push(BootstrapMethod { method_ref_index: mref, args });
			}
		}
		"LineNumberTable" => {
			let n = r.u16()?;
			for _ in 0..n {
				let start_pc = r.u16()? as u32;
				let line = r.u16()? as u32;
				out.line_numbers.push(LineNumber { start_pc, line });
			}
		}
		"LocalVariableTable" => {
			let n = r.u16()?;
			for _ in 0..n {
				let start_pc = r.u16()? as u32;
				let length = r.u16()? as u32;
				let var_name = pool.utf8(r.u16()?)?.to_string();
				let desc = pool.utf8(r.u16()?)?.to_string();
				let index = r.u16()? as u32;
				out.local_vars.push(LocalVar { start_pc, length, name: var_name, desc, index });
			}
		}
		"LocalVariableTypeTable" => {
			let n = r.u16()?;
			for _ in 0..n {
				let start_pc = r.u16()? as u32;
				let length = r.u16()? as u32;
				let var_name = pool.utf8(r.u16()?)?.to_string();
				let signature = pool.utf8(r.u16()?)?.to_string();
				let index = r.u16()? as u32;
				out.local_var_types.push(LocalVarType { start_pc, length, name: var_name, signature, index });
			}
		}
		"MethodParameters" => {
			let n = r.u8()?;
			for _ in 0..n {
				let name_idx = r.u16()?;
				let flags = r.u8()? as u16;
				out.method_params.push(MethodParam { name: pool.utf8_or_null(name_idx).map(|s| s.to_string()), flags });
			}
		}
		"RuntimeVisibleAnnotations" => {
			let n = r.u16()?;
			for _ in 0..n {
				out.annotations.push(read_annotation(r, pool, Visibility::Runtime)?);
			}
		}
		"RuntimeInvisibleAnnotations" => {
			let n = r.u16()?;
			for _ in 0..n {
				out.annotations.push(read_annotation(r, pool, Visibility::Build)?);
			}
		}
		"RuntimeVisibleParameterAnnotations" => {
			read_param_annotations(r, pool, Visibility::Runtime, out)?;
		}
		"RuntimeInvisibleParameterAnnotations" => {
			read_param_annotations(r, pool, Visibility::Build, out)?;
		}
		"AnnotationDefault" => {
			out.annotation_default = Some(read_element_value(r, pool)?);
		}
		"StackMapTable" => {
			out.stack_map = Some(read_stack_map(r, pool)?);
		}
		"EnclosingMethod" => {
			let cls = r.u16()?;
			let m = r.u16()?;
			let class_name = if cls == 0 { String::new() } else { pool.class_name(cls)? };
			let mut method_name = None;
			let mut method_desc = None;
			if m != 0 {
				let ri = pool.method_ref(m)?;
				method_name = Some(ri.name);
				method_desc = Some(ri.desc);
			}
			out.enclosing_method = Some(EnclosingMethod { class_name, method_name, method_desc });
		}
		"Deprecated" => {
			out.deprecated_attr = true;
		}
		"Synthetic" => {
			out.synthetic_attr = true;
		}
		"NestHost" => {
			let idx = r.u16()?;
			out.nest_host = Some(pool.class_name(idx)?);
		}
		"NestMembers" => {
			let n = r.u16()?;
			for _ in 0..n {
				let idx = r.u16()?;
				out.nest_members.push(pool.class_name(idx)?);
			}
		}
		"PermittedSubclasses" => {
			let n = r.u16()?;
			for _ in 0..n {
				let idx = r.u16()?;
				out.permitted_subclasses.push(pool.class_name(idx)?);
			}
		}
		"Record" => {
			let n = r.u16()?;
			for _ in 0..n {
				let comp_name = pool.utf8(r.u16()?)?.to_string();
				let desc = pool.utf8(r.u16()?)?.to_string();
				out.record_components.push(RecordComponent { name: comp_name, desc });
			}
		}
		// recognised but not needed by this port (jadx: `IgnoredAttr`)
		"RuntimeVisibleTypeAnnotations"
		| "RuntimeInvisibleTypeAnnotations"
		| "Module"
		| "ModuleMainClass"
		| "ModulePackages"
		| "ModuleResolution"
		| "Package"
		| "CompilationID"
		| "SourceDebugExtension"
		| "SourceDir" => {}
		_ => {
			// unknown attribute: skipped by the length re-sync in read_attrs
		}
	}
	Ok(())
}

fn read_class_opt(pool: &ConstPool, idx: u16) -> Res<Option<String>> {
	if idx == 0 {
		return Ok(None);
	}
	Ok(Some(pool.class_name(idx)?))
}

/// jadx: `JavaParamAnnsAttr.reader` -- `u1 numParameters` then one list each.
fn read_param_annotations(r: &mut BinReader, pool: &ConstPool, vis: Visibility, out: &mut Attrs) -> Res<()> {
	let count = r.u8()?;
	// both visibility kinds merge into the same list, matching how jadx exposes
	// `JadxAttrType.RUNTIME_PARAMETER_ANNOTATIONS`
	while out.param_annotations.len() < count as usize {
		out.param_annotations.push(Vec::new());
	}
	for i in 0..count as usize {
		let n = r.u16()?;
		for _ in 0..n {
			let ann = read_annotation(r, pool, vis)?;
			out.param_annotations[i].push(ann);
		}
	}
	Ok(())
}

fn read_code(r: &mut BinReader, pool: &ConstPool) -> Res<CodeAttr> {
	let max_stack = r.u16()?;
	let max_locals = r.u16()?;
	let code_len = r.u32()? as usize;
	let code = r.bytes_owned(code_len)?;
	let handlers_count = r.u16()?;
	let mut handlers = Vec::with_capacity(handlers_count as usize);
	for _ in 0..handlers_count {
		let start = r.u16()? as u32;
		let end = r.u16()? as u32;
		let handler_pc = r.u16()? as u32;
		let catch_index = r.u16()?;
		let catch_type = if catch_index == 0 { None } else { Some(pool.class_type(catch_index)?) };
		handlers.push(ExceptionHandler { start, end, handler_pc, catch_type });
	}
	let attrs = read_attrs(r, pool, AttrLevel::Code)?;
	Ok(CodeAttr { max_stack, max_locals, code, handlers, attrs: Box::new(attrs) })
}

/// jadx: `JavaAnnotationsAttr.reader`.
fn read_annotation(r: &mut BinReader, pool: &ConstPool, visibility: Visibility) -> Res<Annotation> {
	let type_index = r.u16()?;
	let raw = pool.utf8(type_index)?.to_string();
	let class_name = crate::java::const_pool::fix_type(&raw);
	let pairs = r.u16()?;
	let mut values = Vec::with_capacity(pairs as usize);
	for _ in 0..pairs {
		let value_name = pool.utf8(r.u16()?)?.to_string();
		let value = read_element_value(r, pool)?;
		values.push((value_name, value));
	}
	Ok(Annotation { class_name, values, visibility })
}

/// jadx: `EncodedValueReader.read`.
fn read_element_value(r: &mut BinReader, pool: &ConstPool) -> Res<EncodedValue> {
	let tag = r.u8()?;
	Ok(match tag {
		b'B' => EncodedValue::Byte(r.i8()?),
		b'C' => {
			let v = r.u16()?;
			EncodedValue::Char(char::from_u32(v as u32).unwrap_or('?'))
		}
		b'D' => EncodedValue::Double(r.f64()?),
		b'F' => EncodedValue::Float(r.f32()?),
		b'I' => EncodedValue::Int(r.i32()?),
		b'J' => EncodedValue::Long(r.i64()?),
		b'S' => EncodedValue::Short(r.i32()? as i16),
		b'Z' => EncodedValue::Bool(r.u16()? != 0),
		b's' => EncodedValue::Str(pool.string(r.u16()?)?),
		b'e' => {
			let cls = pool.class_name(r.u16()?)?;
			let name = pool.utf8(r.u16()?)?.to_string();
			EncodedValue::Enum { class: cls, name }
		}
		b'c' => {
			let raw = pool.utf8(r.u16()?)?.to_string();
			if raw.starts_with('[') {
				EncodedValue::Class(JType::from_descriptor(&raw))
			} else {
				EncodedValue::Class(JType::Class(crate::java::const_pool::fix_type(&raw)))
			}
		}
		b'@' => EncodedValue::Nested(read_annotation(r, pool, Visibility::Build)?),
		b'[' => {
			let n = r.u16()?;
			let mut vals = Vec::with_capacity(n as usize);
			for _ in 0..n {
				vals.push(read_element_value(r, pool)?);
			}
			EncodedValue::Array(vals)
		}
		b'm' => EncodedValue::MethodType(pool.utf8(r.u16()?)?.to_string()),
		b'h' => EncodedValue::MethodHandle(pool.method_handle(r.u16()?)?),
		other => return Err(JadxError::format(format!("unexpected annotation element tag '{}'", other as char))),
	})
}

/// jadx: `StackMapTableReader`. Frames are delta encoded and each one inherits
/// the locals of the previous frame, so the previous frame has to be tracked
/// while parsing (jadx does the same in `StackMapTableReader.processFrame`).
fn read_stack_map(r: &mut BinReader, pool: &ConstPool) -> Res<StackMapTable> {
	let count = r.u16()?;
	let mut frames: Vec<(u32, StackFrame)> = Vec::with_capacity(count as usize);
	let mut prev_locals: Vec<StackVal> = Vec::new();
	// offset of the last frame, 0 before the first one (JVMS 4.7.4)
	let mut last_offset: u32 = 0;
	for _ in 0..count {
		let frame_type = r.u8()?;
		let locals: Vec<StackVal>;
		let mut stack: Vec<StackVal> = Vec::new();
		let delta: u32;
		match frame_type {
			0..=63 => {
				// same_frame: exactly the previous locals, empty stack
				delta = frame_type as u32;
				locals = prev_locals.clone();
			}
			64..=127 => {
				delta = (frame_type - 64) as u32;
				locals = prev_locals.clone();
				stack.push(read_verification_type(r, pool)?);
			}
			247 => {
				delta = r.u16()? as u32;
				locals = prev_locals.clone();
				stack.push(read_verification_type(r, pool)?);
			}
			248..=251 => {
				// same_locals_1_stack_item_frame_extended / chop_frame: k locals dropped
				delta = r.u16()? as u32;
				let k = (251 - frame_type + 1) as usize;
				let keep = prev_locals.len().saturating_sub(k);
				locals = prev_locals[..keep].to_vec();
			}
			252 => {
				delta = r.u16()? as u32;
				locals = Vec::new();
			}
			253..=254 => {
				// append_frame: k new locals added to the previous frame
				delta = r.u16()? as u32;
				let k = frame_type - 251;
				let mut l = prev_locals.clone();
				for _ in 0..k {
					l.push(read_verification_type(r, pool)?);
				}
				locals = l;
			}
			255 => {
				// full_frame
				delta = r.u16()? as u32;
				let nl = r.u16()?;
				let mut l = Vec::with_capacity(nl as usize);
				for _ in 0..nl {
					l.push(read_verification_type(r, pool)?);
				}
				locals = l;
				let ns = r.u16()?;
				for _ in 0..ns {
					stack.push(read_verification_type(r, pool)?);
				}
			}
			other => return Err(JadxError::format(format!("unexpected stack map frame type {}", other))),
		}
		let offset = last_offset + delta + 1;
		last_offset = offset;
		prev_locals = locals.clone();
		frames.push((offset, StackFrame { locals, stack }));
	}
	Ok(StackMapTable { frames })
}

/// jadx: `TypeInfoReader.readVerificationType`.
fn read_verification_type(r: &mut BinReader, pool: &ConstPool) -> Res<StackVal> {
	let tag = r.u8()?;
	Ok(match tag {
		0 => StackVal::Int,
		1 => StackVal::Float,
		2 => StackVal::Double,
		3 => StackVal::Long,
		4 => StackVal::Null,
		5 => StackVal::UninitThis,
		6 => StackVal::Top,
		7 => {
			let idx = r.u16()?;
			StackVal::Obj(pool.class_type(idx)?)
		}
		8 => {
			let off = r.u16()?;
			StackVal::Uninit(off as u32)
		}
		other => return Err(JadxError::format(format!("unexpected verification type tag {}", other))),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::java::const_pool::ConstPool;

	fn put_u16(v: &mut Vec<u8>, x: u16) {
		v.extend_from_slice(&x.to_be_bytes());
	}

	fn put_u32(v: &mut Vec<u8>, x: u32) {
		v.extend_from_slice(&x.to_be_bytes());
	}

	fn put_utf8(v: &mut Vec<u8>, s: &str) {
		v.push(1);
		put_u16(v, s.len() as u16);
		v.extend_from_slice(s.as_bytes());
	}

	/// reads a whole attribute table out of `data`
	fn parse_attrs(data: &[u8], pool: ConstPool, level: AttrLevel) -> Res<Attrs> {
		let mut r = BinReader::new(data);
		read_attrs(&mut r, &pool, level)
	}

	fn pool_of(entries: &[u8]) -> ConstPool {
		ConstPool::parse(&mut BinReader::new(entries)).unwrap()
	}

	/// one attribute entry: name index + body
	fn attr(name_index: u16, body: &[u8]) -> Vec<u8> {
		let mut out = Vec::new();
		put_u16(&mut out, name_index);
		put_u32(&mut out, body.len() as u32);
		out.extend_from_slice(body);
		out
	}

	fn attrs_table(entries: &[Vec<u8>]) -> Vec<u8> {
		let mut out = Vec::new();
		put_u16(&mut out, entries.len() as u16);
		for e in entries {
			out.extend_from_slice(e);
		}
		out
	}

	#[test]
	fn read_source_file_attribute() {
		// pool: #1 "SourceFile", #2 "A.java"
		let mut cp: Vec<u8> = vec![0x00, 0x03];
		put_utf8(&mut cp, "SourceFile");
		put_utf8(&mut cp, "A.java");
		let pool = pool_of(&cp);

		let mut body: Vec<u8> = Vec::new();
		put_u16(&mut body, 2);
		let data = attrs_table(&[attr(1, &body)]);
		let attrs = parse_attrs(&data, pool, AttrLevel::Class).unwrap();
		assert_eq!(attrs.source_file.as_deref(), Some("A.java"));
	}

	#[test]
	fn read_code_attribute_with_handler_and_nested_attrs() {
		// pool: #1 "Code", #2 "LineNumberTable", #3 class java/lang/Exception
		let mut cp: Vec<u8> = vec![0x00, 0x04];
		put_utf8(&mut cp, "Code");
		put_utf8(&mut cp, "LineNumberTable");
		put_utf8(&mut cp, "java/lang/Exception");
		cp.push(7); // CONSTANT_Class
		put_u16(&mut cp, 3);
		let pool = pool_of(&cp);

		// nested attribute table of the Code attribute
		let mut ln: Vec<u8> = Vec::new();
		put_u16(&mut ln, 1); // one entry
		put_u16(&mut ln, 0); // start_pc
		put_u16(&mut ln, 7); // line
		let nested = attrs_table(&[attr(2, &ln)]);

		let mut body: Vec<u8> = Vec::new();
		put_u16(&mut body, 3); // max_stack
		put_u16(&mut body, 2); // max_locals
		put_u32(&mut body, 4); // code_length
		body.extend_from_slice(&[0x1a, 0x1a, 0x1a, 0xb1]); // iload_0 x3, return
		put_u16(&mut body, 1); // exception_table_length
		put_u16(&mut body, 0); // start_pc
		put_u16(&mut body, 2); // end_pc
		put_u16(&mut body, 3); // handler_pc
		put_u16(&mut body, 4); // catch_type -> #4 (Class java/lang/Exception)
		body.extend_from_slice(&nested);

		let data = attrs_table(&[attr(1, &body)]);
		let attrs = parse_attrs(&data, pool, AttrLevel::Method).unwrap();
		let code = attrs.code.expect("Code attribute");
		assert_eq!(code.max_stack, 3);
		assert_eq!(code.max_locals, 2);
		assert_eq!(code.code, vec![0x1a, 0x1a, 0x1a, 0xb1]);
		assert_eq!(code.handlers.len(), 1);
		assert_eq!(code.handlers[0].start, 0);
		assert_eq!(code.handlers[0].end, 2);
		assert_eq!(code.handlers[0].handler_pc, 3);
		assert_eq!(code.handlers[0].catch_type, Some(JType::class("java/lang/Exception")));
		assert_eq!(code.attrs.line_numbers, vec![LineNumber { start_pc: 0, line: 7 }]);
	}

	#[test]
	fn catch_all_handler_has_no_catch_type() {
		let mut cp: Vec<u8> = vec![0x00, 0x02];
		put_utf8(&mut cp, "Code");
		let pool = pool_of(&cp);
		let mut body: Vec<u8> = Vec::new();
		put_u16(&mut body, 1); // max_stack
		put_u16(&mut body, 1); // max_locals
		put_u32(&mut body, 1); // code_length
		body.push(0xb1);
		put_u16(&mut body, 1); // one handler
		put_u16(&mut body, 0);
		put_u16(&mut body, 1);
		put_u16(&mut body, 0);
		put_u16(&mut body, 0); // catch_type 0 == finally / catch-all
		put_u16(&mut body, 0); // no nested attributes
		let data = attrs_table(&[attr(1, &body)]);
		let attrs = parse_attrs(&data, pool, AttrLevel::Method).unwrap();
		let code = attrs.code.unwrap();
		assert_eq!(code.handlers[0].catch_type, None);
	}

	#[test]
	fn annotations_are_read_with_values() {
		// pool: #1 "RuntimeVisibleAnnotations", #2 "Ljava/lang/Override;",
		//       #3 "value", #4 Utf8 "hello"
		let mut cp: Vec<u8> = vec![0x00, 0x05];
		put_utf8(&mut cp, "RuntimeVisibleAnnotations");
		put_utf8(&mut cp, "Ljava/lang/Override;");
		put_utf8(&mut cp, "value");
		put_utf8(&mut cp, "hello");
		let pool = pool_of(&cp);

		let mut body: Vec<u8> = Vec::new();
		put_u16(&mut body, 1); // 1 annotation
		put_u16(&mut body, 2); // type -> #2
		put_u16(&mut body, 1); // 1 element value pair
		put_u16(&mut body, 3); // element_name -> #3 "value"
		body.push(b's'); // tag: String
		put_u16(&mut body, 4); // const value index
		let data = attrs_table(&[attr(1, &body)]);
		let attrs = parse_attrs(&data, pool, AttrLevel::Method).unwrap();
		assert_eq!(attrs.annotations.len(), 1);
		assert!(attrs.annotations[0].is_override());
		assert_eq!(attrs.annotations[0].visibility, Visibility::Runtime);
		match &attrs.annotations[0].values[0].1 {
			EncodedValue::Str(s) => assert_eq!(s, "hello"),
			other => panic!("unexpected value {:?}", other),
		}
	}

	#[test]
	fn stack_map_frame_delta_chains() {
		let pool = pool_of(&[0x00, 0x01]);
		// 2 frames: same_frame(0) then same_frame(5) => applied at offsets 1 and 7
		let raw = [0x00u8, 0x02, 0x00, 0x05];
		let mut r = BinReader::new(&raw);
		let map = read_stack_map(&mut r, &pool).unwrap();
		assert_eq!(map.frames.len(), 2);
		assert_eq!(map.frames[0].0, 1);
		assert_eq!(map.frames[1].0, 7);
		assert!(map.get_for(7).is_some());
		assert!(map.get_for(3).is_none());
	}

	#[test]
	fn frame_local_slot_types() {
		let f = StackFrame {
			locals: vec![StackVal::Obj(JType::class("java/lang/String")), StackVal::Long, StackVal::Int],
			stack: vec![],
		};
		assert_eq!(f.local_type(0), Some(JType::class("java/lang/String")));
		// Long occupies slots 1 and 2, so slot 2 is not addressable
		assert_eq!(f.local_type(1), Some(JType::Long));
		assert_eq!(f.local_type(2), None);
		assert_eq!(f.local_type(3), Some(JType::Int));
	}

	#[test]
	fn unknown_attribute_is_skipped_by_length() {
		let mut cp: Vec<u8> = vec![0x00, 0x02];
		put_utf8(&mut cp, "MyJunk");
		put_utf8(&mut cp, "SourceFile");
		cp.extend_from_slice(&[8, 0, 1]); // #3 String -> #4? (unused)
		let pool = pool_of(&cp);
		let mut body: Vec<u8> = vec![1, 2, 3, 4, 5];
		let junk = attr(1, &body);
		let mut ln_body: Vec<u8> = Vec::new();
		put_u16(&mut ln_body, 2);
		let sf = attr(2, &ln_body);
		let data = attrs_table(&[junk, sf]);
		let attrs = parse_attrs(&data, pool, AttrLevel::Class).unwrap();
		assert_eq!(attrs.source_file.as_deref(), Some("SourceFile"));
	}
}
