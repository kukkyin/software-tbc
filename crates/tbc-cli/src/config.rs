//! Configuration resolution: presets, TOML file and CLI flags.
//!
//! Precedence (SPEC.md §6/§7): CLI flags > TOML file > preset > defaults.
//! Each layer is a set of optional fields; later layers overwrite only
//! the fields they actually set.

use serde::Deserialize;
use tbc_core::head_switch::HeadSwitchMode;
use tbc_core::{FieldOrder, FrameRate, PixelFormat, RatePolicy, SignalLossPolicy, TbcConfig};

use crate::Args;

/// How the reader thread synthesizes input timestamps (Issue #20).
/// Raw streams carry no timestamps of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampMode {
    /// pts = sequence x nominal frame duration. Deterministic; the
    /// right choice for file input and tests.
    Nominal,
    /// pts = arrival time relative to the first frame. Lets the
    /// synchronizer see a live source's real clock drift and arrival
    /// jitter; meaningless for sequence input (files have no arrival
    /// time), where resolution falls back to `Nominal` with a warning.
    WallClock,
}

/// Fully resolved configuration for one run.
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub core: TbcConfig,
    /// Geometry separation (Issue #31): `core.width/height` is the
    /// processing (active-area) size. The input size defaults to
    /// the processing size; a 0 here means "follow the processing
    /// size" and is resolved before use.
    pub input_width: u32,
    pub input_height: u32,
    /// Origin of the processing window inside the input frame.
    pub crop_left: u32,
    pub crop_top: u32,
    /// Output size; 0 means "follow the processing size".
    pub output_width: u32,
    pub output_height: u32,
    /// Position of the processed frame inside the output frame.
    pub pad_left: u32,
    pub pad_top: u32,
    /// Detect the field order at runtime (Issue #26). When true,
    /// `core.field_order` is the fallback used until (and unless)
    /// the detector locks onto the stream's real order.
    pub field_order_auto: bool,
    pub signal_loss_timeout_ms: u64,
    pub telemetry_interval_ms: u64,
    pub max_shift_px: u32,
    pub stamp: StampMode,
    /// Telemetry output format (Issue #36).
    pub telemetry_format: TelemetryFormat,
    pub overload_fallback: bool,
    /// Auto-adapt mode (Issue #32): range estimation and jitter
    /// search-range adaptation follow the source at runtime.
    pub auto_adapt: bool,
    pub input: String,
    pub output: String,
    pub max_frames: Option<u64>,
    pub fail_on_signal_loss: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        CliConfig {
            core: TbcConfig::default(),
            input_width: 0,
            input_height: 0,
            crop_left: 0,
            crop_top: 0,
            output_width: 0,
            output_height: 0,
            pad_left: 0,
            pad_top: 0,
            field_order_auto: false,
            signal_loss_timeout_ms: 200,
            telemetry_interval_ms: 1000,
            max_shift_px: 8,
            stamp: StampMode::Nominal,
            telemetry_format: TelemetryFormat::Json,
            overload_fallback: true,
            auto_adapt: false,
            input: "-".to_string(),
            output: "-".to_string(),
            max_frames: None,
            fail_on_signal_loss: false,
        }
    }
}

/// One layer of optional overrides.
#[derive(Debug, Default, Clone)]
struct Overrides {
    width: Option<u32>,
    height: Option<u32>,
    input_width: Option<u32>,
    input_height: Option<u32>,
    crop_left: Option<u32>,
    crop_top: Option<u32>,
    output_width: Option<u32>,
    output_height: Option<u32>,
    pad_left: Option<u32>,
    pad_top: Option<u32>,
    format: Option<PixelFormat>,
    field_order: Option<FieldOrder>,
    field_order_auto: Option<bool>,
    target_rate: Option<FrameRate>,
    rate_policy: Option<RatePolicy>,
    buffer_frames: Option<usize>,
    jitter_correction: Option<bool>,
    head_switch_lines: Option<u32>,
    head_switch_mode: Option<HeadSwitchMode>,
    signal_loss: Option<SignalLossPolicy>,
    signal_loss_timeout_ms: Option<u64>,
    telemetry_interval_ms: Option<u64>,
    max_shift_px: Option<u32>,
    stamp: Option<StampMode>,
    telemetry_format: Option<TelemetryFormat>,
    overload_fallback: Option<bool>,
    auto_adapt: Option<bool>,
    procamp: ProcampOverrides,
}

#[derive(Debug, Default, Clone)]
struct ProcampOverrides {
    luma_gain: Option<f32>,
    pedestal: Option<f32>,
    saturation: Option<f32>,
    hue_degrees: Option<f32>,
    gamma: Option<f32>,
    chroma_shift_px: Option<f32>,
}

