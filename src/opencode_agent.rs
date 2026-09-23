//! Interactive `opencode` TUI adapter.
//!
//! `opencode run` exits after one prompt, so it cannot host a live Session.
//! The TUI stays up in the dlgt PTY. A plugin under the user config
//! directory translates `session.*` and `chat.message` events into the
//! same Claude-shaped hooks Grok uses. The plugin is inert unless
//! `DLGT_OPENCODE_LAUNCH` is set, which only dlgt children receive.
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::provider::{CommandSpec, LaunchOptions};

const PLUGIN: &str = include_str!("opencode_plugin.js");
const MARKER: &str = "DLGT_OPENCODE_LAUNCH";

pub fn program() -> PathBuf {
    std::env::var_os("DLGT_OPENCODE_BIN").map_or_else(|| PathBuf::from("opencode"), PathBuf::from)
}

pub fn configure(
    environment: &mut HashMap<String, String>,
    launch: &str,
    resume_provider_id: Option<&str>,
) -> Result<()> {
    let home = environment
        .get("HOME")
        .context("OpenCode launch requires HOME")?;
    let config_home = match environment.get("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => Path::new(home).join(".config"),
    };
    install_plugin(&config_home.join("opencode"))?;
    environment.insert("DLGT_OPENCODE_LAUNCH".to_owned(), launch.to_owned());
    environment.insert(
        "DLGT_OPENCODE_HOOK_BIN".to_owned(),
        std::env::current_exe()?.to_string_lossy().into_owned(),
    );
    environment.insert(
        "DLGT_SOCKET".to_owned(),
        crate::paths::socket_path()?.to_string_lossy().into_owned(),
    );
    // The plugin worker does not receive CLI arguments, and a resumed TUI
    // emits no session event until the next prompt. The id has to travel in
    // the environment or readiness waits until the startup timeout.
    if let Some(id) = resume_provider_id.filter(|id| !id.is_empty()) {
        environment.insert("DLGT_OPENCODE_RESUME".to_owned(), id.to_owned());
    } else {
        environment.remove("DLGT_OPENCODE_RESUME");
    }
    Ok(())
}

fn install_plugin(directory: &Path) -> Result<()> {
    let plugins = directory.join("plugins");
    fs::create_dir_all(&plugins)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("dlgt-plugin.lock"))?;
    // SAFETY: lock owns a valid file descriptor; closing it releases flock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock OpenCode plugin");
    }
    let path = plugins.join("dlgt.js");
    match fs::read_to_string(&path) {
        Ok(existing) if existing == PLUGIN => return Ok(()),
        Ok(existing) if !existing.contains(MARKER) => {
            bail!(
                "OpenCode plugin {} is not the dlgt bridge; left unchanged",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    crate::provider::write_config_atomic(&path, PLUGIN)?;
    Ok(())
}

pub fn validate(effort: Option<&str>, options: &[String]) -> Result<()> {
    if effort.is_some() {
        bail!("OpenCode does not support --effort; choose a model with --model");
    }
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key != "agent" || value.is_empty() || value.starts_with('-') {
            bail!("invalid OpenCode harness option {key:?}; use agent=<name>");
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
    if options.auto_approve {
        args.push("--auto".to_owned());
    }
    for option in options.harness_options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key == "agent" {
            args.extend(["--agent".to_owned(), value.to_owned()]);
        }
    }
    if let Some(id) = options.resume_provider_id {
        reject_flag_like("session", id)?;
        args.extend(["--session".to_owned(), id.to_owned()]);
    }
    // A new TUI does not create a session until this prompt exists. Resume
    // leaves it unset: `opencode --session` ignores `--prompt`, and dlgt
    // pastes after SessionStart instead.
    if let Some(prompt) = options.initial_prompt {
        options.agent.semantic_input(prompt)?;
        args.extend(["--prompt".to_owned(), prompt.to_owned()]);
    }
    Ok(CommandSpec {
        program: program(),
        args,
        cwd: options.cwd.to_path_buf(),
        environment: options.environment.clone(),
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
    fn command_passes_the_launch_prompt_and_resumes_a_session() -> Result<()> {
        let environment = HashMap::new();
        let mut options = LaunchOptions {
            agent: crate::provider::Agent::OpenCode,
            session_id: "internal:TEST",
            title: "review",
            cwd: Path::new("/tmp"),
            model: Some("xai/grok-4.7"),
            effort: None,
            harness_options: &["agent=build".to_owned()],
            new_provider_id: None,
            resume_provider_id: Some("ses_abc"),
            environment: &environment,
            auto_approve: true,
            initial_prompt: Some("Review the change"),
        };
        let spec = command(&options)?;
        assert_eq!(
            spec.args,
            [
                "--model",
                "xai/grok-4.7",
                "--auto",
                "--agent",
                "build",
                "--session",
                "ses_abc",
                "--prompt",
                "Review the change"
            ]
        );
        options.auto_approve = false;
        options.harness_options = &[];
        options.resume_provider_id = None;
        options.initial_prompt = None;
        let plain = command(&options)?;
        assert_eq!(plain.args, ["--model", "xai/grok-4.7"]);
        assert!(validate(Some("high"), &[]).is_err());
        assert!(validate(None, &["model=x".to_owned()]).is_err());
        assert!(validate(None, &["agent=build".to_owned()]).is_ok());
        Ok(())
    }

    #[test]
    fn plugin_install_is_idempotent_and_preserves_foreign_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut environment = HashMap::from([(
            "HOME".to_owned(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        configure(&mut environment, "internal:ONE", None)?;
        let path = temp.path().join(".config/opencode/plugins/dlgt.js");
        let first = fs::read_to_string(&path)?;
        assert!(first.contains(MARKER));
        assert!(first.contains("session.idle"));
        configure(&mut environment, "internal:TWO", None)?;
        assert_eq!(fs::read_to_string(&path)?, first);
        assert_eq!(environment["DLGT_OPENCODE_LAUNCH"], "internal:TWO");
        assert!(!environment.contains_key("DLGT_OPENCODE_RESUME"));
        environment.insert("DLGT_OPENCODE_RESUME".to_owned(), "stale".to_owned());
        configure(&mut environment, "internal:TWO", Some("ses_abc"))?;
        assert_eq!(environment["DLGT_OPENCODE_RESUME"], "ses_abc");
        assert!(first.contains("DLGT_OPENCODE_RESUME"));

        fs::write(&path, "export const DlgtPlugin = async () => ({});\n")?;
        let foreign = fs::read_to_string(&path)?;
        assert!(configure(&mut environment, "internal:THREE", None).is_err());
        assert_eq!(fs::read_to_string(&path)?, foreign);
        Ok(())
    }
}
