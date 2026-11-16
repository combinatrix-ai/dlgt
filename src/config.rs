use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::error::{Code, coded};
use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::protocol::{ApprovalsMode, AuthPolicy};

const PROFILE_KEYS: &[&str] = &[
    "harness",
    "model",
    "effort",
    "approvals",
    "auth",
    "clean_env",
    "pass_env",
    "unset_env",
    "env",
    "harness_options",
];
const DEFAULT_HARNESS_KEYS: &[&str] = &[
    "model",
    "effort",
    "approvals",
    "auth",
    "clean_env",
    "pass_env",
    "unset_env",
    "env",
    "harness_options",
];

#[derive(Debug, Clone, Default)]
pub struct Config {
    profiles: BTreeMap<String, Profile>,
    approvals: Option<ApprovalsMode>,
    claude: PartialLaunch,
    codex: PartialLaunch,
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub harness: Option<String>,
    settings: PartialLaunch,
    public: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialLaunch {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approvals: Option<ApprovalsMode>,
    pub auth: Option<AuthPolicy>,
    pub clean_env: Option<bool>,
    pub env: BTreeMap<String, String>,
    pub pass_env: Vec<String>,
    pub unset_env: Vec<String>,
    pub harness_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approvals: ApprovalsMode,
    pub auth: AuthPolicy,
    pub clean_env: bool,
    pub harness_options: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

impl Default for ResolvedLaunch {
    fn default() -> Self {
        Self {
            model: None,
            effort: None,
            approvals: ApprovalsMode::Auto,
            auth: AuthPolicy::Subscription,
            clean_env: false,
            harness_options: Vec::new(),
            environment: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthMismatch {
    pub variables: Vec<String>,
}

impl AuthMismatch {
    pub fn from_variables(variables: Vec<String>) -> Self {
        Self { variables }
    }

    pub fn hint(&self) -> String {
        if self.variables.is_empty() {
            "Confirm Claude is logged in with a claude.ai subscription in the selected launch environment, or choose --auth any if another provider source is intended.".to_owned()
        } else {
            let unset_options = self
                .variables
                .iter()
                .map(|name| format!("--unset-env {name}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unset the blocking environment variable(s) {} with {} (or unset them in [defaults.claude]); choose --auth any only if another provider source is intended.",
                self.variables.join(", "),
                unset_options,
            )
        }
    }
}

impl fmt::Display for AuthMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.variables.is_empty() {
            formatter.write_str(AUTH_MISMATCH_MESSAGE)
        } else {
            write!(
                formatter,
                "{AUTH_MISMATCH_MESSAGE}: {}",
                self.variables.join(", ")
            )
        }
    }
}

impl Error for AuthMismatch {}

pub const AUTH_MISMATCH_MESSAGE: &str = "Claude subscription authentication could not be confirmed or a non-subscription provider source is configured";

const SUBSCRIPTION_BLOCKING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_AWS_WORKSPACE_ID",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_BEDROCK_REGION_PREFIX",
    "ANTHROPIC_BEDROCK_SERVICE_TIER",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_GOOGLE_CLOUD_BASE_URL",
    "ANTHROPIC_GOOGLE_CLOUD_LOCATION",
    "ANTHROPIC_GOOGLE_CLOUD_PROJECT",
    "ANTHROPIC_GOOGLE_CLOUD_WORKSPACE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_WORKSPACE_ID",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
    "CLAUDE_CODE_SKIP_ANTHROPIC_GOOGLE_CLOUD_AUTH",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_SKIP_MANTLE_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
];

pub fn path() -> PathBuf {
    std::env::var_os("DLGT_CONFIG").map_or_else(
        || {
            std::env::var_os("HOME")
                .map_or_else(PathBuf::new, PathBuf::from)
                .join(".config/dlgt/config.toml")
        },
        PathBuf::from,
    )
}

pub fn load() -> Result<Config> {
    load_from_path(&path())
}

pub fn load_from_path(path: &std::path::Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    parse(&text).with_context(|| format!("invalid dlgt config at {}", path.display()))
}

pub fn parse(text: &str) -> Result<Config> {
    let document = toml_edit::Document::parse(text).map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid dlgt config at {line}:{column}: invalid TOML syntax"),
        )
    })?;
    // The reason can quote a configured value, so only the location of the
    // innermost offending item is reported.
    parse_document(document.as_table()).map_err(|error| {
        let offset = error.downcast_ref::<ItemSpan>().map_or(0, |span| span.0);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid dlgt config at {line}:{column}: invalid configuration entry"),
        )
    })
}

