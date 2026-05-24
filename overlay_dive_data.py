#!/usr/bin/env python3
"""Overlay dive CSV data onto a video.

Example:
    python overlay_dive_data.py \
        --csv dive.csv \
        --video input.mp4 \
        --video-sync-sec 3.2 \
        --csv-sync-mmss 0:10
"""

from __future__ import annotations

import argparse
import bisect
import csv
import math
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Dict, List, Optional, Tuple

import cv2
import numpy as np


@dataclass
class DiveSample:
    elapsed_sec: float
    depth_m: Optional[float]
    temp_c: Optional[float]
    pressure_bar: Optional[float]
    heart_rate: Optional[float]


@dataclass
class ClipJob:
    video_path: Path
    output_path: Path
    video_sync_sec: float
    csv_sync_sec: float
    video_start_utc: Optional[datetime] = None


def parse_duration_to_seconds(value: str) -> float:
    value = (value or "").strip()
    if not value:
        raise ValueError("Leerer Zeitwert")

    if ":" not in value:
        return float(value)

    parts = value.split(":")
    if len(parts) == 2:
        minutes, seconds = parts
        return int(minutes) * 60 + float(seconds)
    if len(parts) == 3:
        hours, minutes, seconds = parts
        return int(hours) * 3600 + int(minutes) * 60 + float(seconds)

    raise ValueError(f"Unbekanntes Zeitformat: {value}")


def format_duration(seconds: float) -> str:
    total = max(0, int(round(seconds)))
    hours = total // 3600
    minutes = (total % 3600) // 60
    sec = total % 60
    if hours > 0:
        return f"{hours:02d}:{minutes:02d}:{sec:02d}"
    return f"{minutes:02d}:{sec:02d}"


def parse_optional_float(value: Optional[str]) -> Optional[float]:
    if value is None:
        return None
    text = value.strip().strip('"')
    if not text:
        return None
    text = text.replace(",", ".")
    try:
        return float(text)
    except ValueError:
        return None


def find_column(headers: List[str], candidates: List[str]) -> Optional[str]:
    lower_headers = {h.lower().strip(): h for h in headers}

    for candidate in candidates:
        if candidate in lower_headers:
            return lower_headers[candidate]

    for lower, original in lower_headers.items():
        for candidate in candidates:
            if candidate in lower:
                return original

    return None


def read_csv_headers(csv_path: Path) -> List[str]:
    with csv_path.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        if reader.fieldnames is None:
            raise ValueError("CSV hat keine Kopfzeile")
        return [h.strip().strip('"') for h in reader.fieldnames]


def read_csv_datetime_columns(csv_path: Path) -> Tuple[Optional[str], Optional[str]]:
    headers = read_csv_headers(csv_path)
    lower_headers = {h.lower().strip(): h for h in headers}

    def find_candidate(candidates: List[str]) -> Optional[str]:
        for c in candidates:
            if c in lower_headers:
                return lower_headers[c]
        for lower, original in lower_headers.items():
            for c in candidates:
                if c in lower:
                    return original
        return None

    date_col = find_candidate(["date", "datum", "sample date"])
    time_col = find_candidate(["time", "zeit", "sample time", "clock time"])
    return date_col, time_col


def parse_datetime_utc(date_str: str, time_str: str) -> datetime:
    dt = datetime.fromisoformat(f"{date_str.strip()} {time_str.strip()}")
    return dt.replace(tzinfo=timezone.utc)


