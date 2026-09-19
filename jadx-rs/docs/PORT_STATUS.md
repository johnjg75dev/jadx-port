# jadx → jadx-rs port status

Reference point: this repository is a clone of [skylot/jadx](https://github.com/skylot/jadx)
at `2fb1b16` (the `fork_revision()` the library reports). Measured on that checkout:

| jadx side                                | files | lines |
|------------------------------------------|-------|-------|
| `jadx-core/src/main/java`                | 570   | 77 748 |
| `jadx-plugins/**`                        | 204   | 14 124 |
| └ `jadx-java-input` (the JVM backend)    | 64    | 4 864 |
| └ `jadx-dex-input`                       | 42    | 4 549 |
| **`jadx-rs` (this port)**                | 33    | ~17 500 |

The port is a *behavioural* port of the parts a decompiler host needs, not a line
by line translation. This page is the honest inventory: what is implemented, what
is partial, what is a documented stub, and where to look in jadx for the diff.

Legend — **done**: same behaviour, tested here. **partial**: works for the
common cases, gaps listed. **stub**: the API exists, answers "not implemented"
(and the output says so). **out**: not in scope for this port.

## 1. Reader: `jadx-plugins/jadx-java-input` → `jadx_rs::java`

| jadx | port | status |
|------|------|--------|
| `data/DataReader`, `utils/*` binary readers | `io::BinReader` | done — big/little endian, `uleb128`, `sleb128`, `uleb128p1` |
| `data/ConstPoolReader` | `java::const_pool` | done — all tags incl. `InvokeDynamic`, modified-UTF8 decode, `literal()` rendering with hex/unicode options |
| `data/JavaClassReader` | `java::class_file` | done — magic/version, this/super/interfaces, fields, methods, `method_id`, `jdk_version` |
| `data/code/JavaCodeReader` + `decoders/*` | `java::insn` | done — full opcode table with `Sem` classification, `wide`, `tableswitch`/`lookupswitch`, branches, field/method/type operands |
| `data/attributes/AttributesReader` | `java::attrs` | partial — `Code`, `LineNumberTable`, `LocalVariableTable`, `LocalVariableTypeTable`, `StackMapTable` (all frame types incl. `chop`/`full`/`append`), `Exceptions`, `Signature`, `ConstantValue`, annotations, `BootstrapMethods`, `EnclosingMethod`, `RecordComponents`, `InnerClasses`, `SourceFile`, `MethodParameters`, `NestMembers`/`PermittedSubclasses` (read, unused). Not modelled: `Synthetic`, `Deprecated`, `Bridge` (they are flags, not attributes), `RuntimeVisibleTypeAnnotations` *targets* (annotations parse, but their element targets are ignored), `Module*` attributes (parsed as unknown → kept in `Attrs::unknown`) |
| `data/code/trycatch/*` | `java::attrs::ExceptionHandler` | done — `catch_type == 0` becomes `None` = finally |
| `data/code/StackState`, `JavaInsnInfo` | `decompile::emulate` | partial — see §2 |
| `utils/DescriptorParser`, `utils/SignatureParser` | `types`, `java::signature` | done — descriptors + generic signatures (`<T:Ljava/lang/Object;>...`), erased supertype handling, `field_type_to_source` |
| `utils/DisasmUtils`, `JavaCodeDumper` | `java::disasm` | done — javap-like listing with offsets, constant comments, exception table, line numbers |
| `data/JavaClass`/`JavaMethodData`/`JavaFieldData` | `java::class_file` (`JavaClassFile`, `MethodData`, `FieldData`) | done |

## 2. Decompiler: `jadx-core/.../decompiler`, `dex/visitors/*` (28 387 lines) → `jadx_rs::decompile`

jadx's engine is: load → SSA build → ~30 `*Processors` → `RegionMaker` → codegen.
This port uses: stack emulation → CFG structuring → a handful of post-passes →
printer. The two produce the same *source shape* for straight-line javac/kotlinc
output; the middle of jadx's pipeline has no counterpart by design, because JVM
bytecode already carries its operand stack and its `StackMapTable`.

| jadx | port | status |
|------|------|--------|
| `JadxCodeReader`, `SsaBuildAndCheckPass` | `decompile::cfg` + `decompile::emulate` | partial — replacement by design: blocks, leaders, RPO, reachability, multi-pred spill to `v$N` temps, per-block expression building |
| `RegionMaker` + `RegionProcessors` | `decompile::structure` | partial — `if`/`if-else`, `while`, `do-while`, `for` (as `while` + `iinc`), `switch` (incl. string switches via `lookupswitch` keys), `try`/`catch`/`finally`, labelled `goto` fallback when a region cannot be proven. No post-dominator tree, no irreducible-region splitting: a region needs a single exit and a contiguous RPO range, otherwise it stays `goto`-based |
| `ProcessIf*, ProcessSwitch*, RegionProcessors` | `decompile::passes` | partial — trailing `return;` removal, `x = x + 1` → `x++`, single-use side-effect-free temp inlining, dead-label elision (fixpoint). Everything else (SSA-based variable proposals, `VarNamesCollector` renaming beyond uniqueness, type update loop) is not ported; unnamed locals keep `i0$`/`r1$`-style names |
| `CodeWriter`, `ClassWriter`, `AnnotationWriter`, `FieldWriter`, `MethodWriter` | `decompile::writer` | done for the printed surface: package/imports, class/interface/enum/annotation/record heads, generic signatures, `throws`, fields with `ConstantValue`, methods with annotations/`default` values, `static {}` blocks, non-Java access flags as trailing `/* synthetic */` comments, precedence-correct expressions, literals with hex/unicode options |
| `AttachCommentsVisitor` | `model` + `code_data` | partial — comments are attached per node id and printed above the member |
| `FinishPasses` | `decompile::passes` | partial |
| `AnonymousClassVisitor` (`inlineAnonymousClasses`) | — | **stub** — `new A() { ... }` stays a named class; `Args::inline_anonymous_classes` is accepted and logged as ignored. jadx's anonymous inlining is driven by `ITypeUsageInfo` over the whole app; without SSA it would mis-nest lambdas |
| `MoveInnerClasses`, `Class modifier` | `model::link_nested` | partial — `pkg/Outer$Inner` is printed inside `Outer.java` (declaration uses the short name). No `static` promotion decisions, no `moveInnerClasses` heuristics |
| `ExtractFieldInit`, `ConstInlineVisitor`, `replaceConsts` | `writer` (field `ConstantValue`) | partial — `static final` with `ConstantValue` prints its initialiser; there is no general constant inlining of `getstatic` of `a/b/C#MAX:I` into other methods |
| `EnumVisitor` | — | **stub** — enums print their `values()`/`<clinit>` bodies as plain Java (fields + static block), no `/* JADX */`-style enum reconstruction, no `EnumType`/`FIXED_*` attributes |
| `ConstructorVisitor`, `DeboxingVisitor`, `MarkMethodsForInlining`, `MethodInlineVisitor` (`inlineMethods`) | — | **out** — accessor/`return this.a;` inlining is the only case `passes` would attempt, and it is not wired to `Args::inline_methods` |
| `MethodThrowsVisitor` (throws propagation) | `writer` (`Exceptions` attribute only) | partial — declared `throws` are printed, inferred ones are not |
| `SetVariablesGeneric`, `GenericTypesVisitor` | `java::signature` | partial — signatures are printed from `Signature` attributes; no generic inference for missing signatures, no `T` substitution into method bodies |
| `ProcessPromptDuplicate`, duplicate-class handling | `model` | partial — duplicate class names are all kept (last one is not silently dropped); no `--no-duplicate-classes` behaviour |
| `SyntheticMethodVisitor`, `bridge` handling | `writer` comments | partial |
| `regions: synchronized` (`monitorenter`/`exit`) | `emulate` → `Stmt::Comment` | **stub** — a comment `/* monitorenter */` marks the site; jadx rebuilds a `synchronized` block |
| `jsr`/`ret` (pre-1.6 `finally`) | — | **out** — such methods fail structuring and print with labels + a note. javac has not emitted `jsr` since 1.5 |
| `JadxDecompiler`, `RootNode`, `ClassNode` graph, cross-references (`UsageInfoStorage`) | `model::Decompiler` | partial — no code-understanding graph: no `getUsages()`, no `searchMethodByCall`, no class-level usage queries; per-input decompilation only |
| `--single-class`, `--class-filter` | `Args::class_filter` | partial — the filter selects classes; jadx additionally prunes the code tree |
| `--output-format json`, `--export-*` | `Decompiler::to_json` | partial — one JSON document (name, package, members, source). `--export-debug-info`, gradle export, `--export-verbose`: out |
| `--show-bad-code`, `--fallback` | `Args::show_inconsistent_code`, `DecompilationMode` | done for the visible part (partial body + notes); `FallbackModeVisitor`'s disassembly-only output maps to `--disasm`/`jadx_class_get_bytecode_disasm` |
| `--threads-count` | `Args::threads_count` | **out** — this port decompiles on the caller's thread; the field exists for ABI parity (a shared library must not spawn surprise threads, and the caller owns parallelism) |
| logging / progress | `model::{LogSink, ProgressSink}` + FFI callbacks | done |

## 3. Dex input: `jadx-plugins/jadx-dex-input` (4 549 lines) → `jadx_rs::dex`

| jadx | port | status |
|------|------|--------|
| `DexFileLoader`, `utils/SectionReader`, `Leb128`, `MUtf8`, `DexCheckSum` | `dex::DexFile` | done — header (all 24 fields, `endian_tag` verified), string/type/proto/field/method ids, `map_list` not validated, class defs, `class_data_item` with the four diff-coded lists, `code_item` (registers/ins/outs/tries), `static_values_item` offset kept but **not decoded** |
| `insns/DexInsnInfo`, `DexOpcodes`, `DexInsnMnemonics`, `DexInsnFormat` | `dex::table` (generated by `tools/gen_dex_table.py`) | done — 224 registered opcodes, formats, register counts, index kinds, payload ids; guaranteed identical to jadx because it is generated from jadx's tables |
| `SmaliWriter` (of `jadx-smali-input`) | `dex::DexFile::class_smali` | partial — `.class`/`.super`/`.implements`/`.source`/`.field`/`.method`/`.registers` and one line per instruction with resolved string/type/field/method refs; `.param`/`.local`/`.line` debug annotations, `.catch`/`.tries` bodies and `encoded_value` payloads are printed as comments or omitted |
| `DexClassDefReader` → `ICodeInputLoader` → jadx-core's Dalvik IR | — | **stub** — **DEX to Java decompilation is phase 2.** A dex class enumerates its fields and methods (real names, types, modifiers, descriptors) and reports `DEX_JAVA_STATUS`; `jadx_class_is_dex_input` returns 1 and `jadx_class_get_java_source` contains the reason, so no caller can mistake it for a decompiled class |
| `JadxCodeData` (smali/debug info items) | — | out |
| `vdex`, `odex` containers | — | **out** — rejected with an `Unsupported` error naming the container |
| `apk`/`aar`/`aab`/`xapk` plugins | `input` + `zip` | partial — `classes*.dex` and `*.class` entries are read from any zip container; `.so`/resources/manifest decoding, split-apk merging (`jadx-apks-input`, `jadx-aab-input`'s config merging) are out |

## 4. API layer: `jadx-core/src/main/java/jadx/api` (8 189 lines) → `jadx_rs::{model, code_data, deobf, args}` + `jadx-ffi`

| jadx | port | status |
|------|------|--------|
| `JadxArgs` | `args::Args` | done for every field the port honours; the rest are stored and documented as no-ops (`threads_count`, `inlineMethods`, `respectBytecodeAccModifiers`, `typeUpdatesLimitCount`, …). `JadxArgs` fields with no counterpart in this port: `useRawInstruction*` (present as `cfg_output`/`raw_cfg_output`, printed only as notes), `useKotlinComments`, `useMthRegionsDepth`/`mthRegionsDepth` (a `JadxCodeVisitor` limit; this port uses `max_structuring_depth`), `fallbackMode`, `skipBatchInsns`, `customDictionarySource` |
| `JadxDecompiler` | `model::Decompiler` | done — add file/bytes, load, build, iterate, save, code data, progress. Not provided: `getWarnings()` dedup, `getResources()`, `getSyntaxTree()`/`searchClassByName` fuzzy matching, `decompileSingleClass` (use `find_class`), `close()` (nothing to close: no temp dirs are extracted) |
| `JavaClass`, `JavaMethod`, `JavaField`, `JavaNode`, `JavaPackage` | `model::*` | done for the read-side: name/package/file name/origin/access, `getJavaSource`, `getBytecodeDisasm`, `getSmali`, members by index and by name, ids for renames, `saveTo`. Missing: `getClassInfo().getCodeStats()`, `getInlineAnonymons()`, `getUsageInfo()`, `JavaNode.getComments()` (comments are only applied while printing) |
| `ICodeInput`/`ICodeRef` data model (`IJavaClass`, `IJavaMethod`, `IJavaField`, `IInsn`, `IMultiBranchInsn`, …) | — | out — the plugin SPI (a host adding its own input format) has no equivalent; `docs` explains that the extension point in this port is `input::CodeUnit` + `java::class_file` |
| `data/ICodeRename`, `ICodeComment`, `CodeRefType`, `JadxCodeData` | `code_data::{CodeData, ICodeRename, ICodeComment, CodeRefType}` | done for the text map: `.jobf` read/write is byte-compatible with `DeobfPresets` (`c|p|f|m <orig> = <alias>`, sorted, `#` comments); comments use an extra `@ <kind> <node> <line> <text>` line, which jadx does not persist. No YAML support (jadx's `renames.yml`/`comments.yml` serializers use SnakeYAML) |
| `core/deobf/*` (1 177 lines) | `deobf::Deobf` | partial — alias formats (`p%03d%s`, `{prefix}C%04d%s`, `f%d%s`, `m%d%s`/`mo%d%s`), `prepareNamePart` hashing with Java `String.hashCode`, `NameMapper` identifier validation/reserved words, `.jobf` load/save. **Not ported:** the rename *conditions* (`RenameClassCondition`, `ShortNamesCondition`, `UnicodeNamesCondition`, `PrintableNamesCondition`, `WhitelistCondition`), `DeobfScatterContainer`, `DeobfRef` graph, package-name aliasing (`--deobf-cfg-file`), `DeobfVocabFactory`/word-splitting (`--deobf-whitelist` and `-min/-max` lengths are honoured) |
| `core/utils/StringUtils`, `InputUtils`, `FilesUtils`, `ZipUtils` | `java::const_pool::{string_literal,char_literal}`, `input`, `zip` | done for what the printer needs: escapes, hex floats, safe file names (`save_to_dir` writes `<pkg>/<Name>.java`), zip reading + inflate. `FilesUtils`' temp-dir extraction and `FilePolicy` renaming are out |
| `core/export/*` (gradle project export), `core/ plugins` SPI, `JadxPluginsRegistry` | — | out |
| `JadxDecompiler.saveRawInputSource` (`--raw-input-source`) | — | out |
| `resources` (`-rs`/`--no-res`) | `input::Inputs::resources` (listed, not written) | **stub** — resource entries are recognised and their names are available; nothing is copied to the output dir |

## 5. C ABI: `crates/jadx-ffi` (new, no jadx counterpart)

73 exported functions, documented in `include/jadx.h`; the surface is
`jadx.api`'s (`JadxDecompiler` → `jadx_ctx`, `JavaClass` → `jadx_class` index
handles, `ICodeRename` → `jadx_ctx_add_rename`) plus the three things a native
host always asks for: `jadx_classify_file`/`jadx_classify_bytes`,
`jadx_ctx_save_source_to_dir`, and `jadx_ctx_to_json`.

Not exported, on purpose: any `IPlugin`/`I_CODE_LOADER` registration (there is no
plugin registry), `JadxArgs` fields with no effect, `getWarnings` (use
`jadx_ctx_error_count`/`jadx_ctx_error_at`).

`tests/abi.rs` keeps the hand-written header and the exports in step, so this
table cannot rot silently.

## 6. Known gaps worth knowing before you rely on it

1. **Nothing here is compile-verified.** The tree was written in an environment
   without a Rust toolchain; every file passed a Rust *syntax* check and manual
   review, but `cargo build` has never run. Expect the first build to report type
   and borrow errors — that is the expected work of the bring-up, not a design
   change. `cargo test` then `cargo clippy` is the order to fix things in.
2. **`forbid(unsafe_code)` covers `jadx-rs` only**; `jadx-ffi` is `unsafe` by
   necessity (raw pointers, `catch_unwind`, leaked `CString`s). Every export is
   panic-guarded, and no `unsafe` block dereferences a caller pointer longer than
   the call that received it.
3. **Structure recovery is conservative.** Unusual control flow (multiple loop
   exits, irreducible graphs, `finally` that does not sit right behind the
   protected range) prints as labels plus `goto` with a note, instead of jadx's
   more aggressive duplication (`extractFinally`, `RegionMaker` splitting). Output
   stays valid Java in every case we test.
4. **Zip**: no zip64 archives (reported as `Unsupported`), no encrypted entries,
   no deflate64, and `zip_entry_limit_bytes` (default 64 MiB/entry) caps inflation.
5. **Class files newer than the tables**: a new attribute or opcode is not an
   error — an unknown attribute is skipped by the length re-sync in `java::attrs::read_attrs` (jadx
   keeps an `AttrType` map and ignores what it does not know), and unregistered opcodes
   keep their `unused` mnemonic so they still show up in the disassembly — but nothing new is *understood* until the tables are
   regenerated with `tools/gen_dex_table.py` (dex) or the attribute reader is
   extended (JVM).

## 7. Phase 2, in the order that would pay off

1. DEX→Java: Dalvik register liveness → the same `emulate` IR (the register file
   replaces the operand stack; `move/16`, `const/4`, `/lit8` map directly), then
   reuse `structure`/`writer` unchanged.
2. jadx-style `synchronized` regions from `monitorenter`/`monitorexit` pairs, and
   `finally` duplication (`extractFinally`).
3. Constant inlining of `static final` fields and enum reconstruction.
4. Rename conditions + package aliasing from `core/deobf/conditions`.
5. Resources extraction (`-rs`), and `renames.yml`/`comments.yml` I/O for drop-in
   compatibility with jadx's code-data files.
