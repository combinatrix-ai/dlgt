//! Forward-observation cursors: per-scope ordinal positions.
//!
//! A cursor is a plain number. Every acceptance and every fetch response mints
//! the addressed scope's next position -- 1, 2, 3 -- and the daemon keeps the
//! watermark vector behind it.
//!
//! The vector used to be encoded into the token itself, which bought nothing:
//! it was bound to the daemon instance either way, because the state it points
//! at is memory-only. What it cost was 200-300 characters per response, and up
//! to tens of kilobytes for a daemon-wide scope -- context an LLM caller has to
//! carry across every turn and reliably loses to compaction. A one- or
//! two-digit number survives that.
//!
//! Restart safety is by construction rather than by a boot identifier: a new
//! daemon mints fresh Session UIDs, so a number from a previous boot has
//! nothing to resolve against and reads as expired.
//!
//! Scope binds to the immutable internal Session UID, never to the public
//! Session ID, because Claude rotates that ID on rekey.

use std::collections::{BTreeMap, HashMap, VecDeque};

use anyhow::{Result, bail};

/// Scope covering every Session owned by one daemon.
pub const SCOPE_ALL: &str = "all";

/// Vectors retained per single-Session scope.
const MAX_SESSION_CURSORS: usize = 64;
/// Vectors retained for the daemon-wide scope, which pages further.
const MAX_ALL_CURSORS: usize = 256;
/// Vectors retained across every scope.
const MAX_TOTAL_CURSORS: usize = 4096;
/// Most per-Session watermarks one cursor may carry.
const MAX_SESSIONS: usize = 256;

/// The watermarks one observation position stands for. Internal: a caller
/// only ever sees the position number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// One Session UID, or `all`.
    pub s: String,
    /// Global lifecycle event watermark.
    pub e: i64,
    /// Baseline enumeration position for `all` scope: the Session UID after
    /// which the next baseline page resumes. Present only while a baseline is
    /// still paging, so a caller cannot lose the Sessions it has not seen.
    pub bl: Option<String>,
    /// Per-Session watermarks keyed by Session UID.
    pub p: BTreeMap<String, SessionCursor>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCursor {
    /// Stable absolute row watermark: the last row delivered in full.
    pub r: u64,
    /// Bytes of row `r + 1` a previous response already delivered. Non-zero
    /// only while one oversized row is being chunked across responses.
    pub ro: u64,
    /// Screen epoch observed when the cursor was issued.
    pub ep: u64,
    /// Highest fully delivered terminal result execution sequence.
    pub x: i64,
    /// Execution sequence of a partially delivered final text, if any.
    pub px: Option<i64>,
    /// Byte offset already delivered for that final text.
    pub po: u64,
}

impl Cursor {
    pub fn new(scope: &str) -> Self {
        Self {
            s: scope.to_owned(),
            e: 0,
            bl: None,
            p: BTreeMap::new(),
        }
    }

    pub fn session(&self, uid: &str) -> SessionCursor {
        self.p.get(uid).copied().unwrap_or_default()
    }

    pub fn set_session(&mut self, uid: &str, session: SessionCursor) {
        self.p.insert(uid.to_owned(), session);
    }
}

/// Ordinal positions and the vectors behind them.
///
/// Numbering is per scope, so a number denotes *that* scope's Nth observation
/// position. Using one scope's number against another is therefore not an
/// error: it names a genuine position in the scope addressed. The cursor is a
/// position, not a capability; `request_id` carries idempotency.
#[derive(Default)]
pub struct CursorTable {
    entries: HashMap<(String, u64), Cursor>,
    next: HashMap<String, u64>,
    /// Issue order per scope, so one busy Session cannot evict another's.
    order: HashMap<String, VecDeque<u64>>,
    /// Issue order across all scopes, for the total bound.
    issued: VecDeque<(String, u64)>,
}

impl CursorTable {
    /// Take the scope's next position. The vector is stored afterwards, so a
    /// response can carry its own number while it is still being measured.
    pub fn reserve(&mut self, scope: &str) -> u64 {
        let next = self.next.entry(scope.to_owned()).or_insert(0);
        *next += 1;
        *next
    }

