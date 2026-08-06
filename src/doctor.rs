use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::{Value, json};

use crate::{client, paths, skill, update};

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub status: &'static str,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub status: &'static str,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({"status":"fail","checks":[]}))
    }
}

pub fn run(probe: bool) -> Report {
    let mut checks = Vec::new();
    checks.push(binary_check());
    checks.push(config_check());
    checks.extend(runtime_checks());
    checks.push(provider_check(
        "codex",
        &provider_program("DLGT_CODEX_BIN", "codex"),
    ));
    checks.push(provider_check(
        "claude",
        &provider_program("DLGT_CLAUDE_BIN", "claude"),
    ));
    checks.push(skill_check("codex-skill", &codex_skill_path()));
    checks.push(skill_check("claude-skill", &claude_skill_path()));
    if probe {
        checks.push(codex_probe());
        checks.push(update_check());
    } else {
        checks.push(skipped(
            "codex-app-server",
            "provider probe not requested",
            "run `dlgt doctor --probe` to initialize Codex app-server without starting a model turn",
        ));
        checks.push(skipped(
            "release",
            "online release check not requested",
            "run `dlgt doctor --probe` to check the published version",
        ));
    }
    let status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "ok"
    };
    Report { status, checks }
}

pub fn human(report: &Report) -> String {
    let mut output = String::from("CHECK                 STATUS  DETAIL\n");
    for check in &report.checks {
        let _ = writeln!(
            output,
            "{:<21} {:<7} {}",
            check.id,
            check.status.to_ascii_uppercase(),
            check.evidence
        );
        if let Some(hint) = &check.hint {
            let _ = writeln!(output, "{:<30} hint: {hint}", "");
        }
    }
    let _ = writeln!(output, "\nOverall: {}", report.status.to_ascii_uppercase());
    output
}

fn binary_check() -> Check {
    match std::env::current_exe() {
        Ok(path) => ok(
            "binary",
            format!("{} ({})", env!("CARGO_PKG_VERSION"), path.display()),
        ),
        Err(error) => fail("binary", error.to_string(), None),
    }
}

fn config_check() -> Check {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => match text.parse::<toml_edit::DocumentMut>() {
            Ok(_) => ok("config", format!("valid TOML at {}", path.display())),
            Err(error) => fail(
                "config",
                format!("invalid TOML at {}: {error}", path.display()),
                Some("fix the config before launching another Session"),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ok(
            "config",
            format!("no config at {}; defaults apply", path.display()),
        ),
        Err(error) => fail(
            "config",
            format!("cannot read {}: {error}", path.display()),
            None,
        ),
    }
}

fn runtime_checks() -> Vec<Check> {
    let sockets = match paths::runtime_sockets() {
        Ok(sockets) => sockets,
        Err(error) => return vec![fail("runtime", error.to_string(), None)],
    };
    if sockets.is_empty() {
        return vec![ok("runtime", "no live daemon sockets")];
    }
    sockets
        .iter()
        .enumerate()
        .map(|(index, socket)| runtime_check(index, socket))
        .collect()
}

fn runtime_check(index: usize, socket: &Path) -> Check {
    let id = if index == 0 {
        "runtime"
    } else {
        "runtime-extra"
    };
    let mode = fs::metadata(socket)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .ok();
    match client::call_socket(socket, "server.ping", json!({})) {
        Ok(ping) => {
            let version = ping
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let evidence = format!(
                "v{version} at {} mode {:04o}",
                socket.display(),
                mode.unwrap_or_default()
            );
            if mode == Some(0o600) {
                ok(id, evidence)
            } else {
                fail(
                    id,
                    evidence,
                    Some("restart the daemon so dlgt recreates the socket with mode 0600"),
                )
            }
        }
        Err(error) => warn(
            id,
            format!("stale or unreachable socket {}: {error}", socket.display()),
            Some("remove the stale socket only after confirming no dlgt daemon owns it"),
        ),
    }
}

fn provider_check(id: &'static str, program: &Path) -> Check {
    match Command::new(program).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = first_nonempty(&output.stdout).or_else(|| first_nonempty(&output.stderr));
            ok(
                id,
                format!(
                    "{} ({})",
                    version.unwrap_or_else(|| "version command succeeded".to_owned()),
                    program.display()
                ),
            )
        }
        Ok(output) => warn(
            id,
            format!(
                "{} --version exited {}: {}",
                program.display(),
                output.status,
                first_nonempty(&output.stderr).unwrap_or_default()
            ),
            Some("repair or re-authenticate this Harness before delegating to it"),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => warn(
            id,
            format!("{} is not installed", program.display()),
            Some("install this Harness only if you intend to delegate to it"),
        ),
        Err(error) => warn(id, error.to_string(), None),
    }
}

