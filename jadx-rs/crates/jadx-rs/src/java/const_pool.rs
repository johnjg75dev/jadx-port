//! Java class file constant pool.
//!
//! Port of `jadx.plugins.input.java.data.ConstPoolReader` + `ModifiedUTF8Decoder`.
//! jadx seeks back into the class file bytes lazily for every constant pool
//! access; this port decodes the pool once into an owned vector, which keeps the
//! borrow checker happy (the parsed class no longer needs to keep the raw buffer
//! alive) and costs one linear pass.

use crate::error::{JadxError, Res};
use crate::io::BinReader;
use crate::types::JType;

/// Constant pool entry tags (JVMS table 4.4-A).
pub mod tag {
	pub const UTF8: u8 = 1;
	pub const INTEGER: u8 = 3;
	pub const FLOAT: u8 = 4;
	pub const LONG: u8 = 5;
	pub const DOUBLE: u8 = 6;
	pub const CLASS: u8 = 7;
	pub const STRING: u8 = 8;
	pub const FIELD_REF: u8 = 9;
	pub const METHOD_REF: u8 = 10;
	pub const INTERFACE_METHOD_REF: u8 = 11;
	pub const NAME_AND_TYPE: u8 = 12;
	pub const METHOD_HANDLE: u8 = 15;
	pub const METHOD_TYPE: u8 = 16;
	pub const DYNAMIC: u8 = 17;
	pub const INVOKE_DYNAMIC: u8 = 18;
	pub const MODULE: u8 = 19;
	pub const PACKAGE: u8 = 20;
}

/// A resolved field/method reference: owner, name and descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
	pub class: String,
	pub name: String,
	pub desc: String,
}

impl RefInfo {
	pub fn method_desc(&self) -> crate::types::MethodProto {
		JType::parse_method_descriptor(&self.desc)
	}

	/// `java.lang.String.length():int` style rendering used by disassembly and the
	/// `ICodeRef` export.
	pub fn full_name(&self) -> String {
		format!(
			"{}.{}:{}",
			crate::types::name_to_source(&self.class, true),
			self.name,
			self.desc
		)
	}
}

/// `CONSTANT_Method_handle_info` kinds (JVMS 4.4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
	GetField,
	GetStatic,
	PutField,
	PutStatic,
	InvokeVirtual,
	InvokeStatic,
	InvokeSpecial,
	NewInvokeSpecial,
	InvokeInterface,
	Unknown(u8),
}

impl HandleKind {
	pub fn from_u8(v: u8) -> HandleKind {
		match v {
			1 => HandleKind::GetField,
			2 => HandleKind::GetStatic,
			3 => HandleKind::PutField,
			4 => HandleKind::PutStatic,
			5 => HandleKind::InvokeVirtual,
			6 => HandleKind::InvokeStatic,
			7 => HandleKind::InvokeSpecial,
			8 => HandleKind::NewInvokeSpecial,
			9 => HandleKind::InvokeInterface,
			other => HandleKind::Unknown(other),
		}
	}

	pub fn as_str(self) -> &'static str {
		match self {
			HandleKind::GetField => "getfield",
			HandleKind::GetStatic => "getstatic",
			HandleKind::PutField => "putfield",
			HandleKind::PutStatic => "putstatic",
			HandleKind::InvokeVirtual => "invokevirtual",
			HandleKind::InvokeStatic => "invokestatic",
			HandleKind::InvokeSpecial => "invokespecial",
			HandleKind::NewInvokeSpecial => "new_invoke_special",
			HandleKind::InvokeInterface => "invokeinterface",
			HandleKind::Unknown(_) => "unknown",
		}
	}
}

#[derive(Debug, Clone)]
pub struct MethodHandle {
	pub kind: HandleKind,
	/// owner/name:desc of the target method or field
	pub target: RefInfo,
}

