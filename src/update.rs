use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use uuid::Uuid;

const LATEST_RELEASE_URL: &str = "https://github.com/combinatrix-ai/dlgt/releases/latest";
const INSTALLER_URL: &str = "https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh";

pub fn check_for_update() -> Option<Value> {
    let latest = resolve_latest_version().ok()?;
    let current = env!("CARGO_PKG_VERSION");
    version_is_newer(&latest, current).then(|| {
        json!({
            "code": "UPDATE_AVAILABLE",
            "message": "A new version of dlgt is available.",
            "current_version": current,
            "latest_version": latest,
            "command": "dlgt update",
        })
    })
}

pub fn install_latest() -> Result<Value> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = resolve_latest_version()?;
    if !version_is_newer(&latest, current) {
        return Ok(json!({
            "updated": false,
            "current_version": current,
            "latest_version": latest,
        }));
    }

    let executable = std::env::current_exe().context("failed to locate dlgt executable")?;
    let bin_dir = executable
        .parent()
        .context("dlgt executable has no parent directory")?;
    let installer =
        std::env::temp_dir().join(format!("dlgt-installer-{}.sh", Uuid::new_v4().simple()));
    download_installer(&installer)?;
    let result = Command::new("sh")
        .arg(&installer)
        .args(["--bin-dir", &bin_dir.to_string_lossy(), "--skill", "both"])
        .arg("--version")
        .arg(format!("v{latest}"))
        .output();
    let _ = std::fs::remove_file(&installer);
    let result = result.context("failed to run dlgt installer")?;
    if !result.status.success() {
        bail!(
            "dlgt installer failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(json!({
        "updated": true,
        "previous_version": current,
        "version": latest,
        "binary": executable,
        "skills": ["codex", "claude"],
    }))
}

fn resolve_latest_version() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "3",
            "--output",
            "/dev/null",
            "--write-out",
            "%{url_effective}",
            LATEST_RELEASE_URL,
        ])
        .output()
        .context("failed to check the latest dlgt release")?;
    if !output.status.success() {
        bail!(
            "latest release check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let url = String::from_utf8(output.stdout).context("latest release URL was not UTF-8")?;
    let tag = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse_version(version).context("latest release had an invalid version")?;
    Ok(version.to_owned())
}

fn download_installer(path: &Path) -> Result<()> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
        ])
        .arg(path)
        .arg(INSTALLER_URL)
        .output()
        .context("failed to download the dlgt installer")?;
    if !output.status.success() {
        let _ = std::fs::remove_file(path);
        bail!(
            "installer download failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate)
        .zip(parse_version(current))
        .is_some_and(|(candidate, current)| candidate > current)
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(str::parse::<u64>);
    let parsed = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::{parse_version, version_is_newer};

    #[test]
    fn compares_release_versions_numerically() {
        assert!(version_is_newer("0.10.0", "0.9.9"));
        assert!(!version_is_newer("0.1.4", "0.1.4"));
        assert!(!version_is_newer("0.1.3", "0.1.4"));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("main"), None);
    }
}
