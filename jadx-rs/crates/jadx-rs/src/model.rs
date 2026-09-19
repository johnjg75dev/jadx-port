//! The public facade of the library -- the counterpart of `jadx.api`.
//!
//! | jadx                              | this module                       |
//! |-----------------------------------|-----------------------------------|
//! | `JadxDecompiler`                  | [`Decompiler`]                    |
//! | `JavaClass`                       | [`JavaClass`]                     |
//! | `JavaMethod` / `JavaField`        | [`JavaMethod`] / [`JavaField`]    |
//! | `JavaPackage`                     | [`JavaPackage`]                   |
//! | `JavaNode.saveObjectAs`           | [`JavaClass::save_to_dir`]        |
//! | `JadxDecompiler.getPackages`      | [`Decompiler::packages`]          |
//! | `JadxDecompiler.getCodeData`      | [`Decompiler::code_data`]         |
//!
//! Like jadx, [`Decompiler::build`] does two things: *load* (parse every input
//! into the internal model, link nested classes into their outer class) and
//! *decompile* (run the whole pipeline for each class and cache the printed
//! source, so that [`JavaClass::java_source`] is cheap and progress can be
//! reported per class). jadx decompiles lazily on the first
//! `JavaClass.getJavaSource()` call; the eager variant used here keeps the shared
//! library free of interior mutability, which matters for a C ABI a caller may use
//! from several threads with one context each.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access_flags;
use crate::args::Args;
use crate::code_data::{CodeData, CodeRefType, ICodeComment, ICodeRename};
use crate::decompile::writer::{self, ClassDef, ClassSource, FieldDef, MethodDef, ParamDef};
use crate::decompile::{build_class_def, print_class};
use crate::deobf::Deobf;
use crate::error::{ErrorKind, JadxError, Res};
use crate::input::{classify, classify_bytes, CodeUnit, InputKind, Inputs};
use crate::java::class_file::JavaClassFile;
use crate::java::const_pool::Cst;
use crate::types::{package_of, simple_internal_name, JType};

/// A log line sink, the equivalent of jadx wiring slf4j through
/// `JadxDecompiler`'s logger factory.
pub type LogSink = Box<dyn Fn(&str) + Send + Sync>;
/// Called once per finished class (jadx: `IProgressListener`).
pub type ProgressSink = Box<dyn Fn(&Progress) + Send + Sync>;

/// Progress of [`Decompiler::build`].
#[derive(Debug, Clone)]
pub struct Progress {
	pub done: usize,
	pub total: usize,
	/// what is happening right now (`"decompiling a.b.C"`)
	pub what: String,
}

/// One decompiled class plus everything a caller can ask about it.
#[derive(Debug)]
pub struct ClassInner {
	/// `path`, `archive!entry` or the blob name
	pub origin: String,
	/// the parsed class file; `None` for a dex input or an unreadable class
	pub raw: Option<JavaClassFile>,
	/// the printer model, *before* renames; used for ids
	pub def_orig: ClassDef,
	/// the printer model that gets printed (renames applied during `build`)
	pub def: ClassDef,
	/// the args this class was built with (needed by the lazy per-method API)
	pub args: Args,
	/// the printed source, filled in by [`Decompiler::build`]
	pub source: ClassSource,
	pub disasm: String,
	/// smali, only for dex inputs
	pub smali: String,
	pub errors: Vec<String>,
	/// true when this class was moved into another file as a nested class
	pub parented: bool,
	/// the rename table as it looked when this class was printed, so the lazy
	/// per-method API can apply the same aliases
	pub renames: Vec<(CodeRefType, String, String)>,
	/// per-method smali, only for dex inputs
	pub dex_method_smali: Option<Vec<String>>,
}

/// A decompiled class. Cloning is cheap: the data is shared.
#[derive(Debug, Clone)]
pub struct JavaClass {
	inner: Arc<ClassInner>,
}

impl JavaClass {
	pub(crate) fn from_inner(inner: Arc<ClassInner>) -> JavaClass {
		JavaClass { inner }
	}

	/// `a/b/C` -- the internal name (jadx: `ClassInfo.getRawFullName`).
	pub fn name(&self) -> &str {
		&self.inner.def.name
	}

	/// `a.b.C` (jadx: `ClassInfo.getFullName`).
	pub fn full_name(&self) -> String {
		self.name().replace('/', ".")
	}

	/// `a/b`, `""` for the default package (jadx: `JavaClass.getPackage()`).
	pub fn package(&self) -> &str {
		package_of(&self.inner.def.name)
	}

