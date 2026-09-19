//! Java source printer.
//!
//! The port of `jadx.core.code.writer.JCodeWriter` + `AtomicWriter`: it turns the
//! statement/expression tree from [`super::structure`] into text, adds the
//! `package`/`import` header (jadx collects imports *while* printing the body and
//! writes them in front, which is why the body is rendered into a buffer first),
//! and keeps jadx's layout rules: blank line between members, tab indentation,
//! opening brace on the same line.
//!
//! The input is [`ClassDef`], a small fully-resolved description of one class, so
//! the printer does not depend on the class file reader and can also print a class
//! assembled by a caller (or, later, by the DEX front end).

use std::collections::{BTreeSet, HashSet};

use crate::access_flags::{self, flag, FlagsScope};
use crate::args::Args;
use crate::java::attrs::{Annotation, EncodedValue};
use crate::java::const_pool::Cst;
use crate::java::insn::{BinOp, BitOp, CmpKind, Cond, InvokeKind};
use crate::java::signature;
use crate::types::{name_to_source, package_of, simple_internal_name, JType};

use super::ir::{ends, Expr, NewKind, Stmt};

/// A field, ready to print.
#[derive(Debug, Clone)]
pub struct FieldDef {
	pub access: u32,
	pub name: String,
	pub ty: JType,
	/// generic signature of the field type, when the class has one
	pub ty_signature: Option<String>,
	/// `ConstantValue`, printed as the initializer of a `static final` field
	pub const_value: Option<Cst>,
	pub annotations: Vec<Annotation>,
}

/// A method, ready to print. `body` is empty for abstract and native methods,
/// which are printed with a `;` instead of `{ }`.
#[derive(Debug, Clone)]
pub struct MethodDef {
	pub access: u32,
	/// `<init>`, `<clinit>` or a normal name
	pub name: String,
	pub params: Vec<ParamDef>,
	pub ret: JType,
	/// `Signature` attribute of the method; rendered at print time
	pub signature: Option<String>,
	/// declared `throws` types (from the `Exceptions` attribute)
	pub throws: Vec<JType>,
	pub body: Vec<Stmt>,
	/// warnings from the CFG structurer, printed above the declaration
	pub notes: Vec<String>,
	/// `AnnotationDefault` of an annotation type member
	pub default_value: Option<EncodedValue>,
	pub annotations: Vec<Annotation>,
	/// names of all locals of the method, used to decide when `this.` is needed
	pub local_names: HashSet<String>,
}

impl MethodDef {
	/// The raw JVM descriptor of this method, rebuilt from the parameter and
	/// return types (`(II)I`). Used for rename ids and by the `jadx.api` facade.
	pub fn desc_text(&self) -> String {
		let args: Vec<JType> = self.params.iter().map(|p| p.ty.clone()).collect();
		JType::method_descriptor(&args, &self.ret)
	}

	/// `pkg/C#m(I)V`-style id, given the owning class' internal name.
	pub fn full_id(&self, owner: &str) -> String {
		format!("{}#{}{}", owner, self.name, self.desc_text())
	}
}

/// One method parameter.
#[derive(Debug, Clone)]
pub struct ParamDef {
	pub name: String,
	pub ty: JType,
	pub ty_signature: Option<String>,
}

/// A class, ready to print, with its nested classes attached.
#[derive(Debug, Clone)]
pub struct ClassDef {
	pub access: u32,
	/// internal name with `/` separators, e.g. `pkg/Outer$Inner`
	pub name: String,
	/// class `Signature` attribute
	pub signature: Option<String>,
	pub superclass: Option<JType>,
	pub interfaces: Vec<JType>,
	pub fields: Vec<FieldDef>,
	pub methods: Vec<MethodDef>,
	pub nested: Vec<ClassDef>,
	pub annotations: Vec<Annotation>,
	/// `record` components, when the class is a record
	pub record_components: Vec<(String, JType)>,
	/// free-form notes printed above the declaration
	pub notes: Vec<String>,
	/// `true` when the class could not be read: only `notes` are printed
	pub broken: bool,
}

impl ClassDef {
	pub fn simple_name(&self) -> String {
		simple_internal_name(&self.name).to_string()
	}

	pub fn package(&self) -> &str {
		package_of(&self.name)
	}

	pub fn is_interface(&self) -> bool {
		access_flags::has(self.access, flag::INTERFACE)
	}

	pub fn is_record(&self) -> bool {
		!self.record_components.is_empty()
	}
}

