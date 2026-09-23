//! Pi interactive TUI adapter.
//!
//! Pi's `--mode rpc` stream does report `agent_settled`, which is the right
//! completion signal (`agent_end` can still be followed by a retry or
//! compaction). That event is also available to an extension inside the
//! interactive TUI. RPC would replace stdin and stdout with JSONL, so attach,
//! bracketed-paste follow-ups, and this hook lifecycle would no longer apply.
//! The extension therefore emits Claude-shaped hooks and waits for
//! `agent_settled`. It is loaded with `--extension` from a dlgt-owned path
//! outside Pi's autoload directory, and it no-ops unless `DLGT_PI_LAUNCH` is
//! set. Resume passes the session file path when it can be found: `pi
//! --session <id>` from another directory stops to ask whether to fork, and
//! that prompt never reaches `session_start`.
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::provider::{CommandSpec, LaunchOptions};

const EXTENSION: &str = include_str!("pi_extension.js");
const MARKER: &str = "DLGT_PI_LAUNCH";
const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn program() -> PathBuf {
    std::env::var_os("DLGT_PI_BIN").map_or_else(|| PathBuf::from("pi"), PathBuf::from)
}

pub fn configure(environment: &mut HashMap<String, String>, launch: &str) -> Result<()> {
    let home = environment.get("HOME").context("Pi launch requires HOME")?;
    let directory = Path::new(home).join(".pi/agent/dlgt");
    install_extension(&directory)?;
    environment.insert("DLGT_PI_LAUNCH".to_owned(), launch.to_owned());
    environment.insert(
        "DLGT_PI_HOOK_BIN".to_owned(),
        std::env::current_exe()?.to_string_lossy().into_owned(),
    );
    environment.insert(
        "DLGT_PI_EXTENSION".to_owned(),
        directory.join("bridge.js").to_string_lossy().into_owned(),
    );
    environment.insert(
        "DLGT_SOCKET".to_owned(),
        crate::paths::socket_path()?.to_string_lossy().into_owned(),
    );
    Ok(())
}

fn install_extension(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("bridge.lock"))?;
    // SAFETY: lock owns a valid file descriptor; closing it releases flock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock Pi extension");
    }
    let path = directory.join("bridge.js");
    match fs::read_to_string(&path) {
        Ok(existing) if existing == EXTENSION => return Ok(()),
        Ok(existing) if !existing.contains(MARKER) => {
            bail!(
                "Pi extension {} is not the dlgt bridge; left unchanged",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    crate::provider::write_config_atomic(&path, EXTENSION)?;
    Ok(())
}

pub fn validate(effort: Option<&str>, options: &[String]) -> Result<()> {
    if let Some(effort) = effort
        && !THINKING_LEVELS.contains(&effort)
    {
        bail!("invalid Pi effort {effort:?}; use off, minimal, low, medium, high, xhigh, or max");
    }
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key != "provider" || !valid_token(value) {
            bail!("invalid Pi harness option {key:?}; use provider=<name>");
        }
    }
    Ok(())
}

pub fn command(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    validate(options.effort, options.harness_options)?;
    let mut args = Vec::new();
    if let Some(model) = options.model {
        reject_flag_like("model", model)?;
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = options.effort {
        args.extend(["--thinking".to_owned(), effort.to_owned()]);
    }
    for option in options.harness_options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key == "provider" {
            args.extend(["--provider".to_owned(), value.to_owned()]);
        }
    }
    if options.auto_approve {
        // Pi does not prompt before each tool call. --approve skips the
        // project-trust dialog so the TUI can reach session_start.
        args.push("--approve".to_owned());
    }
    if let Some(extension) = options
        .environment
        .get("DLGT_PI_EXTENSION")
        .filter(|path| !path.is_empty())
    {
        args.extend(["--extension".to_owned(), extension.clone()]);
    }
    if let Some(id) = options.resume_provider_id {
        reject_flag_like("session", id)?;
        args.extend([
            "--session".to_owned(),
            resume_session_argument(options.environment, id),
        ]);
    }
    Ok(CommandSpec {
        program: program(),
        args,
        cwd: options.cwd.to_path_buf(),
        environment: options.environment.clone(),
    })
}

/// Prefer the on-disk session file. An id-only `--session` from another
/// directory is a global match, and Pi then blocks on an interactive fork
/// prompt before `session_start`.
fn resume_session_argument(environment: &HashMap<String, String>, id: &str) -> String {
    session_file(environment, id)
        .map_or_else(|| id.to_owned(), |path| path.to_string_lossy().into_owned())
}

fn session_file(environment: &HashMap<String, String>, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(dir) = environment
        .get("PI_CODING_AGENT_SESSION_DIR")
        .filter(|value| !value.is_empty())
    {
        roots.push(PathBuf::from(dir));
    }
    if let Some(home) = environment.get("HOME").filter(|value| !value.is_empty()) {
        let agent = Path::new(home).join(".pi/agent");
        if let Some(dir) = settings_session_dir(&agent.join("settings.json")) {
            roots.push(dir);
        }
        roots.push(agent.join("sessions"));
    }
    let suffix = format!("_{id}.jsonl");
    roots
        .into_iter()
        .find_map(|root| find_session_file(&root, &suffix))
}

fn settings_session_dir(path: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let dir = value
        .get("sessionDir")
        .and_then(serde_json::Value::as_str)
        .filter(|dir| !dir.is_empty())?;
    Some(PathBuf::from(dir))
}

fn find_session_file(root: &Path, suffix: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            return Some(path);
        }
        if !path.is_dir() {
            continue;
        }
        let Ok(nested) = fs::read_dir(&path) else {
            continue;
        };
        for child in nested.flatten() {
            if child.file_name().to_string_lossy().ends_with(suffix) {
                return Some(child.path());
            }
        }
    }
    None
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
        })
}