impl Overrides {
    fn apply_to(&self, cfg: &mut CliConfig) {
        let core = &mut cfg.core;
        if let Some(v) = self.width {
            core.width = v;
        }
        if let Some(v) = self.height {
            core.height = v;
        }
        if let Some(v) = self.input_width {
            cfg.input_width = v;
        }
        if let Some(v) = self.input_height {
            cfg.input_height = v;
        }
        if let Some(v) = self.crop_left {
            cfg.crop_left = v;
        }
        if let Some(v) = self.crop_top {
            cfg.crop_top = v;
        }
        if let Some(v) = self.output_width {
            cfg.output_width = v;
        }
        if let Some(v) = self.output_height {
            cfg.output_height = v;
        }
        if let Some(v) = self.pad_left {
            cfg.pad_left = v;
        }
        if let Some(v) = self.pad_top {
            cfg.pad_top = v;
        }
        if let Some(v) = self.format {
            core.format = v;
        }
        if let Some(v) = self.field_order {
            core.field_order = v;
        }
        if let Some(v) = self.field_order_auto {
            cfg.field_order_auto = v;
        }
        if let Some(v) = self.target_rate {
            core.target_rate = v;
        }
        if let Some(v) = self.rate_policy {
            core.rate_policy = v;
        }
        if let Some(v) = self.buffer_frames {
            core.buffer_frames = v;
        }
        if let Some(v) = self.jitter_correction {
            core.jitter_correction = v;
        }
        if let Some(v) = self.head_switch_lines {
            core.head_switch.lines = v;
        }
        if let Some(v) = self.head_switch_mode {
            core.head_switch.mode = v;
        }
        if let Some(v) = self.signal_loss {
            core.signal_loss = v;
        }
        if let Some(v) = self.signal_loss_timeout_ms {
            cfg.signal_loss_timeout_ms = v;
        }
        if let Some(v) = self.telemetry_interval_ms {
            cfg.telemetry_interval_ms = v;
        }
        if let Some(v) = self.max_shift_px {
            cfg.max_shift_px = v;
        }
        if let Some(v) = self.stamp {
            cfg.stamp = v;
        }
        if let Some(v) = self.telemetry_format {
            cfg.telemetry_format = v;
        }
        if let Some(v) = self.overload_fallback {
            cfg.overload_fallback = v;
        }
        if let Some(v) = self.auto_adapt {
            cfg.auto_adapt = v;
        }
        let p = &mut core.procamp;
        if let Some(v) = self.procamp.luma_gain {
            p.luma_gain = v;
        }
        if let Some(v) = self.procamp.pedestal {
            p.pedestal = v;
        }
        if let Some(v) = self.procamp.saturation {
            p.saturation = v;
        }
        if let Some(v) = self.procamp.hue_degrees {
            p.hue_degrees = v;
        }
        if let Some(v) = self.procamp.gamma {
            p.gamma = v;
        }
        if let Some(v) = self.procamp.chroma_shift_px {
            p.chroma_shift_px = v;
        }
    }
}

/// Named presets from SPEC.md §7.
fn preset(name: &str) -> Result<Overrides, String> {
    let mut o = Overrides::default();
    match name {
        "vhs" => {
            o.buffer_frames = Some(12);
            o.jitter_correction = Some(true);
            o.max_shift_px = Some(12);
            o.rate_policy = Some(RatePolicy::Blend);
            o.procamp.pedestal = Some(2.0);
        }
        "svhs" => {
            o.buffer_frames = Some(8);
            o.jitter_correction = Some(true);
            o.rate_policy = Some(RatePolicy::HoldDrop);
        }
        "unstable-composite" => {
            o.buffer_frames = Some(16);
            o.signal_loss_timeout_ms = Some(500);
            o.signal_loss = Some(SignalLossPolicy::TestPattern);
        }
        "low-latency" => {
            o.buffer_frames = Some(2);
            o.rate_policy = Some(RatePolicy::HoldDrop);
            o.jitter_correction = Some(false);
            // Already at the lightest settings (no jitter, hold/drop),
            // so the overload ladder has no stages left here; the flag
            // is set explicitly to document the intent (Issue #21).
            o.overload_fallback = Some(true);
        }
        other => {
            return Err(format!(
                "unknown preset '{other}' (expected: vhs | svhs | unstable-composite | low-latency)"
            ));
        }
    }
    Ok(o)
}

/// Named signal standards (Issue #25): geometry, target rate and
/// field order for a broadcast standard, as one layer of overrides.
fn parse_standard(name: &str) -> Result<Overrides, String> {
    let mut o = Overrides::default();
    match name {
        "ntsc" => {
            o.width = Some(720);
            o.height = Some(480);
            o.target_rate = Some(FrameRate::NTSC_29_97);
            o.field_order = Some(FieldOrder::BottomFieldFirst);
        }
        "pal" => {
            o.width = Some(720);
            o.height = Some(576);
            o.target_rate = Some(FrameRate::PAL_25);
            o.field_order = Some(FieldOrder::TopFieldFirst);
        }
        "1080i" => {
            o.width = Some(1920);
            o.height = Some(1080);
            o.target_rate = Some(FrameRate::NTSC_29_97);
            o.field_order = Some(FieldOrder::TopFieldFirst);
        }
        other => {
            return Err(format!(
                "unknown standard '{other}' (expected: ntsc | pal | 1080i)"
            ));
        }
    }
    Ok(o)
}