/// The result of printing one class.
#[derive(Debug, Clone)]
pub struct ClassSource {
	/// file name to save it under, without a directory (jadx derives it from the
	/// outermost class: `pkg/Outer$Inner` is written into `Outer.java`)
	pub file_name: String,
	/// source form of the package, `""` for the default package
	pub package: String,
	pub imports: Vec<String>,
	pub text: String,
}

/// Print one class (and everything nested in it) as Java source.
pub fn print_class(def: &ClassDef, args: &Args) -> ClassSource {
	let mut w = W {
		args,
		imports: BTreeSet::new(),
		pkg: def.package().to_string(),
		enclosing: def.name.clone(),
		self_names: self_name_set(&def.name),
		shadowed: HashSet::new(),
		out: String::new(),
		indent: 0,
	};
	w.class(def, false);
	let imports: Vec<String> = w.imports.into_iter().collect();
	let mut text = String::new();
	if !def.package().is_empty() {
		text.push_str(&format!("package {};\n\n", def.package().replace('/', ".")));
	} else {
		text.push_str("// declared in the default package, where imports cannot be used\n\n");
	}
	if args.use_imports && !imports.is_empty() {
		for i in &imports {
			text.push_str(&format!("import {};\n", i));
		}
		text.push('\n');
	}
	for n in &def.notes {
		text.push_str(&format!("/* JADX: {} */\n", n));
	}
	text.push_str(&w.out);
	ClassSource { file_name: file_name_for(def), package: def.package().replace('/', "."), imports, text }
}

/// Print a single method declaration and body, without the class wrapper. Used by
/// `jadx_method_get_java_source` and by `--single-method` style tooling.
pub fn method_source(m: &MethodDef, owner: &ClassDef, args: &Args) -> String {
	let mut w = W {
		args,
		imports: BTreeSet::new(),
		pkg: owner.package().to_string(),
		enclosing: owner.name.clone(),
		self_names: self_name_set(&owner.name),
		shadowed: HashSet::new(),
		out: String::new(),
		indent: 0,
	};
	w.method(m, owner);
	w.out
}

fn file_name_for(def: &ClassDef) -> String {
	let simple = simple_internal_name(&def.name);
	let outer = simple.split('$').next().unwrap_or(simple);
	format!("{}.java", outer)
}

/// Every prefix of `Outer$Inner$Deep` (`Outer`, `Outer.Inner`, ...), so that a
/// reference to the class being printed never produces an import.
fn self_name_set(name: &str) -> HashSet<String> {
	let mut out = HashSet::new();
	let simple = simple_internal_name(name);
	let mut acc = String::new();
	for part in simple.split('$') {
		if !acc.is_empty() {
			acc.push('.');
		}
		acc.push_str(part);
		out.insert(acc.clone());
	}
	out
}

/// Java operator precedence, low to high; used to decide where parentheses are
/// needed (jadx: `JInsn.getInvokeLimitMbi`/`printExprWithParentheses`).
mod prec {
	pub const ASSIGN: u8 = 1;
	pub const TERNARY: u8 = 2;
	pub const OR: u8 = 3;
	pub const AND: u8 = 4;
	pub const BIT_OR: u8 = 5;
	pub const BIT_XOR: u8 = 6;
	pub const BIT_AND: u8 = 7;
	pub const EQ: u8 = 8;
	pub const REL: u8 = 9;
	pub const SHIFT: u8 = 10;
	pub const ADD: u8 = 11;
	pub const MUL: u8 = 12;
	pub const UNARY: u8 = 13;
	pub const POSTFIX: u8 = 14;
}

struct W<'a> {
	args: &'a Args,
	imports: BTreeSet<String>,
	pkg: String,
	enclosing: String,
	self_names: HashSet<String>,
	shadowed: HashSet<String>,
	out: String,
	indent: usize,
}

impl<'a> W<'a> {
	// --- low level output ---------------------------------------------------

	fn ind(&mut self) {
		for _ in 0..self.indent {
			self.out.push('\t');
		}
	}

	fn line(&mut self, s: &str) {
		self.ind();
		self.out.push_str(s);
		self.out.push('\n');
	}

	fn blank(&mut self) {
		if !self.out.is_empty() && !self.out.ends_with("\n\n") {
			self.out.push('\n');
		}
	}

	fn close_brace(&mut self, suffix: &str) {
		self.indent -= 1;
		self.ind();
		self.out.push('}');
		self.out.push_str(suffix);
		self.out.push('\n');
	}

	// --- names --------------------------------------------------------------

