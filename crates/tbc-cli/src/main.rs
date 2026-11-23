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
use tbc_core::guard::SignalGuard;
use tbc_core::jitter::{apply_shifts, JitterDetector};
use tbc_core::procamp::ProcAmp;
use tbc_core::sync::ring::RingBuffer;
use tbc_core::sync::FrameSynchronizer;
use tbc_core::telemetry::TelemetryCollector;
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
    /// Pixel format: yuv420p | yuv422p | rgb24.
    #[arg(long)]
    format: Option<String>,
    /// Named preset: vhs | svhs | unstable-composite | low-latency.
    #[arg(long)]
    preset: Option<String>,
    /// TOML configuration file (CLI flags override it).
    #[arg(long)]
    config: Option<String>,
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

/// Processing-side state: everything the output path touches.
struct Pipeline {
    sync: FrameSynchronizer,
    procamp: ProcAmp,
    detector: Option<JitterDetector>,
    guard: SignalGuard,
    collector: TelemetryCollector,
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
        out: tbc_core::VideoFrame,
    ) -> bool {
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
            if let Some(mut fb) =
                self.guard
                    .fallback_frame(cfg.core.width, cfg.core.height, cfg.core.format)
            {
                fb.sequence = self.frames_out;
                fb.pts_ns = self.last_output_pts;
                return self.emit(cfg, sink, fb);
            }
            return true;
        }
        match self.sync.next_output() {
            Some(mut out) => {
                if let Some(det) = self.detector.as_mut() {
                    if let Some(shifts) = det.estimate(&out) {
                        let _ = apply_shifts(&mut out, &shifts);
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
            "{{\"latency_ms\":{:.3},\"buffer_fill_ratio\":{:.3},\"dropped_frames\":{},\"duplicated_frames\":{},\"jitter_ms\":{:.3},\"frames_in\":{},\"frames_out\":{}}}",
            latency_ms,
            ring.fill_ratio(),
            self.sync.dropped_frames(),
            self.sync.duplicated_frames(),
            t.jitter_ms,
            t.frames_in,
            self.frames_out
        );
    }
}

fn run(cfg: &config::CliConfig) -> i32 {
    let core = cfg.core.clone();
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

    // Reader thread: stamp each frame with the nominal input timestamp
    // (raw streams carry none) and push it into the ring.
    let reader_thread = {
        let ring = Arc::clone(&ring);
        let input_done = Arc::clone(&input_done);
        let stop_reader = Arc::clone(&stop_reader);
        let reader_error = Arc::clone(&reader_error);
        let rate = core.target_rate;
        std::thread::spawn(move || {
            let mut sequence = 0u64;
            'read: loop {
                if stop_reader.load(Ordering::Acquire) {
                    break 'read;
                }
                match source.next_frame() {
                    Ok(Some(mut frame)) => {
                        frame.pts_ns = ((sequence as u128 * rate.den as u128 * 1_000_000_000)
                            / rate.num as u128) as u64;
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

                if !pipe.process_output(cfg, &mut sink) {
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
