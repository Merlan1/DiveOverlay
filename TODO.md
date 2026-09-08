# TODO

- [x] Translate the entire program to English: CLI help text/flag descriptions, error messages (`CoreError` variants and all `format!`/`bail!` strings), and the GUI's labels, buttons, dropdowns, and status/log messages (all currently German). Also translate `README.md` to English.

- [x] Verify and implement NVIDIA/AMD Hardware acceleration (NVENC confirmed on RTX 2070; AMF confirmed on an AMD iGPU — all of QSV/NVENC/AMF are in `ENABLED_HW_CANDIDATES`)

- [ ] Replace linear interpolation (`--interpolate` / GUI checkbox) with a Fourier-transform-based reconstruction for smoother inter-sample estimates. Note: dive-computer samples are sparse and irregularly spaced, so a spline/cubic fit may suit this data better than FFT-based reconstruction — worth evaluating both.
- [x] Add an output-resolution toggle (`--resolution` / GUI dropdown): original (default), 4k, 1080p, 720p. Downscales only, preserves aspect ratio, applied in the decoder so the pipe, the drawing and the encode all shrink together.

- [x] Replace the rgb24 pipe with planar YUV 4:2:0. Frames now cross both
  pipes in the format the codecs already speak, halving the bytes (47.6 MB ->
  23.8 MB per frame at 5.3K, 24.9 MB -> 12.4 MB at 4K) and removing both
  swscale passes and the lossy `yuv420p -> rgb24 -> yuv420p` chroma
  round-trip. Every overlay element is rendered to an RGBA tile, so only
  `composite_tile`/`composite_tile_yuv` know about pixel layout; the depth
  curve became light grey because a saturated thin line subsamples badly at
  half-resolution chroma.

  Measured on `test_clip0.MP4` (210 frames), before -> after:

  | Configuration | before | after |
  |---|---|---|
  | original + veryfast | 4.16 fps | 4.52 fps |
  | `--resolution 4k --hw-accel` | 12.70 fps | **14.60 fps** |
  | `--resolution 1080p --hw-accel` | 21.80 fps | 21.87 fps |

  +15% where it mattered (4K with a hardware encoder). The 1080p row is flat
  because the bottleneck there has moved to the HEVC decode.

- [ ] **Remaining bottleneck: the frame loop is single-threaded.** `process_clip`
  serializes read -> draw -> write on one thread while 12 cores sit partly
  idle. Moving the overlay draw to a worker with a small frame queue would let
  decode and draw overlap the encode. Ceiling is modest -- drawing is ~6% of
  runtime and decode ~13% -- so this is worth doing only after measuring
  again, and only for the hardware-encoder configurations where the encode no
  longer dominates.

  Do not chase the drawing code on its own: it is ~6% of runtime, and
  `--show-graph` costs ~1%. Benchmark on `test_clip0.MP4`, never on `lavfi`
  sources -- x264's speed collapses on real content while fixed-function
  encoders barely move, so synthetic clips rank the encoders backwards.
