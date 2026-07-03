# Dive Data Overlay (Rust)

This tool overlays the values from a dive-log CSV onto a video: depth, temperature, pressure, heart rate, and dive time. It supports multiple video clips with gaps in between, each clip with its own sync point, as well as automatic sync via the MP4 files' recording time.

Two output modes are available:

- **Overlay** (default): the values are burned directly into the video pixels.
- **Subtitles**: the values are instead written as a soft subtitle track (SRT/`mov_text`) that can be toggled on and off in the player afterward. Video/audio are copied losslessly (no re-encode), and an additional `.srt` sidecar file is created next to the output.

Formerly a Python/OpenCV script, now a Rust workspace that uses `ffmpeg`/`ffprobe` as a subprocess for decoding/encoding (no OpenCV/libav linking needed).

## Screenshots

Overlay example from a rendered clip:

![Overlay example](screenshots/Preview.png)

## Workspace layout

- `crates/dive_overlay_core` — library: CSV parsing, sample lookup, overlay drawing, ffprobe wrapper, ffmpeg pipeline, multi-clip/auto-sync
- `crates/dive_overlay_cli` — CLI binary (clap)
- `crates/dive_overlay_gui` — GUI binary (egui/eframe)

## Requirements

- Rust (stable, 2021 edition) via [rustup](https://rustup.rs/)
- `ffmpeg` and `ffprobe` on PATH (e.g. `winget install Gyan.FFmpeg` on Windows, or your distribution's package)

## Building

```bash
cargo build --release
```

Binaries end up in `target/release/dive_overlay_cli(.exe)` and `target/release/dive_overlay_gui(.exe)`.

## Testing

```bash
cargo test --workspace
```

The test suite covers CSV parsing, sample lookup, overlay drawing, subtitle generation, ffprobe parsing, and the full ffmpeg pipeline (decode/overlay/encode+audio mux, subtitle remux, cancellation, multi-clip auto-sync). Some tests synthesize test clips via `ffmpeg -f lavfi` and therefore need a working `ffmpeg` on PATH. A hardware-acceleration test only runs if this machine actually has a working hardware encoder (e.g. Intel Quick Sync) — otherwise it's skipped rather than failing.

## Expected CSV format

The tool recognizes column names flexibly. It works directly with the sample file `dive.csv`, e.g. with:

- `sample time (min)`
- `sample depth (m)`
- `sample temperature (C)`
- `sample pressure (bar)`
- `sample heartrate`

## Usage

### Starting the GUI

```bash
cargo run --release --bin dive_overlay_gui
```

In the GUI:

- Select a CSV file
- Set fields (e.g. `time,depth,temp`)
- Choose mode: `Overlay (burned-in)` or `Subtitles (toggle on/off)`, with a depth-profile toggle next to it (overlay mode only)
- In overlay mode, choose a codec if needed (`auto` recommended, otherwise e.g. `avc1`, `H264`, or `hevc`), a preset (speed vs. compression), and optionally hardware acceleration
- Add clips individually (video, video sync, CSV sync, output)
- Use `Sync preview` to check the frame at the sync point including the overlay
- Fine-tune the sync in the preview with `-0.5s` / `+0.5s` (up to `-1 min` / `+1 min`)
- Start processing
- Progress is shown as a percentage bar (including fps) during processing, with the actually-used encoder (software or hardware) shown below it; cancel at any time

### Single clip

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --video input.mp4 \
  --video-sync-sec 3.2 \
  --csv-sync-mmss 0:10
```

This produces `input_overlay.mp4` by default.

For the subtitle variant instead of a burned-in overlay, just append `--mode subtitles`:

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --video input.mp4 \
  --video-sync-sec 3.2 \
  --csv-sync-mmss 0:10 \
  --mode subtitles
```

For faster encoding (overlay mode) via hardware acceleration, with a fallback to software if no matching hardware is found:

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --video input.mp4 \
  --video-sync-sec 3.2 \
  --csv-sync-mmss 0:10 \
  --codec hevc \
  --hw-accel
```

### Multiple clips (with gaps)

You specify a sync point per clip, so long gaps between clips stay correct.

Format per `--clip`:

`video_path|video_sync_sec|csv_sync_mmss[|output_path]`

Example:

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --fields time,depth,temp \
  --clip "clip1.mp4|2.1|0:10|clip1_overlay.mp4" \
  --clip "clip2.mp4|0.8|18:35|clip2_overlay.mp4" \
  --clip "clip3.mp4|5.0|31:20"
```

Note:

- For each clip, `video_sync_sec` is the point in that specific video.
- `csv_sync_mmss` is the dive time displayed at that exact moment.
- If `output_path` is missing, `<video_stem>_overlay.mp4` is used.

### Automatic sync (auto-sync)

Instead of manually syncing every clip, the recording time (MP4 `creation_time`, read via `ffprobe`) can be used: one base clip is synced manually, and all other clips are shifted automatically based on the difference in their recording time.

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --clip "clip1.mp4|0|0:00" \
  --clip "clip2.mp4|0|0:00" \
  --auto-sync \
  --base-clip clip1.mp4 \
  --base-video-sync-sec 0 \
  --base-csv-datetime "2025-07-05 10:00:00"
```

Important: `video_sync_sec` stays the same for all clips (copied from the base clip) — only `csv_sync_sec` is shifted per clip based on the recording-time difference. This assumes that each clip's manual sync point sits at the same video second (e.g. "point the camera at the dive computer for the first few seconds of every clip").

The CSV needs a date column and a time column for this.

## Sync explained

- `--video-sync-sec`: the point in the video (in seconds) where you film the dive computer as a reference.
- `--csv-sync-mmss`: the dive time shown on the computer at that exact moment.

Example:

- At `3.2` seconds into the video, you see `0:10` on the computer.
- Then use `--video-sync-sec 3.2 --csv-sync-mmss 0:10`.

## Optional parameters

- `--output out.mp4` : custom output filename
- `--fields time,depth,temp,pressure,hr` : which values are displayed
- `--column-map time=TIME,depth=Depth` : manual column mapping, in case auto-detection gets it wrong
- `--clip "video|video_sync|csv_sync[|out]"` : usable multiple times for multi-clip
- `--codec auto|avc1|H264|hevc|H265|mp4v|XVID|MJPG` : video codec (mapped to the matching ffmpeg encoder, `auto`/`H264`/`avc1` -> `libx264`, `hevc`/`H265` -> `libx265`), only applies in overlay mode
- `--preset ultrafast|superfast|veryfast|faster|fast|medium|slow|slower|veryslow|placebo` : encoder preset for H264/H265 (faster = larger file at the same quality), default `veryfast`; ignored for other codecs
- `--hw-accel` : tries to use hardware acceleration (currently Intel Quick Sync) for H264/H265 and falls back to software automatically if no matching hardware is found; the CLI prints which encoder was actually used
- `--show-graph` : shows a small depth profile in the video, only applies in overlay mode
- `--mode overlay|subtitles` : `overlay` (default) burns the values into the pixels; `subtitles` instead writes them as a soft subtitle track that can be toggled on/off in the player (video/audio are copied losslessly via `-c copy`, and a `.srt` file is additionally created next to the output). The depth profile (`--show-graph`) isn't available in this mode, since subtitles can only display text.
- `--auto-sync`, `--base-clip`, `--base-video-sync-sec`, `--base-csv-datetime` : automatic sync (see above)

Allowed fields:

- `time`
- `depth`
- `temp`
- `pressure`
- `hr`

Example with just time + depth:

```bash
cargo run --release --bin dive_overlay_cli -- --csv dive.csv --video input.mp4 --video-sync-sec 0 --csv-sync-mmss 0:00 --fields time,depth
```

## Notes

- If no CSV time has been reached yet at the start of the video, only the dive time is shown.
- Missing CSV values (e.g. temperature in individual rows) are automatically skipped.
- The last known measurement is always used (stable for 10s logging).
- The video's original audio track is preserved in the result (AAC, 192 kbit/s), if present.
- In subtitle mode, whether the embedded track can be toggled on/off depends on the player/container — the additionally written `.srt` file can also be loaded separately if needed.