	/// Resolve an internal class name to source text, registering the import.
	/// `pkg/Outer$Inner` prints as `Outer.Inner` with `pkg.Outer` imported, which
	/// is what jadx does (`ImportsControl.addImport`).
	fn class_name(&mut self, internal: &str) -> String {
		let simple = simple_internal_name(internal);
		let pkg = package_of(internal);
		let printed = name_to_source(simple, false);
		if pkg.is_empty() || pkg == self.pkg || pkg == "java/lang" || self.self_names.contains(&printed) {
			return printed;
		}
		if self.args.use_imports {
			let outer = simple.split('$').next().unwrap_or(simple);
			self.imports.insert(format!("{}.{}", pkg.replace('/', "."), outer));
		}
		printed
	}

	fn ty(&mut self, t: &JType) -> String {
		match t {
			JType::Void => "void".to_string(),
			JType::Boolean => "boolean".to_string(),
			JType::Char => "char".to_string(),
			JType::Byte => "byte".to_string(),
			JType::Short => "short".to_string(),
			JType::Int => "int".to_string(),
			JType::Long => "long".to_string(),
			JType::Float => "float".to_string(),
			JType::Double => "double".to_string(),
			JType::Class(c) => self.class_name(c),
			JType::Array(el) => {
				let s = self.ty(el);
				format!("{}[]", s)
			}
			JType::TypeVar(v) => v.clone(),
		}
	}

	// --- class and members --------------------------------------------------

	fn class(&mut self, def: &ClassDef, nested: bool) {
		if def.broken {
			for n in &def.notes {
				self.line(&format!("/* {} */", n));
			}
			return;
		}
		self.annotations(&def.annotations);
		let (mods, extra) = self.modifiers(def.access, FlagsScope::Class);
		let kind = if def.is_interface() {
			if access_flags::has(def.access, flag::ANNOTATION) {
				"@interface"
			} else {
				"interface"
			}
		} else if access_flags::has(def.access, flag::ENUM) {
			"enum"
		} else if def.is_record() {
			"record"
		} else {
			"class"
		};
		let sig = match def.signature.as_deref() {
			Some(s) => signature::parse(s, &mut |n| self.class_name(n)),
			None => None,
		};
		// a nested class is declared with its last name only, like jadx
		let name = if nested {
			def.name.split('$').next_back().unwrap_or("unknown").to_string()
		} else {
			self.class_name(&def.name)
		};
		let mut head = format!("{}{} {}", mods, kind, name);
		if let Some(ref s) = sig {
			if !s.type_params.is_empty() {
				head.push_str(&format!("<{}>", s.type_params.join(", ")));
			}
		}
		let mut mods = mods;
		if kind == "interface" || kind == "@interface" {
			mods = mods.replace("abstract ", "");
		}
		if def.is_record() {
			let mut parts: Vec<String> = Vec::new();
			for (n, t) in &def.record_components {
				let ty = self.ty(t);
				parts.push(format!("{} {}", ty, n));
			}
			head.push_str(&format!("({}) {{", parts.join(", ")));
		} else {
			// `extends`: the generic signature wins over the erased supertype
			if kind == "class" || kind == "enum" {
				let from_sig = sig.as_ref().and_then(|s| s.extends.clone());
				let sup = match from_sig {
					Some(t) => Some(t),
					None => match &def.superclass {
						Some(t) if *t != JType::class("java/lang/Object") => Some(self.ty(t)),
						_ => None,
					},
				};
				if let Some(sup) = sup {
					head.push_str(&format!(" extends {}", sup));
				}
			}
			let mut impls: Vec<String> = Vec::new();
			if let Some(s) = sig.as_ref() {
				impls = s.implements.clone();
			}
			if impls.is_empty() {
				for i in &def.interfaces {
					impls.push(self.ty(i));
				}
			}
			if !impls.is_empty() {
				let words = if kind == "interface" { "extends" } else { "implements" };
				head.push_str(&format!(" {} {}", words, impls.join(", ")));
			}
			head.push_str(" {");
		}
		if !extra.is_empty() {
			head.push_str(&format!(" /* {} */", extra.join(", ")));
		}
		self.ind();
		self.out.push_str(head.trim_start());
		self.out.push('\n');
		self.indent += 1;
		let mut first = true;
		for f in &def.fields {
			if !first {
				self.blank();
			}
			first = false;
			self.field(f);
		}
		for m in &def.methods {
			if !first {
				self.blank();
			}
			first = false;
			self.method(m, def);
		}
		for n in &def.nested {
			if !first {
				self.blank();
			}
			first = false;
			self.class(n, true);
		}
		self.close_brace("");
	}

