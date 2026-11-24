# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-25

First public release. The implementation planned in the specification is
complete: frame synchronization, jitter correction, proc-amp processing,
signal-loss handling and telemetry for raw video streams, driven through
FFmpeg pipes or raw files.

### Added

- Frame synchronization with rational-rate output scheduling, clock drift
  estimation (EMA) with re-anchoring, a hardened re-lock gate and the
  hold/drop and blend rate policies
- Horizontal line jitter correction by normalised cross-correlation with
  sub-pixel (1/256 px) parabolic interpolation, plus per-line shift
  application
- Processing amplifier (proc amp): luma gain, pedestal, saturation, hue,
  gamma, and chroma delay correction (sub-pixel horizontal chroma shift)
- Signal-loss handling with four policies: last-frame hold, blackout,
  test pattern, and pass-through (unprocessed output while the signal is
  lost); `--fail-on-signal-loss` exit-code integration
- Head-switching noise concealment for the bottom lines of VHS captures
  (replicate and interpolate modes)
- Crop and pad with separate input, processing and output geometry
- Source-adaptive correction (`--auto-adapt`): input range classification
  (full / limited) and adaptive jitter search range
- Field-order auto-detection (`--field-order auto`) in addition to
  progressive / top-first / bottom-first
- Timing stamp modes (`--stamp nominal | wall-clock`) and an overload
  fallback ladder for live operation in wall-clock mode
- Telemetry as JSON Lines or CSV (fixed 24-column schema), signal-loss
  event rows, and an end-of-run summary
- Analysis-only mode (`--analyze-only`) printing a single JSON report
  (signal presence, drift, jitter statistics, luma range) without writing
  output
- Presets: `vhs`, `svhs`, `unstable-composite`, `low-latency`; standards:
  `ntsc`, `pal`, `1080i`; TOML configuration files with CLI > standard >
  TOML > preset > default resolution
- Audio/video synchronization guide (`examples/av-sync.md`)

### Supported formats

- yuv420p, yuv422p (packed), rgb24, uyvy, yuyv, yuv420p10le,
  yuv422p10le, nv12
- 10-bit and NV12 inputs are converted at the processing boundary to an
  internal 8-bit representation (see Known limitations)

### Validation

- 159 automated tests passing, including round-trip tests with PSNR
  acceptance thresholds and a 100,000-frame drift simulation
- `cargo fmt` and `cargo clippy` clean; CI on Windows and Ubuntu

### Known limitations

- Extended live operation with physical capture devices and real tape
  sources has not yet been performed. File and pipe processing are
  validated; live capture should be verified with your own hardware
  before unattended use.
- Internal processing is 8-bit. 10-bit and NV12 formats are converted at
  the input/output boundary.
- The internal `yuv422p` layout is packed (Y0 Cb Y1 Cr), matching
  FFmpeg's `yuyv422` byte order; see SPEC.md section 3.