	/// `C` (jadx: `ClassInfo.getSimpleName`).
	pub fn short_name(&self) -> String {
		simple_internal_name(self.name()).to_string()
	}

	/// the file [`JavaClass::java_source`] is saved as
	pub fn file_name(&self) -> &str {
		&self.inner.source.file_name
	}

	pub fn origin(&self) -> &str {
		&self.inner.origin
	}

	pub fn access_flags(&self) -> u32 {
		self.inner.def.access
	}

	/// The decompiled Java source including the `package`/`import` header
	/// (jadx: `JavaClass.getJavaSource()`).
	pub fn java_source(&self) -> &str {
		&self.inner.source.text
	}

	/// Imports the printer selected for this file (jadx resolves them in
	/// `UseImportsImportVisitor`).
	pub fn imports(&self) -> &[String] {
		&self.inner.source.imports
	}

	/// JVM bytecode disassembly (jadx: `JavaClass.getBytecodeDisasm()`; for the
	/// java input backend that text comes from `JavaCodeDumper`).
	pub fn bytecode_disasm(&self) -> &str {
		&self.inner.disasm
	}

	/// Smali listing, only non-empty for dex inputs.
	pub fn smali(&self) -> &str {
		&self.inner.smali
	}

	/// jadx: `JavaClass.getErrors()` -- non-empty when decompilation was partial.
	pub fn errors(&self) -> &[String] {
		&self.inner.errors
	}

	pub fn is_interface(&self) -> bool {
		access_flags::has(self.access_flags(), access_flags::flag::INTERFACE)
	}

	pub fn is_enum(&self) -> bool {
		access_flags::has(self.access_flags(), access_flags::flag::ENUM)
	}

	pub fn is_dex_input(&self) -> bool {
		self.inner.raw.is_none()
	}

	pub fn fields(&self) -> Vec<JavaField> {
		self.inner
			.def
			.fields
			.iter()
			.enumerate()
			.map(|(i, f)| JavaField {
				cls: self.clone(),
				index: i,
				name: f.name.clone(),
				ty: f.ty.clone(),
				access: f.access,
				const_value: f.const_value.clone(),
			})
			.collect()
	}

	pub fn methods(&self) -> Vec<JavaMethod> {
		self.inner
			.def
			.methods
			.iter()
			.enumerate()
			.map(|(i, m)| JavaMethod {
				cls: self.clone(),
				index: i,
				name: m.name.clone(),
				desc: m.desc_text(),
				access: m.access,
				ret: m.ret.clone(),
			})
			.collect()
	}

	/// `a/b/C#method(I)V`, the id used in `.jobf` rename entries (jadx:
	/// `MethodInfo.getRawFullId`).
	pub fn method_id(&self, index: usize) -> String {
		match self.inner.raw.as_ref().and_then(|r| r.methods.get(index)) {
			Some(m) => format!("{}#{}{}", self.inner.def_orig.name, m.name, m.desc),
			None => format!("{}#{}", self.inner.def_orig.name, index),
		}
	}

	/// `a/b/C#field:I` (jadx: `FieldInfo.getRawFullId`).
	pub fn field_id(&self, index: usize) -> String {
		match self.inner.raw.as_ref().and_then(|r| r.fields.get(index)) {
			Some(f) => format!("{}#{}:{}", self.inner.def_orig.name, f.name, f.desc),
			None => format!("{}#{}", self.inner.def_orig.name, index),
		}
	}

	/// The Java source of a single method (jadx: `JavaMethod.getJavaSource()`).
	pub fn method_source(&self, index: usize) -> String {
		let Some(raw) = self.inner.raw.as_ref() else {
			return format!("/* no code available: {} */\n", self.inner.errors.join("; "));
		};
		match crate::decompile::decompile_method(raw, index, &self.inner.args) {
			Ok(dm) => {
				let mut def = dm.def;
				// keep the caller-visible aliases in the single-method view too
				let id = format!("{}#{}{}", self.inner.def_orig.name, def.name, def.desc_text());
				if let Some(a) = self.rename_for(CodeRefType::Method, &id) {
					def.name = a;
				}
				writer::method_source(&def, &self.inner.def_orig, &self.inner.args)
			}
			Err(e) => format!("/* failed to decompile: {} */\n", e.message),
		}
	}

	/// Look up a user rename for a node id. Only available after `build()`,
	/// because that is when the deobfuscator's generated renames are added.
	pub fn rename_for(&self, kind: CodeRefType, id: &str) -> Option<String> {
		self.inner.renames.iter().find(|(k, n, _)| *k == kind && n == id).map(|(_, _, a)| a.clone())
	}

