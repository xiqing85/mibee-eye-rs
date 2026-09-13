# Contributing

Welcome! We're glad you want to contribute to MiBee Eye (Rust implementation).

## Development Setup

- Rust 1.88+ (install via [`rustup`](https://rustup.rs); the repo pins a toolchain in `rust-toolchain.toml`)
- V4L2 development headers for native builds (Debian: `sudo apt install libv4l-dev`)

```bash
cargo build --release   # build
cargo test              # tests
cargo clippy -- -D warnings   # lint (warnings are errors)
cargo fmt --check       # format check
```

## Code Style

- `cargo fmt` formatting; `clippy` clean with `-D warnings`
- No panics on fallible operations in service paths: propagate `Result`, no
  `.unwrap()`/`.expect()` on operations that can fail at runtime
- Public APIs carry doc comments; protocol wire formats keep golden-string
  tests as contracts
- The default feature set is `v4l2-encoder` + `software-encoder` — keep the
  build working with and without optional features (`ai`, `gb35114`, …)

## Commit Convention

Use conventional commits:

- `feat(camera): add encoder probe fallback`
- `fix(gb28181): retransmit INVITE response on loss`
- `docs: expand hardware support matrix`
- `ci: pin zigbuild targets`
- `test(config): cover encoder env overrides`

## Pull Request Process

1. Fork the repository
2. Create a feature branch
3. Make commits following the conventional format
4. Push to your fork
5. Open a pull request against `main` (protected: linear history, PR-only merges)
6. Ensure CI passes — repo hygiene gate + `cargo fmt`/`clippy`/`test` on stable
7. Address review feedback

Tests land in the same change as the code they cover; bug fixes come with a
failing test that reproduces the bug first.

## Issue Tracker

Issues for both MiBee Eye implementations (Rust and Go) are tracked in one
place: [xiqing85/mibee-eye-go/issues](https://github.com/xiqing85/mibee-eye-go/issues).
Prefix the title with `[rs]` for Rust-specific reports.

## Protocol Layers

ONVIF and GB28181 protocol semantics (SOAP/XML element order, SIP state
machines, SDP, RTP/PS muxing, MANSCDP) live in the upstream libraries
[onvif-device-rs](https://github.com/mickeyzzc/onvif-rs) and
[gb28181-rs](https://github.com/mickeyzzc/gb28181-rs). Protocol fixes and
additions go there first; this repo consumes them via versioned git pins and
only contains capture, encoding, streaming and web glue.

## Project Structure

```
src/
  main.rs             # entry point, pipeline assembly
  camera/             # V4L2 capture, encoder probe, M2M + openh264 encoders
  pipeline/           # capture → watermark → encode → fan-out
  watermark.rs        # OSD text + clock burn-in
  h264/               # Annex-B helpers
  streaming/          # RTSP server, RTMP push
  recording/          # continuous segments + index.jsonl (GB28181 playback)
  storage/            # segment storage backends (local; S3/SMB/WebDAV optional)
  web/                # SPEC v1 REST API + SSE + embedded UI (rust-embed)
  ai/                 # NanoDet detection (feature ai)
  motion/             # frame-delta detector (not yet wired to the web API)
  hardware/           # board capability gating (AI memory/model checks)
  config/             # TOML config + env overrides
  gb28181_snapshot.rs # platform snapshot command glue
  gb35114_glue.rs     # GB 35114 A-level auth glue (feature gb35114)
  ptz/                # PTZ plumbing
  features/           # reserved feature stubs (multi-camera / webrtc / h265)
static/               # web UI assets (synced from mibee-webui, do not hand-edit)
deploy/               # systemd unit + install script
bench/                # on-device benchmark script
docs/                 # engineering docs (feature designs, NVR notes)
```