/// TOML file shape (flat keys plus a `[procamp]` table, SPEC.md §7).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    width: Option<u32>,
    height: Option<u32>,
    input_width: Option<u32>,
    input_height: Option<u32>,
    crop_left: Option<u32>,
    crop_top: Option<u32>,
    output_width: Option<u32>,
    output_height: Option<u32>,
    pad_left: Option<u32>,
    pad_top: Option<u32>,
    format: Option<String>,
    field_order: Option<String>,
    target_rate: Option<String>,
    rate_policy: Option<String>,
    buffer_frames: Option<usize>,
    jitter_correction: Option<bool>,
    head_switch_lines: Option<u32>,
    head_switch_mode: Option<String>,
    signal_loss: Option<String>,
    signal_loss_timeout_ms: Option<u64>,
    telemetry_interval_ms: Option<u64>,
    max_shift_px: Option<u32>,
    stamp: Option<String>,
    telemetry_format: Option<String>,
    overload_fallback: Option<bool>,
    auto_adapt: Option<bool>,
    procamp: Option<TomlProcamp>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlProcamp {
    luma_gain: Option<f32>,
    pedestal: Option<f32>,
    saturation: Option<f32>,
    hue_degrees: Option<f32>,
    gamma: Option<f32>,
    chroma_shift_px: Option<f32>,
}

impl TomlConfig {
    fn into_overrides(self) -> Result<Overrides, String> {
        let mut o = Overrides {
            width: self.width,
            height: self.height,
            input_width: self.input_width,
            input_height: self.input_height,
            crop_left: self.crop_left,
            crop_top: self.crop_top,
            output_width: self.output_width,
            output_height: self.output_height,
            pad_left: self.pad_left,
            pad_top: self.pad_top,
            buffer_frames: self.buffer_frames,
            jitter_correction: self.jitter_correction,
            head_switch_lines: self.head_switch_lines,
            signal_loss_timeout_ms: self.signal_loss_timeout_ms,
            telemetry_interval_ms: self.telemetry_interval_ms,
            max_shift_px: self.max_shift_px,
            overload_fallback: self.overload_fallback,
            auto_adapt: self.auto_adapt,
            ..Default::default()
        };
        if let Some(s) = self.format {
            o.format = Some(parse_format(&s)?);
        }
        if let Some(s) = self.field_order {
            apply_field_order_setting(&mut o, &s)?;
        }
        if let Some(s) = self.target_rate {
            o.target_rate = Some(parse_rate(&s)?);
        }
        if let Some(s) = self.rate_policy {
            o.rate_policy = Some(parse_rate_policy(&s)?);
        }
        if let Some(s) = self.head_switch_mode {
            o.head_switch_mode = Some(parse_head_switch_mode(&s)?);
        }
        if let Some(s) = self.signal_loss {
            o.signal_loss = Some(parse_signal_loss(&s)?);
        }
        if let Some(s) = self.stamp {
            o.stamp = Some(parse_stamp_mode(&s)?);
        }
        if let Some(s) = self.telemetry_format {
            o.telemetry_format = Some(parse_telemetry_format(&s)?);
        }
        if let Some(p) = self.procamp {
            o.procamp = ProcampOverrides {
                luma_gain: p.luma_gain,
                pedestal: p.pedestal,
                saturation: p.saturation,
                hue_degrees: p.hue_degrees,
                gamma: p.gamma,
                chroma_shift_px: p.chroma_shift_px,
            };
        }
        Ok(o)
    }
}

pub fn parse_format(s: &str) -> Result<PixelFormat, String> {
    match s {
        "yuv420p" => Ok(PixelFormat::Yuv420p),
        "nv12" => Ok(PixelFormat::Nv12),
        "yuv422p" => Ok(PixelFormat::Yuv422p),
        "rgb24" => Ok(PixelFormat::Rgb24),
        "uyvy" => Ok(PixelFormat::Uyvy),
        "yuyv" => Ok(PixelFormat::Yuyv),
        "yuv420p10le" => Ok(PixelFormat::Yuv420p10),
        "yuv422p10le" => Ok(PixelFormat::Yuv422p10),
        other => Err(format!(
            "unknown format '{other}' (expected: yuv420p | yuv422p | rgb24 | uyvy | yuyv | yuv420p10le | yuv422p10le)"
        )),
    }
}

/// Apply one `field_order` setting to a layer of overrides. The
/// value `auto` (Issue #26) enables runtime detection and leaves the
/// order itself untouched, so the fallback stays whatever the lower
/// layers resolved; a concrete order sets the order and disables
/// auto (an explicit choice always wins over an earlier `auto`).
fn apply_field_order_setting(o: &mut Overrides, s: &str) -> Result<(), String> {
    if s == "auto" {
        o.field_order_auto = Some(true);
    } else {
        o.field_order = Some(parse_field_order(s)?);
        o.field_order_auto = Some(false);
    }
    Ok(())
}

