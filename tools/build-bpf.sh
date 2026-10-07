#!/usr/bin/env bash
# Rebuilds the eBPF object the daemon embeds for per-process networking.
#
#   tools/build-bpf.sh
#
# Only needed after editing `daemon/crates/network/ebpf`. The result,
# `daemon/crates/network/bpf/pyren-net.bpf.o`, is checked in, so an
# ordinary `cargo build` of the daemon needs none of what this does: a
# nightly toolchain with `rust-src` (the eBPF target is not on stable, and
# `core` has to be built for it) and `bpf-linker`:
#
#   rustup toolchain install nightly --component rust-src
#   cargo install bpf-linker
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/daemon/crates/network/ebpf"
OUT="$ROOT/daemon/crates/network/bpf/pyren-net.bpf.o"

for tool in bpf-linker llvm-objcopy; do
  command -v "$tool" >/dev/null || {
    echo "build-bpf: $tool not found (see the header of this script)" >&2
    exit 1
  }
done

# The BTF string table keeps source paths even after the strip below, so
# they are rewritten at compile time: the object should not differ by, or
# name, the home directory it was built in. Setting RUSTFLAGS replaces the
# flags in `ebpf/.cargo/config.toml`, hence the first two being repeated.
(cd "$SRC" && RUSTFLAGS="-C debuginfo=2 -C link-arg=--btf \
  --remap-path-prefix=$HOME=/build --remap-path-prefix=$ROOT=/pyren" \
  cargo build --release)

# DWARF is what bpf-linker derives the BTF from, and is dead weight once it
# has; `.BTF.ext` is line info naming paths on the machine that built it.
# Neither is needed to load the programs, and dropping both keeps the
# checked-in object small and free of anyone's home directory.
llvm-objcopy --strip-debug \
  --remove-section .BTF.ext --remove-section .rel.BTF.ext \
  "$SRC/target/bpfel-unknown-none/release/pyren-net" "$OUT"

echo "build-bpf: wrote ${OUT#"$ROOT"/} ($(stat -c %s "$OUT") bytes)"
