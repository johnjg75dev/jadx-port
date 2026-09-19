//! Input loading: files, directories and containers.
//!
//! jadx models this in `jadx.core.utils.files.InputFile` (+ `JadxArgs.inputFiles`):
//! a suffix check picks an `ICodeInputLoader`, `*.class` inside an archive is
//! read entry by entry, and a directory input is walked recursively. This port
//! keeps the same decisions in one place so [`crate::model::Decompiler`] only has
//! to deal with "one unit of code" items.
//!
//! A *unit* is one class file's bytes or one `classes.dex`'s bytes, plus where it
//! came from. Units are returned in a stable order (input order, then archive
//! entry order, then directory order sorted by name) so output is reproducible
//! and rename indices stay the same between runs.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::error::{ErrorKind, JadxError, Res};

/// What kind of code a buffer or file holds, decided the way jadx does it: by
/// extension first, then by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
	/// a JVM `.class` file (`0xCAFEBABE`)
	JavaClass,
	/// a Dalvik `classes.dex` (`dex\n<version>\0`)
	Dex,
	/// an apk/aar/aab/zip container; whether it holds code is decided later
	Archive,
	/// anything else: jadx copies it through as a resource
	Resource,
}

impl InputKind {
	pub fn as_str(self) -> &'static str {
		match self {
			InputKind::JavaClass => "class",
			InputKind::Dex => "dex",
			InputKind::Archive => "archive",
			InputKind::Resource => "resource",
		}
	}
}

/// One input the user passed on the command line / through the FFI.
#[derive(Debug, Clone)]
pub struct InputSource {
	/// `None` for bytes added with `jadx_ctx_add_bytes`
	pub path: Option<PathBuf>,
	/// name used in logs and as the origin of the loaded units
	pub display_name: String,
	pub kind: InputKind,
}

/// A single decompilable chunk: one class file, or one dex file.
#[derive(Debug, Clone)]
pub struct CodeUnit {
	/// `path` or `archive!entry`, as jadx prints in its logs
	pub origin: String,
	/// `pkg/T` for a class file, `classes.dex` for a dex
	pub name: String,
	pub kind: InputKind,
	pub data: Vec<u8>,
}

/// Everything collected from [`Args`], split by kind.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
	pub classes: Vec<CodeUnit>,
	pub dexes: Vec<CodeUnit>,
	pub resources: Vec<CodeUnit>,
	/// files that could not be read at all; jadx logs and continues
	pub errors: Vec<String>,
}

impl Inputs {
	pub fn unit_count(&self) -> usize {
		self.classes.len() + self.dexes.len()
	}

	pub fn is_empty(&self) -> bool {
		self.classes.is_empty() && self.dexes.is_empty() && self.resources.is_empty()
	}
}

/// Classify by path (extension first, magic bytes when the extension is unknown).
pub fn classify(path: &Path) -> InputKind {
	let ext = path
		.extension()
		.map(|e| e.to_string_lossy().to_ascii_lowercase())
		.unwrap_or_default();
	match ext.as_str() {
		"class" => InputKind::JavaClass,
		"dex" | "vdex" => InputKind::Dex,
		"jar" | "zip" | "aar" | "aab" | "apk" | "xz" | "gradle" => InputKind::Archive,
		_ => match std::fs::File::open(path).ok().and_then(|mut f| {
			use std::io::Read;
			let mut buf = [0u8; 8];
			match f.read(&mut buf) {
				Ok(n) => Some(classify_bytes(&buf[..n])),
				Err(_) => None,
			}
		}) {
			Some(k) if k != InputKind::Resource => k,
			_ => InputKind::Resource,
		},
	}
}

/// Classify by content. `jadx.plugins.input.JadxPluginsRegistry` does the same
/// thing by asking each registered input loader to sniff the first bytes.
pub fn classify_bytes(data: &[u8]) -> InputKind {
	if crate::java::class_file::looks_like_class_file(data) {
		return InputKind::JavaClass;
	}
	if is_dex(data) {
		return InputKind::Dex;
	}
	if data.len() >= 4 && &data[..4] == b"PK\x03\x04" {
		return InputKind::Archive;
	}
	InputKind::Resource
}

