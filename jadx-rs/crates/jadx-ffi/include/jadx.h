/*
 * jadx.h -- C interface of the jadx-rs shared library.
 *
 * Hand-written (not generated) and kept in sync with `crates/jadx-ffi/src/lib.rs`
 * by `crates/jadx-ffi/tests/abi.rs`, which checks the symbol list. `cbindgen` is
 * configured for convenience (`cbindgen --config crates/jadx-ffi/cbindgen.toml
 * crates/jadx-ffi/src/lib.rs`) but this file, not its output, is what ships: the
 * documentation, the enums and the ownership notes below are the API contract.
 *
 * Calling rules
 * -------------
 * - One `jadx_ctx*` per thread. A context is not thread-safe, and neither is
 *   `JadxDecompiler` in jadx.
 * - `jadx_class` handles are index handles: they are copies of a number, may be
 *   stored, and are valid until the context is freed or rebuilt. Never
 *   dereference one.
 * - Every `char *` returned by this header is owned by the caller and must be
 *   released with jadx_str_free(). Strings returned by jadx_version(),
 *   jadx_license(), jadx_fork_revision() and jadx_last_error() are borrowed from
 *   the library: do not free them, and copy them if you need them later.
 * - Functions returning int32_t answer 0 on success; a negative value is minus
 *   the error code of the failure (see JADX_ERR_* below), and the message is
 *   available from jadx_last_error() until the next call on the same context.
 * - No C++ types, no exceptions: every entry point catches Rust panics and turns
 *   them into JADX_ERR_INTERNAL.
 */

#ifndef JADX_H
#define JADX_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) || defined(__CYGWIN__)
#  ifdef JADX_STATIC
#    define JADX_API
#  elif defined(JADX_BUILDING)
#    define JADX_API __declspec(dllexport)
#  else
#    define JADX_API __declspec(dllimport)
#  endif
#  define JADX_CALL __cdecl
#else
#  if defined(JADX_BUILDING) && defined(__GNUC__)
#    define JADX_API __attribute__((visibility("default")))
#  else
#    define JADX_API
#  endif
#  define JADX_CALL
#endif

typedef struct jadx_ctx jadx_ctx;
typedef struct jadx_class jadx_class;

/* Error codes: a failing call returns `-JADX_ERR_*`. */
enum {
	JADX_OK = 0,
	JADX_ERR_IO = -1,
	JADX_ERR_INPUT_FORMAT = -2,
	JADX_ERR_UNSUPPORTED = -3,
	JADX_ERR_INVALID_ARGUMENT = -4,
	JADX_ERR_NOT_FOUND = -5,
	JADX_ERR_INCOMPLETE = -6,
	JADX_ERR_INTERNAL = -7
};

/* Node kinds accepted by jadx_ctx_add_rename()/jadx_ctx_add_comment(), and the
 * first column of a .jobf map: 0 package, 1 class, 2 field, 3 method,
 * 4 method argument, 5 local variable, 6 catch, 7 instruction. */
typedef enum jadx_ref_type {
	JADX_REF_PACKAGE = 0,
	JADX_REF_CLASS = 1,
	JADX_REF_FIELD = 2,
	JADX_REF_METHOD = 3,
	JADX_REF_MTH_ARG = 4,
	JADX_REF_VAR = 5,
	JADX_REF_CATCH = 6,
	JADX_REF_INSN = 7
} jadx_ref_type;

/* What jadx_input_kind_* reports for a file or buffer. */
typedef enum jadx_input_kind {
	JADX_INPUT_CLASS = 0,   /* a JVM .class file (0xCAFEBABE) */
	JADX_INPUT_DEX = 1,     /* a Dalvik classes.dex           */
	JADX_INPUT_ARCHIVE = 2, /* zip/jar/aar/aab/apk            */
	JADX_INPUT_RESOURCE = 3 /* anything else                   */
} jadx_input_kind;

/* Callbacks. `user` is passed back unchanged. Do not call back into the same
 * context from inside a callback. */
typedef void (*jadx_log_cb)(void *user, const char *line);
typedef void (*jadx_progress_cb)(void *user, size_t done, size_t total, const char *what);

/* ------------------------------------------------------------------------- */
/* library information                                                       */
/* ------------------------------------------------------------------------- */

/* Version of the library, e.g. "0.1.0". Borrowed. */
JADX_API const char *JADX_CALL jadx_version(void);
JADX_API int32_t JADX_CALL jadx_version_major(void);
JADX_API int32_t JADX_CALL jadx_version_minor(void);
JADX_API int32_t JADX_CALL jadx_version_patch(void);
/* SPDX id of the license, "Apache-2.0". Borrowed. */
JADX_API const char *JADX_CALL jadx_license(void);
/* Upstream jadx commit the parsing tables were taken from. Borrowed. */
JADX_API const char *JADX_CALL jadx_fork_revision(void);

/* Release a string returned by this library. NULL is allowed. */
JADX_API void JADX_CALL jadx_str_free(char *p);
/* Copy a borrowed string into an owned one (free with jadx_str_free). */
JADX_API char *JADX_CALL jadx_str_dup(const char *p);

