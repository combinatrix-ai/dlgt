use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

#[cfg(test)]
use crate::protocol::InputRecord;
use crate::protocol::{EventRecord, SessionRecord, TurnRecord};

pub struct Store {
    connection: Connection,
}

const OUTPUT_CHUNK_LIMIT: i64 = 8192;

pub struct OutputPage {
    pub data: Vec<u8>,
    pub next_after: i64,
    pub has_more: bool,
}

pub struct NewSession<'a> {
    pub id: &'a str,
    pub alias: &'a str,
    pub title: &'a str,
    pub agent: &'a str,
    pub cwd: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("failed to configure SQLite busy timeout")?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS sessions (
                   id TEXT PRIMARY KEY,
                   alias TEXT NOT NULL,
                   title TEXT NOT NULL,
                   agent TEXT NOT NULL,
                   cwd TEXT NOT NULL,
                   state TEXT NOT NULL,
                   model TEXT,
                   effort TEXT,
                   terminal_rows INTEGER NOT NULL DEFAULT 24,
                   terminal_cols INTEGER NOT NULL DEFAULT 80,
                   provider_session_id TEXT,
                   active_turn_id TEXT,
                   pid INTEGER,
                   created_at_ms INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS turns (
                   id TEXT PRIMARY KEY,
                   session_id TEXT NOT NULL REFERENCES sessions(id),
                   execution_seq INTEGER NOT NULL,
                   prompt TEXT NOT NULL,
                   state TEXT NOT NULL,
                   provider_turn_id TEXT,
                   final_message TEXT,
                   error TEXT,
                   created_at_ms INTEGER NOT NULL,
                   started_at_ms INTEGER,
                   completed_at_ms INTEGER
                   ,usage_json TEXT
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT,
                   turn_id TEXT,
                   kind TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS inputs (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL,
                   turn_id TEXT,
                   source TEXT NOT NULL,
                   data BLOB NOT NULL,
                   display TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS output_chunks (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL,
                   data BLOB NOT NULL,
                   created_at_ms INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS events_session_seq
                   ON events(session_id, seq);
                 CREATE INDEX IF NOT EXISTS output_session_seq
                   ON output_chunks(session_id, seq);
                 CREATE UNIQUE INDEX IF NOT EXISTS active_session_alias
                   ON sessions(alias) WHERE state NOT IN ('stopped', 'failed');
                 CREATE UNIQUE INDEX IF NOT EXISTS execution_session_seq
                   ON turns(session_id, execution_seq);",
            )
            .context("failed to migrate SQLite schema")?;
        Ok(Self { connection })
    }

    pub fn reconcile_after_restart(&self) -> Result<()> {
        let now = now_ms();
        self.connection.execute(
            "UPDATE sessions SET
             state = 'stopped', pid = NULL,
             active_turn_id = NULL, updated_at_ms = ?1
             WHERE state NOT IN ('stopped', 'failed')",
            [now],
        )?;
        self.connection.execute(
            "UPDATE turns SET state = 'interrupted', completed_at_ms = ?1,
             error = COALESCE(error, 'dlgt daemon restarted')
             WHERE state IN ('submitted', 'running')",
            [now],
        )?;
        Ok(())
    }

    pub fn insert_session(&self, session: &NewSession<'_>) -> Result<()> {
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO sessions (
               id, alias, title, agent, cwd, state, model, effort, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'starting', ?6, ?7, ?8, ?8)",
            params![
                session.id,
                session.alias,
                session.title,
                session.agent,
                session.cwd,
                session.model,
                session.effort,
                now
            ],
        )?;
        Ok(())
    }

    pub fn set_session_running(&self, id: &str, pid: Option<u32>) -> Result<bool> {
        let updated = self.connection.execute(
            "UPDATE sessions SET
             state = CASE WHEN state = 'starting' THEN 'running' ELSE state END,
             pid = ?2, updated_at_ms = ?3
             WHERE id = ?1 AND state IN ('starting', 'idle')",
            params![id, pid.map(i64::from), now_ms()],
        )?;
        Ok(updated == 1)
    }

    pub fn set_session_state(&self, id: &str, state: &str) -> Result<bool> {
        let updated = self.connection.execute(
            "UPDATE sessions SET state = ?2, updated_at_ms = ?3
             WHERE id = ?1 AND state NOT IN ('stopped', 'failed')",
            params![id, state, now_ms()],
        )?;
        Ok(updated == 1)
    }

    pub fn set_session_stopped(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET
             state = 'stopped', pid = NULL,
             active_turn_id = NULL, updated_at_ms = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_session_failed(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET
             state = 'failed', pid = NULL,
             active_turn_id = NULL, updated_at_ms = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_session_provider_id(&self, id: &str, provider_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET provider_session_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
            params![id, provider_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_active_turn(&self, session_id: &str, turn_id: Option<&str>) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET active_turn_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
            params![session_id, turn_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_terminal_size(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET terminal_rows = ?2, terminal_cols = ?3, updated_at_ms = ?4 WHERE id = ?1",
            params![session_id, rows, cols, now_ms()],
        )?;
        Ok(())
    }

    pub fn terminal_size(&self, session_id: &str) -> Result<(u16, u16)> {
        self.connection
            .query_row(
                "SELECT terminal_rows, terminal_cols FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("failed to read terminal size")
    }

    pub fn get_session(&self, selector: &str) -> Result<Option<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, alias, title, agent, cwd, state, model, effort,
                    provider_session_id, active_turn_id, pid, created_at_ms, updated_at_ms
             FROM sessions WHERE id = ?1 OR
               (alias = ?1 AND state NOT IN ('stopped', 'failed'))
             ORDER BY created_at_ms DESC LIMIT 1",
        )?;
        statement
            .query_row([selector], session_from_row)
            .optional()
            .context("failed to read session")
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, alias, title, agent, cwd, state, model, effort,
                    provider_session_id, active_turn_id, pid, created_at_ms, updated_at_ms
             FROM sessions ORDER BY created_at_ms DESC",
        )?;
        let rows = statement.query_map([], session_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to list sessions")
    }

    pub fn insert_turn(&mut self, id: &str, session_id: &str, prompt: &str) -> Result<TurnRecord> {
        let now = now_ms();
        let transaction = self.connection.transaction()?;
        let claimed = transaction.execute(
            "UPDATE sessions SET active_turn_id = ?2, updated_at_ms = ?3
             WHERE id = ?1 AND active_turn_id IS NULL AND state = 'idle'",
            params![session_id, id, now],
        )?;
        if claimed == 0 {
            bail!("session already has an active turn or is not ready");
        }
        let execution_seq: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(execution_seq), 0) + 1 FROM turns WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO turns (id, session_id, execution_seq, prompt, state, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'submitted', ?5)",
            params![id, session_id, execution_seq, prompt, now],
        )?;
        transaction.commit()?;
        self.get_turn(id)?
            .context("turn disappeared after insertion")
    }

    pub fn get_turn(&self, id: &str) -> Result<Option<TurnRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, session_id, execution_seq, prompt, state, provider_turn_id,
                    final_message, error, created_at_ms, started_at_ms, completed_at_ms, usage_json
             FROM turns WHERE id = ?1",
        )?;
        statement
            .query_row([id], turn_from_row)
            .optional()
            .context("failed to read turn")
    }

    pub fn latest_turn(&self, session_id: &str) -> Result<Option<TurnRecord>> {
        let id = self
            .connection
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 ORDER BY execution_seq DESC LIMIT 1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map_or(Ok(None), |id| self.get_turn(&id))
    }

    pub fn mark_turn_started(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let updated = self.connection.execute(
            "UPDATE turns SET state = 'running', provider_turn_id = COALESCE(?2, provider_turn_id),
             started_at_ms = COALESCE(started_at_ms, ?3)
             WHERE id = ?1 AND state = 'submitted'",
            params![id, provider_turn_id, now_ms()],
        )?;
        Ok(updated == 1)
    }

    pub fn complete_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        final_message: Option<&str>,
    ) -> Result<bool> {
        self.finish_turn_if_matching(id, provider_turn_id, "completed", final_message, None)
    }

    pub fn finish_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        state: &str,
        final_message: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        if !matches!(state, "completed" | "failed" | "interrupted") {
            bail!("invalid terminal turn state {state:?}");
        }
        let turn = self.get_turn(id)?.context("turn not found")?;
        if !matches!(turn.state.as_str(), "submitted" | "running")
            || provider_turn_id.is_some()
                && turn.provider_turn_id.is_some()
                && turn.provider_turn_id.as_deref() != provider_turn_id
        {
            return Ok(false);
        }
        let updated = self.connection.execute(
            "UPDATE turns SET state = ?2, provider_turn_id = COALESCE(?3, provider_turn_id),
             final_message = ?4, error = ?5, completed_at_ms = ?6
             WHERE id = ?1 AND state IN ('submitted', 'running')",
            params![id, state, provider_turn_id, final_message, error, now_ms()],
        )?;
        if updated == 1 {
            self.set_active_turn(&turn.session_id, None)?;
        }
        Ok(updated == 1)
    }

    pub fn interrupt_active_turn(&self, session_id: &str, error: &str) -> Result<Option<String>> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        let Some(turn_id) = session.active_turn_id else {
            return Ok(None);
        };
        let updated = self.connection.execute(
            "UPDATE turns SET state = 'interrupted', error = ?2, completed_at_ms = ?3
             WHERE id = ?1 AND state IN ('submitted', 'running')",
            params![turn_id, error, now_ms()],
        )?;
        if updated == 1 {
            self.set_active_turn(session_id, None)?;
            Ok(Some(turn_id))
        } else {
            Ok(None)
        }
    }

    pub fn cancel_turn(&mut self, id: &str) -> Result<bool> {
        let turn = self.get_turn(id)?.context("turn not found")?;
        let transaction = self.connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE turns SET state = 'canceled', completed_at_ms = ?2
             WHERE id = ?1 AND state IN ('submitted', 'running')",
            params![id, now_ms()],
        )?;
        if updated == 1 {
            transaction.execute(
                "UPDATE sessions SET state = 'quiescing', updated_at_ms = ?3
                 WHERE id = ?1 AND active_turn_id = ?2",
                params![turn.session_id, id, now_ms()],
            )?;
        }
        transaction.commit()?;
        Ok(updated == 1)
    }

    pub fn settle_canceled_turn(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let turn = self.get_turn(id)?.context("turn not found")?;
        if turn.state != "canceled"
            || provider_turn_id.is_some()
                && turn.provider_turn_id.is_some()
                && turn.provider_turn_id.as_deref() != provider_turn_id
        {
            return Ok(false);
        }
        let updated = self.connection.execute(
            "UPDATE sessions SET active_turn_id = NULL, state = 'idle', updated_at_ms = ?3
             WHERE id = ?1 AND active_turn_id = ?2 AND state = 'quiescing'",
            params![turn.session_id, id, now_ms()],
        )?;
        Ok(updated == 1)
    }

    pub fn record_event(
        &self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        kind: &str,
        payload: &Value,
    ) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO events (session_id, turn_id, kind, payload_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, turn_id, kind, payload.to_string(), now_ms()],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn record_input(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        source: &str,
        data: &[u8],
    ) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO inputs (session_id, turn_id, source, data, display, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                turn_id,
                source,
                data,
                display_bytes(data),
                now_ms()
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn record_output(&self, session_id: &str, data: &[u8]) -> Result<()> {
        self.connection.execute(
            "INSERT INTO output_chunks (session_id, data, created_at_ms) VALUES (?1, ?2, ?3)",
            params![session_id, data, now_ms()],
        )?;
        self.connection.execute(
            "DELETE FROM output_chunks
             WHERE session_id = ?1 AND seq <= (
               SELECT seq FROM output_chunks
               WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2
             )",
            params![session_id, OUTPUT_CHUNK_LIMIT],
        )?;
        Ok(())
    }

    pub fn read_output_page(
        &self,
        session_id: &str,
        after: i64,
        limit_bytes: usize,
    ) -> Result<OutputPage> {
        let mut statement = self.connection.prepare(
            "SELECT seq, data FROM output_chunks
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![session_id, after])?;
        let mut output = Vec::new();
        let mut next_after = after;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let data: Vec<u8> = row.get(1)?;
            if !output.is_empty() && output.len().saturating_add(data.len()) > limit_bytes {
                break;
            }
            output.extend(data);
            next_after = seq;
            if output.len() >= limit_bytes {
                break;
            }
        }
        let has_more = self.connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM output_chunks WHERE session_id = ?1 AND seq > ?2
             )",
            params![session_id, next_after],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(OutputPage {
            data: output,
            next_after,
            has_more,
        })
    }

    #[cfg(test)]
    pub fn read_inputs(&self, session_id: &str, after: i64) -> Result<Vec<InputRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, session_id, turn_id, source, data, display, created_at_ms
             FROM inputs WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![session_id, after], |row| {
            let data: Vec<u8> = row.get(4)?;
            Ok(InputRecord {
                seq: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                source: row.get(3)?,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                display: row.get(5)?,
                byte_len: data.len(),
                created_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read input log")
    }

    pub fn read_events(&self, session_id: Option<&str>, after: i64) -> Result<Vec<EventRecord>> {
        let sql = if session_id.is_some() {
            "SELECT seq, session_id, turn_id, kind, payload_json, created_at_ms
             FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq"
        } else {
            "SELECT seq, session_id, turn_id, kind, payload_json, created_at_ms
             FROM events WHERE ?1 IS NULL AND seq > ?2 ORDER BY seq"
        };
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(params![session_id, after], |row| {
            let payload_json: String = row.get(4)?;
            Ok(EventRecord {
                seq: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                kind: row.get(3)?,
                payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
                created_at_ms: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read events")
    }
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let pid = row
        .get::<_, Option<i64>>(10)?
        .and_then(|value| u32::try_from(value).ok());
    Ok(SessionRecord {
        id: row.get(0)?,
        alias: row.get(1)?,
        title: row.get(2)?,
        agent: row.get(3)?,
        cwd: row.get(4)?,
        state: row.get(5)?,
        model: row.get(6)?,
        effort: row.get(7)?,
        provider_session_id: row.get(8)?,
        active_turn_id: row.get(9)?,
        pid,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
    })
}

fn turn_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRecord> {
    Ok(TurnRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        execution_seq: row.get(2)?,
        prompt: row.get(3)?,
        state: row.get(4)?,
        provider_turn_id: row.get(5)?,
        final_message: row.get(6)?,
        error: row.get(7)?,
        created_at_ms: row.get(8)?,
        started_at_ms: row.get(9)?,
        completed_at_ms: row.get(10)?,
        usage: row
            .get::<_, Option<String>>(11)?
            .and_then(|value| serde_json::from_str(&value).ok()),
    })
}

pub fn now_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_millis());
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

pub fn display_bytes(data: &[u8]) -> String {
    let mut display = String::new();
    for &byte in data {
        match byte {
            b'\n' => display.push_str("\\n"),
            b'\r' => display.push_str("\\r"),
            b'\t' => display.push_str("\\t"),
            0x20..=0x7e => display.push(char::from(byte)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(display, "\\x{byte:02x}");
            }
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::{NewSession, Store, display_bytes};
    use serde_json::json;

    fn mark_ready(store: &Store, session_id: &str) {
        assert!(
            store
                .set_session_running(session_id, Some(42))
                .unwrap_or_else(|error| panic!("failed to start session: {error}"))
        );
        assert!(
            store
                .set_session_state(session_id, "idle")
                .unwrap_or_else(|error| panic!("failed to mark session ready: {error}"))
        );
    }

    #[test]
    fn persists_session_turn_event_and_input() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@test",
                title: "test",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        let turn = store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.state, "submitted");
        store
            .record_event(Some("ses_1"), Some("turn_1"), "turn.submitted", &json!({}))
            .unwrap_or_else(|error| panic!("failed to record event: {error}"));
        store
            .record_input("ses_1", Some("turn_1"), "api", b"hello\r")
            .unwrap_or_else(|error| panic!("failed to record input: {error}"));
        assert_eq!(
            store
                .read_inputs("ses_1", 0)
                .unwrap_or_else(|error| panic!("failed to read inputs: {error}"))[0]
                .display,
            "hello\\r"
        );
    }

    #[test]
    fn formats_control_bytes_without_losing_data() {
        assert_eq!(display_bytes(b"a\n\x1b"), "a\\n\\x1b");
    }

    #[test]
    fn stopped_session_releases_its_alias() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_old",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_old");
        store
            .set_session_stopped("ses_old")
            .unwrap_or_else(|error| panic!("failed to stop session: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_new",
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to reuse alias: {error}"));
        let archived = store
            .get_session("ses_old")
            .unwrap_or_else(|error| panic!("failed to read old session: {error}"))
            .unwrap_or_else(|| panic!("old session missing"));
        assert_eq!(archived.alias, "@worker");
        assert_eq!(
            store
                .get_session("@worker")
                .unwrap_or_else(|error| panic!("failed to read new session: {error}"))
                .unwrap_or_else(|| panic!("new session missing"))
                .id,
            "ses_new"
        );
    }

    #[test]
    fn process_exit_interrupts_active_turn() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        let interrupted = store
            .interrupt_active_turn("ses_1", "agent exited")
            .unwrap_or_else(|error| panic!("failed to interrupt turn: {error}"));
        assert_eq!(interrupted.as_deref(), Some("turn_1"));
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, "interrupted");
        assert_eq!(turn.error.as_deref(), Some("agent exited"));
    }

    #[test]
    fn output_reads_are_paginated() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store
            .record_output("ses_1", b"first")
            .unwrap_or_else(|error| panic!("failed to write first output: {error}"));
        store
            .record_output("ses_1", b"second")
            .unwrap_or_else(|error| panic!("failed to write second output: {error}"));
        let first = store
            .read_output_page("ses_1", 0, 5)
            .unwrap_or_else(|error| panic!("failed to read first page: {error}"));
        assert_eq!(first.data, b"first");
        assert!(first.has_more);
        let second = store
            .read_output_page("ses_1", first.next_after, 5)
            .unwrap_or_else(|error| panic!("failed to read second page: {error}"));
        assert_eq!(second.data, b"second");
        assert!(!second.has_more);
    }

    #[test]
    fn only_one_turn_can_claim_a_session() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(store.insert_turn("turn_2", "ses_1", "second").is_err());
    }

    #[test]
    fn claude_cannot_claim_a_turn_before_session_start() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_claude",
                alias: "@claude",
                title: "claude",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(
            store
                .set_session_running("ses_claude", Some(42))
                .unwrap_or_else(|error| panic!("failed to start session: {error}"))
        );

        assert!(store.insert_turn("turn_1", "ses_claude", "first").is_err());
    }

    #[test]
    fn stop_must_match_a_running_provider_turn() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", Some("provider-1"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        assert!(
            !store
                .complete_turn_if_matching("turn_1", Some("provider-2"), Some("wrong"))
                .unwrap_or_else(|error| panic!("failed to reject stop: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("provider-1"), Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
    }

    #[test]
    fn late_cancel_cannot_clear_a_newer_active_turn() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", None)
                .unwrap_or_else(|error| panic!("failed to start first turn: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_1", None, Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete first turn: {error}"))
        );
        store
            .insert_turn("turn_2", "ses_1", "second")
            .unwrap_or_else(|error| panic!("failed to insert second turn: {error}"));
        assert!(
            !store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to reject late cancel: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_2"));
    }

    #[test]
    fn active_cancel_waits_for_provider_quiescence() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let mut store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        store
            .set_session_state("ses_1", "busy")
            .unwrap_or_else(|error| panic!("failed to mark session busy: {error}"));
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel turn: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "quiescing");
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_1"));
        assert!(
            store
                .settle_canceled_turn("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to settle canceled turn: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read settled session: {error}"))
            .unwrap_or_else(|| panic!("settled session missing"));
        assert_eq!(session.state, "idle");
        assert!(session.active_turn_id.is_none());
    }

    #[test]
    fn terminal_session_cannot_be_resurrected() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store
            .set_session_stopped("ses_1")
            .unwrap_or_else(|error| panic!("failed to stop session: {error}"));
        assert!(
            !store
                .set_session_state("ses_1", "idle")
                .unwrap_or_else(|error| panic!("failed to reject transition: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "stopped");
    }
}
