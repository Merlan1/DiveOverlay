//! Joining the per-clip results of one dive back into a single video.
//!
//! A dive filmed with an action cam usually arrives as several clips (the
//! camera splits on a file-size limit, or the diver stops and restarts
//! recording). All of them are overlaid from the *same* CSV, so they are
//! really one dive, and the natural deliverable is one file.
//!
//! The flow both frontends drive is: [`plan_merge`] (order the clips and
//! point them at scratch part files), run `pipeline::process_clip` per job
//! as usual, then [`finish_merge`] (concatenate, re-base the subtitle
//! sidecar, delete the parts).
//!
//! Clips are joined back-to-back with no filler for the real-world gaps
//! between them, so the overlay's dive time jumps at each seam -- which is
//! an honest rendering of footage that genuinely does not exist.

use std::cmp::Ordering;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as MemOrdering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

use crate::error::CoreError;
use crate::ffprobe::probe_video;
use crate::model::ClipJob;
use crate::pipeline::{
    ensure_output_differs_from_input, ffmpeg_failure, finish_stderr, resolve_encoder, spawn_stderr_capture, OutputMode,
    ProcessingOptions,
};
use crate::subtitle::{concat_srt, SrtPart};

/// Sorts clips into the order they happened during the dive. Frontends
/// accept clips in whatever order the user listed them, which is not
/// necessarily chronological -- concatenating in list order would splice
/// the dive out of sequence.
///
/// `video_start_utc` (populated by auto-sync) only breaks ties, since it is
/// `None` for manually synced clips.
pub fn sort_jobs_chronologically(jobs: &mut [ClipJob]) {
    jobs.sort_by(|a, b| {
        a.dive_start_sec
            .partial_cmp(&b.dive_start_sec)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.video_start_utc.cmp(&b.video_start_utc))
    });
}

/// Where the per-clip results of a merged run live until they are joined.
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub parts_dir: PathBuf,
    /// Part files in merge order -- the same order `jobs` was left in by
    /// [`plan_merge`], so job `i` writes `part_paths[i]`.
    pub part_paths: Vec<PathBuf>,
}

/// Prepares a merged run: sorts `jobs` into dive order and redirects each
/// one's `output_path` to a numbered scratch file next to `merge_output`.
///
/// The per-clip outputs the caller configured are deliberately overridden:
/// a merged run keeps only the merged file, and numbering the parts keeps
/// two source clips that happen to share a file name from colliding.
pub fn plan_merge(jobs: &mut [ClipJob], merge_output: &Path) -> Result<MergePlan, CoreError> {
    if jobs.is_empty() {
        return Err(CoreError::Other("Merging needs at least one clip".to_string()));
    }
    for job in jobs.iter() {
        ensure_output_differs_from_input(&job.video_path, merge_output)?;
    }

    sort_jobs_chronologically(jobs);

    let stem = merge_output.file_stem().and_then(|s| s.to_str()).unwrap_or("dive");
    let parts_dir = merge_output.with_file_name(format!("{stem}_parts"));
    std::fs::create_dir_all(&parts_dir)?;

    let mut part_paths = Vec::with_capacity(jobs.len());
    for (i, job) in jobs.iter_mut().enumerate() {
        let path = parts_dir.join(format!("part_{i:03}.mp4"));
        job.output_path = path.clone();
        part_paths.push(path);
    }

    Ok(MergePlan { parts_dir, part_paths })
}

/// Concatenates the finished parts into `merge_output`, re-bases the
/// subtitle sidecar (subtitle mode only) and removes the scratch parts.
///
/// `progress` is called with `(seconds_written, total_seconds)` as ffmpeg
/// reports its position, so a frontend can show that a long concatenation
/// is moving.
///
/// Returns `Ok(false)` if `stop_flag` cancelled the concatenation; the
/// parts are then left in place rather than deleted, so the expensive
/// per-clip encodes are not thrown away.
pub fn finish_merge(
    plan: &MergePlan,
    merge_output: &Path,
    mode: OutputMode,
    options: &ProcessingOptions,
    stop_flag: &Arc<AtomicBool>,
    progress: impl FnMut(f64, f64),
) -> Result<bool, CoreError> {
    if !merge_clips(&plan.part_paths, merge_output, mode, options, stop_flag, progress)? {
        return Ok(false);
    }
    if mode == OutputMode::Subtitles {
        write_merged_srt(&plan.part_paths, merge_output)?;
    }
    cleanup_parts(plan);
    Ok(true)
}