	/// Disassembly of one method (jadx: `JavaMethod.getBytecodeDisasm()`).
	pub fn method_disasm(&self, index: usize) -> String {
		match self.inner.raw.as_ref() {
			Some(raw) => match raw.methods.get(index) {
				Some(m) => crate::java::disasm::disasm_method(raw, m),
				None => String::new(),
			},
			None => self.smali_for_method(index),
		}
	}

	/// Smali of one method for dex inputs (empty for JVM classes).
	pub fn smali_for_method(&self, index: usize) -> String {
		match self.inner.dex_method_smali.as_ref().and_then(|v| v.get(index)) {
			Some(s) => s.clone(),
			None => String::new(),
		}
	}

	/// Write `<out_dir>/<package>/<Outer>.java` and return the path, the way
	/// `jadx -d out` does per class.
	pub fn save_to_dir(&self, out_dir: &Path) -> Res<PathBuf> {
		let mut dir = out_dir.to_path_buf();
		let pkg = self.package().to_string();
		if !pkg.is_empty() {
			for part in pkg.split('/') {
				dir.push(part);
			}
		}
		std::fs::create_dir_all(&dir)
			.map_err(|e| JadxError::new(ErrorKind::Io, format!("cannot create {}: {}", dir.display(), e)))?;
		let file = dir.join(self.file_name());
		std::fs::write(&file, self.java_source())
			.map_err(|e| JadxError::new(ErrorKind::Io, format!("cannot write {}: {}", file.display(), e)))?;
		Ok(file)
	}

	pub fn inner(&self) -> &Arc<ClassInner> {
		&self.inner
	}
}

/// A field of a decompiled class (jadx: `JavaField`).
#[derive(Debug, Clone)]
pub struct JavaField {
	pub cls: JavaClass,
	pub index: usize,
	pub name: String,
	pub ty: JType,
	pub access: u32,
	pub const_value: Option<Cst>,
}

impl JavaField {
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn type_name(&self) -> String {
		self.ty.qualified_name()
	}

	pub fn descriptor(&self) -> String {
		self.ty.descriptor()
	}

	pub fn access_flags(&self) -> u32 {
		self.access
	}

	/// `ConstantValue`, rendered exactly as the printer writes it (`42`, `'a'`).
	pub fn value(&self) -> Option<String> {
		self.const_value
			.as_ref()
			.map(|c| c.literal(self.cls.inner().args.print_hex_ints(), self.cls.inner().args.escape_unicode))
	}

	/// `a/b/C#field:I`
	pub fn def_id(&self) -> String {
		self.cls.field_id(self.index)
	}

	pub fn class(&self) -> &JavaClass {
		&self.cls
	}
}

/// A method of a decompiled class (jadx: `JavaMethod`).
#[derive(Debug, Clone)]
pub struct JavaMethod {
	pub cls: JavaClass,
	pub index: usize,
	pub name: String,
	pub desc: String,
	pub access: u32,
	pub ret: JType,
}

impl JavaMethod {
	pub fn name(&self) -> &str {
		&self.name
	}

	/// the raw JVM descriptor, e.g. `(I)V`
	pub fn descriptor(&self) -> &str {
		&self.desc
	}

	pub fn access_flags(&self) -> u32 {
		self.access
	}

	pub fn return_type(&self) -> String {
		self.ret.qualified_name()
	}

	pub fn is_constructor(&self) -> bool {
		self.name == "<init>"
	}

	/// `a/b/C#m(I)V`
	pub fn def_id(&self) -> String {
		self.cls.method_id(self.index)
	}

	/// The method with its declaration line and body (jadx:
	/// `JavaMethod.getJavaSource()`).
	pub fn java_source(&self) -> String {
		self.cls.method_source(self.index)
	}

	/// Bytecode disassembly of just this method.
	pub fn bytecode_disasm(&self) -> String {
		self.cls.method_disasm(self.index)
	}

	pub fn class(&self) -> &JavaClass {
		&self.cls
	}
}

/// A package view over the loaded classes (jadx: `JavaPackage`).
#[derive(Debug, Clone)]
pub struct JavaPackage {
	pub name: String,
	pub classes: Vec<JavaClass>,
}

impl JavaPackage {
	/// dotted name; `""` for the default package
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn classes(&self) -> &[JavaClass] {
		&self.classes
	}

