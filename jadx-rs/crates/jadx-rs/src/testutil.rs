//! A tiny class-file assembler used by the test-suite and by anyone who wants to
//! synthesise bytecode without a JDK.
//!
//! This exists because the port must be testable in environments that have no
//! `javac`: every parser and decompiler test builds its fixture here, so the byte
//! layout is explicit and reviewed. It writes the subset of JVMS chapter 4 that
//! the readers in this crate support: constant pool, `Code` (with exception
//! table, `LineNumberTable`, `LocalVariableTable`, `StackMapTable`),
//! `ConstantValue`, `Exceptions`, `Signature`, `SourceFile` and
//! `RuntimeVisibleAnnotations`.
//!
//! Gated behind the `fixtures` feature (`cargo test` turns it on automatically).

#![allow(dead_code)]

use crate::java::class_file::MAGIC;

/// Attribute names the builder reserves at fixed constant pool indices 1..=6,
/// which keeps hand-written attribute references readable in tests.
const ATTR_NAMES: [&str; 7] = [
	"Code",
	"ConstantValue",
	"Exceptions",
	"LineNumberTable",
	"LocalVariableTable",
	"StackMapTable",
	"SourceFile",
];

pub const ATTR_CODE: u16 = 1;
pub const ATTR_CONSTANT_VALUE: u16 = 2;
pub const ATTR_EXCEPTIONS: u16 = 3;
pub const ATTR_LINE_NUMBER_TABLE: u16 = 4;
pub const ATTR_LOCAL_VARIABLE_TABLE: u16 = 5;
pub const ATTR_STACK_MAP_TABLE: u16 = 6;
pub const ATTR_SOURCE_FILE: u16 = 7;

/// A forward-referenced branch target inside a [`CodeBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label {
	id: usize,
}

#[derive(Debug, Clone, Copy)]
struct Patch {
	/// offset of the branch operand inside the code array
	at: usize,
	/// offset of the opcode itself (branch offsets are relative to this)
	base: usize,
	wide: bool,
	label: usize,
}

/// Builds the body of one `Code` attribute.
#[derive(Debug)]
pub struct CodeBuilder {
	pub max_stack: u16,
	pub max_locals: u16,
	insns: Vec<u8>,
	labels: Vec<Option<usize>>,
	patches: Vec<Patch>,
	handlers: Vec<Handler>,
	line_numbers: Vec<(u32, u32)>,
	locals: Vec<LocalVar>,
	stack_map: Option<(u16, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy)]
pub struct Handler {
	pub start: u32,
	pub end: u32,
	pub handler: u32,
	/// constant pool index of the caught class; 0 means catch-all (`finally`)
	pub catch_type: u16,
}

#[derive(Debug, Clone)]
pub struct LocalVar {
	pub start: u32,
	pub length: u32,
	pub name: u16,
	pub desc: u16,
	pub index: u16,
}

impl Default for CodeBuilder {
	fn default() -> Self {
		CodeBuilder::new(8, 8)
	}
}

impl CodeBuilder {
	pub fn new(max_stack: u16, max_locals: u16) -> CodeBuilder {
		CodeBuilder {
			max_stack,
			max_locals,
			insns: Vec::new(),
			labels: Vec::new(),
			patches: Vec::new(),
			handlers: Vec::new(),
			line_numbers: Vec::new(),
			locals: Vec::new(),
			stack_map: None,
		}
	}

	/// current offset inside the code array
	pub fn pc(&self) -> usize {
		self.insns.len()
	}

	pub fn new_label(&mut self) -> Label {
		let id = self.labels.len();
		self.labels.push(None);
		Label { id }
	}

	/// bind `l` to the current offset
	pub fn mark(&mut self, l: Label) {
		self.labels[l.id] = Some(self.pc());
	}

	pub fn op(&mut self, opcode: u8) {
		self.insns.push(opcode);
	}

