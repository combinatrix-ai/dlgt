use serde_json::{Value, json};
use std::process::Command;

struct Fixture {
    home: tempfile::TempDir,
}
impl Fixture {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dlgt"));
        command
            .env("DLGT_HOME", self.home.path().join("runtime"))
            .env("HOME", self.home.path())
            .env(
                "DLGT_CURSOR_BIN",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-cursor.py"),
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
fn assert_result(fetch: &Value, text: &str, seq: i64) {
    assert_eq!(fetch["reason"], "result", "{fetch}");
    let results = fetch["sessions"][0]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("missing results: {fetch}"));
    assert!(
        results.iter().any(|r| r["status"] == "completed"
            && r["final_text"] == text
            && r["execution_seq"] == seq),
        "{fetch}"
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.command().args(["server", "stop"]).output();
    }
}

#[test]
fn interactive_cursor_round_trip_followup_parallel_session_and_resume()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture {
        home: tempfile::Builder::new().prefix("dc-").tempdir_in("/tmp")?,
    };
    let start = fixture.run(&[
        "new",
        "--title",
        "cursor first",
        "--harness",
        "cursor",
        "--cwd",
        "/tmp",
        "--request-id",
        "c1",
        "--",
        "first",
    ])?;
    // CLI result envelopes flatten fields into the successful response.
    let id = start["session"]["id"]
        .as_str()
        .ok_or("missing session id")?;
    assert!(id.starts_with("cursor:"));
    assert_eq!(start["submission"], "confirmed");
    let fetch = fixture.run(&["fetch", id, "--wait", "10s"])?;
    assert_result(&fetch, "result:first", 1);
    let replay = fixture.run(&[
        "new",
        "--title",
        "cursor first",
        "--harness",
        "cursor",
        "--cwd",
        "/tmp",
        "--request-id",
        "c1",
        "--",
        "first",
    ])?;
    assert_eq!(replay["session"]["id"], id);
    let other = fixture.run(&[
        "new",
        "--title",
        "cursor other",
        "--harness",
        "cursor",
        "--cwd",
        "/tmp",
        "--request-id",
        "c2",
        "--",
        "other",
    ])?;
    let other_id = other["session"]["id"].as_str().ok_or("other id")?;
    assert_ne!(id, other_id);
    fixture.run(&["send", id, "--request-id", "c3", "--", "followup\n日本語"])?;
    let fetch = fixture.run(&["fetch", id, "--wait", "10s"])?;
    assert_result(&fetch, "result:followup\n日本語", 2);
    fixture.run(&["stop", id])?;
    fixture.run(&[
        "send",
        id,
        "--resume",
        "--cwd",
        "/tmp",
        "--request-id",
        "c4",
        "--",
        "resume",
    ])?;
    let fetch = fixture.run(&["fetch", id, "--wait", "10s"])?;
    assert_result(&fetch, "result:resume", 3);
    let other_fetch = fixture.run(&["fetch", other_id, "--wait", "10s"])?;
    assert_result(&other_fetch, "result:other", 1);
    assert!(!other_fetch.to_string().contains("result:resume"));
    let hooks: Value = serde_json::from_slice(&std::fs::read(
        fixture.home.path().join(".cursor/hooks.json"),
    )?)?;
    assert_eq!(hooks["hooks"]["stop"].as_array().map(Vec::len), Some(1));
    assert_eq!(hooks["version"], json!(1));
    fixture.run(&["stop", id])?;
    fixture.run(&["stop", other_id])?;
    Ok(())
}
