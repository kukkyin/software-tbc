//! software-tbc command-line front end.
//!
//! Scaffold state: parses arguments and prints the resolved configuration.
//! The streaming pipeline is wired up by the Phase 3 issues (see SPEC.md).

use clap::Parser;

/// Software TBC: read raw video, correct its time base, write raw video.
///
/// Typical use is in a pipe with FFmpeg, e.g.
/// `ffmpeg -i in.mp4 -f rawvideo -pix_fmt yuv422p - | software-tbc ... | ffmpeg ...`
#[derive(Parser, Debug)]
#[command(name = "software-tbc", version, about)]
struct Args {
    /// Input raw file, or `-` for stdin.
    #[arg(long, default_value = "-")]
    input: String,
    /// Output raw file, or `-` for stdout.
    #[arg(long, default_value = "-")]
    output: String,
    /// Frame width in pixels.
    #[arg(long, default_value_t = 720)]
    width: u32,
    /// Frame height in pixels.
    #[arg(long, default_value_t = 480)]
    height: u32,
    /// Pixel format: yuv420p | yuv422p | rgb24.
    #[arg(long, default_value = "yuv422p")]
    format: String,
    /// Named preset: vhs | svhs | unstable-composite | low-latency.
    #[arg(long)]
    preset: Option<String>,
    /// TOML configuration file (CLI flags override it).
    #[arg(long)]
    config: Option<String>,
}

fn main() {
    let args = Args::parse();
    eprintln!("software-tbc scaffold: {args:?}");
    eprintln!("pipeline not wired yet — see the Phase 3 issues in this repository");
}
