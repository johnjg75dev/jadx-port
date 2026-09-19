//! JVM bytecode disassembly.
//!
//! jadx reaches for this when decompilation is not possible or not wanted:
//! `jadx-java-input`'s `JavaCodeDumper` (used by `--use-dx`-style fallbacks and
//! by `JavaMethod.getBytecodeDisasm()`) prints one line per instruction with the
//! raw offsets and the exception table. The same output is exposed here by
//! [`disasm_method`] and [`disasm_class`], and the FFI by
//! `jadx_method_get_bytecode_disasm`.

use crate::access_flags;
use crate::java::class_file::{JavaClassFile, MethodData};
use crate::java::const_pool::Cst;
use crate::java::insn::{Insn, Op};

/// One instruction line: `12: aload_1  // v1`.
pub fn insn_line(i: &Insn, pool: &crate::java::const_pool::ConstPool) -> String {
	let mut s = format!("{:>5}: {}", i.off, i.name());
	match i.ops.as_slice() {
		[] => {}
		[Op::Local(v)] => s.push_str(&format!(" v{}", v)),
		[Op::Inc { index, delta }] => s.push_str(&format!(" v{}, {}", index, delta)),
		[Op::Const(v)] => s.push_str(&format!(" {}", v)),
		[Op::Cst(c)] => s.push_str(&format!(" {}", cst_text(c, pool))),
		[Op::Branch(t)] => s.push_str(&format!(" {}", t)),
		[Op::Switch { default, pairs }] => {
			s.push_str(&format!(" default({})", default));
			for (k, t) in pairs {
				s.push_str(&format!(", {}->{}", k, t));
			}
		}
		[Op::Field(r)] => s.push_str(&format!(" {}", r.full_name())),
		[Op::Method(r)] => s.push_str(&format!(" {}", r.full_name())),
		[Op::Type(t)] => s.push_str(&format!(" {}", t.qualified_name())),
		[Op::MultiArray { ty, dims }] => s.push_str(&format!(" {} dims={}", ty.qualified_name(), dims)),
		other => {
			// never expected: every `Op` shape above is covered, this keeps a new
			// operand kind from silently disappearing from the dump
			for o in other {
				s.push_str(&format!(" {:?}", o));
			}
		}
	}
	if i.wide {
		s.push_str("  (wide)");
	}
	s
}

/// Constant pool entry as text, in the `// comment` style of `javap -c`.
pub fn cst_text(c: &Cst, pool: &crate::java::const_pool::ConstPool) -> String {
	match c {
		Cst::Str(s) => format!("// {}", s),
		Cst::Class(t) => format!("// {}", t.qualified_name()),
		Cst::MethodType(d) => format!("// {}", d),
		Cst::MethodHandle(mh) => format!("// {} {}", mh.kind.as_str(), mh.target.full_name()),
		Cst::Dynamic { name, desc, .. } => format!("// {} {}", name, desc),
		other => format!("// {}", other.literal(false, true)),
	}
}

/// Disassemble one method, including its exception table (jadx:
/// `JavaCodeDumper.dumpMethod`).
pub fn disasm_method(cls: &JavaClassFile, m: &MethodData) -> String {
	let mut out = String::new();
	out.push_str(&format!(
		"  {} {}{}\n",
		access_flags::format(m.access_flags as u32, access_flags::FlagsScope::Method).trim_end(),
		m.name,
		m.desc
	));
	let Some(code) = m.code() else {
		out.push_str("    (no Code attribute)\n");
		return out;
	};
	out.push_str(&format!(
		"    stack={}, locals={}, code_length={}\n",
		code.max_stack, code.max_locals, code.code.len()
	));
	out.push_str("    Code:\n");
	let insns = m.decode_insns_lossy(&cls.pool);
	if insns.is_empty() && !code.code.is_empty() {
		// decoding failed; dump the raw bytes instead of nothing at all
		out.push_str("      (undecodable bytecodes)");
		for (n, b) in code.code.iter().enumerate() {
			if n % 16 == 0 {
				out.push('\n');
				out.push_str("      ");
			}
			out.push_str(&format!("{:02x} ", b));
		}
		out.push('\n');
	}
	for i in &insns {
		out.push_str(&format!("      {}\n", insn_line(i, &cls.pool)));
	}
	if !code.handlers.is_empty() {
		out.push_str("      Exception table:\n");
		for h in &code.handlers {
			out.push_str(&format!(
				"      from {} to {} target {} type {}\n",
				h.start,
				h.end,
				h.handler_pc,
				match &h.catch_type {
					Some(t) => t.qualified_name(),
					// `catch_type == 0` is `finally` in javap's notation
					None => "any".to_string(),
				}
			));
		}
	}
	if !code.attrs.line_numbers.is_empty() {
		out.push_str("      LineNumberTable:\n");
		for ln in &code.attrs.line_numbers {
			out.push_str(&format!("      line {}: {}\n", ln.line, ln.start_pc));
		}
	}
	out
}

