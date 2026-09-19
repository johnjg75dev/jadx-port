//! Java value model: types used by both the JVM and the Dalvik front ends.
//!
//! jadx keeps types as strings (`"java/lang/String"`, `"[I"`) inside
//! `JadxField`/`JavaMethodProto`; here they are a real enum so the decompiler can
//! reason about category-2 (wide) values, array element types and casts.
//! Qualified names use the JVM internal form with `/` separators, exactly as in
//! jadx; conversion to source form happens in `decompile::writer`.

use std::fmt;

/// A Java type as understood by the decompiler.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JType {
	/// `void`
	Void,
	/// `boolean`
	Boolean,
	/// `char`
	Char,
	/// `byte`
	Byte,
	/// `short`
	Short,
	/// `int`
	Int,
	/// `long`
	Long,
	/// `float`
	Float,
	/// `double`
	Double,
	/// class / interface type: internal qualified name, e.g. `java/lang/String`,
	/// `pkg/Outer$Inner`.
	Class(String),
	/// array type, `Array(Int)` is `int[]`
	Array(Box<JType>),
	/// unresolved type variable or erased placeholder, e.g. `T`
	TypeVar(String),
}

impl JType {
	pub fn void() -> JType {
		JType::Void
	}

	pub fn class<S: Into<String>>(internal_name: S) -> JType {
		JType::Class(internal_name.into())
	}

	pub fn array(el: JType) -> JType {
		JType::Array(Box::new(el))
	}

	/// The `Object` type, used as the erased supertype whenever inference fails.
	pub fn object() -> JType {
		JType::Class("java/lang/Object".to_string())
	}

	/// `boolean` for branch conditions and `int`-typed conditions in bytecode.
	pub fn int() -> JType {
		JType::Int
	}

	pub fn is_primitive(&self) -> bool {
		matches!(
			self,
			JType::Boolean
				| JType::Char
				| JType::Byte
				| JType::Short
				| JType::Int
				| JType::Long
				| JType::Float
				| JType::Double
		)
	}

	pub fn is_void(&self) -> bool {
		matches!(self, JType::Void)
	}

	pub fn is_wide(&self) -> bool {
		// category 2 values: occupy two stack slots
		matches!(self, JType::Long | JType::Double)
	}

	pub fn is_numeric(&self) -> bool {
		matches!(
			self,
			JType::Char
				| JType::Byte
				| JType::Short
				| JType::Int
				| JType::Long
				| JType::Float
				| JType::Double
		)
	}

	/// Types that the JVM treats as `int` on the operand stack.
	pub fn is_int_like(&self) -> bool {
		matches!(self, JType::Boolean | JType::Char | JType::Byte | JType::Short | JType::Int)
	}

	pub fn is_float_like(&self) -> bool {
		matches!(self, JType::Float | JType::Double)
	}

	/// `true` when the value is a reference type (may be null).
	pub fn is_reference(&self) -> bool {
		matches!(self, JType::Class(_) | JType::Array(_) | JType::TypeVar(_))
	}

	/// Boxed counterpart, used when a primitive flows into a generic position.
	pub fn boxed(&self) -> JType {
		match self {
			JType::Boolean => JType::class("java/lang/Boolean"),
			JType::Char => JType::class("java/lang/Character"),
			JType::Byte => JType::class("java/lang/Byte"),
			JType::Short => JType::class("java/lang/Short"),
			JType::Int => JType::class("java/lang/Integer"),
			JType::Long => JType::class("java/lang/Long"),
			JType::Float => JType::class("java/lang/Float"),
			JType::Double => JType::class("java/lang/Double"),
			other => other.clone(),
		}
	}

