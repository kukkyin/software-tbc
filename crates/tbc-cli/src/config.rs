//! Configuration resolution: presets, TOML file and CLI flags.
//!
//! Precedence (SPEC.md §6/§7): CLI flags > TOML file > preset > defaults.
//! Each layer is a set of optional fields; later layers overwrite only
//! the fields they actually set.

use serde::Deserialize;
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
    pub signal_loss_timeout_ms: u64,
    pub telemetry_interval_ms: u64,
    pub max_shift_px: u32,
    pub stamp: StampMode,
    pub overload_fallback: bool,
    pub input: String,
    pub output: String,
    pub max_frames: Option<u64>,
    pub fail_on_signal_loss: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        CliConfig {
            core: TbcConfig::default(),
            signal_loss_timeout_ms: 200,
            telemetry_interval_ms: 1000,
            max_shift_px: 8,
            stamp: StampMode::Nominal,
            overload_fallback: true,
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
    format: Option<PixelFormat>,
    field_order: Option<FieldOrder>,
    target_rate: Option<FrameRate>,
    rate_policy: Option<RatePolicy>,
    buffer_frames: Option<usize>,
    jitter_correction: Option<bool>,
    signal_loss: Option<SignalLossPolicy>,
    signal_loss_timeout_ms: Option<u64>,
    telemetry_interval_ms: Option<u64>,
    max_shift_px: Option<u32>,
    stamp: Option<StampMode>,
    overload_fallback: Option<bool>,
    procamp: ProcampOverrides,
}

#[derive(Debug, Default, Clone)]
struct ProcampOverrides {
    luma_gain: Option<f32>,
    pedestal: Option<f32>,
    saturation: Option<f32>,
    hue_degrees: Option<f32>,
    gamma: Option<f32>,
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
        if let Some(v) = self.format {
            core.format = v;
        }
        if let Some(v) = self.field_order {
            core.field_order = v;
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
        if let Some(v) = self.overload_fallback {
            cfg.overload_fallback = v;
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

/// TOML file shape (flat keys plus a `[procamp]` table, SPEC.md §7).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    width: Option<u32>,
    height: Option<u32>,
    format: Option<String>,
    field_order: Option<String>,
    target_rate: Option<String>,
    rate_policy: Option<String>,
    buffer_frames: Option<usize>,
    jitter_correction: Option<bool>,
    signal_loss: Option<String>,
    signal_loss_timeout_ms: Option<u64>,
    telemetry_interval_ms: Option<u64>,
    max_shift_px: Option<u32>,
    stamp: Option<String>,
    overload_fallback: Option<bool>,
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
}

impl TomlConfig {
    fn into_overrides(self) -> Result<Overrides, String> {
        let mut o = Overrides {
            width: self.width,
            height: self.height,
            buffer_frames: self.buffer_frames,
            jitter_correction: self.jitter_correction,
            signal_loss_timeout_ms: self.signal_loss_timeout_ms,
            telemetry_interval_ms: self.telemetry_interval_ms,
            max_shift_px: self.max_shift_px,
            overload_fallback: self.overload_fallback,
            ..Default::default()
        };
        if let Some(s) = self.format {
            o.format = Some(parse_format(&s)?);
        }
        if let Some(s) = self.field_order {
            o.field_order = Some(parse_field_order(&s)?);
        }
        if let Some(s) = self.target_rate {
            o.target_rate = Some(parse_rate(&s)?);
        }
        if let Some(s) = self.rate_policy {
            o.rate_policy = Some(parse_rate_policy(&s)?);
        }
        if let Some(s) = self.signal_loss {
            o.signal_loss = Some(parse_signal_loss(&s)?);
        }
        if let Some(s) = self.stamp {
            o.stamp = Some(parse_stamp_mode(&s)?);
        }
        if let Some(p) = self.procamp {
            o.procamp = ProcampOverrides {
                luma_gain: p.luma_gain,
                pedestal: p.pedestal,
                saturation: p.saturation,
                hue_degrees: p.hue_degrees,
                gamma: p.gamma,
            };
        }
        Ok(o)
    }
}

pub fn parse_format(s: &str) -> Result<PixelFormat, String> {
    match s {
        "yuv420p" => Ok(PixelFormat::Yuv420p),
        "yuv422p" => Ok(PixelFormat::Yuv422p),
        "rgb24" => Ok(PixelFormat::Rgb24),
        "uyvy" => Ok(PixelFormat::Uyvy),
        "yuyv" => Ok(PixelFormat::Yuyv),
        other => Err(format!(
            "unknown format '{other}' (expected: yuv420p | yuv422p | rgb24 | uyvy | yuyv)"
        )),
    }
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

pub fn parse_signal_loss(s: &str) -> Result<SignalLossPolicy, String> {
    match s {
        "last-frame-hold" => Ok(SignalLossPolicy::LastFrameHold),
        "blackout" => Ok(SignalLossPolicy::Blackout),
        "test-pattern" => Ok(SignalLossPolicy::TestPattern),
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
    if let Some(s) = &args.stamp {
        o.stamp = Some(parse_stamp_mode(s)?);
    }
    o.apply_to(&mut cfg);

    if args.no_overload_fallback {
        cfg.overload_fallback = false;
    }

    cfg.input = args.input.clone();
    cfg.output = args.output.clone();
    cfg.max_frames = args.max_frames;
    cfg.fail_on_signal_loss = args.fail_on_signal_loss;

    cfg.core
        .validate()
        .map_err(|e| format!("invalid configuration: {e}"))?;
    Ok(cfg)
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
            format: None,
            preset: preset.map(str::to_string),
            config: config.map(str::to_string),
            stamp: None,
            no_overload_fallback: false,
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
    fn format_parsing_includes_packed_422() {
        assert_eq!(parse_format("yuv420p").expect("420"), PixelFormat::Yuv420p);
        assert_eq!(parse_format("yuv422p").expect("422"), PixelFormat::Yuv422p);
        assert_eq!(parse_format("rgb24").expect("rgb"), PixelFormat::Rgb24);
        assert_eq!(parse_format("uyvy").expect("uyvy"), PixelFormat::Uyvy);
        assert_eq!(parse_format("yuyv").expect("yuyv"), PixelFormat::Yuyv);
        assert!(parse_format("nv12").is_err());

        // A packed format resolves end to end and validates at even width.
        let mut a = args(None, None);
        a.format = Some("uyvy".to_string());
        let cfg = resolve(&a).expect("resolve");
        assert_eq!(cfg.core.format, PixelFormat::Uyvy);
        // ... and is rejected at an odd width.
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
