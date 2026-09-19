//! Access flags, shared by the JVM and Dalvik front ends.
//!
//! Port of `jadx.api.plugins.input.data.AccessFlags`. The numeric values and the
//! printing order are kept identical so output matches jadx for the same input.

/// Access flag values (JVMS table 4.1-A / DEX access flags).
pub mod flag {
	pub const PUBLIC: u32 = 0x1;
	pub const PRIVATE: u32 = 0x2;
	pub const PROTECTED: u32 = 0x4;
	pub const STATIC: u32 = 0x8;
	pub const FINAL: u32 = 0x10;
	/// method-only (0x20), shares the value with `SUPER` for classes
	pub const SYNCHRONIZED: u32 = 0x20;
	pub const SUPER: u32 = 0x20;
	/// field-only (0x40), shares the value with `BRIDGE` for methods
	pub const VOLATILE: u32 = 0x40;
	pub const BRIDGE: u32 = 0x40;
	/// field-only (0x80), shares the value with `VARARGS` for methods
	pub const TRANSIENT: u32 = 0x80;
	pub const VARARGS: u32 = 0x80;
	pub const NATIVE: u32 = 0x100;
	pub const INTERFACE: u32 = 0x200;
	pub const ABSTRACT: u32 = 0x400;
	pub const STRICT: u32 = 0x800;
	pub const SYNTHETIC: u32 = 0x1000;
	pub const ANNOTATION: u32 = 0x2000;
	pub const ENUM: u32 = 0x4000;
	pub const MODULE: u32 = 0x8000;
	pub const CONSTRUCTOR: u32 = 0x10000;
	pub const DECLARED_SYNCHRONIZED: u32 = 0x20000;
	pub const DATA: u32 = 0x40000;
}

/// Which table the flags were read from, so the conflicting bit meanings are
/// resolved (jadx: `AccessFlagsScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagsScope {
	Class,
	Method,
	Field,
}

pub fn has(flags: u32, flag: u32) -> bool {
	(flags & flag) != 0
}

/// jadx: `AccessFlags.format(flags, scope)`. The trailing space is included, and
/// an empty result means "package private".
pub fn format(flags: u32, scope: FlagsScope) -> String {
	let mut out = String::new();
	push(&mut out, flags, flag::PUBLIC, "public ");
	push(&mut out, flags, flag::PRIVATE, "private ");
	push(&mut out, flags, flag::PROTECTED, "protected ");
	push(&mut out, flags, flag::STATIC, "static ");
	push(&mut out, flags, flag::FINAL, "final ");
	push(&mut out, flags, flag::ABSTRACT, "abstract ");
	push(&mut out, flags, flag::NATIVE, "native ");
	match scope {
		FlagsScope::Method => {
			push(&mut out, flags, flag::SYNCHRONIZED, "synchronized ");
			push(&mut out, flags, flag::BRIDGE, "bridge ");
			push(&mut out, flags, flag::VARARGS, "varargs ");
		}
		FlagsScope::Field => {
			push(&mut out, flags, flag::VOLATILE, "volatile ");
			push(&mut out, flags, flag::TRANSIENT, "transient ");
		}
		FlagsScope::Class => {
			push(&mut out, flags, flag::MODULE, "module ");
			push(&mut out, flags, flag::STRICT, "strict ");
			push(&mut out, flags, flag::SUPER, "super ");
			push(&mut out, flags, flag::ENUM, "enum ");
			push(&mut out, flags, flag::DATA, "data ");
		}
	}
	push(&mut out, flags, flag::SYNTHETIC, "synthetic ");
	out
}

fn push(out: &mut String, flags: u32, flag: u32, text: &str) {
	if has(flags, flag) {
		out.push_str(text);
	}
}

pub fn is_public(flags: u32) -> bool {
	has(flags, flag::PUBLIC)
}

pub fn is_static(flags: u32) -> bool {
	has(flags, flag::STATIC)
}

pub fn is_final(flags: u32) -> bool {
	has(flags, flag::FINAL)
}

pub fn is_abstract(flags: u32) -> bool {
	has(flags, flag::ABSTRACT)
}

pub fn is_synthetic(flags: u32) -> bool {
	has(flags, flag::SYNTHETIC)
}

pub fn is_bridge(flags: u32) -> bool {
	has(flags, flag::BRIDGE)
}

pub fn is_enum(flags: u32) -> bool {
	has(flags, flag::ENUM)
}

pub fn is_interface(flags: u32) -> bool {
	has(flags, flag::INTERFACE)
}

pub fn is_annotation(flags: u32) -> bool {
	has(flags, flag::ANNOTATION)
}

/// `package-info`, `synthetic` and compiler-added members are hidden in jadx when
/// `--show-synthetic` is off; the filter lives here so both front ends share it.
pub fn is_hidden_synthetic(flags: u32, show_synthetic: bool) -> bool {
	!show_synthetic && is_synthetic(flags)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn format_order_matches_jadx() {
		let f = flag::PUBLIC | flag::STATIC | flag::FINAL;
		assert_eq!(format(f, FlagsScope::Class), "public static final ");
		let m = flag::PROTECTED | flag::SYNCHRONIZED | flag::BRIDGE;
		assert_eq!(format(m, FlagsScope::Method), "protected synchronized bridge ");
		let fd = flag::PRIVATE | flag::VOLATILE | flag::TRANSIENT;
		assert_eq!(format(fd, FlagsScope::Field), "private volatile transient ");
		assert_eq!(format(0, FlagsScope::Class), "");
	}

	#[test]
	fn overlapping_bits_use_the_scope() {
		// 0x20 means `super` on a class and `synchronized` on a method
		assert_eq!(format(flag::SUPER, FlagsScope::Class), "super ");
		assert_eq!(format(flag::SYNCHRONIZED, FlagsScope::Method), "synchronized ");
	}
}