	/// Unboxes `java/lang/Integer`-like types back to primitives (jadx:
	/// `TypeMapper`/`PassedParam` unwrapping, reduced to the common cases).
	pub fn unboxed(&self) -> Option<JType> {
		match self {
			JType::Class(c) => match c.as_str() {
				"java/lang/Boolean" => Some(JType::Boolean),
				"java/lang/Character" => Some(JType::Char),
				"java/lang/Byte" => Some(JType::Byte),
				"java/lang/Short" => Some(JType::Short),
				"java/lang/Integer" => Some(JType::Int),
				"java/lang/Long" => Some(JType::Long),
				"java/lang/Float" => Some(JType::Float),
				"java/lang/Double" => Some(JType::Double),
				_ => None,
			},
			_ => None,
		}
	}

	/// Internal name of a class type, if this is one.
	pub fn class_name(&self) -> Option<&str> {
		match self {
			JType::Class(c) => Some(c.as_str()),
			_ => None,
		}
	}

	/// Root class name of an array type (`int[][]` -> `int`, `String[]` -> `String`).
	pub fn array_element(&self) -> &JType {
		match self {
			JType::Array(el) => el.array_element(),
			other => other,
		}
	}

	/// Number of `[]` suffixes.
	pub fn array_dimensions(&self) -> usize {
		match self {
			JType::Array(el) => 1 + el.array_dimensions(),
			_ => 0,
		}
	}

	/// Depth-first supertype walk used by the (very small) type inference pass:
	/// `null` and unresolved references widen to `Object`, everything else is
	/// exact. jadx runs a full `TypeUsage`/`TypeProcessor` fixpoint over the whole
	/// program; this port infers locally from descriptors + StackMapTable frames.
	pub fn is_assignable_from(&self, other: &JType) -> bool {
		if self == other {
			return true;
		}
		match (self, other) {
			(JType::Class(_), JType::Array(_)) => true, // arrays are Objects
			(JType::Class(c), JType::Class(o)) => c == o || c == "java/lang/Object" || o == "java/lang/Void",
			(JType::Class(_), JType::TypeVar(_)) => true,
			(JType::TypeVar(_), _) => true,
			(JType::Array(a), JType::Array(b)) => a.is_assignable_from(b),
			(JType::Int, x) if x.is_int_like() => true,
			(JType::Long, x) if x.is_int_like() || matches!(x, JType::Long) => true,
			(JType::Float, x) if x.is_numeric() => true,
			(JType::Double, x) if x.is_numeric() => true,
			_ => false,
		}
	}

	/// Binary numeric promotion (JLS 5.6.2) for the arithmetic ops.
	pub fn promote(a: &JType, b: &JType) -> JType {
		if a.is_float_like() || b.is_float_like() {
			if a == &JType::Double || b == &JType::Double {
				return JType::Double;
			}
			return JType::Float;
		}
		if a == &JType::Long || b == &JType::Long {
			return JType::Long;
		}
		JType::Int
	}

	/// `int` -> `JType::Int`, `Ljava/lang/String;` -> `Class`, `[I` -> `Array(Int)`.
	/// Unknown characters fall back to `TypeVar(desc)` rather than failing, so a
	/// single bad descriptor cannot abort a whole class decompilation.
	pub fn from_descriptor(desc: &str) -> JType {
		let b = desc.as_bytes();
		if b.is_empty() {
			return JType::Void;
		}
		JType::from_descriptor_at(desc, &mut 0usize).unwrap_or_else(|| JType::TypeVar(desc.to_string()))
	}

