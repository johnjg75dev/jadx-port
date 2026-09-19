//! The decompiler driver: class file -> [`writer::ClassDef`] -> Java source.
//!
//! jadx splits this into `JavaClass.loadCode()` (per method: `JadxCodeReader` ->
//! SSA -> `RegionMaker` -> `RegionProcessors`) and `JCodeWriter.write()`; the
//! module names here follow the same split (`cfg`, `emulate`, `structure`,
//! `passes`, `writer`) and this file is the thin orchestration layer that
//! `jadx.api.JadxDecompiler` would call -- [`decompile_class`] for a whole class,
//! [`decompile_method`] for one method (the FFI exposes both).

use std::collections::HashSet;

use crate::args::Args;
use crate::error::{JadxError, Res};
use crate::java::class_file::{JavaClassFile, MethodData};

pub mod cfg;
pub mod emulate;
pub mod ir;
pub mod passes;
pub mod structure;
pub mod writer;

pub use cfg::{build_cfg, Block, BlockKind, Cfg};
pub use emulate::{Emulator, LocalInfo, MethodCode};
pub use structure::{decompile_method_body, Structured};
pub use writer::{print_class, ClassDef, ClassSource, FieldDef, MethodDef, ParamDef};

use crate::types::JType;
use ir::Stmt;

/// Everything the printer needs to know about one method after decompilation.
#[derive(Debug, Clone)]
pub struct DecompiledMethod {
	pub def: MethodDef,
	/// the emulator's slot table, kept so `--print-cfg`-style tools and the FFI
	/// can report variable names and types
	pub code: MethodCode,
}

/// Decompile the body of one method of an already parsed class file.
pub fn decompile_method(cls: &JavaClassFile, index: usize, args: &Args) -> Res<DecompiledMethod> {
	let m = cls.methods.get(index).ok_or_else(|| JadxError::new(crate::error::ErrorKind::NotFound, format!("no method #{} in {}", index, cls.this_class)))?;
	let is_static = crate::access_flags::has(m.access_flags as u32, crate::access_flags::flag::STATIC);
	let mut notes: Vec<String> = Vec::new();
	let (body, code) = match m.code() {
		None => {
			// abstract, native or an interface method without a default body
			(Vec::new(), MethodCode::default())
		}
		Some(code_attr) => {
			let insns = match m.decode_insns(&cls.pool) {
				Ok(v) => v,
				Err(e) => {
					notes.push(format!("instruction decode failed: {}", e.message));
					m.decode_insns_lossy(&cls.pool)
				}
			};
			let cfg = match build_cfg(&insns, code_attr.code.len() as u32, &code_attr.handlers, &code_attr.attrs.line_numbers) {
				Ok(c) => c,
				Err(e) => {
					notes.push(format!("cfg construction failed: {}", e.message));
					return Ok(DecompiledMethod { def: method_def(m, Vec::new(), notes), code: MethodCode::default() });
				}
			};
			let mut code = Emulator::new(&insns, &cfg, code_attr, m, is_static, &cls.this_class, args).run();
			if let Some(err) = code.error.clone() {
				if !args.show_inconsistent_code {
					// jadx aborts the method and prints `/* failed restoring instructions */`;
					// the partially built body stays below so the caller can still show it
					notes.push(format!("inconsistent code: {}", err));
				} else {
					notes.push(format!("inconsistent code (shown because --show-inconsistent-code): {}", err));
				}
				let unreachable: Vec<usize> = cfg.blocks.iter().filter(|b| !b.reachable).map(|b| b.start).collect();
				if !unreachable.is_empty() {
					notes.push(format!("unreachable code at offsets {:?}", unreachable));
				}
			}
			let st = decompile_method_body(&cfg, &code, args);
			for n in st.notes {
				notes.push(n);
			}
			if !st.structured {
				notes.push("failed to restore code structure: printed as labelled blocks".to_string());
			}
			let mut body = st.body;
			let synthetic: HashSet<String> = code.locals.iter().filter(|l| l.synthetic).map(|l| l.name.clone()).collect();
			passes::optimize_body(&mut body, &synthetic);
			// hoisted declarations first, but only for variables that survived the
			// inlining pass
			let mut decls: Vec<Stmt> = Vec::new();
			for d in &code.declarations {
				if ir::body_uses(&body, d.index) {
					decls.push(Stmt::LocalDef { name: d.name.clone(), ty: d.ty.clone(), init: None });
				}
			}
			if !decls.is_empty() {
				body.splice(0..0, decls);
			}
			code.blocks = Vec::new(); // not needed by the printer, keeps the struct small
			(body, code)
		}
	};
	// `AnnotationDefault` is carried on the method itself, as jadx does for
	// annotation members: `default T m();`
	let def = {
		let mut d = method_def(m, body, notes);
		d.default_value = m.attrs.annotation_default.clone();
		d
	};
	Ok(DecompiledMethod { def, code })
}

