# software-tbc

A **software Time Base Corrector (TBC)** written in Rust.

English | [日本語](README.ja.md)

software-tbc sits in a raw-video pipe — typically between two FFmpeg
processes — and turns the unstable output of analog / legacy video
sources (VHS, S-VHS, composite game consoles, aging capture devices)
into a clean, precisely timed stream. It processes video only; audio
stays on the FFmpeg side (see [Audio](#audio)).

Key features:

- **Frame synchronization** with clock-drift estimation — absorbs
  input jitter, drift and drops in a lock-free ring buffer and
  re-emits frames on an exact rational-rate grid (30000/1001, 25/1,
  …) with hold/drop or blend policies. Sustained drift re-locks
  through a hysteresis gate, so a wandering source clock is followed
  without chasing noise.
- **Horizontal line jitter correction** — detects and removes
  per-line horizontal phase wobble, the classic VHS artefact, with
  sub-pixel (1/256 px) accuracy.
- **Head-switching noise concealment** — covers the noisy bottom
  lines of VHS captures by replicating or interpolating the clean
  lines above them.
- **Processing amplifier (proc amp)** — luma gain, black level
  (pedestal), saturation, hue, gamma and chroma delay, like the
  knobs on a hardware proc amp.
- **Signal-loss detection and handling** — when the input collapses,
  the output clock never stalls: the last frame is held, black or a
  test pattern is substituted, or the raw input is passed through
  with correction bypassed.
- **Source-adaptive correction** (`--auto-adapt`) — classifies the
  input's luma range (limited/full) and adapts the jitter search
  range to the source at runtime.
- **Runtime telemetry and analysis** — structured telemetry (JSON
  Lines or CSV) with signal-loss events and an end-of-run summary,
  plus an analysis-only mode that reports on a source without
  writing any video.
- **Geometry handling** — separate input / processing / output
  sizes with crop and padding, for captures whose active picture
  does not fill the frame.

## Quick start

```sh
cargo build --release
```

Post-process an unstable capture, with FFmpeg doing the codecs on
both sides:

```powershell
ffmpeg -i capture.mp4 -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 - |
  software-tbc --width 720 --height 480 --format yuv422p --preset vhs |
  ffmpeg -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 -i - -c:v libx264 out.mp4
```

Raw video carries no metadata: size, pixel format and frame rate
must be stated explicitly on both sides of the pipe, and they must
match software-tbc's own `--width` / `--height` / `--format`.
Everything software-tbc prints (telemetry, notices, errors) goes to
stderr — stdout carries video bytes only. Defaults when flags are
omitted: 720×480, `yuv422p`, bottom-field-first, 30000/1001.

## Supported formats and standards

| Pixel formats (wire) | Notes |
|---|---|
| `yuv422p`, `yuv420p`, `rgb24` | Native pipeline formats (`yuv422p` is the packed YUYV byte order) |
| `uyvy`, `yuyv` | Converted at the boundary |
| `yuv420p10le`, `yuv422p10le` | 10-bit; converted to 8-bit internally (see Limitations) |
| `nv12` | Semi-planar 4:2:0; converted to/from `yuv420p` at the boundary |

`--standard` sets geometry, rate and field order in one step:
`ntsc` (720×480, 30000/1001, BFF), `pal` (720×576, 25/1, TFF),
`1080i` (1920×1080, 30000/1001, TFF). Individual flags override it.

`--field-order` accepts `progressive`, `top-first`, `bottom-first`,
or `auto`: the field order is detected from the stream, with the
otherwise-configured order as the fallback until detection locks.

Input and output are headerless raw video: stdin/stdout pipes, a
single raw file, or a sequence of files (`%` in the path, e.g.
`frames/frame_%06d.raw`). There is no direct capture-device support;
capture cards are fed through FFmpeg (e.g. `-f dshow` on Windows).

## How it works

```
            ┌───────────── tbc-cli (software-tbc binary) ─────────────┐
            │  input thread → SPSC ring buffer → processing thread    │
            └──────┬──────────────────────────────────────┬─────────────┘
              FrameSource                            FrameSink      (tbc-io)
                   │                                        │
            ┌──────▼────────────────────────────────────────▼──────────┐
            │                      tbc-core                          │
            │  sync → head-switch mask → jitter → proc-amp → guard   │
            │  (pure transforms, no I/O, integer-ns timing)          │
            └────────────────────────────────────────────────────────┘

            tbc-sim: synthetic unstable-video generator + benchmarks
```

| Crate | Responsibility |
|---|---|
| `tbc-core` | The signal-processing pipeline. No I/O, no threads, no wall clock. |
| `tbc-io` | `FrameSource` / `FrameSink` traits; raw files, stdio pipes, file sequences. |
| `tbc-sim` | Generates degraded video (jitter, drift, dropouts) so the TBC can be measured without real hardware. |
| `tbc-cli` | The `software-tbc` binary: CLI flags, TOML config, presets, pipeline wiring. |

The full design — algorithms, formulas, performance budgets, the
complete config schema — lives in [SPEC.md](SPEC.md).

## Presets

| Preset | Intended source | Buffer | Rate policy | Notes |
|---|---|---|---|---|
| `vhs` | Worn VHS tapes | 12 frames | Blend | Jitter correction up to ±12 px, slight pedestal lift |
| `svhs` | S-VHS / cleaner tapes | 8 frames | Hold/drop | Jitter correction up to ±8 px |
| `unstable-composite` | Very unstable composite sources | 16 frames | Hold/drop | Test pattern on signal loss, 500 ms loss timeout |
| `low-latency` | Live preview / streaming | 2 frames | Hold/drop | Jitter correction off; ≈2 frames of added latency |

## Configuration

Every parameter can be set by CLI flag or in a TOML file
(`--config tbc.toml`). Layers resolve in this order, later layers
winning:

```
defaults  →  preset  →  TOML file  →  --standard  →  individual CLI flags
```

The most useful flags day to day: `--preset`, `--standard`,
`--format`, `--field-order auto`, `--head-switch-lines N` (conceal
VHS head-switching noise), `--auto-adapt`, `--signal-loss
last-frame-hold|blackout|test-pattern|passthrough`, and the
geometry flags (`--input-width` …, `--crop-left` …, `--pad-left` …)
for off-centre captures. The complete list is in SPEC.md §6 (CLI)
and §7 (TOML schema); `software-tbc --help` shows the same flags.

## Live operation

For live capture, feed frames with `--stamp wall-clock` so the
synchronizer works from arrival times (the default `nominal` mode
derives timestamps from the frame sequence and rate, which suits
file processing). In wall-clock mode an **overload fallback**
protects the output rate if processing cannot keep up: it first
skips jitter correction, then degrades blending to hold/drop
(`--no-overload-fallback` disables it; the ladder is
preset-dependent and `low-latency` has nothing left to shed).

Signal-loss handling policies (`--signal-loss`):

| Policy | Behaviour while the signal is lost |
|---|---|
| `last-frame-hold` | Repeat the last good frame |
| `blackout` | Emit black frames |
| `test-pattern` | Emit a test pattern |
| `passthrough` | Emit the synchronizer's frames with all correction stages bypassed — estimators are not fed, so their state survives the outage untouched |

With `--fail-on-signal-loss`, a run that ends in a signal-loss
state exits with code 4.

## Telemetry and analysis

Telemetry is written to stderr once per second (interval:
`telemetry_interval_ms` in TOML): latency, buffer fill,
dropped/duplicated frames, drift in ppm, re-lock state, per-frame
processing time and deadline misses, plus an immediate event row
whenever the signal is lost or recovered, and a one-line summary at
the end of every run. `--telemetry-format csv` switches to a fixed
24-column CSV schema for spreadsheet/log pipelines.

To inspect a source without processing it, use `--analyze-only`:
the pipeline runs against a discarding sink, the output path is
never opened, and a single JSON report is printed to stdout —
frame counts, signal presence, jitter statistics, drift, and the
observed luma range:

```sh
software-tbc --analyze-only --input capture.raw --width 720 --height 480 --format yuv422p
```

Example output (a synthetic test pattern, 30 frames):

```json
{"type":"analysis","width":720,"height":480,"format":"yuv422p","runtime_s":3.598,"frames_in":30,"frames_out":30,"dropped_frames":0,"duplicated_frames":0,"signal_present":true,"signal_loss_events":0,"signal_lost_ms":0,"drift_ppm":0,"drift_valid":true,"jitter_samples":13920,"jitter_frames":29,"jitter_unestimated_frames":1,"jitter_bypassed_frames":0,"jitter_mean_abs_px":2.931,"jitter_mean_signed_px":-2.931,"jitter_max_abs_px":3.098,"luma_min":0,"luma_max":255,"range_class":"limited"}
```

Exit code is 0 when a signal is present at the end, 4 when the
stream ends without one (or the input is empty).

## Audio

software-tbc does not touch audio. Because the TBC adds real-time
latency (buffer/2 + 1 frames), live recordings need the audio
delayed on the FFmpeg side — while file processing, whose timeline
is rebuilt from the frame sequence, does not. The full recipes,
latency table and verification procedure are in
[examples/av-sync.md](examples/av-sync.md).

## Performance

Reference measurements (release build, full pipeline at SD
resolution, `tbc-sim` bench on a shared virtual machine — treat as
indicative, not guaranteed):

| Standard | Average processing time | Throughput | Frame interval |
|---|---|---|---|
| NTSC 720×480 | 5.6 ms/frame | 178 fps | 33.4 ms |
| PAL 720×576 | 6.9 ms/frame | 145 fps | 40 ms |

That is roughly 6× headroom against real time at SD on a modest
shared VM; a dedicated machine will do better. Reproduce with
`cargo run --release -p tbc-sim --example bench`.

Added latency is `buffer_frames / 2 + 1` frames (default ≈5 frames,
≈167 ms at 29.97 fps; `low-latency` ≈2 frames, ≈67 ms).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Completed normally |
| 2 | Configuration error (bad flag, TOML or value combination) |
| 3 | I/O error (input unreadable, output unwritable) |
| 4 | Ended in a signal-loss state (with `--fail-on-signal-loss`, or analysis-only mode ending without a signal) |

## Status and limitations

The core implementation is **complete**: all eight development
phases are implemented and the test suite passes **159 tests**,
including round-trip quality tests (a synthetically degraded stream
must come out measurably cleaner) and long-run drift simulations.

What is verified: file- and pipe-based processing with synthetic
and generated inputs, on Windows and Linux CI configurations.
What is **not** yet verified: extended operation with physical
capture devices and real tape sources over long sessions. The
signal-loss paths in particular are exercised by unit tests with
forced states, because file input cannot produce invalid frames.
software-tbc is functional and ready for real-world evaluation;
validate it in your own capture chain before unattended production
use.

Known limitations:

- Internal processing is 8-bit; 10-bit formats are converted at the
  boundary (native 16-bit pipeline is future work, SPEC §10).
- The internal `yuv422p` layout is the packed YUYV byte order, kept
  for compatibility and documented in SPEC §3.
- No direct capture-device input (use FFmpeg as the front end).
- Output pacing is input-driven: if the input stalls, the output
  stalls with it.

## Development

```sh
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Contributors using coding agents: read [AGENTS.md](AGENTS.md) first.

## License

MIT — see [LICENSE](LICENSE).
