#!/bin/bash
# aarch64 linker wrapper using rust-lld (bundled with Rust toolchain)
# Strips GCC-style -Wl, prefix flags that LLD can't parse directly.

SYSROOT=$(rustc --print sysroot)
LLD="$SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/ld.lld"

ARGS=(-m aarch64linux)
for arg in "$@"; do
    case "$arg" in
        -Wl,*)
            IFS=',' read -ra SUB <<< "${arg#-Wl,}"
            for s in "${SUB[@]}"; do
                ARGS+=("$s")
            done
            ;;
        -nodefaultlibs|-nostartfiles|-B*)
            # Skip: not needed or GCC-specific path
            ;;
        *)
            ARGS+=("$arg")
            ;;
    esac
done

exec "$LLD" "${ARGS[@]}"