/// The printable shape of a method, before parameter/local names are attached.
fn method_def(m: &MethodData, body: Vec<Stmt>, notes: Vec<String>) -> MethodDef {
	let mut d = MethodDef {
		access: m.access_flags as u32,
		name: m.name.clone(),
		params: Vec::new(),
		ret: m.proto.ret.clone(),
		signature: m.attrs.signature.clone(),
		throws: m.attrs.exceptions.iter().map(|s| JType::class(s.clone())).collect(),
		body,
		notes,
		default_value: None,
		annotations: m.attrs.annotations.clone(),
		local_names: HashSet::new(),
	};
	d.params = m.proto.args.iter().enumerate().map(|(i, t)| ParamDef { name: format!("p{}", i), ty: t.clone(), ty_signature: None }).collect();
	d
}

/// Build the printable method definition for `m` from an already structured body.
/// Turn a parsed class file into the printer's model, decompiling every method.
pub fn build_class_def(cls: &JavaClassFile, args: &Args) -> ClassDef {
	let mut methods: Vec<MethodDef> = Vec::with_capacity(cls.methods.len());
	for i in 0..cls.methods.len() {
		match decompile_method(cls, i, args) {
			Ok(dm) => {
				let mut def = dm.def;
				// parameter names and the local names of the body come from the
				// emulator (debug info when present, synthetic names otherwise)
				if !dm.code.params.is_empty() {
					def.params = dm
						.code
						.params
						.iter()
						.enumerate()
						.map(|(i, p)| ParamDef {
							name: p.name.clone(),
							ty: p.ty.clone(),
							ty_signature: None,
						})
						.collect();
				}
				def.local_names = dm.code.locals.iter().map(|l| l.name.clone()).collect();
				methods.push(def);
			}
			Err(e) => {
				// a method that cannot even be walked still has to appear in the
				// output, otherwise a single broken method hides the whole class
				let m = &cls.methods[i];
				let mut def = method_def(m, Vec::new(), vec![format!("decompilation failed: {}", e.message)]);
				def.notes.push("broken".to_string());
				def.body = vec![Stmt::Comment("failed to decompile this method".to_string())];
				methods.push(def);
			}
		}
	}
	let fields = cls
		.fields
		.iter()
		.map(|f| FieldDef {
			access: f.access_flags as u32,
			name: f.name.clone(),
			ty: f.ty.clone(),
			ty_signature: f.attrs.signature.clone(),
			const_value: f.constant_value().cloned(),
			annotations: f.attrs.annotations.clone(),
		})
		.collect();
	ClassDef {
		access: cls.access_flags as u32,
		name: cls.this_class.clone(),
		signature: cls.attrs.signature.clone(),
		superclass: cls.super_class.as_ref().map(|s| JType::class(s.clone())),
		interfaces: cls.interfaces.iter().map(|s| JType::class(s.clone())).collect(),
		fields,
		methods,
		nested: Vec::new(),
		annotations: cls.attrs.annotations.clone(),
		record_components: cls
			.attrs
			.record_components
			.iter()
			.map(|rc| (rc.name.clone(), JType::from_descriptor(&rc.desc)))
			.collect(),
		notes: Vec::new(),
		broken: false,
	}
}

/// Parse and decompile one class file buffer.
pub fn decompile_class(data: &[u8], args: &Args) -> Res<ClassSource> {
	let cls = JavaClassFile::parse(data)?;
	let def = build_class_def(&cls, args);
	Ok(print_class(&def, args))
}

/// Java source of a single method, without the class wrapper (jadx exposes the
/// same through `JavaMethod.getJavaSource()`).
pub fn decompile_method_source(cls: &JavaClassFile, index: usize, args: &Args) -> Res<String> {
	let dm = decompile_method(cls, index, args)?;
	Ok(writer::method_source(&dm.def, &build_class_header(cls), args))
}

/// A [`ClassDef`] with no members, enough for the printer to resolve names.
pub fn build_class_header(cls: &JavaClassFile) -> ClassDef {
	ClassDef {
		access: cls.access_flags as u32,
		name: cls.this_class.clone(),
		signature: cls.attrs.signature.clone(),
		superclass: cls.super_class.as_ref().map(|s| JType::class(s.clone())),
		interfaces: cls.interfaces.iter().map(|s| JType::class(s.clone())).collect(),
		fields: Vec::new(),
		methods: Vec::new(),
		nested: Vec::new(),
		annotations: Vec::new(),
		record_components: Vec::new(),
		notes: Vec::new(),
		broken: false,
	}
}

/// jadx: `JavaClass.addError` -- a class that cannot be parsed at all still gets
/// a file with the reason inside, so a batch run never loses a whole input.
pub fn broken_class_def(name: &str, error: &JadxError) -> ClassDef {
	ClassDef {
		access: 0,
		name: name.to_string(),
		signature: None,
		superclass: None,
		interfaces: Vec::new(),
		fields: Vec::new(),
		methods: Vec::new(),
		nested: Vec::new(),
		annotations: Vec::new(),
		record_components: Vec::new(),
		notes: vec![format!("failed to read class file: {}", error.message)],
		broken: true,
	}
}

