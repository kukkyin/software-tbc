# Audio/Video Synchronization

software-tbc processes **video only**. Audio never passes through it;
audio is handled entirely on the FFmpeg side. This document explains
how much delay the TBC adds, when that delay matters for audio sync,
and how to compensate and verify.

## Two different kinds of "delay"

Keep these apart — confusing them is the most common source of sync
mistakes with a raw-video pipe:

- **Real-time latency**: how long a frame spends inside the TBC
  between arriving and leaving. This is the
  `buffer_frames / 2 + 1` figure from SPEC §5.
- **Media-timeline offset**: how far the output video's timestamps
  sit from the audio's timestamps in the recorded file.

RAW video carries no timestamps. FFmpeg regenerates the video
timeline from the frame sequence and the `-framerate` you declare
when reading the TBC's output. software-tbc stamps frames internally
and emits them on a constant-rate grid, duplicating or dropping
frames to hold that grid — so the output frame sequence stays
aligned with the input's duration. The consequence:

- **Live capture**: the video path really is late by the real-time
  latency while the audio path is not, so the recorded video lags
  the audio by that amount. Compensate (Recipe A).
- **File processing**: the timeline is rebuilt from the frame
  sequence, so the processing latency does **not** become a timeline
  offset. Expect at most about one frame of residual offset from
  frame selection. Do not apply the table value — that would
  *create* an offset. Verify and trim only what you measure
  (Recipe B).

## Added latency (real time)

`latency_frames = buffer_frames / 2 + 1`, and
`latency_ms = latency_frames × 1000 / fps`. For a custom
`buffer_frames`, compute it from the formula.

| Configuration | buffer_frames | Latency | @ 29.97 fps (30000/1001) | @ 25 fps | Samples @ 48 kHz (29.97 / 25) |
|---|---|---|---|---|---|
| default, `svhs` | 8 | 5 frames | ≈ 166.8 ms | 200 ms | ≈ 8008 / 9600 |
| `vhs` | 12 | 7 frames | ≈ 233.6 ms | 280 ms | ≈ 11211 / 13440 |
| `unstable-composite` | 16 | 9 frames | ≈ 300.3 ms | 360 ms | ≈ 14414 / 17280 |
| `low-latency` | 2 | 2 frames | ≈ 66.7 ms | 80 ms | ≈ 3203 / 3840 |

29.97 fps is exactly 30000/1001, so 5 frames is 166.83 ms, not
exactly 167 ms; the table rounds to one decimal.

## Recipe A: live capture — delay the audio

One FFmpeg process cannot insert an external program in the middle
of a filter graph, so the capture runs as two FFmpeg processes
around software-tbc. The first sends video through the TBC and
writes the captured audio to a FIFO (a plain file also works when
recording, at the cost of a manual merge); the second muxes
everything and delays the audio by the latency from the table
(default preset, NTSC: 167 ms).

```bash
mkfifo audio.fifo

# Capture: video -> stdout (into software-tbc), audio -> FIFO.
ffmpeg -f v4l2 -framerate 30000/1001 -video_size 720x480 -i /dev/video0 \
       -f alsa -i hw:1 \
       -map 0:v -f rawvideo -pix_fmt yuv422p pipe:1 \
       -map 1:a -c:a pcm_s16le -f wav audio.fifo | \
software-tbc --preset svhs --width 720 --height 480 --format yuv422p | \
ffmpeg -f rawvideo -pix_fmt yuv422p -video_size 720x480 \
       -framerate 30000/1001 -i pipe:0 \
       -f wav -i audio.fifo \
       -map 0:v -map 1:a -af "adelay=167|167" \
       -c:v libx264 -c:a aac -b:a 192k out.mkv
```

Notes:

- `adelay` delays per channel: `adelay=167` alone delays only the
  first channel. Use `adelay=167|167` for stereo, or
  `adelay=delays=167:all=1` for any channel count.
- Any audio filter (`adelay`, `aresample`) requires decoding and
  re-encoding the audio — `-c:a copy` cannot be combined with `-af`.
- The alternative is shifting the video input's timestamps with
  `-itsoffset -0.167` before the rawvideo input of the second
  FFmpeg. Use **one** method, never both, or the correction is
  applied twice.
- This assumes a constant frame rate end to end. Variable-rate
  capture sources break the fixed-delay assumption; check the
  capture device's actual rate first.

## Recipe B: processing an existing file — no fixed delay

```bash
ffmpeg -i in.mkv -map 0:v -f rawvideo -pix_fmt yuv422p - \
  | software-tbc --preset vhs --width 720 --height 480 --format yuv422p \
  | ffmpeg -f rawvideo -pix_fmt yuv422p -video_size 720x480 \
           -framerate 30000/1001 -i - \
           -i in.mkv -map 0:v -map 1:a -c:v libx264 -c:a copy out.mkv
```

The video timeline is regenerated from the frame sequence, so no
`adelay` is applied and the original audio can be stream-copied.
Always state size, pixel format and rate explicitly on both sides
of the pipe: raw video carries none of them (nor field order —
declare interlacing on the FFmpeg side consistently). Let
software-tbc own frame duplication/dropping; do not let the
receiving FFmpeg redo it (`-fps_mode:v passthrough` where
applicable).

## Long runs: clock drift is a separate problem

`aresample=async` compensates discrepancies between an audio
stream's timestamps and its sample count — for example when the
audio and video capture devices run on slightly different clocks.
It does **not** observe or follow software-tbc's frame duplication
and dropping, and it is not a substitute for the fixed delay in
Recipe A.

- If both streams' timestamps are trustworthy and aligned, you do
  not need `aresample` at all.
- If the offset grows linearly over time, measure the drift and set
  the compensation cap from it: `async` is the maximum number of
  samples per second that may be added or removed, so
  1000 ppm at 48 kHz is `aresample=async=48`. Larger is not better;
  an oversized cap lets the resampler chase timestamp noise.
- `min_hard_comp` (default 0.1 s) is only the threshold above which
  whole samples are inserted/dropped instead of stretched; leave it
  alone unless you are deliberately handling large timestamp jumps.
- During signal loss the guard substitutes or repeats frames to
  hold the output timeline, so an outage does not by itself shift
  sync; check the telemetry `signal_loss_events` count when a
  recording shows a sudden small jump.

## Verifying sync

Stream-level check:

```bash
ffprobe -v error -show_entries stream=codec_type,start_time,duration \
        -of default=noprint_wrappers=1 out.mkv
```

Packet-level check (first packets of each stream):

```bash
ffprobe -v error -show_entries packet=stream_index,pts_time \
        -of csv out.mkv | head
```

Numbers alone do not prove lip sync. Use a clip with an unambiguous
sync event (a clap, a slate) and check it at the start **and** near
the end of the longest recording you make; a check that only covers
the first seconds cannot detect drift.

## Troubleshooting

| Symptom | Likely cause | Action |
|---|---|---|
| Constant offset in a live recording | TBC latency not (or doubly) compensated | Set `adelay` to the table value for your preset; make sure `-itsoffset` is not also applied |
| Constant offset in a processed file | Start-time mismatch, not TBC latency | Compare `start_time` with ffprobe; trim with `-itsoffset` on the audio input — do not use the latency table |
| Offset grows over time | Capture-device clock drift | Measure the drift (ppm), then `aresample=async=<rate × ppm / 1e6>` on the audio |
| Occasional small jumps | Signal-loss episodes or capture drops | Check telemetry `signal_loss_events` / `degrade_events`; fix the source or cabling |
| Offset changed after switching presets | `buffer_frames` changed the latency | Recompute from the table/formula (live recordings only) |