def parse_datetime_text(value: str) -> datetime:
    dt = datetime.fromisoformat(value.strip())
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def get_video_creation_time_utc(video_path: Path) -> datetime:
    cmd = [
        "ffprobe",
        "-v",
        "error",
        "-show_entries",
        "format_tags=creation_time",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        str(video_path),
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    text = (result.stdout or "").strip()
    if not text:
        raise RuntimeError(f"Keine creation_time in MP4: {video_path}")
    raw = text.replace("Z", "+00:00")
    try:
        dt = datetime.fromisoformat(raw)
    except ValueError as exc:
        raise ValueError(f"Unbekanntes creation_time Format: {text}") from exc
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def load_samples(csv_path: Path, column_map: Optional[Dict[str, str]] = None) -> List[DiveSample]:

    with csv_path.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        if reader.fieldnames is None:
            raise ValueError("CSV hat keine Kopfzeile")

        headers = [h.strip().strip('"') for h in reader.fieldnames]
        normalized_to_original = dict(zip(headers, reader.fieldnames))

        def resolve(candidates: List[str]) -> Optional[str]:
            key = find_column(headers, candidates)
            if key is None:
                return None
            return normalized_to_original[key]

        def resolve_from_map(key: str) -> Optional[str]:
            if not column_map:
                return None
            mapped = (column_map.get(key) or "").strip()
            if not mapped:
                return None
            lower_headers = {h.lower().strip(): h for h in headers}
            mapped_key = mapped.lower().strip()
            if mapped_key in lower_headers:
                return normalized_to_original[lower_headers[mapped_key]]
            raise ValueError(f"Spalte '{mapped}' nicht gefunden")

        time_col = resolve_from_map("time") or resolve(["sample time (min)", "sample time", "time"])
        depth_col = resolve_from_map("depth") or resolve(["sample depth (m)", "sample depth", "depth"])
        temp_col = resolve_from_map("temp") or resolve(["sample temperature (c)", "sample temperature", "temperature"])
        pressure_col = resolve_from_map("pressure") or resolve(["sample pressure (bar)", "sample pressure", "pressure"])
        hr_col = resolve_from_map("hr") or resolve(["sample heartrate", "sample heart rate", "heartrate", "heart rate"])

        date_col = resolve_from_map("date") or resolve(["date", "datum", "sample date"])
        clock_col = resolve_from_map("clock") or resolve(["time", "zeit", "clock time", "sample time"])

        if time_col is None:
            raise ValueError("Keine Zeitspalte gefunden (z. B. 'sample time (min)')")
        if depth_col is None:
            raise ValueError("Keine Tiefenspalte gefunden (z. B. 'sample depth (m)')")

        samples: List[DiveSample] = []
        for row in reader:
            time_raw = (row.get(time_col) or "").strip().strip('"')
            if not time_raw:
                continue

            elapsed = parse_duration_to_seconds(time_raw)
            sample = DiveSample(
                elapsed_sec=elapsed,
                depth_m=parse_optional_float(row.get(depth_col)),
                temp_c=parse_optional_float(row.get(temp_col)) if temp_col else None,
                pressure_bar=parse_optional_float(row.get(pressure_col)) if pressure_col else None,
                heart_rate=parse_optional_float(row.get(hr_col)) if hr_col else None,
            )
            samples.append(sample)

    samples.sort(key=lambda s: s.elapsed_sec)
    if not samples:
        raise ValueError("CSV enthält keine verwertbaren Samples")
    return samples


def choose_sample_index(times: List[float], dive_sec: float) -> Optional[int]:
    idx = bisect.bisect_right(times, dive_sec) - 1
    if idx < 0:
        return None
    return idx


def value_for_field(sample: DiveSample, field: str) -> Optional[str]:
    if field == "depth":
        if sample.depth_m is None:
            return None
        return f"Tiefe: {sample.depth_m:.1f} m"
    if field == "temp":
        if sample.temp_c is None:
            return None
        return f"Temp: {sample.temp_c:.1f} C"
    if field == "pressure":
        if sample.pressure_bar is None:
            return None
        return f"Druck: {sample.pressure_bar:.0f} bar"
    if field == "hr":
        if sample.heart_rate is None:
            return None
        return f"Puls: {sample.heart_rate:.0f} bpm"
    raise ValueError(f"Unbekanntes Feld: {field}")


def draw_overlay(frame, lines: List[str]) -> None:
    h, w = frame.shape[:2]
    x = int(w * 0.04)
    y = int(h * 0.06)
    line_height = max(24, int(h * 0.045))
    padding = 14

    text_sizes = [cv2.getTextSize(line, cv2.FONT_HERSHEY_SIMPLEX, 0.7, 2)[0] for line in lines]
    box_w = max((tw for tw, _ in text_sizes), default=0) + padding * 2
    box_h = line_height * len(lines) + padding

    x2 = min(w - 1, x + box_w)
    y2 = min(h - 1, y + box_h)

    overlay = frame.copy()
    cv2.rectangle(overlay, (x, y), (x2, y2), (20, 20, 20), -1)
    cv2.addWeighted(overlay, 0.45, frame, 0.55, 0, frame)

    text_y = y + line_height
    for line in lines:
        cv2.putText(
            frame,
            line,
            (x + padding, text_y),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.7,
            (230, 245, 255),
            2,
            cv2.LINE_AA,
        )
        text_y += line_height


def draw_depth_graph(
    frame,
    samples: List[DiveSample],
    times: List[float],
    dive_sec: float,
    window_sec: float = 600.0,
) -> None:
    if not samples:
        return

    h, w = frame.shape[:2]
    graph_w = int(w * 0.32)
    graph_h = int(h * 0.18)
    x = int(w * 0.04)
    y = int(h * 0.72)

    x2 = min(w - 1, x + graph_w)
    y2 = min(h - 1, y + graph_h)

    start_sec = max(0.0, dive_sec - window_sec)
    end_sec = max(start_sec + 1.0, dive_sec)

    start_idx = bisect.bisect_left(times, start_sec)
    end_idx = bisect.bisect_right(times, end_sec)
    window_samples = samples[start_idx:end_idx]
    if not window_samples:
        return

    depths = [s.depth_m for s in window_samples if s.depth_m is not None]
    if not depths:
        return

    max_depth = max(depths)
    min_depth = min(depths)
    if math.isclose(max_depth, min_depth):
        max_depth = min_depth + 1.0

    overlay = frame.copy()
    cv2.rectangle(overlay, (x, y), (x2, y2), (10, 10, 10), -1)
    cv2.addWeighted(overlay, 0.35, frame, 0.65, 0, frame)
    cv2.rectangle(frame, (x, y), (x2, y2), (90, 90, 90), 1)

    points: List[Tuple[int, int]] = []
    for sample in window_samples:
        if sample.depth_m is None:
            continue
        t = sample.elapsed_sec
        if t < start_sec or t > end_sec:
            continue
        tx = (t - start_sec) / (end_sec - start_sec)
        # Tiefer = weiter unten (invertierte Y-Achse)
        ty = (sample.depth_m - min_depth) / (max_depth - min_depth)
        px = x + int(tx * (graph_w - 2)) + 1
        py = y + int(ty * (graph_h - 2)) + 1
        points.append((px, py))

    if len(points) >= 2:
        cv2.polylines(frame, [np.array(points, dtype="int32")], False, (100, 220, 255), 2)

    label = f"Depth {min_depth:.1f}-{max_depth:.1f}m"
    cv2.putText(
        frame,
        label,
        (x + 6, y - 6),
        cv2.FONT_HERSHEY_SIMPLEX,
        0.5,
        (200, 200, 200),
        1,
        cv2.LINE_AA,
    )


def parse_fields(value: str) -> List[str]:
    allowed = {"time", "depth", "temp", "pressure", "hr"}
    fields = [v.strip().lower() for v in value.split(",") if v.strip()]
    if not fields:
        raise ValueError("--fields darf nicht leer sein")

    unknown = [f for f in fields if f not in allowed]
    if unknown:
        raise ValueError(
            "Unbekannte Felder in --fields: " + ", ".join(unknown) + ". Erlaubt: " + ", ".join(sorted(allowed))
        )
    return fields


def parse_column_map(value: Optional[str]) -> Dict[str, str]:
    if not value:
        return {}
    mapping: Dict[str, str] = {}
    parts = [p.strip() for p in value.split(",") if p.strip()]
    for part in parts:
        if "=" not in part:
            raise ValueError("--column-map Format: key=Spaltenname, z. B. time=TIME,depth=Depth")
        key, col = [p.strip() for p in part.split("=", 1)]
        if key not in {"time", "depth", "temp", "pressure", "hr", "date", "clock"}:
            raise ValueError(f"Unbekannter column-map Key: {key}")
        if not col:
            raise ValueError("Spaltenname in --column-map darf nicht leer sein")
        mapping[key] = col
    return mapping


def open_video_writer(output_path: Path, fps: float, width: int, height: int, codec: str):
    output_path = output_path.with_suffix(".mp4")

    codec = (codec or "auto").strip()

    fourcc_map = {
        "avc1": cv2.VideoWriter_fourcc(*"avc1"),
        "H264": cv2.VideoWriter_fourcc(*"H264"),
        "h264": cv2.VideoWriter_fourcc(*"H264"),
        "mp4v": cv2.VideoWriter_fourcc(*"mp4v"),
        "XVID": cv2.VideoWriter_fourcc(*"XVID"),
        "MJPG": cv2.VideoWriter_fourcc(*"MJPG"),
        "mjpg": cv2.VideoWriter_fourcc(*"MJPG"),
    }

    if codec.lower() == "auto":
        order = ["avc1", "H264", "mp4v"]
    else:
        order = [codec]

    attempted: List[str] = []
    for cc in order:
        fourcc = fourcc_map.get(cc)
        if fourcc is None:
            continue
        attempted.append(cc)
        writer = cv2.VideoWriter(str(output_path), fourcc, fps, (width, height))
        if writer.isOpened():
            return writer, cc
        writer.release()

    raise RuntimeError(
        f"Konnte Ausgabedatei nicht öffnen: {output_path}. "
        f"Getestete Codecs: {', '.join(attempted)}"
    )


def derive_output_path(video_path: Path, output: Optional[Path]) -> Path:
    if output is None:
        return video_path.with_name(f"{video_path.stem}_overlay.mp4")
    return output


def parse_clip_spec(spec: str) -> ClipJob:
    parts = [p.strip() for p in spec.split("|")]
    if len(parts) not in (3, 4):
        raise ValueError(
            "Ungültiges --clip Format. Erwartet: "
            "video_path|video_sync_sec|csv_sync_mmss[|output_path]"
        )

    video_path = Path(parts[0])
    try:
        video_sync_sec = float(parts[1])
    except ValueError as exc:
        raise ValueError(f"Ungültige video_sync_sec in --clip: {parts[1]}") from exc

    csv_sync_sec = parse_duration_to_seconds(parts[2])
    output_path = Path(parts[3]) if len(parts) == 4 else video_path.with_name(f"{video_path.stem}_overlay.mp4")

    return ClipJob(
        video_path=video_path,
        output_path=output_path,
        video_sync_sec=video_sync_sec,
        csv_sync_sec=csv_sync_sec,
    )


def build_jobs(args) -> List[ClipJob]:
    clips = args.clip or []
    if clips:
        jobs = [parse_clip_spec(spec) for spec in clips]
        return jobs

    if args.video is None:
        raise ValueError("Bitte --video angeben oder mindestens ein --clip verwenden")

    return [
        ClipJob(
            video_path=args.video,
            output_path=derive_output_path(args.video, args.output),
            video_sync_sec=args.video_sync_sec,
            csv_sync_sec=parse_duration_to_seconds(args.csv_sync_mmss),
        )
    ]


def process_video(
    video_path: Path,
    output_path: Path,
    video_sync_sec: float,
    csv_sync_sec: float,
    fields: List[str],
    samples: List[DiveSample],
    times: List[float],
    codec: str = "auto",
    swap_rb: bool = False,
    show_graph: bool = False,
    progress_callback: Optional[Callable[[int, int], None]] = None,
    progress_interval_frames: int = 10,
    stop_check: Optional[Callable[[], bool]] = None,
) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    cap = cv2.VideoCapture(str(video_path))
    if not cap.isOpened():
        raise RuntimeError(f"Konnte Video nicht öffnen: {video_path}")

    fps = cap.get(cv2.CAP_PROP_FPS)
    if not fps or not math.isfinite(fps) or fps <= 0:
        fps = 30.0

    total_frames = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))
    if total_frames < 0:
        total_frames = 0

    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))

    writer, used_codec = open_video_writer(output_path, fps, width, height, codec)

    frame_idx = 0
    if progress_callback:
        progress_callback(0, total_frames)

    while True:
        if stop_check and stop_check():
            break
        ok, frame = cap.read()
        if not ok:
            break

        video_sec = frame_idx / fps
        dive_sec = csv_sync_sec + (video_sec - video_sync_sec)

        lines: List[str] = []
        if "time" in fields:
            lines.append(f"Tauchzeit: {format_duration(dive_sec)}")

        sample_idx = choose_sample_index(times, dive_sec)
        if sample_idx is not None:
            sample = samples[sample_idx]
            for field in fields:
                if field == "time":
                    continue
                value = value_for_field(sample, field)
                if value:
                    lines.append(value)

        if not lines:
            lines = ["Keine Daten"]

        draw_overlay(frame, lines)
        if show_graph:
            draw_depth_graph(frame, samples, times, dive_sec)
        if swap_rb:
            out_frame = cv2.cvtColor(frame, cv2.COLOR_BGR2RGB)
            writer.write(out_frame)
        else:
            writer.write(frame)
        frame_idx += 1

        if progress_callback and progress_interval_frames > 0 and frame_idx % progress_interval_frames == 0:
            progress_callback(frame_idx, total_frames)

    cap.release()
    writer.release()

    if progress_callback:
        progress_callback(frame_idx, total_frames)

    print(f"Codec: {used_codec}")
    print(f"Swap RB: {swap_rb}")
    if stop_check and stop_check():
        print("Abbruch durch Benutzer")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Blendet Tauchdaten aus CSV über ein Video ein")
    parser.add_argument("--csv", required=True, type=Path, help="Pfad zur CSV-Datei")
    parser.add_argument("--video", type=Path, help="Pfad zur Video-Datei (Single-Clip Modus)")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Ausgabedatei (Standard: <video_stem>_overlay.mp4)",
    )
    parser.add_argument(
        "--video-sync-sec",
        type=float,
        default=0.0,
        help="Sekunde im Video, bei der die CSV-Sync-Zeit gilt",
    )
    parser.add_argument(
        "--csv-sync-mmss",
        type=str,
        default="0:00",
        help="Tauchzeit am Sync-Punkt (Format mm:ss oder hh:mm:ss)",
    )
    parser.add_argument(
        "--fields",
        type=str,
        default="time,depth,temp,pressure,hr",
        help="Anzuzeigende Felder: time,depth,temp,pressure,hr",
    )
    parser.add_argument(
        "--column-map",
        type=str,
        default="",
        help="CSV-Spaltenzuordnung: time=...,depth=...,temp=...,pressure=...,hr=...,date=...,clock=...",
    )
    parser.add_argument(
        "--codec",
        type=str,
        default="auto",
        help="Video-Codec: auto, avc1, H264, mp4v, XVID, MJPG",
    )
    parser.add_argument(
        "--swap-rb",
        action="store_true",
        help="Tauscht Rot/Blau der Video-Frames (Fix bei blauer Haut)",
    )
    parser.add_argument(
        "--show-graph",
        action="store_true",
        help="Zeigt kleines Tiefenprofil (Graph) an",
    )
    parser.add_argument(
        "--auto-sync",
        action="store_true",
        help="Automatisches Sync basierend auf MP4 CreationTime + CSV Datum/Uhrzeit",
    )
    parser.add_argument(
        "--base-clip",
        type=str,
        default="",
        help="Clip-Pfad fuer Auto-Sync (muss in --clip enthalten sein)",
    )
    parser.add_argument(
        "--base-video-sync-sec",
        type=float,
        default=0.0,
        help="Video-Sekunde des manuellen Sync-Punkts (nur Auto-Sync)",
    )
    parser.add_argument(
        "--base-csv-datetime",
        type=str,
        default="",
        help="CSV Datum/Uhrzeit am Sync-Punkt (ISO: YYYY-MM-DD HH:MM:SS)",
    )
    parser.add_argument(
        "--clip",
        action="append",
        default=[],
        help=(
            "Mehrere Clips verarbeiten. Format: "
            "video_path|video_sync_sec|csv_sync_mmss[|output_path]. "
            "Kann mehrfach angegeben werden."
        ),
    )
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    csv_path: Path = args.csv

    if not csv_path.exists():
        raise FileNotFoundError(f"CSV nicht gefunden: {csv_path}")

    fields = parse_fields(args.fields)
    column_map = parse_column_map(args.column_map)
    jobs = build_jobs(args)

    for job in jobs:
        if not job.video_path.exists():
            raise FileNotFoundError(f"Video nicht gefunden: {job.video_path}")

    samples = load_samples(csv_path, column_map=column_map)
    times = [s.elapsed_sec for s in samples]

    if args.auto_sync:
        if not args.clip:
            raise ValueError("Auto-Sync benötigt --clip Angaben")
        if not args.base_clip:
            raise ValueError("Auto-Sync benötigt --base-clip")
        if not args.base_csv_datetime:
            raise ValueError("Auto-Sync benötigt --base-csv-datetime")

        base_clip = Path(args.base_clip)
        base_job = None
        for job in jobs:
            if job.video_path.resolve() == base_clip.resolve():
                base_job = job
                break
        if base_job is None:
            raise ValueError("--base-clip muss einer der --clip Pfade sein")

        base_video_start = get_video_creation_time_utc(base_job.video_path)
        base_csv_dt = parse_datetime_text(args.base_csv_datetime)
        base_video_sync = float(args.base_video_sync_sec)

        date_col, clock_col = read_csv_datetime_columns(csv_path)
        if date_col is None or clock_col is None:
            raise ValueError("CSV braucht Datum- und Uhrzeit-Spalten für Auto-Sync")

        with csv_path.open("r", encoding="utf-8-sig", newline="") as f:
            reader = csv.DictReader(f)
            first_row = next(reader, None)
        if not first_row:
            raise ValueError("CSV enthält keine Zeilen")
        first_dt = parse_datetime_utc(first_row[date_col], first_row[clock_col])
        base_csv_dt_offset_sec = (base_csv_dt - first_dt).total_seconds()

        for job in jobs:
            job.video_start_utc = get_video_creation_time_utc(job.video_path)
            delta_sec = (job.video_start_utc - base_video_start).total_seconds()
            job.video_sync_sec = base_video_sync
            job.csv_sync_sec = max(0.0, base_csv_dt_offset_sec + delta_sec)

    for i, job in enumerate(jobs, start=1):
        job.output_path = job.output_path.with_suffix(".mp4")
        process_video(
            video_path=job.video_path,
            output_path=job.output_path,
            video_sync_sec=job.video_sync_sec,
            csv_sync_sec=job.csv_sync_sec,
            fields=fields,
            samples=samples,
            times=times,
            codec=args.codec,
            swap_rb=args.swap_rb,
            show_graph=args.show_graph,
        )
        print(f"[{i}/{len(jobs)}] Fertig: {job.output_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