	/// Direct children of this package, as jadx's tree shows them.
	pub fn subpackages(&self) -> Vec<String> {
		let mut out: Vec<String> = Vec::new();
		for c in &self.classes {
			let full = c.package().replace('/', ".");
			let tail = if self.name.is_empty() {
				Some(full.as_str())
			} else {
				full.strip_prefix(&format!("{}.", self.name))
			};
			if let Some(tail) = tail {
				let sub = match tail.find('.') {
					Some(p) => format!("{}.{}", self.name, &tail[..p]),
					None => tail.to_string(),
				};
				if !sub.is_empty() && !out.contains(&sub) {
					out.push(sub);
				}
			}
		}
		out.sort();
		out
	}
}

/// Load inputs, decompile them, hand out the results (jadx: `JadxDecompiler`).
#[derive(Default)]
pub struct Decompiler {
	pub args: Args,
	pub code_data: CodeData,
	classes: Vec<JavaClass>,
	errors: Vec<String>,
	inputs: Option<Inputs>,
	log: Option<LogSink>,
	progress: Option<ProgressSink>,
}

impl std::fmt::Debug for Decompiler {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Decompiler")
			.field("classes", &self.classes.len())
			.field("errors", &self.errors)
			.field("renames", &self.code_data.renames.len())
			.finish()
	}
}

impl Decompiler {
	pub fn new(args: Args) -> Decompiler {
		Decompiler { args, ..Default::default() }
	}

	/// jadx: `JadxDecompiler.addInput(File)`
	pub fn add_file(&mut self, path: impl AsRef<Path>) {
		self.args.input_paths.push(path.as_ref().to_path_buf());
	}

	/// Add a buffer directly (`jadx_ctx_add_bytes`); `name` decides how it is
	/// classified.
	pub fn add_bytes(&mut self, name: impl Into<String>, data: Vec<u8>) {
		self.args.input_blobs.push((name.into(), data));
	}

	pub fn set_code_data(&mut self, data: CodeData) {
		self.code_data = data;
	}

	pub fn code_data(&self) -> &CodeData {
		&self.code_data
	}

	pub fn code_data_mut(&mut self) -> &mut CodeData {
		&mut self.code_data
	}

	pub fn args(&self) -> &Args {
		&self.args
	}

	pub fn args_mut(&mut self) -> &mut Args {
		&mut self.args
	}

	pub fn set_log(&mut self, sink: LogSink) {
		self.log = Some(sink);
	}

	pub fn set_progress(&mut self, sink: ProgressSink) {
		self.progress = Some(sink);
	}

	/// jadx logs through slf4j; here the caller decides (the FFI forwards these to
	/// a callback).
	pub fn log(&self, msg: &str) {
		if let Some(s) = &self.log {
			s(msg);
		}
	}

	fn report(&self, done: usize, total: usize, what: &str) {
		if let Some(p) = &self.progress {
			p(&Progress { done, total, what: what.to_string() });
		}
	}

	pub fn inputs(&self) -> Option<&Inputs> {
		self.inputs.as_ref()
	}

	pub fn classes(&self) -> &[JavaClass] {
		&self.classes
	}

	pub fn class_count(&self) -> usize {
		self.classes.len()
	}

	pub fn errors(&self) -> &[String] {
		&self.errors
	}

	/// Accepts both `a.b.C` and `a/b/C`.
	pub fn find_class(&self, name: &str) -> Option<&JavaClass> {
		let internal = name.replace('.', "/");
		self.classes.iter().find(|c| c.name() == internal || c.name() == name)
	}

	pub fn class_at(&self, index: usize) -> Option<&JavaClass> {
		self.classes.get(index)
	}

	/// Classes grouped by package, sorted by name (jadx:
	/// `JadxDecompiler.getPackages`).
	pub fn packages(&self) -> Vec<JavaPackage> {
		let mut names: Vec<String> = Vec::new();
		for c in &self.classes {
			let p = c.package().replace('/', ".");
			if !names.contains(&p) {
				names.push(p);
			}
		}
		names.sort();
		names
			.into_iter()
			.map(|name| JavaPackage {
				classes: self.classes.iter().filter(|c| c.package().replace('/', ".") == name).cloned().collect(),
				name,
			})
			.collect()
	}