/* ------------------------------------------------------------------------- */
/* context                                                                   */
/* ------------------------------------------------------------------------- */

JADX_API jadx_ctx *JADX_CALL jadx_ctx_new(void);
JADX_API void JADX_CALL jadx_ctx_free(jadx_ctx *ctx);
/* Message of the last failure on this context; borrowed, NULL if none. */
JADX_API const char *JADX_CALL jadx_last_error(jadx_ctx *ctx);
/* The negative code of the last failure, 0 if the last call succeeded. */
JADX_API int32_t JADX_CALL jadx_last_error_code(jadx_ctx *ctx);
/* Drop the loaded code and the errors, keep the configuration. */
JADX_API int32_t JADX_CALL jadx_ctx_reset(jadx_ctx *ctx);

/* ------------------------------------------------------------------------- */
/* configuration (jadx: JadxArgs)                                            */
/* ------------------------------------------------------------------------- */

/* String-valued options, by jadx name: outDir, outDirSrc, useImports,
 * debugInfo, printLineNumbers, escapeUnicode, showInconsistentCode,
 * deobfuscationOn, decompilationMode (auto|restructure|simple|fallback),
 * outputFormat (java|json), commentsLevel (none|userOnly|error|warn|info|debug),
 * integerFormat (auto|decimal|hexadecimal), classFilter (comma separated),
 * codeIndent. Booleans are "1"/"true"/"yes"/"on". */
JADX_API int32_t JADX_CALL jadx_ctx_set_option(jadx_ctx *ctx, const char *key, const char *value);
/* Integer-valued options: threadsCount, deobfuscationMinLength,
 * deobfuscationMaxLength, maxStructuringDepth, zipEntryLimitBytes,
 * useDebugVarsNames, cfgOutput, skipResources, inlineAnonymousClasses. */
JADX_API int32_t JADX_CALL jadx_ctx_set_option_int(jadx_ctx *ctx, const char *key, int64_t value);

/* ------------------------------------------------------------------------- */
/* inputs and the run                                                        */
/* ------------------------------------------------------------------------- */

/* Add a .class file, a jar/zip/apk archive or a directory of class files. */
JADX_API int32_t JADX_CALL jadx_ctx_add_file(jadx_ctx *ctx, const char *path);
/* Add code from memory. `name` decides the kind, e.g. "a/b/C.class". */
JADX_API int32_t JADX_CALL jadx_ctx_add_bytes(jadx_ctx *ctx, const char *name, const uint8_t *data, size_t len);
/* Parse and decompile everything added so far. */
JADX_API int32_t JADX_CALL jadx_ctx_build(jadx_ctx *ctx);

JADX_API size_t JADX_CALL jadx_ctx_class_count(jadx_ctx *ctx);
/* NULL when the index is out of range. */
JADX_API const jadx_class *JADX_CALL jadx_ctx_class_at(jadx_ctx *ctx, size_t index);
/* Accepts both "a.b.C" and "a/b/C"; NULL when not found. */
JADX_API const jadx_class *JADX_CALL jadx_ctx_find_class(jadx_ctx *ctx, const char *name);

JADX_API size_t JADX_CALL jadx_ctx_package_count(jadx_ctx *ctx);
JADX_API char *JADX_CALL jadx_ctx_package_name(jadx_ctx *ctx, size_t index);
JADX_API size_t JADX_CALL jadx_ctx_package_class_count(jadx_ctx *ctx, size_t index);
JADX_API const jadx_class *JADX_CALL jadx_ctx_package_class(jadx_ctx *ctx, size_t index, size_t class_index);

/* Inputs that failed to load are collected, never fatal (jadx does the same). */
JADX_API size_t JADX_CALL jadx_ctx_error_count(jadx_ctx *ctx);
JADX_API char *JADX_CALL jadx_ctx_error_at(jadx_ctx *ctx, size_t index);

/* Write every class as <dir>/<package>/<Outer>.java. */
JADX_API int32_t JADX_CALL jadx_ctx_save_source_to_dir(jadx_ctx *ctx, const char *dir);
/* The whole run as JSON, the sibling of `jadx --output-format json`. */
JADX_API char *JADX_CALL jadx_ctx_to_json(jadx_ctx *ctx);

/* ------------------------------------------------------------------------- */
/* classes                                                                   */
/* ------------------------------------------------------------------------- */