	/// Split the access flag text into Java modifiers and the flags that only
	/// exist in bytecode (`synthetic`, `bridge`, ...), which jadx prints as a
	/// comment so the source still compiles.
	fn modifiers(&mut self, access: u32, scope: FlagsScope) -> (String, Vec<String>) {
		let mut mods = String::new();
		let mut extra: Vec<String> = Vec::new();
		for part in access_flags::format(access, scope).split(' ').filter(|p| !p.is_empty()) {
			match part {
				"public" | "protected" | "private" | "static" | "final" | "abstract" | "native" | "synchronized" | "volatile" | "transient" | "default" => {
					mods.push_str(part);
					mods.push(' ');
				}
				other => extra.push(other.to_string()),
			}
		}
		(mods, extra)
	}

	fn annotations(&mut self, list: &[Annotation]) {
		for a in list {
			let ty = self.class_name(&a.class_name);
			let simple = match ty.rfind('.') {
				Some(i) => &ty[i + 1..],
				None => &ty[..],
			};
			if a.values.is_empty() {
				self.line(&format!("@{}", simple));
				continue;
			}
			let only_value = a.values.len() == 1 && a.values[0].0 == "value";
			let mut parts: Vec<String> = Vec::new();
			for (k, v) in &a.values {
				let txt = encode_value_text(v, self.args);
				if only_value {
					parts.push(txt);
				} else {
					parts.push(format!("{} = {}", k, txt));
				}
			}
			self.line(&format!("@{}({})", simple, parts.join(", ")));
		}
	}

	fn field(&mut self, f: &FieldDef) {
		self.annotations(&f.annotations);
		let (mods, extra) = self.modifiers(f.access, FlagsScope::Field);
		let ty = match &f.ty_signature {
			Some(s) => self.ty_from_signature(s, &f.ty),
			None => self.ty(&f.ty),
		};
		let mut line = format!("{}{} {}", mods, ty, f.name);
		if let Some(c) = &f.const_value {
			line.push_str(&format!(" = {}", c.literal(self.args.print_hex_ints(), self.args.escape_unicode)));
		}
		line.push(';');
		if !extra.is_empty() {
			line.push_str(&format!(" /* {} */", extra.join(", ")));
		}
		self.line(line.trim_start());
	}

	/// A generic field/parameter signature, falling back to the erased type when
	/// the signature is malformed (jadx: `GenericTypeFormatter`).
	fn ty_from_signature(&mut self, sig: &str, fallback: &JType) -> String {
		match signature::field_type_to_source(sig, &mut |n| n.replace('/', ".").replace('$', '.')) {
			Some(t) => t,
			None => self.ty(fallback),
		}
	}

