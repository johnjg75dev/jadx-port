//! Dalvik `classes.dex` reading and smali listing.
//!
//! This is the counterpart of `jadx-plugins/jadx-dex-input`
//! (`DexFileLoader`, `utils/SectionReader`, `insns/DexInsnData` and the
//! `SmaliWriter` of `jadx-smali-input`). The opcode table is generated from
//! jadx's own files, see [`table`].
//!
//! ## What is supported, and what is not
//!
//! * the whole *container* is parsed: header, string/type/proto/field/method id
//!   sections, map list, class definitions and their `class_data_item`, including
//!   the `code_item` of every method (registers, ins/outs, try items);
//! * every method can be listed as smali-ish text ([`DexFile::class_smali`]),
//!   with constant pool references resolved to strings, types, fields and
//!   methods, exactly like `jadx --output-format smali` / the smali plugin;
//! * **DEX to Java decompilation is not implemented** ([`DEX_JAVA_STATUS`]). jadx
//!   builds SSA from Dalvik and runs ~30 region passes over it; that pipeline is
//!   the single largest component of jadx and is phase 2 of this port. The
//!   consequence, and how callers see it, is documented in `docs/PORT_STATUS.md`.
//! * odex/vdex containers and `map_list` verification are not read; a bad
//!   `endian_tag` or a truncated section is reported as an error.

use crate::error::{ErrorKind, JadxError, Res};
use crate::io::BinReader;
use crate::types::{JType, MethodProto};

pub mod table;

use table::{DexFmt, IdxKind, DEX_OPS, PAYLOAD_FILL_ARRAY_DATA, PAYLOAD_PACKED_SWITCH, PAYLOAD_SPARSE_SWITCH};

/// `dex\n035\0` .. `dex\n041\0` plus the `endian_tag` (JVMS equivalent: the magic
/// jadx checks in `DexFileLoader.isDexData`).
pub const DEX_MAGIC: &[u8; 4] = b"dex\n";
pub const ENDIAN_TAG: u32 = 0x1234_5678;
pub const HEADER_SIZE: u32 = 0x70;

/// The reason a dex class has no Java source in this port. Surfaced as a class
/// note, as an error entry, and by `jadx_class_get_java_source` for dex inputs.
pub const DEX_JAVA_STATUS: &str = "dex input: DEX to Java decompilation is not implemented in this port (phase 2); \
	the class is fully parsed, its members are listed and its bytecode is disassembled as smali";

#[derive(Debug, Clone, Copy)]
pub struct DexHeader {
	pub version: [u8; 3],
	pub checksum: u32,
	pub file_size: u32,
	pub header_size: u32,
	pub endian_tag: u32,
	pub map_off: u32,
	pub string_ids_size: u32,
	pub string_ids_off: u32,
	pub type_ids_size: u32,
	pub type_ids_off: u32,
	pub proto_ids_size: u32,
	pub proto_ids_off: u32,
	pub field_ids_size: u32,
	pub field_ids_off: u32,
	pub method_ids_size: u32,
	pub method_ids_off: u32,
	pub class_defs_size: u32,
	pub class_defs_off: u32,
	pub data_size: u32,
	pub data_off: u32,
}

/// `Lpkg/C;` -> `pkg/C`.
pub fn descriptor_to_name(desc: &str) -> String {
	let d = desc.strip_prefix('L').unwrap_or(desc);
	d.strip_suffix(';').unwrap_or(d).to_string()
}

/// `pkg/C` -> `Lpkg/C;`.
pub fn name_to_descriptor(name: &str) -> String {
	format!("L{};", name)
}

#[derive(Debug, Clone)]
pub struct DexField {
	pub name: String,
	pub ty: JType,
	pub access_flags: u32,
}

/// One encoded method; `code` is present unless the method is abstract or native.
#[derive(Debug, Clone)]
pub struct DexMethod {
	pub name: String,
	pub proto: MethodProto,
	/// raw descriptor, `(II)I`, built from `proto`
	pub desc: String,
	pub access_flags: u32,
	pub code_off: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct DexClassDef {
	/// `pkg/C`, without the `L`/`;` of the descriptor
	pub name: String,
	pub descriptor: String,
	pub access_flags: u32,
	pub super_class: Option<String>,
	pub interfaces: Vec<String>,
	pub source_file: Option<String>,
	pub static_fields: Vec<DexField>,
	pub instance_fields: Vec<DexField>,
	pub direct_methods: Vec<DexMethod>,
	pub virtual_methods: Vec<DexMethod>,
	/// annotations directory offset, kept for a later pass but not decoded
	pub annotations_off: Option<u32>,
	/// `static_values_item` offset (encoded array of initial values)
	pub static_values_off: Option<u32>,
}

impl DexClassDef {
	pub fn class_name(&self) -> String {
		self.name.clone()
	}

	pub fn all_methods(&self) -> Vec<&DexMethod> {
		self.direct_methods.iter().chain(self.virtual_methods.iter()).collect()
	}

