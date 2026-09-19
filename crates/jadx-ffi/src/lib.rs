//! C ABI of the jadx-rs decompiler: the `jadx.dll` / `libjadx.so` exports.
//!
//! Style (agreed for this port): **opaque handles + flat functions**. Every
//! function is `extern "C"`, no C++ types, no lifetimes, no panics crossing the
//! boundary. The shape follows jadx's own Java API one level down: a context plays
//! the role of `JadxDecompiler`, a `jadx_class*` handle is a `JavaClass`, and the
//! member getters below mirror `JavaMethod`/`JavaField`.
//!
//! ## Conventions
//!
//! * **Handles.** `jadx_ctx*` is a real pointer to a box allocated here. Class
//!   handles are *index handles*: the pointer is `index + 1` cast to
//!   `const jadx_class*`, which makes them copyable, cheap, and never dangling --
//!   they are valid as long as the context exists and is not rebuilt. A context is
//!   not thread-safe: use one context per thread (jadx has the same property, its
//!   `JadxDecompiler` is not shared either).
//! * **Strings.** Functions ending in `_get_*` that return `char *` hand over an
//!   owned NUL-terminated UTF-8 string which the caller must release with
//!   [`jadx_str_free`]. Strings returned by `jadx_last_error`, `jadx_version` and
//!   friends are borrowed from the library and must **not** be freed.
//! * **Errors.** `i32` results are `0` for success and `-code` for failure, where
//!   `code` is [`error::ErrorKind::code`] of the jadx-rs crate (1 = io, 2 = input
//!   format, 3 = unsupported, 4 = invalid argument, 5 = not found, 6 = incomplete,
//!   7 = internal). The human-readable message of the last failure of this context
//!   is available from [`jadx_last_error`].
//! * **Panics.** every entry point runs inside `catch_unwind`; a panic becomes
//!   error code 7 (internal) instead of unwinding into the caller's frame.
//! * **Buffers.** `jadx_out_buf` parameters are `char**`; the library allocates and
//!   the caller frees. `jadx_str_dup` exists for callers that want to keep a
//!   borrowed string beyond the next call.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use jadx_rs::code_data::{CodeRefType, ICodeComment, ICodeRename};
use jadx_rs::error::ErrorKind;
use jadx_rs::model::{JavaClass, Progress};
use jadx_rs::Decompiler;

/// Version of the shared library; matches `jadx-rs`' Cargo version.
const VERSION: &str = jadx_rs::version();

/// A `CString` that lives forever, so a borrowed `const char *` can be handed to
/// the caller without any ownership question. `OnceLock` initialises it on first
/// use; `std::sync::OnceLock` is why this crate needs Rust 1.70.
fn borrowed(s: &str) -> *const c_char {
	// one leaked allocation per distinct constant string -- three in total for
	// this library -- which is what makes the returned pointer safe to keep
	static CACHE: std::sync::Mutex<Vec<CString>> = std::sync::Mutex::new(Vec::new());
	let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
	if let Some(existing) = guard.iter().find(|c| c.to_str().unwrap_or("") == s) {
		return existing.as_ptr();
	}
	guard.push(CString::new(s.replace('\0', " ")).unwrap_or_default());
	guard.last().map(|c| c.as_ptr()).unwrap_or(std::ptr::null())
}

/// Opaque context handle (jadx: `JadxDecompiler`).
#[repr(C)]
pub struct jadx_ctx {
	dec: Decompiler,
	last_error: Option<String>,
	last_error_code: i32,
	/// kept so `jadx_ctx_save_all` can write the sources it printed
	out_dir: PathBuf,
}

impl jadx_ctx {
	fn fail(&mut self, kind: ErrorKind, msg: impl Into<String>) {
		let msg = msg.into();
		self.last_error_code = -(kind.code());
		self.last_error = Some(msg);
	}
}