	pub fn op_u1(&mut self, opcode: u8, v: u8) {
		self.insns.push(opcode);
		self.insns.push(v);
	}

	pub fn op_i1(&mut self, opcode: u8, v: i8) {
		self.op_u1(opcode, v as u8);
	}

	pub fn op_u2(&mut self, opcode: u8, v: u16) {
		self.insns.push(opcode);
		self.insns.extend_from_slice(&v.to_be_bytes());
	}

	pub fn op_i2(&mut self, opcode: u8, v: i16) {
		self.insns.push(opcode);
		self.insns.extend_from_slice(&v.to_be_bytes());
	}

	pub fn op_u4(&mut self, opcode: u8, v: u32) {
		self.insns.push(opcode);
		self.insns.extend_from_slice(&v.to_be_bytes());
	}

	/// `iinc index, delta`
	pub fn iinc(&mut self, index: u8, delta: i8) {
		self.op_u1(0x84, index);
		self.insns.push(delta as u8);
	}

	/// `wide iinc index, delta`
	pub fn wide_iinc(&mut self, index: u16, delta: i16) {
		self.insns.push(0xc4);
		self.insns.push(0x84);
		self.insns.extend_from_slice(&index.to_be_bytes());
		self.insns.extend_from_slice(&delta.to_be_bytes());
	}

	/// `invokeinterface`/`invokedynamic`: u2 index, u1 count, u1 zero
	pub fn op_u2_u1_u1(&mut self, opcode: u8, index: u16, a: u8, b: u8) {
		self.insns.push(opcode);
		self.insns.extend_from_slice(&index.to_be_bytes());
		self.insns.push(a);
		self.insns.push(b);
	}

	/// `multianewarray idx, dims`
	pub fn multianewarray(&mut self, index: u16, dims: u8) {
		self.insns.push(0xc5);
		self.insns.extend_from_slice(&index.to_be_bytes());
		self.insns.push(dims);
	}

	/// branch with a `s2` relative operand
	pub fn branch(&mut self, opcode: u8, label: Label) {
		let base = self.pc();
		self.insns.push(opcode);
		let at = self.pc();
		self.insns.extend_from_slice(&0i16.to_be_bytes());
		self.patches.push(Patch { at, base, wide: false, label: label.id });
	}

	/// branch with an `s4` relative operand (`goto_w`, and switch operands)
	pub fn branch_wide(&mut self, opcode: u8, label: Label) {
		let base = self.pc();
		self.insns.push(opcode);
		let at = self.pc();
		self.insns.extend_from_slice(&0i32.to_be_bytes());
		self.patches.push(Patch { at, base, wide: true, label: label.id });
	}

	/// `tableswitch low..high` with a default and one target per key
	pub fn table_switch(&mut self, low: i32, high: i32, default: Label, targets: Vec<Label>) {
		let base = self.pc();
		self.insns.push(0xaa);
		while self.pc() % 4 != 0 {
			self.insns.push(0);
		}
		self.push_branch(base, default, true);
		self.insns.extend_from_slice(&(high - low + 1).to_be_bytes());
		for t in targets {
			self.push_branch(base, t, true);
		}
	}

	/// `lookupswitch` with (key, label) pairs
	pub fn lookup_switch(&mut self, default: Label, pairs: Vec<(i32, Label)>) {
		let base = self.pc();
		self.insns.push(0xab);
		while self.pc() % 4 != 0 {
			self.insns.push(0);
		}
		self.push_branch(base, default, true);
		self.insns.extend_from_slice(&(pairs.len() as i32).to_be_bytes());
		for (key, label) in pairs {
			self.insns.extend_from_slice(&key.to_be_bytes());
			self.push_branch(base, label, true);
		}
	}

	fn push_branch(&mut self, base: usize, label: Label, wide: bool) {
		let at = self.pc();
		if wide {
			self.insns.extend_from_slice(&0i32.to_be_bytes());
		} else {
			self.insns.extend_from_slice(&0i16.to_be_bytes());
		}
		self.patches.push(Patch { at, base, wide, label: label.id });
	}