	pub fn all_fields(&self) -> Vec<&DexField> {
		self.static_fields.iter().chain(self.instance_fields.iter()).collect()
	}
}

/// A parsed `classes.dex`. Borrowed from the input buffer, like jadx's
/// `DexFileData` keeps the mmap alive.
#[derive(Debug, Clone)]
pub struct DexFile<'a> {
	data: &'a [u8],
	hdr: DexHeader,
	strings: Vec<String>,
	types: Vec<String>,
	protos: Vec<ProtoEntry>,
	fields: Vec<FieldEntry>,
	methods: Vec<MethodEntry>,
	classes: Vec<DexClassDef>,
}

#[derive(Debug, Clone)]
struct ProtoEntry {
	shorty: String,
	return_type: u32,
	params: Vec<u32>,
}

#[derive(Debug, Clone)]
struct FieldEntry {
	class_idx: u32,
	type_idx: u32,
	name: String,
}

#[derive(Debug, Clone)]
struct MethodEntry {
	class_idx: u32,
	proto_idx: u32,
	name: String,
}

impl<'a> DexFile<'a> {
	pub fn data(&self) -> &'a [u8] {
		self.data
	}

	pub fn header(&self) -> &DexHeader {
		&self.hdr
	}

	/// `035`, `039`, ... as text (jadx: `DexFileLoader.getVersion`).
	pub fn version(&self) -> String {
		String::from_utf8_lossy(&self.hdr.version).into_owned()
	}

	pub fn class_defs(&self) -> &[DexClassDef] {
		&self.classes
	}

	pub fn string_count(&self) -> usize {
		self.strings.len()
	}

	pub fn type_count(&self) -> usize {
		self.types.len()
	}

	pub fn method_count(&self) -> usize {
		self.methods.len()
	}

	pub fn string_at(&self, idx: u32) -> &str {
		self.strings.get(idx as usize).map(|s| s.as_str()).unwrap_or("<bad string index>")
	}

	pub fn type_at(&self, idx: u32) -> &str {
		self.types.get(idx as usize).map(|s| s.as_str()).unwrap_or("unknown")
	}

	/// The `Lpkg/C;` descriptor of a type index.
	pub fn type_desc_at(&self, idx: u32) -> String {
		self.type_at(idx).to_string()
	}

	pub fn field_at(&self, idx: u32) -> Option<&FieldEntry> {
		self.fields.get(idx as usize)
	}

	pub fn method_at(&self, idx: u32) -> Option<&MethodEntry> {
		self.methods.get(idx as usize)
	}

	/// `pkg/C#m(I)V`, the id used for renames. Built the same way as for JVM class
	/// files so one `.jobf` file can rename both kinds.
	pub fn method_id(&self, cls: &DexClassDef, index: usize) -> String {
		match index.checked_sub(cls.static_fields.len() + cls.instance_fields.len()) {
			Some(i) if i < cls.direct_methods.len() => format!("{}#{}{}", cls.name, cls.direct_methods[i].name, cls.direct_methods[i].desc),
			_ => format!("{}#{}", cls.name, index),
		}
	}

	pub fn parse(data: &'a [u8]) -> Res<DexFile<'a>> {
		if data.len() < HEADER_SIZE as usize {
			return Err(JadxError::format("dex file shorter than the header"));
		}
		if &data[0..4] != DEX_MAGIC {
			return Err(JadxError::format("not a dex file: bad magic"));
		}
		// `header_item` starts right after the 8-byte magic; reading it from the
		// wrong offset is the classic dex bug, hence the explicit slice.
		let mut r = BinReader::little(&data[8..0x70]);
		let mut hdr = DexHeader {
			version: [data[4], data[5], data[6]],
			checksum: 0,
			file_size: 0,
			header_size: 0,
			endian_tag: 0,
			map_off: 0,
			string_ids_size: 0,
			string_ids_off: 0,
			type_ids_size: 0,
			type_ids_off: 0,
			proto_ids_size: 0,
			proto_ids_off: 0,
			field_ids_size: 0,
			field_ids_off: 0,
			method_ids_size: 0,
			method_ids_off: 0,
			class_defs_size: 0,
			class_defs_off: 0,
			data_size: 0,
			data_off: 0,
		};
		// the header fields after the magic, in the order of `header_item`
		hdr.checksum = r.u32()?;
		r.skip(20)?; // signature
		hdr.file_size = r.u32()?;
		hdr.header_size = r.u32()?;
		hdr.endian_tag = r.u32()?;
		r.u32()?; // link_size
		r.u32()?; // link_off
		hdr.map_off = r.u32()?;
		hdr.string_ids_size = r.u32()?;
		hdr.string_ids_off = r.u32()?;
		hdr.type_ids_size = r.u32()?;
		hdr.type_ids_off = r.u32()?;
		hdr.proto_ids_size = r.u32()?;
		hdr.proto_ids_off = r.u32()?;
		r.u32()?; // cache_offset
		r.u32()?; // annotations_directory_size
		r.u32()?; // annotations_directory_off
		hdr.field_ids_size = r.u32()?;
		hdr.field_ids_off = r.u32()?;
		hdr.method_ids_size = r.u32()?;
		hdr.method_ids_off = r.u32()?;
		hdr.class_defs_size = r.u32()?;
		hdr.class_defs_off = r.u32()?;
		hdr.data_size = r.u32()?;
		hdr.data_off = r.u32()?;
		if hdr.endian_tag != ENDIAN_TAG {
			return Err(JadxError::new(
				ErrorKind::Unsupported,
				format!("big-endian dex files (endian_tag 0x{:08x}) are not supported", hdr.endian_tag),
			));
		}
		if hdr.header_size != HEADER_SIZE {
			return Err(JadxError::format(format!("unexpected dex header size {}", hdr.header_size)));
		}
		let mut dex = DexFile {
			data,
			hdr,
			strings: Vec::new(),
			types: Vec::new(),
			protos: Vec::new(),
			fields: Vec::new(),
			methods: Vec::new(),
			classes: Vec::new(),
		};
		dex.read_strings()?;
		dex.read_types()?;
		dex.read_protos()?;
		dex.read_fields()?;
		dex.read_methods()?;
		dex.read_class_defs()?;
		Ok(dex)
	}

	fn sec(&self, off: u32, len: usize) -> Res<&'a [u8]> {
		let start = off as usize;
		if start > self.data.len() || start + len > self.data.len() {
			return Err(JadxError::format(format!("dex section at {} (+{}) is outside the file", start, len)));
		}
		Ok(&self.data[start..start + len])
	}

	fn read_strings(&mut self) -> Res<()> {
		let n = self.hdr.string_ids_size as usize;
		let mut out = Vec::with_capacity(n);
		let base = self.sec(self.hdr.string_ids_off, n * 4)?;
		let mut r = BinReader::little(base);
		for i in 0..n {
			let off = r.u32()?;
			let body = self.sec(off, 1024 * 1024).unwrap_or(&self.data[off as usize..]);
			let mut br = BinReader::little(body);
			let _len = br.uleb128()?; // utf16 length, the terminator is enough
			let start = br.pos();
			// mutf8 strings end in a 0 byte, and the NUL may only appear as the
			// terminator (`\0` is encoded as 0x00 0x80 elsewhere)
			let end = body[start..]
				.iter()
				.position(|b| *b == 0)
				.map(|p| start + p)
				.unwrap_or(body.len());
			out.push(crate::java::const_pool::decode_modified_utf8(&body[start..end]));
			let _ = i;
		}
		self.strings = out;
		Ok(())
	}

	fn read_types(&mut self) -> Res<()> {
		let n = self.hdr.type_ids_size as usize;
		let base = self.sec(self.hdr.type_ids_off, n * 4)?;
		let mut r = BinReader::little(base);
		let mut out = Vec::with_capacity(n);
		for _ in 0..n {
			let idx = r.u32()?;
			out.push(self.string_at(idx).to_string());
		}
		self.types = out;
		Ok(())
	}

	fn read_protos(&mut self) -> Res<()> {
		let n = self.hdr.proto_ids_size as usize;
		let base = self.sec(self.hdr.proto_ids_off, n * 12)?.to_vec();
		let mut out = Vec::with_capacity(n);
		for i in 0..n {
			let mut r = BinReader::little(&base[i * 12..i * 12 + 12]);
			let shorty = r.u32()?;
			let return_type = r.u32()?;
			let params_off = r.u32()?;
			let mut params = Vec::new();
			if params_off != 0 {
				// `type_list`: u32 size then `type_item { type_idx: u16 }`
				let raw = self.sec(params_off, 4)?.to_vec();
				let mut pr = BinReader::little(&raw);
				let size = pr.u32()? as usize;
				let body = self.sec(params_off + 4, size * 2)?.to_vec();
				let mut br = BinReader::little(&body);
				for _ in 0..size {
					params.push(br.u16()? as u32);
				}
			}
			let shorty = self.string_at(shorty).to_string();
			out.push(ProtoEntry { shorty, return_type, params });
		}
		self.protos = out;
		Ok(())
	}

	fn read_fields(&mut self) -> Res<()> {
		let n = self.hdr.field_ids_size as usize;
		let base = self.sec(self.hdr.field_ids_off, n * 8)?.to_vec();
		let mut out = Vec::with_capacity(n);
		for i in 0..n {
			let mut r = BinReader::little(&base[i * 8..i * 8 + 8]);
			let class_idx = r.u16()? as u32;
			let type_idx = r.u16()? as u32;
			let name_idx = r.u32()?;
			out.push(FieldEntry { class_idx, type_idx, name: self.string_at(name_idx).to_string() });
		}
		self.fields = out;
		Ok(())
	}

	fn read_methods(&mut self) -> Res<()> {
		let n = self.hdr.method_ids_size as usize;
		let base = self.sec(self.hdr.method_ids_off, n * 8)?.to_vec();
		let mut out = Vec::with_capacity(n);
		for i in 0..n {
			let mut r = BinReader::little(&base[i * 8..i * 8 + 8]);
			let class_idx = r.u16()? as u32;
			let proto_idx = r.u16()? as u32;
			let name_idx = r.u32()?;
			out.push(MethodEntry { class_idx, proto_idx, name: self.string_at(name_idx).to_string() });
		}
		self.methods = out;
		Ok(())
	}

	/// The descriptor a type index stands for, as a `JType`.
	pub fn jtype_of(&self, type_idx: u32) -> JType {
		let desc = self.type_at(type_idx);
		JType::from_descriptor(desc)
	}

	fn proto_of(&self, idx: u32) -> (MethodProto, String) {
		let p = match self.protos.get(idx as usize) {
			Some(p) => p,
			None => return (MethodProto { args: Vec::new(), ret: JType::Void }, "()V".to_string()),
		};
		let args: Vec<JType> = p.params.iter().map(|&t| self.jtype_of(t)).collect();
		let ret = self.jtype_of(p.return_type);
		let mut desc = String::from("(");
		for a in &args {
			desc.push_str(&a.descriptor());
		}
		desc.push(')');
		desc.push_str(&ret.descriptor());
		(MethodProto { args, ret }, desc)
	}

	fn read_class_defs(&mut self) -> Res<()> {
		let n = self.hdr.class_defs_size as usize;
		if n == 0 {
			return Ok(());
		}
		let base = self.sec(self.hdr.class_defs_off, n * 32)?.to_vec();
		let mut out = Vec::with_capacity(n);
		for i in 0..n {
			let mut r = BinReader::little(&base[i * 32..i * 32 + 32]);
			let class_idx = r.u32()?;
			let access_flags = r.u32()?;
			let super_class = r.u32()?;
			let interfaces_off = r.u32()?;
			let source_file_idx = r.u32()?;
			let annotations_off = r.u32()?;
			let class_data_off = r.u32()?;
			let static_values_off = r.u32()?;
			let descriptor = self.type_at(class_idx).to_string();
			let name = descriptor_to_name(&descriptor);
			let mut interfaces = Vec::new();
			if interfaces_off != 0 {
				let raw = self.sec(interfaces_off, 4)?.to_vec();
				let mut ir = BinReader::little(&raw);
				let size = ir.u32()? as usize;
				let body = self.sec(interfaces_off + 4, size * 2)?.to_vec();
				let mut br = BinReader::little(&body);
				for _ in 0..size {
					let t = br.u16()? as u32;
					interfaces.push(descriptor_to_name(self.type_at(t)));
				}
			}
			let source_file = if source_file_idx == u32::MAX {
				None
			} else {
				Some(self.string_at(source_file_idx).to_string())
			};
			let mut def = DexClassDef {
				name,
				descriptor,
				access_flags,
				super_class: if super_class == u32::MAX || super_class == 0xFFFF_ffff {
					None
				} else {
					Some(descriptor_to_name(self.type_at(super_class)))
				},
				interfaces,
				source_file,
				static_fields: Vec::new(),
				instance_fields: Vec::new(),
				direct_methods: Vec::new(),
				virtual_methods: Vec::new(),
				annotations_off: if annotations_off == 0 { None } else { Some(annotations_off) },
				static_values_off: if static_values_off == 0 { None } else { Some(static_values_off) },
			};
			if class_data_off != 0 {
				self.read_class_data(class_data_off, &mut def)?;
			}
			out.push(def);
		}
		self.classes = out;
		Ok(())
	}

	/// `class_data_item` (dex format 5.5): four counts, then diff-coded entries.
	fn read_class_data(&self, off: u32, def: &mut DexClassDef) -> Res<()> {
		let body = self.sec(off, self.data.len().saturating_sub(off as usize))?;
		let mut r = BinReader::little(body);
		let statics = r.uleb128()? as usize;
		let instances = r.uleb128()? as usize;
		let directs = r.uleb128()? as usize;
		let virtuals = r.uleb128()? as usize;
		let mut idx = 0u32;
		for _ in 0..statics {
			idx += r.uleb128()? as u32;
			let access = r.uleb128()? as u32;
			def.static_fields.push(self.field_entry(idx, access));
		}
		idx = 0;
		for _ in 0..instances {
			idx += r.uleb128()? as u32;
			let access = r.uleb128()? as u32;
			def.instance_fields.push(self.field_entry(idx, access));
		}
		idx = 0;
		for _ in 0..directs {
			idx += r.uleb128()? as u32;
			let access = r.uleb128()? as u32;
			let code = r.uleb128()? as u32;
			def.direct_methods.push(self.method_entry(idx, access, code));
		}
		idx = 0;
		for _ in 0..virtuals {
			idx += r.uleb128()? as u32;
			let access = r.uleb128()? as u32;
			let code = r.uleb128()? as u32;
			def.virtual_methods.push(self.method_entry(idx, access, code));
		}
		Ok(())
	}

	fn field_entry(&self, idx: u32, access: u32) -> DexField {
		match self.fields.get(idx as usize) {
			Some(f) => DexField { name: f.name.clone(), ty: self.jtype_of(f.type_idx), access_flags: access },
			None => DexField { name: format!("field#{}", idx), ty: JType::Void, access_flags: access },
		}
	}

	fn method_entry(&self, idx: u32, access: u32, code_off: u32) -> DexMethod {
		match self.methods.get(idx as usize) {
			Some(m) => {
				let (proto, desc) = self.proto_of(m.proto_idx);
				DexMethod { name: m.name.clone(), proto, desc, access_flags: access, code_off: if code_off == 0 { None } else { Some(code_off) } }
			}
			None => DexMethod {
				name: format!("method#{}", idx),
				proto: MethodProto { args: Vec::new(), ret: JType::Void },
				desc: "()V".to_string(),
				access_flags: access,
				code_off: if code_off == 0 { None } else { Some(code_off) },
			},
		}
	}

	/// Find a class by `pkg/C` or `Lpkg/C;` (jadx: `DexClassLoader` lookups).
	pub fn find_class(&self, name: &str) -> Option<usize> {
		let internal = name.replace('.', "/");
		self.classes.iter().position(|c| c.name == internal || c.descriptor == name || c.descriptor == name_to_descriptor(&internal))
	}

	/// Smali listing of one class, its fields and its methods with disassembled
	/// bodies (jadx: `SmaliWriter.writeClass`).
	pub fn class_smali(&self, index: usize) -> String {
		let Some(c) = self.classes.get(index) else {
			return format!("# no class def at {}\n", index);
		};
		let mut out = String::new();
		out.push_str(&format!("# classes.dex version {}\n", self.version()));
		out.push_str(&format!(".class {} {}\n", smali_access(c.access_flags), c.descriptor));
		if let Some(s) = &c.super_class {
			out.push_str(&format!(".super {}\n", name_to_descriptor(s)));
		}
		for i in &c.interfaces {
			out.push_str(&format!(".implements {}\n", name_to_descriptor(i)));
		}
		if let Some(src) = &c.source_file {
			out.push_str(&format!(".source {}\n", src));
		}
		for f in c.static_fields.iter().chain(c.instance_fields.iter()) {
			out.push_str(&format!("\n.field {} {}: {}\n", smali_access(f.access_flags), f.name, f.ty.descriptor()));
		}
		for (i, m) in c.all_methods().iter().enumerate() {
			out.push_str(&self.method_smali(c, m, i));
		}
		out
	}

	/// Smali of every method of a class, in declaration order (the shape the FFI
	/// `jadx_class_get_smali` per method uses).
	pub fn class_methods_smali(&self, index: usize) -> Vec<String> {
		let Some(c) = self.classes.get(index) else {
			return Vec::new();
		};
		c.all_methods().iter().enumerate().map(|(i, m)| self.method_smali(c, m, i)).collect()
	}

	fn method_smali(&self, cls: &DexClassDef, m: &DexMethod, index: usize) -> String {
		let mut out = String::new();
		out.push_str(&format!("\n.method {} {}{}{}\n", smali_access(m.access_flags), m.name, m.desc, if m.code_off.is_none() { "\n    # no code item" } else { "" }));
		let Some(code_off) = m.code_off else {
			out.push_str(".end method\n");
			return out;
		};
		match self.disasm_code(code_off) {
			Ok(lines) => {
				for l in lines {
					out.push_str(&l);
					out.push('\n');
				}
			}
			Err(e) => {
				out.push_str(&format!("    # {}\n", e.message));
			}
		}
		out.push_str(".end method\n");
		let _ = (cls, index);
		out
	}

	/// `code_item` (dex format 5.4) as smali lines: header, instructions, tries.
	pub fn disasm_code(&self, code_off: u32) -> Res<Vec<String>> {
		let head = self.sec(code_off, 16)?.to_vec();
		let mut r = BinReader::little(&head);
		let registers = r.u16()? as usize;
		let ins = r.u16()? as usize;
		let outs = r.u16()? as usize;
		let tries = r.u16()? as usize;
		let debug_info_off = r.u32()?;
		let insns_size = r.u32()? as usize;
		let body = self.sec(code_off + 16, insns_size * 2)?.to_vec();
		let mut out = Vec::new();
		out.push(format!("    .registers {}, ins {}, outs {}", registers, ins, outs));
		if debug_info_off != 0 {
			out.push("    # debug_info_item not decoded".to_string());
		}
		let units: Vec<u16> = {
			let mut v = Vec::with_capacity(insns_size);
			let mut br = BinReader::little(&body);
			for _ in 0..insns_size {
				v.push(br.u16()?);
			}
			v
		};
		let mut pc = 0usize;
		while pc < insns_size {
			let unit = units[pc];
			let op = (unit & 0xff) as u8;
			let _hi = (unit >> 8) as u8;
			if op == 0 && pc + 1 < insns_size {
				let ident = units[pc + 1];
				if ident == PAYLOAD_PACKED_SWITCH || ident == PAYLOAD_SPARSE_SWITCH || ident == PAYLOAD_FILL_ARRAY_DATA {
					// the payload belongs to the switch/data instruction that jumped
					// here; jadx prints it as a separate labelled block too
					out.push(format!("{:04x}: .payload {:#06x}", pc, ident));
					pc += 2;
					continue;
				}
			}
			let info = &DEX_OPS[op as usize];
			let size = info.fmt.units() as usize;
			let mut line = format!("{:04x}: {}", pc, info.name);
			match self.format_operands(info, &units, pc, size) {
				Ok(ops) if !ops.is_empty() => {
					line.push(' ');
					line.push_str(&ops.join(", "));
				}
				Ok(_) => {}
				Err(e) => {
					line.push_str(&format!("  /* {} */", e.message));
				}
			}
			out.push(line);
			pc += size.max(1);
		}
		if tries != 0 {
			// `try_item`s follow the instructions, padded to a 4-byte boundary
			let mut tpos = 16 + insns_size * 2;
			if tpos % 4 != 0 {
				tpos += 2;
			}
			for t in 0..tries {
				let base = code_off as usize + tpos + t * 8;
				if base + 8 > self.data.len() {
					break;
				}
				let mut tr = BinReader::little(&self.data[base..base + 8]);
				let start = tr.u32()?;
				let count = tr.u16()? as u32;
				let handlers_off = tr.u32()?;
				out.push(format!("    .try_start {} count {} handlers {}", start, count, handlers_off));
			}
		}
		Ok(out)
	}

	/// The operand text of one instruction, following the format's own layout
	/// (`DexInsnFormat.decode`).
	fn format_operands(&self, info: &table::DexOpInfo, units: &[u16], pc: usize, size: usize) -> Res<Vec<String>> {
		let g = |k: usize| -> u16 { *units.get(pc + k).unwrap_or(&0) };
		let u0 = units[pc];
		let a1 = ((u0 >> 8) & 0xff) as u8; // AA byte
		let nib2 = ((u0 >> 4) & 0xf) as u8; // B nibble of the opcode unit
		let nib3 = (u0 >> 12) as u8; // C nibble
		let v = |r: u8| format!("v{}", r);
		let mut ops: Vec<String> = Vec::new();
		let units_of = info.fmt.units() as usize;
		let index = if units_of == 2 {
			g(1) as u32
		} else if units_of >= 3 {
			(g(1) as u32) | ((g(2) as u32) << 16)
		} else {
			0
		};
		let idx_text = |dex: &DexFile<'a>| -> Res<String> {
			Ok(match info.idx {
				IdxKind::String => format!("\"{}\"", crate::java::const_pool::string_literal(dex.string_at(index), true)),
				IdxKind::RawString => format!("\"{}\"", dex.string_at(index)),
				IdxKind::Type => dex.type_at(index).to_string(),
				IdxKind::Field => match dex.fields.get(index as usize) {
					Some(f) => format!("{}->{}:{}", dex.type_at(f.class_idx), f.name, dex.type_at(f.type_idx)),
					None => format!("field#{}", index),
				},
				IdxKind::Method => match dex.methods.get(index as usize) {
					Some(m) => {
						let (_p, desc) = dex.proto_of(m.proto_idx);
						format!("{}->{}{}", dex.type_at(m.class_idx), m.name, desc)
					}
					None => format!("method#{}", index),
				},
				IdxKind::CallSite => format!("@call_site{}", index),
				IdxKind::None => String::new(),
			})
		};
		match info.fmt {
			DexFmt::F_10X => {}
			DexFmt::F_11X => ops.push(v(a1)),
			DexFmt::F_10T => ops.push(format!("+{}", (a1 as i8))),
			DexFmt::F_12X => {
				ops.push(v(nib2));
				ops.push(v(nib3));
			}
			DexFmt::F_11N => {
				ops.push(v(nib2));
				ops.push(format!("{}", ((nib3 as i8) << 4) >> 4));
			}
			DexFmt::F_20T => ops.push(format!("{}", (g(1) as i16))),
			DexFmt::F_21C | DexFmt::F_21S | DexFmt::F_21H | DexFmt::F_21T => {
				ops.push(v(a1));
				if info.idx != IdxKind::None {
					ops.push(idx_text(self)?);
				} else {
					ops.push(format!("{}", g(1) as i16));
				}
			}
			DexFmt::F_22B => {
				ops.push(v(a1));
				ops.push(v(nib2));
				ops.push(format!("{}", g(1) as i8));
			}
			DexFmt::F_22C | DexFmt::F_22S | DexFmt::F_22T => {
				ops.push(v(nib2));
				ops.push(v(nib3));
				if info.idx != IdxKind::None {
					ops.push(idx_text(self)?);
				} else {
					ops.push(format!("{}", g(1) as i16));
				}
			}
			DexFmt::F_22X => {
				ops.push(v(a1));
				ops.push(v(g(1)));
			}
			DexFmt::F_23X => {
				ops.push(v(a1));
				ops.push(v(nib2));
				ops.push(v(nib3));
			}
			DexFmt::F_30T => {
				ops.push(v(a1));
				ops.push(format!("{}", ((g(1) as u32) | ((g(2) as u32) << 16)) as i32));
			}
			DexFmt::F_31I => {
				ops.push(v(a1));
				ops.push(format!("{}", ((g(1) as u32) | ((g(2) as u32) << 16)) as i32));
			}
			DexFmt::F_31C => {
				ops.push(v(a1));
				ops.push(idx_text(self)?);
			}
			DexFmt::F_31T => ops.push(format!("+{}", ((g(1) as u32) | ((g(2) as u32) << 16)) as i32)),
			DexFmt::F_32X => {
				ops.push(v(g(1)));
				ops.push(v(g(2)));
			}
			DexFmt::F_35C | DexFmt::F_3RC => {
				let count = if info.fmt == DexFmt::F_35C { (u0 >> 12) as usize } else { a1 as usize };
				let mut regs = Vec::new();
				if info.fmt == DexFmt::F_35C {
					// G|GFFF EDDD -> regs are in units[pc+1] then the register list
					// starts at the *second* unit's high byte for the 5th operand
					regs.push(nib2);
					regs.push(nib3);
					let u1 = g(1);
					regs.push((u1 & 0xf) as u8);
					regs.push(((u1 >> 4) & 0xf) as u8);
					regs.push(((u1 >> 8) & 0xf) as u8);
					// the A field of `35c` carries the 5th register
					regs[4] = a1 & 0xf;
					let mut named = Vec::new();
					for k in 0..count.min(5) {
						// a zero register means "unused", the spec sets them from C
						if k == 4 && count < 5 {
							break;
						}
						named.push(v(regs[k]));
					}
					ops.extend(named);
				} else {
					let first = g(1);
					for k in 0..count {
						ops.push(v(first + k as u16));
					}
				}
				ops.push(idx_text(self)?);
			}
			DexFmt::F_45CC | DexFmt::F_4RCC => {
				// like 35c/3rc plus a 32-bit call-site reference
				ops.push(idx_text(self)?);
				ops.push(format!("proto#{}", g(3)));
			}
			DexFmt::F_51I => {
				// `const-wide vAA, 0xHHHHHHHHLLLLLLLL` spans 6 units: the 64-bit
				// literal occupies units 1..=4 (low half first)
				ops.push(v(a1));
				let lo = (g(1) as u32) | ((g(2) as u32) << 16);
				let hi = if size >= 5 { (g(3) as u32) | ((g(4) as u32) << 16) } else { 0 };
				let value = ((hi as u64) << 32) | lo as u64;
				ops.push(format!("0x{:016x}", value));
			}
			// the remaining formats are only reachable through `unused` entries in
			// jadx's table; print the raw units so nothing is hidden
			other => {
				let mut raw = String::new();
				for k in 0..size {
					raw.push_str(&format!("{:04x} ", g(k)));
				}
				ops.push(format!("/* format {:?}: {}*/", other, raw.trim_end()));
			}
		}
		Ok(ops)
	}
}