/// Opaque class handle (jadx: `JavaClass`).
///
/// This type is never constructed: as the module docs explain, a `*const
/// jadx_class` handed to the caller is just `index + 1` reinterpreted, and the
/// library only reads the numeric value back, never the memory behind it. That
/// keeps class handles copyable, cheap and impossible to dangle.
#[repr(C)]
pub struct jadx_class {
	/// the class index inside the context, `handle as usize - 1`
	_index: usize,
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

/// Read a caller-owned C string. The lifetime is whatever the caller promises,
/// which is why every use below copies into a `String` before the pointer can
/// go away.
unsafe fn to_str<'a>(p: *const c_char) -> Option<&'a str> {
	if p.is_null() {
		return None;
	}
	CStr::from_ptr(p).to_str().ok()
}

unsafe fn to_bytes<'a>(p: *const u8, len: usize) -> &'a [u8] {
	if p.is_null() || len == 0 {
		return &[];
	}
	std::slice::from_raw_parts(p, len)
}

fn owned(s: &str) -> *mut c_char {
	match CString::new(s) {
		Ok(c) => c.into_raw(),
		// a NUL inside the string (a class could be named `a\0b`) -- truncate at
		// the first NUL instead of failing the whole call
		Err(_) => {
			let cut = s.find('\0').unwrap_or(s.len());
			match CString::new(&s[..cut]) {
				Ok(c) => c.into_raw(),
				Err(_) => std::ptr::null_mut(),
			}
		}
	}
}

fn class_handle(index: usize) -> *const jadx_class {
	(index + 1) as *const jadx_class
}

fn class_index(p: *const jadx_class) -> Option<usize> {
	let v = p as usize;
	if v == 0 {
		None
	} else {
		Some(v - 1)
	}
}

/// Run `f`, turning any panic into an internal error on the context.
fn guard<F: FnOnce(&mut jadx_ctx)>(ctx: *mut jadx_ctx, f: F) -> i32 {
	let code = catch_unwind(AssertUnwindSafe(|| {
		let Some(c) = (unsafe { ctx.as_mut() }) else {
			return -ErrorKind::InvalidArgument.code();
		};
		c.last_error = None;
		c.last_error_code = 0;
		f(c);
		// a `fail` inside `f` is reported through the return code too
		c.last_error_code
	}));
	match code {
		Ok(v) => v,
		Err(_) => -ErrorKind::Internal.code(),
	}
}

/// The same, for calls that answer with a string (`char *`).
fn guard_str<F: FnOnce(&mut jadx_ctx) -> Option<String>>(ctx: *mut jadx_ctx, f: F) -> *mut c_char {
	let res = catch_unwind(AssertUnwindSafe(|| match unsafe { ctx.as_mut() } {
		Some(c) => {
			c.last_error = None;
			c.last_error_code = 0;
			f(c).map(Ok).unwrap_or_else(|| Err((ErrorKind::NotFound, "no such entry".to_string())))
		}
		None => Err((ErrorKind::InvalidArgument, "null context".to_string())),
	}));
	match res {
		Ok(Ok(s)) => owned(&s),
		Ok(Err((kind, msg))) => {
			if let Some(c) = unsafe { ctx.as_mut() } {
				c.fail(kind, msg);
			}
			std::ptr::null_mut()
		}
		Err(_) => {
			if let Some(c) = unsafe { ctx.as_mut() } {
				c.fail(ErrorKind::Internal, "panic inside jadx-rs");
			}
			std::ptr::null_mut()
		}
	}
}

fn ref_type(code: i32) -> Option<CodeRefType> {
	Some(match code {
		0 => CodeRefType::Package,
		1 => CodeRefType::Class,
		2 => CodeRefType::Field,
		3 => CodeRefType::Method,
		4 => CodeRefType::MthArg,
		5 => CodeRefType::Var,
		6 => CodeRefType::Catch,
		7 => CodeRefType::Insn,
		_ => return None,
	})
}

// ---------------------------------------------------------------------------
// library information / memory
// ---------------------------------------------------------------------------

/// Library version, e.g. `0.1.0` (jadx: `VersionInfo.getVersion`).
#[no_mangle]
pub extern "C" fn jadx_version() -> *const c_char {
	borrowed(VERSION)
}

#[no_mangle]
pub extern "C" fn jadx_version_major() -> i32 {
	VERSION.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_version_minor() -> i32 {
	VERSION.split('.').nth(1).and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_version_patch() -> i32 {
	VERSION
		.split('.')
		.nth(2)
		.unwrap_or("0")
		.split(|c: char| !c.is_ascii_digit())
		.next()
		.and_then(|v| v.parse().ok())
		.unwrap_or(0)
}

/// SPDX id of this port's license (jadx is Apache-2.0 as well).
#[no_mangle]
pub extern "C" fn jadx_license() -> *const c_char {
	borrowed(jadx_rs::license())
}

/// The upstream jadx commit this port's parsing tables were taken from; lets a
/// host tell how current the reader is.
#[no_mangle]
pub extern "C" fn jadx_fork_revision() -> *const c_char {
	borrowed(jadx_rs::fork_revision())
}

/// Release a string returned by any `*_get_*` function of this library.
///
/// # Safety
/// `p` must be a pointer returned by this library (or null).
#[no_mangle]
pub unsafe extern "C" fn jadx_str_free(p: *mut c_char) {
	if p.is_null() {
		return;
	}
	drop(CString::from_raw(p));
}

/// Copy a borrowed string into one the caller owns (and must free).
///
/// # Safety
/// `p` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn jadx_str_dup(p: *const c_char) -> *mut c_char {
	match to_str(p) {
		Some(s) => owned(s),
		None => std::ptr::null_mut(),
	}
}

// ---------------------------------------------------------------------------
// context lifetime
// ---------------------------------------------------------------------------

/// Create a context. Returns null only if the allocation failed.
#[no_mangle]
pub extern "C" fn jadx_ctx_new() -> *mut jadx_ctx {
	Box::into_raw(Box::new(jadx_ctx {
		dec: Decompiler::default(),
		last_error: None,
		last_error_code: 0,
		out_dir: PathBuf::new(),
	}))
}

/// # Safety
/// `ctx` must come from [`jadx_ctx_new`] and not have been freed yet.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_free(ctx: *mut jadx_ctx) {
	if ctx.is_null() {
		return;
	}
	drop(Box::from_raw(ctx));
}

/// Message of the last failure on this context; borrowed, valid until the next
/// call on the same context. Returns null when the last call succeeded.
///
/// # Safety
/// `ctx` must be a live context.
#[no_mangle]
pub unsafe extern "C" fn jadx_last_error(ctx: *mut jadx_ctx) -> *const c_char {
	let Some(c) = ctx.as_ref() else {
		return std::ptr::null();
	};
	match &c.last_error {
		Some(s) => s.as_ptr() as *const c_char,
		None => std::ptr::null(),
	}
}

/// Error code of the last failure (negative, as returned by the calls), or 0.
///
/// # Safety
/// `ctx` must be a live context.
#[no_mangle]
pub unsafe extern "C" fn jadx_last_error_code(ctx: *mut jadx_ctx) -> i32 {
	match ctx.as_ref() {
		Some(c) => c.last_error_code,
		None => -ErrorKind::InvalidArgument.code(),
	}
}

/// Reset the collected errors and the loaded code, keeping the configuration.
#[no_mangle]
pub extern "C" fn jadx_ctx_reset(ctx: *mut jadx_ctx) -> i32 {
	guard(ctx, |c| {
		let args = c.dec.args.clone();
		c.dec = Decompiler::new(args);
		c.out_dir = PathBuf::new();
	})
}

// ---------------------------------------------------------------------------
// arguments (jadx: `JadxArgs`)
// ---------------------------------------------------------------------------

/// Set a string-valued option. `key` is the jadx option name in camelCase, so a
/// host can forward its own configuration map without a recompile; unknown keys
/// are reported as `InvalidArgument`.
///
/// # Safety
/// `key` and `value` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_set_option(ctx: *mut jadx_ctx, key: *const c_char, value: *const c_char) -> i32 {
	let key = to_str(key).unwrap_or("");
	let value = to_str(value).unwrap_or("");
	guard(ctx, move |c| {
		let args = &mut c.dec.args;
		let ok = match key {
			"outDir" | "output-dir" => {
				args.out_dir = PathBuf::from(value);
				c.out_dir = PathBuf::from(value);
				true
			}
			"outDirSrc" => {
				args.out_dir_src = PathBuf::from(value);
				true
			}
			"useImports" => {
				args.use_imports = parse_bool(value);
				true
			}
			"debugInfo" => {
				args.debug_info = parse_bool(value);
				true
			}
			"printLineNumbers" => {
				args.print_line_numbers = parse_bool(value);
				true
			}
			"escapeUnicode" => {
				args.escape_unicode = parse_bool(value);
				true
			}
			"showInconsistentCode" => {
				args.show_inconsistent_code = parse_bool(value);
				true
			}
			"deobfuscationOn" => {
				args.deobfuscation_on = parse_bool(value);
				true
			}
			"decompilationMode" => {
				args.decompilation_mode = match value {
					"restructure" => jadx_rs::DecompilationMode::Restructure,
					"simple" => jadx_rs::DecompilationMode::Simple,
					"fallback" => jadx_rs::DecompilationMode::Fallback,
					_ => jadx_rs::DecompilationMode::Auto,
				};
				true
			}
			"outputFormat" => {
				args.output_format = match value {
					"json" => jadx_rs::OutputFormat::Json,
					_ => jadx_rs::OutputFormat::Java,
				};
				true
			}
			"commentsLevel" => {
				args.comments_level = match value {
					"error" => jadx_rs::CommentsLevel::Error,
					"warn" => jadx_rs::CommentsLevel::Warn,
					"info" => jadx_rs::CommentsLevel::Info,
					_ => jadx_rs::CommentsLevel::None,
				};
				true
			}
			"integerFormat" => {
				args.integer_format = match value {
					"decimal" => jadx_rs::IntegerFormat::Decimal,
					"hex" | "hexadecimal" => jadx_rs::IntegerFormat::Hexadecimal,
					_ => jadx_rs::IntegerFormat::Auto,
				};
				true
			}
			"classFilter" => {
				args.class_filter = Some(value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
				true
			}
			"codeIndent" => {
				args.code_indent = value.to_string();
				true
			}
			_ => false,
		};
		if !ok {
			c.fail(ErrorKind::InvalidArgument, format!("unknown option: {}", key));
		}
	})
}

/// Set a numeric option (`int`/`bool` valued, like the ones above but typed).
#[no_mangle]
pub extern "C" fn jadx_ctx_set_option_int(ctx: *mut jadx_ctx, key: *const c_char, value: i64) -> i32 {
	let key = unsafe { to_str(key) }.unwrap_or("");
	guard(ctx, move |c| {
		let args = &mut c.dec.args;
		let ok = match key {
			"threadsCount" => {
				args.threads_count = value.max(1) as usize;
				true
			}
			"deobfuscationMinLength" => {
				args.deobfuscation_min_length = value.max(0) as usize;
				true
			}
			"deobfuscationMaxLength" => {
				args.deobfuscation_max_length = value.max(1) as usize;
				true
			}
			"maxStructuringDepth" => {
				args.max_structuring_depth = value.max(0) as usize;
				true
			}
			"zipEntryLimitBytes" => {
				args.zip_entry_limit_bytes = value.max(1) as u64;
				true
			}
			"useDebugVarsNames" => {
				args.use_debug_var_names = value != 0;
				true
			}
			"cfgOutput" => {
				args.cfg_output = value != 0;
				true
			}
			"skipResources" => {
				args.skip_resources = value != 0;
				true
			}
			"inlineAnonymousClasses" => {
				args.inline_anonymous_classes = value != 0;
				true
			}
			_ => false,
		};
		if !ok {
			c.fail(ErrorKind::InvalidArgument, format!("unknown numeric option: {}", key));
		}
	})
}

fn parse_bool(v: &str) -> bool {
	matches!(v, "1" | "true" | "yes" | "on")
}

// ---------------------------------------------------------------------------
// inputs
// ---------------------------------------------------------------------------

/// Add an input file or directory (jadx: `addInput(File)`).
///
/// # Safety
/// `path` must be a valid NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_add_file(ctx: *mut jadx_ctx, path: *const c_char) -> i32 {
	let Some(p) = to_str(path) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let p = p.to_string();
	guard(ctx, move |c| match Path::new(&p).exists() {
		true => c.dec.add_file(&p),
		false => c.fail(ErrorKind::NotFound, format!("input file does not exist: {}", p)),
	})
}

