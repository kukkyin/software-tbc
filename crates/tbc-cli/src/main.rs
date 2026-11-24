//! software-tbc command-line front end.
//!
//! Pipeline (SPEC.md §6): a reader thread feeds the SPSC ring buffer;
//! the main thread runs sync -> jitter -> proc-amp -> guard, writes
//! frames out, and reports telemetry as JSON Lines on stderr.
//!
//! Exit codes: 0 = clean end of stream, 2 = argument/config error,
//! 3 = input/output error, 4 = ended in signal loss while
//! `--fail-on-signal-loss` was given.

mod config;

use clap::Parser;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tbc_core::adapt::{JitterAdapter, RangeClass, RangeEstimator};
use tbc_core::field::FieldOrderDetector;
use tbc_core::guard::{GuardEvent, SignalGuard};
use tbc_core::head_switch::apply_head_switch;
use tbc_core::jitter::{apply_shifts, JitterDetector, LineShift};
use tbc_core::procamp::ProcAmp;
use tbc_core::sync::ring::RingBuffer;
use tbc_core::sync::FrameSynchronizer;
use tbc_core::telemetry::TelemetryCollector;
use tbc_core::{
    FieldOrder, FrameRate, PixelFormat, RatePolicy, SignalLossPolicy, TbcError, VideoFrame,
};
use tbc_io::{
    FrameSink, FrameSource, IoError, RawGeometry, RawReader, RawWriter, SequenceSink,
    SequenceSource,
};

/// Software TBC: read raw video, correct its time base, write raw video.
///
/// Typical use is in a pipe with FFmpeg, e.g.
/// `ffmpeg -i in.mp4 -f rawvideo -pix_fmt yuv422p - | software-tbc ... | ffmpeg ...`
#[derive(Parser, Debug)]
#[command(name = "software-tbc", version, about)]
pub struct Args {
    /// Input raw file, `-` for stdin, or a directory + `%06d` pattern
    /// (e.g. `frames/frame_%06d.raw`) for a file sequence.
    #[arg(long, default_value = "-")]
    input: String,
    /// Output raw file, `-` for stdout, or a sequence pattern like input.
    #[arg(long, default_value = "-")]
    output: String,
    /// Frame width in pixels.
    #[arg(long)]
    width: Option<u32>,
    /// Frame height in pixels.
    #[arg(long)]
    height: Option<u32>,
    /// Pixel format: yuv420p | yuv422p | rgb24 | uyvy | yuyv |
    /// yuv420p10le | yuv422p10le.
    #[arg(long)]
    format: Option<String>,
    /// Field order: progressive | top-first | bottom-first | auto
    /// (detect from the stream; the otherwise-configured order is
    /// the fallback until detection locks).
    #[arg(long)]
    field_order: Option<String>,
    /// Conceal this many bottom lines (head-switching noise);
    /// 0 = off.
    #[arg(long)]
    head_switch_lines: Option<u32>,
    /// Head-switch concealment mode: replicate | interpolate.
    #[arg(long)]
    head_switch_mode: Option<String>,
    /// Input frame width (default: the processing width).
    #[arg(long)]
    input_width: Option<u32>,
    /// Input frame height (default: the processing height).
    #[arg(long)]
    input_height: Option<u32>,
    /// Left edge of the processing window inside the input frame.
    #[arg(long)]
    crop_left: Option<u32>,
    /// Top edge of the processing window inside the input frame.
    #[arg(long)]
    crop_top: Option<u32>,
    /// Output frame width (default: the processing width).
    #[arg(long)]
    output_width: Option<u32>,
    /// Output frame height (default: the processing height).
    #[arg(long)]
    output_height: Option<u32>,
    /// Left position of the processed frame inside the output frame.
    #[arg(long)]
    pad_left: Option<u32>,
    /// Top position of the processed frame inside the output frame.
    #[arg(long)]
    pad_top: Option<u32>,
    /// Named preset: vhs | svhs | unstable-composite | low-latency.
    #[arg(long)]
    preset: Option<String>,
    /// Signal standard preset for geometry, rate and field order:
    /// ntsc | pal | 1080i. Individual flags override it.
    #[arg(long)]
    standard: Option<String>,
    /// TOML configuration file (CLI flags override it).
    #[arg(long)]
    config: Option<String>,
    /// Input timestamp mode: nominal (sequence x rate, deterministic)
    /// or wall-clock (arrival time; for live capture). Sequence input
    /// always uses nominal.
    #[arg(long)]
    stamp: Option<String>,
    /// Signal-loss policy: last-frame-hold | blackout | test-pattern |
    /// passthrough (while the signal is lost, send the input through
    /// unprocessed instead of substituting a fallback frame).
    #[arg(long)]
    signal_loss: Option<String>,
    /// Telemetry format: json (JSON Lines) or csv (fixed columns).
    #[arg(long)]
    telemetry_format: Option<String>,
    /// Disable the overload fallback (staged degradation that only
    /// engages in wall-clock stamp mode).
    #[arg(long)]
    no_overload_fallback: bool,
    /// Auto-adapt mode: estimate the luma range (limited/full) and
    /// adapt the jitter search range to the source at runtime.
    #[arg(long)]
    auto_adapt: bool,
    /// Analyze only: run the pipeline, write no output (the output
    /// path is never opened), and print a JSON analysis report to
    /// stdout. Exit code 4 if the stream ends without a signal.
    #[arg(long)]
    analyze_only: bool,
    /// Stop after emitting this many output frames.
    #[arg(long)]
    max_frames: Option<u64>,
    /// Exit with code 4 if the stream ends in a signal-loss state.
    #[arg(long)]
    fail_on_signal_loss: bool,
}

fn main() {
    let args = Args::parse();
    let cfg = match config::resolve(&args) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("software-tbc: {e}");
            std::process::exit(2);
        }
    };
    std::process::exit(run(&cfg));
}

/// Split a sequence path like `frames/frame_%06d.raw` into dir + pattern.
fn split_sequence(path: &str) -> Option<(String, String)> {
    if !path.contains('%') {
        return None;
    }
    match path.rfind(['/', '\\']) {
        Some(i) => Some((path[..i].to_string(), path[i + 1..].to_string())),
        None => Some((".".to_string(), path.to_string())),
    }
}

/// Synthesizes presentation timestamps for the reader thread
/// (Issue #20). In `Nominal` mode the stamp depends only on the frame
/// sequence; in `WallClock` mode it is the elapsed time since the
/// first frame, so downstream drift estimation sees the source's real
/// clock. Integer nanoseconds throughout, no float time math.
struct Stamper {
    mode: config::StampMode,
    rate: FrameRate,
    start: Option<Instant>,
    sequence: u64,
}

impl Stamper {
    fn new(mode: config::StampMode, rate: FrameRate) -> Self {
        Stamper {
            mode,
            rate,
            start: None,
            sequence: 0,
        }
    }

    /// Stamp the next frame. `now` is the moment its read completed;
    /// it is ignored in `Nominal` mode.
    fn stamp(&mut self, now: Instant) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        match self.mode {
            config::StampMode::Nominal => {
                ((sequence as u128 * self.rate.den as u128 * 1_000_000_000) / self.rate.num as u128)
                    as u64
            }
            config::StampMode::WallClock => {
                let start = *self.start.get_or_insert(now);
                now.saturating_duration_since(start).as_nanos() as u64
            }
        }
    }
}

/// Frames of sustained overload before the stage rises (~1 s at 30 fps).
const OVER_STREAK_FRAMES: u32 = 30;
/// Frames of sustained headroom before the stage falls (~10 s).
const UNDER_STREAK_FRAMES: u32 = 300;
/// Minimum frames between stage changes (~5 s), against flapping.
const MIN_DWELL_FRAMES: u32 = 150;