	fn method(&mut self, m: &MethodDef, owner: &ClassDef) {
		for n in &m.notes {
			self.line(&format!("/* JADX: {} */", n));
		}
		self.annotations(&m.annotations);
		let (mods, extra) = self.modifiers(m.access, FlagsScope::Method);
		let is_ctor = m.name == "<init>";
		let is_clinit = m.name == "<clinit>";
		let sig = match m.signature.as_deref() {
			Some(s) => signature::parse(s, &mut |n| n.replace('/', ".").replace('$', '.')),
			None => None,
		};
		let name = if is_clinit {
			String::new()
		} else if is_ctor {
			simple_internal_name(&owner.name).split('$').next_back().unwrap_or("unknown").to_string()
		} else {
			m.name.clone()
		};
		let bodyless = m.body.is_empty()
			&& (access_flags::has(m.access, flag::ABSTRACT)
				|| access_flags::has(m.access, flag::NATIVE)
				|| m.default_value.is_some()
				|| (owner.is_interface() && !access_flags::has(m.access, flag::STATIC) && !is_ctor && !is_clinit));
		let mut head = String::new();
		if is_clinit {
			head.push_str("static");
		} else {
			if let Some(s) = sig.as_ref() {
				if !s.type_params.is_empty() {
					head.push_str(&format!("<{}> ", s.type_params.join(", ")));
				}
			}
			head.push_str(&mods);
			if !is_ctor {
				let ret = match sig.as_ref().and_then(|s| s.ret.clone()) {
					Some(r) => r,
					None => self.ty(&m.ret),
				};
				head.push_str(&format!("{} ", ret));
			}
			head.push_str(&name);
			head.push('(');
			let mut parts: Vec<String> = Vec::new();
			for (i, p) in m.params.iter().enumerate() {
				let ty = match sig.as_ref().and_then(|s| s.args.get(i)) {
					Some(t) => t.clone(),
					None => match &p.ty_signature {
						Some(sg) => signature::field_type_to_source(sg, &mut |n| n.replace('/', ".").replace('$', '.')).unwrap_or_else(|| self.ty(&p.ty)),
						None => self.ty(&p.ty),
					},
				};
				parts.push(format!("{} {}", ty, p.name));
			}
			head.push_str(&parts.join(", "));
			head.push(')');
			let mut throws: Vec<String> = match sig.as_ref() {
				Some(s) if !s.throws.is_empty() => s.throws.clone(),
				_ => Vec::new(),
			};
			if throws.is_empty() {
				for t in &m.throws {
					throws.push(self.ty(t));
				}
			}
			if !throws.is_empty() {
				head.push_str(&format!(" throws {}", throws.join(", ")));
			}
		}
		if !extra.is_empty() {
			head.push_str(&format!(" /* {} */", extra.join(", ")));
		}
		if let Some(dv) = &m.default_value {
			let txt = encode_value_text(dv, self.args);
			let mut l = head.trim_start().to_string();
			l.push_str(&format!(" default {};", txt));
			self.line(&l);
			return;
		}
		if bodyless {
			let mut l = head.trim_start().to_string();
			l.push(';');
			self.line(&l);
			return;
		}
		self.ind();
		self.out.push_str(head.trim_start());
		self.out.push_str(" {\n");
		self.indent += 1;
		self.shadowed = m.local_names.clone();
		self.stmts(&m.body);
		self.shadowed = HashSet::new();
		self.close_brace("");
	}

	// --- statements ---------------------------------------------------------

	fn stmts(&mut self, list: &[Stmt]) {
		for s in list {
			self.stmt(s);
		}
	}

	fn stmt(&mut self, s: &Stmt) {
		match s {
			Stmt::LocalDef { name, ty, init } => {
				let t = self.ty(ty);
				let line = match init {
					Some(e) => format!("{} {} = {};", t, name, self.expr(e, prec::ASSIGN + 1)),
					None => format!("{} {};", t, name),
				};
				self.line(&line);
			}
			Stmt::Assign { target, value } => {
				let l = self.expr(target, prec::POSTFIX);
				let r = self.expr(value, prec::ASSIGN);
				self.line(&format!("{} = {};", l, r));
			}
			Stmt::ExprStmt { expr } => {
				let t = self.expr(expr, prec::ASSIGN);
				self.line(&format!("{};", t));
			}
			Stmt::If { cond, then_body, else_body, has_else } => {
				let c = self.expr(cond, prec::ASSIGN);
				self.line(&format!("if ({}) {{", c));
				self.indent += 1;
				self.stmts(then_body);
				self.indent -= 1;
				if *has_else {
					self.ind();
					self.out.push_str("} else {\n");
					self.indent += 1;
					self.stmts(else_body);
					self.indent -= 1;
					self.line("}");
				} else {
					self.line("}");
				}
			}
			Stmt::While { cond, body, do_while, label } => {
				let head = match label {
					u32::MAX => String::new(),
					id => format!("LBL_{}: ", id),
				};
				match (cond, do_while) {
					(Some(c), false) => {
						let t = self.expr(c, prec::ASSIGN);
						self.line(&format!("{}while ({}) {{", head, t));
						self.indent += 1;
						self.stmts(body);
						self.indent -= 1;
						self.line("}");
					}
					(Some(c), true) => {
						self.line(&format!("{}do {{", head));
						self.indent += 1;
						self.stmts(body);
						self.indent -= 1;
						let t = self.expr(c, prec::ASSIGN);
						self.close_brace(&format!(" while ({});", t));
					}
					(None, _) => {
						self.line(&format!("{}while (true) {{", head));
						self.indent += 1;
						self.stmts(body);
						self.indent -= 1;
						self.line("}");
					}
				}
			}
			Stmt::Switch { subject, cases, has_default, default_body } => {
				self.line(&format!("switch ({}) {{", self.expr(subject, prec::ASSIGN)));
				self.indent += 1;
				if *has_default {
					self.line("default:");
					self.indent += 1;
					self.stmts(default_body);
					self.indent -= 1;
				}
				for c in cases {
					for k in &c.keys {
						self.line(&format!("case {}:", k));
					}
					self.indent += 1;
					self.stmts(&c.body);
					if !c.falls_through && !ends(&c.body) {
						self.line("break;");
					}
					self.indent -= 1;
				}
				self.indent -= 1;
				self.line("}");
			}
			Stmt::Return { value } => {
				let l = match value {
					Some(v) => format!("return {};", self.expr(v, prec::ASSIGN)),
					None => "return;".to_string(),
				};
				self.line(&l);
			}
			Stmt::Throw { value } => {
				self.line(&format!("throw {};", self.expr(value, prec::ASSIGN)));
			}
			Stmt::TryCatch { try_body, catches, finally_body } => {
				self.line("try {");
				self.indent += 1;
				self.stmts(try_body);
				self.indent -= 1;
				for c in catches {
					let ty = match &c.ty {
						Some(t) => self.ty(t),
						None => "Throwable".to_string(),
					};
					self.ind();
					self.out.push_str(&format!("}} catch ({} {}) {{\n", ty, c.var_name));
					self.indent += 1;
					self.stmts(&c.body);
					self.indent -= 1;
				}
				match finally_body {
					Some(f) => {
						self.ind();
						self.out.push_str("} finally {\n");
						self.indent += 1;
						self.stmts(f);
						self.indent -= 1;
						self.line("}");
					}
					None => self.line("}"),
				}
			}
			Stmt::Label { id } => self.line(&format!("LBL_{}:", id)),
			Stmt::Goto { id } => self.line(&format!("goto LBL_{};", id)),
			Stmt::Break { id } => {
				self.line(match id {
					Some(i) => &format!("break LBL_{};", i),
					None => "break;",
				});
			}
			Stmt::Continue { id } => {
				self.line(match id {
					Some(i) => &format!("continue LBL_{};", i),
					None => "continue;",
				});
			}
			Stmt::Comment { text } => {
				for l in text.split('\n') {
					self.line(&format!("// {}", l.replace("*/", "* /").replace("/*", "/ *")));
				}
			}
			Stmt::LineNumber { line } => self.line(&format!("// [line: {}]", line)),
		}
	}

