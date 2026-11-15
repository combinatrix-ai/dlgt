//! Messages: what a provider reported during a Session, in order, reduced to
//! a bounded form. They replace the rendered terminal screen as the way to
//! follow progress. The authoritative answer is always the retained result.

use std::collections::VecDeque;

use serde::Serialize;

/// Longest `text` or `summary` a message carries.
pub const TEXT_LIMIT: usize = 4096;
/// Messages retained per Session before the oldest are evicted.
pub const MESSAGE_RETENTION: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Denied,
    TimedOut,
    Aborted,
}

impl Decision {
    /// Whether the worker was refused the action it asked for.
    pub const fn refuses(self) -> bool {
        matches!(self, Self::Denied | Self::TimedOut)
    }
}

/// Who decided an approval. dlgt is never one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    ProviderReviewer,
    Caller,
    ProviderPolicy,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageBody {
    Assistant {
        text: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    ToolCall {
        tool: String,
        summary: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    ToolResult {
        tool: String,
        ok: bool,
        summary: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Approval {
        decision: Decision,
        by: Decider,
        #[serde(skip_serializing_if = "Option::is_none")]
        risk: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        tool: String,
        summary: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Steer {
        delivered: bool,
        /// The execution ended, or was canceled, before the provider read it.
        #[serde(skip_serializing_if = "is_false")]
        expired: bool,
        text: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Notice {
        text: String,
        #[serde(skip_serializing_if = "is_false")]
        truncated: bool,
    },
}

impl MessageBody {
    pub fn assistant(text: &str) -> Self {
        let (text, truncated) = bounded(text);
        Self::Assistant { text, truncated }
    }

    pub fn tool_call(tool: &str, summary: &str) -> Self {
        let (summary, truncated) = one_line(summary);
        Self::ToolCall {
            tool: tool.to_owned(),
            summary,
            truncated,
        }
    }

    pub fn tool_result(tool: &str, ok: bool, summary: &str) -> Self {
        let (summary, truncated) = one_line(summary);
        Self::ToolResult {
            tool: tool.to_owned(),
            ok,
            summary,
            truncated,
        }
    }

    pub fn approval(
        decision: Decision,
        by: Decider,
        risk: Option<&str>,
        request_id: Option<&str>,
        tool: &str,
        summary: &str,
    ) -> Self {
        let (summary, truncated) = one_line(summary);
        Self::Approval {
            decision,
            by,
            risk: risk.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
            tool: tool.to_owned(),
            summary,
            truncated,
        }
    }

    pub fn steer(text: &str, delivered: bool, expired: bool) -> Self {
        let (text, truncated) = bounded(text);
        Self::Steer {
            delivered,
            expired,
            text,
            truncated,
        }
    }

    pub fn notice(text: &str) -> Self {
        let (text, truncated) = bounded(text);
        Self::Notice { text, truncated }
    }

    pub const fn refuses(&self) -> bool {
        matches!(self, Self::Approval { decision, .. } if decision.refuses())
    }
}

/// Truncate at a character boundary within [`TEXT_LIMIT`] bytes.
pub fn bounded(text: &str) -> (String, bool) {
    if text.len() <= TEXT_LIMIT {
        return (text.to_owned(), false);
    }
    let mut end = TEXT_LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// Collapse a value to one bounded line.
pub fn one_line(text: &str) -> (String, bool) {
    let mut line = String::with_capacity(text.len().min(TEXT_LIMIT));
    let mut truncated = false;
    for word in text.split_whitespace() {
        if line.len() + word.len() + 1 > TEXT_LIMIT {
            truncated = true;
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if truncated && line.is_empty() {
        return bounded(text.trim_start());
    }
    (line, truncated)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Per-Session ordinal, starting at 1.
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_seq: Option<i64>,
    #[serde(flatten)]
    pub body: MessageBody,
}

#[derive(Debug, Default)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub has_more: bool,
    /// Retention evicted part of the requested window.
    pub gap: bool,
    /// Sequence immediately before `messages[0]`, or the log head when the
    /// page is empty.
    pub start: u64,
}

#[derive(Debug, Default)]
pub struct MessageLog {
    messages: VecDeque<Message>,
    head: u64,
    /// Highest sequence already dropped by retention.
    evicted: u64,
}

impl MessageLog {
    pub fn push(&mut self, execution_seq: Option<i64>, body: MessageBody) -> u64 {
        self.head += 1;
        self.messages.push_back(Message {
            seq: self.head,
            execution_seq,
            body,
        });
        while self.messages.len() > MESSAGE_RETENTION {
            if let Some(evicted) = self.messages.pop_front() {
                self.evicted = evicted.seq;
            }
        }
        self.head
    }

    pub const fn head(&self) -> u64 {
        self.head
    }

    pub fn page_after(&self, after: u64, limit: usize) -> MessagePage {
        let gap = after < self.evicted;
        let after = after.max(self.evicted);
        let mut pending = self.messages.iter().filter(|message| message.seq > after);
        let messages = pending.by_ref().take(limit).cloned().collect::<Vec<_>>();
        MessagePage {
            has_more: pending.next().is_some(),
            gap,
            start: after.min(self.head),
            messages,
        }
    }

    pub fn tail(&self, limit: usize) -> MessagePage {
        let skip = self.messages.len().saturating_sub(limit);
        let messages = self.messages.iter().skip(skip).cloned().collect::<Vec<_>>();
        MessagePage {
            start: messages.first().map_or(self.head, |first| first.seq - 1),
            has_more: false,
            gap: false,
            messages,
        }
    }

    /// Append another log's messages, renumbered, when two launch records
    /// turn out to be one logical Session.
    pub fn absorb(&mut self, other: Self) {
        for message in other.messages {
            self.push(message.execution_seq, message.body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Decider, Decision, MESSAGE_RETENTION, MessageBody, MessageLog, TEXT_LIMIT};

    #[test]
    fn long_text_is_truncated_at_a_character_boundary_and_flagged() {
        let text = "é".repeat(TEXT_LIMIT);
        let value = serde_json::to_value(MessageBody::assistant(&text)).unwrap_or_default();
        let kept = value["text"].as_str().unwrap_or_default();
        assert!(kept.len() <= TEXT_LIMIT);
        assert!(text.starts_with(kept));
        assert_eq!(value["truncated"], true);

        let short = serde_json::to_value(MessageBody::assistant("ok")).unwrap_or_default();
        assert!(short.get("truncated").is_none());
    }

    #[test]
    fn a_summary_is_one_line() {
        let value = serde_json::to_value(MessageBody::tool_call("Bash", "cargo test\n  --all\n"))
            .unwrap_or_default();
        assert_eq!(value["kind"], "tool_call");
        assert_eq!(value["summary"], "cargo test --all");
    }

    #[test]
    fn an_approval_names_its_reviewer_and_never_dlgt() {
        let value = serde_json::to_value(MessageBody::approval(
            Decision::Denied,
            Decider::Caller,
            None,
            Some("r3"),
            "Bash",
            "git push --force origin main",
        ))
        .unwrap_or_default();
        assert_eq!(value["decision"], "denied");
        assert_eq!(value["by"], "caller");
        assert_eq!(value["request_id"], "r3");
        assert!(value.get("risk").is_none());
    }

    #[test]
    fn pages_are_ascending_and_report_what_is_left() {
        let mut log = MessageLog::default();
        for index in 0..5 {
            log.push(Some(1), MessageBody::assistant(&index.to_string()));
        }
        let page = log.page_after(1, 2);
        assert_eq!(
            page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(page.has_more);
        assert!(!page.gap);
        let rest = log.page_after(3, 64);
        assert_eq!(rest.messages.len(), 2);
        assert!(!rest.has_more);
        assert_eq!(log.tail(2).start, 3);
    }

    #[test]
    fn eviction_is_reported_as_a_gap_instead_of_a_silent_skip() {
        let mut log = MessageLog::default();
        for _ in 0..MESSAGE_RETENTION + 10 {
            log.push(None, MessageBody::notice("n"));
        }
        assert!(log.page_after(5, 1).gap);
        assert!(!log.page_after(10, 1).gap);
        assert_eq!(log.page_after(5, 1).messages[0].seq, 11);
    }
}
