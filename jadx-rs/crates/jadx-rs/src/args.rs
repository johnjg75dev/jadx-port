//! Decompiler options: a port of `jadx.api.JadxArgs`.
//!
//! Field names and defaults mirror the Java class one-to-one where this port
//! honours the option. Options that only make sense for the full jadx pipeline
//! (plugin loading, gradle export, usage info caches, ...) are accepted so the C
//! ABI stays source-compatible with scripts written against jadx, but they are
//! listed in [`Args::ignored_options`] and documented as no-ops -- see
//! `docs/PORT_STATUS.md`.

use std::path::PathBuf;

/// How integer literals are printed (jadx: `jadx.api.args.IntegerFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerFormat {
	/// decimal, except for values where hex is clearer (bit masks, `0x`-friendly)
	Auto,
	Decimal,
	Hexadecimal,
}

impl IntegerFormat {
	pub fn is_hexadecimal(self) -> bool {
		matches!(self, IntegerFormat::Hexadecimal)
	}

	pub fn from_str_name(s: &str) -> Option<IntegerFormat> {
		match s.to_ascii_uppercase().as_str() {
			"AUTO" => Some(IntegerFormat::Auto),
			"DECIMAL" => Some(IntegerFormat::Decimal),
			"HEXADECIMAL" | "HEX" => Some(IntegerFormat::Hexadecimal),
			_ => None,
		}
	}
}

/// How much structure the code writer tries to restore
/// (jadx: `jadx.api.DecompilationMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompilationMode {
	/// Best effort: restructure when possible, keep `goto` otherwise
	Auto,
	/// Always restore structured control flow
	Restructure,
	/// Linear code with explicit labels and `goto`s
	Simple,
	/// Raw instructions, no transformations
	Fallback,
}

impl DecompilationMode {
	/// jadx: `isSpecial()` -- `Simple`/`Fallback` skip most of the pipeline.
	pub fn is_special(self) -> bool {
		matches!(self, DecompilationMode::Simple | DecompilationMode::Fallback)
	}

	pub fn from_str_name(s: &str) -> Option<DecompilationMode> {
		match s.to_ascii_uppercase().as_str() {
			"AUTO" => Some(DecompilationMode::Auto),
			"RESTRUCTURE" => Some(DecompilationMode::Restructure),
			"SIMPLE" => Some(DecompilationMode::Simple),
			"FALLBACK" => Some(DecompilationMode::Fallback),
			_ => None,
		}
	}
}

/// Which comments the writer is allowed to print (jadx: `jadx.api.CommentsLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommentsLevel {
	None,
	UserOnly,
	Error,
	Warn,
	Info,
	Debug,
}

impl CommentsLevel {
	/// jadx: `filter(limit)`
	pub fn allowed(self, limit: CommentsLevel) -> bool {
		(self as u8) <= (limit as u8)
	}
}

/// All options that influence the decompiled output.
///
/// `Default` matches jadx's defaults, with two deliberate exceptions noted
/// inline: `escape_unicode` stays `true` because a shared library has no
/// terminal to protect, and `debug_info` is `true` like jadx.
#[derive(Debug, Clone)]
pub struct Args {
	/// input files, in the order they were added (jadx: `inputFiles`)
	pub input_paths: Vec<PathBuf>,
	/// raw inputs added through the FFI (bytes, no filesystem path)
	pub input_blobs: Vec<(String, Vec<u8>)>,

	pub out_dir: PathBuf,
	pub out_dir_src: PathBuf,
	pub out_dir_res: PathBuf,

	/// jadx: `threadsCount`; this port decompiles every class on the caller's
	/// thread, so the value is only used to size the in-memory cache.
	pub threads_count: usize,

	/// print the CFG of each method as a comment (jadx: `cfgOutput`)
	pub cfg_output: bool,
	/// jadx: `rawCFGOutput`
	pub raw_cfg_output: bool,
	/// jadx: `showInconsistentCode` -- emit partially decompiled bodies with an
	/// error comment instead of failing the whole class.
	pub show_inconsistent_code: bool,

	/// jadx: `useImports`
	pub use_imports: bool,
	/// jadx: `debugInfo` -- use `LocalVariableTable` names and `LineNumberTable`
	pub debug_info: bool,
	/// jadx: `insertDebugLines` -- emit `// [line: N]` markers
	pub print_line_numbers: bool,
	/// jadx: `extractFinally` -- duplicate `finally` blocks the way javac emits them
	pub extract_finally: bool,
	/// jadx: `inlineAnonymousClasses`; NOT ported, anonymous classes are printed
	/// as separate top-level classes like `jadx --no-inline-anonymous`
	pub inline_anonymous_classes: bool,
	/// jadx: `inlineMethods`; only the trivial `return this.a;` accessor case is
	/// attempted (see `decompile::passes`)
	pub inline_methods: bool,
	/// jadx: `moveInnerClasses`
	pub move_inner_classes: bool,

