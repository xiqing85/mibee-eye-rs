# MiBee Eye Makefile (Rust)
# Cross-compile for SBC (ARM64) from workstation

BINARY := target/release/mibee-eye-raspi-rs
CROSS_BINARY := target/$(TARGET)/release/mibee-eye-raspi-rs
REMOTE_HOST ?= pi@192.168.1.100
REMOTE_DIR ?= ~/mibee-eye
# Primary cross-compilation target: gnu (dynamic, links against system glibc + libcamera)
# For glibc targets use: aarch64-unknown-linux-gnu
TARGET := aarch64-unknown-linux-gnu

.PHONY: build test clippy fmt cross-build cross-build-musl cross-build-cross cross-build-zig cross-build-native deploy deploy-cross clean run

build:
	cargo build --release

test:
	cargo test

clippy:
	cargo clippy -- -D warnings

fmt:
	cargo fmt --check

# Cross-compile using rust-lld (bundled with Rust toolchain)
# Target: aarch64-unknown-linux-musl (fully static binary, no dependencies)
# Requires: .cargo/config.toml and .cargo/aarch64-linker.sh
# Works without any external tools - just rustup target add
# Cross-compile using cargo-zigbuild (Zig-based cross-compiler)
# Requires: cargo install cargo-zigbuild && zig installed
# `ai` bundles the ONNX detector so deploys keep AI capability (the
# runtime stays opt-in via [features.ai] enabled).
cross-build:
	cargo zigbuild --release --features v4l2-encoder,ai --target $(TARGET)

# Also supported but not enabled by default (see .cargo/config.toml):

# Cross-compile for glibc target using the same rust-lld approach
cross-build-musl:
	cargo build --release --target aarch64-unknown-linux-musl

# Cross-compile using `cross` (Docker/Podman-based)
# Requires: cargo install cross && podman or docker
cross-build-cross:
	cross build --release --target aarch64-unknown-linux-gnu

# Cross-compile using `cargo-zigbuild` (Zig-based, lighter weight)
# Requires: cargo install cargo-zigbuild
cross-build-zig:
	cargo zigbuild --release --target aarch64-unknown-linux-musl

# Cross-compile using native aarch64-gcc toolchain (no container needed)
# Requires: aarch64-linux-gnu-gcc installed on system
# On Arch: sudo pacman -S aarch64-linux-gnu-gcc
# On Debian: sudo apt install gcc-aarch64-linux-gnu
cross-build-native:
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
		cargo build --release --target aarch64-unknown-linux-gnu

deploy: build
	scp $(BINARY) $(REMOTE_HOST):$(REMOTE_DIR)/
	ssh $(REMOTE_HOST) 'sudo systemctl restart mibee-eye-raspi || true'

deploy-cross: cross-build
	scp $(CROSS_BINARY) $(REMOTE_HOST):$(REMOTE_DIR)/
	ssh $(REMOTE_HOST) 'sudo systemctl restart mibee-eye-raspi || true'

clean:
	cargo clean

run:
	cargo run --release
