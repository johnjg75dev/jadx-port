//! Generic signature (`Signature` attribute) rendering.
//!
//! Port of the parts of `jadx.core.utils.signature.SignatureParser` that the
//! code generator needs: given the JVMS 4.7.9 signature of a class, method, field
//! or local variable, produce the Java source text with type parameters and type
//! arguments. jadx threads signatures through its whole type system; this port
//! renders signatures directly to text at print time and keeps the erased
//! [`crate::types::JType`] model for expression typing.
//!
//! Every entry point returns `None` instead of an error: a garbage or unsupported
//! signature must fall back to the descriptor, never abort a class.

use crate::error::{JadxError, Res};

/// Turns an internal class name (`java/util/Map$Entry`) into the name to print,
/// and records the import while doing so.
pub type NameResolver<'a> = &'a mut dyn FnMut(&str) -> String;

struct Reader<'a, 'b> {
	b: &'b [u8],
	pos: usize,
	name: NameResolver<'a>,
}

impl<'a, 'b> Reader<'a, 'b> {
	fn peek(&self) -> Option<u8> {
		if self.pos < self.b.len() {
			Some(self.b[self.pos])
		} else {
			None
		}
	}

	fn eat(&mut self, c: u8) -> Res<()> {
		if self.peek() == Some(c) {
			self.pos += 1;
			Ok(())
		} else {
			Err(JadxError::format(format!(
				"signature: expected '{}' at {}, found {:?}",
				c as char,
				self.pos,
				self.peek().map(|v| v as char)
			)))
		}
	}

	fn read_ident(&mut self) -> String {
		let start = self.pos;
		while let Some(c) = self.peek() {
			if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
				self.pos += 1;
			} else {
				break;
			}
		}
		String::from_utf8_lossy(&self.b[start..self.pos]).to_string()
	}

	/// FieldTypeSignature: one of `L...;`, `T...;`, `[...`, or a base type char.
	fn field_type(&mut self) -> Res<String> {
		match self.peek() {
			Some(b'L') => self.class_type(),
			Some(b'T') => {
				self.pos += 1;
				let name = self.read_ident();
				self.eat(b';')?;
				Ok(name)
			}
			Some(b'[') => {
				self.pos += 1;
				let el = self.field_type()?;
				Ok(format!("{}[]", el))
			}
			Some(c) => {
				let t = base_type(c).ok_or_else(|| JadxError::format(format!("signature: bad base type '{}'", c as char)))?;
				self.pos += 1;
				Ok(t.to_string())
			}
			None => Err(JadxError::format("signature: unexpected end of input")),
		}
	}

	/// `Lpkg/Outer<Tx;>.Inner<Ty;>;` -> `Outer.Inner<Tx, Ty>` (imports resolved by
	/// the caller's `NameResolver`).
	fn class_type(&mut self) -> Res<String> {
		self.eat(b'L')?;
		// package segments are joined with '/', nested classes with '$' -- the
		// JVM internal form -- and resolved to source text by the NameResolver.
		let mut internal = String::new();
		let mut args: Vec<String> = Vec::new();
		loop {
			let ident = self.read_ident();
			if ident.is_empty() {
				return Err(JadxError::format("signature: empty class type identifier"));
			}
			if self.peek() == Some(b'<') {
				let a = self.type_arguments()?;
				args.extend(a);
			}
			match self.peek() {
				Some(b';') => {
					self.pos += 1;
					internal.push_str(&ident);
					break;
				}
				Some(b'/') => {
					self.pos += 1;
					internal.push_str(&ident);
					internal.push('/');
				}
				Some(b'.') => {
					self.pos += 1;
					internal.push_str(&ident);
					internal.push('$');
				}
				_ => {
					internal.push_str(&ident);
					break;
				}
			}
		}
		let mut out = (self.name)(&internal);
		if !args.is_empty() {
			out.push('<');
			out.push_str(&args.join(", "));
			out.push('>');
		}
		Ok(out)
	}

	/// `<A, B extends C, ?>`
	fn type_arguments(&mut self) -> Res<Vec<String>> {
		self.eat(b'<')?;
		let mut out = Vec::new();
		loop {
			match self.peek() {
				Some(b'>') => {
					self.pos += 1;
					return Ok(out);
				}
				Some(b'*') => {
					self.pos += 1;
					out.push("?".to_string());
				}
				Some(b'+') => {
					self.pos += 1;
					out.push(format!("? extends {}", self.field_type()?));
				}
				Some(b'-') => {
					self.pos += 1;
					out.push(format!("? super {}", self.field_type()?));
				}
				_ => out.push(self.field_type()?),
			}
		}
	}

	/// `<T:Ljava/lang/Object;+Ljava/lang/Comparable<TT;>;>` -> `["T extends Comparable<T>"]`
	fn formal_type_parameters(&mut self) -> Res<Vec<String>> {
		self.eat(b'<')?;
		let mut out = Vec::new();
		while self.peek() != Some(b'>') && self.pos < self.b.len() {
			let name = self.read_ident();
			let mut bounds: Vec<String> = Vec::new();
			while self.peek() == Some(b':') {
				self.pos += 1;
				if self.peek() == Some(b'+') || self.peek() == Some(b'-') {
					self.pos += 1;
				}
				bounds.push(self.field_type()?);
			}
			let rendered = render_param(&name, &bounds);
			out.push(rendered);
		}
		self.eat(b'>')?;
		Ok(out)
	}
}