	pub fn handler(&mut self, h: Handler) {
		self.handlers.push(h);
	}

	pub fn line(&mut self, pc: u32, line: u32) {
		self.line_numbers.push((pc, line));
	}

	pub fn local_var(&mut self, v: LocalVar) {
		self.locals.push(v);
	}

	/// `count` frames followed by `frames` bytes, copied verbatim into the
	/// nested `StackMapTable` attribute.
	pub fn raw_stack_map(&mut self, count: u16, frames: &[u8]) {
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&count.to_be_bytes());
		body.extend_from_slice(frames);
		self.stack_map = Some((count, body));
	}

	/// one `same_frame`; `delta` is relative to the previous frame offset (0 for
	/// the first frame), as in JVMS 4.7.4
	pub fn same_frame(&mut self, delta: u32) {
		self.push_frame_raw(&[delta as u8]);
	}

	/// `full_frame` with the given locals/stack as verification type bytes
	pub fn full_frame(&mut self, delta: u32, locals: &[u8], stack: &[u8]) {
		let mut f: Vec<u8> = vec![255];
		f.extend_from_slice(&delta.to_be_bytes()[2..]);
		f.extend_from_slice(&(locals.len() as u16).to_be_bytes());
		f.extend_from_slice(locals);
		f.extend_from_slice(&(stack.len() as u16).to_be_bytes());
		f.extend_from_slice(stack);
		self.push_frame_raw(&f);
	}

	fn push_frame_raw(&mut self, frame: &[u8]) {
		match self.stack_map.as_mut() {
			Some((c, body)) => {
				*c += 1;
				body.extend_from_slice(frame);
				let n = *c;
				body[0..2].copy_from_slice(&n.to_be_bytes());
			}
			None => {
				let mut body: Vec<u8> = vec![0u8, 0u8];
				body.extend_from_slice(frame);
				body[0..2].copy_from_slice(&1u16.to_be_bytes());
				self.stack_map = Some((1, body));
			}
		}
	}

	fn resolve_patches(&mut self) -> Result<(), String> {
		let patches = std::mem::take(&mut self.patches);
		let labels = self.labels.clone();
		for p in patches {
			let target = match labels[p.label] {
				Some(t) => t,
				None => return Err(format!("unresolved label {}", p.label)),
			};
			let delta = target as i64 - p.base as i64;
			let code_len = self.insns.len();
			if p.wide {
				let v = delta as i32;
				let bytes = v.to_be_bytes();
				self.insns[p.at..p.at + 4].copy_from_slice(&bytes);
			} else {
				let v = delta as i16;
				let bytes = v.to_be_bytes();
				self.insns[p.at..p.at + 2].copy_from_slice(&bytes);
			}
			if p.at + 4 > code_len {
				return Err("patch out of range".to_string());
			}
		}
		Ok(())
	}

	/// Serialise the `Code` attribute body. Panics on unresolved labels: fixture
	/// bugs should fail loudly inside `#[test]`.
	pub fn build(mut self) -> Vec<u8> {
		self.resolve_patches().expect("fixture: unresolved branch label");
		let mut out: Vec<u8> = Vec::new();
		out.extend_from_slice(&self.max_stack.to_be_bytes());
		out.extend_from_slice(&self.max_locals.to_be_bytes());
		out.extend_from_slice(&(self.insns.len() as u32).to_be_bytes());
		out.extend_from_slice(&self.insns);
		out.extend_from_slice(&(self.handlers.len() as u16).to_be_bytes());
		for h in &self.handlers {
			out.extend_from_slice(&(h.start as u16).to_be_bytes());
			out.extend_from_slice(&(h.end as u16).to_be_bytes());
			out.extend_from_slice(&(h.handler as u16).to_be_bytes());
			out.extend_from_slice(&h.catch_type.to_be_bytes());
		}
		let mut nested: Vec<(u16, Vec<u8>)> = Vec::new();
		if !self.line_numbers.is_empty() {
			let mut b: Vec<u8> = Vec::new();
			b.extend_from_slice(&(self.line_numbers.len() as u16).to_be_bytes());
			for (pc, line) in &self.line_numbers {
				b.extend_from_slice(&(*pc as u16).to_be_bytes());
				b.extend_from_slice(&(*line as u16).to_be_bytes());
			}
			nested.push((ATTR_LINE_NUMBER_TABLE, b));
		}
		if !self.locals.is_empty() {
			let mut b: Vec<u8> = Vec::new();
			b.extend_from_slice(&(self.locals.len() as u16).to_be_bytes());
			for l in &self.locals {
				b.extend_from_slice(&(l.start as u16).to_be_bytes());
				b.extend_from_slice(&(l.length as u16).to_be_bytes());
				b.extend_from_slice(&l.name.to_be_bytes());
				b.extend_from_slice(&l.desc.to_be_bytes());
				b.extend_from_slice(&l.index.to_be_bytes());
			}
			nested.push((ATTR_LOCAL_VARIABLE_TABLE, b));
		}
		if let Some((_, body)) = &self.stack_map {
			nested.push((ATTR_STACK_MAP_TABLE, body.clone()));
		}
		out.extend_from_slice(&(nested.len() as u16).to_be_bytes());
		for (name, body) in nested {
			out.extend_from_slice(&name.to_be_bytes());
			out.extend_from_slice(&(body.len() as u32).to_be_bytes());
			out.extend_from_slice(&body);
		}
		out
	}
}