/// Add a class file, jar or dex from memory (`jadx.api`: jadx only reads files, so
/// this is an addition of the port; `name` decides the type, e.g. `a/b/C.class`).
///
/// # Safety
/// `name` must be a valid string and `data` a readable buffer of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_add_bytes(ctx: *mut jadx_ctx, name: *const c_char, data: *const u8, len: usize) -> i32 {
	let Some(n) = to_str(name) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let n = n.to_string();
	let bytes = to_bytes(data, len).to_vec();
	guard(ctx, move |c| c.dec.add_bytes(n, bytes))
}

/// Load and decompile everything added so far. Must be called before any
/// `jadx_class_*` function.
#[no_mangle]
pub extern "C" fn jadx_ctx_build(ctx: *mut jadx_ctx) -> i32 {
	guard(ctx, |c| {
		if let Err(e) = c.dec.build() {
			c.fail(e.kind, e.message);
		}
	})
}

/// Number of classes after a build. 0 is a legitimate answer, so use
/// [`jadx_ctx_class_at`] (null = out of range) to distinguish it from an error.
#[no_mangle]
pub extern "C" fn jadx_ctx_class_count(ctx: *mut jadx_ctx) -> usize {
	match unsafe { ctx.as_ref() } {
		Some(c) => catch_unwind(AssertUnwindSafe(|| c.dec.class_count())).unwrap_or(0),
		None => 0,
	}
}