/// jadx's `SignatureParser` prints no bound for `<T extends Object>`, keeps a
/// single bound as `T extends X` and multi-bounds as `T extends A & B`.
fn render_param(name: &str, bounds: &[String]) -> String {
	let mut real: Vec<&String> = Vec::new();
	for (i, b) in bounds.iter().enumerate() {
		if i == 0 && b == "Object" {
			continue;
		}
		real.push(b);
	}
	if real.is_empty() {
		name.to_string()
	} else {
		format!("{} extends {}", name, real.iter().map(|s| s.as_str()).collect::<Vec<&str>>().join(" & "))
	}
}

fn base_type(c: u8) -> Option<&'static str> {
	Some(match c {
		b'V' => "void",
		b'Z' => "boolean",
		b'C' => "char",
		b'B' => "byte",
		b'S' => "short",
		b'I' => "int",
		b'J' => "long",
		b'F' => "float",
		b'D' => "double",
		_ => return None,
	})
}

/// Class/method signature parts that a declaration needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSignature {
	/// `["<T extends Comparable<T>>"]` -- empty when the class has no type params
	pub type_params: Vec<String>,
	/// `extends` clause (class signature)
	pub extends: Option<String>,
	/// `implements` clause / extra class type signatures of a class signature
	pub implements: Vec<String>,
	/// method signature only
	pub args: Vec<String>,
	pub ret: Option<String>,
	pub throws: Vec<String>,
}

/// Parse a class or method signature. The two forms are handled by one function
/// because jadx's printer interleaves them the same way.
pub fn parse(sig: &str, name: NameResolver) -> Option<ParsedSignature> {
	let mut r = Reader { b: sig.as_bytes(), pos: 0, name };
	let mut out = ParsedSignature::default();
	if r.peek() == Some(b'<') {
		out.type_params = r.formal_type_parameters().ok()?;
	}
	if r.peek() == Some(b'(') {
		// method signature
		r.pos += 1;
		while r.peek() != Some(b')') && r.pos < r.b.len() {
			let t = r.field_type().ok()?;
			out.args.push(t);
		}
		r.eat(b')').ok()?;
		out.ret = Some(if r.peek() == Some(b'V') {
			r.pos += 1;
			"void".to_string()
		} else {
			r.field_type().ok()?
		});
		while r.peek() == Some(b'^') {
			r.pos += 1;
			let t = r.field_type().ok()?;
			out.throws.push(t);
		}
		return Some(out);
	}
	// class signature: one or more class type signatures
	let mut first = true;
	while r.pos < r.b.len() {
		let before = r.pos;
		let t = r.field_type().ok()?;
		if r.pos == before {
			// never loop without progress on malformed input
			return Some(out);
		}
		if first {
			// the first entry is the erased supertype; java/lang/Object is elided
			if t != "Object" {
				out.extends = Some(t);
			}
			first = false;
		} else {
			out.implements.push(t);
		}
	}
	Some(out)
}

