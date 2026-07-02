use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;

fn make_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("dive_overlay_cli_test").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn synth_clip(dir: &Path, name: &str, duration_secs: u32, fps: u32) -> PathBuf {
    let path = dir.join(name);
    let video_src = format!("testsrc=size=160x120:rate={fps}:duration={duration_secs}");
    let status = StdCommand::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i", &video_src])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .status()
        .expect("failed to run ffmpeg");
    assert!(status.success());
    path
}

fn synth_clip_with_creation_time(dir: &Path, name: &str, duration_secs: u32, fps: u32, creation_time: &str) -> PathBuf {
    let path = dir.join(name);
    let video_src = format!("testsrc=size=160x120:rate={fps}:duration={duration_secs}");
    let status = StdCommand::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i", &video_src])
        .args(["-metadata", &format!("creation_time={creation_time}")])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .status()
        .expect("failed to run ffmpeg");
    assert!(status.success());
    path
}

fn write_csv(dir: &Path) -> PathBuf {
    let csv_path = dir.join("dive.csv");
    std::fs::write(
        &csv_path,
        "sample time (min),sample depth (m)\n0:00,1.0\n0:01,2.0\n0:02,3.0\n",
    )
    .unwrap();
    csv_path
}

fn write_csv_with_datetime(dir: &Path) -> PathBuf {
    let csv_path = dir.join("dive.csv");
    std::fs::write(
        &csv_path,
        "date,time,sample time (min),sample depth (m)\n2025-07-05,09:58:00,0:00,1.0\n2025-07-05,09:59:00,1:00,2.0\n",
    )
    .unwrap();
    csv_path
}

#[test]
fn single_clip_produces_output() {
    let dir = make_dir("single");
    let clip = synth_clip(&dir, "input.mp4", 1, 5);
    let csv = write_csv(&dir);
    let output = dir.join("out.mp4");

    let mut cmd = Command::cargo_bin("dive_overlay_cli").unwrap();
    cmd.args(["--csv"])
        .arg(&csv)
        .args(["--video"])
        .arg(&clip)
        .args(["--video-sync-sec", "0", "--csv-sync-mmss", "0:00"])
        .args(["--output"])
        .arg(&output);
    cmd.assert().success();
    assert!(output.exists());
}

#[test]
fn multi_clip_via_repeated_clip_flag() {
    let dir = make_dir("multi");
    let clip1 = synth_clip(&dir, "c1.mp4", 1, 5);
    let clip2 = synth_clip(&dir, "c2.mp4", 1, 5);
    let csv = write_csv(&dir);

    let clip1_spec = format!("{}|0|0:00", clip1.display());
    let clip2_spec = format!("{}|0|0:01", clip2.display());

    let mut cmd = Command::cargo_bin("dive_overlay_cli").unwrap();
    cmd.args(["--csv"])
        .arg(&csv)
        .args(["--clip", &clip1_spec])
        .args(["--clip", &clip2_spec]);
    cmd.assert().success();

    assert!(clip1.with_file_name("c1_overlay.mp4").exists());
    assert!(clip2.with_file_name("c2_overlay.mp4").exists());
}

#[test]
fn auto_sync_end_to_end() {
    let dir = make_dir("auto_sync");
    let clip1 = synth_clip_with_creation_time(&dir, "c1.mp4", 1, 5, "2025-07-05T10:00:00Z");
    let clip2 = synth_clip_with_creation_time(&dir, "c2.mp4", 1, 5, "2025-07-05T10:05:00Z");
    let csv = write_csv_with_datetime(&dir);

    let clip1_spec = format!("{}|0|0:00", clip1.display());
    let clip2_spec = format!("{}|0|0:00", clip2.display());

    let mut cmd = Command::cargo_bin("dive_overlay_cli").unwrap();
    cmd.args(["--csv"])
        .arg(&csv)
        .args(["--clip", &clip1_spec])
        .args(["--clip", &clip2_spec])
        .arg("--auto-sync")
        .args(["--base-clip"])
        .arg(&clip1)
        .args(["--base-video-sync-sec", "0"])
        .args(["--base-csv-datetime", "2025-07-05 10:00:00"]);
    cmd.assert().success();

    assert!(clip1.with_file_name("c1_overlay.mp4").exists());
    assert!(clip2.with_file_name("c2_overlay.mp4").exists());
}

#[test]
fn missing_csv_fails_with_clear_error() {
    let dir = make_dir("missing_csv");
    let clip = synth_clip(&dir, "input.mp4", 1, 5);

    let mut cmd = Command::cargo_bin("dive_overlay_cli").unwrap();
    cmd.args(["--csv"])
        .arg(dir.join("does_not_exist.csv"))
        .args(["--video"])
        .arg(&clip);
    cmd.assert().failure();
}