/// Runtime constant (`ldc` payload). jadx calls this the "const pool value" and
/// models it as `EncodedValue`; only the literal-carrying variants are needed to
/// print Java source.
#[derive(Debug, Clone, PartialEq)]
pub enum Cst {
	Null,
	Int(i32),
	Float(f32),
	Long(i64),
	Double(f64),
	Str(String),
	/// `ldc` of a class constant
	Class(JType),
	/// `ldc` of a method type constant
	MethodType(String),
	MethodHandle(Box<MethodHandle>),
	/// `ldc` of a `CONSTANT_Dynamic`; resolution needs the bootstrap method, which
	/// this port does not execute.
	Dynamic {
		name: String,
		desc: String,
	},
}

impl Cst {
	pub fn type_of(&self) -> JType {
		match self {
			Cst::Null => JType::class("java/lang/Void"),
			Cst::Int(_) => JType::Int,
			Cst::Float(_) => JType::Float,
			Cst::Long(_) => JType::Long,
			Cst::Double(_) => JType::Double,
			Cst::Str(_) => JType::class("java/lang/String"),
			Cst::Class(_) => JType::class("java/lang/Class"),
			Cst::MethodType(_) => JType::class("java/lang/Class"),
			Cst::MethodHandle(_) | Cst::Dynamic { .. } => JType::object(),
		}
	}

	pub fn is_wide(&self) -> bool {
		matches!(self, Cst::Long(_) | Cst::Double(_))
	}

	/// Render as a Java literal.
	///
	/// * `hex_int` - print integers in hex and spell out `Integer.MAX_VALUE` &
	///   friends (jadx `IntegerFormat::AUTO`/`HEX` -> `StringUtils.isHexadecimal`).
	/// * `escape_unicode` - emit `\uXXXX` for non-ASCII characters
	///   (jadx `--escape-unicode`).
	pub fn literal(&self, hex_int: bool, escape_unicode: bool) -> String {
		match self {
			Cst::Null => "null".to_string(),
			Cst::Int(v) => format_int(*v, hex_int),
			Cst::Long(v) => format_long(*v, hex_int),
			Cst::Float(v) => format_float(*v, hex_int),
			Cst::Double(v) => format_double(*v, hex_int),
			Cst::Str(s) => string_literal(s, escape_unicode),
			Cst::Class(t) => format!("{}.class", t.qualified_name()),
			Cst::MethodType(d) => format!("/* method type {} */ null", d),
			Cst::MethodHandle(h) => format!("/* handle {} {} */ null", h.kind.as_str(), h.target.full_name()),
			Cst::Dynamic { name, desc } => format!("/* dynamic {}{} */ null", name, desc),
		}
	}
}

/// jadx `StringUtils.formatInteger`: `Integer.MAX_VALUE`/`MIN_VALUE` get named,
/// everything else is hex (`AUTO`/`HEX`) or decimal.
pub fn format_int(v: i32, hex: bool) -> String {
	if hex {
		if v == i32::MAX {
			return "Integer.MAX_VALUE".to_string();
		}
		if v == i32::MIN {
			return "Integer.MIN_VALUE".to_string();
		}
		return format_number(v as i64, 4, true);
	}
	v.to_string()
}

/// jadx `StringUtils.formatLong` -- same rules plus the `L` suffix.
pub fn format_long(v: i64, hex: bool) -> String {
	if hex {
		if v == i64::MAX {
			return "Long.MAX_VALUE".to_string();
		}
		if v == i64::MIN {
			return "Long.MIN_VALUE".to_string();
		}
		return format!("{}L", format_number(v, 8, true));
	}
	format!("{}L", v)
}

fn format_number(v: i64, bytes_len: usize, hex: bool) -> String {
	if !hex {
		return v.to_string();
	}
	if v < 0 {
		// jadx cuts the leading 'f' nibbles so the literal fits the value type
		let full = format!("{:x}", v as u64);
		let len = full.len();
		format!("0x{}", &full[len - bytes_len * 2..])
	} else {
		format!("0x{:x}", v)
	}
}

