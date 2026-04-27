# Tauchdaten-Overlay (Python)

Dieses Skript blendet die Werte aus einer Tauchgang-CSV als Overlay in ein Video ein.

Es unterstuetzt jetzt auch mehrere Videoclips mit Pausen dazwischen. Jeder Clip bekommt einen eigenen Sync-Punkt.

Datei: `overlay_dive_data.py`

## Voraussetzungen

- Python 3.10+ (oder neuer)
- Pakete:

```bash
pip install opencv-python
```

`tkinter` wird fuer die GUI verwendet (bei den meisten Python-Installationen bereits enthalten).

## Erwartetes CSV-Format

Das Skript erkennt die Spaltennamen flexibel. Mit deiner Beispiel-Datei `dive.csv` funktioniert es direkt, z. B. mit:

- `sample time (min)`
- `sample depth (m)`
- `sample temperature (C)`
- `sample pressure (bar)`
- `sample heartrate`

## Verwendung

### GUI starten

```bash
python overlay_dive_data_gui.py
```

In der GUI:

- CSV-Datei auswaehlen
- Felder setzen (z. B. `time,depth,temp`)
- Bei Bedarf Codec waehlen (`auto` empfohlen, sonst z. B. `avc1` oder `H264`)
- Clips einzeln hinzufuegen (Video, Video-Sync, CSV-Sync, Output)
- Mit `Sync Vorschau` den Frame an der Sync-Stelle inkl. Overlay kontrollieren
- In der Vorschau mit `-0.5s` / `+0.5s` den Sync feinjustieren
- Verarbeitung starten

### Einzelner Clip

```bash
python overlay_dive_data.py \
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
python overlay_dive_data.py \
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

## Synchronisation erklaert

- `--video-sync-sec`: Zeitpunkt im Video (in Sekunden), an dem du den Tauchcomputer als Referenz abfilmst.
- `--csv-sync-mmss`: Tauchzeit, die am Computer in diesem Moment angezeigt wird.

Beispiel:

- Bei `3.2` Sekunden Video siehst du auf dem Computer `0:10`.
- Dann nutze `--video-sync-sec 3.2 --csv-sync-mmss 0:10`.

## Optionale Parameter

- `--output out.mp4` : eigener Dateiname fuer Ausgabe
- `--fields time,depth,temp,pressure,hr` : welche Werte eingeblendet werden
- `--clip "video|video_sync|csv_sync[|out]"` : mehrfach nutzbar fuer Multi-Clip
- `--codec auto|avc1|H264|mp4v|XVID|MJPG` : bevorzugter Video-Codec

Zulaessige Felder:

- `time`
- `depth`
- `temp`
- `pressure`
- `hr`

Beispiel nur Zeit + Tiefe:

```bash
python overlay_dive_data.py --csv dive.csv --video input.mp4 --video-sync-sec 0 --csv-sync-mmss 0:00 --fields time,depth
```

## Hinweise

- Wenn zu Beginn des Videos noch keine CSV-Zeit erreicht ist, wird nur die Tauchzeit angezeigt.
- Fehlende CSV-Werte (z. B. Temperatur in einzelnen Zeilen) werden automatisch ausgelassen.
- Es wird immer der letzte bekannte Messwert verwendet (stabil fuer 10s-Logging).
- Wenn Farben in der Ausgabe komisch aussehen, zuerst `--codec avc1` oder `--codec H264` testen (GUI: Codec-Auswahl).