/// Overload fallback controller (Issue #21). Watches the processing
/// time per output frame (integer EMA, alpha 1/16) against the frame
/// period and degrades the pipeline in stages when it cannot keep up:
/// stage 1 skips jitter correction, stage 2 forces hold/drop over
/// blend. Which stages exist depends on the configured levers (jitter
/// on/off, blend on/off). Live-only by construction: the owner enables
/// it solely in wall-clock stamp mode, because for file processing a
/// slow machine must cost time, not quality. Hysteresis is asymmetric
/// (fast attack, slow release) plus a minimum dwell per stage.
struct OverloadController {
    enabled: bool,
    has_jitter: bool,
    has_blend: bool,
    period_ns: i64,
    stage: u8,
    events: u64,
    ema_proc_ns: Option<i64>,
    over_streak: u32,
    under_streak: u32,
    since_change: u32,
}

impl OverloadController {
    fn new(enabled: bool, has_jitter: bool, has_blend: bool, period_ns: u64) -> Self {
        OverloadController {
            enabled,
            has_jitter,
            has_blend,
            period_ns: period_ns as i64,
            stage: 0,
            events: 0,
            ema_proc_ns: None,
            over_streak: 0,
            under_streak: 0,
            since_change: MIN_DWELL_FRAMES,
        }
    }

    fn max_stage(&self) -> u8 {
        self.has_jitter as u8 + self.has_blend as u8
    }

    fn stage(&self) -> u8 {
        self.stage
    }

    /// Cumulative count of stage rises.
    fn events(&self) -> u64 {
        self.events
    }

    fn skip_jitter(&self) -> bool {
        self.has_jitter && self.stage >= 1
    }

    fn force_hold_drop(&self) -> bool {
        if !self.has_blend {
            return false;
        }
        // Blend is the second lever when jitter exists, else the first.
        let needed = if self.has_jitter { 2 } else { 1 };
        self.stage >= needed
    }

    /// Feed one output frame's processing time.
    fn observe(&mut self, proc_ns: u64) {
        if !self.enabled || self.max_stage() == 0 || self.period_ns <= 0 {
            return;
        }
        let sample = proc_ns as i64;
        let ema = match self.ema_proc_ns {
            None => sample,
            Some(e) => e + (sample - e) / 16,
        };
        self.ema_proc_ns = Some(ema);
        self.since_change = self.since_change.saturating_add(1);
        if ema > self.period_ns {
            self.over_streak += 1;
            self.under_streak = 0;
        } else if ema * 2 < self.period_ns {
            self.under_streak += 1;
            self.over_streak = 0;
        } else {
            self.over_streak = 0;
            self.under_streak = 0;
        }
        if self.since_change < MIN_DWELL_FRAMES {
            return;
        }
        if self.over_streak >= OVER_STREAK_FRAMES && self.stage < self.max_stage() {
            self.stage += 1;
            self.events += 1;
            self.over_streak = 0;
            self.since_change = 0;
        } else if self.under_streak >= UNDER_STREAK_FRAMES && self.stage > 0 {
            self.stage -= 1;
            self.under_streak = 0;
            self.since_change = 0;
        }
    }
}

fn open_source(cfg: &config::CliConfig) -> Result<Box<dyn FrameSource + Send>, IoError> {
    let geometry = RawGeometry {
        width: cfg.input_width,
        height: cfg.input_height,
        format: cfg.core.format,
    };
    if let Some((dir, pattern)) = split_sequence(&cfg.input) {
        return Ok(Box::new(SequenceSource::new(dir, pattern, geometry, 0)));
    }
    let reader: Box<dyn Read + Send> = if cfg.input == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(BufReader::new(std::fs::File::open(&cfg.input)?))
    };
    Ok(Box::new(RawReader::new(reader, geometry)))
}

fn open_sink(cfg: &config::CliConfig) -> Result<Box<dyn FrameSink + Send>, IoError> {
    if let Some((dir, pattern)) = split_sequence(&cfg.output) {
        return Ok(Box::new(SequenceSink::new(dir, pattern, 0)));
    }
    let writer: Box<dyn Write + Send> = if cfg.output == "-" {
        Box::new(BufWriter::new(std::io::stdout()))
    } else {
        Box::new(BufWriter::new(std::fs::File::create(&cfg.output)?))
    };
    Ok(Box::new(RawWriter::new(writer)))
}

/// The format the pipeline processes for a declared wire format:
/// packed 422 (Issue #24) and the 10-bit formats (Issue #28) are
/// converted at the boundary; everything else is processed as-is.
fn pipeline_format(wire: PixelFormat) -> PixelFormat {
    match wire {
        PixelFormat::Yuv422p10 => PixelFormat::Yuv422p,
        PixelFormat::Yuv420p10 => PixelFormat::Yuv420p,
        PixelFormat::Nv12 => PixelFormat::Yuv420p,
        f if f.is_packed_422() => PixelFormat::Yuv422p,
        f => f,
    }
}

/// Re-pack a pipeline frame into the declared wire format (Issue #24).
/// Packed formats (uyvy/yuyv) are converted from the pipeline's
/// yuv422p representation and 10-bit formats from their 8-bit
/// equivalent (Issue #28); every other format passes through unchanged.
fn to_wire_frame(wire: PixelFormat, frame: VideoFrame) -> Result<VideoFrame, TbcError> {
    if wire.is_10bit() {
        VideoFrame::from_8bit(&frame, wire)
    } else if wire.is_packed_422() {
        VideoFrame::from_yuv422p(&frame, wire)
    } else if wire == PixelFormat::Nv12 {
        VideoFrame::from_yuv420p(&frame, wire)
    } else {
        Ok(frame)
    }
}

/// Processing-side state: everything the output path touches.
struct Pipeline {
    sync: FrameSynchronizer,
    procamp: ProcAmp,
    detector: Option<JitterDetector>,
    guard: SignalGuard,
    collector: TelemetryCollector,
    controller: OverloadController,
    /// Auto field-order mode (Issue #26): the reader thread's
    /// detector publishes the effective order here, and `emit` tags
    /// every output frame with it. `None` outside auto mode.
    auto_order: Option<Arc<Mutex<FieldOrder>>>,
    /// Auto-adapt estimators (Issue #32). `None` outside auto mode.
    range_estimator: Option<RangeEstimator>,
    jitter_adapter: Option<JitterAdapter>,
    applied_hold_drop: bool,
    frames_out: u64,
    last_emitted_seq: Option<u64>,
    last_output_pts: u64,
    last_input_pts: u64,
    prev_dropped: u64,
    prev_duplicated: u64,
    output_error: bool,
    /// Signal-loss bookkeeping for telemetry (Issue #36): episode
    /// count, the current episode's start, and total lost time.
    signal_loss_events: u64,
    lost_since: Option<Instant>,
    signal_lost_ms_total: u64,
    /// Analysis statistics (Issue #37); `Some` only in analyze-only
    /// mode, where the pipeline runs against a discarding sink.
    analyze: Option<AnalyzeStats>,
}

/// Statistics for analyze-only mode (Issue #37). Fixed-size
/// counters only; per-frame work is a handful of integer updates
/// plus the range estimator's own luma scan (shared with
/// auto-adapt, never a second scan of the frame).
#[derive(Default)]
struct AnalyzeStats {
    /// Observer instance of the core range estimator: the report's
    /// classification and luma range are exactly what auto-adapt
    /// would see, with no analysis-only thresholds.
    range: RangeEstimator,
    /// Jitter line-shift samples actually estimated. Frames whose
    /// estimate did not run (signal-loss bypass, no estimate) never
    /// enter the sums, so the means describe estimated frames only.
    jitter_samples: u64,
    jitter_frames: u64,
    jitter_sum_abs_q8: u128,
    jitter_sum_signed_q8: i128,
    jitter_max_abs_q8: u32,
    jitter_unestimated_frames: u64,
    bypassed_frames: u64,
}

impl AnalyzeStats {
    fn observe(&mut self, frame: &VideoFrame) {
        self.range.observe(frame);
    }

    fn note_shifts(&mut self, shifts: &[LineShift]) {
        self.jitter_frames += 1;
        for s in shifts {
            self.jitter_samples += 1;
            self.jitter_sum_abs_q8 += s.shift_q8.unsigned_abs() as u128;
            self.jitter_sum_signed_q8 += s.shift_q8 as i128;
            self.jitter_max_abs_q8 = self.jitter_max_abs_q8.max(s.shift_q8.unsigned_abs());
        }
    }

