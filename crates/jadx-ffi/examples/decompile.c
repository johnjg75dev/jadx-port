/*
 * decompile.c -- minimal host for libjadx, the "read a .class or .jar and print
 * Java" path of the C ABI.
 *
 * Build on Linux:
 *   cargo build --release -p jadx-ffi
 *   cc -I../include examples/decompile.c -o /tmp/decompile \
 *      -L../../target/release -ljadx -Wl,-rpath,../../target/release
 *
 * Build on Windows (MSVC):
 *   cargo build --release -p jadx-ffi
 *   cl /I../include examples/decompile.c /link /LIBPATH:../../target/release jadx.lib
 *
 * On Windows the DLL also needs to be next to the executable or on PATH.
 *
 * Usage: /tmp/decompile path/to/T.class path/to/app.jar
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "jadx.h"

/* Every `*_get_*` result is owned by the caller, so a small helper keeps the
 * "print then free" pattern in one place. */
static void print_and_free(const char *label, char *value) {
	if (value == NULL) {
		printf("%s: <none>\n", label);
		return;
	}
	if (strchr(value, '\n') != NULL) {
		printf("%s:\n%s\n", label, value);
	} else {
		printf("%s: %s\n", label, value);
	}
	jadx_str_free(value);
}

static void on_log(void *user, const char *line) {
	(void) user;
	fprintf(stderr, "[jadx] %s\n", line);
}

static void on_progress(void *user, size_t done, size_t total, const char *what) {
	(void) user;
	if (total > 0 && (done == total || done % 32 == 0)) {
		fprintf(stderr, "[jadx] %zu/%zu %s\n", done, total, what ? what : "");
	}
}

static int run_class(jadx_ctx *ctx, const jadx_class *cls) {
	char *name = jadx_class_get_full_name(ctx, cls);
	printf("==== %s ====\n", name ? name : "?");
	jadx_str_free(name);

	size_t fields = jadx_class_field_count(ctx, cls);
	for (size_t i = 0; i < fields; i++) {
		char *fname = jadx_field_get_name(ctx, cls, i);
		char *ftype = jadx_field_get_type(ctx, cls, i);
		char *value = jadx_field_get_value(ctx, cls, i);
		printf("  field   %-24s %-20s %s\n",
			fname ? fname : "?", ftype ? ftype : "?", value && *value ? value : "");
		jadx_str_free(fname);
		jadx_str_free(ftype);
		jadx_str_free(value);
	}

	size_t methods = jadx_class_method_count(ctx, cls);
	for (size_t i = 0; i < methods; i++) {
		char *mname = jadx_method_get_name(ctx, cls, i);
		char *mdesc = jadx_method_get_descriptor(ctx, cls, i);
		printf("  method  %-24s %s\n", mname ? mname : "?", mdesc ? mdesc : "?");
		jadx_str_free(mname);
		jadx_str_free(mdesc);
	}

	/* the actual decompiled source; for a dex class this carries the reason why
	 * there is none, and jadx_class_get_smali has the disassembly instead */
	char *src = jadx_class_get_java_source(ctx, cls);
	print_and_free("  source", src);
	if (jadx_class_is_dex_input(ctx, cls)) {
		char *smali = jadx_class_get_smali(ctx, cls);
		print_and_free("  smali", smali);
	}
	return 0;
}

int main(int argc, char **argv) {
	if (argc < 2) {
		fprintf(stderr, "usage: %s <file.class|file.jar|directory> ...\n", argv[0]);
		fprintf(stderr, "jadx-rs %s (jadx %s)\n", jadx_version(), jadx_fork_revision());
		return 2;
	}
	printf("jadx-rs %s, license %s, tables from jadx %s\n",
		jadx_version(), jadx_license(), jadx_fork_revision());

	jadx_ctx *ctx = jadx_ctx_new();
	if (ctx == NULL) {
		fprintf(stderr, "out of memory\n");
		return 1;
	}
	jadx_ctx_set_log_callback(ctx, &on_log, NULL);
	jadx_ctx_set_progress_callback(ctx, &on_progress, NULL);
	/* keep the output close to what `jadx` prints */
	jadx_ctx_set_option(ctx, "useImports", "true");
	jadx_ctx_set_option(ctx, "showInconsistentCode", "true");

	for (int i = 1; i < argc; i++) {
		int kind = jadx_classify_file(argv[i]);
		printf("input %s (kind %d)\n", argv[i], kind);
		int rc = jadx_ctx_add_file(ctx, argv[i]);
		if (rc != JADX_OK) {
			fprintf(stderr, "failed to add %s: %s\n", argv[i], jadx_last_error(ctx));
			jadx_ctx_free(ctx);
			return 1;
		}
	}

	int rc = jadx_ctx_build(ctx);
	if (rc != JADX_OK) {
		fprintf(stderr, "build failed (%d): %s\n", rc, jadx_last_error(ctx));
		jadx_ctx_free(ctx);
		return 1;
	}

	size_t count = jadx_ctx_class_count(ctx);
	printf("%zu class(es)\n", count);
	for (size_t i = 0; i < count; i++) {
		const jadx_class *cls = jadx_ctx_class_at(ctx, i);
		if (cls != NULL) {
			run_class(ctx, cls);
		}
	}
	for (size_t i = 0; i < jadx_ctx_error_count(ctx); i++) {
		char *e = jadx_ctx_error_at(ctx, i);
		print_and_free("problem", e);
	}

	jadx_ctx_free(ctx);
	return 0;
}
