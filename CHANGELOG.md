# Changelog

Notable changes to MiBee Eye (Rust implementation) are documented here.

## [Unreleased]

- **`storage-webdav` / `storage-s3` / `storage-smb` features wired up** —
  the three `StorageBackend` implementations compiled behind these flags
  referenced crates that were never declared in `Cargo.toml`, so enabling
  any of them failed the build (and kept `cargo clippy --all-features`
  red; CI only exercised default features, which is why it went
  unnoticed). They now build and their mock-HTTP test suites pass:
  optional deps `reqwest` (rustls, no OpenSSL) and `rust-s3` 0.37
  (path-style addressing for self-hosted S3-compatible endpoints,
  library-internal retries disabled so the module's documented
  exponential-backoff retry contract is the only policy). CI gains an
  `all-features` job (clippy `--all-features --all-targets` + storage
  tests) so optional features cannot rot silently again. The backends
  remain library-surface opt-ins: the GB28181 recorder still writes local
  disk directly. Two latent test-fixture bugs fixed with evidence: mock
  paths assumed second-based keys while `build_key` emits milliseconds
  (`12345000_00000.m4v`), and the retry-then-success mocks needed
  `up_to_n_times(1)` (wiremock `expect()` verifies but does not limit
  matching).
- **armv7 release target unblocked** — the ILP32 compile failure in
  gb28181-rs (`tm_gmtoff` width, upstream
  [gb28181-rs#55](https://github.com/mickeyzzc/gb28181-rs/issues/55)) is
  fixed upstream and consumed via a git pin (`fc049b8`, unreleased there —
  per the merge-≠-release cadence); `armv7-unknown-linux-musleabihf` joins
  the release matrix with the next tag. Local cross-build verified
  (full feature set, statically linked ~5 MB). No 32-bit hardware in the
  test fleet — treat the armv7 artifact as software-encode oriented
  (boards with a V4L2 M2M encoder are untested on 32-bit).

## [0.2.0] — 2026-09-13

### Added

- **Multi-board support** — the service is no longer Raspberry Pi specific.
  New `[camera]` keys `encoder` (`auto` / `hardware` / `software`, default
  `auto`) and `encoder_device` (default `/dev/video11`) select the H.264
  encoding path: V4L2 M2M hardware encoder when the probed node is capable,
  automatic fallback to the in-process software encoder otherwise
  (`camera.encoder = hardware` fails fast with diagnostics instead of
  falling back).
- In-process **openh264 software encoder** (feature `software-encoder`,
  on by default) — boards without a V4L2 M2M encoder node (x86 boxes, NAS,
  most arm SBCs) work out of the box; verified on real aarch64 hardware.
- Release artifacts for `aarch64-unknown-linux-gnu`,
  `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`
  (armv7 is blocked upstream on
  [gb28181-rs#55](https://github.com/mickeyzzc/gb28181-rs/issues/55)).
- `bench/rpi-bench.sh` — on-device benchmark script to reproduce the
  indicative footprint numbers from the README.

### Changed

- Repository renamed **mibee-eye-raspi-rs → mibee-eye-rs** (multi-board
  scope); binary and service names stay `mibee-eye-raspi-rs` for
  drop-in upgrades. Old URLs redirect.
- Relicensed **CC BY-NC 4.0 → Apache-2.0** (NOTICE carries the
  MediaMTX-MIT / Noto-OFL / openh264-BSD-2 third-party notes).

### Known issues

- The startup banner of the 0.2.0 binaries prints `v0.1.0` (hardcoded
  string); fixed on `main` by deriving from `CARGO_PKG_VERSION`.

## [0.1.0] — 2026-09-13

Initial public release (shipped under CC BY-NC 4.0; see 0.2.0 for the
Apache-2.0 relicense).

### Capabilities

- ONVIF Profile S device: Device/Media/Imaging SOAP services + WS-Discovery
  (port 8080), powered by onvif-device-rs
- GB28181 device: SIP registration (UDP/TCP), digest auth, Catalog /
  DeviceInfo / RecordInfo queries, live / playback / download RTP-PS
  streaming, SIP INFO playback control (pause/resume/seek/speed), platform
  snapshot commands — powered by gb28181-rs
- GB 35114 A-level (optional): SM2 certificate REGISTER auth + keyed-SM3
  integrity (feature `gb35114`)
- RTSP server (port 8554, RTP over TCP interleaved or UDP) and RTMP push
- Continuous local recording: H.264 segments + `index.jsonl`, retention and
  storage caps; GB28181 playback source
- Embedded SPEC v1 web admin UI (port 8088): cookie-session + CSRF auth,
  live preview, partial-merge config API, SSE events
- OSD watermark (text + real-time clock) burned into every output
- AI detection (optional, feature `ai`): NanoDet-Plus ONNX on shared
  pre-encode frames, model registry with runtime switch, fail-open
  capability gating
- Snapshot via HTTP GET