/// Class handle by index, null when out of range.
#[no_mangle]
pub extern "C" fn jadx_ctx_class_at(ctx: *mut jadx_ctx, index: usize) -> *const jadx_class {
	match unsafe { ctx.as_ref() } {
		Some(c) if index < c.dec.class_count() => class_handle(index),
		_ => std::ptr::null(),
	}
}

/// Class by name, accepting both `a.b.C` and `a/b/C`.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_find_class(ctx: *mut jadx_ctx, name: *const c_char) -> *const jadx_class {
	let n = to_str(name).unwrap_or("");
	let n = n.to_string();
	match ctx.as_ref() {
		Some(c) => match c.dec.find_class(&n) {
			Some(cls) => class_handle(index_of(c.dec.classes(), cls)),
			None => std::ptr::null(),
		},
		None => std::ptr::null(),
	}
}

fn index_of(classes: &[JavaClass], cls: &JavaClass) -> usize {
	classes.iter().position(|c| std::ptr::eq(c.inner().as_ptr(), cls.inner().as_ptr())).unwrap_or(usize::MAX)
}

/// Number of packages (`jadx.api`: `getPackages().size()`).
#[no_mangle]
pub extern "C" fn jadx_ctx_package_count(ctx: *mut jadx_ctx) -> usize {
	match unsafe { ctx.as_ref() } {
		Some(c) => catch_unwind(AssertUnwindSafe(|| c.dec.packages().len())).unwrap_or(0),
		None => 0,
	}
}

/// Name of package `index` (owned; free with [`jadx_str_free`]).
#[no_mangle]
pub extern "C" fn jadx_ctx_package_name(ctx: *mut jadx_ctx, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| c.dec.packages().get(index).map(|p| p.name.clone()))
}