fn reject_flag_like(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        bail!("{label} must not be empty or look like a flag");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_uses_the_tui_and_not_rpc() -> Result<()> {
        let environment = HashMap::from([(
            "DLGT_PI_EXTENSION".to_owned(),
            "/tmp/pi-bridge.js".to_owned(),
        )]);
        let mut options = LaunchOptions {
            agent: crate::provider::Agent::Pi,
            session_id: "internal:TEST",
            title: "review",
            cwd: Path::new("/tmp"),
            model: Some("grok-4.7"),
            effort: Some("high"),
            harness_options: &["provider=xai".to_owned()],
            new_provider_id: None,
            resume_provider_id: Some("11111111-1111-1111-1111-111111111111"),
            environment: &environment,
            auto_approve: true,
            initial_prompt: None,
        };
        let spec = command(&options)?;
        assert_eq!(
            spec.args,
            [
                "--model",
                "grok-4.7",
                "--thinking",
                "high",
                "--provider",
                "xai",
                "--approve",
                "--extension",
                "/tmp/pi-bridge.js",
                "--session",
                "11111111-1111-1111-1111-111111111111",
            ]
        );
        assert!(!spec.args.iter().any(|arg| arg == "--mode" || arg == "rpc"));
        options.auto_approve = false;
        options.effort = None;
        options.harness_options = &[];
        options.resume_provider_id = None;
        let plain = command(&options)?;
        assert_eq!(
            plain.args,
            ["--model", "grok-4.7", "--extension", "/tmp/pi-bridge.js"]
        );
        assert!(validate(Some("turbo"), &[]).is_err());
        assert!(validate(None, &["mode=rpc".to_owned()]).is_err());
        assert!(validate(None, &["api-key=secret".to_owned()]).is_err());
        assert!(validate(Some("max"), &["provider=xai".to_owned()]).is_ok());
        Ok(())
    }

    #[test]
    fn resume_opens_the_session_file_so_a_different_cwd_does_not_prompt() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let id = "11111111-1111-1111-1111-111111111111";
        let file = temp
            .path()
            .join(".pi/agent/sessions/--tmp-proj--")
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        fs::create_dir_all(file.parent().context("session parent")?)?;
        fs::write(&file, "{}\n")?;
        let environment = HashMap::from([
            (
                "HOME".to_owned(),
                temp.path().to_string_lossy().into_owned(),
            ),
            (
                "DLGT_PI_EXTENSION".to_owned(),
                "/tmp/pi-bridge.js".to_owned(),
            ),
        ]);
        let options = LaunchOptions {
            agent: crate::provider::Agent::Pi,
            session_id: "internal:TEST",
            title: "review",
            cwd: Path::new("/workspace"),
            model: Some("grok-4.7"),
            effort: None,
            harness_options: &["provider=xai".to_owned()],
            new_provider_id: None,
            resume_provider_id: Some(id),
            environment: &environment,
            auto_approve: true,
            initial_prompt: None,
        };
        let spec = command(&options)?;
        let session = spec
            .args
            .iter()
            .position(|arg| arg == "--session")
            .and_then(|index| spec.args.get(index + 1));
        assert_eq!(session.map(String::as_str), file.to_str());
        Ok(())
    }

    #[test]
    fn extension_install_is_idempotent_and_preserves_foreign_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut environment = HashMap::from([(
            "HOME".to_owned(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        configure(&mut environment, "internal:ONE")?;
        let path = temp.path().join(".pi/agent/dlgt/bridge.js");
        let first = fs::read_to_string(&path)?;
        assert!(first.contains(MARKER));
        assert!(first.contains("agent_settled"));
        assert!(!path.starts_with(temp.path().join(".pi/agent/extensions")));
        configure(&mut environment, "internal:TWO")?;
        assert_eq!(fs::read_to_string(&path)?, first);
        assert_eq!(
            environment["DLGT_PI_EXTENSION"],
            path.to_string_lossy().into_owned()
        );

        fs::write(&path, "export default function () {}\n")?;
        let foreign = fs::read_to_string(&path)?;
        assert!(configure(&mut environment, "internal:THREE").is_err());
        assert_eq!(fs::read_to_string(&path)?, foreign);
        Ok(())
    }
}