	/// Load and decompile everything. A failure of one input is recorded in
	/// [`Decompiler::errors`] instead of aborting the run, as jadx does.
	pub fn build(&mut self) -> Res<()> {
		let inputs = crate::input::load_inputs(&self.args)?;
		for e in &inputs.errors {
			self.errors.push(e.clone());
		}
		let total = inputs.classes.len() + inputs.dexes.len();
		self.log(&format!(
			"loaded {} class file(s), {} dex file(s)",
			inputs.classes.len(),
			inputs.dexes.len()
		));

		let mut defs: Vec<ClassInner> = Vec::with_capacity(total);
		let mut done = 0usize;

		// --- JVM class files ---------------------------------------------------
		for unit in inputs.classes {
			done += 1;
			self.report(done, total, &unit.name.replace('/', "."));
			match self.build_one(&unit) {
				Ok(inner) => defs.push(inner),
				Err(e) => {
					self.log(&format!("skipped {}", unit.origin));
					self.errors.push(e.message);
				}
			}
		}

		// --- dex files ---------------------------------------------------------
		#[cfg(feature = "dex")]
		{
			let mut dex_errors: Vec<(String, usize)> = Vec::new();
			for unit in &inputs.dexes {
				match crate::dex::DexFile::parse(&unit.data) {
					Ok(dex) => {
						let n = dex.class_defs().len();
						for i in 0..n {
							done += 1;
							self.report(done, total.max(done + 1), &format!("dex #{}", i));
							defs.push(self.build_dex_one(unit, &dex, i));
						}
					}
					Err(e) => dex_errors.push((unit.origin.clone(), 0usize)),
				}
			}
			for (origin, _) in dex_errors {
				self.errors.push(format!("{}: failed to parse dex", origin));
			}
		}
		#[cfg(not(feature = "dex"))]
		for unit in inputs.dexes {
			self.errors.push(format!("{}: dex support is not compiled in", unit.origin));
		}

		// --- deobfuscation aliases --------------------------------------------
		if self.args.deobfuscation_on {
			let mut deobf = Deobf::from_args(&self.args);
			for d in &defs {
				let cls_id = d.def_orig.name.clone();
				let pkg = package_of(&cls_id).to_string();
				if deobf.skip_package(&pkg) {
					continue;
				}
				let base = simple_internal_name(&cls_id).to_string();
				let whitelisted = self.args.deobfuscation_whitelist.iter().any(|w| {
					let w = w.trim_end_matches(".*");
					cls_id == w || cls_id.starts_with(&format!("{}/", w))
				});
				if deobf.should_rename(&base) || whitelisted {
					let alias = deobf.class_alias(&base, d.def_orig.access);
					self.code_data.add_rename(ICodeRename::new(CodeRefType::Class, cls_id.clone(), alias));
				}
				for f in &d.def_orig.fields {
					let id = format!("{}#{}:{}", cls_id, f.name, f.ty.descriptor());
					if deobf.should_rename(&f.name) {
						let alias = deobf.field_alias(&f.name);
						self.code_data.add_rename(ICodeRename::new(CodeRefType::Field, id, alias));
					}
				}
				for (i, m) in d.def_orig.methods.iter().enumerate() {
					if !deobf.should_rename(&m.name) {
						continue;
					}
					let id = m.full_id(&cls_id);
					let overridden = d.def_orig.methods.iter().enumerate().any(|(j, o)| j != i && o.name == m.name);
					let alias = deobf.method_alias(&m.name, overridden);
					self.code_data.add_rename(ICodeRename::new(CodeRefType::Method, id, alias));
				}
			}
			self.log(&format!("deobfuscation produced {} rename(s)", self.code_data.renames.len()));
		}

		// --- nested classes go into their outer class' file -------------------
		link_nested(&mut defs);

		// --- apply renames and print -----------------------------------------
		let mut out: Vec<JavaClass> = Vec::with_capacity(defs.len());
		let renames: Vec<(CodeRefType, String, String)> = self
			.code_data
			.renames
			.iter()
			.map(|r| (r.ref_type, r.orig_node.clone(), r.new_name.clone()))
			.collect();
		let comments = self.code_data.comments.clone();
		let total_out = defs.iter().filter(|d| !d.parented).count();
		let mut printed = 0usize;
		for mut d in defs {
			if d.parented {
				// already printed as part of its outer class
				continue;
			}
			printed += 1;
			self.report(printed, total_out, &d.def.name.replace('/', "."));
			let class_comments: Vec<String> = comments
				.iter()
				.filter(|c| c.node == d.def_orig.name)
				.map(|c: &ICodeComment| c.comment.clone())
				.collect();
			for t in class_comments {
				d.def.notes.push(t);
			}
			self.code_data.apply_class(&mut d.def);
			d.source = print_class(&d.def, &self.args);
			d.renames = renames.clone();
			out.push(JavaClass::from_inner(Arc::new(d)));
		}
		out.sort_by(|a, b| a.name().cmp(b.name()));
		self.classes = out;
		if !self.errors.is_empty() {
			self.log(&format!("finished with {} problem(s)", self.errors.len()));
		}
		self.inputs = Some(inputs);
		Ok(())
	}