pub fn parse_field_order(s: &str) -> Result<FieldOrder, String> {
    match s {
        "progressive" => Ok(FieldOrder::Progressive),
        "top-first" => Ok(FieldOrder::TopFieldFirst),
        "bottom-first" => Ok(FieldOrder::BottomFieldFirst),
        other => Err(format!("unknown field_order '{other}'")),
    }
}

/// Parse `30000/1001` or a plain integer rate like `30`.
pub fn parse_head_switch_mode(s: &str) -> Result<HeadSwitchMode, String> {
    match s {
        "replicate" => Ok(HeadSwitchMode::Replicate),
        "interpolate" => Ok(HeadSwitchMode::Interpolate),
        other => Err(format!(
            "unknown head_switch_mode '{other}' (expected: replicate | interpolate)"
        )),
    }
}

pub fn parse_rate(s: &str) -> Result<FrameRate, String> {
    if let Some((num, den)) = s.split_once('/') {
        let num: u32 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid target_rate '{s}'"))?;
        let den: u32 = den
            .trim()
            .parse()
            .map_err(|_| format!("invalid target_rate '{s}'"))?;
        if num == 0 || den == 0 {
            return Err(format!("invalid target_rate '{s}': zero component"));
        }
        Ok(FrameRate { num, den })
    } else {
        let num: u32 = s
            .trim()
            .parse()
            .map_err(|_| format!("invalid target_rate '{s}'"))?;
        if num == 0 {
            return Err(format!("invalid target_rate '{s}': zero"));
        }
        Ok(FrameRate { num, den: 1 })
    }
}

pub fn parse_rate_policy(s: &str) -> Result<RatePolicy, String> {
    match s {
        "hold-drop" => Ok(RatePolicy::HoldDrop),
        "blend" => Ok(RatePolicy::Blend),
        "motion-interpolate" => Ok(RatePolicy::MotionInterpolate),
        other => Err(format!("unknown rate_policy '{other}'")),
    }
}

pub fn parse_stamp_mode(s: &str) -> Result<StampMode, String> {
    match s {
        "nominal" => Ok(StampMode::Nominal),
        "wall-clock" => Ok(StampMode::WallClock),
        other => Err(format!(
            "unknown stamp mode '{other}' (expected: nominal | wall-clock)"
        )),
    }
}

/// Telemetry output format (Issue #36): JSON Lines, or CSV with a
/// fixed column schema shared by periodic, event and summary rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryFormat {
    Json,
    Csv,
}

pub fn parse_telemetry_format(s: &str) -> Result<TelemetryFormat, String> {
    match s {
        "json" => Ok(TelemetryFormat::Json),
        "csv" => Ok(TelemetryFormat::Csv),
        other => Err(format!(
            "unknown telemetry_format '{other}' (expected: json | csv)"
        )),
    }
}

pub fn parse_signal_loss(s: &str) -> Result<SignalLossPolicy, String> {
    match s {
        "last-frame-hold" => Ok(SignalLossPolicy::LastFrameHold),
        "blackout" => Ok(SignalLossPolicy::Blackout),
        "test-pattern" => Ok(SignalLossPolicy::TestPattern),
        "passthrough" => Ok(SignalLossPolicy::Passthrough),
        other => Err(format!("unknown signal_loss '{other}'")),
    }
}

/// Resolve the effective configuration from defaults, preset, TOML and
/// CLI flags, then validate it.
pub fn resolve(args: &Args) -> Result<CliConfig, String> {
    let mut cfg = CliConfig::default();

    if let Some(name) = &args.preset {
        preset(name)?.apply_to(&mut cfg);
    }
    if let Some(path) = &args.config {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config file '{path}': {e}"))?;
        let toml_cfg: TomlConfig =
            toml::from_str(&text).map_err(|e| format!("invalid config file '{path}': {e}"))?;
        toml_cfg.into_overrides()?.apply_to(&mut cfg);
    }

    // The standard layer sits above TOML (it is an explicit CLI
    // choice) and below the individual CLI flags, which override it
    // field by field (Issue #25).
    if let Some(name) = &args.standard {
        parse_standard(name)?.apply_to(&mut cfg);
    }

    // CLI layer: only flags the user actually set.
    let mut o = Overrides::default();
    if let Some(v) = args.width {
        o.width = Some(v);
    }
    if let Some(v) = args.height {
        o.height = Some(v);
    }
    if let Some(s) = &args.format {
        o.format = Some(parse_format(s)?);
    }
    if let Some(s) = &args.field_order {
        apply_field_order_setting(&mut o, s)?;
    }
    if let Some(v) = args.input_width {
        o.input_width = Some(v);
    }
    if let Some(v) = args.input_height {
        o.input_height = Some(v);
    }
    if let Some(v) = args.crop_left {
        o.crop_left = Some(v);
    }
    if let Some(v) = args.crop_top {
        o.crop_top = Some(v);
    }
    if let Some(v) = args.output_width {
        o.output_width = Some(v);
    }
    if let Some(v) = args.output_height {
        o.output_height = Some(v);
    }
    if let Some(v) = args.pad_left {
        o.pad_left = Some(v);
    }
    if let Some(v) = args.pad_top {
        o.pad_top = Some(v);
    }
    if let Some(v) = args.head_switch_lines {
        o.head_switch_lines = Some(v);
    }
    if let Some(s) = &args.head_switch_mode {
        o.head_switch_mode = Some(parse_head_switch_mode(s)?);
    }
    if let Some(s) = &args.stamp {
        o.stamp = Some(parse_stamp_mode(s)?);
    }
    if let Some(s) = &args.signal_loss {
        o.signal_loss = Some(parse_signal_loss(s)?);
    }
    if let Some(s) = &args.telemetry_format {
        o.telemetry_format = Some(parse_telemetry_format(s)?);
    }
    o.apply_to(&mut cfg);

    if args.no_overload_fallback {
        cfg.overload_fallback = false;
    }
    if args.auto_adapt {
        cfg.auto_adapt = true;
    }

    cfg.input = args.input.clone();
    cfg.output = args.output.clone();
    cfg.max_frames = args.max_frames;
    cfg.fail_on_signal_loss = args.fail_on_signal_loss;

    cfg.core
        .validate()
        .map_err(|e| format!("invalid configuration: {e}"))?;

    // Geometry (Issue #31): unset input/output sizes follow the
    // resolved processing size.
    if cfg.input_width == 0 {
        cfg.input_width = cfg.core.width;
    }
    if cfg.input_height == 0 {
        cfg.input_height = cfg.core.height;
    }
    if cfg.output_width == 0 {
        cfg.output_width = cfg.core.width;
    }
    if cfg.output_height == 0 {
        cfg.output_height = cfg.core.height;
    }
    validate_geometry(&cfg)?;
    Ok(cfg)
}