/// Deletes the scratch parts (and their subtitle sidecars). The directory
/// itself is removed with `remove_dir`, which fails harmlessly if anything
/// else ended up in it -- so pointing a merged run at a directory that
/// already held files never deletes them.
pub fn cleanup_parts(plan: &MergePlan) {
    for part in &plan.part_paths {
        let _ = std::fs::remove_file(part);
        let _ = std::fs::remove_file(part.with_extension("srt"));
    }
    let _ = std::fs::remove_dir(&plan.parts_dir);
}

struct PartInfo {
    path: PathBuf,
    width: u32,
    height: u32,
    fps: f64,
    duration_sec: f64,
    has_audio: bool,
}

fn probe_parts(inputs: &[PathBuf]) -> Result<Vec<PartInfo>, CoreError> {
    inputs
        .iter()
        .map(|path| {
            let info = probe_video(path)?;
            Ok(PartInfo {
                path: path.clone(),
                width: info.width,
                height: info.height,
                fps: info.fps,
                duration_sec: part_duration(&info),
                has_audio: info.has_audio,
            })
        })
        .collect()
}

fn part_duration(info: &crate::ffprobe::VideoInfo) -> f64 {
    info.duration_sec
        .or_else(|| info.estimated_frames.map(|frames| frames as f64 / info.fps))
        .unwrap_or(0.0)
}

/// Whether the parts can be joined by a stream copy. Frame geometry and
/// audio presence have to line up; the codec itself always does, since
/// every part came out of the same run's encoder.
fn parts_are_uniform(parts: &[PartInfo]) -> bool {
    let Some(first) = parts.first() else { return true };
    parts.iter().all(|p| {
        p.width == first.width
            && p.height == first.height
            && (p.fps - first.fps).abs() < 0.01
            && p.has_audio == first.has_audio
    })
}

/// Joins `inputs` into `output`, in the order given.
///
/// Takes the lossless route (concat demuxer, `-c copy`) whenever the parts
/// share geometry, frame rate and audio presence, and falls back to a
/// re-encoding concat filter -- scaling and padding everything into the
/// largest part's frame -- when they don't. Subtitle mode has no fallback
/// on purpose: re-encoding there would defeat the whole point of a mode
/// that never touches a pixel.
pub fn merge_clips(
    inputs: &[PathBuf],
    output: &Path,
    mode: OutputMode,
    options: &ProcessingOptions,
    stop_flag: &Arc<AtomicBool>,
    mut progress: impl FnMut(f64, f64),
) -> Result<bool, CoreError> {
    if inputs.is_empty() {
        return Err(CoreError::Other("Merging needs at least one clip".to_string()));
    }
    for input in inputs {
        ensure_output_differs_from_input(input, output)?;
    }
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let parts = probe_parts(inputs)?;
    let uniform = parts_are_uniform(&parts);
    // The merged length is the sum of the parts (they are joined
    // back-to-back with no filler), which is what ffmpeg's reported output
    // position is measured against.
    let total_sec = parts.iter().map(|p| p.duration_sec).sum::<f64>();

    if mode == OutputMode::Subtitles && !uniform {
        return Err(CoreError::Other(
            "Cannot merge these clips in subtitle mode: they differ in resolution, frame rate or audio, \
             which a lossless copy cannot join. Use overlay mode to merge them (it re-encodes), or merge \
             clips recorded with matching camera settings."
                .to_string(),
        ));
    }

    if uniform {
        let list_path = output.with_extension("concat.txt");
        write_concat_list(inputs, &list_path)?;
        let result = concat_copy(&list_path, output, total_sec, stop_flag, &mut progress);
        let _ = std::fs::remove_file(&list_path);
        match result {
            Ok(finished) => return Ok(finished),
            // A stream copy can still be refused for a mismatch ffprobe's
            // geometry doesn't show (different H.264 profile/level between
            // a hardware and a software encoded part, say). Re-encoding
            // always works, so prefer that over failing the whole run.
            Err(copy_err) => {
                if mode == OutputMode::Subtitles {
                    return Err(copy_err);
                }
                eprintln!("[merge] stream copy failed, re-encoding instead: {copy_err}");
            }
        }
    }

    concat_reencode(&parts, output, options, total_sec, stop_flag, &mut progress)
}

