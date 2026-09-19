//! User-supplied code data: renames and comments -- the Rust counterpart of
//! `jadx.api.data.ICodeRename`, `ICodeComment`, `CodeRefType` and the
//! `jadx.core.deobf.DeobfPresets` mapping file.
//!
//! File format (`.jobf`, the name jadx derives from *java obfuscation map*): one
//! entry per line, `#` starts a comment, blank lines are ignored:
//!
//! ```text
//! p a.b.c = p001a
//! c a/b/C = C0002a
//! f a/b/C#field:I = f3a
//! m a/b/C#method(I)V = m1a
//! ```
//!
//! The four prefixes and the `orig = alias` shape are jadx's own
//! (`DeobfPresets.load`/`save`), so a map written by jadx can be read here and
//! vice versa. Line ordering on write is the sorted order of the whole lines, as
//! `DeobfPresets.save` does with `Collections.sort(list)`.
//!
//! Comments use one additional line type, `@`, which jadx does not persist (it
//! only keeps them in memory as `JadxCodeData`), so this part is an extension of
//! this port: `@ <target> <node> <line> <text>`.

use std::path::Path;

use crate::error::{ErrorKind, JadxError, Res};

/// What a rename or comment applies to. jadx: `jadx.api.data.CodeRefType`
/// (`CLASS`, `FIELD`, `METHOD`, `MTH_ARG`, `VAR`, `CATCH`, `INSN`); this port
/// additionally needs `PACKAGE` because the `.jobf` format stores package aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CodeRefType {
	Package,
	Class,
	Field,
	Method,
	/// a method argument (jadx: `MTH_ARG`)
	MthArg,
	/// a local variable (jadx: `VAR`)
	Var,
	/// a catch variable (jadx: `CATCH`)
	Catch,
	/// a single instruction (jadx: `INSN`, comments only)
	Insn,
}

impl CodeRefType {
	/// The single-letter code used in the `.jobf` file.
	pub fn code(self) -> char {
		match self {
			CodeRefType::Package => 'p',
			CodeRefType::Class => 'c',
			CodeRefType::Field => 'f',
			CodeRefType::Method => 'm',
			CodeRefType::MthArg => 'r',
			CodeRefType::Var => 'v',
			CodeRefType::Catch => 't',
			CodeRefType::Insn => 'i',
		}
	}

	pub fn from_code(c: char) -> Option<CodeRefType> {
		Some(match c {
			'p' => CodeRefType::Package,
			'c' => CodeRefType::Class,
			'f' => CodeRefType::Field,
			'm' => CodeRefType::Method,
			'r' => CodeRefType::MthArg,
			'v' => CodeRefType::Var,
			't' => CodeRefType::Catch,
			'i' => CodeRefType::Insn,
			_ => return None,
		})
	}

	/// Prefix used when a rename of this kind has to be made unique.
	pub fn name_prefix(self) -> &'static str {
		match self {
			CodeRefType::Package => "p",
			CodeRefType::Class => "C",
			CodeRefType::Field => "f",
			CodeRefType::Method => "m",
			CodeRefType::MthArg => "a",
			CodeRefType::Var => "v",
			CodeRefType::Catch => "e",
			CodeRefType::Insn => "",
		}
	}
}

/// jadx: `jadx.api.data.ICodeRename`. The node id is the raw (unaliased) id used
/// by [`crate::java::class_file::JavaClassFile::method_id`]: a class is its
/// internal name (`a/b/C`), a member is `class#name` plus its descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ICodeRename {
	pub ref_type: CodeRefType,
	/// original name / node id, never the alias
	pub orig_node: String,
	pub new_name: String,
	/// for `Var`/`Catch`/`MthArg`: which slot of the method (jadx: `getCodeRef`)
	pub code_ref: Option<u32>,
}

impl ICodeRename {
	pub fn new(ref_type: CodeRefType, orig_node: impl Into<String>, new_name: impl Into<String>) -> ICodeRename {
		ICodeRename { ref_type, orig_node: orig_node.into(), new_name: new_name.into(), code_ref: None }
	}