/// Disassembly of a whole class file, the body of
/// `jadx_class_get_bytecode_disasm`.
pub fn disasm_class(cls: &JavaClassFile) -> String {
	let mut out = String::new();
	out.push_str(&format!(
		"class {} {}\n",
		access_flags::format(cls.access_flags as u32, access_flags::FlagsScope::Class).trim_end(),
		cls.this_class
	));
	if let Some(s) = &cls.super_class {
		out.push_str(&format!("  super: {}\n", s));
	}
	for i in &cls.interfaces {
		out.push_str(&format!("  implements: {}\n", i));
	}
	out.push_str(&format!(
		"  minor version: {}, major version: {} (JDK {})\n",
		cls.minor_version,
		cls.major_version,
		crate::java::class_file::jdk_version(cls.major_version)
	));
	out.push_str(&format!("  fields: {}\n", cls.fields.len()));
	for f in &cls.fields {
		out.push_str(&format!(
			"    {} {} {}\n",
			access_flags::format(f.access_flags as u32, access_flags::FlagsScope::Field).trim_end(),
			f.name,
			f.desc
		));
	}
	out.push_str(&format!("  methods: {}\n", cls.methods.len()));
	for m in &cls.methods {
		out.push_str(&disasm_method(cls, m));
	}
	out
}

/// Convenience for the FFI: disassemble by class bytes.
pub fn disasm_class_bytes(data: &[u8]) -> crate::error::Res<String> {
	let cls = JavaClassFile::parse(data)?;
	Ok(disasm_class(&cls))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testutil::{ClassBuilder, CodeBuilder};

	#[test]
	fn a_method_is_disassembled_with_offsets() {
		let mut c = CodeBuilder::new(3, 2);
		c.op(0x1a); // iload_0
		c.iinc(1, 2);
		c.op(0x1c); // iload_2
		c.op(0x60); // iadd
		c.op(0xac); // ireturn
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.method(0x0002, "m", "(III)I", Some(c));
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		let text = disasm_class(&cls);
		assert!(text.contains("class pkg/T"), "{}", text);
		assert!(text.contains("m(III)I"), "{}", text);
		assert!(text.contains("iload_0"), "{}", text);
		assert!(text.contains("iadd"), "{}", text);
		assert!(text.contains("ireturn"), "{}", text);
	}

	#[test]
	fn the_exception_table_is_dumped() {
		let mut c = CodeBuilder::new(4, 3);
		let t0 = c.pc();
		c.op(0x01); // aconst_null
		c.op(0xb1); // return
		let h = c.pc();
		c.op(0x57); // pop
		c.op(0xb1); // return
		let end = c.pc();
		c.handler(crate::testutil::Handler { start: t0 as u32, end: end as u32, handler: h as u32, catch_type: 0 }); // finally
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.method(0x0002, "m", "()V", Some(c));
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		let text = disasm_class(&cls);
		assert!(text.contains("Exception table:"), "{}", text);
		assert!(text.contains("type any"), "{}", text);
	}
}