/// Writes the concat demuxer's playlist. Paths are made absolute (the
/// demuxer resolves relative entries against the list file, not the working
/// directory) and single quotes are escaped the way that format requires.
fn write_concat_list(inputs: &[PathBuf], list_path: &Path) -> Result<(), CoreError> {
    let mut text = String::new();
    for input in inputs {
        let absolute = if input.is_absolute() {
            input.clone()
        } else {
            std::env::current_dir()
                .map(|dir| dir.join(input))
                .unwrap_or_else(|_| input.clone())
        };
        // Forward slashes keep Windows paths from being read as escapes.
        let escaped = absolute.to_string_lossy().replace('\\', "/").replace('\'', "'\\''");
        text.push_str(&format!("file '{escaped}'\n"));
    }
    std::fs::write(list_path, text)?;
    Ok(())
}

fn concat_copy(
    list_path: &Path,
    output: &Path,
    total_sec: f64,
    stop_flag: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<bool, CoreError> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-nostdin", "-v", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(list_path)
        // Each stream named explicitly rather than a blanket `-map 0`. The
        // subtitle stream has to be asked for (default selection would drop
        // it), but `-map 0` is not the way to get it: mp4 files carry data
        // tracks the mp4 muxer then refuses to write back -- a GoPro clip
        // remuxed in subtitle mode ends up with a `tmcd` timecode track, and
        // mapping it fails the whole merge with "Cannot map stream #0:3 -
        // unsupported type". Audio and subtitles stay optional (`?`) so
        // silent clips and overlay mode still work.
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-map", "0:s:0?", "-c", "copy"])
        .arg(output);
    run_ffmpeg(cmd, "Merge", total_sec, stop_flag, progress)
}

