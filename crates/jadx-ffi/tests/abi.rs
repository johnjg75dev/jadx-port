//! The hand-written `include/jadx.h` is the shipped API, so it has to stay in step
//! with the `extern "C"` functions in `src/lib.rs`. This test is what makes that
//! claim checkable in CI without cbindgen.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(p: &Path) -> String {
	std::fs::read_to_string(p).unwrap_or_else(|e| panic!("cannot read {}: {}", p.display(), e))
}

/// Every `#[no_mangle] pub ... extern "C" fn name(...)` in the library.
fn exported_symbols() -> BTreeSet<String> {
	let src = read(&crate_dir().join("src").join("lib.rs"));
	let mut out = BTreeSet::new();
	for line in src.lines() {
		let l = line.trim();
		if !l.contains("extern \"C\" fn ") {
			continue;
		}
		if let Some(rest) = l.split("extern \"C\" fn ").nth(1) {
			let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
			if !name.is_empty() {
				out.insert(name);
			}
		}
	}
	out
}

/// Every `jadx_...(` declared in the header.
fn header_symbols() -> BTreeSet<String> {
	let hdr = read(&crate_dir().join("include").join("jadx.h"));
	let mut out = BTreeSet::new();
	for line in hdr.lines() {
		let l = line.trim();
		// only real declarations, so that `typedef enum jadx_ref_type {` and the
		// doc comments do not read as function names
		if !l.starts_with("JADX_API ") {
			continue;
		}
		// `JADX_API char *JADX_CALL jadx_foo(...)`: the name is the identifier
		// right before the parameter list, which is not necessarily the first
		// `jadx_` in the line (the return type may be `const jadx_class *`).
		let Some(open) = l.find('(') else { continue };
		let before = &l[..open];
		let name: String = before
			.chars()
			.rev()
			.take_while(|c| c.is_alphanumeric() || *c == '_')
			.collect::<Vec<char>>()
			.into_iter()
			.rev()
			.collect();
		if name.starts_with("jadx_") {
			out.insert(name);
		}
	}
	out
}

#[test]
fn every_exported_function_is_declared_in_the_header() {
	let missing: Vec<String> = exported_symbols().difference(&header_symbols()).cloned().collect();
	assert!(missing.is_empty(), "declared in src/lib.rs but missing from include/jadx.h: {:?}", missing);
}

#[test]
fn the_header_declares_nothing_that_does_not_exist() {
	let extra: Vec<String> = header_symbols().difference(&exported_symbols()).cloned().collect();
	assert!(extra.is_empty(), "declared in include/jadx.h but not exported by src/lib.rs: {:?}", extra);
}

#[test]
fn the_export_set_is_the_documented_one() {
	let syms = exported_symbols();
	// a shared library that silently loses `jadx_str_free` or `jadx_ctx_build`
	// breaks every existing caller, so pin the core of the surface
	for core in [
		"jadx_version",
		"jadx_license",
		"jadx_fork_revision",
		"jadx_str_free",
		"jadx_ctx_new",
		"jadx_ctx_free",
		"jadx_ctx_build",
		"jadx_ctx_class_count",
		"jadx_ctx_add_file",
		"jadx_ctx_add_bytes",
		"jadx_class_get_java_source",
		"jadx_class_get_bytecode_disasm",
		"jadx_class_get_smali",
		"jadx_method_get_java_source",
		"jadx_field_get_value",
		"jadx_ctx_load_renames",
		"jadx_ctx_save_renames",
		"jadx_ctx_set_log_callback",
		"jadx_last_error",
	] {
		assert!(syms.contains(core), "export {} disappeared", core);
	}
	assert!(syms.len() > 60, "unexpectedly small export surface: {}", syms.len());
}

#[test]
fn version_constants_are_parseable() {
	let v = jadx_rs::version();
	let parts: Vec<&str> = v.split('.').collect();
	assert_eq!(parts.len(), 3, "version {} must be major.minor.patch", v);
	assert!(parts[0].parse::<u32>().is_ok());
	assert!(jadx_rs::license().contains("Apache"));
	assert_eq!(jadx_rs::fork_revision().len(), 7, "a short commit hash");
}