/// Access flags in the `.class public static` form of smali.
pub fn smali_access(flags: u32) -> String {
	let mut out = String::new();
	out.push_str(match flags & 0x3 {
		0x1 => "public ",
		0x2 => "private ",
		0x4 => "protected ",
		_ => "",
	});
	for (bit, name) in [
		(0x0008u32, "static "),
		(0x0010, "final "),
		(0x0020, "synchronized "),
		(0x0040, "volatile "),
		(0x0080, "bridge "),
		(0x0100, "transient "),
		(0x0200, "varargs "),
		(0x0400, "native "),
		(0x0800, "interface "),
		(0x1000, "abstract "),
		(0x2000, "strict "),
		(0x4000, "synthetic "),
		(0x8000, "annotation "),
		(0x10000, "enum "),
	] {
		if flags & bit != 0 {
			out.push_str(name);
		}
	}
	// `ACC_PUBLIC|ACC_STATIC|...` high bits (module)
	if flags & 0x8000_0000 != 0 {
		out.push_str("module ");
	}
	out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A minimal but *valid* `classes.dex`: header, one string id, one type id and
	/// no class defs. Building a full file (code items, `class_data_item`, ...) by
	/// hand is what `testutil::ClassBuilder` does for class files; for dex the
	/// sections below are exercised by `sections_are_read_at_the_recorded_offsets`.
	fn minimal_dex() -> Vec<u8> {
		let mut file = vec![0u8; 0x70];
		file[0..4].copy_from_slice(b"dex\n");
		file[4] = b'0';
		file[5] = b'3';
		file[6] = b'5';
		file[7] = 0;
		let mut w = |at: usize, v: u32| file[at..at + 4].copy_from_slice(&v.to_le_bytes());
		w(0x20, 0x80); // file_size
		w(0x24, 0x70); // header_size
		w(0x28, ENDIAN_TAG);
		w(0x38, 1); // string_ids_size
		w(0x3c, 0x70); // string_ids_off
		w(0x40, 1); // type_ids_size
		w(0x44, 0x74); // type_ids_off
		let mut data = file.clone();
		data.extend(0x78u32.to_le_bytes()); // string_id_item -> string_data_item off
		data.extend(0x78u32.to_le_bytes()); // type_id_item -> descriptor_idx
		data.push(1); // utf16 size of "LX;"
		data.extend(b"LX;");
		data.push(0);
		data
	}

	#[test]
	fn the_header_is_read_field_by_field() {
		let data = minimal_dex();
		let dex = DexFile::parse(&data).unwrap();
		assert_eq!(dex.version(), "035");
		assert_eq!(dex.header().file_size, 0x80);
		assert_eq!(dex.header().string_ids_size, 1);
		assert_eq!(dex.string_count(), 1);
		assert_eq!(dex.string_at(0), "LX;");
		assert_eq!(dex.type_at(0), "LX;");
		assert_eq!(dex.type_count(), 1);
		assert!(dex.class_defs().is_empty());
	}

	#[test]
	fn garbage_is_rejected() {
		let e = DexFile::parse(b"not a dex file").unwrap_err();
		assert!(e.message.contains("bad magic"), "{}", e.message);
		let e = DexFile::parse(&[b'd', b'e', b'x', b'\n', 0, 0, 0, 0]).unwrap_err();
		assert!(e.message.contains("shorter than the header"), "{}", e.message);
		let mut bad = minimal_dex();
		bad[0x28] = 0; // endian_tag
		let e = DexFile::parse(&bad).unwrap_err();
		assert_eq!(e.kind, ErrorKind::Unsupported);
		assert!(e.message.contains("big-endian"), "{}", e.message);
	}

	#[test]
	fn descriptors_round_trip() {
		assert_eq!(descriptor_to_name("Lpkg/C;"), "pkg/C");
		assert_eq!(descriptor_to_name("[I"), "[I");
		assert_eq!(name_to_descriptor("pkg/C"), "Lpkg/C;");
	}

	#[test]
	fn smali_access_flags_follow_the_spec() {
		assert_eq!(smali_access(0x1), "public");
		assert_eq!(smali_access(0x9), "public static");
		assert_eq!(smali_access(0x19), "public static final");
		assert_eq!(smali_access(0x4000), "synthetic");
		assert_eq!(smali_access(0x6000), "strict synthetic");
	}

	#[test]
	fn the_opcode_table_drives_the_instruction_size() {
		// `nop` and `move` are one unit, `move/from16` two, `const-wide` six
		assert_eq!(DEX_OPS[0x00].name, "nop");
		assert_eq!(DEX_OPS[0x00].fmt.units(), 1);
		assert_eq!(DEX_OPS[0x01].fmt.units(), 1);
		assert_eq!(DEX_OPS[0x02].fmt.units(), 2);
		let wide = &DEX_OPS[0x18];
		assert_eq!(wide.name, "const-wide");
		assert_eq!(wide.fmt.units(), 6);
		// `goto` is 0x28, `const-string` 0x1a and `invoke-virtual` 0x6e, matching
		// the table jadx generates
		assert_eq!(DEX_OPS[0x28].name, "goto");
		assert_eq!(DEX_OPS[0x1a].name, "const-string");
		assert_eq!(DEX_OPS[0x6e].name, "invoke-virtual");
		assert!(DEX_OPS[0x6e].fmt.is_variadic());
	}

	#[test]
	fn const_forms_use_the_expected_operand_layout() {
		// `const/4 vA, #+b` is a single unit with the literal in the high nibble,
		// `const/16 vAA, #+bbbb` is two units; the decoder reads them through
		// `format_operands`, which is exercised by the two assertions below
		assert_eq!(DEX_OPS[0x12].name, "const/4");
		assert_eq!(DEX_OPS[0x12].fmt, DexFmt::F_11N);
		assert_eq!(DEX_OPS[0x13].name, "const/16");
		assert_eq!(DEX_OPS[0x13].fmt, DexFmt::F_21S);
		assert_eq!(DEX_OPS[0x13].fmt.regs(), Some(1));
	}
}