/// One field or method entry.
#[derive(Debug)]
struct Member {
	access: u16,
	name: u16,
	desc: u16,
	attrs: Vec<(u16, Vec<u8>)>,
}

/// Assembles a complete class file. All `xxx_idx` values handed back refer to the
/// constant pool and are used as operands, exactly like a compiler would.
pub struct ClassBuilder {
	pool: Vec<u8>,
	next_index: u16,
	utf8_ids: Vec<(String, u16)>,
	this_class: u16,
	super_class: u16,
	interfaces: Vec<u16>,
	access: u16,
	fields: Vec<Member>,
	methods: Vec<Member>,
	class_attrs: Vec<(u16, Vec<u8>)>,
	version: u16,
}

impl ClassBuilder {
	/// `this_class` / `super_class` are internal names (`pkg/Class`); an empty
	/// super class writes `0` (only legal for `java/lang/Object`).
	pub fn new(this_class: &str, super_class: &str) -> ClassBuilder {
		let mut b = ClassBuilder {
			pool: Vec::new(),
			next_index: 1 + ATTR_NAMES.len() as u16,
			utf8_ids: Vec::new(),
			this_class: 0,
			super_class: 0,
			interfaces: Vec::new(),
			access: 0x21, // PUBLIC | SUPER
			fields: Vec::new(),
			methods: Vec::new(),
			class_attrs: Vec::new(),
			version: 52,
		};
		for n in ATTR_NAMES.iter() {
			b.push_utf8_raw(n);
		}
		b.this_class = b.class(this_class);
		if !super_class.is_empty() {
			b.super_class = b.class(super_class);
		}
		b
	}

	pub fn with_version(mut self, major: u16) -> Self {
		self.version = major;
		self
	}

