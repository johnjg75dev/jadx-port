# jadx-rs — a Rust port of jadx, shipped as a shared library

`jadx-rs` is a pure-Rust reader and decompiler for Java bytecode, built to be
embedded as `libjadx.so` / `jadx.dll` behind a flat C ABI. It follows
[skylot/jadx](https://github.com/skylot/jadx): the same concepts, the same file
formats, the same output style — reimplemented here without a JVM and without a
single crates.io dependency.

> **Status: source-complete, not yet compiled.**
> Every file in this tree was checked with a Rust *parser* (syntax) but there is
> no Rust toolchain in the environment where it was written, so nothing has been
> type-checked, borrowed or linked. `cargo build` on a real toolchain — or the
> [`jadx-rs` workflow](../.github/workflows/jadx-rs.yml), which builds all three
> shared-library targets and runs the tests — is the first authoritative check.
> Expect first-build compile errors; `docs/PORT_STATUS.md` explains what is
> covered and what is deliberately stubbed.

## Scope (what was agreed, and what was not)

| area | decision |
|------|----------|
| breadth | a **working subset**: real end-to-end decompilation for the constructs javac/kotlinc emit (parse → IR → structuring → Java source) plus a C ABI that mirrors jadx's `jadx.api` surface. A 1:1 port of the ~110k lines of `jadx-core` was rejected as unfeasible; unported passes are documented stubs, not silent gaps. |
| bytecode | **JVM `.class`/`.jar`/`.zip` in depth**: full class-file reading and real Java output. **DEX/APK**: fully parsed (header, id sections, class data, code items) with member enumeration and smali disassembly; DEX→Java decompilation is phase 2 and the library says so in the output and through `jadx_class_is_dex_input`. |
| dependencies | **zero**: `std` only, `#![forbid(unsafe_code)]` in `jadx-rs`, so `cargo build --offline` works in an air-gapped build (including our own inflate/zip reader). |
| ABI style | **opaque handles + flat `extern "C"` functions**, owned `char *` freed by `jadx_str_free`, log/progress callbacks, `i32` error codes plus `jadx_last_error`. Usable from C, C++, C#/P-Invoke, Python/ctypes, Kotlin/JNI-thin, etc. |

## Layout

```
jadx-rs/
├── Cargo.toml                     workspace (2 members, no dependencies)
├── rustfmt.toml                   hard_tabs = true
├── build.sh                       linux / windows-gnu / msvc helper
├── tools/gen_dex_table.py         regenerates the DEX opcode table from jadx
├── docs/PORT_STATUS.md            jadx package -> port status table
└── crates/
    ├── jadx-rs/                   the library (safe Rust, std only)
    │   ├── src/error.rs           JadxError/ErrorKind, the codes the C ABI returns
    │   ├── src/io.rs              BinReader (BE/LE, uleb128/sleb128)
    │   ├── src/types.rs           JType, descriptors, method prototypes
    │   ├── src/access_flags.rs    ACC_* flags and their Java keywords
    │   ├── src/args.rs            Args (jadx: JadxArgs)
    │   ├── src/java/              class file: const pool, opcodes, attributes,
    │   │                          generic signatures, disassembly
    │   ├── src/decompile/         cfg → emulate → structure → passes → writer
    │   ├── src/dex/               classes.dex reader + smali (generated tables)
    │   ├── src/zip.rs             zip container + RFC1951 inflate
    │   ├── src/input.rs           file/dir/archive loading and classification
    │   ├── src/model.rs           Decompiler, JavaClass/Method/Field/Package
    │   ├── src/code_data.rs       renames + comments, jadx's .jobf format
    │   ├── src/deobf.rs           --deobf name generation (DeobfAliasProvider)
    │   ├── src/testutil.rs        hand-assembler for class-file fixtures
    │   ├── examples/              decompile.rs (CLI), gen_class.rs (fixture)
    │   └── tests/jvm_end_to_end.rs
    └── jadx-ffi/                  the C ABI
        ├── src/lib.rs             73 `extern "C"` exports
        ├── include/jadx.h         hand-written, documented header (the real API)
        ├── cbindgen.toml          for diffing a generated header, optional
        ├── tests/abi.rs           header ⇄ exports consistency check
        └── examples/              decompile.c, decompile.py (ctypes)
```

## Build

```sh
cd jadx-rs
cargo build --release -p jadx-ffi            # target/release/libjadx.so | jadx.dll
cargo test  --workspace --all-features       # unit + integration tests
./build.sh linux                             # or: windows-gnu | windows-msvc | all
```

MSRV is 1.70 (`std::sync::OnceLock` in the FFI crate); the library crate itself
needs nothing beyond `std`.

### Windows

* MSVC: `cargo build --release -p jadx-ffi --target x86_64-pc-windows-msvc`
  produces `jadx.dll` plus `jadx.lib` (import library).
* MinGW-w64: `--target x86_64-pc-windows-gnu` needs `gcc`/`dlltool` from an
  MSYS2 `mingw64` on `PATH`; produces `jadx.dll` plus `jadx.dll.a`.
  The GNU build is what makes the crate usable from toolchains without MSVC.

## Using the C API

```c
#include "jadx.h"

jadx_ctx *ctx = jadx_ctx_new();
jadx_ctx_set_option(ctx, "outDir", "out");
jadx_ctx_set_option(ctx, "showInconsistentCode", "true");
jadx_ctx_add_file(ctx, "app.jar");        /* .class, directory or archive */
if (jadx_ctx_build(ctx) != JADX_OK) {
    fprintf(stderr, "%s\n", jadx_last_error(ctx));
}
for (size_t i = 0; i < jadx_ctx_class_count(ctx); i++) {
    const jadx_class *cls = jadx_ctx_class_at(ctx, i);
    char *src = jadx_class_get_java_source(ctx, cls);   /* owned */
    puts(src);
    jadx_str_free(src);
}
jadx_ctx_free(ctx);
```

Handles are indices, strings returned by `*_get_*` are owned by the caller, and
`jadx_ctx_set_option` takes jadx's own camelCase names so a host can forward a
settings map unchanged. `include/jadx.h` is the contract — read it, it is short
and every function is documented.

### C#

```csharp
class Jadx {
    [DllImport("jadx", CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr jadx_ctx_new();
    [DllImport("jadx", CallingConvention = CallingConvention.Cdecl)]
    public static extern int jadx_ctx_add_file(IntPtr ctx, string path);
    [DllImport("jadx", CallingConvention = CallingConvention.Cdecl)]
    public static extern int jadx_ctx_build(IntPtr ctx);
    [DllImport("jadx", CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr jadx_class_get_java_source(IntPtr ctx, IntPtr cls);
    [DllImport("jadx", CallingConvention = CallingConvention.Cdecl)]
    public static extern void jadx_str_free(IntPtr s);
    // ... see include/jadx.h for the rest
}
```

### Python

```sh
cargo build -p jadx-ffi
python3 crates/jadx-ffi/examples/decompile.py target/classes/com/example/T.class
```

## Verifying

* `cargo test --workspace --all-features` — the class-file reader, the decompiler
  stages and the end-to-end tests all build their fixtures with
  `jadx_rs::testutil` (no JDK needed), so the suite runs anywhere Rust does.
* `cargo run -p jadx-rs --example gen_class -- /tmp/T.class` writes a class file,
  `cargo run -p jadx-rs --example decompile -- /tmp/T.class` prints it back as
  Java; the CI job additionally compiles `examples/decompile.c` against the built
  `.so` and checks the output contains the expected source.
* Real-world checks worth doing first: `cargo run -p jadx-rs --example decompile -- <your.jar>`
  compared with `jadx --no-inline-anonymous <your.jar>` output, and
  `--disasm` compared with `javap -c`.

## Deliberate differences from jadx

jadx runs on Dalvik and treats JVM bytecode as the plugin case; this port is the
mirror image — the JVM path is first-class. Structural consequences (each is
covered in [docs/PORT_STATUS.md](docs/PORT_STATUS.md)):

* no SSA and no `RegionMaker`: the CFG is structured directly after stack
  emulation, which is simpler and adequate for straight-line javac output;
* `monitorenter`/`monitorexit`, `jsr`/`ret`, `inlineAnonymousClasses` and the
  Kotlin/Android-specific passes are not ported — where jadx would rewrite the
  code, this port prints a comment instead of miscompiling;
* DEX is read and disassembled, but DEX→Java is phase 2;
* resource files are recognised but not extracted.

## License

Apache-2.0, same as jadx. The parsing tables in `src/dex/table.rs` are generated
from the jadx sources in this repository (`tools/gen_dex_table.py`), which is also
why they stay consistent with upstream.
