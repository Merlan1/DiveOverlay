# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

DiveOverlay overlays dive-computer CSV telemetry (depth, temperature, pressure, heart rate, elapsed dive time) onto video. It was originally a Python/OpenCV script and is now a Rust workspace that shells out to `ffmpeg`/`ffprobe` (no OpenCV/libav linking). All user-facing strings (CLI help, error messages, GUI labels) are in German — match that when touching existing strings.

## Commands

```bash
cargo build --release                 # binaries land in target/release/dive_overlay_cli(.exe) / dive_overlay_gui(.exe)
cargo test --workspace                # full suite
cargo test -p dive_overlay_core sync:: # run one module's tests, e.g. sync, csv_data, overlay, pipeline
cargo test some_test_name              # run a single test by name (any crate)
cargo run --release --bin dive_overlay_gui
cargo run --release --bin dive_overlay_cli -- --csv dive.csv --video input.mp4 --video-sync-sec 3.2 --csv-sync-mmss 0:10
```

Requires `ffmpeg`/`ffprobe` on PATH. Many tests in `dive_overlay_core` (pipeline, sync) synthesize clips at runtime via `ffmpeg -f lavfi` and a couple hit the real GitHub API (marked `#[ignore]`) — they need working `ffmpeg` to pass.

## Workspace layout

- `crates/dive_overlay_core` — all domain logic: CSV parsing, sample lookup, overlay drawing, ffprobe wrapper, ffmpeg pipeline, subtitle generation, multi-clip/auto-sync. CLI and GUI are both thin shells over this crate; new dive-processing logic belongs here, not duplicated in either frontend.
- `crates/dive_overlay_cli` — clap-based CLI binary.
- `crates/dive_overlay_gui` — egui/eframe GUI binary.

## Core module map (`dive_overlay_core/src`)

- `model.rs` — `DiveSample`, `ClipJob`, `Field` enum (`Time`/`Depth`/`Temp`/`Pressure`/`Hr`), and `value_for_field` (the German display strings, e.g. `"Tiefe: {:.1} m"`).
- `csv_data.rs` — flexible CSV loading: `find_column_index` does a two-phase match (exact match over candidates first, then substring) ported verbatim from the original Python's `find_column`, including iteration order — this is load-bearing for ambiguous headers, don't "clean it up". Also owns `mm:ss`/`hh:mm:ss` duration parsing/formatting and `--column-map` parsing.
- `lookup.rs` — `choose_sample_index`: finds the latest sample at-or-before a given dive-elapsed-second (last-known-value-carried-forward semantics), a `partition_point`-based port of Python's `bisect.bisect_right(...) - 1`.
- `overlay.rs` — `build_overlay_lines` (shared by CLI burned-in overlay, GUI preview, and SRT generation — keeps all three visually identical), pixel drawing (`draw_overlay`, `draw_depth_graph`) via `imageproc`/`ab_glyph`, using a bundled `DejaVuSans.ttf`.
- `subtitle.rs` — `build_srt`: renders one SRT cue per second, reusing `build_overlay_lines` so subtitle-mode text matches overlay-mode text exactly.
- `pipeline.rs` — the two processing paths:
  - `process_clip` (overlay mode): spawns an ffmpeg decoder (raw rgb24 to stdout) and encoder (raw rgb24 on stdin, muxes original audio via `-map 1:a:0?`, so audio-less inputs don't fail) as subprocesses connected through this process; per-frame overlay is drawn between decode and encode.
  - `process_clip_subtitles` (subtitle mode): no decode/encode loop — writes an SRT sidecar file and remuxes losslessly (`-c copy` video/audio, `mov_text` subtitle stream), since no pixels are touched.
  - `extract_frame_at`: two-tier seek (fast input-side `-ss`, falling back to frame-accurate output-side `-ss`) for the GUI's sync preview.
  - Encoder stdin must be dropped (not just left to `Drop`) before `wait()`, otherwise ffmpeg never sees EOF and hangs without finalizing the mp4.
- `ffprobe.rs` — `probe_video` (width/height/fps/estimated_frames/duration/creation_time via `ffprobe -show_streams -show_format`) and `ensure_ffmpeg_available` (fails fast with a clear message if `ffmpeg`/`ffprobe` aren't on PATH, instead of every downstream `Command::spawn` failing with an opaque ENOENT). `estimated_frames` is a rough estimate (from `nb_frames`, falling back to duration×fps) — fine for progress bars, never usable as a decode-loop termination condition.
- `sync.rs` — `parse_clip_spec` (`video|video_sync_sec|csv_sync_mmss[|output]`) and `compute_auto_sync`: derives each clip's `csv_sync_sec` from the delta between its MP4 `creation_time` and a manually-synced base clip. Every job gets the *same* `video_sync_sec` (copied from the base clip) — only `csv_sync_sec` varies per clip. This is intentional (assumes every clip's manual sync point sits at the same video second, e.g. "film the dive computer for the first few seconds of every clip"), not a bug.
- `error.rs` — `CoreError`/`CoreResult`; error message text is German and part of the CLI's user-facing contract — preserve wording when refactoring.

## Processing modes

Two mutually exclusive `OutputMode`s selected by `--mode`/GUI toggle:
- **Overlay** (default): burns telemetry into pixels via full decode→draw→encode; supports `--codec` and `--show-graph` (depth-profile mini-graph).
- **Subtitles**: writes a soft `mov_text` subtitle track (+ `.srt` sidecar) via lossless remux; no re-encode, no codec/graph options (subtitles can't render a graph).

## GUI architecture (`dive_overlay_gui/src/main.rs`)

Single `App` struct driving an egui immediate-mode UI. Long-running work (video processing, GitHub update check) runs on background `std::thread::spawn` threads that communicate back via `mpsc` channels (`WorkerEvent::{Log,Progress,Done}` for processing, a separate channel for `update_check::UpdateStatus`), with `ctx.request_repaint()` used to wake the UI thread. Cancellation goes through a shared `Arc<AtomicBool>` cancel flag threaded down into `dive_overlay_core::pipeline`.
