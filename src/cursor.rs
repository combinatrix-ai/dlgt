//! Forward-observation cursors: per-scope ordinal positions.
//!
//! A cursor is a plain number. Every acceptance, and every fetch response
//! whose watermark vector advanced, mints the addressed scope's next position
//! -- 1, 2, 3 -- and the daemon keeps the watermark vector behind it. A fetch
//! whose vector did not move returns the caller's own position.
//!
//! The vector used to be encoded into the token itself, which bought nothing:
//! it was bound to the daemon instance either way, because the state it points
//! at is memory-only. A compact ordinal keeps the caller's context small.
//!
//! A position is meaningful only within one daemon lifetime. Nothing marks a
//! number as belonging to a previous daemon, so a stale number resolves
//! against the current table -- returning the current daemon's window of that
//! position, or `CURSOR_EXPIRED` if it has not minted that far. That behavior
//! is defined but potentially lossy: used as a starting position, a stale
//! number skips whatever the current daemon recorded before that window.
//! Callers must therefore discard remembered numbers after a restart and
//! re-enter through a resume acceptance cursor or a cursorless baseline.
//!
//! Scope binds to the immutable internal Session UID, never to the public
//! Session ID, because Claude rotates that ID on rekey.

use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};

/// Vectors retained per single-Session scope.
const MAX_SESSION_CURSORS: usize = 64;
/// Vectors retained across every scope.
const MAX_TOTAL_CURSORS: usize = 4096;

/// The watermarks one observation position stands for. Internal: a caller
/// only ever sees the position number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Global lifecycle event watermark.
    pub e: i64,
    /// Watermarks for the addressed Session.
    pub p: SessionCursor,
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
    pub const fn new() -> Self {
        Self {
            e: 0,
            p: SessionCursor {
                r: 0,
                ro: 0,
                ep: 0,
                x: 0,
                px: None,
                po: 0,
            },
        }
    }

    pub const fn session(&self) -> SessionCursor {
        self.p
    }

    pub const fn set_session(&mut self, session: SessionCursor) {
        self.p = session;
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
    pub fn store(&mut self, scope: &str, number: u64, cursor: Cursor) {
        self.entries.insert((scope.to_owned(), number), cursor);
        self.issued.push_back((scope.to_owned(), number));
        let limit = MAX_SESSION_CURSORS;
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
    }

    /// Resolve a caller-supplied position within one scope.
    ///
    /// The lookup is `(scope, number)` and nothing else, so a number minted by
    /// a previous daemon is not detectable and resolves as this daemon's
    /// position of the same number. The window returned is always current
    /// data, never a stale world -- but as a starting position a stale number
    /// skips whatever this daemon recorded before it, so the documented rule
    /// is to discard remembered numbers after a restart.
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
}

#[cfg(test)]
mod tests {
    use super::{Cursor, CursorTable, MAX_SESSION_CURSORS, SessionCursor};

    fn sample(_scope: &str, row: u64) -> Cursor {
        let mut cursor = Cursor::new();
        cursor.e = 104;
        cursor.set_session(SessionCursor {
            r: row,
            ro: 0,
            ep: 3,
            x: 7,
            px: Some(8),
            po: 4_096,
        });
        cursor
    }

    fn mint(table: &mut CursorTable, scope: &str, row: u64) -> u64 {
        let number = table.reserve(scope);
        table.store(scope, number, sample(scope, row));
        number
    }

    #[test]
    fn positions_count_up_from_one_per_scope() {
        let mut table = CursorTable::default();
        assert_eq!(mint(&mut table, "su_abc", 1), 1);
        assert_eq!(mint(&mut table, "su_abc", 2), 2);
        assert_eq!(mint(&mut table, "su_abc", 3), 3);
        // A second Session numbers independently.
        assert_eq!(mint(&mut table, "su_other", 1), 1);
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

        // Well formed but never minted.
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
    }

    #[test]
    fn a_position_from_another_table_resolves_against_this_one() {
        // Nothing distinguishes a number minted by a previous daemon. It names
        // this table's position of the same number, which is current data.
        let mut old = CursorTable::default();
        let number = mint(&mut old, "su_abc", 111);

        let mut fresh = CursorTable::default();
        assert!(
            fresh
                .resolve("su_abc", &number.to_string())
                .err()
                .is_some_and(|error| error.to_string().contains("CURSOR_EXPIRED")),
            "a position this table has not minted is simply not held"
        );
        mint(&mut fresh, "su_abc", 222);
        assert_eq!(
            fresh
                .resolve("su_abc", &number.to_string())
                .unwrap_or_else(|error| panic!("failed to resolve: {error}")),
            sample("su_abc", 222),
            "the number names this table's window, not the other table's"
        );
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
}