	pub fn with_code_ref(mut self, index: u32) -> ICodeRename {
		self.code_ref = Some(index);
		self
	}

	/// `a/b/C#method(I)V`
	pub fn display_id(&self) -> String {
		match self.code_ref {
			Some(i) => format!("{}@{}", self.orig_node, i),
			None => self.orig_node.clone(),
		}
	}

	/// One `.jobf` line.
	pub fn to_line(&self) -> String {
		match self.code_ref {
			Some(i) => format!("{} {}#{} = {}", self.ref_type.code(), self.orig_node, i, self.new_name),
			None => format!("{} {} = {}", self.ref_type.code(), self.orig_node, self.new_name),
		}
	}

	/// jadx: `DeobfPresets.splitAndTrim(l)` -- `l.substring(2).split("=")`.
	pub fn parse_line(line: &str) -> Option<ICodeRename> {
		let l = line.trim();
		if l.is_empty() || l.starts_with('#') || l.len() < 3 {
			return None;
		}
		let ref_type = CodeRefType::from_code(l.chars().next()?)?;
		let rest = &l[2..];
		let mut it = rest.splitn(2, '=');
		let orig = it.next()?.trim();
		let alias = it.next()?.trim();
		if orig.is_empty() || alias.is_empty() {
			return None;
		}
		// `a/b/C#m(I)V#2 = m0a` carries the code ref after the node id
		let (node, code_ref) = split_code_ref(orig);
		Some(ICodeRename { ref_type, orig_node: node.to_string(), new_name: alias.to_string(), code_ref })
	}
}

/// jadx: `ICodeRename extends Comparable<ICodeRename>` -- sorted by node id so a
/// saved map is stable and diffable.
impl Ord for ICodeRename {
	fn cmp(&self, other: &ICodeRename) -> std::cmp::Ordering {
		self.ref_type.cmp(&other.ref_type).then(self.orig_node.cmp(&other.orig_node))
	}
}
impl PartialOrd for ICodeRename {
	fn partial_cmp(&self, other: &ICodeRename) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

/// A node id may end with `#<index>` when the rename targets a variable.
fn split_code_ref(s: &str) -> (&str, Option<u32>) {
	if let Some(pos) = s.rfind('#') {
		if let Ok(v) = s[pos + 1..].parse::<u32>() {
			return (&s[..pos], Some(v));
		}
	}
	(s, None)
}

/// jadx: `jadx.api.data.ICodeComment` (a comment attached to a code node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ICodeComment {
	pub ref_type: CodeRefType,
	pub node: String,
	/// source line the comment refers to; `-1` when the comment has no position
	/// (jadx: `getInsertBeforeLine`)
	pub insert_before_line: i32,
	pub comment: String,
}

impl ICodeComment {
	pub fn to_line(&self) -> String {
		format!("@ {} {} {} {}", self.ref_type.code(), self.node, self.insert_before_line, self.comment)
	}

	pub fn parse_line(line: &str) -> Option<ICodeComment> {
		let l = line.trim().strip_prefix('@')?;
		let mut it = l.splitn(4, ' ').filter(|s| !s.is_empty());
		let ty = CodeRefType::from_code(it.next()?.chars().next()?)?;
		let node = it.next()?.to_string();
		let line_no = it.next()?.parse::<i32>().unwrap_or(-1);
		let comment = it.next().unwrap_or("").to_string();
		Some(ICodeComment { ref_type: ty, node, insert_before_line: line_no, comment })
	}
}

/// Everything a host application has customised about the decompiled code.
///
/// jadx keeps the same two lists in `JadxCodeData` and applies them in
/// `JadxDecompiler.applyCodeData()` *before* writing sources; this port applies
/// them in [`crate::model`], with [`CodeData::apply_class`].
#[derive(Debug, Clone, Default)]
pub struct CodeData {
	pub renames: Vec<ICodeRename>,
	pub comments: Vec<ICodeComment>,
}

