# TODO

- [x] Translate the entire program to English: CLI help text/flag descriptions, error messages (`CoreError` variants and all `format!`/`bail!` strings), and the GUI's labels, buttons, dropdowns, and status/log messages (all currently German). Also translate `README.md` to English.

- [x] Verify and implement NVIDIA/AMD Hardware acceleration (NVENC confirmed on RTX 2070; AMF confirmed on an AMD iGPU — all of QSV/NVENC/AMF are in `ENABLED_HW_CANDIDATES`)

- [ ] Replace linear interpolation (`--interpolate` / GUI checkbox) with a Fourier-transform-based reconstruction for smoother inter-sample estimates. Note: dive-computer samples are sparse and irregularly spaced, so a spline/cubic fit may suit this data better than FFT-based reconstruction — worth evaluating both.
- [x] Add an output-resolution toggle (`--resolution` / GUI dropdown): original (default), 4k, 1080p, 720p. Downscales only, preserves aspect ratio, applied in the decoder so the pipe, the drawing and the encode all shrink together.

- [ ] **Known bottleneck: the rgb24 round-trip between the decode and encode
  subprocesses.** Every frame crosses two pipes uncompressed — 47.6 MB at
  5312x2988, 28.3 MB at 4K — and `process_clip` serializes read → draw →
  write on one thread. Measured on `test_clip0.MP4` (210 frames, 12 cores):

  | Stage | 5.3K, libx264 veryfast | 4K, h264_amf |
  |---|---|---|
  | encode | 63% | small (21.9 fps in-process) |
  | rgb24 convert + the two pipes | 19% | **~45%, the largest single cost** |
  | HEVC decode | 13% | (decode+scale ceiling: 27.4 fps) |
  | overlay drawing | 6% | 6% |

  Whole-program: 4.16 fps before, 12.70 fps at `--resolution 4k --hw-accel`,
  21.80 at 1080p, 25.63 at 720p. Once the encoder is hardware and the frame
  is 4K, the pipe is what's left. Two candidate fixes, neither started:
  - Move the draw to a worker thread with a small frame queue, so decode and
    draw overlap the encode instead of serializing (~19% ceiling).
  - Stop paying for rgb24. yuv420p would halve the bytes on both pipes, but
    `imageproc`/`ab_glyph` draw into packed RGB, so this needs either a
    planar-YUV text blitter or drawing only inside the overlay's bounding box.

  Do not chase the drawing code: it is 6% of runtime, and `--show-graph`
  costs 3%. Benchmark on `test_clip0.MP4`, never on `lavfi` sources — x264's
  speed collapses on real content while fixed-function encoders barely move,
  so synthetic clips rank the encoders backwards.
