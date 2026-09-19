//! # jadx-rs
//!
//! A Rust port of [jadx](https://github.com/skylot/jadx) -- a Java/Dalvik
//! bytecode reader and decompiler -- built to be shipped as a shared library
//! (`jadx.dll` on Windows, `libjadx.so` on Linux) with a flat C ABI.
//!
//! ## What is here
//!
//! * [`java`] -- a complete JVM class file reader: constant pool, opcodes with a
//!   semantic classification ([`java::insn::Sem`]), attributes (including
//!   `Code`, `StackMapTable`, `LocalVariableTable`), generic signatures and
//!   disassembly ([`java::disasm`]).
//! * [`decompile`] -- the decompiler itself: [`decompile::cfg`] builds the
//!   control-flow graph, [`decompile::emulate`] turns the operand stack into
//!   expressions, [`decompile::structure`] recovers `if`/`while`/`switch`/`try`
//!   regions, [`decompile::writer`] prints Java source, and
//!   [`decompile::passes`] holds the post-rewrites.
//! * [`dex`] (feature `dex`) -- full `classes.dex` parsing plus smali output. A
//!   DEX *to Java* pipeline is phase 2; DEX classes are enumerated and
//!   disassembled, and the reason decompilation is unavailable is reported.
//! * [`zip`] (feature `zip`) -- a dependency-free zip/deflate reader, so `.jar`,
//!   `.aar`, `.apk` and `.zip` inputs work without any crates.io dependency.
//! * [`input`], [`model`], [`code_data`], [`deobf`] -- the pieces of
//!   `jadx.api` a host application needs: file loading, the class/method/field
//!   view, user renames and comments, and the deobfuscator's alias scheme.
//! * [`args::Args`] -- the equivalent of `JadxArgs`.
//!
//! ## Zero dependencies
//!
//! `Cargo.toml` has an empty `[dependencies]` section and this crate is
//! `#![forbid(unsafe_code)]`: it uses only `std`. That is what makes the crate
//! buildable in an air-gapped environment and safe to embed in a `.so`/`.dll`
//! that other languages load.
//!
//! ## Relationship to jadx
//!
//! jadx is ~110k lines of Java in `jadx-core` alone. This port deliberately
//! reproduces the *behaviour* that a decompiler host needs -- parse, model,
//! decompile, print -- for the constructs javac and kotlinc emit, and documents
//! each gap. See `docs/PORT_STATUS.md` for the package-by-package table; the
//! short version:
//!
//! * jadx's IR and SSA pipeline (`JadxCodeReader`, `SsaBuildAndCheckPass`,
//!   `RegionMaker`, ~30 `*Processors`) is replaced by stack emulation plus CFG
//!   structuring, because Dalvik SSA does not exist for JVM bytecode.
//! * `inlineAnonymousClasses`, `synchronized` region reconstruction from
//!   `monitorenter`, and pre-1.6 `jsr`/`ret` `finally` handling are not ported;
//!   each prints a comment where it would have applied instead of miscompiling.
//! * Kotlin/Groovy/Android-specific passes (resource loading, `R` ids, gradle
//!   export) are out of scope.
//!
//! ## Library use
//!
//! ```no_run
//! use jadx_rs::model::Decompiler;
//!
//! let mut d = Decompiler::default();
//! d.add_file("target/classes/pkg/T.class");
//! d.build()?;
//! for class in d.classes() {
//!     println!("{}", class.java_source());
//! }
//! # Ok::<(), jadx_rs::error::JadxError>(())
//! ```
//!
//! ## C ABI
//!
//! The `jadx-ffi` crate next to this one exports the same surface as opaque
//! handles (`jadx_ctx`, `jadx_class`, ...) with `extern "C"` functions; see
//! `include/jadx.h`. Strings are returned as owned `char *` and must be released
//! with `jadx_str_free`. Every call returns `0` on success or a negative
//! [`error::ErrorKind`] code, retrievable with `jadx_last_error`.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod access_flags;
pub mod args;
pub mod code_data;
pub mod deobf;
pub mod decompile;
pub mod error;
pub mod input;
pub mod io;
pub mod java;
pub mod model;
pub mod types;

#[cfg(feature = "dex")]
pub mod dex;
#[cfg(feature = "zip")]
pub mod zip;

/// Test-only class file assembler, public so that integration tests in the
/// workspace and downstream `cargo test` runs can build fixtures without a JDK.
#[doc(hidden)]
pub mod testutil;

// Flat re-exports, mirroring the `jadx.api` package a Java caller imports from.
pub use crate::args::{Args, CommentsLevel, DecompilationMode, IntegerFormat, OutputFormat};
pub use crate::code_data::{CodeData, CodeRefType, ICodeRename};
pub use crate::deobf::Deobf;
pub use crate::decompile::structure::decompile_method_body;
pub use crate::decompile::writer::ClassSource;
pub use crate::error::{ErrorKind, JadxError, Res};
pub use crate::input::{classify, InputKind, InputSource};
pub use crate::java::class_file::JavaClassFile;
pub use crate::model::{Decompiler, JavaClass, JavaField, JavaMethod, JavaPackage};
pub use crate::types::JType;

/// Crate version, taken from `Cargo.toml` (jadx: `VersionInfo.getVersion`).
pub const fn version() -> &'static str {
	env!("CARGO_PKG_VERSION")
}

/// The license of this port. jadx itself is Apache-2.0 as well.
pub const fn license() -> &'static str {
	"Apache-2.0"
}

/// The upstream jadx commit this port was derived from; surfaced by
/// `jadx_fork_revision()` in the C ABI so a caller can tell how current the
/// parsing tables are.
pub const fn fork_revision() -> &'static str {
	"2fb1b16"
}