/// Number of classes in package `index`.
#[no_mangle]
pub extern "C" fn jadx_ctx_package_class_count(ctx: *mut jadx_ctx, index: usize) -> usize {
	match unsafe { ctx.as_ref() } {
		Some(c) => c.dec.packages().get(index).map(|p| p.classes().len()).unwrap_or(0),
		None => 0,
	}
}

/// Class `cls_index` of package `index`, as an index handle of the context.
#[no_mangle]
pub extern "C" fn jadx_ctx_package_class(ctx: *mut jadx_ctx, index: usize, cls_index: usize) -> *const jadx_class {
	let cls = match unsafe { ctx.as_ref() } {
		Some(c) => match c.dec.packages().get(index).and_then(|p| p.classes().get(cls_index)) {
			Some(cls) => cls.clone(),
			None => return std::ptr::null(),
		},
		None => return std::ptr::null(),
	};
	match unsafe { ctx.as_ref() } {
		Some(c) => class_handle(index_of(c.dec.classes(), &cls)),
		None => std::ptr::null(),
	}
}

/// Error count of the whole run (bad inputs are reported, never fatal).
#[no_mangle]
pub extern "C" fn jadx_ctx_error_count(ctx: *mut jadx_ctx) -> usize {
	match unsafe { ctx.as_ref() } {
		Some(c) => c.dec.errors().len(),
		None => 0,
	}
}

/// Error `index` of the run (owned).
#[no_mangle]
pub extern "C" fn jadx_ctx_error_at(ctx: *mut jadx_ctx, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| c.dec.errors().get(index).cloned())
}

/// Write every decompiled class into `dir`, mirroring `jadx -d dir`.
///
/// # Safety
/// `dir` must be a valid NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_save_source_to_dir(ctx: *mut jadx_ctx, dir: *const c_char) -> i32 {
	let Some(d) = to_str(dir) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let d = PathBuf::from(d);
	guard(ctx, move |c| {
		c.out_dir = d.clone();
		if let Err(e) = c.dec.save_to_dir(&d) {
			c.fail(e.kind, e.message);
		}
	})
}

/// The whole run as JSON (jadx: `--output-format json`), owned.
#[no_mangle]
pub extern "C" fn jadx_ctx_to_json(ctx: *mut jadx_ctx) -> *mut c_char {
	guard_str(ctx, move |c| Some(c.dec.to_json()))
}

// ---------------------------------------------------------------------------
// classes
// ---------------------------------------------------------------------------

/// Field `name` (owned).
///
/// # Safety
/// `cls` must be a handle from this context.
#[no_mangle]
pub extern "C" fn jadx_class_get_name(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.name().to_string()))
}

#[no_mangle]
pub extern "C" fn jadx_class_get_full_name(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.full_name()))
}

#[no_mangle]
pub extern "C" fn jadx_class_get_package(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.package().to_string()))
}

#[no_mangle]
pub extern "C" fn jadx_class_get_file_name(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.file_name().to_string()))
}

/// Where the class came from (`path`, `archive!entry` or the blob name).
#[no_mangle]
pub extern "C" fn jadx_class_get_origin(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.origin().to_string()))
}

/// The decompiled source, `package` and `import` header included.
#[no_mangle]
pub extern "C" fn jadx_class_get_java_source(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.java_source().to_string()))
}

/// JVM bytecode disassembly (for dex inputs this is the smali listing, since
/// there is no JVM code).
#[no_mangle]
pub extern "C" fn jadx_class_get_bytecode_disasm(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.bytecode_disasm().to_string()))
}

/// Smali listing of a dex class; empty string for JVM classes.
#[no_mangle]
pub extern "C" fn jadx_class_get_smali(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.smali().to_string()))
}

/// Imports selected for this file, one per line, no trailing newline (owned).
#[no_mangle]
pub extern "C" fn jadx_class_get_imports(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).map(|k| k.imports().join("\n")))
}

#[no_mangle]
pub extern "C" fn jadx_class_get_access_flags(ctx: *mut jadx_ctx, cls: *const jadx_class) -> u32 {
	unsafe { ctx.as_ref() }.and_then(|c| class_of(&c.dec, cls).map(|k| k.access_flags())).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_class_is_interface(ctx: *mut jadx_ctx, cls: *const jadx_class) -> i32 {
	flag(ctx, cls, |k| k.is_interface())
}

#[no_mangle]
pub extern "C" fn jadx_class_is_enum(ctx: *mut jadx_ctx, cls: *const jadx_class) -> i32 {
	flag(ctx, cls, |k| k.is_enum())
}

/// `1` when this class came from a `classes.dex` (and therefore has no Java
/// source from this port).
#[no_mangle]
pub extern "C" fn jadx_class_is_dex_input(ctx: *mut jadx_ctx, cls: *const jadx_class) -> i32 {
	flag(ctx, cls, |k| k.is_dex_input())
}

fn flag<F: Fn(&JavaClass) -> bool>(ctx: *mut jadx_ctx, cls: *const jadx_class, f: F) -> i32 {
	match unsafe { ctx.as_ref() } {
		Some(c) => match class_of(&c.dec, cls) {
			Some(k) if f(k) => 1,
			_ => 0,
		},
		None => 0,
	}
}

fn class_of<'a>(dec: &'a Decompiler, cls: *const jadx_class) -> Option<&'a JavaClass> {
	class_index(cls).and_then(|i| dec.classes().get(i))
}