    /// Mean absolute shift in pixels, or `None` with no samples.
    fn jitter_mean_abs_px(&self) -> Option<f64> {
        (self.jitter_samples > 0)
            .then(|| self.jitter_sum_abs_q8 as f64 / self.jitter_samples as f64 / 256.0)
    }

    /// Mean signed shift in pixels (a directional bias), or `None`.
    fn jitter_mean_signed_px(&self) -> Option<f64> {
        (self.jitter_samples > 0)
            .then(|| self.jitter_sum_signed_q8 as f64 / self.jitter_samples as f64 / 256.0)
    }

    /// Largest absolute shift in pixels, or `None` with no samples.
    fn jitter_max_abs_px(&self) -> Option<f64> {
        (self.jitter_samples > 0).then(|| self.jitter_max_abs_q8 as f64 / 256.0)
    }
}

/// A sink that discards every frame (Issue #37). Analyze-only mode
/// runs the full pipeline against it; the configured output path is
/// never opened, created or truncated.
struct NullSink;

impl FrameSink for NullSink {
    fn write_frame(&mut self, _frame: &VideoFrame) -> Result<(), IoError> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

impl Pipeline {
    /// Write one frame out. Returns false when the run should stop
    /// (max_frames reached or a write failed).
    fn emit(
        &mut self,
        cfg: &config::CliConfig,
        sink: &mut Box<dyn FrameSink + Send>,
        mut out: VideoFrame,
    ) -> bool {
        // In auto mode every output frame carries the currently
        // effective field order (Issue #26).
        if let Some(shared) = &self.auto_order {
            out.field_order = *shared.lock().expect("order slot");
        }
        // Every output frame leaves in the declared wire format,
        // including guard fallbacks (Issue #24).
        let out = match to_wire_frame(cfg.core.format, out) {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("software-tbc: output conversion error: {e}");
                self.output_error = true;
                return false;
            }
        };
        // Pad to the output geometry (Issue #31).
        let out = if cfg.pad_left > 0
            || cfg.pad_top > 0
            || cfg.output_width != cfg.core.width
            || cfg.output_height != cfg.core.height
        {
            match tbc_core::geometry::pad_frame(
                &out,
                cfg.pad_left,
                cfg.pad_top,
                cfg.output_width,
                cfg.output_height,
            ) {
                Ok(frame) => frame,
                Err(e) => {
                    eprintln!("software-tbc: output padding error: {e}");
                    self.output_error = true;
                    return false;
                }
            }
        } else {
            out
        };
        if let Err(e) = sink.write_frame(&out) {
            eprintln!("software-tbc: output error: {e}");
            self.output_error = true;
            return false;
        }
        self.frames_out += 1;
        self.last_emitted_seq = Some(out.sequence);
        self.last_output_pts = out.pts_ns;
        !cfg.max_frames
            .map(|m| self.frames_out >= m)
            .unwrap_or(false)
    }

    /// Produce and emit the next output frame, if one is available.
    fn process_output(
        &mut self,
        cfg: &config::CliConfig,
        sink: &mut Box<dyn FrameSink + Send>,
    ) -> bool {
        if self.guard.is_lost() && cfg.core.signal_loss != SignalLossPolicy::Passthrough {
            // Substitute the policy fallback until the signal recovers.
            // Fallbacks are generated in the pipeline's format; `emit`
            // re-packs them into the wire format (Issue #24).
            // Passthrough (Issue #34) substitutes nothing and falls
            // through to the unprocessed path below.
            let format = pipeline_format(cfg.core.format);
            if let Some(mut fb) = self
                .guard
                .fallback_frame(cfg.core.width, cfg.core.height, format)
            {
                fb.sequence = self.frames_out;
                fb.pts_ns = self.last_output_pts;
                return self.emit(cfg, sink, fb);
            }
            return true;
        }
        match self.sync.next_output() {
            Some(mut out) => {
                // Passthrough (Issue #34): while the guard reports
                // signal loss (reachable here only with the
                // passthrough policy), every correction stage is
                // bypassed and the estimators are not fed, so their
                // state survives the outage untouched. Synchronisation
                // above and wire conversion / padding in `emit` still
                // apply, and no frame is fabricated: when the
                // synchronizer returns nothing, nothing is emitted.
                if self.guard.is_lost() {
                    if let Some(a) = self.analyze.as_mut() {
                        a.bypassed_frames += 1;
                    }
                    return self.emit(cfg, sink, out);
                }
                // Conceal head-switching noise before jitter
                // estimation sees the frame (Issue #29).
                apply_head_switch(&mut out, &cfg.core.head_switch);
                // Analyze-only (Issue #37): the observer estimator
                // sees the same synced frame auto-adapt would see.
                if let Some(a) = self.analyze.as_mut() {
                    a.observe(&out);
                }
                // Auto-adapt (Issue #32): the range estimator sees
                // the synced frame before any level processing; a
                // locked reclassification rebuilds the proc-amp.
                if let Some(est) = self.range_estimator.as_mut() {
                    if let Some(class) = est.observe(&out) {
                        let mut pc = cfg.core.procamp;
                        pc.full_range = class == RangeClass::Full;
                        self.procamp = ProcAmp::new(pc);
                        eprintln!(
                            "software-tbc: auto-adapt: input range classified as {}",
                            if class == RangeClass::Full {
                                "full"
                            } else {
                                "limited"
                            }
                        );
                    }
                }
                if !self.controller.skip_jitter() {
                    if let Some(det) = self.detector.as_mut() {
                        let estimated = det.estimate(&out);
                        if let Some(a) = self.analyze.as_mut() {
                            match &estimated {
                                Some(shifts) => a.note_shifts(shifts),
                                None => a.jitter_unestimated_frames += 1,
                            }
                        }
                        if let Some(shifts) = estimated {
                            let _ = apply_shifts(&mut out, &shifts);
                            // Auto-adapt (Issue #32): follow the
                            // observed jitter with the search range.
                            let next = self
                                .jitter_adapter
                                .as_mut()
                                .and_then(|ad| ad.observe(&shifts));
                            if let Some(new_max) = next {
                                det.set_max_shift_px(new_max);
                                eprintln!(
                                    "software-tbc: auto-adapt: jitter search range now +/-{new_max}px"
                                );
                            }
                        }
                    }
                }
                self.procamp.apply(&mut out);
                self.emit(cfg, sink, out)
            }
            None => true,
        }
    }

    fn report_telemetry(&mut self, cfg: &config::CliConfig, ring: &RingBuffer, uptime: Duration) {
        let t = self.collector.snapshot();
        let latency_ms =
            self.last_input_pts.saturating_sub(self.last_output_pts) as f64 / 1_000_000.0;
        let window_max_ms = self.collector.take_proc_window_max_ms();
        let search_px = self
            .detector
            .as_ref()
            .map(|d| d.max_shift_px())
            .unwrap_or(0);
        let row = PeriodicRow {
            uptime,
            latency_ms,
            buffer_fill_ratio: ring.fill_ratio(),
            dropped: self.sync.dropped_frames(),
            duplicated: self.sync.duplicated_frames(),
            jitter_ms: t.jitter_ms,
            frames_in: t.frames_in,
            frames_out: self.frames_out,
            degraded_stage: self.controller.stage(),
            degrade_events: self.controller.events(),
            drift_ppm: self.sync.drift_ppm_estimate(),
            relock_active: self.sync.relock_active(),
            signal_lost: self.guard.is_lost(),
            proc_ms: t.proc_ms,
            proc_ms_max_1s: window_max_ms,
            deadline_misses: t.deadline_misses,
            jitter_search_px: search_px,
            signal_loss_events: self.signal_loss_events,
        };
        let line = match cfg.telemetry_format {
            config::TelemetryFormat::Json => format_periodic_json(&row),
            config::TelemetryFormat::Csv => format_periodic_csv(&row),
        };
        eprintln!("{line}");
    }