    /// Bind a reserved position to the watermarks it stands for. The vector
    /// behind a position is never mutated, which is what makes replaying a
    /// number return an identical window.
    pub fn store(&mut self, scope: &str, number: u64, cursor: Cursor) -> Result<()> {
        if cursor.p.len() > MAX_SESSIONS {
            bail!(
                "invalid scope: {} Sessions carry retained state, more than the {MAX_SESSIONS} one cursor can address; fetch Sessions individually instead of --all",
                cursor.p.len()
            );
        }
        self.entries.insert((scope.to_owned(), number), cursor);
        self.issued.push_back((scope.to_owned(), number));
        let limit = if scope == SCOPE_ALL {
            MAX_ALL_CURSORS
        } else {
            MAX_SESSION_CURSORS
        };
        let order = self.order.entry(scope.to_owned()).or_default();
        order.push_back(number);
        let mut retired = Vec::new();
        while order.len() > limit {
            if let Some(expired) = order.pop_front() {
                retired.push(expired);
            }
        }
        for expired in retired {
            self.entries.remove(&(scope.to_owned(), expired));
        }
        while self.issued.len() > MAX_TOTAL_CURSORS {
            if let Some(expired) = self.issued.pop_front() {
                self.entries.remove(&expired);
            }
        }
        Ok(())
    }

    /// Resolve a caller-supplied position within one scope.
    ///
    /// Every failure is structured and non-zero, and the recovery for both is
    /// one cursorless baseline fetch.
    pub fn resolve(&self, scope: &str, value: &str) -> Result<Cursor> {
        let Ok(number) = value.parse::<u64>() else {
            bail!("CURSOR_INVALID: {value:?} is not a cursor position");
        };
        let Some(cursor) = self.entries.get(&(scope.to_owned(), number)) else {
            bail!(
                "CURSOR_EXPIRED: this daemon no longer holds position {number}; fetch without --cursor to recover"
            );
        };
        Ok(cursor.clone())
    }

    /// Forget everything a Session ever positioned, when it is replaced.
    pub fn forget(&mut self, scope: &str) {
        self.entries.retain(|(entry, _), _| entry != scope);
        self.order.remove(scope);
        self.next.remove(scope);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cursor, CursorTable, MAX_ALL_CURSORS, MAX_SESSION_CURSORS, SCOPE_ALL, SessionCursor,
    };

    fn sample(scope: &str, row: u64) -> Cursor {
        let mut cursor = Cursor::new(scope);
        cursor.e = 104;
        cursor.set_session(
            "su_abc",
            SessionCursor {
                r: row,
                ro: 0,
                ep: 3,
                x: 7,
                px: Some(8),
                po: 4_096,
            },
        );
        cursor
    }

    fn mint(table: &mut CursorTable, scope: &str, row: u64) -> u64 {
        let number = table.reserve(scope);
        table
            .store(scope, number, sample(scope, row))
            .unwrap_or_else(|error| panic!("failed to store: {error}"));
        number
    }

    #[test]
    fn positions_count_up_from_one_per_scope() {
        let mut table = CursorTable::default();
        assert_eq!(mint(&mut table, "su_abc", 1), 1);
        assert_eq!(mint(&mut table, "su_abc", 2), 2);
        assert_eq!(mint(&mut table, "su_abc", 3), 3);
        // A second Session numbers independently, and so does the daemon-wide
        // scope.
        assert_eq!(mint(&mut table, "su_other", 1), 1);
        assert_eq!(mint(&mut table, SCOPE_ALL, 1), 1);
        assert_eq!(mint(&mut table, SCOPE_ALL, 2), 2);
        assert_eq!(mint(&mut table, "su_abc", 4), 4);
    }

    #[test]
    fn a_position_resolves_to_the_identical_vector_every_time() {
        let mut table = CursorTable::default();
        mint(&mut table, "su_abc", 10);
        let second = mint(&mut table, "su_abc", 20);

        for _ in 0..3 {
            assert_eq!(
                table
                    .resolve("su_abc", &second.to_string())
                    .unwrap_or_else(|error| panic!("failed to resolve: {error}")),
                sample("su_abc", 20),
                "replaying a position must return the same watermarks"
            );
        }
        assert_eq!(
            table
                .resolve("su_abc", "1")
                .unwrap_or_else(|error| panic!("failed to resolve: {error}")),
            sample("su_abc", 10)
        );
    }