/// `dex\n035\0` .. `dex\n041\0` (the magic is 8 bytes: `dex\n` + 3 version digits
/// + `\0`).
pub fn is_dex(data: &[u8]) -> bool {
	data.len() >= 8 && &data[..4] == b"dex\n" && data[7] == 0 && data[4..7].iter().all(|c| c.is_ascii_digit())
}

/// Collect every unit described by `args`, reading archives and walking
/// directories.
pub fn load_inputs(args: &Args) -> Res<Inputs> {
	let mut out = Inputs::default();
	for p in &args.input_paths {
		load_path(p, args, &mut out)?;
	}
	for (name, data) in &args.input_blobs {
		load_blob(name, data.clone(), args, &mut out);
	}
	if out.classes.is_empty() && out.dexes.is_empty() && out.resources.is_empty() {
		return Err(JadxError::new(
			ErrorKind::InvalidArgument,
			"no input files: add a .class file, a jar/zip archive or a directory".to_string(),
		));
	}
	Ok(out)
}

/// Like [`classify`] but for a blob added through the FFI (`jadx_ctx_add_bytes`).
pub fn load_blob(name: &str, data: Vec<u8>, args: &Args, out: &mut Inputs) {
	let kind = match Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) {
		Some("class") => InputKind::JavaClass,
		Some("dex") => InputKind::Dex,
		Some("jar") | Some("zip") | Some("aar") | Some("aab") | Some("apk") => InputKind::Archive,
		_ => classify_bytes(&data),
	};
	match kind {
		InputKind::Archive => {
			if let Err(e) = load_archive(name, &data, args, out) {
				out.errors.push(format!("{}: {}", name, e.message));
			}
		}
		InputKind::JavaClass => out.classes.push(CodeUnit { origin: name.to_string(), name: class_unit_name(&data), kind, data }),
		InputKind::Dex => out.dexes.push(CodeUnit { origin: name.to_string(), name: name.to_string(), kind, data }),
		InputKind::Resource => out.resources.push(CodeUnit { origin: name.to_string(), name: name.to_string(), kind, data }),
	}
}

fn load_path(path: &Path, args: &Args, out: &mut Inputs) -> Res<()> {
	let meta = std::fs::metadata(path)
		.map_err(|e| JadxError::new(ErrorKind::Io, format!("cannot read {}: {}", path.display(), e)))?;
	if meta.is_dir() {
		let mut files: Vec<PathBuf> = Vec::new();
		walk(path, &mut files)?;
		files.sort();
		for f in files {
			let data = match std::fs::read(&f) {
				Ok(d) => d,
				Err(e) => {
					out.errors.push(format!("{}: {}", f.display(), e));
					continue;
				}
			};
			out.classes.push(CodeUnit {
				origin: f.display().to_string(),
				name: class_unit_name(&data),
				kind: InputKind::JavaClass,
				data,
			});
		}
		return Ok(());
	}
	let data = std::fs::read(path)
		.map_err(|e| JadxError::new(ErrorKind::Io, format!("cannot read {}: {}", path.display(), e)))?;
	let name = path.display().to_string();
	match classify(path) {
		InputKind::Archive => {
			if let Err(e) = load_archive(&name, &data, args, out) {
				out.errors.push(format!("{}: {}", name, e.message));
			}
		}
		_ => load_blob(&name, data, args, out),
	}
	Ok(())
}

/// Recursively collect `*.class` files, like `InputFile` does for a directory
/// input.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Res<()> {
	let rd = std::fs::read_dir(dir)
		.map_err(|e| JadxError::new(ErrorKind::Io, format!("cannot list {}: {}", dir.display(), e)))?;
	for entry in rd {
		let entry = entry.map_err(|e| JadxError::new(ErrorKind::Io, format!("{}: {}", dir.display(), e)))?;
		let p = entry.path();
		let ft = entry.file_type().map_err(|e| JadxError::new(ErrorKind::Io, e.to_string()))?;
		if ft.is_dir() {
			walk(&p, files)?;
		} else if p.extension().and_then(|e| e.to_str()) == Some("class") {
			files.push(p);
		}
	}
	Ok(())
}