	/// No jadx counterpart: the largest entry this port will inflate out of a
	/// jar/zip, in bytes. jadx gets this for free from `java.util.zip`; a shared
	/// library needs its own limit so that a hostile archive cannot exhaust memory.
	pub zip_entry_limit_bytes: u64,

	pub skip_resources: bool,
	pub skip_sources: bool,

	/// jadx: `classFilter` -- only classes whose full name matches are decompiled
	pub class_filter: Option<Vec<String>>,
	/// jadx: `deobfuscationOn`
	pub deobfuscation_on: bool,
	/// jadx: `deobfuscationMinLength`
	pub deobfuscation_min_length: usize,
	/// jadx: `deobfuscationMaxLength`
	pub deobfuscation_max_length: usize,
	/// jadx: `deobfuscationWhitelist`; entries ending in `.*` match a package
	pub deobfuscation_whitelist: Vec<String>,

	/// jadx: `escapeUnicode`
	pub escape_unicode: bool,
	/// jadx: `replaceConsts`
	pub replace_consts: bool,
	/// jadx: `respectBytecodeAccModifiers`
	pub respect_bytecode_acc_modifiers: bool,
	/// jadx: `restoreSwitchOverString`
	pub restore_switch_over_string: bool,
	/// jadx: `fsCaseSensitive`
	pub fs_case_sensitive: bool,

	/// jadx: `outputFormat`
	pub output_format: OutputFormat,
	/// jadx: `decompilationMode`
	pub decompilation_mode: DecompilationMode,
	/// jadx: `commentsLevel`
	pub comments_level: CommentsLevel,
	/// jadx: `integerFormat`
	pub integer_format: IntegerFormat,
	/// jadx: `codeNewLineStr`
	pub code_new_line: String,
	/// jadx: `codeIndentStr`; this port uses tabs for its own output and honours
	/// the string for the *number of columns* a tab represents.
	pub code_indent: String,
	/// jadx: `useDebugVarsNames` -- when false, synthetic names (`i3$`) win
	pub use_debug_var_names: bool,
	/// jadx: `skipFilesSave`
	pub skip_files_save: bool,
	/// No jadx counterpart: how deep the CFG structuring pass may nest before it
	/// gives up and emits labels + `goto`s (jadx has the same idea as a fixed
	/// limit inside `RegionMaker`). `0` means unlimited.
	pub max_structuring_depth: usize,
	/// jadx: `typeUpdatesLimitCount` -- kept for ABI parity, no SSA so unused
	pub type_updates_limit_count: usize,
}

/// jadx: `JadxArgs.OutputFormatEnum`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
	Java,
	Json,
}

impl Default for Args {
	fn default() -> Args {
		Args {
			input_paths: Vec::new(),
			input_blobs: Vec::new(),
			out_dir: PathBuf::from("jadx-output"),
			out_dir_src: PathBuf::from("sources"),
			out_dir_res: PathBuf::from("resources"),
			threads_count: default_threads_count(),
			cfg_output: false,
			raw_cfg_output: false,
			show_inconsistent_code: false,
			use_imports: true,
			debug_info: true,
			print_line_numbers: false,
			extract_finally: true,
			inline_anonymous_classes: true,
			inline_methods: true,
			move_inner_classes: true,
			zip_entry_limit_bytes: 64 * 1024 * 1024,
			skip_resources: false,
			skip_sources: false,
			class_filter: None,
			deobfuscation_on: false,
			deobfuscation_min_length: 0,
			deobfuscation_max_length: usize::MAX,
			deobfuscation_whitelist: vec![
				"android.*".to_string(),
				"androidx.*".to_string(),
				"java.*".to_string(),
				"javax.*".to_string(),
				"jdk.*".to_string(),
				"sun.*".to_string(),
				"com.sun.*".to_string(),
				"kotlin.*".to_string(),
				"kotlinx.*".to_string(),
				"scala.*".to_string(),
			],
			// unlike jadx: a library consumer gets the text through a callback or a
			// file, not a terminal, so escaping is always safe and lossless
			escape_unicode: true,
			replace_consts: true,
			respect_bytecode_acc_modifiers: false,
			restore_switch_over_string: true,
			fs_case_sensitive: cfg!(not(windows)),
			output_format: OutputFormat::Java,
			decompilation_mode: DecompilationMode::Auto,
			comments_level: CommentsLevel::Info,
			integer_format: IntegerFormat::Auto,
			code_new_line: "\n".to_string(),
			code_indent: "    ".to_string(),
			use_debug_var_names: true,
			skip_files_save: false,
			max_structuring_depth: 0,
			type_updates_limit_count: 10,
		}
	}
}

impl Args {
	/// jadx: `DEFAULT_THREADS_COUNT`
	pub fn new() -> Args {
		Args::default()
	}

	/// Add an input file or directory (jadx: `addInput` semantics are the
	/// caller's job; this port just records the path).
	pub fn add_input<P: Into<PathBuf>>(&mut self, p: P) {
		self.input_paths.push(p.into());
	}