	fn build_one(&self, unit: &CodeUnit) -> Res<ClassInner> {
		let cls = JavaClassFile::parse(&unit.data).map_err(|e| e.in_context(&unit.origin))?;
		let def = build_class_def(&cls, &self.args);
		let mut def_orig = def.clone();
		// the notes stay on the printed model only
		def_orig.notes.clear();
		let disasm = crate::java::disasm::disasm_class(&cls);
		let mut errors: Vec<String> = Vec::new();
		for m in &def.methods {
			for n in &m.notes {
				errors.push(format!("{}: {}", m.name, n));
			}
		}
		for n in &def.notes {
			errors.push(n.clone());
		}
		Ok(ClassInner {
			origin: unit.origin.clone(),
			raw: Some(cls),
			def,
			def_orig,
			args: self.args.clone(),
			source: ClassSource {
				text: String::new(),
				file_name: format!("{}.java", crate::input::short_class_name(&unit.name)),
				package: package_of(&unit.name).to_string(),
				imports: Vec::new(),
			},
			disasm,
			smali: String::new(),
			errors,
			parented: false,
			renames: Vec::new(),
			dex_method_smali: None,
		})
	}

	/// jadx would run the whole DEX pipeline here; this port enumerates the class
	/// and prints its smali, so the caller still gets a complete inventory
	/// (see `docs/PORT_STATUS.md`).
	#[cfg(feature = "dex")]
	fn build_dex_one(&self, unit: &CodeUnit, dex: &crate::dex::DexFile<'_>, index: usize) -> ClassInner {
		let cd = &dex.class_defs()[index];
		let name = cd.class_name();
		let mut def = ClassDef {
			access: cd.access_flags,
			name: name.clone(),
			signature: None,
			superclass: cd.super_class.as_ref().map(|s| JType::class(s.clone())),
			interfaces: cd.interfaces.iter().map(|s| JType::class(s.clone())).collect(),
			fields: cd
				.static_fields
				.iter()
				.chain(cd.instance_fields.iter())
				.map(|f| FieldDef {
					access: f.access_flags,
					name: f.name.clone(),
					ty: f.ty.clone(),
					ty_signature: None,
					const_value: None,
					annotations: Vec::new(),
				})
				.collect(),
			methods: cd
				.direct_methods
				.iter()
				.chain(cd.virtual_methods.iter())
				.map(|m| MethodDef {
					access: m.access_flags,
					name: m.name.clone(),
					params: m
						.proto
						.args
						.iter()
						.enumerate()
						.map(|(i, t)| ParamDef { name: format!("p{}", i), ty: t.clone(), ty_signature: None })
						.collect(),
					ret: m.proto.ret.clone(),
					signature: None,
					throws: Vec::new(),
					body: Vec::new(),
					notes: vec!["dex input: DEX to Java decompilation is not implemented in this port".to_string()],
					default_value: None,
					annotations: Vec::new(),
					local_names: std::collections::HashSet::new(),
				})
				.collect(),
			nested: Vec::new(),
			annotations: Vec::new(),
			record_components: Vec::new(),
			notes: vec![format!("from {}", unit.origin)],
			broken: false,
		};
		def.notes.push(crate::dex::DEX_JAVA_STATUS.to_string());
		let smali = dex.class_smali(index);
		let per_method: Vec<String> = dex.class_methods_smali(index);
		let errors = vec![crate::dex::DEX_JAVA_STATUS.to_string()];
		let mut def_orig = def.clone();
		def_orig.notes.clear();
		ClassInner {
			origin: unit.origin.clone(),
			raw: None,
			def,
			def_orig,
			args: self.args.clone(),
			source: ClassSource {
				text: String::new(),
				file_name: format!("{}.java", crate::input::short_class_name(&name)),
				package: package_of(&name).to_string(),
				imports: Vec::new(),
			},
			disasm: smali.clone(),
			smali,
			errors,
			parented: false,
			renames: Vec::new(),
			dex_method_smali: Some(per_method),
		}
	}

	/// Print every source into `dir`, mirroring `jadx -d dir`. Nested classes land
	/// in their outer class' file, so the number of files is the number of outer
	/// classes. Returns the paths written.
	pub fn save_to_dir(&self, dir: &Path) -> Res<Vec<PathBuf>> {
		let mut written = Vec::new();
		for c in &self.classes {
			written.push(c.save_to_dir(dir)?);
		}
		Ok(written)
	}