	// --- expressions --------------------------------------------------------

	fn expr(&mut self, e: &Expr, min: u8) -> String {
		let (text, p) = self.expr_raw(e);
		if p < min {
			format!("({})", text)
		} else {
			text
		}
	}

	/// `(text, precedence)`; the caller wraps in parentheses when needed.
	fn expr_raw(&mut self, e: &Expr) -> (String, u8) {
		match e {
			Expr::Const(c) => (c.literal(self.args.print_hex_ints(), self.args.escape_unicode), prec::POSTFIX),
			Expr::Raw { text, .. } => (text.clone(), prec::POSTFIX),
			Expr::Local { name, .. } => (name.clone(), prec::POSTFIX),
			Expr::Field { obj, owner, name, is_static, .. } => {
				if *is_static {
					// a static field of another class has to be qualified, as in
					// `Other.MAX`, unless it is a field of this very class
					if !owner.is_empty() && owner != &self.enclosing {
						let o = self.class_name(owner);
						return (format!("{}.{}", o, name), prec::POSTFIX);
					}
					return (name.clone(), prec::POSTFIX);
				}
				match obj.as_deref() {
					Some(o) => {
						let base = self.expr(o, prec::POSTFIX);
						if base == "this" {
							// jadx prints `x`, adding `this.` only when a local or a
							// parameter shadows the field
							if self.shadowed.contains(name) {
								(format!("this.{}", name), prec::POSTFIX)
							} else {
								(format!("this.{}", name), prec::POSTFIX)
							}
						} else {
							(format!("{}.{}", base, name), prec::POSTFIX)
						}
					}
					None => (name.clone(), prec::POSTFIX),
				}
			}
			Expr::Array { arr, index, .. } => {
				let a = self.expr(arr, self.atom_prec(arr));
				let i = self.expr(index, prec::ASSIGN);
				(format!("{}[{}]", a, i), prec::POSTFIX)
			}
			Expr::Length { a } => {
				let base = self.expr(a, self.atom_prec(a));
				(format!("{}.length", base), prec::POSTFIX)
			}
			Expr::Invoke { kind, obj, args, target, .. } => {
				let mut prefix = String::new();
				match obj.as_deref() {
					Some(o) => {
						let base = self.expr(o, self.atom_prec(o));
						if base != "this" {
							prefix = format!("{}.", base);
						}
					}
					None => {
						if *kind == InvokeKind::Static && self.needs_owner(target) {
							let owner = self.class_name(&target.class);
							prefix = format!("{}.", owner);
						}
					}
				}
				if *kind == InvokeKind::Dynamic {
					// no receiver and no owner in an `invokedynamic`; jadx prints the
					// bootstrap method in a comment for such call sites
					let inner = self.args_text(args);
					return (format!("/* invokedynamic */ {}({})", target.name, inner), prec::POSTFIX);
				}
				let inner = self.args_text(args);
				(format!("{}{}({})", prefix, target.name, inner), prec::POSTFIX)
			}
			Expr::New { ty, args, dims, kind, .. } => match kind {
				NewKind::Super => (format!("super({})", self.args_text(args)), prec::POSTFIX),
				NewKind::This => (format!("this({})", self.args_text(args)), prec::POSTFIX),
				NewKind::Array if !dims.is_empty() => {
					// `new int[n][m]`: the element type of the outermost array, one
					// bracket per dimension expression, then the empty ones
					let base = self.array_base(ty, dims.len());
					let mut out = format!("new {}", base);
					for d in dims.iter() {
						let t = self.expr(d, prec::ASSIGN);
						out.push_str(&format!("[{}]", t));
					}
					for _ in 0..ty.array_dimensions().saturating_sub(dims.len()) {
						out.push_str("[]");
					}
					(out, prec::POSTFIX)
				}
				NewKind::Array => {
					let base = self.ty(ty);
					(format!("new {}[]", base), prec::POSTFIX)
				}
				NewKind::Class => {
					let base = self.ty(ty);
					(format!("new {}({})", base, self.args_text(args)), prec::POSTFIX)
				}
			},
			Expr::Bin { op, a, b, .. } => {
				let p = match op {
					BinOp::Mul | BinOp::Div | BinOp::Rem => prec::MUL,
					BinOp::Add | BinOp::Sub => prec::ADD,
				};
				let lt = self.expr(a, p);
				let rt = self.expr(b, p + 1);
				(format!("{} {} {}", lt, op.java_op(), rt), p)
			}
			Expr::Shift { op, a, b, .. } => {
				let lt = self.expr(a, prec::SHIFT);
				let rt = self.expr(b, prec::SHIFT + 1);
				(format!("{} {} {}", lt, op.java_op(), rt), prec::SHIFT)
			}
			Expr::Bit { op, a, b, .. } => {
				let p = match op {
					BitOp::And => prec::BIT_AND,
					BitOp::Or => prec::BIT_OR,
					BitOp::Xor => prec::BIT_XOR,
				};
				let lt = self.expr(a, p);
				let rt = self.expr(b, p + 1);
				(format!("{} {} {}", lt, op.java_op(), rt), p)
			}
			Expr::Cmp { op, a, b } => {
				let p = match op {
					Cond::Eq | Cond::Ne => prec::EQ,
					_ => prec::REL,
				};
				let lt = self.expr(a, p);
				let rt = self.expr(b, p + 1);
				(format!("{} {} {}", lt, op.java_op(), rt), p)
			}
			Expr::Cmp3 { kind, a, b } => {
				// `lcmp`/`fcmpl`/`dcmpg` have no Java operator; the comparison
				// helpers that produce -1/0/1 are used instead, as jadx does for
				// Dalvik `LCMP`
				let f = match kind {
					CmpKind::Long => "Long.compare",
					CmpKind::Fmpl | CmpKind::Fmpg => "Float.compare",
					CmpKind::Dmpl | CmpKind::Dmpg => "Double.compare",
				};
				let av = self.expr(a, prec::ASSIGN);
				let bv = self.expr(b, prec::ASSIGN);
				(format!("{}({}, {})", f, av, bv), prec::POSTFIX)
			}
			Expr::Not { a } => {
				let t = self.expr(a, prec::UNARY);
				(format!("!{}", t), prec::UNARY)
			}
			Expr::Neg { a, .. } => {
				let t = self.expr(a, prec::UNARY);
				(format!("-{}", t), prec::UNARY)
			}
			Expr::Cast { to, a } => {
				let t = self.ty(to);
				let inner = self.expr(a, prec::UNARY);
				(format!("({}) {}", t, inner), prec::UNARY)
			}
			Expr::InstanceOf { a, ty } => {
				let t = self.ty(ty);
				let v = self.expr(a, prec::REL);
				(format!("{} instanceof {}", v, t), prec::REL)
			}
			Expr::Inc { name, delta, pre, .. } => {
				if *delta == 1 {
					if *pre {
						(format!("++{}", name), prec::UNARY)
					} else {
						(format!("{}++", name), prec::POSTFIX)
					}
				} else if *delta == -1 {
					if *pre {
						(format!("--{}", name), prec::UNARY)
					} else {
						(format!("{}--", name), prec::POSTFIX)
					}
				} else {
					(format!("{} += {}", name, delta), prec::ASSIGN)
				}
			}
			Expr::Ternary { cond, a, b, .. } => {
				let c = self.expr(cond, prec::TERNARY + 1);
				let l = self.expr(a, prec::ASSIGN);
				let r = self.expr(b, prec::ASSIGN);
				(format!("{} ? {} : {}", c, l, r), prec::TERNARY)
			}
		}
	}

