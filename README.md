# software-tbc

A **software Time Base Corrector (TBC)** written in Rust.

It sits in a raw-video pipe — typically between two FFmpeg processes — and turns
the unstable output of analog / legacy video sources (VHS, S-VHS, composite
game consoles, aging capture devices) into a clean, precisely timed stream:

- **Frame synchronisation** — absorbs input clock jitter, drift and drops in a
  lock-free ring buffer and re-emits frames aligned to an exact rational target
  rate (29.97, 59.94, 25, 50 fps, …) with hold/drop or blend policies.
- **Line jitter correction** — detects and removes per-line horizontal phase
  wobble, the classic VHS artefact.
- **Proc-amp controls** — luma gain, black level (pedestal), saturation, hue
  and gamma, like the knobs on a hardware proc-amp.
- **Signal-loss guard** — freezes, blacks out or shows a test pattern when the
  input signal collapses, without ever stalling the output clock.
- **Telemetry** — latency, buffer fill, drop/duplicate counts and jitter
  estimates as structured logs.

## Architecture

```
            ┌───────────── tbc-cli (software-tbc binary) ─────────────┐
            │  input thread → SPSC ring buffer → processing thread    │
            └──────┬──────────────────────────────────────┬─────────────┘
              FrameSource                            FrameSink      (tbc-io)
                   │                                        │
            ┌──────▼────────────────────────────────────────▼──────────┐
            │                      tbc-core                          │
            │  sync → line jitter → field → proc-amp → guard         │
            │  (pure transforms, no I/O, integer-ns timing)          │
            └────────────────────────────────────────────────────────┘

            tbc-sim: synthetic unstable-video generator + benchmarks
```

| Crate | Responsibility |
|---|---|
| `tbc-core` | The signal-processing pipeline. No I/O, no threads, no wall clock. |
| `tbc-io` | `FrameSource` / `FrameSink` traits; raw file and stdio pipes; plugin point for Windows capture devices. |
| `tbc-sim` | Generates degraded video (jitter, drift, dropouts) so the TBC can be measured without real hardware. |
| `tbc-cli` | The `software-tbc` binary: CLI flags, TOML config, presets, pipeline wiring. |

The full design — algorithms, formulas, performance budgets, config schema —
lives in [SPEC.md](SPEC.md).

## Usage

Build:

```sh
cargo build --release
```

Post-process an unstable capture with FFmpeg doing the codecs on both sides:

```powershell
ffmpeg -i capture.mp4 -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 - |
  software-tbc --width 720 --height 480 --format yuv422p --preset vhs |
  ffmpeg -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 -i - -c:v libx264 out.mp4
```

Presets: `vhs`, `svhs`, `unstable-composite`, `low-latency`.
Every parameter can also be set in a TOML file (`--config tbc.toml`);
CLI flags override the file. See SPEC.md §7 for the schema.

## Status and roadmap

Early development. The workspace scaffold, core types and raw pipe I/O are in
place; the pipeline stages land phase by phase (tracked in Issues):

- **Phase 1** — frame pool, SPSC ring buffer, config/error plumbing
- **Phase 2** — rate alignment, line jitter correction, field handling,
  proc-amp, signal-loss guard, telemetry
- **Phase 3** — CLI wiring, TOML config and presets, sequence files, FFmpeg recipes
- **Phase 4** — simulator, benchmarks, round-trip quality tests

## Development

```sh
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Contributors using coding agents: read [AGENTS.md](AGENTS.md) first.

## License

MIT — see [LICENSE](LICENSE).