	fn push_utf8_raw(&mut self, s: &str) -> u16 {
		self.pool.push(1);
		self.pool.extend_from_slice(&(s.len() as u16).to_be_bytes());
		self.pool.extend_from_slice(s.as_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn utf8(&mut self, s: &str) -> u16 {
		if let Some((_, id)) = self.utf8_ids.iter().find(|(v, _)| v == s) {
			return *id;
		}
		let id = self.push_utf8_raw(s);
		self.utf8_ids.push((s.to_string(), id));
		id
	}

	pub fn class(&mut self, internal_name: &str) -> u16 {
		let name_idx = self.utf8(internal_name);
		self.pool.push(7);
		self.pool.extend_from_slice(&name_idx.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn string_const(&mut self, s: &str) -> u16 {
		let str_idx = self.utf8(s);
		self.pool.push(8);
		self.pool.extend_from_slice(&str_idx.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn int_const(&mut self, v: i32) -> u16 {
		self.pool.push(3);
		self.pool.extend_from_slice(&v.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn float_const(&mut self, v: f32) -> u16 {
		self.pool.push(4);
		self.pool.extend_from_slice(&v.to_bits().to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	/// `Long` and `Double` take two pool slots (JVMS 4.4.5).
	pub fn long_const(&mut self, v: i64) -> u16 {
		self.pool.push(5);
		self.pool.extend_from_slice(&v.to_be_bytes());
		let at = self.next_index;
		self.next_index += 2;
		at
	}

	pub fn double_const(&mut self, v: f64) -> u16 {
		self.pool.push(6);
		self.pool.extend_from_slice(&v.to_bits().to_be_bytes());
		let at = self.next_index;
		self.next_index += 2;
		at
	}

	pub fn name_and_type(&mut self, name: &str, desc: &str) -> u16 {
		let n = self.utf8(name);
		let d = self.utf8(desc);
		self.pool.push(12);
		self.pool.extend_from_slice(&n.to_be_bytes());
		self.pool.extend_from_slice(&d.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	fn ref_entry(&mut self, tag: u8, class_index: u16, nat: u16) -> u16 {
		self.pool.push(tag);
		self.pool.extend_from_slice(&class_index.to_be_bytes());
		self.pool.extend_from_slice(&nat.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn field_ref(&mut self, owner_class_index: u16, name: &str, desc: &str) -> u16 {
		let nat = self.name_and_type(name, desc);
		self.ref_entry(9, owner_class_index, nat)
	}

	pub fn method_ref(&mut self, owner_class_index: u16, name: &str, desc: &str) -> u16 {
		let nat = self.name_and_type(name, desc);
		self.ref_entry(10, owner_class_index, nat)
	}

	pub fn iface_method_ref(&mut self, owner_class_index: u16, name: &str, desc: &str) -> u16 {
		let nat = self.name_and_type(name, desc);
		self.ref_entry(11, owner_class_index, nat)
	}

	/// `CONSTANT_MethodHandle_info`, used by `@Annotation`/indy fixtures.
	pub fn method_handle(&mut self, kind: u8, ref_index: u16) -> u16 {
		self.pool.push(15);
		self.pool.push(kind);
		self.pool.extend_from_slice(&ref_index.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	pub fn method_type(&mut self, desc: &str) -> u16 {
		let d = self.utf8(desc);
		self.pool.push(16);
		self.pool.extend_from_slice(&d.to_be_bytes());
		let at = self.next_index;
		self.next_index += 1;
		at
	}

	/// `BootstrapMethods` attribute with one entry; returns nothing, call before
	/// `to_bytes`.
	pub fn bootstrap_method(&mut self, method_ref_index: u16, args: &[u16]) {
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&1u16.to_be_bytes()); // num_bootstrap_methods
		body.extend_from_slice(&method_ref_index.to_be_bytes());
		body.extend_from_slice(&(args.len() as u16).to_be_bytes());
		for a in args {
			body.extend_from_slice(&a.to_be_bytes());
		}
		let attr = self.utf8("BootstrapMethods");
		self.class_attrs.push((attr, body));
	}

	pub fn add_interface(&mut self, internal_name: &str) {
		let idx = self.class(internal_name);
		self.interfaces.push(idx);
	}

	pub fn set_access(&mut self, flags: u16) {
		self.access = flags;
	}

	pub fn source_file(&mut self, name: &str) {
		let sf_name = self.utf8(name);
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&sf_name.to_be_bytes());
		self.class_attrs.push((ATTR_SOURCE_FILE, body));
	}

	/// `Signature` attribute with a raw generic signature.
	pub fn signature(&mut self, sig: &str) {
		let sig_idx = self.utf8(sig);
		let attr = self.utf8("Signature");
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&sig_idx.to_be_bytes());
		self.class_attrs.push((attr, body));
	}

	pub fn field(&mut self, access: u16, name: &str, desc: &str) {
		let n = self.utf8(name);
		let d = self.utf8(desc);
		self.fields.push(Member { access, name: n, desc: d, attrs: Vec::new() });
	}

	/// field with a `ConstantValue` (compile-time constant)
	pub fn field_const(&mut self, access: u16, name: &str, desc: &str, const_index: u16) {
		let n = self.utf8(name);
		let d = self.utf8(desc);
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&const_index.to_be_bytes());
		self.fields.push(Member { access, name: n, desc: d, attrs: vec![(ATTR_CONSTANT_VALUE, body)] });
	}

	pub fn method(&mut self, access: u16, name: &str, desc: &str, code: Option<CodeBuilder>) {
		let n = self.utf8(name);
		let d = self.utf8(desc);
		let mut attrs = Vec::new();
		if let Some(c) = code {
			attrs.push((ATTR_CODE, c.build()));
		}
		self.methods.push(Member { access, name: n, desc: d, attrs });
	}

	/// method with an `Exceptions` attribute (`throws` clause)
	pub fn method_throws(&mut self, access: u16, name: &str, desc: &str, code: Option<CodeBuilder>, throws: &[&str]) {
		let n = self.utf8(name);
		let d = self.utf8(desc);
		let mut attrs = Vec::new();
		if let Some(c) = code {
			attrs.push((ATTR_CODE, c.build()));
		}
		let mut body: Vec<u8> = Vec::new();
		body.extend_from_slice(&(throws.len() as u16).to_be_bytes());
		for t in throws {
			let cls = self.class(t);
			body.extend_from_slice(&cls.to_be_bytes());
		}
		attrs.push((ATTR_EXCEPTIONS, body));
		self.methods.push(Member { access, name: n, desc: d, attrs });
	}

	fn member_bytes(&self, members: &[Member]) -> Vec<u8> {
		let mut out: Vec<u8> = Vec::new();
		out.extend_from_slice(&(members.len() as u16).to_be_bytes());
		for m in members {
			out.extend_from_slice(&m.access.to_be_bytes());
			out.extend_from_slice(&m.name.to_be_bytes());
			out.extend_from_slice(&m.desc.to_be_bytes());
			out.extend_from_slice(&(m.attrs.len() as u16).to_be_bytes());
			for (name, body) in &m.attrs {
				out.extend_from_slice(&name.to_be_bytes());
				out.extend_from_slice(&(body.len() as u32).to_be_bytes());
				out.extend_from_slice(body);
			}
		}
		out
	}

	pub fn to_bytes(&mut self) -> Vec<u8> {
		let mut out: Vec<u8> = Vec::new();
		out.extend_from_slice(&MAGIC.to_be_bytes());
		out.extend_from_slice(&0u16.to_be_bytes()); // minor
		out.extend_from_slice(&self.version.to_be_bytes()); // major
		out.extend_from_slice(&self.next_index.to_be_bytes()); // constant_pool_count
		out.extend_from_slice(&self.pool);
		out.extend_from_slice(&self.access.to_be_bytes());
		out.extend_from_slice(&self.this_class.to_be_bytes());
		out.extend_from_slice(&self.super_class.to_be_bytes());
		out.extend_from_slice(&(self.interfaces.len() as u16).to_be_bytes());
		for i in &self.interfaces {
			out.extend_from_slice(i.to_be_bytes());
		}
		let fields = self.member_bytes(&self.fields);
		let methods = self.member_bytes(&self.methods);
		out.extend_from_slice(&fields);
		out.extend_from_slice(&methods);
		out.extend_from_slice(&(self.class_attrs.len() as u16).to_be_bytes());
		for (name, body) in &self.class_attrs {
			out.extend_from_slice(&name.to_be_bytes());
			out.extend_from_slice(&(body.len() as u32).to_be_bytes());
			out.extend_from_slice(body);
		}
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::java::class_file::JavaClassFile;

	#[test]
	fn builder_output_parses_back() {
		let mut b = ClassBuilder::new("pkg/Hello", "java/lang/Object");
		b.source_file("Hello.java");
		b.field(0x0002, "count", "I"); // private
		let mut c = CodeBuilder::new(1, 1);
		c.op(0xb1); // return
		b.method(0x0001, "run", "()V", Some(c));
		let bytes = b.to_bytes();

		let cls = JavaClassFile::parse(&bytes).expect("parse");
		assert_eq!(cls.this_class, "pkg/Hello");
		assert_eq!(cls.super_class.as_deref(), Some("java/lang/Object"));
		assert_eq!(cls.source_file(), Some("Hello.java"));
		assert_eq!(cls.fields.len(), 1);
		assert_eq!(cls.fields[0].name, "count");
		assert_eq!(cls.methods.len(), 1);
		assert_eq!(cls.methods[0].name, "run");
		let code = cls.methods[0].code().expect("code attribute");
		assert_eq!(code.code, vec![0xb1]);
	}

	#[test]
	fn branch_labels_are_patched() {
		let mut c = CodeBuilder::new(1, 2);
		let end = c.new_label();
		c.op_u1(0x15, 1); // iload 1        -> offset 0..2
		c.branch(0x9a, end); // ifne end   -> opcode at 2, operand at 3
		c.op(0xb1); // return              -> offset 5
		c.mark(end);
		c.op(0xb1); // return              -> offset 6
		assert_eq!(c.pc(), 7);
		let body = c.build();
		let code = &body[8..8 + 7];
		// target (6) - branch base (2) == 4
		assert_eq!(code[3], 0x00);
		assert_eq!(code[4], 0x04);
	}

	#[test]
	fn switch_targets_round_trip_through_the_decoder() {
		use crate::io::BinReader;
		use crate::java::const_pool::ConstPool;
		use crate::java::insn::decode_code;
		use crate::java::insn::Sem;

		let mut c = CodeBuilder::new(1, 1);
		let d = c.new_label();
		let t0 = c.new_label();
		c.op(0x1a); // iload_0, offset 0
		c.table_switch(0, 0, d, vec![t0]);
		c.op(0xb1); // return
		c.mark(d);
		c.mark(t0);
		let body = c.build();
		let code: Vec<u8> = body[8..].to_vec(); // strip the Code attribute header

		let pool = ConstPool::parse(&mut BinReader::new(&[0u8, 0x01])).unwrap();
		let insns = decode_code(&code, &pool).unwrap();
		assert_eq!(insns.len(), 3);
		assert_eq!(insns[1].sem(), Sem::TableSwitch);
		let (default, pairs) = insns[1].switch_data().unwrap();
		assert_eq!(default, insns[2].off);
		assert_eq!(pairs, vec![(0, insns[2].off)]);
	}

	#[test]
	fn constant_pool_slots_for_wide_constants() {
		let mut b = ClassBuilder::new("A", "java/lang/Object");
		let l = b.long_const(1234567890123);
		let after = b.utf8("after-long");
		// a Long occupies two slots, so the next entry index is +2
		assert_eq!(after, l + 2);
		let cls = JavaClassFile::parse(&b.to_bytes()).unwrap();
		assert_eq!(cls.pool.long_or_zero(l), 1234567890123);
	}
}
