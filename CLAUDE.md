# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

DiveOverlay overlays dive-computer CSV telemetry (depth, temperature, pressure, heart rate, elapsed dive time) onto video. It was originally a Python/OpenCV script and is now a Rust workspace that shells out to `ffmpeg`/`ffprobe` (no OpenCV/libav linking). All user-facing strings (CLI help, error messages, GUI labels) are in English.

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

- `model.rs` — `DiveSample`, `ClipJob`, `Field` enum (`Time`/`Depth`/`Temp`/`Pressure`/`Hr`), and `value_for_field` (the overlay display strings, e.g. `"Depth: {:.1} m"`).
- `csv_data.rs` — flexible CSV loading: `find_column_index` does a two-phase match (exact match over candidates first, then substring) ported verbatim from the original Python's `find_column`, including iteration order — this is load-bearing for ambiguous headers, don't "clean it up". Also owns `mm:ss`/`hh:mm:ss` duration parsing/formatting and `--column-map` parsing. `read_csv_datetime_columns`/`read_first_row_columns` (and the `date=`/`clock=` map keys) are no longer used by auto-sync, which reads its deltas from the video files alone; they remain as public API.
- `lookup.rs` — `choose_sample_index`: finds the latest sample at-or-before a given dive-elapsed-second (last-known-value-carried-forward semantics), a `partition_point`-based port of Python's `bisect.bisect_right(...) - 1`.
- `overlay.rs` — `build_overlay_lines` (shared by CLI burned-in overlay, GUI preview, and SRT generation — keeps all three visually identical), pixel drawing (`draw_overlay`, `draw_depth_graph`) via `imageproc`/`ab_glyph`, using a bundled `DejaVuSans.ttf`. Every drawn size — both fonts, padding, label insets, stroke width — must come from `OverlayMetrics`/`GraphMetrics`, never a literal pixel count: they scale off `line_height_for(height)` so the overlay is the same fraction of the picture at any resolution. The sizes were hand-tuned at 1080p and the factor is 1.0 there by construction, so that rendering stays pixel-identical (pinned by `metrics_at_1080p_match_the_hand_tuned_originals`). Both fonts have a floor for small frames, where strict proportionality would go sub-readable. Adding a hard-coded size here is the bug that made 5.3K footage draw 22px glyphs inside a 134px line.
- `subtitle.rs` — `build_srt`: renders one SRT cue per second, reusing `build_overlay_lines` so subtitle-mode text matches overlay-mode text exactly. `concat_srt` is the inverse-ish partner used by merging: it re-parses rendered cues, shifts them by a per-part offset and renumbers them onto a joined timeline.
- `merge.rs` — combining one dive's clips into a single output. `plan_merge` sorts jobs by `dive_start_sec` (`csv_sync_sec - video_sync_sec`, valid in both sync modes) and redirects each job's `output_path` to a numbered scratch part; `finish_merge` concatenates, rebuilds the subtitle sidecar and deletes the parts. The concat demuxer call maps streams explicitly (`0:v:0`, `0:a:0?`, `0:s:0?`), never `-map 0`: real camera footage carries a `tmcd` timecode track that survives a `-c copy` remux, and mapping it fails the merge with "Cannot map stream #0:3 - unsupported type". `merge_clips` prefers the lossless concat demuxer (`-c copy`) and falls back to a re-encoding concat filter when clips differ in resolution/fps/audio — except in subtitle mode, which errors instead, since re-encoding would defeat that mode. Clips are joined back-to-back with no gap filler on purpose (the overlay's dive time visibly jumps at each seam). Parts are deliberately *not* deleted on failure or cancellation, so the expensive per-clip encodes survive. The concat runs entirely inside one ffmpeg call, so the only position information available comes from `-progress pipe:1` on its stdout (`spawn_progress_reader` parses `out_time_us`); `merge_clips`/`finish_merge` forward it as `(done_sec, total_sec)` to a caller callback, which is what feeds the CLI's `Combining` line and the GUI's bar during a multi-minute merge.
- `pipeline.rs` — the two processing paths:
  - `process_clip` (overlay mode): spawns an ffmpeg decoder (raw rgb24 to stdout) and encoder (raw rgb24 on stdin, muxes original audio via `-map 1:a:0?`, so audio-less inputs don't fail) as subprocesses connected through this process; per-frame overlay is drawn between decode and encode.
  - `process_clip_subtitles` (subtitle mode): no decode/encode loop — writes an SRT sidecar file and remuxes losslessly (`-c copy` video/audio, `mov_text` subtitle stream), since no pixels are touched.
  - `extract_frame_at`: two-tier seek (fast input-side `-ss`, falling back to frame-accurate output-side `-ss`) for the GUI's sync preview.
  - `probe_hw_encoder` must run at the *job's* frame size (it takes `width`/`height` and `resolve_encoder` passes them through). A hardware encoder's maximum resolution is a property of the silicon: the dev machine's `h264_amf` initializes at 4096x2304 and refuses 4608x2592 with `encoder->Init() failed with error 5`. Probing at a fixed small size reported "available" and then failed the whole job on 5.3K GoPro footage. Results are memoized per `(encoder, width, height)` so a multi-clip run probes once.
  - Encoder stdin must be dropped (not just left to `Drop`) before `wait()`, otherwise ffmpeg never sees EOF and hangs without finalizing the mp4.
  - Frames go to the encoder through `write_frame`, in 256 KiB chunks. A single `write_all` of a whole 5.3K rgb24 frame (~36 MB) into the stdin pipe fails partway through a full-length dive on Windows with `ERROR_NO_SYSTEM_RESOURCES` (os error 1450) — one write large enough to exhaust the non-paged pool. Chunk writes retry that error and `EINTR` with a backoff; every other write error is still a dead encoder.
- `ffprobe.rs` — `probe_video` (width/height/fps/estimated_frames/duration/creation_time/timecode via `ffprobe -show_streams -show_format`). `timecode_sec` is the `tmcd` start timecode as seconds since local midnight, parsed at the *nominal* frame rate (`fps.round()`, so 30 for 30000/1001): drop-frame only governs how a timecode advances *through* a clip, and each clip's start value is stamped fresh from the camera's clock, so no 1000/1001 correction applies. `00:00:00:00` reads as `None` — that is what an unset camera clock writes, and taking it as midnight would place the clip 12 hours from every real timecode. `width`/`height` are the *decoded* frame dimensions: they're swapped for ±90° `Display Matrix` rotation tags because ffmpeg auto-rotates on decode, and `fps` prefers `avg_frame_rate` over `r_frame_rate` (which overstates the rate for variable-frame-rate sources). The decoder in `pipeline.rs` runs `-fps_mode cfr -r <fps>` so `frame_idx / fps` is a valid timestamp; don't switch it back to `passthrough`. and `ensure_ffmpeg_available` (fails fast with a clear message if `ffmpeg`/`ffprobe` aren't on PATH, instead of every downstream `Command::spawn` failing with an opaque ENOENT). `estimated_frames` is a rough estimate (from `nb_frames`, falling back to duration×fps) — fine for progress bars, never usable as a decode-loop termination condition.
- `sync.rs` — `parse_clip_spec` (`video|video_sync_sec|csv_sync_mmss[|output]`) and `compute_auto_sync`: derives each clip's `csv_sync_sec` by offsetting the base clip's *own* hand-entered `csv_sync_sec` by how much later the clip started recording. Nothing wall-clock is read from the CSV — a delta between two clips of one camera needs no shared epoch, and requiring date/clock columns locked auto-sync out of elapsed-time-only exports. Deltas come from the `tmcd` start timecode when *every* clip has one and from `creation_time` otherwise; the choice is all-or-nothing because the two are independent clocks sitting ~45 s apart on the HERO11 test footage, so a per-clip fallback would silently inject that gap. Timecode is preferred because `creation_time` is quantized to whole seconds (verified: 253.167 s vs 253.000 s between the two test clips). `timecode_delta` handles the midnight wrap, since a timecode is a time of day with no date. Every job gets the *same* `video_sync_sec` (copied from the base clip) — only `csv_sync_sec` varies per clip. This is intentional (assumes every clip's manual sync point sits at the same video second, e.g. "film the dive computer for the first few seconds of every clip"), not a bug. `AutoSyncReport.warnings` flags clips landing outside the CSV's dive, which is how a clip from a *different* dive gets caught; it is computed before `csv_sync_sec` is clamped to 0, because the clamp would hide it.
- `error.rs` — `CoreError`/`CoreResult`; error message text is part of the CLI's user-facing contract — preserve wording when refactoring.

## Processing modes

Two mutually exclusive `OutputMode`s selected by `--mode`/GUI toggle:
- **Overlay** (default): burns telemetry into pixels via full decode→draw→encode; supports `--codec` and `--show-graph` (depth-profile mini-graph).
- **Subtitles**: writes a soft `mov_text` subtitle track (+ `.srt` sidecar) via lossless remux; no re-encode, no codec/graph options (subtitles can't render a graph).

## GUI architecture (`dive_overlay_gui/src/main.rs`)

Single `App` struct driving an egui immediate-mode UI. `run_worker` takes one `WorkerConfig` struct rather than positional arguments — keep new settings inside it.

The sync preview is deliberately two-staged: the scrub buttons move `video_sync_sec` and re-seek (`refresh_preview_frame`), the CSV sync text field only redraws the info box (`redraw_preview_overlay`). `PreviewState` therefore caches both the *un-overlaid* frame and the parsed CSV samples, so a keystroke costs neither an ffmpeg seek nor a CSV re-parse; `try_render_preview` is the only path that pays for both. The CSV sync field edits a buffer (`csv_sync_edit`) and writes back to the clip only once the value parses, so half-typed input like `1:` doesn't clobber it.

Long-running work (video processing, GitHub update check) runs on background `std::thread::spawn` threads that communicate back via `mpsc` channels (`WorkerEvent::{Log,Progress,Fps,Encoder,Done}` for processing, a separate channel for `update_check::UpdateStatus`), with `ctx.request_repaint()` used to wake the UI thread. Cancellation goes through a shared `Arc<AtomicBool>` cancel flag threaded down into `dive_overlay_core::pipeline`.