#[no_mangle]
pub extern "C" fn jadx_class_method_count(ctx: *mut jadx_ctx, cls: *const jadx_class) -> usize {
	unsafe { ctx.as_ref() }.and_then(|c| class_of(&c.dec, cls).map(|k| k.methods().len())).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_class_field_count(ctx: *mut jadx_ctx, cls: *const jadx_class) -> usize {
	unsafe { ctx.as_ref() }.and_then(|c| class_of(&c.dec, cls).map(|k| k.fields().len())).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_class_error_count(ctx: *mut jadx_ctx, cls: *const jadx_class) -> usize {
	unsafe { ctx.as_ref() }.and_then(|c| class_of(&c.dec, cls).map(|k| k.errors().len())).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_class_error_at(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| class_of(&c.dec, cls).and_then(|k| k.errors().get(index).cloned()))
}

/// `method_index` of a class member, or -1 when no method of that name exists.
///
/// # Safety
/// `name` must be a valid NUL-terminated method name (no descriptor).
#[no_mangle]
pub unsafe extern "C" fn jadx_class_method_index_by_name(ctx: *mut jadx_ctx, cls: *const jadx_class, name: *const c_char) -> i64 {
	let n = to_str(name).unwrap_or("").to_string();
	match ctx.as_ref().and_then(|c| class_of(&c.dec, cls)) {
		Some(k) => match k.methods().iter().position(|m| m.name() == n) {
			Some(i) => i as i64,
			None => -1,
		},
		None => -1,
	}
}

/// # Safety
/// `name` must be a valid NUL-terminated field name.
#[no_mangle]
pub unsafe extern "C" fn jadx_class_field_index_by_name(ctx: *mut jadx_ctx, cls: *const jadx_class, name: *const c_char) -> i64 {
	let n = to_str(name).unwrap_or("").to_string();
	match ctx.as_ref().and_then(|c| class_of(&c.dec, cls)) {
		Some(k) => match k.fields().iter().position(|f| f.name() == n) {
			Some(i) => i as i64,
			None => -1,
		},
		None => -1,
	}
}

// ---------------------------------------------------------------------------
// methods and fields
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn jadx_method_get_name(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.name().to_string()))
}

/// Raw JVM descriptor, e.g. `(II)I`.
#[no_mangle]
pub extern "C" fn jadx_method_get_descriptor(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.descriptor().to_string()))
}

/// `a/b/C#m(II)I`, the id a rename entry uses.
#[no_mangle]
pub extern "C" fn jadx_method_get_id(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.def_id()))
}

#[no_mangle]
pub extern "C" fn jadx_method_get_return_type(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.return_type()))
}

/// Decompile just this method and return its source (declaration + body).
#[no_mangle]
pub extern "C" fn jadx_method_get_java_source(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.java_source()))
}

#[no_mangle]
pub extern "C" fn jadx_method_get_bytecode_disasm(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_method(&c.dec, cls, index, |m| m.bytecode_disasm()))
}

#[no_mangle]
pub extern "C" fn jadx_method_get_access_flags(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> u32 {
	unsafe { ctx.as_ref() }.and_then(|c| with_method(&c.dec, cls, index, |m| m.access_flags())).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn jadx_method_is_constructor(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> i32 {
	match unsafe { ctx.as_ref() } {
		Some(c) => with_method(&c.dec, cls, index, |m| if m.is_constructor() { 1 } else { 0 }).unwrap_or(0),
		None => 0,
	}
}

fn with_method<R, F: Fn(&jadx_rs::JavaMethod) -> R>(dec: &Decompiler, cls: *const jadx_class, index: usize, f: F) -> Option<R> {
	class_of(dec, cls).map(|k| k.methods()).and_then(|v| v.get(index)).map(f)
}

#[no_mangle]
pub extern "C" fn jadx_field_get_name(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_field(&c.dec, cls, index, |f| f.name().to_string()))
}

#[no_mangle]
pub extern "C" fn jadx_field_get_type(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_field(&c.dec, cls, index, |f| f.type_name()))
}

#[no_mangle]
pub extern "C" fn jadx_field_get_descriptor(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_field(&c.dec, cls, index, |f| f.descriptor()))
}

#[no_mangle]
pub extern "C" fn jadx_field_get_id(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_field(&c.dec, cls, index, |f| f.def_id()))
}

/// `ConstantValue` of a `static final` field, as it would appear in source
/// (`42`, `'a'`, `3.14f`); null when the field has none.
#[no_mangle]
pub extern "C" fn jadx_field_get_value(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| with_field(&c.dec, cls, index, |f| f.value().unwrap_or_default()))
}