JADX_API char *JADX_CALL jadx_class_get_name(jadx_ctx *ctx, const jadx_class *cls);            /* a/b/C   */
JADX_API char *JADX_CALL jadx_class_get_full_name(jadx_ctx *ctx, const jadx_class *cls);       /* a.b.C   */
JADX_API char *JADX_CALL jadx_class_get_package(jadx_ctx *ctx, const jadx_class *cls);         /* a/b     */
JADX_API char *JADX_CALL jadx_class_get_file_name(jadx_ctx *ctx, const jadx_class *cls);       /* C.java  */
JADX_API char *JADX_CALL jadx_class_get_origin(jadx_ctx *ctx, const jadx_class *cls);
JADX_API char *JADX_CALL jadx_class_get_java_source(jadx_ctx *ctx, const jadx_class *cls);
JADX_API char *JADX_CALL jadx_class_get_bytecode_disasm(jadx_ctx *ctx, const jadx_class *cls);
JADX_API char *JADX_CALL jadx_class_get_smali(jadx_ctx *ctx, const jadx_class *cls);
JADX_API char *JADX_CALL jadx_class_get_imports(jadx_ctx *ctx, const jadx_class *cls);        /* one per line */
JADX_API uint32_t JADX_CALL jadx_class_get_access_flags(jadx_ctx *ctx, const jadx_class *cls);
JADX_API int32_t JADX_CALL jadx_class_is_interface(jadx_ctx *ctx, const jadx_class *cls);
JADX_API int32_t JADX_CALL jadx_class_is_enum(jadx_ctx *ctx, const jadx_class *cls);
/* 1 when the class came from a .dex: no Java source from this port (phase 2). */
JADX_API int32_t JADX_CALL jadx_class_is_dex_input(jadx_ctx *ctx, const jadx_class *cls);

JADX_API size_t JADX_CALL jadx_class_method_count(jadx_ctx *ctx, const jadx_class *cls);
JADX_API size_t JADX_CALL jadx_class_field_count(jadx_ctx *ctx, const jadx_class *cls);
JADX_API size_t JADX_CALL jadx_class_error_count(jadx_ctx *ctx, const jadx_class *cls);
JADX_API char *JADX_CALL jadx_class_error_at(jadx_ctx *ctx, const jadx_class *cls, size_t index);
/* Member index by simple name, -1 when there is none. */
JADX_API int64_t JADX_CALL jadx_class_method_index_by_name(jadx_ctx *ctx, const jadx_class *cls, const char *name);
JADX_API int64_t JADX_CALL jadx_class_field_index_by_name(jadx_ctx *ctx, const jadx_class *cls, const char *name);
/* The class' own code data: one `c|f|m <id>` line per node. */
JADX_API char *JADX_CALL jadx_class_get_code_data(jadx_ctx *ctx, const jadx_class *cls);

/* ------------------------------------------------------------------------- */
/* methods and fields                                                        */
/* ------------------------------------------------------------------------- */

JADX_API char *JADX_CALL jadx_method_get_name(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_method_get_descriptor(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_method_get_id(jadx_ctx *ctx, const jadx_class *cls, size_t index);        /* a/b/C#m(I)V */
JADX_API char *JADX_CALL jadx_method_get_return_type(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_method_get_java_source(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_method_get_bytecode_disasm(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API uint32_t JADX_CALL jadx_method_get_access_flags(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API int32_t JADX_CALL jadx_method_is_constructor(jadx_ctx *ctx, const jadx_class *cls, size_t index);

JADX_API char *JADX_CALL jadx_field_get_name(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_field_get_type(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_field_get_descriptor(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API char *JADX_CALL jadx_field_get_id(jadx_ctx *ctx, const jadx_class *cls, size_t index);
/* ConstantValue as it would be printed ("42", "'a'"); empty string when there is none. */
JADX_API char *JADX_CALL jadx_field_get_value(jadx_ctx *ctx, const jadx_class *cls, size_t index);
JADX_API uint32_t JADX_CALL jadx_field_get_access_flags(jadx_ctx *ctx, const jadx_class *cls, size_t index);

/* ------------------------------------------------------------------------- */
/* code data: renames and comments                                           */
/* ------------------------------------------------------------------------- */

/* Renames must be added before jadx_ctx_build() to affect the printed source. */
JADX_API int32_t JADX_CALL jadx_ctx_add_rename(jadx_ctx *ctx, jadx_ref_type kind, const char *node, const char *alias);
JADX_API int32_t JADX_CALL jadx_ctx_add_comment(jadx_ctx *ctx, jadx_ref_type kind, const char *node, int32_t line, const char *text);
/* Load/save a jadx `.jobf` map (`c a/b/C = C0002a` style lines). */
JADX_API int32_t JADX_CALL jadx_ctx_load_renames(jadx_ctx *ctx, const char *path);
JADX_API int32_t JADX_CALL jadx_ctx_save_renames(jadx_ctx *ctx, const char *path);
JADX_API size_t JADX_CALL jadx_ctx_rename_count(jadx_ctx *ctx);
/* One entry as "<kind>\t<node>\t<alias>". */
JADX_API char *JADX_CALL jadx_ctx_rename_at(jadx_ctx *ctx, size_t index);

JADX_API int32_t JADX_CALL jadx_ctx_set_log_callback(jadx_ctx *ctx, jadx_log_cb cb, void *user);
JADX_API int32_t JADX_CALL jadx_ctx_set_progress_callback(jadx_ctx *ctx, jadx_progress_cb cb, void *user);

/* ------------------------------------------------------------------------- */
/* input inspection (stateless)                                              */
/* ------------------------------------------------------------------------- */

JADX_API int32_t JADX_CALL jadx_classify_file(const char *path);  /* jadx_input_kind or -1 */
JADX_API int32_t JADX_CALL jadx_classify_bytes(const uint8_t *data, size_t len);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* JADX_H */