/// Validate the separated input/processing/output geometry
/// (Issue #31): windows must fit, and offsets must keep chroma
/// sample alignment for the wire format's subsampling class.
fn validate_geometry(cfg: &CliConfig) -> Result<(), String> {
    let invalid = |msg: String| format!("invalid configuration: {msg}");
    // Even-dimension rules for a frame size in this format.
    let check_size = |w: u32, h: u32, what: &str| -> Result<(), String> {
        match cfg.core.format {
            PixelFormat::Yuv420p | PixelFormat::Yuv420p10 | PixelFormat::Nv12
                if w % 2 == 1 || h % 2 == 1 =>
            {
                Err(invalid(format!(
                    "{what} {w}x{h} must be even in both dimensions for 4:2:0"
                )))
            }
            PixelFormat::Yuv422p10 | PixelFormat::Uyvy | PixelFormat::Yuyv if w % 2 == 1 => {
                Err(invalid(format!(
                    "{what} width {w} must be even for packed/planar 4:2:2"
                )))
            }
            _ => Ok(()),
        }
    };
    check_size(cfg.input_width, cfg.input_height, "input size")?;
    check_size(cfg.output_width, cfg.output_height, "output size")?;
    // Offset parity: 4:2:0 subsamples chroma on both axes, the
    // packed/4:2:2 family horizontally only, RGB not at all.
    let check_offsets = |left: u32, top: u32, what: &str| -> Result<(), String> {
        match cfg.core.format {
            PixelFormat::Yuv420p | PixelFormat::Yuv420p10 | PixelFormat::Nv12
                if left % 2 == 1 || top % 2 == 1 =>
            {
                Err(invalid(format!(
                    "{what} offsets {left},{top} must be even for 4:2:0"
                )))
            }
            PixelFormat::Yuv422p
            | PixelFormat::Yuv422p10
            | PixelFormat::Uyvy
            | PixelFormat::Yuyv
                if left % 2 == 1 =>
            {
                Err(invalid(format!(
                    "{what} left offset {left} must be even for 4:2:2"
                )))
            }
            _ => Ok(()),
        }
    };
    check_offsets(cfg.crop_left, cfg.crop_top, "crop")?;
    check_offsets(cfg.pad_left, cfg.pad_top, "pad")?;
    if cfg.crop_left as u64 + cfg.core.width as u64 > cfg.input_width as u64
        || cfg.crop_top as u64 + cfg.core.height as u64 > cfg.input_height as u64
    {
        return Err(invalid(format!(
            "crop window at {},{} of {}x{} does not fit in input {}x{}",
            cfg.crop_left,
            cfg.crop_top,
            cfg.core.width,
            cfg.core.height,
            cfg.input_width,
            cfg.input_height
        )));
    }
    if cfg.pad_left as u64 + cfg.core.width as u64 > cfg.output_width as u64
        || cfg.pad_top as u64 + cfg.core.height as u64 > cfg.output_height as u64
    {
        return Err(invalid(format!(
            "processed {}x{} at pad {},{} does not fit in output {}x{}",
            cfg.core.width,
            cfg.core.height,
            cfg.pad_left,
            cfg.pad_top,
            cfg.output_width,
            cfg.output_height
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(preset: Option<&str>, config: Option<&str>) -> Args {
        Args {
            input: "-".into(),
            output: "-".into(),
            width: None,
            height: None,
            input_width: None,
            input_height: None,
            crop_left: None,
            crop_top: None,
            output_width: None,
            output_height: None,
            pad_left: None,
            pad_top: None,
            format: None,
            field_order: None,
            head_switch_lines: None,
            head_switch_mode: None,
            preset: preset.map(str::to_string),
            standard: None,
            config: config.map(str::to_string),
            stamp: None,
            signal_loss: None,
            telemetry_format: None,
            no_overload_fallback: false,
            auto_adapt: false,
            max_frames: None,
            fail_on_signal_loss: false,
        }
    }

    #[test]
    fn defaults_apply_without_layers() {
        let cfg = resolve(&args(None, None)).expect("resolve");
        assert_eq!(cfg.core.buffer_frames, 8);
        assert_eq!(cfg.signal_loss_timeout_ms, 200);
    }

    #[test]
    fn vhs_preset_values() {
        let cfg = resolve(&args(Some("vhs"), None)).expect("resolve");
        assert_eq!(cfg.core.buffer_frames, 12);
        assert_eq!(cfg.core.rate_policy, RatePolicy::Blend);
        assert_eq!(cfg.core.procamp.pedestal, 2.0);
        assert_eq!(cfg.max_shift_px, 12);
    }

    #[test]
    fn unknown_preset_is_an_error() {
        assert!(resolve(&args(Some("betamax"), None)).is_err());
    }

    #[test]
    fn nv12_format_parses_and_validates_like_420() {
        let mut a = args(None, None);
        a.format = Some("nv12".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.format, PixelFormat::Nv12);
        // Odd processing height is rejected for the 4:2:0 class.
        a.height = Some(479);
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn signal_loss_passthrough_layers() {
        assert_eq!(
            parse_signal_loss("passthrough").expect("parse passthrough"),
            SignalLossPolicy::Passthrough
        );
        // Default is last-frame-hold.
        let a = args(None, None);
        assert_eq!(
            resolve(&a).expect("resolve").core.signal_loss,
            SignalLossPolicy::LastFrameHold
        );
        // TOML selects passthrough.
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-sl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "signal_loss = \"passthrough\"\n").expect("write toml");
        let a = args(None, path.to_str());
        assert_eq!(
            resolve(&a).expect("resolve").core.signal_loss,
            SignalLossPolicy::Passthrough
        );
        // The CLI flag overrides TOML, and TOML overrides the flag's
        // absence only (an explicit CLI choice always wins).
        let mut a = args(None, path.to_str());
        a.signal_loss = Some("blackout".to_string());
        assert_eq!(
            resolve(&a).expect("resolve").core.signal_loss,
            SignalLossPolicy::Blackout
        );
        let mut a = args(None, None);
        a.signal_loss = Some("passthrough".to_string());
        assert_eq!(
            resolve(&a).expect("resolve").core.signal_loss,
            SignalLossPolicy::Passthrough
        );
        assert!(parse_signal_loss("freeze").is_err());
    }

    #[test]
    fn telemetry_format_layers() {
        let a = args(None, None);
        assert_eq!(
            resolve(&a).expect("resolve").telemetry_format,
            TelemetryFormat::Json
        );
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-tf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "telemetry_format = \"csv\"\n").expect("write toml");
        let a = args(None, path.to_str());
        assert_eq!(
            resolve(&a).expect("resolve").telemetry_format,
            TelemetryFormat::Csv
        );
        let mut a = args(None, path.to_str());
        a.telemetry_format = Some("json".to_string());
        assert_eq!(
            resolve(&a).expect("resolve").telemetry_format,
            TelemetryFormat::Json
        );
        assert!(parse_telemetry_format("yaml").is_err());
    }

    #[test]
    fn auto_adapt_resolves_from_toml_and_cli() {
        let dir =
            std::env::temp_dir().join(format!("software-tbc-cli-adapt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "auto_adapt = true\n").expect("write toml");
        let cfg = resolve(&args(None, path.to_str())).expect("resolve");
        assert!(cfg.auto_adapt);
        let _ = std::fs::remove_dir_all(&dir);

        let mut a = args(None, None);
        assert!(!resolve(&a).expect("resolve").auto_adapt);
        a.auto_adapt = true;
        assert!(resolve(&a).expect("resolve").auto_adapt);
    }

    #[test]
    fn geometry_defaults_follow_processing_size() {
        let mut a = args(None, None);
        a.width = Some(352);
        let cfg = resolve(&a).expect("resolve");
        assert_eq!((cfg.input_width, cfg.input_height), (352, 480));
        assert_eq!((cfg.output_width, cfg.output_height), (352, 480));
        assert_eq!(
            (cfg.crop_left, cfg.crop_top, cfg.pad_left, cfg.pad_top),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn geometry_toml_and_cli_layers_resolve() {
        let dir =
            std::env::temp_dir().join(format!("software-tbc-cli-geom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(
            &path,
            "input_width = 720\ninput_height = 486\ncrop_top = 6\noutput_width = 728\npad_left = 4\n",
        )
        .expect("write toml");
        let mut a = args(None, path.to_str());
        a.crop_top = Some(4); // CLI beats TOML.
        let cfg = resolve(&a).expect("resolve");
        assert_eq!((cfg.input_width, cfg.input_height), (720, 486));
        assert_eq!(cfg.crop_top, 4);
        assert_eq!((cfg.output_width, cfg.pad_left), (728, 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn geometry_window_overflow_is_an_error() {
        // Crop window taller than the input.
        let mut a = args(None, None);
        a.input_height = Some(240);
        assert!(resolve(&a).is_err());
        // Pad window wider than the output.
        let mut a = args(None, None);
        a.output_width = Some(724);
        a.pad_left = Some(8);
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn geometry_odd_offsets_are_an_error_for_422() {
        let mut a = args(None, None);
        a.input_width = Some(726);
        a.crop_left = Some(3);
        assert!(resolve(&a).is_err());
        // Even offsets are fine.
        a.crop_left = Some(2);
        assert!(resolve(&a).is_ok());
    }

    #[test]
    fn toml_procamp_chroma_shift_resolves() {
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-cs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "[procamp]\nchroma_shift_px = 1.5\n").expect("write toml");
        let cfg = resolve(&args(None, path.to_str())).expect("resolve");
        assert_eq!(cfg.core.procamp.chroma_shift_px, 1.5);
        // Untouched controls stay neutral.
        assert_eq!(cfg.core.procamp.saturation, 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_overrides_preset_and_cli_overrides_toml() {
        let dir =
            std::env::temp_dir().join(format!("software-tbc-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(
            &path,
            "buffer_frames = 20\ntarget_rate = \"25/1\"\n\n[procamp]\npedestal = 5.0\n",
        )
        .expect("write toml");

        let mut a = args(Some("vhs"), path.to_str());
        let cfg = resolve(&a).expect("resolve");
        // TOML beats preset for buffer_frames and pedestal.
        assert_eq!(cfg.core.buffer_frames, 20);
        assert_eq!(cfg.core.procamp.pedestal, 5.0);
        assert_eq!(cfg.core.target_rate, FrameRate { num: 25, den: 1 });
        // Preset still supplies what TOML does not set.
        assert_eq!(cfg.core.rate_policy, RatePolicy::Blend);

        // CLI beats TOML.
        a.width = Some(352);
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.width, 352);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_mode_defaults_to_nominal_and_layers_apply() {
        // Default: nominal.
        let cfg = resolve(&args(None, None)).expect("resolve");
        assert_eq!(cfg.stamp, StampMode::Nominal);

        // TOML sets wall-clock.
        let dir =
            std::env::temp_dir().join(format!("software-tbc-cli-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "stamp = \"wall-clock\"\n").expect("write toml");
        let mut a = args(None, path.to_str());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.stamp, StampMode::WallClock);

        // CLI beats TOML.
        a.stamp = Some("nominal".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.stamp, StampMode::Nominal);
        let _ = std::fs::remove_dir_all(&dir);

        // Unknown values are errors in both layers.
        let mut a = args(None, None);
        a.stamp = Some("realtime".to_string());
        assert!(resolve(&a).is_err());
        assert!(parse_stamp_mode("realtime").is_err());
    }

    #[test]
    fn overload_fallback_layers() {
        // Default: enabled.
        let cfg = resolve(&args(None, None)).expect("resolve");
        assert!(cfg.overload_fallback);

        // The low-latency preset sets it explicitly.
        let cfg = resolve(&args(Some("low-latency"), None)).expect("resolve");
        assert!(cfg.overload_fallback);

        // TOML can disable it.
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-fb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "overload_fallback = false\n").expect("write toml");
        let mut a = args(None, path.to_str());
        let cfg = resolve(&a).expect("resolve");
        assert!(!cfg.overload_fallback);

        // The CLI kill switch wins over a TOML that enables it.
        std::fs::write(&path, "overload_fallback = true\n").expect("write toml");
        a.no_overload_fallback = true;
        let cfg = resolve(&a).expect("resolve");
        assert!(!cfg.overload_fallback);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_switch_resolution() {
        use tbc_core::head_switch::HeadSwitchMode;

        // Default: disabled.
        let cfg = resolve(&args(None, None)).expect("resolve");
        assert_eq!(cfg.core.head_switch.lines, 0);

        // TOML sets both keys.
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-hs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(
            &path,
            "head_switch_lines = 6\nhead_switch_mode = \"interpolate\"\n",
        )
        .expect("write toml");
        let cfg = resolve(&args(None, path.to_str())).expect("resolve");
        assert_eq!(cfg.core.head_switch.lines, 6);
        assert_eq!(cfg.core.head_switch.mode, HeadSwitchMode::Interpolate);
        let _ = std::fs::remove_dir_all(&dir);

        // CLI flags override TOML layer by layer.
        let mut a = args(None, None);
        a.head_switch_lines = Some(3);
        a.head_switch_mode = Some("replicate".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.head_switch.lines, 3);
        assert_eq!(cfg.core.head_switch.mode, HeadSwitchMode::Replicate);

        // Unknown mode and a mask covering the whole frame fail.
        a.head_switch_mode = Some("smear".to_string());
        assert!(resolve(&a).is_err());
        let mut a = args(None, None);
        a.head_switch_lines = Some(480);
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn field_order_auto_resolution() {
        // Default: no auto; the fallback is the core default (BFF).
        let cfg = resolve(&args(None, None)).expect("resolve");
        assert!(!cfg.field_order_auto);
        assert_eq!(cfg.core.field_order, FieldOrder::BottomFieldFirst);

        // TOML "auto": detection on, fallback untouched.
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-fo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "field_order = \"auto\"\n").expect("write toml");
        let mut a = args(None, path.to_str());
        let cfg = resolve(&a).expect("resolve");
        assert!(cfg.field_order_auto);
        assert_eq!(cfg.core.field_order, FieldOrder::BottomFieldFirst);

        // TOML auto + standard pal: the standard supplies the
        // fallback (TFF) and auto stays on.
        a.standard = Some("pal".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert!(cfg.field_order_auto);
        assert_eq!(cfg.core.field_order, FieldOrder::TopFieldFirst);

        // A concrete CLI order beats TOML auto and disables it.
        a.field_order = Some("bottom-first".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert!(!cfg.field_order_auto);
        assert_eq!(cfg.core.field_order, FieldOrder::BottomFieldFirst);
        let _ = std::fs::remove_dir_all(&dir);

        // CLI auto alone.
        let mut a = args(None, None);
        a.field_order = Some("auto".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert!(cfg.field_order_auto);

        // Unknown values are errors.
        a.field_order = Some("sideways".to_string());
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn standard_presets_resolve() {
        let mut a = args(None, None);
        a.standard = Some("ntsc".to_string());
        let cfg = resolve(&a).expect("ntsc");
        assert_eq!((cfg.core.width, cfg.core.height), (720, 480));
        assert_eq!(cfg.core.target_rate, FrameRate::NTSC_29_97);
        assert_eq!(cfg.core.field_order, FieldOrder::BottomFieldFirst);

        a.standard = Some("pal".to_string());
        let cfg = resolve(&a).expect("pal");
        assert_eq!((cfg.core.width, cfg.core.height), (720, 576));
        assert_eq!(cfg.core.target_rate, FrameRate::PAL_25);
        assert_eq!(cfg.core.field_order, FieldOrder::TopFieldFirst);

        a.standard = Some("1080i".to_string());
        let cfg = resolve(&a).expect("1080i");
        assert_eq!((cfg.core.width, cfg.core.height), (1920, 1080));
        assert_eq!(cfg.core.target_rate, FrameRate::NTSC_29_97);
        assert_eq!(cfg.core.field_order, FieldOrder::TopFieldFirst);

        a.standard = Some("secam".to_string());
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn standard_layer_order() {
        // TOML sits below the standard layer; individual CLI flags
        // sit above it.
        let dir = std::env::temp_dir().join(format!("software-tbc-cli-std-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tbc.toml");
        std::fs::write(&path, "width = 352\nheight = 288\n").expect("write toml");
        let mut a = args(None, path.to_str());
        a.standard = Some("pal".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!((cfg.core.width, cfg.core.height), (720, 576));
        a.width = Some(352);
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.width, 352);
        assert_eq!(cfg.core.height, 576);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_parsing_includes_packed_422() {
        assert_eq!(parse_format("yuv420p").expect("420"), PixelFormat::Yuv420p);
        assert_eq!(parse_format("yuv422p").expect("422"), PixelFormat::Yuv422p);
        assert_eq!(parse_format("rgb24").expect("rgb"), PixelFormat::Rgb24);
        assert_eq!(parse_format("uyvy").expect("uyvy"), PixelFormat::Uyvy);
        assert_eq!(parse_format("yuyv").expect("yuyv"), PixelFormat::Yuyv);
        assert_eq!(
            parse_format("yuv420p10le").expect("420p10"),
            PixelFormat::Yuv420p10
        );
        assert_eq!(
            parse_format("yuv422p10le").expect("422p10"),
            PixelFormat::Yuv422p10
        );
        assert_eq!(parse_format("nv12").expect("nv12"), PixelFormat::Nv12);
        assert!(parse_format("nv21").is_err());

        // A packed format resolves end to end and validates at even width.
        let mut a = args(None, None);
        a.format = Some("uyvy".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.format, PixelFormat::Uyvy);
        // ... and is rejected at an odd width.
        a.width = Some(719);
        assert!(resolve(&a).is_err());

        // A 10-bit format resolves the same way (Issue #28).
        let mut a = args(None, None);
        a.format = Some("yuv422p10le".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.format, PixelFormat::Yuv422p10);
        a.width = Some(719);
        assert!(resolve(&a).is_err());
    }

    #[test]
    fn rate_parsing() {
        assert_eq!(
            parse_rate("30000/1001").expect("ntsc"),
            FrameRate {
                num: 30_000,
                den: 1001
            }
        );
        assert_eq!(
            parse_rate("30").expect("plain"),
            FrameRate { num: 30, den: 1 }
        );
        assert!(parse_rate("29.97").is_err());
        assert!(parse_rate("30000/0").is_err());
    }
}