/// jadx `StringUtils.formatFloat`.
pub fn format_float(v: f32, _hex: bool) -> String {
	if v.is_nan() {
		return "Float.NaN".to_string();
	}
	if v == f32::NEG_INFINITY {
		return "Float.NEGATIVE_INFINITY".to_string();
	}
	if v == f32::INFINITY {
		return "Float.POSITIVE_INFINITY".to_string();
	}
	if v == f32::MIN {
		return "Float.MIN_VALUE".to_string();
	}
	if v == f32::MAX {
		return "Float.MAX_VALUE".to_string();
	}
	if v == f32::MIN_POSITIVE {
		return "Float.MIN_NORMAL".to_string();
	}
	format!("{}f", float_digits(v as f64))
}

/// jadx `StringUtils.formatDouble`.
pub fn format_double(v: f64, _hex: bool) -> String {
	if v.is_nan() {
		return "Double.NaN".to_string();
	}
	if v == f64::NEG_INFINITY {
		return "Double.NEGATIVE_INFINITY".to_string();
	}
	if v == f64::INFINITY {
		return "Double.POSITIVE_INFINITY".to_string();
	}
	if v == f64::MIN {
		return "Double.MIN_VALUE".to_string();
	}
	if v == f64::MAX {
		return "Double.MAX_VALUE".to_string();
	}
	if v == f64::MIN_POSITIVE {
		return "Double.MIN_NORMAL".to_string();
	}
	format!("{}d", float_digits(v))
}

/// Float text that Java accepts as a floating point literal: Rust's `{}` never
/// prints an exponent, so very large/small values are switched to `e` notation
/// and integral values get a trailing `.0`.
fn float_digits(v: f64) -> String {
	let abs = if v < 0.0 { -v } else { v };
	if v != 0.0 && (abs < 1e-4 || abs >= 1e7) {
		let s = format!("{:e}", v);
		// Rust prints `1e17`; Java needs a fractional part for `1e17` to stay a
		// double literal even with an exponent (it does), but `1e17f` is fine too.
		return ensure_exponent_has_dot(&s);
	}
	let s = format!("{}", v);
	if s.contains('.') || s.contains('e') || s.contains('E') {
		s
	} else {
		format!("{}.0", s)
	}
}

fn ensure_exponent_has_dot(s: &str) -> String {
	let (mantissa, exp) = match s.find('e') {
		Some(i) => (&s[..i], &s[i..]),
		None => return s.to_string(),
	};
	if mantissa.contains('.') {
		s.to_string()
	} else {
		format!("{}.0{}", mantissa, exp)
	}
}