fn concat_reencode(
    parts: &[PartInfo],
    output: &Path,
    options: &ProcessingOptions,
    total_sec: f64,
    stop_flag: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<bool, CoreError> {
    let target = parts
        .iter()
        .max_by_key(|p| u64::from(p.width) * u64::from(p.height))
        .expect("callers reject an empty part list");
    // Encoders (hardware ones especially) reject odd dimensions.
    let width = target.width & !1;
    let height = target.height & !1;
    if width == 0 || height == 0 {
        return Err(CoreError::Ffprobe(
            "Could not determine a frame size to merge into".to_string(),
        ));
    }
    let fps = parts.iter().map(|p| p.fps).fold(0.0f64, f64::max);
    let with_audio = parts.iter().any(|p| p.has_audio);

    let (encoder_name, pix_fmt, extra_args, _info) =
        resolve_encoder(options.codec, options.preset, options.hw_accel, width, height);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-nostdin", "-v", "error"]);
    for part in parts {
        cmd.arg("-i").arg(&part.path);
    }

    // Silent parts get their own anullsrc input, trimmed to that part's
    // length: the concat filter needs every segment to carry the same set
    // of streams, so a mix of audio and audio-less clips has to be evened
    // out before it can be joined.
    let mut silence_input_index = parts.len();
    let mut silence_for_part: Vec<Option<usize>> = Vec::with_capacity(parts.len());
    for part in parts {
        if with_audio && !part.has_audio {
            cmd.args(["-f", "lavfi", "-t", &format!("{:.6}", part.duration_sec.max(0.001))])
                .args(["-i", "anullsrc=channel_layout=stereo:sample_rate=48000"]);
            silence_for_part.push(Some(silence_input_index));
            silence_input_index += 1;
        } else {
            silence_for_part.push(None);
        }
    }

    let mut filter = String::new();
    for (i, _part) in parts.iter().enumerate() {
        filter.push_str(&format!(
            "[{i}:v]scale={width}:{height}:force_original_aspect_ratio=decrease,\
             pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps:.6}[v{i}];"
        ));
        if with_audio {
            let source = match silence_for_part[i] {
                Some(index) => format!("{index}:a"),
                None => format!("{i}:a:0"),
            };
            filter.push_str(&format!(
                "[{source}]aresample=async=1,\
                 aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a{i}];"
            ));
        }
    }
    for i in 0..parts.len() {
        filter.push_str(&format!("[v{i}]"));
        if with_audio {
            filter.push_str(&format!("[a{i}]"));
        }
    }
    filter.push_str(&format!(
        "concat=n={}:v=1:a={}[vout]{}",
        parts.len(),
        u8::from(with_audio),
        if with_audio { "[aout]" } else { "" }
    ));

    cmd.args(["-filter_complex", &filter]).args(["-map", "[vout]"]);
    if with_audio {
        cmd.args(["-map", "[aout]"]);
    }
    cmd.args(["-c:v", encoder_name])
        .args(&extra_args)
        .args(["-pix_fmt", pix_fmt]);
    if with_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    cmd.arg(output);

    run_ffmpeg(cmd, "Merge", total_sec, stop_flag, progress)
}

/// Runs an ffmpeg invocation to completion, polling `stop_flag` so a merge
/// stays as cancellable as the per-clip processing it follows, and
/// forwarding ffmpeg's own position reports to `progress`.
///
/// `-progress pipe:1` makes ffmpeg print machine-readable `key=value`
/// blocks on stdout (independent of `-v error`, which keeps the human log
/// quiet). Merging a full dive can run for minutes -- long enough that no
/// output at all reads as a hang -- and this is the only position
/// information available, since the concatenation happens entirely inside
/// ffmpeg rather than frame by frame in this process.
fn run_ffmpeg(
    mut cmd: Command,
    what: &str,
    total_sec: f64,
    stop_flag: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<bool, CoreError> {
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Ffmpeg(format!("Failed to start merge: {e}")))?;
    let stderr = spawn_stderr_capture(child.stderr.take().expect("stderr was piped"));
    let positions = spawn_progress_reader(child.stdout.take().expect("stdout was piped"));

    let mut last_sec = 0.0f64;
    progress(0.0, total_sec);

    loop {
        while let Ok(sec) = positions.try_recv() {
            last_sec = sec;
        }
        progress(last_sec, total_sec);

        match child
            .try_wait()
            .map_err(|e| CoreError::Ffmpeg(format!("Error waiting for ffmpeg: {e}")))?
        {
            Some(status) => {
                let stderr_text = finish_stderr(stderr, "ffmpeg-merge", status.success());
                if !status.success() {
                    return Err(ffmpeg_failure(what, status, &stderr_text));
                }
                progress(total_sec, total_sec);
                return Ok(true);
            }
            None => {
                if stop_flag.load(MemOrdering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = finish_stderr(stderr, "ffmpeg-merge", false);
                    return Ok(false);
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// Reads ffmpeg's `-progress` stream and forwards the output position in
/// seconds. It runs on its own thread so a filled pipe can never stall
/// ffmpeg while the caller sleeps between `try_wait` polls.
fn spawn_progress_reader(stdout: ChildStdout) -> Receiver<f64> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            // `out_time_us` is microseconds; `out_time_ms` is ffmpeg's
            // misnamed twin of it (also microseconds), taken as a fallback
            // for builds that only emit the older key.
            if key != "out_time_us" && key != "out_time_ms" {
                continue;
            }
            if let Ok(micros) = value.trim().parse::<i64>() {
                if tx.send(micros.max(0) as f64 / 1_000_000.0).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

/// Rebuilds the sidecar `.srt` for a merged subtitle-mode output. The
/// embedded `mov_text` streams are re-timed by the concat demuxer itself,
/// but each part's sidecar was written on its own clip's timeline, so they
/// are shifted by the running total of the parts' durations.
fn write_merged_srt(parts: &[PathBuf], merge_output: &Path) -> Result<(), CoreError> {
    let mut documents: Vec<(String, f64)> = Vec::with_capacity(parts.len());
    let mut offset = 0.0;
    for part in parts {
        let duration = part_duration(&probe_video(part)?);
        // A missing sidecar just leaves that stretch uncaptioned; the
        // offset still advances so later parts stay in sync.
        let text = std::fs::read_to_string(part.with_extension("srt")).unwrap_or_default();
        documents.push((text, offset));
        offset += duration;
    }

    let srt_parts: Vec<SrtPart<'_>> = documents
        .iter()
        .map(|(srt, offset_sec)| SrtPart {
            srt,
            offset_sec: *offset_sec,
        })
        .collect();
    std::fs::write(merge_output.with_extension("srt"), concat_srt(&srt_parts)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn job(video: &str, dive_start_sec: f64) -> ClipJob {
        ClipJob {
            video_path: PathBuf::from(video),
            output_path: PathBuf::from("out.mp4"),
            dive_start_sec,
            video_start_utc: None,
        }
    }

    fn make_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("dive_overlay_merge_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn synth_clip(dir: &Path, name: &str, width: u32, height: u32, fps: u32, secs: u32, audio: bool) -> PathBuf {
        let path = dir.join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={width}x{height}:rate={fps}:duration={secs}"));
        if audio {
            cmd.args(["-f", "lavfi", "-i"])
                .arg(format!("sine=frequency=440:duration={secs}"));
        }
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
        if audio {
            cmd.args(["-c:a", "aac"]);
        }
        let status = cmd.arg(&path).status().unwrap();
        assert!(status.success());
        path
    }

    /// A clip carrying the extra tracks real camera footage has: a `tmcd`
    /// timecode data stream (what a GoPro writes, and what ffmpeg re-creates
    /// on any `-c copy` remux of one) plus a `mov_text` subtitle track, as
    /// subtitle mode produces.
    fn synth_clip_with_data_tracks(dir: &Path, name: &str, secs: u32) -> PathBuf {
        let srt = dir.join(format!("{name}.srt"));
        std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nDepth: 1.0 m\n\n").unwrap();

        let path = dir.join(name);
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size=160x120:rate=10:duration={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .arg("-i")
            .arg(&srt)
            .args(["-map", "0:v", "-map", "1:a", "-map", "2:0"])
            .args(["-timecode", "01:00:00:00"])
            .args([
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-c:s", "mov_text",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    fn stream_types(path: &Path) -> Vec<String> {
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim().trim_end_matches(',').to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn duration_of(path: &Path) -> f64 {
        part_duration(&probe_video(path).unwrap())
    }

    fn default_options() -> ProcessingOptions {
        use crate::pipeline::OutputResolution;
        ProcessingOptions {
            fields: Vec::new(),
            codec: crate::pipeline::Codec::Auto,
            preset: crate::pipeline::Preset::default(),
            hw_accel: false,
            show_graph: false,
            mode: OutputMode::Overlay,
            interpolate: false,
            resolution: OutputResolution::Original,
        }
    }

    #[test]
    fn sorts_clips_into_dive_order_not_list_order() {
        let mut jobs = vec![
            job("third.mp4", 598.0),
            job("first.mp4", 58.0),
            job("second.mp4", 295.0),
        ];
        sort_jobs_chronologically(&mut jobs);
        let order: Vec<_> = jobs.iter().map(|j| j.video_path.display().to_string()).collect();
        assert_eq!(order, vec!["first.mp4", "second.mp4", "third.mp4"]);
    }

    #[test]
    fn equal_dive_starts_fall_back_to_the_recording_timestamp() {
        let mut later = job("later.mp4", 100.0);
        later.video_start_utc = Some(Utc.with_ymd_and_hms(2025, 7, 5, 10, 5, 0).unwrap());
        let mut earlier = job("earlier.mp4", 100.0);
        earlier.video_start_utc = Some(Utc.with_ymd_and_hms(2025, 7, 5, 10, 0, 0).unwrap());

        let mut jobs = vec![later, earlier];
        sort_jobs_chronologically(&mut jobs);
        assert_eq!(jobs[0].video_path, PathBuf::from("earlier.mp4"));
    }

    #[test]
    fn plan_merge_orders_jobs_and_redirects_them_to_numbered_parts() {
        let dir = make_dir("plan");
        let merge_output = dir.join("dive_full.mp4");
        let mut jobs = vec![job("b.mp4", 200.0), job("a.mp4", 100.0)];

        let plan = plan_merge(&mut jobs, &merge_output).unwrap();

        assert_eq!(jobs[0].video_path, PathBuf::from("a.mp4"));
        assert_eq!(plan.parts_dir, dir.join("dive_full_parts"));
        assert_eq!(jobs[0].output_path, plan.part_paths[0]);
        assert_eq!(jobs[1].output_path, plan.part_paths[1]);
        assert!(plan.part_paths[0].ends_with("part_000.mp4"));
        assert!(plan.parts_dir.is_dir());
    }

    #[test]
    fn plan_merge_rejects_an_output_that_is_also_an_input() {
        let dir = make_dir("plan_collision");
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, b"not really a video").unwrap();
        let mut jobs = vec![ClipJob {
            video_path: clip.clone(),
            output_path: dir.join("out.mp4"),
            dive_start_sec: 0.0,
            video_start_utc: None,
        }];
        assert!(plan_merge(&mut jobs, &clip).is_err());
    }

    #[test]
    fn merges_uniform_clips_losslessly_into_one_file() {
        let dir = make_dir("uniform");
        let a = synth_clip(&dir, "a.mp4", 160, 120, 10, 1, true);
        let b = synth_clip(&dir, "b.mp4", 160, 120, 10, 2, true);
        let output = dir.join("merged.mp4");

        let stop = Arc::new(AtomicBool::new(false));
        let finished = merge_clips(
            &[a.clone(), b.clone()],
            &output,
            OutputMode::Overlay,
            &default_options(),
            &stop,
            |_, _| {},
        )
        .unwrap();

        assert!(finished);
        let merged = duration_of(&output);
        let expected = duration_of(&a) + duration_of(&b);
        assert!(
            (merged - expected).abs() < 0.35,
            "merged {merged}s vs expected {expected}s"
        );
        assert!(probe_video(&output).unwrap().has_audio);
        // The playlist is scratch, not a leftover for the user to find.
        assert!(!output.with_extension("concat.txt").exists());
    }

    /// A merge of a real dive runs for minutes inside a single ffmpeg call,
    /// so the frontends have nothing to show unless ffmpeg's own position
    /// reports come back out. Checks that they do, and that the last one
    /// lands on the full merged length.
    #[test]
    fn reports_merge_progress_against_the_total_length() {
        let dir = make_dir("progress");
        let a = synth_clip(&dir, "a.mp4", 160, 120, 10, 1, true);
        let b = synth_clip(&dir, "b.mp4", 160, 120, 10, 2, true);
        let output = dir.join("merged.mp4");

        let stop = Arc::new(AtomicBool::new(false));
        let mut reports: Vec<(f64, f64)> = Vec::new();
        let options = default_options();
        merge_clips(
            &[a.clone(), b.clone()],
            &output,
            OutputMode::Overlay,
            &options,
            &stop,
            |done, total| reports.push((done, total)),
        )
        .unwrap();

        let expected_total = duration_of(&a) + duration_of(&b);
        let (last_done, last_total) = *reports.last().expect("progress was never reported");
        assert!(
            (last_total - expected_total).abs() < 0.35,
            "reported total {last_total}s vs expected {expected_total}s"
        );
        assert!(
            (last_done - last_total).abs() < 1e-6,
            "merge ended at {last_done}s of {last_total}s"
        );
        assert!(reports.iter().all(|(done, _)| *done >= 0.0));
    }

    /// Regression: the merge used to map every input stream (`-map 0`),
    /// which the mp4 muxer rejects for the `tmcd` timecode track real camera
    /// footage carries -- "Cannot map stream #0:3 - unsupported type" failed
    /// the whole merge. Synthetic fixtures have no such track, so this one
    /// builds it deliberately. The subtitle track must still survive.
    #[test]
    fn merges_clips_that_carry_a_timecode_track_and_keeps_the_subtitles() {
        let dir = make_dir("data_tracks");
        let a = synth_clip_with_data_tracks(&dir, "a.mp4", 1);
        let b = synth_clip_with_data_tracks(&dir, "b.mp4", 1);
        assert!(
            stream_types(&a).iter().any(|t| t == "data"),
            "fixture is missing the timecode track this test exists for: {:?}",
            stream_types(&a)
        );

        let output = dir.join("merged.mp4");
        let stop = Arc::new(AtomicBool::new(false));
        let finished = merge_clips(
            &[a, b],
            &output,
            OutputMode::Subtitles,
            &default_options(),
            &stop,
            |_, _| {},
        )
        .unwrap();

        assert!(finished);
        let types = stream_types(&output);
        assert!(types.iter().any(|t| t == "video"), "{types:?}");
        assert!(types.iter().any(|t| t == "audio"), "{types:?}");
        assert!(
            types.iter().any(|t| t == "subtitle"),
            "subtitle track was dropped: {types:?}"
        );
    }

    #[test]
    fn merges_mismatched_clips_by_re_encoding_into_the_largest_frame() {
        let dir = make_dir("mismatched");
        // Different resolution and no audio on the second clip: neither can
        // survive a stream copy, so this must take the re-encode path.
        let a = synth_clip(&dir, "a.mp4", 320, 240, 10, 1, true);
        let b = synth_clip(&dir, "b.mp4", 160, 120, 10, 1, false);
        let output = dir.join("merged.mp4");

        let stop = Arc::new(AtomicBool::new(false));
        let finished = merge_clips(
            &[a.clone(), b.clone()],
            &output,
            OutputMode::Overlay,
            &default_options(),
            &stop,
            |_, _| {},
        )
        .unwrap();

        assert!(finished);
        let info = probe_video(&output).unwrap();
        assert_eq!((info.width, info.height), (320, 240));
        // Silence was synthesized for the audio-less part, so the merged
        // file has one continuous audio stream rather than none.
        assert!(info.has_audio);
        let expected = duration_of(&a) + duration_of(&b);
        assert!((part_duration(&info) - expected).abs() < 0.35);
    }

    #[test]
    fn subtitle_mode_refuses_to_merge_mismatched_clips_instead_of_re_encoding() {
        let dir = make_dir("subtitle_mismatch");
        let a = synth_clip(&dir, "a.mp4", 320, 240, 10, 1, true);
        let b = synth_clip(&dir, "b.mp4", 160, 120, 10, 1, true);
        let output = dir.join("merged.mp4");

        let stop = Arc::new(AtomicBool::new(false));
        let err = merge_clips(
            &[a, b],
            &output,
            OutputMode::Subtitles,
            &default_options(),
            &stop,
            |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("subtitle mode"), "unexpected error: {err}");
    }

    #[test]
    fn merged_srt_sidecar_shifts_each_part_onto_the_joined_timeline() {
        let dir = make_dir("srt");
        let a = synth_clip(&dir, "a.mp4", 160, 120, 10, 2, true);
        let b = synth_clip(&dir, "b.mp4", 160, 120, 10, 2, true);
        std::fs::write(
            a.with_extension("srt"),
            "1\n00:00:00,000 --> 00:00:01,000\nDive: 0:10\n\n",
        )
        .unwrap();
        std::fs::write(
            b.with_extension("srt"),
            "1\n00:00:00,000 --> 00:00:01,000\nDive: 5:00\n\n",
        )
        .unwrap();

        let merged = dir.join("merged.mp4");
        write_merged_srt(&[a.clone(), b], &merged).unwrap();

        let text = std::fs::read_to_string(merged.with_extension("srt")).unwrap();
        assert!(text.contains("1\n00:00:00,000 --> 00:00:01,000\nDive: 0:10"), "{text}");
        // The second part starts one clip-length in, and is renumbered.
        let offset = duration_of(&a);
        assert!(
            text.contains(&format!("2\n00:00:0{:.0}", offset.round())),
            "second cue not shifted by {offset}s: {text}"
        );
        assert!(text.contains("Dive: 5:00"));
    }

    #[test]
    fn cleanup_leaves_a_parts_directory_that_holds_anything_else() {
        let dir = make_dir("cleanup");
        let parts_dir = dir.join("dive_parts");
        std::fs::create_dir_all(&parts_dir).unwrap();
        let part = parts_dir.join("part_000.mp4");
        std::fs::write(&part, b"x").unwrap();
        let bystander = parts_dir.join("notes.txt");
        std::fs::write(&bystander, b"keep me").unwrap();

        cleanup_parts(&MergePlan {
            parts_dir: parts_dir.clone(),
            part_paths: vec![part.clone()],
        });

        assert!(!part.exists());
        assert!(bystander.exists());
        assert!(parts_dir.is_dir());
    }
}
