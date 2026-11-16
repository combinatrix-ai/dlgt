use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::protocol::ApprovalsMode;
use toml_edit::{DocumentMut, Item, Table, value};
use uuid::Uuid;

static CODEX_CONFIG_LOCK: Mutex<()> = Mutex::new(());
static CLAUDE_STATE_LOCK: Mutex<()> = Mutex::new(());

const CODEX_UPDATE_SUPPRESSION: &str = "check_for_update_on_startup=false";
const CLAUDE_AUTOUPDATER_ENV: &str = "DISABLE_AUTOUPDATER";

const CLAUDE_SESSION_MARKER_HEX_LEN: usize = 12;

fn hex_provider_id(provider_id: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(provider_id.len() * 2);
    for byte in provider_id.as_bytes() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    encoded
}

/// Return the short, stable marker shown in a Claude provider title.
pub fn claude_session_marker(provider_id: &str) -> String {
    let mut suffix = Uuid::parse_str(provider_id).map_or_else(
        |_| hex_provider_id(provider_id),
        |id| id.simple().to_string(),
    );
    suffix.truncate(CLAUDE_SESSION_MARKER_HEX_LEN);
    format!("dlgt:{suffix}")
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
}

impl Agent {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => bail!("unsupported agent {value:?}; expected codex or claude"),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// The provider's own command for opening a conversation by hand.
    pub fn resume_command(self, provider_id: &str) -> String {
        match self {
            Self::Codex => format!("codex resume {provider_id}"),
            Self::Claude => format!("claude --resume {provider_id}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchOptions<'a> {
    pub agent: Agent,
    pub title: &'a str,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub harness_options: &'a [String],
    /// Provider ID assigned to a newly-created Claude conversation.
    pub new_provider_id: Option<&'a str>,
    pub resume_provider_id: Option<&'a str>,
    pub environment: &'a HashMap<String, String>,
    pub approvals: ApprovalsMode,
}

pub(crate) fn provider_display_name(title: &str) -> String {
    format!("[dlgt] {title}")
}

fn claude_provider_display_name(options: &LaunchOptions<'_>) -> String {
    let Some(provider_id) = options.resume_provider_id.or(options.new_provider_id) else {
        return provider_display_name(options.title);
    };
    format!(
        "{} [{}]",
        provider_display_name(options.title),
        claude_session_marker(provider_id)
    )
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: HashMap<String, String>,
}

pub fn codex_program() -> PathBuf {
    std::env::var_os("DLGT_CODEX_BIN").map_or_else(|| PathBuf::from("codex"), PathBuf::from)
}

/// Arguments for the app-server process that owns one Session.
pub(crate) fn codex_session_args(options: &LaunchOptions<'_>) -> Result<Vec<String>> {
    let mut args = vec!["--config".to_owned(), CODEX_UPDATE_SUPPRESSION.to_owned()];
    if let Some(effort) = options.effort {
        args.extend([
            "--config".to_owned(),
            format!("model_reasoning_effort={}", toml_string(effort)),
        ]);
    }
    args.extend(codex_harness_args(options.harness_options)?);
    args.push("app-server".to_owned());
    Ok(args)
}

/// Arguments for an app-server that only answers discovery requests.
pub(crate) fn codex_discovery_args() -> Vec<String> {
    vec![
        "--config".to_owned(),
        CODEX_UPDATE_SUPPRESSION.to_owned(),
        "app-server".to_owned(),
    ]
}

/// `thread/start` or `thread/resume` parameters, including who approves.
pub(crate) fn codex_thread_params(options: &LaunchOptions<'_>) -> serde_json::Value {
    let (policy, sandbox, reviewer) = match options.approvals {
        ApprovalsMode::Auto => ("on-request", "workspace-write", "auto_review"),
        ApprovalsMode::Ask => ("on-request", "workspace-write", "user"),
        ApprovalsMode::Bypass => ("never", "danger-full-access", "user"),
    };
    let mut params = serde_json::json!({
        "cwd": options.cwd,
        "approvalPolicy": policy,
        "sandbox": sandbox,
        "approvalsReviewer": reviewer,
    });
    if let Some(model) = options.model {
        params["model"] = serde_json::json!(model);
    }
    if let Some(thread_id) = options.resume_provider_id {
        params["threadId"] = serde_json::json!(thread_id);
    }
    params
}

pub fn prepare_workspace(agent: Agent, cwd: &Path) -> Result<()> {
    match agent {
        Agent::Codex => {
            let home = std::env::var_os("CODEX_HOME").map_or_else(
                || {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|home| home.join(".codex"))
                        .context("HOME is not set")
                },
                |home| Ok(PathBuf::from(home)),
            )?;
            trust_codex_workspace(&home, cwd)?;
        }
        Agent::Claude => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")?;
            trust_claude_workspace(&home, cwd)?;
        }
    }
    Ok(())
}

fn trust_claude_workspace(home: &Path, cwd: &Path) -> Result<()> {
    let _guard = CLAUDE_STATE_LOCK
        .lock()
        .map_err(|_| anyhow!("Claude state lock poisoned"))?;
    fs::create_dir_all(home).with_context(|| format!("failed to create {}", home.display()))?;
    let state_path = home.join(".claude.json");
    let existing = match fs::read_to_string(&state_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", state_path.display()));
        }
    };
    let mut state = serde_json::from_str::<serde_json::Value>(&existing)
        .with_context(|| format!("failed to parse {}", state_path.display()))?;
    let root = state
        .as_object_mut()
        .context("Claude state root must be an object")?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .context("Claude state `projects` must be an object")?;
    let cwd = cwd
        .to_str()
        .context("Claude workspace path is not valid UTF-8")?;
    let project = projects
        .entry(cwd)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| format!("Claude state project {cwd:?} must be an object"))?;
    let is_trusted = project
        .get("hasTrustDialogAccepted")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let onboarding_seen = project
        .get("projectOnboardingSeenCount")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|count| count >= 1);
    if is_trusted && onboarding_seen {
        return Ok(());
    }
    project.insert(
        "hasTrustDialogAccepted".to_owned(),
        serde_json::Value::Bool(true),
    );
    if !onboarding_seen {
        project.insert(
            "projectOnboardingSeenCount".to_owned(),
            serde_json::Value::Number(1.into()),
        );
    }
    let mut contents =
        serde_json::to_string_pretty(&state).context("failed to serialize Claude state")?;
    contents.push('\n');
    write_config_atomic(&state_path, &contents)
}