	fn args_text(&mut self, args: &[Expr]) -> String {
		let mut parts: Vec<String> = Vec::with_capacity(args.len());
		for a in args {
			parts.push(self.expr(a, prec::ASSIGN));
		}
		parts.join(", ")
	}

	/// Precedence the operand of a postfix operator (`a.x`, `a[i]`, `a.length`)
	/// needs: only a cast or a binary expression has to be parenthesised.
	fn atom_prec(&self, e: &Expr) -> u8 {
		if self.is_atom(e) {
			prec::POSTFIX
		} else {
			prec::MUL
		}
	}

	fn is_atom(&self, e: &Expr) -> bool {
		matches!(
			e,
			Expr::Local { .. }
				| Expr::Const(_)
				| Expr::Field { .. }
				| Expr::Array { .. }
				| Expr::Invoke { .. }
				| Expr::Length { .. }
				| Expr::New { .. }
				| Expr::Inc { .. }
		)
	}

	/// `true` when a static call has to be qualified with its owner class, i.e.
	/// the owner is not the class being printed.
	fn needs_owner(&self, target: &crate::java::const_pool::RefInfo) -> bool {
		!target.class.is_empty() && target.class != self.enclosing
	}

	/// The base type of `new T[d0][d1]`, `ty` being the full array type.
	fn array_base(&mut self, ty: &JType, dims: usize) -> String {
		let mut cur = ty.clone();
		for _ in 0..dims {
			match cur {
				JType::Array(el) => cur = *el,
				other => {
					cur = other;
					break;
				}
			}
		}
		self.ty(&cur)
	}
}

