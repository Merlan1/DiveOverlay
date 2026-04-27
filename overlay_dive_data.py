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
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

import cv2


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


def load_samples(csv_path: Path) -> List[DiveSample]:
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

        time_col = resolve(["sample time (min)", "sample time", "time"])
        depth_col = resolve(["sample depth (m)", "sample depth", "depth"])
        temp_col = resolve(["sample temperature (c)", "sample temperature", "temperature"])
        pressure_col = resolve(["sample pressure (bar)", "sample pressure", "pressure"])
        hr_col = resolve(["sample heartrate", "sample heart rate", "heartrate", "heart rate"])

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


def open_video_writer(output_path: Path, fps: float, width: int, height: int, codec: str):
    codec = (codec or "auto").strip()
    if codec.lower() == "auto":
        if output_path.suffix.lower() == ".mp4":
            candidates = ["avc1", "H264", "mp4v"]
        else:
            candidates = ["XVID", "MJPG", "mp4v"]
    else:
        candidates = [codec]

    attempted: List[str] = []
    for cc in candidates:
        attempted.append(cc)
        fourcc = cv2.VideoWriter_fourcc(*cc)
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
) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    cap = cv2.VideoCapture(str(video_path))
    if not cap.isOpened():
        raise RuntimeError(f"Konnte Video nicht öffnen: {video_path}")

    fps = cap.get(cv2.CAP_PROP_FPS)
    if not fps or not math.isfinite(fps) or fps <= 0:
        fps = 30.0

    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))

    writer, used_codec = open_video_writer(output_path, fps, width, height, codec)

    frame_idx = 0
    while True:
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
        writer.write(frame)
        frame_idx += 1

    cap.release()
    writer.release()
    print(f"Codec: {used_codec}")


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
        "--codec",
        type=str,
        default="auto",
        help="Video-Codec: auto, avc1, H264, mp4v, XVID, MJPG",
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
    jobs = build_jobs(args)

    for job in jobs:
        if not job.video_path.exists():
            raise FileNotFoundError(f"Video nicht gefunden: {job.video_path}")

    samples = load_samples(csv_path)
    times = [s.elapsed_sec for s in samples]

    for i, job in enumerate(jobs, start=1):
        process_video(
            video_path=job.video_path,
            output_path=job.output_path,
            video_sync_sec=job.video_sync_sec,
            csv_sync_sec=job.csv_sync_sec,
            fields=fields,
            samples=samples,
            times=times,
            codec=args.codec,
        )
        print(f"[{i}/{len(jobs)}] Fertig: {job.output_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