fn trust_codex_workspace(codex_home: &Path, cwd: &Path) -> Result<()> {
    let _guard = CODEX_CONFIG_LOCK
        .lock()
        .map_err(|_| anyhow!("Codex config lock poisoned"))?;
    fs::create_dir_all(codex_home)
        .with_context(|| format!("failed to create {}", codex_home.display()))?;
    let config_path = codex_home.join("config.toml");
    let existing = match fs::read_to_string(&config_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let mut document = existing
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let cwd = cwd
        .to_str()
        .context("Codex workspace path is not valid UTF-8")?;
    if project_is_trusted(&document, cwd) {
        return Ok(());
    }
    set_project_trusted(&mut document, cwd)?;
    write_config_atomic(&config_path, &document.to_string())
}

fn set_project_trusted(document: &mut DocumentMut, cwd: &str) -> Result<()> {
    if document.get("projects").is_none() {
        document["projects"] = Item::Table(Table::new());
    }
    let projects = document
        .get_mut("projects")
        .and_then(Item::as_table_like_mut)
        .context("Codex config `projects` must be a table")?;
    if projects.get(cwd).is_none() {
        projects.insert(cwd, Item::Table(Table::new()));
    }
    let project = projects
        .get_mut(cwd)
        .and_then(Item::as_table_like_mut)
        .with_context(|| format!("Codex config project {cwd:?} must be a table"))?;
    project.insert("trust_level", value("trusted"));
    Ok(())
}

fn project_is_trusted(document: &DocumentMut, cwd: &str) -> bool {
    document
        .get("projects")
        .and_then(Item::as_table_like)
        .and_then(|projects| projects.get(cwd))
        .and_then(Item::as_table_like)
        .and_then(|project| project.get("trust_level"))
        .and_then(Item::as_str)
        == Some("trusted")
}

pub(crate) fn write_config_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().context("config path has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("config file name is not valid UTF-8")?;
    let temporary = parent.join(format!(".{name}.dlgt-{}.tmp", Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        } else {
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        OpenOptions::new()
            .read(true)
            .open(parent)
            .with_context(|| format!("failed to open {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn claude_command(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    let program =
        std::env::var_os("DLGT_CLAUDE_BIN").map_or_else(|| PathBuf::from("claude"), PathBuf::from);
    let mut args = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
        "--name",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(claude_provider_display_name(options));
    if let Some(model) = options.model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = options.effort {
        args.extend(["--effort".to_owned(), effort.to_owned()]);
    }
    args.extend(claude_harness_args(options.harness_options)?);
    match options.approvals {
        ApprovalsMode::Auto => args.extend(["--permission-mode".to_owned(), "auto".to_owned()]),
        ApprovalsMode::Ask => args.extend(["--permission-mode".to_owned(), "manual".to_owned()]),
        ApprovalsMode::Bypass => args.push("--dangerously-skip-permissions".to_owned()),
    }
    if let Some(provider_id) = options.resume_provider_id {
        args.extend(["--resume".to_owned(), provider_id.to_owned()]);
    } else if let Some(provider_id) = options.new_provider_id {
        args.extend(["--session-id".to_owned(), provider_id.to_owned()]);
    }
    Ok(CommandSpec {
        program,
        args,
        cwd: options.cwd.to_path_buf(),
        environment: claude_environment(options.environment),
    })
}

/// Confirm a Claude subscription login without inheriting provider-routing
/// overrides or exposing the command's potentially sensitive output.
pub fn verify_claude_subscription_login() -> Result<()> {
    let program =
        std::env::var_os("DLGT_CLAUDE_BIN").map_or_else(|| PathBuf::from("claude"), PathBuf::from);
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let mut command = std::process::Command::new(program);
    command
        .args(["auth", "status"])
        .env_clear()
        .env("HOME", home);
    for name in ["PATH", "USER", "TERM"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command.output().map_err(|_| crate::config::AuthMismatch)?;
    if !output.status.success() {
        return Err(crate::config::AuthMismatch.into());
    }
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Err(crate::config::AuthMismatch.into());
    };
    let confirmed = status.get("loggedIn").and_then(serde_json::Value::as_bool) == Some(true)
        && status.get("authMethod").and_then(serde_json::Value::as_str) == Some("claude.ai");
    if !confirmed {
        return Err(crate::config::AuthMismatch.into());
    }
    Ok(())
}

fn claude_harness_args(options: &[String]) -> Result<Vec<String>> {
    options
        .iter()
        .map(|option| {
            let (key, value) = option
                .split_once('=')
                .map_or((option.as_str(), None), |(key, value)| (key, Some(value)));
            if key.is_empty()
                || !key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
            {
                bail!("invalid harness option key {key:?}");
            }
            if value.is_some_and(str::is_empty) {
                bail!("harness option {key:?} requires a non-empty value after '='");
            }
            if matches!(
                key,
                "name"
                    | "model"
                    | "effort"
                    | "resume"
                    | "continue"
                    | "session-id"
                    | "print"
                    | "input-format"
                    | "output-format"
                    | "verbose"
                    | "replay-user-messages"
                    | "permission-mode"
                    | "permission-prompt-tool"
                    | "permission-prompts"
                    | "dangerously-skip-permissions"
                    | "allow-dangerously-skip-permissions"
                    | "allowedTools"
                    | "allowed-tools"
                    | "settings"
                    | "settings-file"
                    | "bare"
            ) {
                bail!("harness option {key:?} is managed by dlgt");
            }
            Ok(value.map_or_else(|| format!("--{key}"), |value| format!("--{key}={value}")))
        })
        .collect()
}

fn codex_harness_args(options: &[String]) -> Result<Vec<String>> {
    let mut args = Vec::with_capacity(options.len() * 2);
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("harness option requires KEY=VALUE")?;
        if key.is_empty()
            || !key.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
        {
            bail!("invalid harness option key {key:?}");
        }
        if value.is_empty() {
            bail!("harness option {key:?} requires a non-empty value");
        }
        if matches!(
            key,
            "check_for_update_on_startup"
                | "model"
                | "model_reasoning_effort"
                | "approval_policy"
                | "sandbox_mode"
                | "approvals_reviewer"
                | "sandbox_workspace_write"
        ) {
            bail!("harness option {key:?} is managed by dlgt");
        }
        if key.starts_with("sandbox_workspace_write.") {
            bail!("harness option {key:?} is managed by dlgt");
        }
        args.extend(["--config".to_owned(), option.clone()]);
    }
    Ok(args)
}

fn claude_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    let mut environment = environment.clone();
    environment.insert(CLAUDE_AUTOUPDATER_ENV.to_owned(), "1".to_owned());
    environment
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use serde_json::json;

    use super::{
        Agent, LaunchOptions, claude_command, claude_harness_args, claude_session_marker,
        codex_harness_args, codex_session_args, codex_thread_params, trust_claude_workspace,
        trust_codex_workspace,
    };
    use crate::protocol::ApprovalsMode;

    fn options<'a>(
        agent: Agent,
        approvals: ApprovalsMode,
        environment: &'a HashMap<String, String>,
        harness_options: &'a [String],
    ) -> LaunchOptions<'a> {
        LaunchOptions {
            agent,
            title: "review",
            cwd: Path::new("/work"),
            model: Some("m1"),
            effort: Some("high"),
            harness_options,
            new_provider_id: Some("9b634cb7-2e3e-437b-96cb-8199ce17afa6"),
            resume_provider_id: None,
            environment,
            approvals,
        }
    }

    fn claude_args(approvals: ApprovalsMode) -> Vec<String> {
        let environment = HashMap::new();
        claude_command(&options(Agent::Claude, approvals, &environment, &[]))
            .unwrap_or_else(|error| panic!("{error}"))
            .args
    }

    fn has_pair(args: &[String], key: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == key && pair[1] == value)
    }

    #[test]
    fn claude_marker_is_stable_and_short() {
        let marker = claude_session_marker("9b634cb7-2e3e-437b-96cb-8199ce17afa6");
        assert_eq!(marker, "dlgt:9b634cb72e3e");
        assert_eq!(claude_session_marker("thread-1"), "dlgt:746872656164");
    }

    #[test]
    fn claude_runs_as_one_stream_json_process_that_asks_over_stdio() {
        let args = claude_args(ApprovalsMode::Auto);
        assert_eq!(args[0], "-p");
        for (key, value) in [
            ("--input-format", "stream-json"),
            ("--output-format", "stream-json"),
            ("--permission-prompt-tool", "stdio"),
            ("--model", "m1"),
            ("--effort", "high"),
            ("--session-id", "9b634cb7-2e3e-437b-96cb-8199ce17afa6"),
            ("--name", "[dlgt] review [dlgt:9b634cb72e3e]"),
        ] {
            assert!(
                has_pair(&args, key, value),
                "missing {key} {value}: {args:?}"
            );
        }
        assert!(args.iter().any(|arg| arg == "--replay-user-messages"));
        assert!(!args.iter().any(|arg| arg == "--settings"));
    }

    #[test]
    fn claude_approvals_map_to_permission_modes() {
        assert!(has_pair(
            &claude_args(ApprovalsMode::Auto),
            "--permission-mode",
            "auto"
        ));
        assert!(has_pair(
            &claude_args(ApprovalsMode::Ask),
            "--permission-mode",
            "manual"
        ));
        let bypass = claude_args(ApprovalsMode::Bypass);
        assert!(
            bypass
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
        assert!(!bypass.iter().any(|arg| arg == "--permission-mode"));
        for mode in [ApprovalsMode::Auto, ApprovalsMode::Ask] {
            assert!(
                !claude_args(mode)
                    .iter()
                    .any(|arg| arg == "--dangerously-skip-permissions")
            );
        }
    }

    #[test]
    fn claude_resume_reopens_the_stored_conversation() {
        let environment = HashMap::new();
        let mut options = options(Agent::Claude, ApprovalsMode::Auto, &environment, &[]);
        options.resume_provider_id = Some("old-session");
        let args = claude_command(&options)
            .unwrap_or_else(|error| panic!("{error}"))
            .args;
        assert!(has_pair(&args, "--resume", "old-session"));
        assert!(!args.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn claude_disables_its_autoupdater_without_touching_other_variables() {
        let environment = HashMap::from([("KEEP".to_owned(), "1".to_owned())]);
        let spec = claude_command(&options(
            Agent::Claude,
            ApprovalsMode::Auto,
            &environment,
            &[],
        ))
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            spec.environment
                .get("DISABLE_AUTOUPDATER")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(spec.environment.get("KEEP").map(String::as_str), Some("1"));
    }

    #[test]
    fn claude_passes_explicit_harness_options() {
        assert_eq!(
            claude_harness_args(&["chrome".to_owned(), "max-turns=3".to_owned()])
                .unwrap_or_else(|error| panic!("{error}")),
            ["--chrome", "--max-turns=3"]
        );
    }

    #[test]
    fn profile_permission_overrides_are_rejected_under_every_approval_mode() -> anyhow::Result<()> {
        let config = crate::config::parse(
            r#"
[profiles.unsafe_claude]
harness = "claude"
harness_options = ["allowedTools=Bash(*)"]

[profiles.unsafe_codex]
harness = "codex"
harness_options = ["approval_policy=never"]
"#,
        )?;
        let environment = HashMap::new();
        let client_environment = std::collections::BTreeMap::new();

        for approvals in [
            ApprovalsMode::Auto,
            ApprovalsMode::Ask,
            ApprovalsMode::Bypass,
        ] {
            let explicit = crate::config::PartialLaunch {
                approvals: Some(approvals),
                ..crate::config::PartialLaunch::default()
            };
            for (profile_name, harness, option) in [
                ("unsafe_claude", "claude", "allowedTools"),
                ("unsafe_codex", "codex", "approval_policy"),
            ] {
                let resolved = config.resolve(
                    harness,
                    Some(config.profile(profile_name)?),
                    &explicit,
                    &client_environment,
                )?;
                let launch = options(
                    Agent::parse(harness)?,
                    approvals,
                    &environment,
                    &resolved.harness_options,
                );
                let result = if harness == "claude" {
                    claude_command(&launch).map(|_| ())
                } else {
                    codex_session_args(&launch).map(|_| ())
                };
                let Err(error) = result else {
                    anyhow::bail!("unsafe {harness} Profile option was accepted");
                };
                assert!(error.to_string().contains(option), "{error:#}");
            }
        }
        Ok(())
    }

    #[test]
    fn claude_rejects_empty_and_managed_harness_options() {
        for option in [
            "",
            "=value",
            "max-turns=",
            "name=x",
            "model=x",
            "resume=x",
            "session-id=x",
            "permission-mode=plan",
            "permission-prompt-tool=x",
            "permission-prompts=none",
            "allowedTools=Bash",
            "allowed-tools=Bash",
            "settings={}",
            "settings-file=permissions.json",
            "bare",
            "dangerously-skip-permissions",
            "output-format=text",
        ] {
            assert!(
                claude_harness_args(&[option.to_owned()]).is_err(),
                "{option:?} was accepted"
            );
        }
    }

    #[test]
    fn codex_approvals_map_to_thread_parameters() {
        let environment = HashMap::new();
        let params =
            |approvals| codex_thread_params(&options(Agent::Codex, approvals, &environment, &[]));
        let auto = params(ApprovalsMode::Auto);
        assert_eq!(auto["approvalPolicy"], "on-request");
        assert_eq!(auto["sandbox"], "workspace-write");
        assert_eq!(auto["approvalsReviewer"], "auto_review");
        assert_eq!(auto["model"], "m1");
        assert_eq!(auto["cwd"], "/work");
        let ask = params(ApprovalsMode::Ask);
        assert_eq!(ask["approvalPolicy"], "on-request");
        assert_eq!(ask["sandbox"], "workspace-write");
        assert_eq!(ask["approvalsReviewer"], "user");
        let bypass = params(ApprovalsMode::Bypass);
        assert_eq!(bypass["approvalPolicy"], "never");
        assert_eq!(bypass["sandbox"], "danger-full-access");
        assert!(auto.get("threadId").is_none());
    }

    #[test]
    fn codex_resume_names_the_stored_thread() {
        let environment = HashMap::new();
        let mut options = options(Agent::Codex, ApprovalsMode::Ask, &environment, &[]);
        options.resume_provider_id = Some("thread-1");
        assert_eq!(codex_thread_params(&options)["threadId"], json!("thread-1"));
    }

    #[test]
    fn codex_app_server_suppresses_update_prompts_and_carries_effort_and_options() {
        let environment = HashMap::new();
        let harness_options = [r#"web_search="live""#.to_owned()];
        let args = codex_session_args(&options(
            Agent::Codex,
            ApprovalsMode::Auto,
            &environment,
            &harness_options,
        ))
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(has_pair(
            &args,
            "--config",
            "check_for_update_on_startup=false"
        ));
        assert!(has_pair(
            &args,
            "--config",
            r#"model_reasoning_effort="high""#
        ));
        assert!(has_pair(&args, "--config", r#"web_search="live""#));
        assert_eq!(args.last().map(String::as_str), Some("app-server"));
        assert!(!args.iter().any(|arg| arg.contains("bypass")));
    }

    #[test]
    fn codex_rejects_bare_and_managed_harness_options() {
        for option in [
            "web_search",
            "model=x",
            "model_reasoning_effort=high",
            "approval_policy=never",
            "sandbox_mode=danger-full-access",
            "approvals_reviewer=user",
            "sandbox_workspace_write=true",
            "sandbox_workspace_write.network_access=true",
            "check_for_update_on_startup=true",
        ] {
            assert!(
                codex_harness_args(&[option.to_owned()]).is_err(),
                "{option:?} was accepted"
            );
        }
    }

    #[test]
    fn resume_commands_are_the_providers_own() {
        assert_eq!(Agent::Claude.resume_command("abc"), "claude --resume abc");
        assert_eq!(Agent::Codex.resume_command("abc"), "codex resume abc");
    }

    #[test]
    fn claude_workspace_trust_preserves_state_and_is_idempotent() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let home = directory.path().join("home");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&home)
            .unwrap_or_else(|error| panic!("failed to create Claude home: {error}"));
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("failed to create workspace: {error}"));
        let state_path = home.join(".claude.json");
        fs::write(
            &state_path,
            r#"{"hasCompletedOnboarding":true,"projects":{"/other":{"custom":42}}}"#,
        )
        .unwrap_or_else(|error| panic!("failed to seed Claude state: {error}"));
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("failed to secure Claude state: {error}"));

        trust_claude_workspace(&home, &workspace)
            .unwrap_or_else(|error| panic!("failed to trust Claude workspace: {error}"));
        let first = fs::read_to_string(&state_path)
            .unwrap_or_else(|error| panic!("failed to read Claude state: {error}"));
        let state: serde_json::Value = serde_json::from_str(&first)
            .unwrap_or_else(|error| panic!("failed to parse Claude state: {error}"));
        let workspace_key = workspace.to_string_lossy();
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["projects"]["/other"]["custom"], 42);
        assert_eq!(
            state["projects"][workspace_key.as_ref()]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            state["projects"][workspace_key.as_ref()]["projectOnboardingSeenCount"],
            1
        );
        assert_eq!(
            fs::metadata(&state_path)
                .unwrap_or_else(|error| panic!("failed to stat Claude state: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        trust_claude_workspace(&home, &workspace)
            .unwrap_or_else(|error| panic!("failed to re-trust Claude workspace: {error}"));
        let second = fs::read_to_string(&state_path)
            .unwrap_or_else(|error| panic!("failed to reread Claude state: {error}"));
        assert_eq!(first, second);
    }

    #[test]
    fn codex_workspace_trust_is_atomic_and_idempotent() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let codex_home = directory.path().join("codex-home");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&codex_home)
            .unwrap_or_else(|error| panic!("failed to create Codex home: {error}"));
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("failed to create workspace: {error}"));
        let config = codex_home.join("config.toml");
        fs::write(&config, "model = \"gpt-test\"\n")
            .unwrap_or_else(|error| panic!("failed to seed config: {error}"));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("failed to secure config: {error}"));

        trust_codex_workspace(&codex_home, &workspace)
            .unwrap_or_else(|error| panic!("failed to trust workspace: {error}"));
        let first = fs::read_to_string(&config)
            .unwrap_or_else(|error| panic!("failed to read config: {error}"));
        assert!(first.contains("model = \"gpt-test\""));
        assert!(first.contains("trust_level = \"trusted\""));
        assert!(first.contains(&workspace.to_string_lossy().to_string()));
        assert_eq!(
            fs::metadata(&config)
                .unwrap_or_else(|error| panic!("failed to stat config: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        trust_codex_workspace(&codex_home, &workspace)
            .unwrap_or_else(|error| panic!("failed to re-trust workspace: {error}"));
        let second = fs::read_to_string(&config)
            .unwrap_or_else(|error| panic!("failed to reread config: {error}"));
        assert_eq!(first, second);
    }

    #[test]
    fn malformed_codex_project_table_returns_an_error() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let codex_home = directory.path().join("codex-home");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&codex_home)
            .unwrap_or_else(|error| panic!("failed to create Codex home: {error}"));
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("failed to create workspace: {error}"));
        fs::write(codex_home.join("config.toml"), "projects = \"invalid\"\n")
            .unwrap_or_else(|error| panic!("failed to seed malformed config: {error}"));

        let error = trust_codex_workspace(&codex_home, &workspace)
            .err()
            .unwrap_or_else(|| panic!("malformed projects table should fail"));
        assert!(error.to_string().contains("must be a table"));
    }
}