	/// A JSON description of the loaded code, the small sibling of jadx's
	/// `OutputFormat::JSON`: name, package, members and the full source text.
	pub fn to_json(&self) -> String {
		let mut out = String::new();
		out.push_str("{\n  \"version\": \"");
		out.push_str(crate::version());
		out.push_str("\",\n  \"classes\": [\n");
		for (i, c) in self.classes().iter().enumerate() {
			out.push_str("    {\n");
			out.push_str(&format!("      \"name\": \"{}\",\n", json_str(&c.full_name())));
			out.push_str(&format!("      \"package\": \"{}\",\n", json_str(&c.package().replace('/', "."))));
			out.push_str(&format!("      \"origin\": \"{}\",\n", json_str(c.origin())));
			out.push_str(&format!("      \"fileName\": \"{}\",\n", json_str(c.file_name())));
			out.push_str("      \"fields\": [");
			let fields: Vec<String> = c
				.fields()
				.iter()
				.map(|f| format!("\"{}: {}\"", json_str(f.name()), json_str(&f.type_name())))
				.collect();
			out.push_str(&fields.join(", "));
			out.push_str("],\n      \"methods\": [");
			let methods: Vec<String> = c
				.methods()
				.iter()
				.map(|m| format!("\"{}{}\"", json_str(m.name()), json_str(m.descriptor())))
				.collect();
			out.push_str(&methods.join(", "));
			out.push_str("],\n");
			out.push_str(&format!("      \"source\": \"{}\"\n", json_str(c.java_source())));
			out.push_str(if i + 1 == self.class_count() { "    }\n" } else { "    },\n" });
		}
		out.push_str("  ]\n}\n");
		out
	}
}

/// jadx: `ClassNode.addNested` -- a class whose outer class was loaded too is
/// printed inside that file instead of getting one of its own.
fn link_nested(defs: &mut [ClassInner]) {
	for i in 0..defs.len() {
		let name = defs[i].def_orig.name.clone();
		let pos = match name.rfind('$') {
			Some(p) => p,
			None => continue,
		};
		let outer = name[..pos].to_string();
		// anonymous and local classes (`Outer$1`) are only linked when the outer
		// class is part of this run, exactly like jadx's `ClassInfo` check
		let j = match defs.iter().position(|d| d.def_orig.name == outer) {
			Some(j) => j,
			None => continue,
		};
		if j == i {
			continue;
		}
		let taken = std::mem::replace(&mut defs[i].def, defs[i].def_orig.clone());
		defs[i].parented = true;
		defs[j].def.nested.push(taken);
	}
}

/// Escape a string as a JSON literal body (without the surrounding quotes).
pub fn json_str(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 8);
	for c in s.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out
}

/// Classify bytes for the FFI (`jadx_classify_bytes`).
pub fn kind_of_bytes(data: &[u8]) -> InputKind {
	classify_bytes(data) }

