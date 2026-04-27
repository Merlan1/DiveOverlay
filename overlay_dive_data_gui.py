#!/usr/bin/env python3
"""Kleine GUI fuer das Tauchdaten-Overlay."""

from __future__ import annotations

import base64
import queue
import threading
import tkinter as tk
from dataclasses import dataclass
from pathlib import Path
from tkinter import filedialog, messagebox, ttk

import overlay_dive_data as core


@dataclass
class ClipEntry:
    video_path: Path
    video_sync_sec: float
    csv_sync_mmss: str
    output_path: Path


class ClipDialog(tk.Toplevel):
    def __init__(self, master, title: str, initial: ClipEntry | None = None):
        super().__init__(master)
        self.title(title)
        self.resizable(False, False)
        self.transient(master)
        self.result: ClipEntry | None = None

        self.video_var = tk.StringVar(value=str(initial.video_path) if initial else "")
        self.video_sync_var = tk.StringVar(value=f"{initial.video_sync_sec}" if initial else "0.0")
        self.csv_sync_var = tk.StringVar(value=initial.csv_sync_mmss if initial else "0:00")
        self.output_var = tk.StringVar(value=str(initial.output_path) if initial else "")

        self._build_ui()
        self.grab_set()
        self.protocol("WM_DELETE_WINDOW", self._cancel)

    def _build_ui(self) -> None:
        frame = ttk.Frame(self, padding=12)
        frame.grid(row=0, column=0, sticky="nsew")

        ttk.Label(frame, text="Video").grid(row=0, column=0, sticky="w")
        ttk.Entry(frame, textvariable=self.video_var, width=58).grid(row=1, column=0, sticky="ew", padx=(0, 8))
        ttk.Button(frame, text="Durchsuchen", command=self._browse_video).grid(row=1, column=1, sticky="ew")

        ttk.Label(frame, text="Video Sync (Sekunden)").grid(row=2, column=0, sticky="w", pady=(8, 0))
        ttk.Entry(frame, textvariable=self.video_sync_var, width=20).grid(row=3, column=0, sticky="w")

        ttk.Label(frame, text="CSV Sync (mm:ss oder hh:mm:ss)").grid(row=4, column=0, sticky="w", pady=(8, 0))
        ttk.Entry(frame, textvariable=self.csv_sync_var, width=20).grid(row=5, column=0, sticky="w")

        ttk.Label(frame, text="Output").grid(row=6, column=0, sticky="w", pady=(8, 0))
        ttk.Entry(frame, textvariable=self.output_var, width=58).grid(row=7, column=0, sticky="ew", padx=(0, 8))
        ttk.Button(frame, text="Speichern unter", command=self._browse_output).grid(row=7, column=1, sticky="ew")

        buttons = ttk.Frame(frame)
        buttons.grid(row=8, column=0, columnspan=2, sticky="e", pady=(12, 0))
        ttk.Button(buttons, text="Abbrechen", command=self._cancel).grid(row=0, column=0, padx=(0, 8))
        ttk.Button(buttons, text="OK", command=self._ok).grid(row=0, column=1)

    def _browse_video(self) -> None:
        path = filedialog.askopenfilename(
            title="Videodatei auswählen",
            filetypes=[("Video", "*.mp4 *.mov *.avi *.mkv"), ("Alle Dateien", "*.*")],
        )
        if path:
            self.video_var.set(path)
            if not self.output_var.get().strip():
                p = Path(path)
                self.output_var.set(str(p.with_name(f"{p.stem}_overlay.mp4")))

    def _browse_output(self) -> None:
        path = filedialog.asksaveasfilename(
            title="Output speichern",
            defaultextension=".mp4",
            filetypes=[("MP4", "*.mp4"), ("Alle Dateien", "*.*")],
        )
        if path:
            self.output_var.set(path)

    def _ok(self) -> None:
        video = self.video_var.get().strip()
        output = self.output_var.get().strip()
        if not video:
            messagebox.showerror("Fehler", "Bitte eine Videodatei wählen.", parent=self)
            return
        if not output:
            messagebox.showerror("Fehler", "Bitte einen Output-Pfad angeben.", parent=self)
            return

        try:
            video_sync_sec = float(self.video_sync_var.get().strip())
        except ValueError:
            messagebox.showerror("Fehler", "Video Sync muss eine Zahl sein.", parent=self)
            return

        csv_sync = self.csv_sync_var.get().strip()
        try:
            core.parse_duration_to_seconds(csv_sync)
        except Exception as exc:
            messagebox.showerror("Fehler", f"CSV Sync ungültig: {exc}", parent=self)
            return

        self.result = ClipEntry(
            video_path=Path(video),
            video_sync_sec=video_sync_sec,
            csv_sync_mmss=csv_sync,
            output_path=Path(output),
        )
        self.destroy()

    def _cancel(self) -> None:
        self.result = None
        self.destroy()