/// Where in the source the item being parsed starts.
#[derive(Debug)]
struct ItemSpan(usize);

impl fmt::Display for ItemSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at offset {}", self.0)
    }
}

impl Error for ItemSpan {}

/// Record the innermost item that failed; outer items keep its location.
fn at_item<T>(item: &Item, result: Result<T>) -> Result<T> {
    match (result, item.span()) {
        (Err(error), Some(span)) if error.downcast_ref::<ItemSpan>().is_none() => {
            Err(error.context(ItemSpan(span.start)))
        }
        (result, _) => result,
    }
}

pub fn parse_toml_document(text: &str, source: &str) -> Result<DocumentMut> {
    parse_toml_document_as(text, &format!("TOML in {source}"))
}

fn parse_toml_document_as(text: &str, subject: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid {subject} at {line}:{column}: invalid TOML syntax"),
        )
    })
}

fn parse_document(document: &toml_edit::Table) -> Result<Config> {
    let mut config = Config::default();

    if let Some(defaults) = document.get("defaults") {
        let table = defaults
            .as_table_like()
            .context("[defaults] must be a table")?;
        for (key, item) in table.iter() {
            at_item(
                item,
                (|| {
                    match key {
                        "approvals" => {
                            config.approvals = Some(parse_approvals(item, "[defaults].approvals")?);
                        }
                        "claude" => {
                            config.claude = parse_defaults_harness(item, "defaults.claude", true)?;
                        }
                        "codex" => {
                            config.codex = parse_defaults_harness(item, "defaults.codex", false)?;
                        }
                        _ => bail!("unknown key {key:?} in [defaults]"),
                    }
                    Ok(())
                })(),
            )?;
        }
    }

    if let Some(profiles) = document.get("profiles") {
        let table = profiles
            .as_table_like()
            .context("[profiles] must be a table")?;
        for (name, item) in table.iter() {
            let profile_table = item
                .as_table_like()
                .with_context(|| format!("profile {name:?} must be a table"))?;
            let profile = at_item(item, parse_profile(name, profile_table))?;
            config.profiles.insert(name.to_owned(), profile);
        }
    }
    Ok(config)
}

