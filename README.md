# Tauchdaten-Overlay (Rust)

Dieses Tool blendet die Werte aus einer Tauchgang-CSV als Overlay in ein Video ein: Tiefe, Temperatur, Druck, Puls und Tauchzeit. Es unterstuetzt mehrere Videoclips mit Pausen dazwischen, jeder Clip mit eigenem Sync-Punkt, sowie automatisches Sync ueber die Aufnahmezeit der MP4-Dateien.

Ehemals ein Python/OpenCV-Skript, jetzt ein Rust-Workspace, der fuer Dekodierung/Encodierung `ffmpeg`/`ffprobe` als Subprozess nutzt (kein OpenCV/libav-Linking noetig).

## Screenshots

Overlay-Beispiel aus einem gerenderten Clip:

![Overlay Beispiel](screenshots/Preview.png)

## Workspace-Layout

- `crates/dive_overlay_core` — Bibliothek: CSV-Parsing, Sample-Lookup, Overlay-Zeichnen, ffprobe-Wrapper, ffmpeg-Pipeline, Multi-Clip/Auto-Sync
- `crates/dive_overlay_cli` — CLI-Binary (clap)
- `crates/dive_overlay_gui` — GUI-Binary (egui/eframe)

## Voraussetzungen

- Rust (stable, 2021 edition) via [rustup](https://rustup.rs/)
- `ffmpeg` und `ffprobe` im PATH (z. B. `winget install Gyan.FFmpeg` unter Windows, oder das Paket der jeweiligen Distribution)

## Bauen

```bash
cargo build --release
```

Binaries landen in `target/release/dive_overlay_cli(.exe)` und `target/release/dive_overlay_gui(.exe)`.

## Testen

```bash
cargo test --workspace
```

Die Test-Suite (35 Tests) deckt CSV-Parsing, Sample-Lookup, Overlay-Zeichnen, ffprobe-Parsing sowie die volle ffmpeg-Pipeline (Dekodieren/Overlay/Encodieren+Audio-Mux, Abbruch, Multi-Clip-Auto-Sync) ab. Ein Teil der Tests synthetisiert Testclips per `ffmpeg -f lavfi` und benoetigt daher ein funktionierendes `ffmpeg` im PATH.

## Erwartetes CSV-Format

Das Tool erkennt die Spaltennamen flexibel. Mit der Beispiel-Datei `dive.csv` funktioniert es direkt, z. B. mit:

- `sample time (min)`
- `sample depth (m)`
- `sample temperature (C)`
- `sample pressure (bar)`
- `sample heartrate`

## Verwendung

### GUI starten

```bash
cargo run --release --bin dive_overlay_gui
```

In der GUI:

- CSV-Datei auswaehlen
- Felder setzen (z. B. `time,depth,temp`)
- Bei Bedarf Codec waehlen (`auto` empfohlen, sonst z. B. `avc1` oder `H264`)
- Clips einzeln hinzufuegen (Video, Video-Sync, CSV-Sync, Output)
- Mit `Sync Vorschau` den Frame an der Sync-Stelle inkl. Overlay kontrollieren
- In der Vorschau mit `-0.5s` / `+0.5s` (bis `-1 min` / `+1 min`) den Sync feinjustieren
- Verarbeitung starten
- Fortschritt wird als Prozentbalken waehrend der Verarbeitung angezeigt, Abbruch jederzeit moeglich

### Einzelner Clip

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --video input.mp4 \
  --video-sync-sec 3.2 \
  --csv-sync-mmss 0:10
```

Danach entsteht standardmaessig: `input_overlay.mp4`

### Mehrere Clips (mit Pausen)

Du gibst pro Clip einen eigenen Sync an. So bleiben auch lange Pausen zwischen Clips korrekt.

Format pro `--clip`:

`video_path|video_sync_sec|csv_sync_mmss[|output_path]`

Beispiel:

```bash
cargo run --release --bin dive_overlay_cli -- \
  --csv dive.csv \
  --fields time,depth,temp \
  --clip "clip1.mp4|2.1|0:10|clip1_overlay.mp4" \
  --clip "clip2.mp4|0.8|18:35|clip2_overlay.mp4" \
  --clip "clip3.mp4|5.0|31:20"
```

Hinweis:

- Bei jedem Clip ist `video_sync_sec` die Stelle im jeweiligen Video.
- `csv_sync_mmss` ist die angezeigte Tauchzeit in genau diesem Moment.
- Wenn `output_path` fehlt, wird `<video_stem>_overlay.mp4` verwendet.

### Automatisches Sync (Auto-Sync)

Statt jeden Clip manuell zu syncen, kann die Aufnahmezeit (MP4 `creation_time`, per `ffprobe` ausgelesen) genutzt werden: ein Basis-Clip wird manuell gesynct, alle anderen Clips werden anhand der Differenz ihrer Aufnahmezeit automatisch versetzt.

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

Wichtig: `video_sync_sec` bleibt dabei fuer alle Clips gleich (kopiert vom Basis-Clip) — nur `csv_sync_sec` wird pro Clip anhand der Aufnahmezeit-Differenz verschoben. Das setzt voraus, dass bei jedem Clip der manuelle Sync-Punkt an derselben Video-Sekunde liegt (z. B. "die ersten Sekunden jedes Clips auf den Tauchcomputer halten").

Die CSV braucht dafuer eine Datums- und eine Uhrzeit-Spalte.

## Synchronisation erklaert

- `--video-sync-sec`: Zeitpunkt im Video (in Sekunden), an dem du den Tauchcomputer als Referenz abfilmst.
- `--csv-sync-mmss`: Tauchzeit, die am Computer in diesem Moment angezeigt wird.

Beispiel:

- Bei `3.2` Sekunden Video siehst du auf dem Computer `0:10`.
- Dann nutze `--video-sync-sec 3.2 --csv-sync-mmss 0:10`.

## Optionale Parameter

- `--output out.mp4` : eigener Dateiname fuer Ausgabe
- `--fields time,depth,temp,pressure,hr` : welche Werte eingeblendet werden
- `--column-map time=TIME,depth=Depth` : manuelle Spaltenzuordnung, falls die Auto-Erkennung daneben liegt
- `--clip "video|video_sync|csv_sync[|out]"` : mehrfach nutzbar fuer Multi-Clip
- `--codec auto|avc1|H264|mp4v|XVID|MJPG` : Video-Codec (wird auf den passenden ffmpeg-Encoder abgebildet, `auto`/`H264`/`avc1` -> `libx264`)
- `--show-graph` : zeigt ein kleines Tiefenprofil im Video
- `--auto-sync`, `--base-clip`, `--base-video-sync-sec`, `--base-csv-datetime` : automatisches Sync (siehe oben)

Zulaessige Felder:

- `time`
- `depth`
- `temp`
- `pressure`
- `hr`

Beispiel nur Zeit + Tiefe:

```bash
cargo run --release --bin dive_overlay_cli -- --csv dive.csv --video input.mp4 --video-sync-sec 0 --csv-sync-mmss 0:00 --fields time,depth
```

## Hinweise

- Wenn zu Beginn des Videos noch keine CSV-Zeit erreicht ist, wird nur die Tauchzeit angezeigt.
- Fehlende CSV-Werte (z. B. Temperatur in einzelnen Zeilen) werden automatisch ausgelassen.
- Es wird immer der letzte bekannte Messwert verwendet (stabil fuer 10s-Logging).
- Die Original-Tonspur des Videos bleibt im Ergebnis erhalten (AAC, 192 kbit/s), sofern vorhanden.