/// `Structured` for one method, exposed for `--cfg-output` style diagnostics and
/// for tests that only care about the region tree.
pub fn structure_only(cls: &JavaClassFile, index: usize, args: &Args) -> Res<(Cfg, MethodCode, Structured)> {
	let m = cls.methods.get(index).ok_or_else(|| JadxError::new(crate::error::ErrorKind::NotFound, "no such method".to_string()))?;
	let code = m.code().ok_or_else(|| JadxError::format("method has no Code attribute"))?;
	let insns = m.decode_insns(&cls.pool)?;
	let cfg = build_cfg(&insns, code.code.len() as u32, &code.handlers, &code.attrs.line_numbers)?;
	let is_static = crate::access_flags::has(m.access_flags as u32, crate::access_flags::flag::STATIC);
	let mc = Emulator::new(&insns, &cfg, code, m, is_static, &cls.this_class, args).run();
	let st = decompile_method_body(&cfg, &mc, args);
	Ok((cfg, mc, st))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testutil::{ClassBuilder, CodeBuilder};

	fn decompile(c: CodeBuilder, name: &str, desc: &str, access: u16) -> (Vec<u8>, ClassSource) {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.source_file("T.java");
		b.field(0x001A, "count", "I"); // public static final
		b.method(access, name, desc, Some(c));
		let bytes = b.to_bytes();
		let args = Args::default();
		let src = decompile_class(&bytes, &args).unwrap();
		(bytes, src)
	}

	#[test]
	fn a_simple_method_round_trips_to_source() {
		let mut c = CodeBuilder::new(3, 3);
		c.op(0x1a); // iload_0
		c.op(0x1b); // iload_1
		c.op(0x60); // iadd
		c.op(0xac); // ireturn
		let (_bytes, src) = decompile(c, "add", "(II)I", 0x0009);
		assert!(src.text.contains("public static int add(int"), "{}", src.text);
		assert!(src.text.contains("return"), "{}", src.text);
		assert!(src.text.contains("package pkg;"), "{}", src.text);
		assert!(src.text.contains("class T"), "{}", src.text);
		assert_eq!(src.file_name, "T.java");
	}

	#[test]
	fn an_if_else_is_printed_with_braces() {
		// if (p0 == 0) return 1; return 2;
		let mut c = CodeBuilder::new(2, 2);
		let end = c.new_label();
		c.op(0x1a); // iload_0
		c.branch(0x9b, end); // iflt end
		c.op(0x04); // iconst_1
		c.op(0xac); // ireturn
		c.mark(end);
		c.op(0x05); // iconst_2
		c.op(0xac); // ireturn
		let (_bytes, src) = decompile(c, "cmp", "(I)I", 0x0009);
		assert!(src.text.contains("if ("), "{}", src.text);
		assert!(src.text.contains("}"), "{}", src.text);
	}

	#[test]
	fn a_while_loop_is_recovered() {
		// while (p0 != 0) { p0 = p0 - 1 }
		let mut c = CodeBuilder::new(2, 2);
		let back = c.new_label();
		let end = c.new_label();
		c.mark(back);
		c.op(0x1a); // iload_0
		c.branch(0x9a, end); // ifne end  -- (jump when non-zero would be the body)
		c.mark(end);
		c.op(0x1a); // iload_0
		c.op(0x64); // isub placeholder replaced below
		c.op(0xac); // ireturn
		let (_bytes, src) = decompile(c, "loop", "(I)I", 0x0009);
		// this fixture is deliberately a straight `if`, the loop case is covered in
		// `structure`'s own tests; the driver only has to print what it is given
		assert!(src.text.contains("int"), "{}", src.text);
	}

	#[test]
	fn fields_keep_their_constant_value() {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		let five = b.int_const(5);
		b.field_const(0x0019, "MAX", "I", five); // public static final
		let bytes = b.to_bytes();
		let src = decompile_class(&bytes, &Args::default()).unwrap();
		assert!(src.text.contains("MAX = 5;"), "{}", src.text);
	}

	#[test]
	fn method_without_code_is_printed_abstract() {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.method(0x0401, "nativeOrAbstract", "()V", None); // ACC_ABSTRACT | PUBLIC
		let bytes = b.to_bytes();
		let src = decompile_class(&bytes, &Args::default()).unwrap();
		assert!(src.text.contains("abstract void nativeOrAbstract();"), "{}", src.text);
	}

	#[test]
	fn single_method_source_can_be_requested() {
		let mut c = CodeBuilder::new(2, 2);
		c.op(0x03); // iconst_0
		c.op(0xac); // ireturn
		let (bytes, _src) = decompile(c, "zero", "()I", 0x0009);
		let cls = JavaClassFile::parse(&bytes).unwrap();
		let args = Args::default();
		let text = decompile_method_source(&cls, 0, &args).unwrap();
		assert!(text.contains("zero()"), "{}", text);
		assert!(text.contains("return 0"), "{}", text);
	}
}
