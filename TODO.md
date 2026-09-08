# TODO

- [x] Translate the entire program to English: CLI help text/flag descriptions, error messages (`CoreError` variants and all `format!`/`bail!` strings), and the GUI's labels, buttons, dropdowns, and status/log messages (all currently German). Also translate `README.md` to English.

- [x] Verify and implement NVIDIA/AMD Hardware acceleration (NVENC confirmed on RTX 2070; AMF confirmed on an AMD iGPU — all of QSV/NVENC/AMF are in `ENABLED_HW_CANDIDATES`)

- [ ] Replace linear interpolation (`--interpolate` / GUI checkbox) with a Fourier-transform-based reconstruction for smoother inter-sample estimates. Note: dive-computer samples are sparse and irregularly spaced, so a spline/cubic fit may suit this data better than FFT-based reconstruction — worth evaluating both.