	fn from_descriptor_at(desc: &str, pos: &mut usize) -> Option<JType> {
		let bytes = desc.as_bytes();
		let i = *pos;
		if i >= bytes.len() {
			return None;
		}
		match bytes[i] {
			b'V' => {
				*pos = i + 1;
				Some(JType::Void)
			}
			b'Z' => {
				*pos = i + 1;
				Some(JType::Boolean)
			}
			b'C' => {
				*pos = i + 1;
				Some(JType::Char)
			}
			b'B' => {
				*pos = i + 1;
				Some(JType::Byte)
			}
			b'S' => {
				*pos = i + 1;
				Some(JType::Short)
			}
			b'I' => {
				*pos = i + 1;
				Some(JType::Int)
			}
			b'J' => {
				*pos = i + 1;
				Some(JType::Long)
			}
			b'F' => {
				*pos = i + 1;
				Some(JType::Float)
			}
			b'D' => {
				*pos = i + 1;
				Some(JType::Double)
			}
			b'L' => {
				let rest = &desc[i + 1..];
				match rest.find(';') {
					Some(rel) => {
						*pos = i + 1 + rel + 1;
						Some(JType::Class(rest[..rel].to_string()))
					}
					None => None,
				}
			}
			b'[' => {
				*pos = i + 1;
				let el = JType::from_descriptor_at(desc, pos)?;
				Some(JType::Array(Box::new(el)))
			}
			_ => None,
		}
	}

	/// Parse a full argument list + return type out of `(ID)V`-style descriptor.
	/// jadx: `DescriptorParser.fillMethodProto`.
	pub fn parse_method_descriptor(desc: &str) -> MethodProto {
		let mut pos = 0usize;
		let mut args = Vec::new();
		let bytes = desc.as_bytes();
		if pos < bytes.len() && bytes[pos] == b'(' {
			pos += 1;
			while pos < bytes.len() && bytes[pos] != b')' {
				match JType::from_descriptor_at(desc, &mut pos) {
					Some(t) => args.push(t),
					None => {
						// skip the offending char so we always terminate
						pos += 1;
					}
				}
			}
			if pos < bytes.len() {
				pos += 1; // ')'
			}
		}
		let ret = JType::from_descriptor_at(desc, &mut pos).unwrap_or(JType::Void);
		MethodProto { args, ret }
	}

	/// The JVMS field descriptor of this type (`JType::Class("a/b/C")` ->
	/// `La/b/C;`, `Array(Int)` -> `[I`). A type variable has no descriptor of its
	/// own, so its name is returned as-is; that only happens for types that came
	/// from a generic signature rather than from a descriptor.
	pub fn descriptor(&self) -> String {
		let mut out = String::new();
		self.write_descriptor(&mut out);
		out
	}

	fn write_descriptor(&self, out: &mut String) {
		match self {
			JType::Void => out.push('V'),
			JType::Boolean => out.push('Z'),
			JType::Char => out.push('C'),
			JType::Byte => out.push('B'),
			JType::Short => out.push('S'),
			JType::Int => out.push('I'),
			JType::Long => out.push('J'),
			JType::Float => out.push('F'),
			JType::Double => out.push('D'),
			JType::TypeVar(n) => out.push_str(n),
			JType::Class(c) => {
				out.push('L');
				out.push_str(c);
				out.push(';');
			}
			JType::Array(el) => {
				out.push('[');
				el.write_descriptor(out);
			}
		}
	}

	/// `(int, String) -> void` as `([Ljava/lang/String;)V`-style text.
	pub fn method_descriptor(args: &[JType], ret: &JType) -> String {
		let mut out = String::from("(");
		for a in args {
			out.push_str(&a.descriptor());
		}
		out.push(')');
		out.push_str(&ret.descriptor());
		out
	}

	/// `java/lang/String` -> `java.lang.String`; arrays keep `[]`.
	pub fn qualified_name(&self) -> String {
		let mut out = String::new();
		self.write_qualified(&mut out, true);
		out
	}

	/// `java/lang/String` -> `String`.
	pub fn simple_name(&self) -> String {
		let mut out = String::new();
		self.write_qualified(&mut out, false);
		out
	}

	fn write_qualified(&self, out: &mut String, qualified: bool) {
		match self {
			JType::Void => out.push_str("void"),
			JType::Boolean => out.push_str("boolean"),
			JType::Char => out.push_str("char"),
			JType::Byte => out.push_str("byte"),
			JType::Short => out.push_str("short"),
			JType::Int => out.push_str("int"),
			JType::Long => out.push_str("long"),
			JType::Float => out.push_str("float"),
			JType::Double => out.push_str("double"),
			JType::TypeVar(n) => out.push_str(n),
			JType::Class(c) => out.push_str(&name_to_source(c, qualified)),
			JType::Array(el) => {
				el.write_qualified(out, qualified);
				out.push_str("[]");
			}
		}
	}
}

