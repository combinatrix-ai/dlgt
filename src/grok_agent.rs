//! Grok Build interactive TUI adapter.
//!
//! Uses Claude-shaped lifecycle hooks (`SessionStart` / `UserPromptSubmit` /
//! `Stop` / …) installed under `~/.grok/hooks/`. Passive hook stdout is discarded
//! by Grok, but the command still runs, so dlgt can observe turn completion
//! the same way it does for Claude. ACP is not used here.
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::provider::{CommandSpec, LaunchOptions};

const BRIDGE: &str = "if [ -n \"${DLGT_GROK_LAUNCH:-}\" ] && [ -n \"${DLGT_GROK_HOOK_BIN:-}\" ]; then \"$DLGT_GROK_HOOK_BIN\" hook emit \"$DLGT_GROK_LAUNCH\" grok; fi";

const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Notification",
    "SessionEnd",
];

/// CamelCase → `snake_case` fields that Claude-shaped dlgt handlers expect.
const PAYLOAD_ALIASES: &[(&str, &str)] = &[
    ("hookEventName", "hook_event_name"),
    ("sessionId", "session_id"),
    ("workspaceRoot", "cwd"),
    ("userPrompt", "prompt"),
    ("lastAssistantMessage", "last_assistant_message"),
    ("turnId", "turn_id"),
    ("transcriptPath", "transcript_path"),
    ("notificationType", "notification_type"),
];

pub fn program() -> PathBuf {
    std::env::var_os("DLGT_GROK_BIN").map_or_else(|| PathBuf::from("grok"), PathBuf::from)
}

pub fn configure(environment: &mut HashMap<String, String>, launch: &str) -> Result<()> {
    let home = environment
        .get("HOME")
        .context("Grok launch requires HOME")?;
    install_hooks(&Path::new(home).join(".grok"))?;
    environment.insert("DLGT_GROK_LAUNCH".to_owned(), launch.to_owned());
    environment.insert(
        "DLGT_GROK_HOOK_BIN".to_owned(),
        std::env::current_exe()?.to_string_lossy().into_owned(),
    );
    environment.insert(
        "DLGT_SOCKET".to_owned(),
        crate::paths::socket_path()?.to_string_lossy().into_owned(),
    );
    Ok(())
}

fn install_hooks(directory: &Path) -> Result<()> {
    let hooks_dir = directory.join("hooks");
    fs::create_dir_all(&hooks_dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("dlgt-hooks.lock"))?;
    // SAFETY: lock owns a valid file descriptor; closing it releases flock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot lock Grok hooks");
    }
    let path = hooks_dir.join("dlgt.json");
    let mut config = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .context("invalid Grok dlgt.json hooks; left unchanged")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"hooks":{}})
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
        .context("Grok hooks config must be an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Grok hooks must be an object")?;
    let mut changed = false;
    for event in EVENTS {
        let matchers = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Grok hook matchers must be an array")?;
        let already = matchers.iter().any(|matcher| {
            matcher
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|handlers| {
                    handlers.iter().any(|handler| {
                        handler.get("command").and_then(Value::as_str) == Some(BRIDGE)
                    })
                })
        });
        if !already {
            matchers.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": BRIDGE,
                    "timeout": 5
                }]
            }));
            changed = true;
        }
    }
    Ok(changed)
}

/// Copy Grok camelCase fields onto the `snake_case` names Claude handlers use.
pub fn normalize_payload(payload: &Value) -> Value {
    let mut object = match payload.as_object() {
        Some(object) => object.clone(),
        None => return payload.clone(),
    };
    for (from, to) in PAYLOAD_ALIASES {
        if object.get(*to).is_none()
            && let Some(value) = object.get(*from).cloned()
        {
            object.insert((*to).to_owned(), value);
        }
    }
    // Prefer workspace cwd when both exist and `cwd` was only an alias target.
    if object.get("cwd").and_then(Value::as_str).is_none()
        && let Some(root) = object.get("workspaceRoot").cloned()
    {
        object.insert("cwd".to_owned(), root);
    }
    Value::Object(object)
}

pub fn validate(options: &[String]) -> Result<()> {
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key.is_empty() || value.is_empty() {
            bail!("harness option {key:?} requires a non-empty value");
        }
        if matches!(
            key,
            "model" | "resume" | "session-id" | "always-approve" | "trust" | "effort" | "cwd"
        ) {
            bail!("harness option {key:?} is managed by dlgt");
        }
    }
    Ok(())
}

pub fn command(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    validate(options.harness_options)?;
    let mut args = Vec::new();
    if let Some(model) = options.model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = options.effort {
        args.extend(["--effort".to_owned(), effort.to_owned()]);
    }
    if options.auto_approve {
        args.push("--always-approve".to_owned());
        args.push("--trust".to_owned());
    }
    for option in options.harness_options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        args.push(format!("--{key}={value}"));
    }
    if let Some(provider_id) = options.resume_provider_id {
        args.extend(["--resume".to_owned(), provider_id.to_owned()]);
    } else if let Some(provider_id) = options.new_provider_id {
        args.extend(["--session-id".to_owned(), provider_id.to_owned()]);
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
    fn normalize_payload_adds_snake_case_aliases() {
        let payload = json!({
            "hookEventName": "Stop",
            "sessionId": "abc",
            "lastAssistantMessage": "done",
            "workspaceRoot": "/tmp/proj"
        });
        let normalized = normalize_payload(&payload);
        assert_eq!(normalized["hook_event_name"], "Stop");
        assert_eq!(normalized["session_id"], "abc");
        assert_eq!(normalized["last_assistant_message"], "done");
        assert_eq!(normalized["cwd"], "/tmp/proj");
    }

    #[test]
    fn registration_preserves_hooks_and_is_idempotent() -> Result<()> {
        let mut config = json!({"hooks":{
            "Stop":[{"hooks":[{"type":"command","command":"existing","timeout":3}]}],
            "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"policy"}]}]
        }});
        assert!(merge_hooks(&mut config)?);
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "existing"
        );
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "policy"
        );
        let snapshot = config.clone();
        assert!(!merge_hooks(&mut config)?);
        assert_eq!(config, snapshot);
        Ok(())
    }

    #[test]
    fn command_defaults_to_auto_approve_and_rejects_managed_options() -> Result<()> {
        let environment = HashMap::new();
        let options = LaunchOptions {
            agent: crate::provider::Agent::Grok,
            session_id: "internal:TEST",
            title: "review",
            cwd: Path::new("/tmp"),
            model: Some("grok-4"),
            effort: Some("high"),
            harness_options: &[],
            new_provider_id: None,
            resume_provider_id: Some("session-1"),
            environment: &environment,
            auto_approve: true,
            initial_prompt: None,
        };
        let spec = command(&options)?;
        assert_eq!(
            spec.args,
            [
                "--model",
                "grok-4",
                "--effort",
                "high",
                "--always-approve",
                "--trust",
                "--resume",
                "session-1"
            ]
        );
        assert!(validate(&["model=x".to_owned()]).is_err());
        Ok(())
    }

    #[test]
    fn invalid_existing_hooks_are_not_overwritten() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("hooks"))?;
        let path = temp.path().join("hooks/dlgt.json");
        let original = r#"{"hooks":{"Stop":"invalid"}}"#;
        fs::write(&path, original)?;
        assert!(install_hooks(temp.path()).is_err());
        assert_eq!(fs::read_to_string(path)?, original);
        Ok(())
    }
}