#[no_mangle]
pub extern "C" fn jadx_field_get_access_flags(ctx: *mut jadx_ctx, cls: *const jadx_class, index: usize) -> u32 {
	unsafe { ctx.as_ref() }.and_then(|c| with_field(&c.dec, cls, index, |f| f.access_flags())).unwrap_or(0)
}

fn with_field<R, F: Fn(&jadx_rs::JavaField) -> R>(dec: &Decompiler, cls: *const jadx_class, index: usize, f: F) -> Option<R> {
	class_of(dec, cls).map(|k| k.fields()).and_then(|v| v.get(index)).map(f)
}

// ---------------------------------------------------------------------------
// code data: renames and comments (jadx: `JadxCodeData`, `DeobfPresets`)
// ---------------------------------------------------------------------------

/// Add a rename: `type` follows `jadx.api.data.CodeRefType` numbering used by
/// this library (0 package, 1 class, 2 field, 3 method, 4 argument, 5 variable,
/// 6 catch, 7 instruction), `node` is the raw id (`a/b/C`, `a/b/C#m(I)V`,
/// `a/b/C#f:I`) and `alias` the name to print.
///
/// # Safety
/// `node` and `alias` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_add_rename(ctx: *mut jadx_ctx, kind: i32, node: *const c_char, alias: *const c_char) -> i32 {
	let (Some(t), Some(n), Some(a)) = (ref_type(kind), to_str(node), to_str(alias)) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let (n, a) = (n.to_string(), a.to_string());
	guard(ctx, move |c| {
		c.dec.code_data_mut().add_rename(ICodeRename::new(t, n, a));
	})
}

/// Add a comment to be printed next to `node`. `line` is the source line to
/// insert before, or -1 for "no position".
///
/// # Safety
/// `node` and `text` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_add_comment(ctx: *mut jadx_ctx, kind: i32, node: *const c_char, line: i32, text: *const c_char) -> i32 {
	let (Some(t), Some(n), Some(x)) = (ref_type(kind), to_str(node), to_str(text)) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let (n, x) = (n.to_string(), x.to_string());
	guard(ctx, move |c| {
		c.dec.code_data_mut().add_comment(ICodeComment { ref_type: t, node: n, insert_before_line: line, comment: x });
	})
}

/// Load a `.jobf` rename map (the format of jadx's `DeobfPresets`), merging it
/// into the current code data. Must be called before [`jadx_ctx_build`].
///
/// # Safety
/// `path` must be a valid NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_load_renames(ctx: *mut jadx_ctx, path: *const c_char) -> i32 {
	let Some(p) = to_str(path) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let p = p.to_string();
	guard(ctx, move |c| match jadx_rs::CodeData::load(&p) {
		Ok(loaded) => {
			let mut merged = loaded;
			for r in std::mem::take(&mut c.dec.code_data.renames) {
				merged.add_rename(r);
			}
			for cm in std::mem::take(&mut c.dec.code_data.comments) {
				merged.add_comment(cm);
			}
			c.dec.set_code_data(merged);
		}
		Err(e) => c.fail(e.kind, e.message),
	})
}

/// Save the current renames as a `.jobf` map, so jadx itself can read them.
///
/// # Safety
/// `path` must be a valid NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_save_renames(ctx: *mut jadx_ctx, path: *const c_char) -> i32 {
	let Some(p) = to_str(path) else {
		return -ErrorKind::InvalidArgument.code();
	};
	let p = p.to_string();
	guard(ctx, move |c| {
		if let Err(e) = c.dec.code_data().save(&p) {
			c.fail(e.kind, e.message);
		}
	})
}

/// Number of rename entries currently loaded.
#[no_mangle]
pub extern "C" fn jadx_ctx_rename_count(ctx: *mut jadx_ctx) -> usize {
	unsafe { ctx.as_ref() }.map(|c| c.dec.code_data().renames.len()).unwrap_or(0)
}

/// Rename entry `index` as `kind\tnode\talias` (owned) -- handy for a UI list.
#[no_mangle]
pub extern "C" fn jadx_ctx_rename_at(ctx: *mut jadx_ctx, index: usize) -> *mut c_char {
	guard_str(ctx, move |c| {
		c.dec.code_data().renames.get(index).map(|r| {
			let kind = match r.ref_type {
				CodeRefType::Package => 0,
				CodeRefType::Class => 1,
				CodeRefType::Field => 2,
				CodeRefType::Method => 3,
				CodeRefType::MthArg => 4,
				CodeRefType::Var => 5,
				CodeRefType::Catch => 6,
				CodeRefType::Insn => 7,
			};
			format!("{}\t{}\t{}", kind, r.orig_node, r.new_name)
		})
	})
}

// ---------------------------------------------------------------------------
// callbacks
// ---------------------------------------------------------------------------

/// Receives one log line (jadx: slf4j output).
pub type jadx_log_cb = Option<unsafe extern "C" fn(user: *mut std::ffi::c_void, line: *const c_char)>;