impl CodeData {
	pub fn new() -> CodeData {
		CodeData::default()
	}

	pub fn add_rename(&mut self, r: ICodeRename) {
		// jadx's `CodeDataRegistry.addRename` keeps the first value for a node
		if self.find_rename(r.ref_type, &r.orig_node).is_none() {
			self.renames.push(r);
		}
	}

	pub fn add_comment(&mut self, c: ICodeComment) {
		self.comments.push(c);
	}

	pub fn find_rename(&self, ref_type: CodeRefType, orig_node: &str) -> Option<&ICodeRename> {
		self.renames.iter().find(|r| r.ref_type == ref_type && r.orig_node == orig_node)
	}

	/// The alias to print instead of `orig_node`, if the user set one.
	pub fn alias_for(&self, ref_type: CodeRefType, orig_node: &str) -> Option<&str> {
		self.find_rename(ref_type, orig_node).map(|r| r.new_name.as_str())
	}

	pub fn comments_for(&self, node: &str) -> Vec<&ICodeComment> {
		self.comments.iter().filter(|c| c.node == node).collect()
	}

	pub fn is_empty(&self) -> bool {
		self.renames.is_empty() && self.comments.is_empty()
	}

	pub fn len(&self) -> usize {
		self.renames.len() + self.comments.len()
	}

	/// Serialise, in the sorted order jadx uses (`DeobfPresets.save`).
	pub fn to_text(&self) -> String {
		let mut lines: Vec<String> = Vec::with_capacity(self.renames.len());
		for r in &self.renames {
			lines.push(r.to_line());
		}
		lines.sort();
		let mut out = String::new();
		for l in lines {
			out.push_str(&l);
			out.push('\n');
		}
		for c in &self.comments {
			out.push_str(&c.to_line());
			out.push('\n');
		}
		out
	}

	/// Parse a `.jobf` map. Unknown or malformed lines are skipped silently, like
	/// jadx does (`splitAndTrim(l).length != 2 -> continue`).
	pub fn parse(text: &str) -> CodeData {
		let mut data = CodeData::default();
		for line in text.lines() {
			let l = line.trim();
			if l.is_empty() || l.starts_with('#') {
				continue;
			}
			if l.starts_with('@') {
				if let Some(c) = ICodeComment::parse_line(l) {
					data.comments.push(c);
				}
				continue;
			}
			if let Some(r) = ICodeRename::parse_line(l) {
				data.renames.push(r);
			}
		}
		data
	}

	pub fn load(path: impl AsRef<Path>) -> Res<CodeData> {
		let path = path.as_ref();
		let text = std::fs::read_to_string(path).map_err(|e| {
			JadxError::new(ErrorKind::Io, format!("failed to read {}: {}", path.display(), e))
		})?;
		Ok(CodeData::parse(&text))
	}

	pub fn save(&self, path: impl AsRef<Path>) -> Res<()> {
		let path = path.as_ref();
		if self.is_empty() {
			// jadx: "Deobfuscation map is empty, not saving it"
			return Ok(());
		}
		std::fs::write(path, self.to_text()).map_err(|e| {
			JadxError::new(ErrorKind::Io, format!("failed to write {}: {}", path.display(), e))
		})
	}