/// Render an annotation or `ConstantValue` payload. Arrays and nested
/// annotations use the Java initialiser syntax (jadx: `AnnotationUtils`).
pub fn encode_value_text(v: &EncodedValue, args: &Args) -> String {
	match v {
		EncodedValue::Bool(b) => b.to_string(),
		EncodedValue::Byte(i) => i.to_string(),
		EncodedValue::Char(c) => crate::java::const_pool::char_literal(*c, args.escape_unicode),
		EncodedValue::Short(i) => i.to_string(),
		EncodedValue::Int(i) => crate::java::const_pool::format_int(*i, args.print_hex_ints()),
		EncodedValue::Long(i) => crate::java::const_pool::format_long(*i, args.print_hex_ints()),
		EncodedValue::Float(f) => crate::java::const_pool::format_float(*f, args.print_hex_ints()),
		EncodedValue::Double(d) => crate::java::const_pool::format_double(*d, args.print_hex_ints()),
		EncodedValue::Str(s) => crate::java::const_pool::string_literal(s, args.escape_unicode),
		EncodedValue::Class(t) => match t {
			JType::Class(c) => format!("{}.class", name_to_source(c, true)),
			other => format!("{}.class", other.qualified_name()),
		},
		EncodedValue::Enum { class, name } => format!("{}.{}", name_to_source(class, false), name),
		EncodedValue::Array(items) => {
			if items.is_empty() {
				"{}".to_string()
			} else {
				format!("{{{}}}", items.iter().map(|i| encode_value_text(i, args)).collect::<Vec<_>>().join(", "))
			}
		}
		EncodedValue::Nested(a) => {
			let mut parts: Vec<String> = Vec::new();
			for (k, val) in &a.values {
				parts.push(format!("{} = {}", k, encode_value_text(val, args)));
			}
			format!("@{}({})", name_to_source(&a.class_name, false), parts.join(", "))
		}
		EncodedValue::MethodType(desc) => format!("/* method type {} */", desc),
		EncodedValue::MethodHandle(h) => format!("/* method handle {} {} */", h.kind.as_str(), h.target.full_name()),
	}
}