    /// Record a signal-guard transition for telemetry (Issue #36):
    /// count episodes, accumulate lost time, and print an event row
    /// immediately, in the active format.
    fn note_guard_event(&mut self, cfg: &config::CliConfig, event: GuardEvent, uptime: Duration) {
        let name = match event {
            GuardEvent::SignalLost => {
                self.signal_loss_events += 1;
                self.lost_since = Some(Instant::now());
                "signal_lost"
            }
            GuardEvent::SignalRecovered => {
                if let Some(since) = self.lost_since.take() {
                    self.signal_lost_ms_total += since.elapsed().as_millis() as u64;
                }
                "signal_recovered"
            }
        };
        // Analyze-only mode keeps stdout as its only output and
        // prints no telemetry rows; the bookkeeping above still
        // feeds the final report (Issue #37).
        if cfg.analyze_only {
            return;
        }
        let t = self.collector.snapshot();
        let line = match cfg.telemetry_format {
            config::TelemetryFormat::Json => format_event_json(
                name,
                uptime,
                t.frames_in,
                self.frames_out,
                self.signal_loss_events,
            ),
            config::TelemetryFormat::Csv => format_event_csv(
                name,
                uptime,
                t.frames_in,
                self.frames_out,
                self.signal_loss_events,
            ),
        };
        eprintln!("{line}");
    }

    /// Print the end-of-run summary row (Issue #36). Called exactly
    /// once, on every normal exit path.
    fn report_summary(&self, cfg: &config::CliConfig, runtime: Duration) {
        let t = self.collector.snapshot();
        let mut lost_ms = self.signal_lost_ms_total;
        if let Some(since) = self.lost_since {
            lost_ms += since.elapsed().as_millis() as u64;
        }
        let row = SummaryRow {
            runtime,
            frames_in: t.frames_in,
            frames_out: self.frames_out,
            dropped: self.sync.dropped_frames(),
            duplicated: self.sync.duplicated_frames(),
            signal_loss_events: self.signal_loss_events,
            signal_lost_ms: lost_ms,
            proc_ms_avg: self.collector.proc_avg_ms(),
            proc_ms_max: self.collector.proc_max_ms(),
            proc_samples: self.collector.proc_samples(),
            signal_lost: self.guard.is_lost(),
        };
        let line = match cfg.telemetry_format {
            config::TelemetryFormat::Json => format_summary_json(&row),
            config::TelemetryFormat::Csv => format_summary_csv(&row),
        };
        eprintln!("{line}");
    }

    /// Whether a signal is present at the end of the run: at least
    /// one frame arrived and the guard is not in a loss state.
    fn signal_present(&self) -> bool {
        self.collector.snapshot().frames_in > 0 && !self.guard.is_lost()
    }

    /// Build the analyze-only report (Issue #37). Printed to stdout
    /// by the caller; every other run output stays on stderr.
    fn report_analysis(&self, cfg: &config::CliConfig, runtime: Duration) -> String {
        let t = self.collector.snapshot();
        let mut lost_ms = self.signal_lost_ms_total;
        if let Some(since) = self.lost_since {
            lost_ms += since.elapsed().as_millis() as u64;
        }
        let a = self.analyze.as_ref();
        let (luma_min, luma_max, range_class) = match a.and_then(|a| a.range.observed_range()) {
            Some((mn, mx)) => (
                mn.to_string(),
                mx.to_string(),
                match a.map(|a| a.range.class()) {
                    Some(RangeClass::Full) => "full",
                    _ => "limited",
                },
            ),
            None => ("null".to_string(), "null".to_string(), "unknown"),
        };
        let opt_px = |v: Option<f64>| match v {
            Some(x) => format!("{x:.3}"),
            None => "null".to_string(),
        };
        format!(
            "{{\"type\":\"analysis\",\"width\":{},\"height\":{},\"format\":\"{}\",\"runtime_s\":{:.3},\"frames_in\":{},\"frames_out\":{},\"dropped_frames\":{},\"duplicated_frames\":{},\"signal_present\":{},\"signal_loss_events\":{},\"signal_lost_ms\":{},\"drift_ppm\":{},\"drift_valid\":{},\"jitter_samples\":{},\"jitter_frames\":{},\"jitter_unestimated_frames\":{},\"jitter_bypassed_frames\":{},\"jitter_mean_abs_px\":{},\"jitter_mean_signed_px\":{},\"jitter_max_abs_px\":{},\"luma_min\":{},\"luma_max\":{},\"range_class\":\"{}\"}}",
            cfg.core.width,
            cfg.core.height,
            wire_format_name(cfg.core.format),
            runtime.as_secs_f64(),
            t.frames_in,
            self.frames_out,
            self.sync.dropped_frames(),
            self.sync.duplicated_frames(),
            self.signal_present(),
            self.signal_loss_events,
            lost_ms,
            self.sync.drift_ppm_estimate(),
            self.signal_present(),
            a.map_or(0, |a| a.jitter_samples),
            a.map_or(0, |a| a.jitter_frames),
            a.map_or(0, |a| a.jitter_unestimated_frames),
            a.map_or(0, |a| a.bypassed_frames),
            opt_px(a.and_then(|a| a.jitter_mean_abs_px())),
            opt_px(a.and_then(|a| a.jitter_mean_signed_px())),
            opt_px(a.and_then(|a| a.jitter_max_abs_px())),
            luma_min,
            luma_max,
            range_class,
        )
    }
}

/// The `--format` spelling of a wire format, for the analysis
/// report (Issue #37).
fn wire_format_name(format: PixelFormat) -> &'static str {
    match format {
        PixelFormat::Yuv420p => "yuv420p",
        PixelFormat::Yuv422p => "yuv422p",
        PixelFormat::Rgb24 => "rgb24",
        PixelFormat::Uyvy => "uyvy",
        PixelFormat::Yuyv => "yuyv",
        PixelFormat::Yuv420p10 => "yuv420p10le",
        PixelFormat::Yuv422p10 => "yuv422p10le",
        PixelFormat::Nv12 => "nv12",
    }
}

/// One periodic telemetry sample (Issue #36).
struct PeriodicRow {
    uptime: Duration,
    latency_ms: f64,
    buffer_fill_ratio: f64,
    dropped: u64,
    duplicated: u64,
    jitter_ms: f64,
    frames_in: u64,
    frames_out: u64,
    degraded_stage: u8,
    degrade_events: u64,
    drift_ppm: i64,
    relock_active: bool,
    signal_lost: bool,
    proc_ms: f64,
    proc_ms_max_1s: f64,
    deadline_misses: u64,
    jitter_search_px: u32,
    signal_loss_events: u64,
}

/// The end-of-run telemetry summary (Issue #36).
struct SummaryRow {
    runtime: Duration,
    frames_in: u64,
    frames_out: u64,
    dropped: u64,
    duplicated: u64,
    signal_loss_events: u64,
    signal_lost_ms: u64,
    proc_ms_avg: f64,
    proc_ms_max: f64,
    proc_samples: u64,
    signal_lost: bool,
}

/// CSV column schema shared by every telemetry row kind, so a CSV
/// stream parses as one table (Issue #36).
const CSV_COLUMNS: [&str; 24] = [
    "type",
    "uptime_s",
    "event",
    "latency_ms",
    "buffer_fill_ratio",
    "dropped_frames",
    "duplicated_frames",
    "jitter_ms",
    "frames_in",
    "frames_out",
    "degraded_stage",
    "degrade_events",
    "drift_ppm",
    "relock_active",
    "signal_lost",
    "proc_ms",
    "proc_ms_max_1s",
    "deadline_misses",
    "jitter_search_px",
    "signal_loss_events",
    "runtime_s",
    "proc_ms_avg",
    "proc_ms_max",
    "signal_lost_ms",
];

fn csv_header() -> String {
    CSV_COLUMNS.join(",")
}

fn csv_row(fill: &[(usize, String)]) -> String {
    let mut cells = vec![String::new(); CSV_COLUMNS.len()];
    for (idx, value) in fill {
        cells[*idx] = value.clone();
    }
    cells.join(",")
}