	/// Rename a printed class description in place (fields and methods by id).
	///
	/// jadx applies renames to the node graph, so every reference is updated
	/// everywhere at once; this port applies them to the printer's model, which is
	/// equivalent for the output because the model is the only thing the printer
	/// reads. The `import`/`package` header of the file follows the class alias
	/// through [`crate::decompile::writer::ClassDef::name`].
	pub fn apply_class(&self, def: &mut crate::decompile::writer::ClassDef) {
		// ids are built from the *original* name, so the class alias must not be
		// applied before the member ids are computed
		let owner = def.name.clone();
		for f in &mut def.fields {
			let id = format!("{}#{}:{}", owner, f.name, f.ty.descriptor());
			if let Some(a) = self.alias_for(CodeRefType::Field, &id) {
				f.name = a.to_string();
			}
		}
		for m in &mut def.methods {
			let args: Vec<crate::types::JType> = m.params.iter().map(|p| p.ty.clone()).collect();
			let id = format!("{}#{}{}", owner, m.name, crate::types::JType::method_descriptor(&args, &m.ret));
			if let Some(a) = self.alias_for(CodeRefType::Method, &id) {
				m.name = a.to_string();
			}
		}
		if let Some(a) = self.alias_for(CodeRefType::Class, &owner) {
			// keep the package, replace the (possibly `Outer$Inner`) tail
			match owner.rfind('/') {
				Some(pos) => def.name = format!("{}/{}", &owner[..pos], a),
				None => def.name = a.to_string(),
			}
		}
	}

	/// The comments to print above a member, given its id.
	pub fn class_comments(&self, owner: &str) -> Vec<&ICodeComment> {
		self.comments.iter().filter(|c| c.node.starts_with(&format!("{}#", owner)) || c.node == owner).collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn jadx_jobf_lines_round_trip() {
		let text = "# generated by jadx\nc a/b/C = C0002a\n\nm a/b/C#run()V = m1run\nf a/b/C#n:I = f0n\n";
		let data = CodeData::parse(text);
		assert_eq!(data.renames.len(), 3);
		assert_eq!(data.alias_for(CodeRefType::Class, "a/b/C"), Some("C0002a"));
		assert_eq!(data.alias_for(CodeRefType::Method, "a/b/C#run()V"), Some("m1run"));
		assert_eq!(data.alias_for(CodeRefType::Field, "a/b/C#n:I"), Some("f0n"));
		let saved = data.to_text();
		let again = CodeData::parse(&saved);
		assert_eq!(again.renames.len(), 3);
		// jadx sorts the lines before writing, so the output is stable
		let mut l: Vec<String> = data.renames.iter().map(|r| r.to_line()).collect();
		l.sort();
		assert_eq!(saved.lines().collect::<Vec<_>>(), l);
	}

	#[test]
	fn variable_renames_keep_their_slot() {
		let r = ICodeRename::new(CodeRefType::Var, "a/b/C#m()V", "value").with_code_ref(3);
		let line = r.to_line();
		assert_eq!(line, "v a/b/C#m()V#3 = value");
		let back = ICodeRename::parse_line(&line).unwrap();
		assert_eq!(back.new_name, "value");
		assert_eq!(back.code_ref, Some(3));
		assert_eq!(back.ref_type, CodeRefType::Var);
	}

	#[test]
	fn comments_are_parsed_and_saved() {
		let mut d = CodeData::new();
		d.add_comment(ICodeComment {
			ref_type: CodeRefType::Method,
			node: "a/b/C#m()V".to_string(),
			insert_before_line: 12,
			comment: "entry point".to_string(),
		});
		let text = d.to_text();
		let back = CodeData::parse(&text);
		assert_eq!(back.comments.len(), 1);
		assert_eq!(back.comments[0].comment, "entry point");
		assert_eq!(back.comments[0].insert_before_line, 12);
	}

	#[test]
	fn garbage_lines_are_skipped() {
		let d = CodeData::parse("hello world\nq x = y\nc only-one-side\n");
		assert!(d.is_empty());
		assert_eq!(d.len(), 0);
	}

	#[test]
	fn first_rename_wins_like_jadx() {
		let mut d = CodeData::new();
		d.add_rename(ICodeRename::new(CodeRefType::Class, "a/b/C", "First"));
		d.add_rename(ICodeRename::new(CodeRefType::Class, "a/b/C", "Second"));
		assert_eq!(d.renames.len(), 1);
		assert_eq!(d.alias_for(CodeRefType::Class, "a/b/C"), Some("First"));
	}
}