    #[test]
    fn a_position_names_the_addressed_scope_not_the_one_that_minted_it() {
        let mut table = CursorTable::default();
        mint(&mut table, "su_abc", 10);
        mint(&mut table, "su_other", 99);

        // Not an error: position 1 of su_other is a genuine position.
        assert_eq!(
            table
                .resolve("su_other", "1")
                .unwrap_or_else(|error| panic!("failed to resolve: {error}")),
            sample("su_other", 99)
        );
        // A position that Session never reached is simply not held.
        assert!(
            table
                .resolve("su_other", "2")
                .err()
                .is_some_and(|error| error.to_string().contains("CURSOR_EXPIRED"))
        );
    }

    #[test]
    fn a_non_numeric_position_is_invalid_and_an_unminted_one_is_expired() {
        let mut table = CursorTable::default();
        mint(&mut table, "su_abc", 10);

        for malformed in ["", "one", "c_AAAA", "f1.eyJ2IjoxfQ", "-1", "1.5", " 1"] {
            let error = table
                .resolve("su_abc", malformed)
                .err()
                .unwrap_or_else(|| panic!("{malformed:?} unexpectedly resolved"));
            assert!(
                error.to_string().contains("CURSOR_INVALID"),
                "{malformed:?} produced {error}"
            );
        }

        // Well formed but never minted, which is also what a number from a
        // previous daemon boot looks like: the table is memory-only and the
        // Session UIDs are new.
        for unminted in ["0", "2", "9999999999"] {
            let error = table
                .resolve("su_abc", unminted)
                .err()
                .unwrap_or_else(|| panic!("{unminted:?} unexpectedly resolved"));
            assert!(
                error.to_string().contains("CURSOR_EXPIRED"),
                "{unminted:?} produced {error}"
            );
        }
    }

    #[test]
    fn each_scope_keeps_its_own_bounded_history() {
        let mut table = CursorTable::default();
        let oldest = mint(&mut table, "su_abc", 1);
        for row in 0..MAX_SESSION_CURSORS {
            mint(&mut table, "su_abc", u64::try_from(row).unwrap_or(0));
        }
        assert!(
            table
                .resolve("su_abc", &oldest.to_string())
                .err()
                .is_some_and(|error| error.to_string().contains("CURSOR_EXPIRED"))
        );

        // A busy Session cannot evict another scope's positions.
        let other = mint(&mut table, "su_other", 7);
        for row in 0..MAX_SESSION_CURSORS {
            mint(&mut table, "su_abc", u64::try_from(row).unwrap_or(0));
        }
        assert!(table.resolve("su_other", &other.to_string()).is_ok());

        // The daemon-wide scope keeps more, because it pages further.
        let mut table = CursorTable::default();
        let first_all = mint(&mut table, SCOPE_ALL, 1);
        for row in 0..MAX_SESSION_CURSORS {
            mint(&mut table, SCOPE_ALL, u64::try_from(row).unwrap_or(0));
        }
        assert!(table.resolve(SCOPE_ALL, &first_all.to_string()).is_ok());
        for row in 0..MAX_ALL_CURSORS {
            mint(&mut table, SCOPE_ALL, u64::try_from(row).unwrap_or(0));
        }
        assert!(table.resolve(SCOPE_ALL, &first_all.to_string()).is_err());
        assert!(table.len() <= MAX_ALL_CURSORS);
    }

    #[test]
    fn a_replaced_session_forgets_its_positions() {
        let mut table = CursorTable::default();
        let number = mint(&mut table, "su_abc", 1);
        table.forget("su_abc");

        assert!(
            table
                .resolve("su_abc", &number.to_string())
                .err()
                .is_some_and(|error| error.to_string().contains("CURSOR_EXPIRED"))
        );
        assert_eq!(table.reserve("su_abc"), 1, "numbering restarts");
    }

    #[test]
    fn a_scope_wider_than_the_table_allows_is_rejected() {
        let mut cursor = Cursor::new(SCOPE_ALL);
        for index in 0..300 {
            cursor.set_session(
                &format!("su_{index}"),
                SessionCursor {
                    x: 1,
                    ..SessionCursor::default()
                },
            );
        }
        let mut table = CursorTable::default();
        let number = table.reserve(SCOPE_ALL);
        let error = table
            .store(SCOPE_ALL, number, cursor)
            .err()
            .unwrap_or_else(|| panic!("an unusable cursor was stored"));
        assert!(
            error.to_string().contains("invalid scope"),
            "unexpected error: {error}"
        );
    }
}