/// Classify a path for the FFI (`jadx_classify_file`).
pub fn kind_of_path(path: &str) -> InputKind {
	classify(Path::new(path))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testutil::{ClassBuilder, CodeBuilder};

	fn fixture() -> Vec<u8> {
		let mut c = CodeBuilder::new(3, 2);
		c.op(0x1a); // iload_0
		c.op(0x1b); // iload_1
		c.op(0x60); // iadd
		c.op(0xac); // ireturn
		let mut b = ClassBuilder::new("pkg/Calc", "java/lang/Object");
		b.source_file("Calc.java");
		b.field(0x001A, "count", "I");
		b.method(0x0009, "add", "(II)I", Some(c));
		b.to_bytes()
	}

	#[test]
	fn a_class_is_decompiled_through_the_facade() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Calc.class", fixture());
		d.build().unwrap();
		assert_eq!(d.class_count(), 1);
		let c = d.find_class("pkg.Calc").unwrap();
		assert_eq!(c.short_name(), "Calc");
		assert_eq!(c.file_name(), "Calc.java");
		assert_eq!(c.fields().len(), 1);
		assert_eq!(c.methods().len(), 1);
		assert_eq!(c.methods()[0].name(), "add");
		assert_eq!(c.methods()[0].return_type(), "int");
		assert_eq!(c.fields()[0].def_id(), "pkg/Calc#count:I");
		assert_eq!(c.methods()[0].def_id(), "pkg/Calc#add(II)I");
		assert!(c.java_source().contains("package pkg;"), "{}", c.java_source());
		assert!(c.java_source().contains("class Calc"), "{}", c.java_source());
		assert!(c.bytecode_disasm().contains("iadd"), "{}", c.bytecode_disasm());
		assert!(c.errors().is_empty(), "{:?}", c.errors());
		assert!(!c.is_interface());
	}

	#[test]
	fn renames_from_code_data_are_applied() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Calc.class", fixture());
		d.code_data.add_rename(ICodeRename::new(CodeRefType::Method, "pkg/Calc#add(II)I", "plus"));
		d.code_data.add_rename(ICodeRename::new(CodeRefType::Class, "pkg/Calc", "Calc2"));
		d.build().unwrap();
		let src = &d.classes()[0];
		assert_eq!(src.name(), "pkg/Calc2");
		let text = src.java_source();
		assert!(text.contains("plus(int"), "{}", text);
		assert!(!text.contains("add(int"), "{}", text);
	}

	#[test]
	fn a_single_method_can_be_requested() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Calc.class", fixture());
		d.code_data.add_rename(ICodeRename::new(CodeRefType::Method, "pkg/Calc#add(II)I", "plus"));
		d.build().unwrap();
		let cls = &d.classes()[0];
		let m = cls.methods()[0].java_source();
		assert!(m.contains("plus"), "{}", m);
		assert!(m.contains("return"), "{}", m);
		assert!(cls.methods()[0].bytecode_disasm().contains("ireturn"));
	}

	#[test]
	fn deobfuscation_renames_short_names() {
		let mut args = Args::default();
		args.deobfuscation_on = true;
		args.deobfuscation_min_length = 4;
		args.input_blobs.push(("pkg/Calc.class".to_string(), fixture()));
		let mut d = Decompiler::new(args);
		d.build().unwrap();
		let text = d.classes()[0].java_source();
		// `add` is short enough to be renamed, `count` is not
		assert!(text.contains("m0add") || text.contains("m0a"), "{}", text);
		assert!(text.contains("count"), "{}", text);
		assert!(!d.code_data.renames.is_empty());
	}

	#[test]
	fn packages_group_classes() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Calc.class", fixture());
		let mut b = ClassBuilder::new("other/deep/Deep", "java/lang/Object");
		b.method(0x0002, "x", "()V", None);
		d.add_bytes("other/deep/Deep.class", b.to_bytes());
		d.build().unwrap();
		let pkgs = d.packages();
		assert_eq!(pkgs.len(), 2);
		assert_eq!(pkgs[0].name(), "other.deep");
		assert_eq!(pkgs[0].classes().len(), 1);
	}

	#[test]
	fn json_output_is_escaped_and_complete() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Calc.class", fixture());
		d.build().unwrap();
		let j = d.to_json();
		assert!(j.starts_with("{\n  \"version\""), "{}", j);
		assert!(j.contains("\"name\": \"pkg.Calc\""), "{}", j);
		assert!(j.contains("\\n"), "{}", j);
		assert!(j.trim_end().ends_with('}'), "{}", j);
	}

	#[test]
	fn a_broken_class_is_reported_and_the_run_continues() {
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Broken.class", b"not a class file".to_vec());
		d.add_bytes("pkg/Good.class", fixture());
		// `load_inputs` accepted the blob (its extension decided the kind), so the
		// parse failure only shows up as an error entry
		d.build().unwrap();
		assert_eq!(d.class_count(), 1);
		assert_eq!(d.errors().len(), 1, "{:?}", d.errors());
		assert!(d.errors()[0].contains("pkg/Broken.class"), "{:?}", d.errors());
	}

	#[test]
	fn nested_classes_are_printed_in_their_outer_file() {
		let mut outer = ClassBuilder::new("pkg/Outer", "java/lang/Object");
		outer.method(0x0002, "run", "()V", None);
		let mut inner = ClassBuilder::new("pkg/Outer$Inner", "java/lang/Object");
		inner.method(0x0002, "go", "()V", None);
		let mut d = Decompiler::default();
		d.add_bytes("pkg/Outer.class", outer.to_bytes());
		d.add_bytes("pkg/Outer$Inner.class", inner.to_bytes());
		d.build().unwrap();
		assert_eq!(d.class_count(), 1);
		let text = d.classes()[0].java_source();
		assert!(text.contains("class Outer"), "{}", text);
		assert!(text.contains("class Inner"), "{}", text);
		assert!(text.contains("go()"), "{}", text);
	}
}
