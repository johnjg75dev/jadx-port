//! The library as a command line: decompile `.class` files, directories and
//! jars/zip archives.
//!
//! ```text
//! cargo run -p jadx-rs --example decompile -- --help
//! cargo run -p jadx-rs --example decompile -- target/classes
//! cargo run -p jadx-rs --example decompile -- --no-imports -o out app.jar
//! ```
//!
//! The flags mirror the real `jadx` CLI for the subset this port implements, and
//! an unknown flag is an error rather than a silent no-op.

use std::path::{Path, PathBuf};

use jadx_rs::model::Progress;
use jadx_rs::{Args, CodeData, Decompiler};

fn usage() -> &'static str {
	"usage: decompile [options] <input>...\n\
	 \n\
	 options:\n\
	 \t-o, --output-dir <dir>   write <dir>/<package>/<Class>.java (default: print)\n\
	 \t--no-imports             fully qualify type names instead of importing them\n\
	 \t--no-debug-info          ignore LocalVariableTable/LineNumberTable names\n\
	 \t--print-lines            emit `// [line: N]` markers\n\
	 \t--show-bad-code          print partially decompiled methods instead of a note\n\
	 \t--escape-unicode         write \\uXXXX for non-ASCII string chars (on by default)\n\
	 \t--no-escape-unicode      print them as they are\n\
	 \t--deobf                  rename short names, as `jadx --deobf`\n\
	 \t--deobf-min <n>          rename names shorter than <n>\n\
	 \t--renames <file.jobf>    load a jadx rename map before decompiling\n\
	 \t--save-renames <file>    write the generated renames afterwards\n\
	 \t--json                   print the run as JSON instead of sources\n\
	 \t--disasm                 print bytecode disassembly instead of Java\n\
	 \t-v, --verbose            log each step to stderr\n\
	 \t-h, --help               this text\n"
}

struct Opt {
	out: Option<PathBuf>,
	json: bool,
	disasm: bool,
	save_renames: Option<PathBuf>,
	load_renames: Option<PathBuf>,
	verbose: bool,
}

fn main() {
	std::process::exit(match run() {
		Ok(code) => code,
		Err(e) => {
			eprintln!("error: {}", e);
			1
		}
	});
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
	let mut args = Args::default();
	let mut opt = Opt { out: None, json: false, disasm: false, save_renames: None, load_renames: None, verbose: false };
	let mut inputs: Vec<String> = Vec::new();

	let mut it = std::env::args().skip(1);
	// a macro rather than a closure: the closure would have to hold a mutable
	// borrow of `it` across the `it.next()` in the loop condition
	macro_rules! need {
		($flag:expr) => {
			it.next().ok_or_else(|| format!("{} needs a value", $flag))?
		};
	}
	while let Some(a) = it.next() {
		match a.as_str() {
			"-h" | "--help" => {
				print!("{}", usage());
				return Ok(0);
			}
			"-o" | "--output-dir" => {
				let p = PathBuf::from(need!("--output-dir"));
				args.out_dir = p.clone();
				opt.out = Some(p);
			}
			"--no-imports" => args.use_imports = false,
			"--no-debug-info" => args.debug_info = false,
			"--print-lines" => args.print_line_numbers = true,
			"--show-bad-code" => args.show_inconsistent_code = true,
			"--escape-unicode" => args.escape_unicode = true,
			"--no-escape-unicode" => args.escape_unicode = false,
			"--deobf" => args.deobfuscation_on = true,
			"--deobf-min" => args.deobfuscation_min_length = need!("--deobf-min").parse()?,
			"--renames" => opt.load_renames = Some(PathBuf::from(need!("--renames"))),
			"--save-renames" => opt.save_renames = Some(PathBuf::from(need!("--save-renames"))),
			"--json" => opt.json = true,
			"--disasm" => opt.disasm = true,
			"-v" | "--verbose" => opt.verbose = true,
			other if other.starts_with('-') => return Err(format!("unknown option: {}\n{}", other, usage()).into()),
			other => inputs.push(other.to_string()),
		}
	}
	if inputs.is_empty() {
		print!("{}", usage());
		return Err("no input given".into());
	}
	args.input_paths = inputs.iter().map(PathBuf::from).collect();

	let mut d = Decompiler::new(args);
	if let Some(p) = &opt.load_renames {
		let data = CodeData::load(p.as_path())?;
		d.set_code_data(data);
	}
	if opt.verbose {
		d.set_log(Box::new(|line: &str| eprintln!("[jadx] {}", line)));
	}
	d.set_progress(Box::new(move |p: &Progress| {
		let percent = if p.total == 0 {
			100
		} else {
			(p.done.saturating_mul(100)) / p.total
		};
		eprintln!("[{}%] {}", percent, p.what);
	}));
	d.build()?;

	if let Some(p) = &opt.save_renames {
		d.code_data().save(p.as_path())?;
		eprintln!("renames written to {}", p.display());
	}
	if opt.json {
		print!("{}", d.to_json());
		return Ok(0);
	}
	let mut files = 0usize;
	for c in d.classes() {
		if opt.disasm {
			print!("{}", c.bytecode_disasm());
			continue;
		}
		match &opt.out {
			Some(dir) => {
				let path = c.save_to_dir(dir)?;
				eprintln!("wrote {}", path.display());
				files += 1;
			}
			None => println!("{}", c.java_source()),
		}
	}
	for e in d.errors() {
		eprintln!("problem: {}", e);
	}
	if files > 0 {
		eprintln!("{} file(s) written", files);
	}
	Ok(if d.class_count() == 0 && !d.errors().is_empty() { 1 } else { 0 })
}