	/// Add an in-memory archive or class file, named `file_name` for logs.
	pub fn add_input_bytes<S: Into<String>>(&mut self, file_name: S, data: Vec<u8>) {
		self.input_blobs.push((file_name.into(), data));
	}

	/// `true` when `full_name` (source form, e.g. `pkg.Outer.Inner`) passes
	/// `class_filter`. Empty/absent filter accepts everything. Entries ending in
	/// `.*` match a package prefix, as in jadx's `--class-filter`.
	pub fn class_accepted(&self, full_name: &str) -> bool {
		let list = match self.class_filter.as_ref() {
			Some(l) if !l.is_empty() => l,
			_ => return true,
		};
		list.iter().any(|pat| {
			if let Some(prefix) = pat.strip_suffix(".*") {
				full_name.starts_with(&format!("{}.", prefix)) || full_name == prefix
			} else {
				full_name == pat
			}
		})
	}

	/// jadx: `codeIndentStr`; converted to the tab stop used by the writer
	/// (`--skip-inline-deep-anon-classes-let` style nesting is unaffected).
	pub fn indent_columns(&self) -> usize {
		let n = self.code_indent.chars().count();
		if n == 0 {
			4
		} else {
			n
		}
	}

	/// Human-readable list of jadx options this port accepts but does not
	/// implement; surfaced by `jadx_args_describe` in the FFI and in the docs.
	pub const fn ignored_options() -> &'static [&'static str] {
		&[
			"codeCache",
			"usageInfoCache",
			"codeWriterProvider",
			"aliasProvider",
			"renameCondition",
			"userRenamesMappingsPath",
			"userRenamesMappingsMode",
			"generatedRenamesMappingFile",
			"generatedRenamesMappingFileMode",
			"resourceNameSource",
			"useSourceNameAsClassNameAlias",
			"sourceNameRepeatLimit",
			"exportGradleType",
			"skipXmlPrettyPrint",
			"allowInlineKotlinLambda",
			"useKotlinMethodsForVarNames",
			"filesGetter",
			"security",
			"securityEnabled",
			"pluginLoader",
			"processedResources",
			"checkRenamesData",
			"debugMode",
			"useDxInput",
		]
	}

	/// `true` when the code writer should print `0x..` literals.
	pub fn print_hex_ints(&self) -> bool {
		self.integer_format.is_hexadecimal()
	}
}

fn default_threads_count() -> usize {
	std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_match_jadx() {
		let a = Args::default();
		assert_eq!(a.out_dir, PathBuf::from("jadx-output"));
		assert!(a.use_imports);
		assert!(a.debug_info);
		assert!(!a.print_line_numbers);
		assert!(!a.deobfuscation_on);
		assert_eq!(a.integer_format, IntegerFormat::Auto);
		assert_eq!(a.decompilation_mode, DecompilationMode::Auto);
		assert_eq!(a.code_indent, "    ");
		assert_eq!(a.type_updates_limit_count, 10);
		assert!(!a.decompilation_mode.is_special());
		assert!(DecompilationMode::Simple.is_special());
	}

	#[test]
	fn integer_format_parsing() {
		assert_eq!(IntegerFormat::from_str_name("hex"), Some(IntegerFormat::Hexadecimal));
		assert_eq!(IntegerFormat::from_str_name("DECIMAL"), Some(IntegerFormat::Decimal));
		assert_eq!(IntegerFormat::from_str_name("nope"), None);
		assert!(IntegerFormat::Hexadecimal.is_hexadecimal());
		assert!(!IntegerFormat::Auto.is_hexadecimal());
	}

	#[test]
	fn comments_level_filter_is_inclusive() {
		assert!(CommentsLevel::Error.allowed(CommentsLevel::Info));
		assert!(CommentsLevel::Info.allowed(CommentsLevel::Info));
		assert!(!CommentsLevel::Debug.allowed(CommentsLevel::Info));
	}

	#[test]
	fn class_filter_accepts_by_prefix_and_exact() {
		let mut a = Args::default();
		assert!(a.class_accepted("anything.AtAll"));
		a.class_filter = Some(vec!["pkg.Outer.Inner".to_string(), "other.*".to_string()]);
		assert!(a.class_accepted("pkg.Outer.Inner"));
		assert!(!a.class_accepted("pkg.Outer.Inner2"));
		assert!(a.class_accepted("other.Pkg"));
		assert!(a.class_accepted("other"));
		assert!(!a.class_accepted("unrelated.Cls"));
	}

	#[test]
	fn inputs_are_recorded_in_order() {
		let mut a = Args::default();
		a.add_input("a.jar");
		a.add_input_bytes("b.class", vec![1, 2, 3]);
		assert_eq!(a.input_paths.len(), 1);
		assert_eq!(a.input_blobs.len(), 1);
		assert_eq!(a.input_blobs[0].0, "b.class");
		assert_eq!(a.indent_columns(), 4);
	}
}