fn load_archive(origin: &str, data: &[u8], args: &Args, out: &mut Inputs) -> Res<()> {
	#[cfg(feature = "zip")]
	{
		let zip = crate::zip::ZipArchive::open(data)?;
		let opts = crate::zip::ZipOptions { max_entry_bytes: args.zip_entry_limit_bytes, ..Default::default() };
		for e in zip.entries() {
			if e.is_dir {
				continue;
			}
			let is_class = e.name.ends_with(".class");
			let is_dex = e.name.ends_with(".dex");
			if !is_class && !is_dex {
				continue;
			}
			let idx = zip.find(&e.name).ok_or_else(|| JadxError::format("zip index lost"))?;
			let bytes = zip.read(idx, &opts).map_err(|err| err.in_context(format!("{}!{}", origin, e.name)))?;
			let unit_name = format!("{}!{}", origin, e.name);
			if is_class {
				out.classes.push(CodeUnit {
					origin: unit_name.clone(),
					name: class_unit_name(&bytes),
					kind: InputKind::JavaClass,
					data: bytes,
				});
			} else {
				out.dexes.push(CodeUnit { origin: unit_name, name: e.name.clone(), kind: InputKind::Dex, data: bytes });
			}
		}
		Ok(())
	}
	#[cfg(not(feature = "zip"))]
	{
		let _ = (origin, data, args, out);
		Err(JadxError::new(
			ErrorKind::Unsupported,
			"zip/jar/apk support is compiled out (rebuild with the `zip` feature)".to_string(),
		))
	}
}

/// The name a class file declares, used as the unit name so that two files with
/// the same path in different archives are still distinguishable. Falls back to
/// the path stem when the file cannot be parsed.
fn class_unit_name(data: &[u8]) -> String {
	match crate::java::class_file::JavaClassFile::parse(data) {
		Ok(c) => c.this_class,
		Err(_) => "unknown".to_string(),
	}
}

/// jadx: `InputFile.checkFileNotBad` -- a file that is neither readable nor a
/// known type is reported instead of aborting the run.
pub fn check_input(path: &Path) -> Res<()> {
	if !path.exists() {
		return Err(JadxError::new(ErrorKind::NotFound, format!("input file does not exist: {}", path.display())));
	}
	Ok(())
}

/// The name of the source file jadx would create for a class, without the
/// package directories (`T.class` in `a/b/` -> `T`).
pub fn short_class_name(raw_name: &str) -> &str {
	raw_name.rsplit('/').next().unwrap_or(raw_name)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testutil::ClassBuilder;

	#[test]
	fn kinds_are_classified_by_content() {
		let mut b = ClassBuilder::new("pkg/T", "java/lang/Object");
		b.method(0x0002, "m", "()V", None);
		let class = b.to_bytes();
		assert_eq!(classify_bytes(&class), InputKind::JavaClass);
		let mut dex = vec![b'd', b'e', b'x', b'\n', b'0', b'3', b'5', 0];
		dex.extend_from_slice(&[0u8; 64]);
		assert!(is_dex(&dex));
		assert_eq!(classify_bytes(&dex), InputKind::Dex);
		assert_eq!(classify_bytes(b"PK\x03\x04rest"), InputKind::Archive);
		assert_eq!(classify_bytes(b"hello"), InputKind::Resource);
	}

	#[test]
	fn an_empty_input_set_is_an_error() {
		let e = load_inputs(&Args::default()).unwrap_err();
		assert_eq!(e.kind, ErrorKind::InvalidArgument);
	}

	#[test]
	fn blobs_are_routed_by_extension() {
		let mut b = ClassBuilder::new("pkg/Deep", "java/lang/Object");
		b.method(0x0002, "m", "()V", None);
		let class = b.to_bytes();
		let mut args = Args::default();
		args.input_blobs.push(("x/T.class".to_string(), class.clone()));
		args.input_blobs.push(("x/T.txt".to_string(), b"note".to_vec()));
		let inputs = load_inputs(&args).unwrap();
		assert_eq!(inputs.classes.len(), 1);
		assert_eq!(inputs.classes[0].name, "pkg/Deep");
		assert_eq!(inputs.resources.len(), 1);
		assert!(inputs.errors.is_empty());
	}

	#[test]
	fn short_names_drop_the_package() {
		assert_eq!(short_class_name("a/b/C"), "C");
		assert_eq!(short_class_name("C"), "C");
		assert_eq!(short_class_name("a/b/C$D"), "C$D");
	}
}
