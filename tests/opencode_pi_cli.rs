use serde_json::{Value, json};
use std::process::Command;
use std::thread;
use std::time::Duration;

struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            home: tempfile::Builder::new()
                .prefix("dlgt-pty-")
                .tempdir_in("/tmp")?,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dlgt"));
        command
            .env("DLGT_HOME", self.home.path().join("runtime"))
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env(
                "DLGT_OPENCODE_BIN",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/fake-pty-agent.py"
                ),
            )
            .env(
                "DLGT_PI_BIN",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/fake-pty-agent.py"
                ),
            );
        command
    }

    fn run(&self, args: &[&str]) -> Result<Value, Box<dyn std::error::Error>> {
        let output = self.command().args(args).output()?;
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["ok"], true, "{value}");
        Ok(value)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.command().args(["server", "stop"]).output();
    }
}

fn assert_result(fetch: &Value, text: &str, seq: i64) {
    assert_eq!(fetch["reason"], "result", "{fetch}");
    let results = fetch["sessions"][0]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("missing results: {fetch}"));
    assert!(
        results.iter().any(|result| {
            result["status"] == "completed"
                && result["final_text"] == text
                && result["execution_seq"] == seq
        }),
        "{fetch}"
    );
}

fn wait_state(
    fixture: &Fixture,
    id: &str,
    state: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let mut last = Value::Null;
    for _ in 0..50 {
        last = fixture.run(&["show", id])?;
        if last["session"]["state"] == state {
            return Ok(last);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("session did not become {state}: {last}").into())
}

#[allow(clippy::too_many_lines)]
fn exercise(harness: &str) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let doctor = fixture.command().args(["doctor", "--json"]).output()?;
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor: Value = serde_json::from_slice(&doctor.stdout)?;
    for id in ["opencode", "pi"] {
        let check = doctor["checks"]
            .as_array()
            .and_then(|checks| checks.iter().find(|check| check["id"] == id))
            .unwrap_or_else(|| panic!("missing doctor check {id}: {doctor}"));
        assert_eq!(check["status"], "ok", "{check}");
    }

    let mut new_args = vec![
        "new",
        "--title",
        "first",
        "--harness",
        harness,
        "--cwd",
        "/tmp",
        "--request-id",
        "n1",
        "--model",
    ];
    if harness == "pi" {
        new_args.extend([
            "grok-4.7",
            "--effort",
            "low",
            "--harness-option",
            "provider=xai",
        ]);
    } else {
        new_args.push("xai/grok-4.7");
    }
    new_args.extend(["--", "first"]);
    let start = fixture.run(&new_args)?;
    let id = start["session"]["id"]
        .as_str()
        .ok_or("missing session id")?
        .to_owned();
    assert!(id.starts_with(&format!("{harness}:")), "{id}");
    assert_eq!(start["submission"], "confirmed");
    let fetch = fixture.run(&["fetch", &id, "--wait", "10s"])?;
    assert_result(&fetch, "result:first", 1);

    let listed = fixture.run(&["harnesses", harness])?;
    assert_eq!(listed["harnesses"]["id"], harness);
    assert_eq!(listed["harnesses"]["model_discovery"], "cli");
    assert_eq!(listed["harnesses"]["restart"], true);
    assert_eq!(listed["harnesses"]["effort"], json!(harness == "pi"));
    let models = fixture.run(&["models", "--harness", harness])?;
    assert_eq!(models["discovery"], "cli", "{models}");
    let model_ids = models["models"]
        .as_array()
        .ok_or("models missing")?
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect::<Vec<_>>();
    assert!(model_ids.contains(&"xai/grok-4.7"), "{models}");

    let other = fixture.run(&[
        "new",
        "--title",
        "other",
        "--harness",
        harness,
        "--cwd",
        "/tmp",
        "--request-id",
        "n2",
        "--",
        "other",
    ])?;
    let other_id = other["session"]["id"]
        .as_str()
        .ok_or("other id")?
        .to_owned();
    assert_ne!(id, other_id);
    fixture.run(&["send", &id, "--request-id", "n3", "--", "followup\n日本語"])?;
    let fetch = fixture.run(&["fetch", &id, "--wait", "10s"])?;
    assert_result(&fetch, "result:followup\n日本語", 2);

    fixture.run(&["stop", &id])?;
    wait_state(&fixture, &id, "stopped")?;
    fixture.run(&[
        "send",
        &id,
        "--resume",
        "--cwd",
        "/tmp",
        "--request-id",
        "n4",
        "--",
        "resume",
    ])?;
    let fetch = fixture.run(&["fetch", &id, "--wait", "10s"])?;
    assert_result(&fetch, "result:resume", 3);
    let other_fetch = fixture.run(&["fetch", &other_id, "--wait", "10s"])?;
    assert_result(&other_fetch, "result:other", 1);
    assert!(!other_fetch.to_string().contains("result:resume"));

    fixture.run(&["stop", &other_id])?;
    let restarted = fixture.run(&["restart", &id])?;
    assert_eq!(restarted["session"]["id"], id);
    assert_eq!(restarted["session"]["state"], "idle");

    let pid_name = format!("{harness}-{}.pid", id.replace(':', "_"));
    let pid = std::fs::read_to_string(fixture.home.path().join(pid_name))?;
    let killed = Command::new("kill").args(["-9", pid.trim()]).status()?;
    assert!(killed.success(), "failed to kill {pid}");
    wait_state(&fixture, &id, "failed")?;
    fixture.run(&[
        "send",
        &id,
        "--resume",
        "--cwd",
        "/tmp",
        "--request-id",
        "n5",
        "--",
        "after-kill",
    ])?;
    let fetch = fixture.run(&["fetch", &id, "--wait", "10s"])?;
    assert_result(&fetch, "result:after-kill", 4);
    if harness == "opencode" {
        let plugin =
            std::fs::read_to_string(fixture.home.path().join(".config/opencode/plugins/dlgt.js"))?;
        assert!(plugin.contains("DLGT_OPENCODE_LAUNCH"));
        assert!(plugin.contains("session.idle"));
    } else {
        let bridge = std::fs::read_to_string(fixture.home.path().join(".pi/agent/dlgt/bridge.js"))?;
        assert!(bridge.contains("DLGT_PI_LAUNCH"));
        assert!(bridge.contains("agent_settled"));
        assert!(!fixture.home.path().join(".pi/agent/extensions").exists());
    }
    fixture.run(&["stop", &id])?;
    Ok(())
}

#[test]
fn opencode_round_trip_resume_restart_and_kill() -> Result<(), Box<dyn std::error::Error>> {
    exercise("opencode")
}

#[test]
fn pi_round_trip_resume_restart_and_kill() -> Result<(), Box<dyn std::error::Error>> {
    exercise("pi")
}