/// Java string literal with escapes (jadx `CodeWriterUtils.printStringLiteral`).
pub fn string_literal(s: &str, escape_unicode: bool) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('"');
	for ch in s.chars() {
		match ch {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'\u{0}' => out.push_str("\\0"),
			c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
				out.push_str(&format!("\\u{:04x}", c as u32));
			}
			c if escape_unicode && (c as u32) > 0x7e => {
				out.push_str(&format!("\\u{:04x}", c as u32));
			}
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

/// Java char literal (jadx `printCharLiteral`).
pub fn char_literal(c: char, escape_unicode: bool) -> String {
	let body = match c {
		'\'' => "'\\''".to_string(),
		'\\' => "'\\\\'".to_string(),
		'\n' => "'\\n'".to_string(),
		'\r' => "'\\r'".to_string(),
		'\t' => "'\\t'".to_string(),
		'\u{0}' => "'\\0'".to_string(),
		c if (c as u32) < 0x20 || (c as u32) == 0x7f => format!("'\\u{:04x}'", c as u32),
		c if escape_unicode && (c as u32) > 0x7e => format!("'\\u{:04x}'", c as u32),
		c => format!("'{}'", c),
	};
	body
}

/// `CONSTANT_Utf8_info` bytes are Java "modified UTF-8" (JVMS 4.4.7):
/// U+0000 is encoded as `0xC0 0x80` and supplementary characters as a pair of
/// three-byte encoded surrogates.
pub fn decode_modified_utf8(bytes: &[u8]) -> String {
	let mut chars: Vec<u16> = Vec::with_capacity(bytes.len());
	let mut i = 0usize;
	while i < bytes.len() {
		let b0 = bytes[i];
		if b0 < 0x80 {
			chars.push(b0 as u16);
			i += 1;
			continue;
		}
		if b0 & 0xe0 == 0xc0 {
			if i + 1 < bytes.len() {
				let b1 = bytes[i + 1];
				if b1 & 0xc0 == 0x80 {
					let v = (((b0 & 0x1f) as u16) << 6) | ((b1 & 0x3f) as u16);
					chars.push(v);
					i += 2;
					continue;
				}
			}
			chars.push('?');
			i += 1;
			continue;
		}
		if b0 & 0xf0 == 0xe0 {
			if i + 2 < bytes.len() {
				let b1 = bytes[i + 1];
				let b2 = bytes[i + 2];
				if b1 & 0xc0 == 0x80 && b2 & 0xc0 == 0x80 {
					let v = (((b0 & 0x0f) as u16) << 12) | (((b1 & 0x3f) as u16) << 6) | ((b2 & 0x3f) as u16);
					chars.push(v);
					i += 3;
					continue;
				}
			}
			chars.push('?');
			i += 1;
			continue;
		}
		chars.push('?');
		i += 1;
	}
	// combine surrogate pairs produced by the 6-byte encoding
	let mut out = String::with_capacity(chars.len());
	let mut idx = 0usize;
	while idx < chars.len() {
		let c1 = chars[idx];
		if (0xd800..0xdc00).contains(&c1) && idx + 1 < chars.len() && (0xdc00..0xe000).contains(&chars[idx + 1]) {
			let c2 = chars[idx + 1];
			let cp = 0x1_0000u32 + (((c1 as u32) - 0xd800) << 10) + ((c2 as u32) - 0xdc00);
			match char::from_u32(cp) {
				Some(ch) => {
					out.push(ch);
					idx += 2;
					continue;
				}
				None => {}
			}
		}
		match char::from_u32(c1 as u32) {
			Some(ch) => out.push(ch),
			None => out.push('\u{fffd}'),
		}
		idx += 1;
	}
	out
}

#[derive(Debug, Clone)]
pub enum CpInfo {
	/// slot 0 and the hole left after a Long/Double
	Empty,
	Utf8(String),
	Integer(i32),
	Float(f32),
	Long(i64),
	Double(f64),
	Class { name_index: u16 },
	Str { string_index: u16 },
	FieldRef { class_index: u16, nat_index: u16 },
	MethodRef { class_index: u16, nat_index: u16 },
	InterfaceMethodRef { class_index: u16, nat_index: u16 },
	NameAndType { name_index: u16, desc_index: u16 },
	MethodHandle { kind: u8, reference_index: u16 },
	MethodType { descriptor_index: u16 },
	Dynamic { bsm_index: u16, nat_index: u16 },
	InvokeDynamic { bsm_index: u16, nat_index: u16 },
	Module { name_index: u16 },
	Package { name_index: u16 },
}

/// The parsed constant pool. Indices are the 1-based JVM indices, so `entries`
/// has one dummy element at position 0.
#[derive(Debug, Clone)]
pub struct ConstPool {
	entries: Vec<CpInfo>,
}

impl ConstPool {
	/// jadx: `ConstPoolReader.readInfo`.
	pub fn parse(r: &mut BinReader) -> Res<ConstPool> {
		let count = r.u16()? as usize;
		let mut entries = Vec::with_capacity(count);
		entries.push(CpInfo::Empty);
		let mut i = 1usize;
		while i < count {
			let tag = r.u8()?;
			// Long and Double each take two pool slots (JVMS 4.4.5/4.4.6)
			let mut wide = false;
			let entry = match tag {
				tag::UTF8 => {
					let len = r.u16()? as usize;
					let bytes = r.bytes(len)?;
					CpInfo::Utf8(decode_modified_utf8(bytes))
				}
				tag::INTEGER => CpInfo::Integer(r.i32()?),
				tag::FLOAT => CpInfo::Float(r.f32()?),
				tag::LONG => {
					let v = r.i64()?;
					wide = true;
					CpInfo::Long(v)
				}
				tag::DOUBLE => {
					let v = r.f64()?;
					wide = true;
					CpInfo::Double(v)
				}
				tag::CLASS => CpInfo::Class { name_index: r.u16()? },
				tag::STRING => CpInfo::Str { string_index: r.u16()? },
				tag::FIELD_REF => CpInfo::FieldRef { class_index: r.u16()?, nat_index: r.u16()? },
				tag::METHOD_REF => CpInfo::MethodRef { class_index: r.u16()?, nat_index: r.u16()? },
				tag::INTERFACE_METHOD_REF => CpInfo::InterfaceMethodRef { class_index: r.u16()?, nat_index: r.u16()? },
				tag::NAME_AND_TYPE => CpInfo::NameAndType { name_index: r.u16()?, desc_index: r.u16()? },
				tag::METHOD_HANDLE => CpInfo::MethodHandle { kind: r.u8()?, reference_index: r.u16()? },
				tag::METHOD_TYPE => CpInfo::MethodType { descriptor_index: r.u16()? },
				tag::DYNAMIC => CpInfo::Dynamic { bsm_index: r.u16()?, nat_index: r.u16()? },
				tag::INVOKE_DYNAMIC => CpInfo::InvokeDynamic { bsm_index: r.u16()?, nat_index: r.u16()? },
				tag::MODULE => CpInfo::Module { name_index: r.u16()? },
				tag::PACKAGE => CpInfo::Package { name_index: r.u16()? },
				other => {
					return Err(JadxError::format(format!(
						"unknown constant pool tag {} at index {} (offset {})",
						other,
						i,
						r.pos() - 1
					)));
				}
			};
			entries.push(entry);
			if wide {
				entries.push(CpInfo::Empty);
			}
			i = entries.len();
		}
		Ok(ConstPool { entries })
	}

	pub fn len(&self) -> usize {
		self.entries.len().saturating_sub(1)
	}

	pub fn is_empty(&self) -> bool {
		self.entries.len() <= 1
	}

	pub fn entry(&self, idx: u16) -> Res<&CpInfo> {
		match idx {
			0 => Err(JadxError::format("invalid constant pool index 0")),
			i if (i as usize) < self.entries.len() => Ok(&self.entries[i as usize]),
			_ => Err(JadxError::format(format!(
				"constant pool index {} out of range (pool size {})",
				idx,
				self.len()
			))),
		}
	}

	fn utf8_entry(&self, idx: u16) -> Res<&str> {
		match self.entry(idx)? {
			CpInfo::Utf8(s) => Ok(s.as_str()),
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a Utf8 entry ({:?})",
				idx, other
			))),
		}
	}

	/// jadx: `getUtf8(int)` (returns null for index 0 -- some attributes rely on
	/// that, e.g. an absent method parameter name).
	pub fn utf8_or_null(&self, idx: u16) -> Option<&str> {
		if idx == 0 {
			return None;
		}
		match self.entry(idx) {
			Ok(CpInfo::Utf8(s)) => Some(s.as_str()),
			_ => None,
		}
	}

	pub fn utf8(&self, idx: u16) -> Res<&str> {
		self.utf8_entry(idx)
	}

	/// jadx: `getClass(int)` + `fixType` (`Lxxx;` -> `xxx`).
	pub fn class_name(&self, idx: u16) -> Res<String> {
		match self.entry(idx)? {
			CpInfo::Class { name_index } => {
				let raw = self.utf8(*name_index)?;
				Ok(fix_type(raw))
			}
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a Class entry ({:?})",
				idx, other
			))),
		}
	}

	pub fn class_type(&self, idx: u16) -> Res<JType> {
		Ok(JType::Class(self.class_name(idx)?))
	}

	pub fn class_desc(&self, idx: u16) -> Res<String> {
		// descriptor form: L<name>;
		Ok(format!("L{};", self.class_name(idx)?))
	}

	/// jadx: `getString(int)`
	pub fn string(&self, idx: u16) -> Res<String> {
		match self.entry(idx)? {
			CpInfo::Str { string_index } => Ok(self.utf8(*string_index)?.to_string()),
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a String entry ({:?})",
				idx, other
			))),
		}
	}

	fn name_and_type(&self, idx: u16) -> Res<(String, String)> {
		match self.entry(idx)? {
			CpInfo::NameAndType { name_index, desc_index } => {
				let name = self.utf8(*name_index)?.to_string();
				let desc = self.utf8(*desc_index)?.to_string();
				Ok((name, desc))
			}
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a NameAndType entry ({:?})",
				idx, other
			))),
		}
	}

	fn ref_info(&self, class_index: u16, nat_index: u16) -> Res<RefInfo> {
		let class = self.class_name(class_index)?;
		let (name, desc) = self.name_and_type(nat_index)?;
		Ok(RefInfo { class, name, desc })
	}

	/// jadx: `getFieldRef(int)`
	pub fn field_ref(&self, idx: u16) -> Res<RefInfo> {
		match self.entry(idx)? {
			CpInfo::FieldRef { class_index, nat_index } => self.ref_info(*class_index, *nat_index),
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a Fieldref ({:?})",
				idx, other
			))),
		}
	}

	/// jadx: `getMethodRef(int)` (interface method refs are accepted too, as in
	/// `getCallSite`).
	pub fn method_ref(&self, idx: u16) -> Res<RefInfo> {
		match self.entry(idx)? {
			CpInfo::MethodRef { class_index, nat_index } | CpInfo::InterfaceMethodRef { class_index, nat_index } => {
				self.ref_info(*class_index, *nat_index)
			}
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a Methodref ({:?})",
				idx, other
			))),
		}
	}

	/// jadx: `getFieldType(int)` / method descriptor lookups used by the code
	/// readers.
	pub fn field_type(&self, idx: u16) -> Res<JType> {
		match self.entry(idx)? {
			CpInfo::FieldRef { nat_index, .. } => {
				let (_, desc) = self.name_and_type(*nat_index)?;
				Ok(JType::from_descriptor(&desc))
			}
			other => Err(JadxError::format(format!("not a field ref: {:?}", other))),
		}
	}

	pub fn method_handle(&self, idx: u16) -> Res<MethodHandle> {
		match self.entry(idx)? {
			CpInfo::MethodHandle { kind, reference_index } => {
				let kind = HandleKind::from_u8(*kind);
				let target = if kind == HandleKind::GetField
					|| kind == HandleKind::GetStatic
					|| kind == HandleKind::PutField
					|| kind == HandleKind::PutStatic
				{
					self.field_ref(*reference_index)?
				} else {
					self.method_ref(*reference_index)?
				};
				Ok(MethodHandle { kind, target })
			}
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a MethodHandle ({:?})",
				idx, other
			))),
		}
	}

	pub fn method_type_desc(&self, idx: u16) -> Res<String> {
		match self.entry(idx)? {
			CpInfo::MethodType { descriptor_index } => Ok(self.utf8(*descriptor_index)?.to_string()),
			other => Err(JadxError::format(format!(
				"constant pool index {} is not a MethodType ({:?})",
				idx, other
			))),
		}
	}

	/// jadx: `readAsEncodedValue(int)`.
	pub fn resolve_const(&self, idx: u16) -> Res<Cst> {
		match self.entry(idx)? {
			CpInfo::Integer(v) => Ok(Cst::Int(*v)),
			CpInfo::Float(v) => Ok(Cst::Float(*v)),
			CpInfo::Long(v) => Ok(Cst::Long(*v)),
			CpInfo::Double(v) => Ok(Cst::Double(*v)),
			CpInfo::Str { .. } => Ok(Cst::Str(self.string(idx)?)),
			CpInfo::Class { .. } => Ok(Cst::Class(self.class_type(idx)?)),
			CpInfo::MethodType { .. } => Ok(Cst::MethodType(self.method_type_desc(idx)?)),
			CpInfo::MethodHandle { .. } => Ok(Cst::MethodHandle(Box::new(self.method_handle(idx)?))),
			CpInfo::Dynamic { nat_index, .. } | CpInfo::InvokeDynamic { nat_index, .. } => {
				let (name, desc) = self.name_and_type(*nat_index)?;
				Ok(Cst::Dynamic { name, desc })
			}
			other => Err(JadxError::format(format!(
				"constant pool index {} cannot be used as a runtime constant ({:?})",
				idx, other
			))),
		}
	}

	/// Debug dump used by the FFI `jadx_class_dump_const_pool` export.
	pub fn dump(&self) -> String {
		let mut out = String::new();
		for (i, e) in self.entries.iter().enumerate() {
			if i == 0 {
				continue;
			}
			out.push_str(&format!("#{}: ", i));
			match e {
				CpInfo::Empty => out.push_str("<unused>"),
				CpInfo::Utf8(s) => out.push_str(&format!("Utf8 {:?}", s)),
				CpInfo::Integer(v) => out.push_str(&format!("Integer {}", v)),
				CpInfo::Float(v) => out.push_str(&format!("Float {}", v)),
				CpInfo::Long(v) => out.push_str(&format!("Long {}", v)),
				CpInfo::Double(v) => out.push_str(&format!("Double {}", v)),
				CpInfo::Class { name_index } => out.push_str(&format!("Class #{}", name_index)),
				CpInfo::Str { string_index } => out.push_str(&format!("String #{}", string_index)),
				CpInfo::FieldRef { class_index, nat_index } => {
					out.push_str(&format!("Fieldref #{}.#{}", class_index, nat_index))
				}
				CpInfo::MethodRef { class_index, nat_index } => {
					out.push_str(&format!("Methodref #{}.#{}", class_index, nat_index))
				}
				CpInfo::InterfaceMethodRef { class_index, nat_index } => {
					out.push_str(&format!("InterfaceMethodref #{}.#{}", class_index, nat_index))
				}
				CpInfo::NameAndType { name_index, desc_index } => {
					out.push_str(&format!("NameAndType #{}.#{}", name_index, desc_index))
				}
				CpInfo::MethodHandle { kind, reference_index } => {
					out.push_str(&format!("MethodHandle {} #{}", kind, reference_index))
				}
				CpInfo::MethodType { descriptor_index } => out.push_str(&format!("MethodType #{}", descriptor_index)),
				CpInfo::Dynamic { bsm_index, nat_index } => {
					out.push_str(&format!("Dynamic #{} #{}", bsm_index, nat_index))
				}
				CpInfo::InvokeDynamic { bsm_index, nat_index } => {
					out.push_str(&format!("InvokeDynamic #{} #{}", bsm_index, nat_index))
				}
				CpInfo::Module { name_index } => out.push_str(&format!("Module #{}", name_index)),
				CpInfo::Package { name_index } => out.push_str(&format!("Package #{}", name_index)),
			}
			out.push('\n');
		}
		out
	}
}