class App:
    def __init__(self, root: tk.Tk):
        self.root = root
        self.root.title("Tauchdaten Overlay")
        self.root.geometry("980x620")

        self.csv_var = tk.StringVar()
        self.fields_var = tk.StringVar(value="time,depth,temp,pressure,hr")
        self.codec_var = tk.StringVar(value="auto")
        self.status_var = tk.StringVar(value="Bereit")
        self.entries: list[ClipEntry] = []
        self.log_queue: queue.Queue[str] = queue.Queue()
        self.worker_thread: threading.Thread | None = None
        self.preview_window: tk.Toplevel | None = None
        self.preview_image: tk.PhotoImage | None = None
        self.preview_clip_index: int | None = None

        self._build_ui()
        self._poll_log_queue()

    def _build_ui(self) -> None:
        frame = ttk.Frame(self.root, padding=12)
        frame.pack(fill="both", expand=True)

        top = ttk.LabelFrame(frame, text="Allgemein", padding=10)
        top.pack(fill="x")

        ttk.Label(top, text="CSV").grid(row=0, column=0, sticky="w")
        ttk.Entry(top, textvariable=self.csv_var, width=90).grid(row=1, column=0, padx=(0, 8), sticky="ew")
        ttk.Button(top, text="Durchsuchen", command=self._choose_csv).grid(row=1, column=1, sticky="ew")

        ttk.Label(top, text="Felder (time,depth,temp,pressure,hr)").grid(row=2, column=0, sticky="w", pady=(8, 0))
        ttk.Entry(top, textvariable=self.fields_var, width=50).grid(row=3, column=0, sticky="w")

        ttk.Label(top, text="Codec (auto, avc1, H264, mp4v, XVID, MJPG)").grid(row=2, column=1, sticky="w", pady=(8, 0))
        codec_combo = ttk.Combobox(
            top,
            textvariable=self.codec_var,
            values=("auto", "avc1", "H264", "mp4v", "XVID", "MJPG"),
            state="readonly",
            width=12,
        )
        codec_combo.grid(row=3, column=1, sticky="w")
        codec_combo.current(0)

        top.columnconfigure(0, weight=1)

        middle = ttk.LabelFrame(frame, text="Clips", padding=10)
        middle.pack(fill="both", expand=True, pady=(10, 10))

        cols = ("video", "video_sync", "csv_sync", "output")
        self.tree = ttk.Treeview(middle, columns=cols, show="headings", height=12)
        self.tree.heading("video", text="Video")
        self.tree.heading("video_sync", text="Video Sync (s)")
        self.tree.heading("csv_sync", text="CSV Sync")
        self.tree.heading("output", text="Output")
        self.tree.column("video", width=260)
        self.tree.column("video_sync", width=110, anchor="e")
        self.tree.column("csv_sync", width=90, anchor="center")
        self.tree.column("output", width=360)
        self.tree.pack(side="left", fill="both", expand=True)

        scrollbar = ttk.Scrollbar(middle, orient="vertical", command=self.tree.yview)
        scrollbar.pack(side="left", fill="y")
        self.tree.configure(yscrollcommand=scrollbar.set)

        buttons = ttk.Frame(middle)
        buttons.pack(side="left", fill="y", padx=(10, 0))
        ttk.Button(buttons, text="Clip hinzufügen", command=self._add_clip).pack(fill="x", pady=(0, 8))
        ttk.Button(buttons, text="Clip bearbeiten", command=self._edit_clip).pack(fill="x", pady=(0, 8))
        ttk.Button(buttons, text="Clip entfernen", command=self._remove_clip).pack(fill="x", pady=(0, 8))
        ttk.Button(buttons, text="Sync Vorschau", command=self._preview_selected_clip).pack(fill="x", pady=(12, 8))

        bottom = ttk.LabelFrame(frame, text="Ausführung", padding=10)
        bottom.pack(fill="both", expand=True)

        run_row = ttk.Frame(bottom)
        run_row.pack(fill="x")
        self.run_button = ttk.Button(run_row, text="Verarbeitung starten", command=self._start_processing)
        self.run_button.pack(side="left")
        ttk.Label(run_row, textvariable=self.status_var).pack(side="left", padx=(12, 0))

        self.log_box = tk.Text(bottom, height=10, wrap="word", state="disabled")
        self.log_box.pack(fill="both", expand=True, pady=(8, 0))

    def _choose_csv(self) -> None:
        path = filedialog.askopenfilename(
            title="CSV auswählen",
            filetypes=[("CSV", "*.csv"), ("Alle Dateien", "*.*")],
        )
        if path:
            self.csv_var.set(path)

    def _selected_index(self) -> int | None:
        selected = self.tree.selection()
        if not selected:
            return None
        return int(selected[0])

    def _refresh_tree(self) -> None:
        for item in self.tree.get_children():
            self.tree.delete(item)
        for i, entry in enumerate(self.entries):
            self.tree.insert(
                "",
                "end",
                iid=str(i),
                values=(
                    str(entry.video_path),
                    f"{entry.video_sync_sec:.2f}",
                    entry.csv_sync_mmss,
                    str(entry.output_path),
                ),
            )

    def _add_clip(self) -> None:
        dlg = ClipDialog(self.root, "Clip hinzufügen")
        self.root.wait_window(dlg)
        if dlg.result:
            self.entries.append(dlg.result)
            self._refresh_tree()

    def _edit_clip(self) -> None:
        idx = self._selected_index()
        if idx is None:
            messagebox.showinfo("Hinweis", "Bitte einen Clip auswählen.")
            return

        dlg = ClipDialog(self.root, "Clip bearbeiten", initial=self.entries[idx])
        self.root.wait_window(dlg)
        if dlg.result:
            self.entries[idx] = dlg.result
            self._refresh_tree()

    def _remove_clip(self) -> None:
        idx = self._selected_index()
        if idx is None:
            messagebox.showinfo("Hinweis", "Bitte einen Clip auswählen.")
            return
        del self.entries[idx]
        self._refresh_tree()

    def _preview_selected_clip(self) -> None:
        idx = self._selected_index()
        if idx is None:
            messagebox.showinfo("Hinweis", "Bitte einen Clip auswählen.")
            return
        self._render_preview_for_index(idx)

    def _render_preview_for_index(self, idx: int) -> None:
        entry = self.entries[idx]
        if not entry.video_path.exists():
            messagebox.showerror("Fehler", f"Video nicht gefunden: {entry.video_path}")
            return

        csv_text = self.csv_var.get().strip()
        if not csv_text:
            messagebox.showerror("Fehler", "Bitte erst eine CSV-Datei auswählen.")
            return

        csv_path = Path(csv_text)
        if not csv_path.exists():
            messagebox.showerror("Fehler", f"CSV nicht gefunden: {csv_path}")
            return

        try:
            fields = core.parse_fields(self.fields_var.get().strip())
            csv_sync_sec = core.parse_duration_to_seconds(entry.csv_sync_mmss)
            samples = core.load_samples(csv_path)
            times = [s.elapsed_sec for s in samples]

            frame = self._extract_frame_at_second(entry.video_path, entry.video_sync_sec)
            lines = self._build_overlay_lines(fields, samples, times, csv_sync_sec)
            core.draw_overlay(frame, lines)
            rgb = self._prepare_preview_frame(frame)
            self._show_preview_window(rgb, idx, lines)
        except Exception as exc:
            messagebox.showerror("Fehler", f"Vorschau fehlgeschlagen: {exc}")

    def _adjust_selected_sync(self, delta_sec: float) -> None:
        idx = self.preview_clip_index
        if idx is None:
            return
        if idx < 0 or idx >= len(self.entries):
            return

        entry = self.entries[idx]
        new_sync = max(0.0, entry.video_sync_sec + delta_sec)
        self.entries[idx] = ClipEntry(
            video_path=entry.video_path,
            video_sync_sec=new_sync,
            csv_sync_mmss=entry.csv_sync_mmss,
            output_path=entry.output_path,
        )
        self._refresh_tree()
        self.tree.selection_set(str(idx))
        self._render_preview_for_index(idx)

    def _extract_frame_at_second(self, video_path: Path, second: float):
        cap = core.cv2.VideoCapture(str(video_path))
        if not cap.isOpened():
            raise RuntimeError(f"Konnte Video nicht öffnen: {video_path}")

        cap.set(core.cv2.CAP_PROP_POS_MSEC, max(0.0, second) * 1000.0)
        ok, frame = cap.read()

        if not ok:
            fps = cap.get(core.cv2.CAP_PROP_FPS)
            if fps and fps > 0:
                cap.set(core.cv2.CAP_PROP_POS_FRAMES, max(0, int(second * fps)))
                ok, frame = cap.read()

        cap.release()
        if not ok:
            raise RuntimeError("Konnte keinen Frame an der Sync-Stelle lesen")
        return frame

    def _build_overlay_lines(
        self,
        fields: list[str],
        samples: list[core.DiveSample],
        times: list[float],
        dive_sec: float,
    ) -> list[str]:
        lines: list[str] = []
        if "time" in fields:
            lines.append(f"Tauchzeit: {core.format_duration(dive_sec)}")

        sample_idx = core.choose_sample_index(times, dive_sec)
        if sample_idx is not None:
            sample = samples[sample_idx]
            for field in fields:
                if field == "time":
                    continue
                value = core.value_for_field(sample, field)
                if value:
                    lines.append(value)

        if not lines:
            lines = ["Keine Daten"]
        return lines

    def _prepare_preview_frame(self, frame):
        h, w = frame.shape[:2]
        max_w = 1100
        max_h = 650
        scale = min(max_w / w, max_h / h, 1.0)
        if scale < 1.0:
            new_w = int(w * scale)
            new_h = int(h * scale)
            frame = core.cv2.resize(frame, (new_w, new_h), interpolation=core.cv2.INTER_AREA)
        return core.cv2.cvtColor(frame, core.cv2.COLOR_BGR2RGB)

    def _show_preview_window(self, rgb_frame, clip_index: int, lines: list[str]) -> None:
        entry = self.entries[clip_index]
        ok, png = core.cv2.imencode(".png", rgb_frame)
        if not ok:
            raise RuntimeError("Konnte Vorschau-Bild nicht kodieren")

        img_b64 = base64.b64encode(png.tobytes())
        self.preview_image = tk.PhotoImage(data=img_b64)

        if self.preview_window and self.preview_window.winfo_exists():
            self.preview_window.destroy()

        self.preview_window = tk.Toplevel(self.root)
        self.preview_window.title("Sync Vorschau")
        self.preview_window.transient(self.root)
        self.preview_clip_index = clip_index

        frame = ttk.Frame(self.preview_window, padding=10)
        frame.pack(fill="both", expand=True)

        info = (
            f"Video: {entry.video_path.name} | Video Sync: {entry.video_sync_sec:.2f}s | "
            f"CSV Sync: {entry.csv_sync_mmss}"
        )
        ttk.Label(frame, text=info).pack(anchor="w", pady=(0, 6))

        controls = ttk.Frame(frame)
        controls.pack(anchor="w", pady=(0, 6))
        ttk.Button(controls, text="-0.5s", command=lambda: self._adjust_selected_sync(-0.5)).pack(side="left", padx=(0, 6))
        ttk.Button(controls, text="+0.5s", command=lambda: self._adjust_selected_sync(0.5)).pack(side="left", padx=(0, 6))
        ttk.Button(controls, text="Neu laden", command=lambda: self._render_preview_for_index(clip_index)).pack(side="left")

        preview_label = ttk.Label(frame, image=self.preview_image)
        preview_label.pack(fill="both", expand=True)
        preview_label.image = self.preview_image

        lines_text = " | ".join(lines)
        ttk.Label(frame, text=lines_text).pack(anchor="w", pady=(6, 0))

    def _set_running(self, running: bool) -> None:
        state = "disabled" if running else "normal"
        self.run_button.configure(state=state)

    def _log(self, text: str) -> None:
        self.log_queue.put(text)

    def _poll_log_queue(self) -> None:
        try:
            while True:
                line = self.log_queue.get_nowait()
                self.log_box.configure(state="normal")
                self.log_box.insert("end", line + "\n")
                self.log_box.see("end")
                self.log_box.configure(state="disabled")
        except queue.Empty:
            pass
        self.root.after(100, self._poll_log_queue)

    def _start_processing(self) -> None:
        if self.worker_thread and self.worker_thread.is_alive():
            return

        csv_path = Path(self.csv_var.get().strip()) if self.csv_var.get().strip() else None
        if not csv_path:
            messagebox.showerror("Fehler", "Bitte CSV-Datei auswählen.")
            return
        if not csv_path.exists():
            messagebox.showerror("Fehler", f"CSV nicht gefunden: {csv_path}")
            return
        if not self.entries:
            messagebox.showerror("Fehler", "Bitte mindestens einen Clip hinzufügen.")
            return

        try:
            fields = core.parse_fields(self.fields_var.get().strip())
        except Exception as exc:
            messagebox.showerror("Fehler", f"Feldliste ungültig: {exc}")
            return

        codec = self.codec_var.get().strip() or "auto"

        for entry in self.entries:
            if not entry.video_path.exists():
                messagebox.showerror("Fehler", f"Video nicht gefunden: {entry.video_path}")
                return

        self._set_running(True)
        self.status_var.set("Verarbeite...")
        self._log("Starte Verarbeitung...")

        self.worker_thread = threading.Thread(
            target=self._run_worker,
            args=(csv_path, fields, list(self.entries), codec),
            daemon=True,
        )
        self.worker_thread.start()

    def _run_worker(self, csv_path: Path, fields: list[str], entries: list[ClipEntry], codec: str) -> None:
        try:
            samples = core.load_samples(csv_path)
            times = [s.elapsed_sec for s in samples]
            total = len(entries)

            for idx, entry in enumerate(entries, start=1):
                self._log(f"[{idx}/{total}] {entry.video_path.name} -> {entry.output_path.name}")
                job = core.ClipJob(
                    video_path=entry.video_path,
                    output_path=entry.output_path,
                    video_sync_sec=entry.video_sync_sec,
                    csv_sync_sec=core.parse_duration_to_seconds(entry.csv_sync_mmss),
                )
                core.process_video(
                    video_path=job.video_path,
                    output_path=job.output_path,
                    video_sync_sec=job.video_sync_sec,
                    csv_sync_sec=job.csv_sync_sec,
                    fields=fields,
                    samples=samples,
                    times=times,
                    codec=codec,
                )
                self._log(f"[{idx}/{total}] Fertig: {job.output_path}")

            self._log("Alle Clips wurden verarbeitet.")
            self.root.after(0, self._on_done_success)
        except Exception as exc:
            self._log(f"Fehler: {exc}")
            self.root.after(0, lambda: self._on_done_error(str(exc)))

    def _on_done_success(self) -> None:
        self._set_running(False)
        self.status_var.set("Fertig")
        messagebox.showinfo("Fertig", "Alle Clips wurden erfolgreich verarbeitet.")

    def _on_done_error(self, error: str) -> None:
        self._set_running(False)
        self.status_var.set("Fehler")
        messagebox.showerror("Fehler", error)


def main() -> int:
    root = tk.Tk()
    App(root)
    root.mainloop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
