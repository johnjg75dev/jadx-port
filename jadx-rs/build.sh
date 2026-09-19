#!/usr/bin/env bash
# Build the jadx-rs shared library, or several flavours of it.
#
#   ./build.sh linux          # target/release/libjadx.so
#   ./build.sh windows-gnu    # target/x86_64-pc-windows-gnu/release/jadx.dll
#   ./build.sh windows-msvc   # target/x86_64-pc-windows-msvc/release/jadx.dll
#   ./build.sh all            # everything the host toolchain can reach
#   ./build.sh test           # cargo test --workspace --all-features
#   ./build.sh check          # syntax-free fast pass: fmt + clippy + tests
#   ./build.sh dist           # build + collect headers and libs into ./dist
#
# Nothing here needs network access: the crates have no dependencies.
set -euo pipefail

cd "$(dirname "$0")"
WORKSPACE="$PWD"
PROFILE_FLAG="${JADX_PROFILE:-release}"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1m\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need_cargo() {
	command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH (install rustup)"
}

build_target() {
	local target="$1"
	say "building jadx-ffi for ${target} (${PROFILE_FLAG})"
	rustup target add "${target}" >/dev/null 2>&1 || true
	( cd "${WORKSPACE}" && cargo build -p jadx-ffi --all-features --target "${target}" --${PROFILE_FLAG} )
	local out="target/${target}/${PROFILE_FLAG}"
	for lib in libjadx.so jadx.dll libjadx.dylib; do
		if [[ -f "${out}/${lib}" ]]; then
			say "  ${out}/${lib} ($(( $(stat -c%s "${out}/${lib}" 2>/dev/null || stat -f%z "${out}/${lib}") / 1024 )) KiB)"
		fi
	done
	for imp in jadx.lib libjadx.a jadx.dll.a; do
		[[ -f "${out}/${imp}" ]] && say "  ${out}/${imp}"
	done
}

host_target() {
	rustc -vV 2>/dev/null | sed -n 's/^host: //p'
}

do_linux() { build_target "$(host_target)"; }
do_windows_gnu() {
	command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 \
		|| die "the gnu target needs mingw-w64 gcc on PATH (MSYS2: pacman -S mingw-w64-x86_64-toolchain)"
	build_target x86_64-pc-windows-gnu
}
do_windows_msvc() {
	[[ "$(uname -s || true)" =~ MINGW|MSYS|CYGWIN ]] || die "the msvc target must be built from a Developer Command Prompt on Windows"
	build_target x86_64-pc-windows-msvc
}

do_test() {
	need_cargo
	say "cargo test --workspace --all-features"
	( cd "${WORKSPACE}" && cargo test --workspace --all-features )
}

do_check() {
	need_cargo
	say "cargo fmt --check (advisory)"
	( cd "${WORKSPACE}" && cargo fmt --all -- --check ) || say "  formatting differs; run: cargo fmt --all"
	say "cargo clippy"
	( cd "${WORKSPACE}" && cargo clippy --workspace --all-targets --all-features )
	do_test
}

do_dist() {
	need_cargo
	mkdir -p dist
	( cd "${WORKSPACE}" && cargo build -p jadx-ffi --all-features --${PROFILE_FLAG} )
	local out="target/${PROFILE_FLAG}"
	cp crates/jadx-ffi/include/jadx.h dist/
	cp LICENSE dist/LICENSE 2>/dev/null || true
	for f in libjadx.so jadx.dll libjadx.dylib libjadx.a jadx.lib jadx.dll.a; do
		[[ -f "${out}/${f}" ]] && cp "${out}/${f}" dist/
	done
	ls -la dist
	say "dist ready"
}

main() {
	need_cargo
	case "${1:-linux}" in
		linux) do_linux ;;
		windows-gnu) do_windows_gnu ;;
		windows-msvc) do_windows_msvc ;;
		all)
			do_linux
			[[ "$(host_target)" == *-windows-* ]] && do_windows_gnu || true
			;;
		test) do_test ;;
		check) do_check ;;
		dist) do_dist ;;
		-h|--help|help)
			sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
			;;
		*) die "unknown command: $1 (try --help)" ;;
	esac
}

main "$@"
