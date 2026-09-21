//! Cursor's interactive CLI adapter. No ACP, print mode, or desktop automation.
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::provider::{CommandSpec, LaunchOptions};

// A shared, inert bridge: only dlgt children receive these variables. Never
// embed a build path or a Session ID in the user's persistent configuration.
const BRIDGE: &str = "if [ -n \"${DLGT_CURSOR_LAUNCH:-}\" ] && [ -n \"${DLGT_CURSOR_HOOK_BIN:-}\" ]; then \"$DLGT_CURSOR_HOOK_BIN\" hook emit \"$DLGT_CURSOR_LAUNCH\" cursor; fi";
const EVENTS: &[&str] = &["beforeSubmitPrompt", "afterAgentResponse", "stop"];

pub fn program() -> PathBuf {
    std::env::var_os("DLGT_CURSOR_BIN").map_or_else(|| PathBuf::from("cursor-agent"), PathBuf::from)
}

pub fn configure(environment: &mut HashMap<String, String>, launch: &str) -> Result<()> {
    let home = environment
        .get("HOME")
        .context("Cursor launch requires HOME")?;
    install_hooks(&Path::new(home).join(".cursor"))?;
    environment.insert("DLGT_CURSOR_LAUNCH".to_owned(), launch.to_owned());
    environment.insert(
        "DLGT_CURSOR_HOOK_BIN".to_owned(),
        std::env::current_exe()?.to_string_lossy().into_owned(),
    );
    environment.insert(
        "DLGT_SOCKET".to_owned(),
        crate::paths::socket_path()?.to_string_lossy().into_owned(),
    );
    Ok(())
}

fn install_hooks(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    // Daemons from different versions may register at the same time.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("dlgt-hooks.lock"))?;
    // SAFETY: lock owns a valid file descriptor; closing it releases flock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock Cursor hooks");
    }
    let path = directory.join("hooks.json");
    let mut config = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .context("invalid Cursor hooks.json; left unchanged")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"version":1,"hooks":{}})
        }
        Err(error) => return Err(error.into()),
    };
    let changed = merge_hooks(&mut config)?;
    if changed {
        crate::provider::write_config_atomic(&path, &serde_json::to_string_pretty(&config)?)?;
    }
    Ok(())
}

fn merge_hooks(config: &mut Value) -> Result<bool> {
    let root = config
        .as_object_mut()
        .context("Cursor hooks config must be an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    let mut changed = false;
    for event in EVENTS {
        let handlers = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Cursor hook handlers must be an array")?;
        if !handlers
            .iter()
            .any(|handler| handler.get("command").and_then(Value::as_str) == Some(BRIDGE))
        {
            handlers.push(json!({"command":BRIDGE,"timeout":5}));
            changed = true;
        }
    }
    Ok(changed)
}

pub fn validate(effort: Option<&str>, options: &[String]) -> Result<()> {
    if effort.is_some() {
        bail!("Cursor does not support --effort; choose a model with --model");
    }
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if !matches!(
            (key, value),
            ("mode", "plan" | "ask") | ("sandbox", "enabled" | "disabled")
        ) {
            bail!(
                "invalid Cursor harness option {key:?}; use mode=plan|ask or sandbox=enabled|disabled"
            );
        }
    }
    Ok(())
}

pub fn command(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    validate(options.effort, options.harness_options)?;
    let mut args = Vec::new();
    if let Some(model) = options.model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(id) = options.resume_provider_id {
        args.extend(["--resume".to_owned(), id.to_owned()]);
    }
    if options.auto_approve {
        args.extend(["--trust".to_owned(), "--force".to_owned()]);
    }
    for option in options.harness_options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        args.push(format!("--{key}={value}"));
    }
    if let Some(prompt) = options.initial_prompt {
        options.agent.semantic_input(prompt)?;
        args.extend(["--".to_owned(), prompt.to_owned()]);
    }
    Ok(CommandSpec {
        program: program(),
        args,
        cwd: options.cwd.to_path_buf(),
        environment: options.environment.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_launch_is_interactive_and_managed_options_cannot_be_overridden() -> Result<()> {
        let environment = HashMap::new();
        let mut options = LaunchOptions {
            agent: crate::provider::Agent::Cursor,
            session_id: "internal:TEST",
            title: "review",
            cwd: Path::new("/tmp"),
            model: Some("chosen-model"),
            effort: None,
            harness_options: &[],
            new_provider_id: None,
            resume_provider_id: Some("chat-1"),
            environment: &environment,
            auto_approve: false,
            initial_prompt: Some("--literal prompt"),
        };
        let spec = command(&options)?;
        assert_eq!(
            spec.args,
            [
                "--model",
                "chosen-model",
                "--resume",
                "chat-1",
                "--",
                "--literal prompt"
            ]
        );
        options.auto_approve = true;
        assert!(command(&options)?.args.contains(&"--force".to_owned()));
        assert!(validate(Some("low"), &[]).is_err());
        for option in [
            "print=true",
            "resume=other",
            "workspace=/other",
            "plugin-dir=/other",
            "api-key=secret",
            "mode=invalid",
        ] {
            assert!(validate(None, &[option.to_owned()]).is_err());
        }
        assert!(validate(None, &["mode=ask".to_owned(), "sandbox=enabled".to_owned()]).is_ok());
        Ok(())
    }

    #[test]
    fn registration_preserves_hooks_and_is_idempotent() -> Result<()> {
        let mut config = json!({"version":1,"other":"preserved","hooks":{
            "stop":[{"command":"existing-stop","loop_limit":2}],
            "beforeReadFile":[{"command":"policy"}]}});
        assert!(merge_hooks(&mut config)?);
        assert_eq!(config["hooks"]["stop"][0]["command"], "existing-stop");
        assert_eq!(config["hooks"]["beforeReadFile"][0]["command"], "policy");
        assert_eq!(config["other"], "preserved");
        let snapshot = config.clone();
        assert!(!merge_hooks(&mut config)?);
        assert_eq!(config, snapshot);
        Ok(())
    }

    #[test]
    fn invalid_existing_hooks_are_not_overwritten() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("hooks.json");
        let original = r#"{"hooks":{"stop":"invalid"}}"#;
        fs::write(&path, original)?;
        assert!(install_hooks(temp.path()).is_err());
        assert_eq!(fs::read_to_string(path)?, original);
        Ok(())
    }
}
