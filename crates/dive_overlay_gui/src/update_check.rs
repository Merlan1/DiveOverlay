use serde::Deserialize;

const REPO: &str = "Merlan1/DiveOverlay";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: String,
}

pub enum UpdateStatus {
    Available { version: String, url: String },
    UpToDate,
    Error(String),
}

pub fn spawn_check(tx: std::sync::mpsc::Sender<UpdateStatus>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let status = check();
        let _ = tx.send(status);
        ctx.request_repaint();
    });
}

fn check() -> UpdateStatus {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = ureq::get(&url)
        .set("User-Agent", "DiveOverlay-GUI")
        .call();

    let release: ReleaseResponse = match response {
        Ok(resp) => match resp.into_json() {
            Ok(release) => release,
            Err(e) => return UpdateStatus::Error(e.to_string()),
        },
        Err(e) => return UpdateStatus::Error(e.to_string()),
    };

    let latest = release.tag_name.trim_start_matches('v');
    if is_newer(latest, CURRENT_VERSION) {
        UpdateStatus::Available {
            version: release.tag_name,
            url: release.html_url,
        }
    } else {
        UpdateStatus::UpToDate
    }
}

fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> Vec<u32> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_version_is_detected() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn equal_or_older_version_is_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    #[ignore = "hits the real GitHub API"]
    fn check_reaches_github_and_parses_the_release() {
        match check() {
            UpdateStatus::Error(e) => panic!("check() failed: {e}"),
            UpdateStatus::Available { .. } | UpdateStatus::UpToDate => {}
        }
    }
}