fn skill_check(id: &'static str, path: &Path) -> Check {
    match fs::read(path) {
        Ok(installed) if installed == skill::TEXT.as_bytes() => {
            ok(id, format!("matches embedded skill at {}", path.display()))
        }
        Ok(_) => warn(
            id,
            format!("differs from embedded skill at {}", path.display()),
            Some("re-register the skill from this exact dlgt binary"),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => warn(
            id,
            format!("not installed at {}", path.display()),
            Some("register the embedded skill for this Harness if it should use dlgt"),
        ),
        Err(error) => warn(id, format!("cannot read {}: {error}", path.display()), None),
    }
}

fn codex_probe() -> Check {
    match client::call(
        "model.list",
        json!({"harness":"codex","include_hidden":false}),
    ) {
        Ok(_) => ok("codex-app-server", "initialize and model/list succeeded"),
        Err(error) => fail(
            "codex-app-server",
            error.to_string(),
            Some("run `codex --version` and inspect dlgt daemon diagnostics"),
        ),
    }
}

fn update_check() -> Check {
    match update::check_for_update() {
        Ok(Some(notice)) => warn(
            "release",
            format!(
                "update available: {} -> {}",
                notice["current_version"].as_str().unwrap_or("unknown"),
                notice["latest_version"].as_str().unwrap_or("unknown")
            ),
            Some("run `dlgt update` only after approving the replacement"),
        ),
        Ok(None) => ok(
            "release",
            format!("{} is current", env!("CARGO_PKG_VERSION")),
        ),
        Err(error) => warn(
            "release",
            format!("online check unavailable: {error}"),
            Some("retry when network access is available"),
        ),
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("DLGT_CONFIG").map_or_else(
        || home_dir().join(".config/dlgt/config.toml"),
        PathBuf::from,
    )
}

fn codex_skill_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map_or_else(|| home_dir().join(".codex"), PathBuf::from)
        .join("skills/dlgt/SKILL.md")
}

fn claude_skill_path() -> PathBuf {
    home_dir().join(".claude/skills/dlgt/SKILL.md")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from)
}

fn provider_program(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| PathBuf::from(fallback), PathBuf::from)
}

fn first_nonempty(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn ok(id: &'static str, evidence: impl Into<String>) -> Check {
    Check {
        id,
        status: "ok",
        evidence: evidence.into(),
        hint: None,
    }
}

fn warn(id: &'static str, evidence: impl Into<String>, hint: Option<&str>) -> Check {
    Check {
        id,
        status: "warn",
        evidence: evidence.into(),
        hint: hint.map(str::to_owned),
    }
}

fn fail(id: &'static str, evidence: impl Into<String>, hint: Option<&str>) -> Check {
    Check {
        id,
        status: "fail",
        evidence: evidence.into(),
        hint: hint.map(str::to_owned),
    }
}

fn skipped(id: &'static str, evidence: &str, hint: &str) -> Check {
    Check {
        id,
        status: "skip",
        evidence: evidence.to_owned(),
        hint: Some(hint.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Check, Report, human};

    #[test]
    fn human_report_contains_status_evidence_and_hint() {
        let report = Report {
            status: "warn",
            checks: vec![Check {
                id: "config",
                status: "warn",
                evidence: "missing".to_owned(),
                hint: Some("create it".to_owned()),
            }],
        };
        let text = human(&report);
        assert!(text.contains("config"));
        assert!(text.contains("WARN"));
        assert!(text.contains("missing"));
        assert!(text.contains("hint: create it"));
        assert!(text.contains("Overall: WARN"));
    }
}