fn format_periodic_json(r: &PeriodicRow) -> String {
    format!(
        "{{\"type\":\"periodic\",\"uptime_s\":{:.3},\"latency_ms\":{:.3},\"buffer_fill_ratio\":{:.3},\"dropped_frames\":{},\"duplicated_frames\":{},\"jitter_ms\":{:.3},\"frames_in\":{},\"frames_out\":{},\"degraded_stage\":{},\"degrade_events\":{},\"drift_ppm\":{},\"relock_active\":{},\"signal_lost\":{},\"proc_ms\":{:.3},\"proc_ms_max_1s\":{:.3},\"deadline_misses\":{},\"jitter_search_px\":{},\"signal_loss_events\":{}}}",
        r.uptime.as_secs_f64(),
        r.latency_ms,
        r.buffer_fill_ratio,
        r.dropped,
        r.duplicated,
        r.jitter_ms,
        r.frames_in,
        r.frames_out,
        r.degraded_stage,
        r.degrade_events,
        r.drift_ppm,
        r.relock_active,
        r.signal_lost,
        r.proc_ms,
        r.proc_ms_max_1s,
        r.deadline_misses,
        r.jitter_search_px,
        r.signal_loss_events
    )
}

fn format_periodic_csv(r: &PeriodicRow) -> String {
    csv_row(&[
        (0, "periodic".to_string()),
        (1, format!("{:.3}", r.uptime.as_secs_f64())),
        (3, format!("{:.3}", r.latency_ms)),
        (4, format!("{:.3}", r.buffer_fill_ratio)),
        (5, r.dropped.to_string()),
        (6, r.duplicated.to_string()),
        (7, format!("{:.3}", r.jitter_ms)),
        (8, r.frames_in.to_string()),
        (9, r.frames_out.to_string()),
        (10, r.degraded_stage.to_string()),
        (11, r.degrade_events.to_string()),
        (12, r.drift_ppm.to_string()),
        (13, r.relock_active.to_string()),
        (14, r.signal_lost.to_string()),
        (15, format!("{:.3}", r.proc_ms)),
        (16, format!("{:.3}", r.proc_ms_max_1s)),
        (17, r.deadline_misses.to_string()),
        (18, r.jitter_search_px.to_string()),
        (19, r.signal_loss_events.to_string()),
    ])
}

fn format_event_json(
    name: &str,
    uptime: Duration,
    frames_in: u64,
    frames_out: u64,
    signal_loss_events: u64,
) -> String {
    format!(
        "{{\"type\":\"event\",\"event\":\"{name}\",\"uptime_s\":{:.3},\"frames_in\":{},\"frames_out\":{},\"signal_loss_events\":{}}}",
        uptime.as_secs_f64(),
        frames_in,
        frames_out,
        signal_loss_events
    )
}

fn format_event_csv(
    name: &str,
    uptime: Duration,
    frames_in: u64,
    frames_out: u64,
    signal_loss_events: u64,
) -> String {
    csv_row(&[
        (0, "event".to_string()),
        (1, format!("{:.3}", uptime.as_secs_f64())),
        (2, name.to_string()),
        (8, frames_in.to_string()),
        (9, frames_out.to_string()),
        (19, signal_loss_events.to_string()),
    ])
}

fn format_summary_json(r: &SummaryRow) -> String {
    format!(
        "{{\"type\":\"summary\",\"runtime_s\":{:.3},\"frames_in\":{},\"frames_out\":{},\"dropped_frames\":{},\"duplicated_frames\":{},\"signal_loss_events\":{},\"signal_lost_ms\":{},\"proc_ms_avg\":{:.3},\"proc_ms_max\":{:.3},\"proc_samples\":{},\"signal_lost\":{}}}",
        r.runtime.as_secs_f64(),
        r.frames_in,
        r.frames_out,
        r.dropped,
        r.duplicated,
        r.signal_loss_events,
        r.signal_lost_ms,
        r.proc_ms_avg,
        r.proc_ms_max,
        r.proc_samples,
        r.signal_lost
    )
}

fn format_summary_csv(r: &SummaryRow) -> String {
    csv_row(&[
        (0, "summary".to_string()),
        (1, format!("{:.3}", r.runtime.as_secs_f64())),
        (5, r.dropped.to_string()),
        (6, r.duplicated.to_string()),
        (8, r.frames_in.to_string()),
        (9, r.frames_out.to_string()),
        (14, r.signal_lost.to_string()),
        (19, r.signal_loss_events.to_string()),
        (20, format!("{:.3}", r.runtime.as_secs_f64())),
        (21, format!("{:.3}", r.proc_ms_avg)),
        (22, format!("{:.3}", r.proc_ms_max)),
        (23, r.signal_lost_ms.to_string()),
    ])
}