/// jadx: `ConstPoolReader.fixType` -- strip the `L`/`;` wrapper if present.
pub fn fix_type(raw: &str) -> String {
	let bytes = raw.as_bytes();
	if bytes.len() >= 2 && bytes[0] == b'L' && bytes[bytes.len() - 1] == b';' {
		raw[1..raw.len() - 1].to_string()
	} else {
		raw.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fix_type_strips_wrapper() {
		assert_eq!(fix_type("Ljava/lang/String;"), "java/lang/String");
		assert_eq!(fix_type("java/lang/String"), "java/lang/String");
		assert_eq!(fix_type("L;"), "");
	}

	#[test]
	fn modified_utf8_decoding() {
		// "A\0B" encoded the modified-utf8 way
		let bytes = [0x41u8, 0xc0, 0x80, 0x42];
		assert_eq!(decode_modified_utf8(&bytes), "A\u{0}B");
		// 3-byte form for U+0800
		let bytes = [0xe0u8, 0xa0, 0x80];
		assert_eq!(decode_modified_utf8(&bytes), "\u{0800}");
		// supplementary char as surrogate pair (U+1D11E)
		let bytes = [0xedu8, 0xa0, 0xbd, 0xed, 0xb4, 0x9e];
		assert_eq!(decode_modified_utf8(&bytes), "\u{1d11e}");
	}

	#[test]
	fn literals() {
		assert_eq!(Cst::Int(12).literal(false, false), "12");
		assert_eq!(Cst::Int(0x11).literal(true, false), "0x11");
		assert_eq!(Cst::Int(-1).literal(true, false), "0xffffffff");
		assert_eq!(Cst::Int(i32::MAX).literal(true, false), "Integer.MAX_VALUE");
		assert_eq!(Cst::Long(-1).literal(false, false), "-1L");
		assert_eq!(Cst::Float(0.0).literal(false, false), "0.0f");
		assert_eq!(Cst::Double(1.5).literal(false, false), "1.5d");
		assert_eq!(Cst::Double(1e20).literal(false, false), "1.0e20d");
		assert_eq!(Cst::Float(f32::NAN).literal(false, false), "Float.NaN");
		assert_eq!(Cst::Str("a\n\"b".to_string()).literal(false, false), "\"a\\n\\\"b\"");
		assert_eq!(
			Cst::Class(JType::class("java/lang/String")).literal(false, false),
			"java.lang.String.class"
		);
		assert_eq!(char_literal('\n', false), "'\\n'");
		assert_eq!(string_literal("\u{e9}", true), "\"\\u00e9\"");
		assert_eq!(string_literal("\u{e9}", false), "\"\u{e9}\"");
	}

	#[test]
	fn number_formats() {
		assert_eq!(format_int(0x1_0000, true), "0x10000");
		assert_eq!(format_int(7, true), "7");
		assert_eq!(format_int(7, false), "7");
		assert_eq!(format_long(i64::MAX, true), "Long.MAX_VALUE");
		assert_eq!(format_float(1.5, false), "1.5f");
		assert_eq!(format_double(100.0, false), "100.0d");
	}

	#[test]
	fn pool_layout_of_wide_entries() {
		// build a tiny pool: #1 Integer, #2 Long (occupying 2 and 3), #4 Utf8
		let mut data: Vec<u8> = Vec::new();
		data.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
		data.push(3);
		data.extend_from_slice(&777i32.to_be_bytes());
		data.push(5);
		data.extend_from_slice(&(-1i64).to_be_bytes());
		data.push(1);
		data.extend_from_slice(&2u16.to_be_bytes());
		data.extend_from_slice(b"ab");
		let mut r = BinReader::new(&data);
		let pool = ConstPool::parse(&mut r).unwrap();
		assert_eq!(pool.integer_or_zero(1), 777);
		assert_eq!(pool.long_or_zero(2), -1);
		assert_eq!(pool.utf8(4).unwrap(), "ab");
	}

	impl ConstPool {
		fn integer_or_zero(&self, idx: u16) -> i32 {
			match self.entry(idx) {
				Ok(CpInfo::Integer(v)) => *v,
				_ => 0,
			}
		}
		fn long_or_zero(&self, idx: u16) -> i64 {
			match self.entry(idx) {
				Ok(CpInfo::Long(v)) => *v,
				_ => 0,
			}
		}
	}
}
