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
use std::time::Instant;
use tbc_core::field::FieldOrderDetector;
use tbc_core::guard::SignalGuard;
use tbc_core::head_switch::apply_head_switch;
use tbc_core::jitter::{apply_shifts, JitterDetector};
use tbc_core::procamp::ProcAmp;
use tbc_core::sync::ring::RingBuffer;
use tbc_core::sync::FrameSynchronizer;
use tbc_core::telemetry::TelemetryCollector;
use tbc_core::{FieldOrder, FrameRate, PixelFormat, RatePolicy, TbcError, VideoFrame};
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
    /// Disable the overload fallback (staged degradation that only
    /// engages in wall-clock stamp mode).
    #[arg(long)]
    no_overload_fallback: bool,
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
        width: cfg.core.width,
        height: cfg.core.height,
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
    applied_hold_drop: bool,
    frames_out: u64,
    last_emitted_seq: Option<u64>,
    last_output_pts: u64,
    last_input_pts: u64,
    prev_dropped: u64,
    prev_duplicated: u64,
    output_error: bool,
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
        if self.guard.is_lost() {
            // Substitute the policy fallback until the signal recovers.
            // Fallbacks are generated in the pipeline's format; `emit`
            // re-packs them into the wire format (Issue #24).
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
                // Conceal head-switching noise before jitter
                // estimation sees the frame (Issue #29).
                apply_head_switch(&mut out, &cfg.core.head_switch);
                if !self.controller.skip_jitter() {
                    if let Some(det) = self.detector.as_mut() {
                        if let Some(shifts) = det.estimate(&out) {
                            let _ = apply_shifts(&mut out, &shifts);
                        }
                    }
                }
                self.procamp.apply(&mut out);
                self.emit(cfg, sink, out)
            }
            None => true,
        }
    }

    fn report_telemetry(&self, ring: &RingBuffer) {
        let t = self.collector.snapshot();
        let latency_ms =
            self.last_input_pts.saturating_sub(self.last_output_pts) as f64 / 1_000_000.0;
        eprintln!(
            "{{\"latency_ms\":{:.3},\"buffer_fill_ratio\":{:.3},\"dropped_frames\":{},\"duplicated_frames\":{},\"jitter_ms\":{:.3},\"frames_in\":{},\"frames_out\":{},\"degraded_stage\":{},\"degrade_events\":{}}}",
            latency_ms,
            ring.fill_ratio(),
            self.sync.dropped_frames(),
            self.sync.duplicated_frames(),
            t.jitter_ms,
            t.frames_in,
            self.frames_out,
            self.controller.stage(),
            self.controller.events()
        );
    }
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
    let mut sink = match open_sink(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("software-tbc: output error: {e}");
            return 3;
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
        applied_hold_drop: false,
        frames_out: 0,
        last_emitted_seq: None,
        last_output_pts: 0,
        last_input_pts: 0,
        prev_dropped: 0,
        prev_duplicated: 0,
        output_error: false,
    };
    let mut last_ingest_seq: Option<u64> = None;
    let mut last_telemetry = Instant::now();
    let mut stopped = false;

    'main: loop {
        match ring.pop() {
            Some(frame) => {
                pipe.collector.note_input(frame.pts_ns);
                pipe.last_input_pts = frame.pts_ns;
                last_ingest_seq = Some(frame.sequence);
                if frame.validate().is_ok() {
                    pipe.guard.note_good_frame(&frame);
                    pipe.sync.ingest(frame);
                } else {
                    pipe.guard.note_bad_frame();
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
                pipe.controller.observe(started.elapsed().as_nanos() as u64);
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
        if cfg.telemetry_interval_ms > 0
            && last_telemetry.elapsed().as_millis() >= cfg.telemetry_interval_ms as u128
        {
            pipe.report_telemetry(&ring);
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

    if pipe.output_error {
        return 3;
    }
    if let Some(e) = reader_error.lock().expect("error slot").take() {
        eprintln!("software-tbc: input error: {e}");
        return 3;
    }
    if cfg.fail_on_signal_loss && pipe.guard.is_lost() {
        eprintln!("software-tbc: ended in signal-loss state");
        return 4;
    }
    0
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
}