fn run(cfg: &config::CliConfig) -> i32 {
    let mut core = cfg.core.clone();
    // Boundary formats (packed 422, 10-bit) are processed in their
    // 8-bit pipeline representation (Issues #24, #28): the reader
    // thread converts on the way in and `emit` re-packs on the way
    // out. `cfg.core` keeps the declared wire format for I/O geometry.
    core.format = pipeline_format(core.format);
    let mut source = match open_source(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("software-tbc: input error: {e}");
            return 3;
        }
    };
    // Analyze-only mode never opens the output path (Issue #37):
    // the pipeline runs against a sink that discards every frame.
    let mut sink: Box<dyn FrameSink + Send> = if cfg.analyze_only {
        Box::new(NullSink)
    } else {
        match open_sink(cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("software-tbc: output error: {e}");
                return 3;
            }
        }
    };

    let ring = Arc::new(RingBuffer::new(core.buffer_frames.max(2)));
    let input_done = Arc::new(AtomicBool::new(false));
    // Set when the consumer stops early (max_frames / write error): the
    // reader must abandon its push loop, or joining it would deadlock.
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_error: Arc<Mutex<Option<IoError>>> = Arc::new(Mutex::new(None));

    // Wall-clock stamps need arrival times; sequence files have none,
    // so that combination falls back to nominal with a warning.
    let stamp_mode = if cfg.stamp == config::StampMode::WallClock
        && split_sequence(&cfg.input).is_some()
    {
        eprintln!(
            "software-tbc: --stamp wall-clock has no arrival times for sequence input; using nominal"
        );
        config::StampMode::Nominal
    } else {
        cfg.stamp
    };

    // In auto field-order mode the reader thread runs the detector
    // and publishes its decision here; the value starts at the
    // configured fallback order (Issue #26).
    let effective_order = Arc::new(Mutex::new(cfg.core.field_order));

    // Reader thread: stamp each frame (raw streams carry no timestamps
    // of their own) and push it into the ring.
    let reader_thread = {
        let ring = Arc::clone(&ring);
        let input_done = Arc::clone(&input_done);
        let stop_reader = Arc::clone(&stop_reader);
        let reader_error = Arc::clone(&reader_error);
        let effective_order = Arc::clone(&effective_order);
        let auto_detect = cfg.field_order_auto;
        let rate = core.target_rate;
        // Crop window (Issue #31): (left, top, processing width,
        // processing height) when the input geometry differs from
        // the processing geometry.
        let crop = (cfg.input_width != core.width
            || cfg.input_height != core.height
            || cfg.crop_left != 0
            || cfg.crop_top != 0)
            .then_some((cfg.crop_left, cfg.crop_top, core.width, core.height));
        std::thread::spawn(move || {
            let mut detector = if auto_detect {
                Some(FieldOrderDetector::new())
            } else {
                None
            };
            let mut stamper = Stamper::new(stamp_mode, rate);
            let mut sequence = 0u64;
            'read: loop {
                if stop_reader.load(Ordering::Acquire) {
                    break 'read;
                }
                match source.next_frame() {
                    Ok(Some(mut frame)) => {
                        // Convert packed boundary formats into the
                        // pipeline's yuv422p representation (Issue #24).
                        if frame.format.is_packed_422() {
                            match frame.to_yuv422p() {
                                Ok(converted) => frame = converted,
                                Err(e) => {
                                    *reader_error.lock().expect("error slot") =
                                        Some(IoError::Core(e));
                                    break 'read;
                                }
                            }
                        }
                        // Convert NV12 into the pipeline's planar
                        // 4:2:0 representation (Issue #33).
                        if frame.format == PixelFormat::Nv12 {
                            match frame.to_yuv420p() {
                                Ok(converted) => frame = converted,
                                Err(e) => {
                                    *reader_error.lock().expect("error slot") =
                                        Some(IoError::Core(e));
                                    break 'read;
                                }
                            }
                        }
                        // Convert 10-bit boundary formats into the
                        // pipeline's 8-bit representation (Issue #28).
                        if frame.format.is_10bit() {
                            match frame.to_8bit() {
                                Ok(converted) => frame = converted,
                                Err(e) => {
                                    *reader_error.lock().expect("error slot") =
                                        Some(IoError::Core(e));
                                    break 'read;
                                }
                            }
                        }
                        // Crop to the processing geometry
                        // (Issue #31), before field-order detection
                        // and stamping.
                        if let Some((left, top, w, h)) = crop {
                            match tbc_core::geometry::crop_frame(&frame, left, top, w, h) {
                                Ok(cropped) => frame = cropped,
                                Err(e) => {
                                    *reader_error.lock().expect("error slot") =
                                        Some(IoError::Core(e));
                                    break 'read;
                                }
                            }
                        }
                        if let Some(det) = detector.as_mut() {
                            if let Some(order) = det.observe(&frame) {
                                *effective_order.lock().expect("order slot") = order;
                            }
                        }
                        frame.pts_ns = stamper.stamp(Instant::now());
                        frame.sequence = sequence;
                        sequence += 1;
                        loop {
                            if stop_reader.load(Ordering::Acquire) {
                                break 'read;
                            }
                            match ring.push(frame) {
                                Ok(()) => break,
                                Err(tbc_core::sync::ring::Full(back)) => {
                                    frame = back;
                                    std::thread::yield_now();
                                }
                            }
                        }
                    }
                    Ok(None) => break 'read,
                    Err(e) => {
                        *reader_error.lock().expect("error slot") = Some(e);
                        break 'read;
                    }
                }
            }
            input_done.store(true, Ordering::Release);
        })
    };

    let mut pipe = Pipeline {
        sync: FrameSynchronizer::new(core.clone()),
        procamp: ProcAmp::new(core.procamp),
        detector: if core.jitter_correction {
            Some(JitterDetector::new(cfg.max_shift_px))
        } else {
            None
        },
        guard: SignalGuard::new(core.signal_loss, cfg.signal_loss_timeout_ms),
        collector: TelemetryCollector::new(),
        controller: OverloadController::new(
            cfg.overload_fallback && stamp_mode == config::StampMode::WallClock,
            core.jitter_correction,
            core.rate_policy == RatePolicy::Blend,
            ((core.target_rate.den as u128 * 1_000_000_000) / core.target_rate.num as u128) as u64,
        ),
        auto_order: cfg.field_order_auto.then(|| Arc::clone(&effective_order)),
        analyze: cfg.analyze_only.then(AnalyzeStats::default),
        range_estimator: cfg.auto_adapt.then(RangeEstimator::new),
        jitter_adapter: (cfg.auto_adapt && core.jitter_correction)
            .then(|| JitterAdapter::new(cfg.max_shift_px)),
        applied_hold_drop: false,
        frames_out: 0,
        last_emitted_seq: None,
        last_output_pts: 0,
        last_input_pts: 0,
        prev_dropped: 0,
        prev_duplicated: 0,
        output_error: false,
        signal_loss_events: 0,
        lost_since: None,
        signal_lost_ms_total: 0,
    };
    // Processing deadline for telemetry: the target frame interval
    // (Issue #36).
    pipe.collector.set_deadline_ns(
        ((core.target_rate.den as u128 * 1_000_000_000) / core.target_rate.num as u128) as u64,
    );
    let run_started = Instant::now();
    if cfg.telemetry_format == config::TelemetryFormat::Csv && !cfg.analyze_only {
        eprintln!("{}", csv_header());
    }
    let mut last_ingest_seq: Option<u64> = None;
    let mut last_telemetry = Instant::now();
    let mut stopped = false;

    'main: loop {
        match ring.pop() {
            Some(frame) => {
                pipe.collector.note_input(frame.pts_ns);
                pipe.last_input_pts = frame.pts_ns;
                last_ingest_seq = Some(frame.sequence);
                let guard_event = if frame.validate().is_ok() {
                    let event = pipe.guard.note_good_frame(&frame);
                    pipe.sync.ingest(frame);
                    event
                } else {
                    pipe.guard.note_bad_frame()
                };
                if let Some(event) = guard_event {
                    pipe.note_guard_event(cfg, event, run_started.elapsed());
                }
                // Fold the synchronizer's cumulative counters into the
                // collector as deltas.
                let dropped = pipe.sync.dropped_frames();
                let duplicated = pipe.sync.duplicated_frames();
                pipe.collector.note_dropped(dropped - pipe.prev_dropped);
                pipe.collector
                    .note_duplicated(duplicated - pipe.prev_duplicated);
                pipe.prev_dropped = dropped;
                pipe.prev_duplicated = duplicated;

                let started = Instant::now();
                let keep_going = pipe.process_output(cfg, &mut sink);
                let proc_ns = started.elapsed().as_nanos() as u64;
                pipe.controller.observe(proc_ns);
                pipe.collector.note_processing_time(proc_ns);
                let want_hold_drop = pipe.controller.force_hold_drop();
                if want_hold_drop != pipe.applied_hold_drop {
                    pipe.sync.set_rate_policy(if want_hold_drop {
                        RatePolicy::HoldDrop
                    } else {
                        core.rate_policy
                    });
                    pipe.applied_hold_drop = want_hold_drop;
                }
                if !keep_going {
                    stopped = true;
                    break 'main;
                }
            }
            None => {
                if input_done.load(Ordering::Acquire) {
                    break 'main;
                }
                std::thread::yield_now();
            }
        }
        if !cfg.analyze_only
            && cfg.telemetry_interval_ms > 0
            && last_telemetry.elapsed().as_millis() >= cfg.telemetry_interval_ms as u128
        {
            pipe.report_telemetry(cfg, &ring, run_started.elapsed());
            last_telemetry = Instant::now();
        }
    }

    // Drain: after end of input, emit until the last ingested frame has
    // been emitted once (unless the run was already stopped).
    if !stopped && !pipe.output_error {
        if let Some(last_seq) = last_ingest_seq {
            if pipe.last_emitted_seq != Some(last_seq) {
                for _ in 0..100_000 {
                    if !pipe.process_output(cfg, &mut sink) {
                        break;
                    }
                    if pipe.last_emitted_seq == Some(last_seq) {
                        break;
                    }
                }
            }
        }
    }

    // Unblock the reader if it is still pushing into a ring nobody
    // drains any more, then join it.
    stop_reader.store(true, Ordering::Release);
    let _ = sink.flush();
    let _ = reader_thread.join();

    let code = if pipe.output_error {
        3
    } else if let Some(e) = reader_error.lock().expect("error slot").take() {
        eprintln!("software-tbc: input error: {e}");
        3
    } else if cfg.fail_on_signal_loss && pipe.guard.is_lost() {
        eprintln!("software-tbc: ended in signal-loss state");
        4
    } else {
        0
    };
    if cfg.analyze_only {
        // The analysis report is the run's only stdout output
        // (Issue #37). Exit 4 when the stream ended without a
        // signal, unless an I/O error already decided the code.
        println!("{}", pipe.report_analysis(cfg, run_started.elapsed()));
        if code == 0 && !pipe.signal_present() {
            return 4;
        }
        return code;
    }
    // End-of-run summary, exactly once on every normal exit path
    // (Issue #36).
    pipe.report_summary(cfg, run_started.elapsed());
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ntsc() -> FrameRate {
        FrameRate {
            num: 30_000,
            den: 1001,
        }
    }

    #[test]
    fn nominal_stamps_follow_the_rate() {
        let mut stamper = Stamper::new(config::StampMode::Nominal, ntsc());
        let now = Instant::now();
        // Sequence -> expected pts_ns at 30000/1001 (integer ns).
        let expected = [
            (0u64, 0u64),
            (1, 33_366_666),
            (2, 66_733_333),
            (30, 1_001_000_000),
        ];
        for seq in 0..=30u64 {
            let pts = stamper.stamp(now);
            if let Some((_, want)) = expected.iter().find(|(s, _)| *s == seq) {
                assert_eq!(pts, *want, "sequence {seq}");
            }
        }
    }

    #[test]
    fn wall_clock_stamps_measure_elapsed_time() {
        let mut stamper = Stamper::new(config::StampMode::WallClock, ntsc());
        let t0 = Instant::now();
        // The first frame anchors the clock at zero.
        assert_eq!(stamper.stamp(t0), 0);
        // A frame 12.3 ms later stamps at ~12.3 ms (clock granularity
        // varies by platform, so assert a window, not an exact value).
        let v = stamper.stamp(t0 + Duration::from_nanos(12_345_678));
        assert!(
            (12_000_000..=13_000_000).contains(&v),
            "wall-clock stamp was {v}"
        );
        // An out-of-order instant saturates at zero instead of wrapping.
        assert_eq!(stamper.stamp(t0), 0);
    }

    const PERIOD: u64 = 33_366_666; // 30000/1001

    fn feed(controller: &mut OverloadController, proc_ns: u64, frames: u32) {
        for _ in 0..frames {
            controller.observe(proc_ns);
        }
    }

    #[test]
    fn controller_escalates_and_recovers_in_stages() {
        let mut c = OverloadController::new(true, true, true, PERIOD);
        // Sustained 2x overload: stage 1 after the attack streak.
        feed(&mut c, 2 * PERIOD, 30);
        assert_eq!(c.stage(), 1);
        assert!(c.skip_jitter());
        assert!(!c.force_hold_drop());
        // Still overloaded past the dwell: stage 2.
        feed(&mut c, 2 * PERIOD, 200);
        assert_eq!(c.stage(), 2);
        assert!(c.force_hold_drop());
        assert_eq!(c.events(), 2);
        // Sustained headroom releases one stage at a time.
        feed(&mut c, PERIOD / 3, 500);
        assert_eq!(c.stage(), 1);
        feed(&mut c, PERIOD / 3, 500);
        assert_eq!(c.stage(), 0);
        assert!(!c.skip_jitter());
    }

    #[test]
    fn controller_ignores_a_single_spike() {
        let mut c = OverloadController::new(true, true, true, PERIOD);
        feed(&mut c, PERIOD / 5, 100);
        c.observe(10 * PERIOD); // one transient spike
        feed(&mut c, PERIOD / 5, 100);
        assert_eq!(c.stage(), 0);
        assert_eq!(c.events(), 0);
    }

    #[test]
    fn controller_does_not_flap_around_the_threshold() {
        let mut c = OverloadController::new(true, true, true, PERIOD);
        // Alternating short blocks never build a full streak.
        for _ in 0..10 {
            feed(&mut c, PERIOD * 6 / 5, 20);
            feed(&mut c, PERIOD * 2 / 5, 20);
        }
        assert_eq!(c.stage(), 0);
        assert_eq!(c.events(), 0);
    }

    #[test]
    fn controller_caps_at_the_available_levers() {
        // Blend only: stage 1 already forces hold/drop, and is the cap.
        let mut c = OverloadController::new(true, false, true, PERIOD);
        feed(&mut c, 2 * PERIOD, 400);
        assert_eq!(c.stage(), 1);
        assert!(c.force_hold_drop());
        assert!(!c.skip_jitter());
        // Jitter only: stage 1 skips jitter, and is the cap.
        let mut c = OverloadController::new(true, true, false, PERIOD);
        feed(&mut c, 2 * PERIOD, 400);
        assert_eq!(c.stage(), 1);
        assert!(c.skip_jitter());
        assert!(!c.force_hold_drop());
    }

    #[test]
    fn controller_inert_when_disabled_or_leverless() {
        let mut c = OverloadController::new(false, true, true, PERIOD);
        feed(&mut c, 2 * PERIOD, 500);
        assert_eq!(c.stage(), 0);
        let mut c = OverloadController::new(true, false, false, PERIOD);
        feed(&mut c, 2 * PERIOD, 500);
        assert_eq!(c.stage(), 0);
        assert_eq!(c.events(), 0);
    }

    fn frame422(data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 1,
            format: PixelFormat::Yuv422p,
            field_order: tbc_core::FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data,
        }
    }

    #[test]
    fn wire_frame_repaks_for_packed_formats() {
        // Pipeline order [Y0, U, Y1, V] -> uyvy [U, Y0, V, Y1].
        let out = to_wire_frame(
            PixelFormat::Uyvy,
            frame422(vec![20, 10, 40, 30, 60, 50, 80, 70]),
        )
        .expect("re-pack uyvy");
        assert_eq!(out.format, PixelFormat::Uyvy);
        assert_eq!(out.data, vec![10, 20, 30, 40, 50, 60, 70, 80]);
        // yuyv keeps the byte order and only changes the tag.
        let out = to_wire_frame(
            PixelFormat::Yuyv,
            frame422(vec![20, 10, 40, 30, 60, 50, 80, 70]),
        )
        .expect("re-pack yuyv");
        assert_eq!(out.format, PixelFormat::Yuyv);
        assert_eq!(out.data, vec![20, 10, 40, 30, 60, 50, 80, 70]);
    }

    #[test]
    fn wire_frame_interleaves_for_nv12() {
        // 4x2 yuv420p: Y = 1..=8, Cb = [101, 102], Cr = [201, 202]
        // becomes NV12 with the interleaved plane [101, 201, 102, 202].
        let frame420 = VideoFrame {
            width: 4,
            height: 2,
            format: PixelFormat::Yuv420p,
            field_order: tbc_core::FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8, 101, 102, 201, 202],
        };
        let out = to_wire_frame(PixelFormat::Nv12, frame420).expect("interleave nv12");
        assert_eq!(out.format, PixelFormat::Nv12);
        assert_eq!(out.data, vec![1, 2, 3, 4, 5, 6, 7, 8, 101, 201, 102, 202]);
        assert_eq!(pipeline_format(PixelFormat::Nv12), PixelFormat::Yuv420p);
    }

    #[test]
    fn wire_frame_passes_other_formats_through() {
        let frame = frame422(vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let out = to_wire_frame(PixelFormat::Yuv422p, frame).expect("passthrough");
        assert_eq!(out.format, PixelFormat::Yuv422p);
        assert_eq!(out.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn wire_frame_expands_for_10bit_formats() {
        // 422p10: the packed frame becomes planar u16 planes
        // (Y plane, then U, then V), samples scaled to 10-bit.
        let out = to_wire_frame(
            PixelFormat::Yuv422p10,
            frame422(vec![20, 10, 40, 30, 60, 50, 80, 70]),
        )
        .expect("expand 422p10");
        assert_eq!(out.format, PixelFormat::Yuv422p10);
        let samples: Vec<u16> = out
            .data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        // Y = [20, 40, 60, 80], U = [10, 50], V = [30, 70], scaled
        // with (v * 1023 + 127) / 255.
        let scale = |v: u8| ((v as u32 * 1023 + 127) / 255) as u16;
        assert_eq!(
            samples,
            vec![
                scale(20),
                scale(40),
                scale(60),
                scale(80),
                scale(10),
                scale(50),
                scale(30),
                scale(70)
            ]
        );
        // The pipeline format mapping pairs each wire format with
        // its 8-bit representation.
        assert_eq!(
            pipeline_format(PixelFormat::Yuv422p10),
            PixelFormat::Yuv422p
        );
        assert_eq!(
            pipeline_format(PixelFormat::Yuv420p10),
            PixelFormat::Yuv420p
        );
        assert_eq!(pipeline_format(PixelFormat::Uyvy), PixelFormat::Yuv422p);
        assert_eq!(pipeline_format(PixelFormat::Rgb24), PixelFormat::Rgb24);
    }

    /// In-memory sink for pipeline tests (Issue #34).
    struct VecSink {
        frames: Arc<Mutex<Vec<VideoFrame>>>,
    }

    impl FrameSink for VecSink {
        fn write_frame(&mut self, frame: &VideoFrame) -> Result<(), IoError> {
            self.frames.lock().expect("frames").push(frame.clone());
            Ok(())
        }
        fn flush(&mut self) -> Result<(), IoError> {
            Ok(())
        }
    }

    /// 4x2 config whose correction stages are all visibly non-neutral
    /// (proc-amp luma gain 2.0, one head-switch line), so a processed
    /// frame can never equal the input bytes.
    fn passthrough_cfg(policy: SignalLossPolicy) -> config::CliConfig {
        let mut cfg = config::CliConfig::default();
        cfg.core.width = 4;
        cfg.core.height = 2;
        cfg.core.signal_loss = policy;
        cfg.core.procamp.luma_gain = 2.0;
        cfg.core.head_switch.lines = 1;
        cfg.input_width = 4;
        cfg.input_height = 2;
        cfg.output_width = 4;
        cfg.output_height = 2;
        cfg
    }

    fn passthrough_pipeline(cfg: &config::CliConfig) -> Pipeline {
        let core = cfg.core.clone();
        Pipeline {
            sync: FrameSynchronizer::new(core.clone()),
            procamp: ProcAmp::new(core.procamp),
            detector: None,
            guard: SignalGuard::new(core.signal_loss, cfg.signal_loss_timeout_ms),
            collector: TelemetryCollector::new(),
            controller: OverloadController::new(false, false, false, 33_366_666),
            auto_order: None,
            range_estimator: None,
            jitter_adapter: None,
            applied_hold_drop: false,
            frames_out: 0,
            last_emitted_seq: None,
            last_output_pts: 0,
            last_input_pts: 0,
            prev_dropped: 0,
            prev_duplicated: 0,
            output_error: false,
            signal_loss_events: 0,
            lost_since: None,
            signal_lost_ms_total: 0,
            analyze: None,
        }
    }

    fn frame_4x2() -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 2,
            format: PixelFormat::Yuv422p,
            field_order: FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data: vec![
                100, 128, 100, 128, 100, 128, 100, 128, // line 0
                90, 128, 90, 128, 90, 128, 90, 128, // line 1
            ],
        }
    }

    fn run_one(cfg: &config::CliConfig, pipe: &mut Pipeline) -> Vec<VideoFrame> {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let mut sink: Box<dyn FrameSink + Send> = Box::new(VecSink {
            frames: Arc::clone(&frames),
        });
        assert!(pipe.process_output(cfg, &mut sink));
        let out = frames.lock().expect("frames").clone();
        out
    }

    #[test]
    fn passthrough_emits_the_frame_unprocessed_while_lost() {
        let cfg = passthrough_cfg(SignalLossPolicy::Passthrough);
        let mut pipe = passthrough_pipeline(&cfg);
        for _ in 0..3 {
            pipe.guard.note_bad_frame();
        }
        assert!(pipe.guard.is_lost());
        pipe.sync.ingest(frame_4x2());
        let out = run_one(&cfg, &mut pipe);
        assert_eq!(out.len(), 1);
        // No head-switch masking, no proc-amp: bytes are untouched.
        assert_eq!(out[0].data, frame_4x2().data);
    }

    #[test]
    fn passthrough_processes_normally_when_not_lost() {
        let cfg = passthrough_cfg(SignalLossPolicy::Passthrough);
        let mut pipe = passthrough_pipeline(&cfg);
        pipe.sync.ingest(frame_4x2());
        let out = run_one(&cfg, &mut pipe);
        assert_eq!(out.len(), 1);
        assert_ne!(out[0].data, frame_4x2().data);
    }

    fn sample_periodic() -> PeriodicRow {
        PeriodicRow {
            uptime: Duration::from_millis(1500),
            latency_ms: 33.367,
            buffer_fill_ratio: 0.5,
            dropped: 1,
            duplicated: 2,
            jitter_ms: 0.123,
            frames_in: 100,
            frames_out: 99,
            degraded_stage: 0,
            degrade_events: 0,
            drift_ppm: 123,
            relock_active: true,
            signal_lost: false,
            proc_ms: 1.234,
            proc_ms_max_1s: 2.345,
            deadline_misses: 0,
            jitter_search_px: 8,
            signal_loss_events: 0,
        }
    }

    fn sample_summary() -> SummaryRow {
        SummaryRow {
            runtime: Duration::from_millis(2500),
            frames_in: 100,
            frames_out: 99,
            dropped: 1,
            duplicated: 2,
            signal_loss_events: 1,
            signal_lost_ms: 1200,
            proc_ms_avg: 1.5,
            proc_ms_max: 9.5,
            proc_samples: 42,
            signal_lost: false,
        }
    }

    #[test]
    fn telemetry_json_rows_have_stable_shape() {
        let p = format_periodic_json(&sample_periodic());
        assert!(p.starts_with("{\"type\":\"periodic\""), "{p}");
        assert!(p.contains("\"drift_ppm\":123"), "{p}");
        assert!(p.contains("\"relock_active\":true"), "{p}");
        assert!(p.contains("\"proc_ms_max_1s\":2.345"), "{p}");
        let e = format_event_json("signal_lost", Duration::from_millis(2500), 10, 9, 1);
        assert!(e.starts_with("{\"type\":\"event\""), "{e}");
        assert!(e.contains("\"event\":\"signal_lost\""), "{e}");
        assert!(e.contains("\"signal_loss_events\":1"), "{e}");
        let s = format_summary_json(&sample_summary());
        assert!(s.starts_with("{\"type\":\"summary\""), "{s}");
        assert!(s.contains("\"proc_samples\":42"), "{s}");
        assert!(s.contains("\"signal_lost_ms\":1200"), "{s}");
    }

    #[test]
    fn telemetry_csv_rows_share_the_header_column_count() {
        let n = csv_header().split(',').count();
        assert_eq!(n, CSV_COLUMNS.len());
        let rows = [
            format_periodic_csv(&sample_periodic()),
            format_event_csv("signal_lost", Duration::from_millis(2500), 10, 9, 1),
            format_summary_csv(&sample_summary()),
        ];
        for row in &rows {
            assert_eq!(row.split(',').count(), n, "row: {row}");
        }
        assert!(rows[0].starts_with("periodic,"));
        assert!(rows[1].starts_with("event,"));
        assert!(rows[2].starts_with("summary,"));
    }

    #[test]
    fn analyze_stats_accumulate_only_estimated_samples() {
        let mut a = AnalyzeStats::default();
        assert!(a.jitter_mean_abs_px().is_none());
        assert!(a.jitter_max_abs_px().is_none());
        assert_eq!(a.range.observed_range(), None);
        a.observe(&frame_4x2());
        assert!(a.range.observed_range().is_some());
        a.note_shifts(&[
            LineShift {
                line: 0,
                shift_q8: 256,
            },
            LineShift {
                line: 1,
                shift_q8: -128,
            },
        ]);
        assert_eq!(a.jitter_samples, 2);
        assert_eq!(a.jitter_frames, 1);
        assert_eq!(a.jitter_mean_abs_px(), Some(0.75));
        assert_eq!(a.jitter_mean_signed_px(), Some(0.25));
        assert_eq!(a.jitter_max_abs_px(), Some(1.0));
    }

    #[test]
    fn analysis_report_shape_and_null_statistics() {
        let cfg = config::CliConfig::default();
        let mut pipe = passthrough_pipeline(&cfg);
        pipe.analyze = Some(AnalyzeStats::default());
        let report = pipe.report_analysis(&cfg, Duration::from_secs(1));
        assert!(report.starts_with("{\"type\":\"analysis\""), "{report}");
        assert!(report.contains("\"signal_present\":false"), "{report}");
        assert!(report.contains("\"jitter_mean_abs_px\":null"), "{report}");
        assert!(report.contains("\"luma_min\":null"), "{report}");
        assert!(report.contains("\"range_class\":\"unknown\""), "{report}");
        assert!(report.contains("\"format\":\"yuv422p\""), "{report}");
    }

    #[test]
    fn blackout_still_substitutes_a_fallback_while_lost() {
        let cfg = passthrough_cfg(SignalLossPolicy::Blackout);
        let mut pipe = passthrough_pipeline(&cfg);
        for _ in 0..3 {
            pipe.guard.note_bad_frame();
        }
        assert!(pipe.guard.is_lost());
        pipe.sync.ingest(frame_4x2());
        let out = run_one(&cfg, &mut pipe);
        assert_eq!(out.len(), 1);
        // Broadcast black in packed 422: [16, 128, 16, 128] quads.
        assert_eq!(out[0].data, [16, 128, 16, 128].repeat(4));
    }
}