impl fmt::Display for JType {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.qualified_name())
	}
}

/// `(Ljava/lang/Object;)V` -> args `[Object]`, ret `void`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodProto {
	pub args: Vec<JType>,
	pub ret: JType,
}

impl MethodProto {
	pub fn args_count(&self) -> usize {
		self.args.len()
	}

	/// Stack slots consumed by the arguments (long/double count twice).
	pub fn args_slots(&self) -> usize {
		self.args.iter().map(|t| if t.is_wide() { 2 } else { 1 }).sum()
	}
}

/// `a/b/C` -> `a.b.C`, `a/b/C$D` -> `a.b.C.D`, `C` -> `C`.
pub fn name_to_source(internal: &str, qualified: bool) -> String {
	if !qualified {
		let tail = match internal.rfind('/') {
			Some(i) => &internal[i + 1..],
			None => internal,
		};
		return tail.replace('$', ".");
	}
	internal.replace('/', ".").replace('$', ".")
}

/// Package part of an internal name: `a/b/C` -> `a/b`.
pub fn package_of(internal: &str) -> &str {
	match internal.rfind('/') {
		Some(i) => &internal[..i],
		None => "",
	}
}

/// Simple name of an internal class name: `a/b/C$D` -> `C$D`.
pub fn simple_internal_name(internal: &str) -> &str {
	match internal.rfind('/') {
		Some(i) => &internal[i + 1..],
		None => internal,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn descriptor_parsing() {
		assert_eq!(JType::from_descriptor("I"), JType::Int);
		assert_eq!(JType::from_descriptor("V"), JType::Void);
		assert_eq!(JType::from_descriptor("[[I"), JType::array(JType::array(JType::Int)));
		assert_eq!(JType::from_descriptor("Ljava/lang/String;"), JType::class("java/lang/String"));
		assert_eq!(JType::from_descriptor("Lpkg/A$B;").qualified_name(), "pkg.A.B");
	}

	#[test]
	fn method_descriptors() {
		let p = JType::parse_method_descriptor("([Ljava/lang/String;)V");
		assert_eq!(p.args, vec![JType::array(JType::class("java/lang/String"))]);
		assert_eq!(p.ret, JType::Void);

		let p = JType::parse_method_descriptor("(IDJ)Ljava/util/List;");
		assert_eq!(p.args, vec![JType::Int, JType::Double, JType::Long]);
		assert_eq!(p.args_slots(), 4);
		assert_eq!(p.ret, JType::class("java/util/List"));

		let p = JType::parse_method_descriptor("()V");
		assert!(p.args.is_empty());
		assert_eq!(p.ret, JType::Void);
	}

	#[test]
	fn slots_and_categories() {
		assert!(JType::Long.is_wide());
		assert!(JType::Double.is_wide());
		assert!(!JType::Int.is_wide());
		assert!(JType::Char.is_int_like());
		assert_eq!(JType::promote(&JType::Int, &JType::Double), JType::Double);
		assert_eq!(JType::promote(&JType::Byte, &JType::Short), JType::Int);
		assert_eq!(JType::Int.boxed(), JType::class("java/lang/Integer"));
		assert_eq!(JType::class("java/lang/Integer").unboxed(), Some(JType::Int));
	}

	#[test]
	fn names() {
		assert_eq!(name_to_source("java/util/Map$Entry", true), "java.util.Map.Entry");
		assert_eq!(name_to_source("java/util/Map$Entry", false), "Map.Entry");
		assert_eq!(package_of("a/b/C"), "a/b");
		assert_eq!(package_of("C"), "");
		assert_eq!(JType::array(JType::class("a/b/C")).simple_name(), "C[]");
	}
}
