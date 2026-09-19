#!/usr/bin/env python3
"""Decompile JVM class files through the shared library, from Python.

This is the ctypes proof of the C ABI: no bindings, no build step, nothing but the
header's contract. It is also the shortest way to try a change to the library
without writing C.

    cargo build -p jadx-ffi
    python3 jadx-rs/crates/jadx-ffi/examples/decompile.py target/classes/pkg/T.class

Point at another build with JADX_LIB, e.g.
    JADX_LIB=./target/release/libjadx.so python3 .../decompile.py x.class
"""
import ctypes as C
import os
import sys
from pathlib import Path


def candidate_libs():
	env = os.environ.get("JADX_LIB")
	if env:
		yield Path(env)
	here = Path(__file__).resolve()
	for profile in ("debug", "release"):
		for name in ("libjadx.so", "jadx.dll", "libjadx.dylib"):
			yield here.parents[4] / "target" / profile / name
	for name in ("libjadx.so", "jadx.dll"):
		yield Path("/usr/local/lib") / name


def load():
	for p in candidate_libs():
		if p.is_file():
			return C.CDLL(str(p))
	sys.exit(
		"shared library not found; build it first:\n"
		"  cargo build -p jadx-ffi\n"
		"or set JADX_LIB=/path/to/libjadx.so"
	)


def bind(lib):
	c = C.c_char_p
	u32 = C.c_uint32
	i32 = C.c_int32
	i64 = C.c_int64
	size = C.c_size_t
	lib.jadx_version.restype = c
	lib.jadx_license.restype = c
	lib.jadx_fork_revision.restype = c
	lib.jadx_str_free.argtypes = [C.c_void_p]
	lib.jadx_ctx_new.restype = C.c_void_p
	lib.jadx_ctx_free.argtypes = [C.c_void_p]
	lib.jadx_last_error.restype = c
	lib.jadx_ctx_set_option.argtypes = [C.c_void_p, c, c]
	lib.jadx_ctx_set_option.restype = i32
	lib.jadx_ctx_add_file.argtypes = [C.c_void_p, c]
	lib.jadx_ctx_add_file.restype = i32
	lib.jadx_ctx_add_bytes.argtypes = [C.c_void_p, c, C.c_char_p, size]
	lib.jadx_ctx_add_bytes.restype = i32
	lib.jadx_ctx_build.argtypes = [C.c_void_p]
	lib.jadx_ctx_build.restype = i32
	lib.jadx_ctx_class_count.argtypes = [C.c_void_p]
	lib.jadx_ctx_class_count.restype = size
	lib.jadx_ctx_class_at.argtypes = [C.c_void_p, size]
	lib.jadx_ctx_class_at.restype = C.c_void_p
	lib.jadx_class_get_full_name.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_get_full_name.restype = c
	lib.jadx_class_get_java_source.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_get_java_source.restype = c
	lib.jadx_class_get_bytecode_disasm.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_get_bytecode_disasm.restype = c
	lib.jadx_class_get_smali.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_get_smali.restype = c
	lib.jadx_class_is_dex_input.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_is_dex_input.restype = i32
	lib.jadx_class_method_count.argtypes = [C.c_void_p, C.c_void_p]
	lib.jadx_class_method_count.restype = size
	lib.jadx_method_get_name.argtypes = [C.c_void_p, C.c_void_p, size]
	lib.jadx_method_get_name.restype = c
	lib.jadx_method_get_java_source.argtypes = [C.c_void_p, C.c_void_p, size]
	lib.jadx_method_get_java_source.restype = c
	lib.jadx_ctx_load_renames.argtypes = [C.c_void_p, c]
	lib.jadx_ctx_load_renames.restype = i32
	lib.jadx_classify_file.argtypes = [c]
	lib.jadx_classify_file.restype = i32
	# `char *` results are allocated by the library; widen them so the pointer is
	# not truncated on 64-bit Windows (where c_char_p would also auto-decode)
	for name in (
		"jadx_class_get_full_name",
		"jadx_class_get_java_source",
		"jadx_class_get_bytecode_disasm",
		"jadx_class_get_smali",
		"jadx_method_get_name",
		"jadx_method_get_java_source",
	):
		lib.__getattr__(name).restype = C.c_void_p
	return lib


def take_string(lib, ptr):
	if not ptr:
		return None
	try:
		return C.string_at(ptr).decode("utf-8", "replace")
	finally:
		lib.jadx_str_free(C.c_void_p(ptr))


def main(argv):
	if len(argv) < 2:
		print(__doc__)
		return 2
	lib = bind(load())
	print(
		"jadx-rs {}, license {}, tables from jadx {}".format(
			lib.jadx_version().decode(),
			lib.jadx_license().decode(),
			lib.jadx_fork_revision().decode(),
		)
	)
	ctx = lib.jadx_ctx_new()
	if not ctx:
		print("jadx_ctx_new failed", file=sys.stderr)
		return 1
	# optional: apply a jadx `.jobf` rename map
		renames = os.environ.get("JADX_RENAMES")
		if renames and lib.jadx_ctx_load_renames(ctx, renames.encode()) != 0:
			print("cannot load {}: {}".format(renames, lib.jadx_last_error(ctx).decode()), file=sys.stderr)
			return 1
	try:
		for path in argv[1:]:
			if lib.jadx_ctx_add_file(ctx, path.encode()) != 0:
				print("cannot add {}: {}".format(path, lib.jadx_last_error(ctx).decode()), file=sys.stderr)
				return 1
		if lib.jadx_ctx_build(ctx) != 0:
			print("build failed: {}".format(lib.jadx_last_error(ctx).decode()), file=sys.stderr)
			return 1
		total = lib.jadx_ctx_class_count(ctx)
		print("{} class(es)".format(total))
		for i in range(total):
			cls = lib.jadx_ctx_class_at(ctx, C.c_size_t(i))
			name = take_string(lib, lib.jadx_class_get_full_name(ctx, cls))
			print("==== {} ====".format(name))
			for m in range(lib.jadx_class_method_count(ctx, cls)):
				mname = take_string(lib, lib.jadx_method_get_name(ctx, cls, C.c_size_t(m)))
				print("  method {}".format(mname))
			if lib.jadx_class_is_dex_input(ctx, cls):
				print(take_string(lib, lib.jadx_class_get_smali(ctx, cls)))
			else:
				print(take_string(lib, lib.jadx_class_get_java_source(ctx, cls)))
	finally:
		lib.jadx_ctx_free(C.c_void_p(ctx))
	return 0


if __name__ == "__main__":
	sys.exit(main(sys.argv))