impl Config {
    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles
            .get(name)
            .with_context(|| format!("profile not found: {name}"))
    }

    pub fn profiles_value(&self) -> Value {
        Value::Object(
            self.profiles
                .iter()
                .map(|(name, profile)| (name.clone(), profile.public.clone()))
                .collect(),
        )
    }

    pub fn profile_value(&self, name: &str) -> Result<Value> {
        Ok(self.profile(name)?.public.clone())
    }

    pub fn resolved_defaults_value(&self) -> Value {
        json!({
            "claude": defaults_value(
                self.approvals,
                &self.claude,
                Some(AuthPolicy::Subscription),
            ),
            "codex": defaults_value(self.approvals, &self.codex, None),
        })
    }

    pub fn resolve(
        &self,
        harness: &str,
        profile: Option<&Profile>,
        explicit: &PartialLaunch,
        client_environment: &BTreeMap<String, String>,
    ) -> Result<ResolvedLaunch> {
        let mut resolved = ResolvedLaunch::default();
        if let Some(approvals) = self.approvals {
            resolved.approvals = approvals;
        }
        let defaults = match harness {
            "claude" => &self.claude,
            "codex" => &self.codex,
            other => bail!("unsupported harness {other:?}; expected codex or claude"),
        };
        overlay(&mut resolved, defaults);
        if let Some(profile) = profile {
            if harness == "codex" && profile.settings.auth.is_some() {
                bail!("profile auth is only valid with the Claude harness");
            }
            overlay(&mut resolved, &profile.settings);
        }
        if harness == "codex" && explicit.auth.is_some() {
            bail!("--auth is only valid with --harness claude");
        }
        overlay(&mut resolved, explicit);

        let clean_env = resolved.clean_env;
        let pass_env = collect_pass_env(self, harness, profile, explicit);
        let unset_env = collect_unset_env(self, harness, profile, explicit);
        let mut environment = if clean_env {
            BTreeMap::new()
        } else {
            client_environment.clone()
        };
        if clean_env {
            for key in pass_env {
                if let Some(value) = client_environment.get(&key) {
                    environment.insert(key, value.clone());
                }
            }
        }
        let mut assignments = BTreeMap::new();
        assignments.extend(
            defaults
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        if let Some(profile) = profile {
            assignments.extend(
                profile
                    .settings
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        assignments.extend(
            explicit
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        environment.extend(assignments);
        for key in unset_env {
            environment.remove(&key);
        }
        resolved.environment = environment;
        Ok(resolved)
    }
}

pub fn validate_auth_environment<'a>(
    policy: AuthPolicy,
    values: impl Iterator<Item = (&'a String, &'a String)>,
) -> Result<()> {
    let blocked = values
        .filter(|(key, value)| {
            SUBSCRIPTION_BLOCKING_ENV.contains(&key.as_str())
                && if key.starts_with("CLAUDE_CODE_USE_")
                    || key.starts_with("CLAUDE_CODE_SKIP_")
                    || key.as_str() == "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"
                    || key.as_str() == "CLAUDE_CODE_SIMPLE"
                {
                    is_truthy(value)
                } else {
                    !value.is_empty()
                }
        })
        .map(|(key, _)| key.as_str().to_owned())
        .collect::<Vec<_>>();
    if policy == AuthPolicy::Subscription && !blocked.is_empty() {
        return Err(AuthMismatch::from_variables(blocked).into());
    }
    Ok(())
}

fn is_truthy(value: &str) -> bool {
    !value.is_empty()
        && !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
}

pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let prefix = &text[..offset.min(text.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit('\n')
        .next()
        .map_or(1, |last| last.chars().count() + 1);
    (line, column)
}

fn collect_pass_env(
    config: &Config,
    harness: &str,
    profile: Option<&Profile>,
    explicit: &PartialLaunch,
) -> Vec<String> {
    let mut values = config.defaults_for(harness).pass_env.clone();
    if let Some(profile) = profile {
        values.extend(profile.settings.pass_env.iter().cloned());
    }
    values.extend(explicit.pass_env.iter().cloned());
    values
}

fn collect_unset_env(
    config: &Config,
    harness: &str,
    profile: Option<&Profile>,
    explicit: &PartialLaunch,
) -> Vec<String> {
    let mut values = config.defaults_for(harness).unset_env.clone();
    if let Some(profile) = profile {
        values.extend(profile.settings.unset_env.iter().cloned());
    }
    values.extend(explicit.unset_env.iter().cloned());
    values
}

impl Config {
    fn defaults_for(&self, harness: &str) -> &PartialLaunch {
        if harness == "claude" {
            &self.claude
        } else {
            &self.codex
        }
    }
}

fn overlay(resolved: &mut ResolvedLaunch, layer: &PartialLaunch) {
    if let Some(value) = &layer.model {
        resolved.model.clone_from(&Some(value.clone()));
    }
    if let Some(value) = &layer.effort {
        resolved.effort.clone_from(&Some(value.clone()));
    }
    if let Some(value) = layer.approvals {
        resolved.approvals = value;
    }
    if let Some(value) = layer.auth {
        resolved.auth = value;
    }
    if let Some(value) = layer.clean_env {
        resolved.clean_env = value;
    }
    resolved
        .harness_options
        .extend(layer.harness_options.iter().cloned());
}

fn defaults_value(
    global_approvals: Option<ApprovalsMode>,
    settings: &PartialLaunch,
    default_auth: Option<AuthPolicy>,
) -> Value {
    let mut value = Map::new();
    value.insert(
        "approvals".to_owned(),
        json!(
            settings
                .approvals
                .unwrap_or(global_approvals.unwrap_or(ApprovalsMode::Auto))
        ),
    );
    if let Some(auth) = settings.auth.or(default_auth) {
        value.insert("auth".to_owned(), json!(auth.as_str()));
    }
    if let Some(model) = &settings.model {
        value.insert("model".to_owned(), json!(model));
    }
    if let Some(effort) = &settings.effort {
        value.insert("effort".to_owned(), json!(effort));
    }
    value.insert(
        "clean_env".to_owned(),
        json!(settings.clean_env.unwrap_or(false)),
    );
    value.insert("pass_env".to_owned(), json!(settings.pass_env));
    value.insert("unset_env".to_owned(), json!(settings.unset_env));
    value.insert(
        "harness_options".to_owned(),
        json!(settings.harness_options),
    );
    value.insert("env".to_owned(), redact_env(&settings.env));
    Value::Object(value)
}

fn parse_defaults_harness(item: &Item, name: &str, claude: bool) -> Result<PartialLaunch> {
    let table = item
        .as_table_like()
        .with_context(|| format!("[{name}] must be a table"))?;
    parse_settings(table, name, claude, DEFAULT_HARNESS_KEYS)
}

fn parse_profile(name: &str, table: &dyn TableLike) -> Result<Profile> {
    let profile_name = format!("profiles.{name}");
    let settings = parse_settings(table, &profile_name, true, PROFILE_KEYS)?;
    let harness = table
        .get("harness")
        .map(|item| required_string(item, &format!("{profile_name}.harness")))
        .transpose()?;
    if let Some(harness) = &harness
        && !matches!(harness.as_str(), "claude" | "codex")
    {
        bail!("{profile_name}.harness must be \"claude\" or \"codex\"");
    }
    if harness.as_deref() == Some("codex") && settings.auth.is_some() {
        bail!("auth is only valid for Claude Profiles");
    }
    let mut public = Map::new();
    for (key, item) in table.iter() {
        let value = if key == "env" {
            redact_env(&parse_env(item, &format!("{profile_name}.env"))?)
        } else {
            item_to_json(item)
                .with_context(|| format!("unsupported value for {profile_name}.{key}"))?
        };
        public.insert(key.to_owned(), value);
    }
    Ok(Profile {
        harness,
        settings,
        public: Value::Object(public),
    })
}

fn parse_settings(
    table: &dyn TableLike,
    name: &str,
    claude: bool,
    allowed_keys: &[&str],
) -> Result<PartialLaunch> {
    let mut settings = PartialLaunch::default();
    for (key, item) in table.iter() {
        at_item(
            item,
            (|| {
                if !allowed_keys.contains(&key) || key == "auth" && !claude {
                    bail!("unknown key {key:?} in [{name}]");
                }
                let key_name = format!("{name}.{key}");
                match key {
                    "model" => settings.model = Some(required_string(item, &key_name)?),
                    "effort" => settings.effort = Some(required_string(item, &key_name)?),
                    "approvals" => settings.approvals = Some(parse_approvals(item, &key_name)?),
                    "auth" => {
                        let value = required_string(item, &key_name)?;
                        settings.auth = Some(AuthPolicy::parse(&value).with_context(|| {
                            format!("{key_name} must be \"subscription\" or \"any\"")
                        })?);
                    }
                    "clean_env" => {
                        settings.clean_env = Some(
                            item.as_bool()
                                .with_context(|| format!("{key_name} must be a boolean"))?,
                        );
                    }
                    "env" => settings.env = parse_env(item, &key_name)?,
                    "pass_env" => settings.pass_env = string_array(item, &key_name)?,
                    "unset_env" => settings.unset_env = string_array(item, &key_name)?,
                    "harness_options" => settings.harness_options = string_array(item, &key_name)?,
                    "harness" => {}
                    _ => bail!("unknown key {key:?} in [{name}]"),
                }
                Ok(())
            })(),
        )?;
    }
    Ok(settings)
}

fn parse_approvals(item: &Item, name: &str) -> Result<ApprovalsMode> {
    let value = required_string(item, name)?;
    ApprovalsMode::parse(&value).with_context(|| format!("{name} must be auto, ask, or bypass"))
}

fn required_string(item: &Item, name: &str) -> Result<String> {
    item.as_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} must be a string"))
}

fn string_array(item: &Item, name: &str) -> Result<Vec<String>> {
    item.as_array()
        .with_context(|| format!("{name} must be an array of strings"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("{name} entries must be strings"))
        })
        .collect()
}

fn parse_env(item: &Item, name: &str) -> Result<BTreeMap<String, String>> {
    if let Some(table) = item.as_table_like() {
        return table
            .iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .with_context(|| format!("{name}.{key} must be a string"))?;
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect();
    }
    if let Some(array) = item.as_array() {
        return array
            .iter()
            .map(|value| {
                let assignment = value
                    .as_str()
                    .with_context(|| format!("{name} entries must be KEY=VALUE strings"))?;
                let (key, value) = assignment
                    .split_once('=')
                    .with_context(|| format!("{name} entries must use KEY=VALUE"))?;
                if key.is_empty() {
                    bail!("{name} keys must not be empty");
                }
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect();
    }
    Err(anyhow!(
        "{name} must be a table or an array of KEY=VALUE strings"
    ))
}

fn redact_env(values: &BTreeMap<String, String>) -> Value {
    Value::Object(
        values
            .keys()
            .map(|key| (key.clone(), json!("<redacted>")))
            .collect(),
    )
}

fn item_to_json(item: &Item) -> Result<Value> {
    if let Some(value) = item.as_str() {
        return Ok(json!(value));
    }
    if let Some(value) = item.as_bool() {
        return Ok(json!(value));
    }
    if let Some(array) = item.as_array() {
        return array
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(|value| json!(value))
                    .context("array entries must be strings")
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    bail!("unsupported TOML value")
}

#[cfg(test)]
mod tests {
    use super::{AuthMismatch, PartialLaunch, validate_auth_environment};
    use crate::protocol::{ApprovalsMode, AuthPolicy};
    use std::collections::BTreeMap;

    fn client_environment(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn each_precedence_layer_overrides_scalars_and_concatenates_lists() -> anyhow::Result<()> {
        let config = super::parse(
            r#"
[defaults]
approvals = "ask"

[defaults.claude]
model = "defaults-model"
effort = "defaults-effort"
approvals = "bypass"
auth = "any"
harness_options = ["defaults-option"]
pass_env = ["DEFAULT_PASS"]
unset_env = ["DEFAULT_UNSET"]
env = { LAYER = "defaults", DEFAULT_ONLY = "yes" }

[profiles.review]
harness = "claude"
model = "profile-model"
effort = "profile-effort"
approvals = "ask"
auth = "subscription"
harness_options = ["profile-option"]
pass_env = ["PROFILE_PASS"]
unset_env = ["PROFILE_UNSET"]
env = ["LAYER=profile", "PROFILE_ONLY=yes"]
"#,
        )?;
        let profile = config.profile("review")?;
        let mut explicit = PartialLaunch {
            model: Some("cli-model".to_owned()),
            effort: Some("cli-effort".to_owned()),
            approvals: Some(ApprovalsMode::Auto),
            auth: Some(AuthPolicy::Any),
            clean_env: Some(true),
            pass_env: vec!["CLI_PASS".to_owned()],
            unset_env: vec!["CLI_UNSET".to_owned()],
            harness_options: vec!["cli-option".to_owned()],
            ..PartialLaunch::default()
        };
        explicit.env.insert("LAYER".to_owned(), "cli".to_owned());
        let client_env = client_environment(&[
            ("DEFAULT_PASS", "default-pass"),
            ("PROFILE_PASS", "profile-pass"),
            ("CLI_PASS", "cli-pass"),
            ("DEFAULT_UNSET", "remove"),
            ("PROFILE_UNSET", "remove"),
            ("CLI_UNSET", "remove"),
        ]);
        let resolved = config.resolve("claude", Some(profile), &explicit, &client_env)?;

        assert_eq!(resolved.model.as_deref(), Some("cli-model"));
        assert_eq!(resolved.effort.as_deref(), Some("cli-effort"));
        assert_eq!(resolved.approvals, ApprovalsMode::Auto);
        assert_eq!(resolved.auth, AuthPolicy::Any);
        assert_eq!(
            resolved.harness_options,
            ["defaults-option", "profile-option", "cli-option"]
        );
        assert_eq!(
            resolved.environment.get("LAYER").map(String::as_str),
            Some("cli")
        );
        assert_eq!(
            resolved.environment.get("DEFAULT_ONLY").map(String::as_str),
            Some("yes")
        );
        assert_eq!(
            resolved.environment.get("PROFILE_ONLY").map(String::as_str),
            Some("yes")
        );
        assert_eq!(
            resolved.environment.get("DEFAULT_PASS").map(String::as_str),
            Some("default-pass")
        );
        assert_eq!(
            resolved.environment.get("PROFILE_PASS").map(String::as_str),
            Some("profile-pass")
        );
        assert_eq!(
            resolved.environment.get("CLI_PASS").map(String::as_str),
            Some("cli-pass")
        );
        for key in ["DEFAULT_UNSET", "PROFILE_UNSET", "CLI_UNSET"] {
            assert!(!resolved.environment.contains_key(key));
        }

        let no_cli = config.resolve(
            "claude",
            Some(profile),
            &PartialLaunch::default(),
            &client_env,
        )?;
        assert_eq!(no_cli.model.as_deref(), Some("profile-model"));
        assert_eq!(no_cli.approvals, ApprovalsMode::Ask);
        assert_eq!(no_cli.auth, AuthPolicy::Subscription);

        let no_profile = config.resolve("claude", None, &PartialLaunch::default(), &client_env)?;
        assert_eq!(no_profile.model.as_deref(), Some("defaults-model"));
        assert_eq!(no_profile.approvals, ApprovalsMode::Bypass);
        assert_eq!(no_profile.auth, AuthPolicy::Any);

        let codex = config.resolve("codex", None, &PartialLaunch::default(), &client_env)?;
        assert_eq!(codex.approvals, ApprovalsMode::Ask);
        assert_eq!(codex.auth, AuthPolicy::Subscription);
        Ok(())
    }

    #[test]
    fn invalid_entries_are_rejected_without_echoing_the_config_source() {
        for (text, location) in [
            ("[defaults.claude]\nunknown = true", "2:"),
            ("[defaults.mystery]\nmodel = \"x\"", "1:"),
            (
                "[defaults]\napprovals = \"ask\"\n\n[defaults.claude]\nclean_env = \"mystery\"",
                "5:",
            ),
            (
                "[profiles.p]\nharness = \"claude\"\nmodel = \"m\"\nunknown = \"mystery\"",
                "4:",
            ),
        ] {
            let result = super::parse(text);
            assert!(
                result.is_err(),
                "invalid defaults unexpectedly parsed: {text}"
            );
            let message = result.err().map_or_else(
                || "invalid defaults unexpectedly parsed".to_owned(),
                |error| error.to_string(),
            );
            assert!(
                message.contains(&format!("at {location}")),
                "wrong location for {text:?}: {message}"
            );
            assert!(
                !message.contains("unknown") && !message.contains("mystery"),
                "source text leaked in error: {message}"
            );
        }
    }

    #[test]
    fn malformed_toml_errors_include_location_without_source_text() {
        let secret = "super-secret-config-token";
        let text = format!("[profiles.secret]\nvalue = \"{secret}\"\nthis is not TOML");
        let Err(error) = super::parse(&text) else {
            panic!("malformed configuration was accepted");
        };
        let message = error.to_string();
        assert!(message.contains("3:"), "missing source location: {message}");
        assert!(
            !message.contains(secret),
            "secret leaked in error: {message}"
        );
    }

    #[test]
    fn profile_output_redacts_environment_values() -> anyhow::Result<()> {
        let config = super::parse(
            "[profiles.safe]\nharness = \"claude\"\nenv = { API_KEY = \"secret-value\" }",
        )?;
        let rendered = config.profiles_value().to_string();
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("<redacted>"));
        Ok(())
    }

    #[test]
    fn resolved_defaults_include_builtins_and_specific_overrides() -> anyhow::Result<()> {
        let config =
            super::parse("[defaults]\napprovals = \"ask\"\n[defaults.claude]\nauth = \"any\"")?;
        let defaults = config.resolved_defaults_value();
        assert_eq!(defaults["claude"]["approvals"], "ask");
        assert_eq!(defaults["claude"]["auth"], "any");
        assert_eq!(defaults["codex"]["approvals"], "ask");
        assert!(defaults["codex"].get("auth").is_none());
        Ok(())
    }

    #[test]
    fn subscription_policy_rejects_api_keys_and_provider_routing_environment() {
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AWS_BASE_URL",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
            "ANTHROPIC_FOUNDRY_BASE_URL",
            "ANTHROPIC_GOOGLE_CLOUD_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
        ] {
            let values = client_environment(&[(key, "sensitive-route-value")]);
            let Err(error) = validate_auth_environment(AuthPolicy::Subscription, values.iter())
            else {
                panic!("subscription auth accepted {key}");
            };
            assert!(error.downcast_ref::<AuthMismatch>().is_some());
            assert!(!error.to_string().contains("sensitive-route-value"));
        }
        for key in [
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "CLAUDE_CODE_SIMPLE",
        ] {
            let values = client_environment(&[(key, "true")]);
            assert!(
                validate_auth_environment(AuthPolicy::Subscription, values.iter()).is_err(),
                "subscription auth accepted {key}"
            );
            let false_values = client_environment(&[(key, "false")]);
            assert!(
                validate_auth_environment(AuthPolicy::Subscription, false_values.iter()).is_ok()
            );
        }
        let values = client_environment(&[("ANTHROPIC_API_KEY", "dummy-not-a-key")]);
        assert!(validate_auth_environment(AuthPolicy::Any, values.iter()).is_ok());
        let empty = client_environment(&[("ANTHROPIC_API_KEY", "")]);
        assert!(validate_auth_environment(AuthPolicy::Subscription, empty.iter()).is_ok());
    }

    #[test]
    fn auth_mismatch_names_only_the_blocking_environment_variables() {
        let entries = std::collections::BTreeMap::from([
            (
                "ANTHROPIC_BASE_URL".to_owned(),
                "https://private.invalid".to_owned(),
            ),
            ("UNRELATED".to_owned(), "unrelated-secret".to_owned()),
        ]);
        let error = super::validate_auth_environment(AuthPolicy::Subscription, entries.iter())
            .err()
            .unwrap_or_else(|| panic!("blocked auth override was accepted"));
        let message = format!("{error:#}");
        assert!(message.contains("ANTHROPIC_BASE_URL"), "{message}");
        assert!(!message.contains("https://private.invalid"), "{message}");
        assert!(!message.contains("unrelated-secret"), "{message}");
        assert!(
            !message.contains("ANTHROPIC_API_KEY"),
            "unrelated remedy: {message}"
        );
        let mismatch = error
            .downcast_ref::<AuthMismatch>()
            .unwrap_or_else(|| panic!("auth mismatch type was lost"));
        let hint = mismatch.hint();
        assert!(hint.contains("ANTHROPIC_BASE_URL"), "{hint}");
        assert!(hint.contains("--unset-env ANTHROPIC_BASE_URL"), "{hint}");
        assert!(!hint.contains("ANTHROPIC_API_KEY"), "{hint}");
        assert!(!hint.contains("https://private.invalid"), "{hint}");
    }
}