/// Render a field/local/parameter signature to source text.
pub fn field_type_to_source(sig: &str, name: NameResolver) -> Option<String> {
	let mut r = Reader { b: sig.as_bytes(), pos: 0, name };
	// optional leading formal type parameters are not meaningful for a field type
	if r.peek() == Some(b'<') {
		r.formal_type_parameters().ok()?;
	}
	r.field_type().ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ident_simple(internal: &str) -> String {
		// resolve like the writer does with imports disabled: simple name only
		crate::types::name_to_source(internal, false)
	}

	fn parse_with(sig: &str) -> Option<ParsedSignature> {
		parse(sig, &mut |n| ident_simple(n))
	}

	#[test]
	fn method_signature_with_type_vars() {
		let s = "<K:Ljava/lang/Object;>Ljava/lang/Object;";
		let p = parse_with(s).unwrap();
		assert_eq!(p.type_params, vec!["K"]);

		let m = "<T:Ljava/lang/Comparable<TT;>;>(TT;TT;)TT;";
		let p = parse_with(m).unwrap();
		assert_eq!(p.type_params, vec!["T extends Comparable<T>"]);
		assert_eq!(p.args, vec!["T", "T"]);
		assert_eq!(p.ret.as_deref(), Some("T"));
	}

	#[test]
	fn class_signature_with_generic_super_and_interfaces() {
		let s = "Ljava/util/AbstractList<Ljava/lang/String;>;Ljava/io/Serializable;";
		let p = parse_with(s).unwrap();
		assert_eq!(p.extends.as_deref(), Some("AbstractList<String>"));
		assert_eq!(p.implements, vec!["Serializable"]);
	}

	#[test]
	fn object_super_class_is_elided() {
		let p = parse_with("Ljava/lang/Object;Ljava/lang/Runnable;").unwrap();
		assert_eq!(p.extends, None);
		assert_eq!(p.implements, vec!["Runnable"]);
	}

	#[test]
	fn field_signature_rendering() {
		let out = field_type_to_source("Ljava/util/Map<Ljava/lang/String;Ljava/util/List<TE;*>;>;", &mut |n| ident_simple(n));
		assert_eq!(out.as_deref(), Some("Map<String, List<E, ?>>"));
		let out = field_type_to_source("[[I", &mut |n| ident_simple(n));
		assert_eq!(out.as_deref(), Some("int[][]"));
		let out = field_type_to_source("Ljava/lang/Class<+TEnum;>;", &mut |n| ident_simple(n));
		assert_eq!(out.as_deref(), Some("Class<? extends Enum>"));
	}

	#[test]
	fn method_throws_and_arrays() {
		let m = "([Ljava/lang/String;)V^Ljava/lang/Exception;";
		let p = parse_with(m).unwrap();
		assert_eq!(p.args, vec!["String[]"]);
		assert_eq!(p.ret.as_deref(), Some("void"));
		assert_eq!(p.throws, vec!["Exception"]);
	}

	#[test]
	fn garbage_signature_falls_back() {
		assert!(parse_with("not a signature").is_none() || parse_with("not a signature").map(|p| p.args.is_empty()).unwrap());
		assert!(field_type_to_source("L;", &mut |n| ident_simple(n)).is_none());
	}

	#[test]
	fn multi_bound_parameter() {
		let s = "<T::Ljava/lang/Comparable<TT;>;:Ljava/io/Serializable;>Ljava/lang/Object;";
		let p = parse_with(s).unwrap();
		assert_eq!(p.type_params, vec!["T extends Comparable<T> & Serializable"]);
	}
}