/// A C function pointer plus its user data. Raw pointers are not `Send`/`Sync`,
/// and the sinks in [`crate::model`] require both, so the pair gets the impls
/// here: the caller is responsible for the callback's own thread safety, exactly
/// as in any C API.
#[derive(Clone, Copy)]
struct RawSink {
	code: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const c_char)>,
	user: *mut std::ffi::c_void,
}
unsafe impl Send for RawSink {}
unsafe impl Sync for RawSink {}

#[derive(Clone, Copy)]
struct RawProgress {
	code: Option<unsafe extern "C" fn(*mut std::ffi::c_void, usize, usize, *const c_char)>,
	user: *mut std::ffi::c_void,
}
unsafe impl Send for RawProgress {}
unsafe impl Sync for RawProgress {}
/// Receives build progress (jadx: `IProgressListener`).
pub type jadx_progress_cb = Option<unsafe extern "C" fn(user: *mut std::ffi::c_void, done: usize, total: usize, what: *const c_char)>;

/// Install a log sink. `user` is passed back verbatim; the callback must not call
/// back into this context (the data is borrowed while the call runs).
///
/// # Safety
/// The callbacks must be valid for the lifetime of the context, and `user` must
/// stay alive as long as it is not replaced.
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_set_log_callback(ctx: *mut jadx_ctx, cb: jadx_log_cb, user: *mut std::ffi::c_void) -> i32 {
	let sink = RawSink { code: cb, user };
	guard(ctx, move |c| {
		c.dec.set_log(match sink.code {
			Some(f) => {
				let s = sink;
				Box::new(move |line: &str| {
					let text = CString::new(line.replace('\0', " ")).unwrap_or_default();
					if let Some(f) = s.code {
						f(s.user, text.as_ptr());
					}
				})
			}
			None => Box::new(|_line: &str| {}),
		});
	})
}

/// # Safety
/// See [`jadx_ctx_set_log_callback`].
#[no_mangle]
pub unsafe extern "C" fn jadx_ctx_set_progress_callback(ctx: *mut jadx_ctx, cb: jadx_progress_cb, user: *mut std::ffi::c_void) -> i32 {
	let sink = RawProgress { code: cb, user };
	guard(ctx, move |c| {
		c.dec.set_progress(match sink.code {
			Some(_f) => {
				let s = sink;
				Box::new(move |p: &Progress| {
					let text = CString::new(p.what.replace('\0', " ")).unwrap_or_default();
					if let Some(f) = s.code {
						f(s.user, p.done, p.total, text.as_ptr());
					}
				})
			}
			None => Box::new(|_p: &Progress| {}),
		});
	})
}

// ---------------------------------------------------------------------------
// inputs helpers (stateless)
// ---------------------------------------------------------------------------

/// Classify a file by extension and magic: 0 class, 1 dex, 2 archive, 3
/// resource (jadx picks the input loader by sniffing the same way).
///
/// # Safety
/// `path` must be a valid NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn jadx_classify_file(path: *const c_char) -> i32 {
	let p = to_str(path).unwrap_or("");
	match to_kind(jadx_rs::model::kind_of_path(p)) {
		Some(v) => v,
		None => -1,
	}
}

/// Classify a buffer: 0 class, 1 dex, 2 archive, 3 resource.
///
/// # Safety
/// `data` must be a readable buffer of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn jadx_classify_bytes(data: *const u8, len: usize) -> i32 {
	let bytes = to_bytes(data, len);
	to_kind(jadx_rs::model::kind_of_bytes(bytes)).unwrap_or(-1)
}

fn to_kind(k: jadx_rs::InputKind) -> Option<i32> {
	Some(match k {
		jadx_rs::InputKind::JavaClass => 0,
		jadx_rs::InputKind::Dex => 1,
		jadx_rs::InputKind::Archive => 2,
		jadx_rs::InputKind::Resource => 3,
	})
}

// ---------------------------------------------------------------------------
// code data for one class (jadx: `ICodeData`)
// ---------------------------------------------------------------------------

/// The `jadx.api.data.ICodeData` view of one class: a YAML-free text dump of the
/// class, its fields and its methods with their ids, so a host can build rename
/// or comment entries without knowing the internals. Owned, one entry per line:
/// `c <id>` / `f <id>` / `m <id>`.
#[no_mangle]
pub extern "C" fn jadx_class_get_code_data(ctx: *mut jadx_ctx, cls: *const jadx_class) -> *mut c_char {
	guard_str(ctx, move |c| {
		class_of(&c.dec, cls).map(|k| {
			let mut out = String::new();
			out.push_str(&format!("c {}\n", k.name()));
			for i in 0..k.fields().len() {
				out.push_str(&format!("f {}\n", k.field_id(i)));
			}
			for i in 0..k.methods().len() {
				out.push_str(&format!("m {}\n", k.method_id(i)));
			}
			out
		})
	})
}
