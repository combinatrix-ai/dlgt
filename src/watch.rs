//! `dlgt watch`: the human view of a Session. It follows the same `fetch`
//! deltas an agent reads and prints them as text. It is read-only.

use std::collections::HashSet;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MESSAGE_PAGE: u64 = 512;

pub fn run(selector: &str, from_start: bool, until_idle: bool) -> Result<()> {
    let route = client::find_live_session(selector)?;
    let fetch = |params: Value| match &route {
        Some(route) => client::call_socket(&route.socket, "session.fetch", params),
        None => client::call("session.fetch", params),
    };
    let session = route
        .as_ref()
        .map_or(selector, |route| route.session_id.as_str());
    // Without --from-start the first read only takes the current position,
    // so its baseline result is history rather than news.
    let mut params = if from_start {
        json!({"session": session, "from_start": true, "messages": MESSAGE_PAGE})
    } else {
        json!({"session": session, "messages": false})
    };
    let mut asked = HashSet::new();
    // Results wait here until their execution's messages are all printed,
    // which can take several pages.
    let mut held = Vec::new();
    // Messages and results page independently, so a page can carry a later
    // execution's messages before an earlier execution's result. Both are
    // collected until the response has no more pages, then printed in order.
    let mut collected_messages = Vec::new();
    let mut collected_results = Vec::new();
    let mut history = !from_start;
    loop {
        let response = fetch(params)?;
        let bucket = response
            .pointer("/sessions/0")
            .context("fetch response carried no Session")?;
        let mut lines = Vec::new();
        for gap in gaps(&response, bucket) {
            lines.push(format!("[gap]    {gap} retention_overrun"));
        }
        let more = response.get("has_more").and_then(Value::as_bool) == Some(true);
        let list = |name: &str| {
            bucket
                .get(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        collected_messages.extend(list("messages"));
        collected_results.extend(list("results"));
        let mut page = bucket.clone();
        if more {
            page["messages"] = json!([]);
            page["results"] = json!([]);
        } else {
            page["messages"] = Value::Array(std::mem::take(&mut collected_messages));
            page["results"] = Value::Array(std::mem::take(&mut collected_results));
        }
        lines.extend(render(&page, &mut asked, &mut held, history, !more));
        if !more {
            history = false;
        }
        let mut stdout = std::io::stdout().lock();
        for line in lines {
            writeln!(stdout, "{line}")?;
        }
        stdout.flush()?;
        drop(stdout);

        let state = bucket
            .pointer("/session/state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !more && (matches!(state, "stopped" | "failed") || (until_idle && state == "idle")) {
            return Ok(());
        }
        params = json!({
            "session": session,
            "cursor": response.get("cursor"),
            "messages": MESSAGE_PAGE,
        });
        if !more {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

fn gaps<'a>(response: &'a Value, bucket: &'a Value) -> impl Iterator<Item = &'a str> {
    [response.get("gaps"), bucket.get("gaps")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|gap| gap.get("component").and_then(Value::as_str))
}

/// Text lines for one fetch bucket: messages with each execution's result
/// after them, then Requests that have not been announced yet. A result is
/// held until a later execution's message appears or the page is `drained`.
fn render(
    bucket: &Value,
    asked: &mut HashSet<String>,
    held: &mut Vec<Value>,
    history: bool,
    drained: bool,
) -> Vec<String> {
    let harness = bucket
        .pointer("/session/harness")
        .and_then(Value::as_str)
        .unwrap_or("worker");
    let list = |name: &str| {
        bucket
            .get(name)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let execution = |value: &Value| value.get("execution_seq").and_then(Value::as_i64);
    // A result that starts in this bucket closes its execution: it is
    // printed after that execution's messages, not after the whole page.
    held.extend(
        list("results")
            .filter(|result| {
                !history && result.get("final_text_offset").and_then(Value::as_u64) == Some(0)
            })
            .cloned(),
    );
    let done = |result: &Value| {
        let status = result.get("status").and_then(Value::as_str).unwrap_or("?");
        format!("{} done   {status}", prefix(result))
    };
    let mut lines = Vec::new();
    for message in list("messages") {
        let (earlier, later): (Vec<_>, Vec<_>) = held
            .drain(..)
            .partition(|result| execution(result) < execution(message));
        *held = later;
        lines.extend(earlier.iter().map(done));
        lines.push(message_line(harness, message));
    }
    if drained {
        lines.extend(held.drain(..).map(|result| done(&result)));
    }
    for request in list("requests") {
        let text = |name: &str| {
            request
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        if !asked.insert(text("id").to_owned()) {
            continue;
        }
        let subject = if text("kind") == "question" {
            text("text").replace('\n', " ")
        } else {
            format!("{}  {}", text("tool"), text("summary"))
        };
        lines.push(format!(
            "{} ask    {}  {subject}",
            prefix(request),
            text("id")
        ));
    }
    lines
}

fn prefix(value: &Value) -> String {
    value
        .get("execution_seq")
        .and_then(Value::as_i64)
        .map_or_else(|| "[session]".to_owned(), |seq| format!("[exec {seq}]"))
}

fn message_line(harness: &str, message: &Value) -> String {
    let text = |name: &str| {
        message
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let flag = |name: &str| message.get(name).and_then(Value::as_bool) == Some(true);
    let body = match text("kind") {
        "assistant" => format!("{harness:<6} {}", text("text")),
        "tool_call" => format!("tool   {}  {}", text("tool"), text("summary")),
        "tool_result" => format!(
            "{}  {}  {}",
            if flag("ok") { "ok   " } else { "fail " },
            text("tool"),
            text("summary")
        ),
        "approval" => {
            let verb = match text("decision") {
                "approved" => "allow",
                "denied" => "deny",
                other => other,
            };
            let subject = match message.get("request_id").and_then(Value::as_str) {
                Some(request_id) => request_id.to_owned(),
                None => format!("{}  {}", text("tool"), text("summary")),
            };
            format!("{verb:<6} {subject}  by {}", text("by"))
        }
        "steer" => {
            let stage = if flag("expired") {
                "expired"
            } else if flag("delivered") {
                "delivered"
            } else {
                "accepted"
            };
            format!("steer  {stage}  {}", text("text"))
        }
        "notice" => format!("note   {}", text("text")),
        other => format!("{other}  {message}"),
    };
    format!("{} {body}", prefix(message))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::render;

    #[test]
    fn a_bucket_reads_as_the_documented_transcript() {
        let bucket = json!({
            "session": {"harness": "claude", "state": "idle"},
            "requests": [
                {"id": "r4", "kind": "approval", "execution_seq": 7, "tool": "Bash",
                 "summary": "git push --force origin main"},
            ],
            "results": [
                {"execution_seq": 6, "status": "canceled", "final_text_offset": 0},
                {"execution_seq": 7, "status": "completed", "final_text_offset": 0},
            ],
            "messages": [
                {"seq": 0, "execution_seq": 6, "kind": "notice", "text": "earlier"},
                {"seq": 1, "execution_seq": 7, "kind": "tool_call", "tool": "Bash",
                 "summary": "cargo test"},
                {"seq": 2, "execution_seq": 7, "kind": "tool_result", "tool": "Bash",
                 "ok": true, "summary": "42 passed"},
                {"seq": 3, "execution_seq": 7, "kind": "assistant", "text": "Found one race."},
                {"seq": 4, "execution_seq": 7, "kind": "approval", "decision": "denied",
                 "by": "caller", "request_id": "r3", "tool": "Bash", "summary": "x"},
            ],
        });
        let mut asked = HashSet::new();
        let mut held = Vec::new();
        assert_eq!(
            render(&bucket, &mut asked, &mut held, false, true),
            [
                "[exec 6] note   earlier",
                "[exec 6] done   canceled",
                "[exec 7] tool   Bash  cargo test",
                "[exec 7] ok     Bash  42 passed",
                "[exec 7] claude Found one race.",
                "[exec 7] deny   r3  by caller",
                "[exec 7] done   completed",
                "[exec 7] ask    r4  Bash  git push --force origin main",
            ]
        );
        assert!(
            render(&bucket, &mut asked, &mut held, true, true)
                .iter()
                .all(|line| !line.contains("ask") && !line.contains("done")),
            "a Request is announced once and a baseline result is history"
        );
    }

    #[test]
    fn a_result_waits_for_its_messages_on_later_pages() {
        let first = json!({
            "session": {"harness": "codex"},
            "results": [{"execution_seq": 1, "status": "completed", "final_text_offset": 0}],
            "messages": [{"seq": 0, "execution_seq": 1, "kind": "assistant", "text": "one"}],
        });
        let second = json!({
            "session": {"harness": "codex"},
            "messages": [{"seq": 1, "execution_seq": 1, "kind": "assistant", "text": "two"}],
        });
        let mut asked = HashSet::new();
        let mut held = Vec::new();
        let mut lines = render(&first, &mut asked, &mut held, false, false);
        lines.extend(render(&second, &mut asked, &mut held, false, true));
        assert_eq!(
            lines,
            [
                "[exec 1] codex  one",
                "[exec 1] codex  two",
                "[exec 1] done   completed",
            ]
        );
    }
}
