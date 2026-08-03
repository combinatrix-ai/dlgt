//! Persistent incremental VT emulation per process generation.
//!
//! The raw PTY ring cannot back a forward cursor: eviction can cut mid escape
//! sequence, which changes the interpretation and the row indexing of every
//! remaining byte. This module keeps one live VT emulator per Session process
//! generation, promotes rows that scroll out of the live grid into an
//! append-only vector with monotonically increasing absolute row IDs, and
//! renders the live grid on demand.

use std::collections::VecDeque;

/// Stable rows retained per Session before the oldest rows are evicted.
pub const STABLE_ROW_RETENTION: usize = 10_000;
/// Live grid rows returned by a bounded observation.
pub const LIVE_ROW_LIMIT: usize = 40;

/// Scrollback used purely as a transfer buffer between the emulator and the
/// stable-row vector. Rows are promoted after every small feed, and the
/// emulator is rebased well before this bound, so no row can be evicted by
/// vt100 itself.
const TRANSFER_SCROLLBACK: usize = 1024;
const REBASE_THRESHOLD: usize = 512;
const FEED_PIECE: usize = 256;
/// Longest boundary sequence recognized by the scanner, minus one byte.
const BOUNDARY_CARRY: usize = 7;

/// A byte sequence the promotion accounting has to see, rather than discover
/// after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// History is destroyed: promote first, re-anchor, and start a new epoch.
    Reset(EpochReason),
    /// The main grid stops receiving output: promote everything it holds
    /// before the switch, or it stays invisible until the application leaves
    /// the alternate screen, which may be never.
    AlternateEnter,
    /// The main grid resumes: re-anchor and promote whatever it still holds.
    AlternateExit,
}

/// Sequences that move between the main and alternate grids. They are drain
/// boundaries, not resets: no epoch changes, because stable history is
/// untouched.
const BOUNDARIES: [(&[u8], Boundary); 8] = [
    (b"\x1bc", Boundary::Reset(EpochReason::TerminalReset)),
    (b"\x1b[3J", Boundary::Reset(EpochReason::EraseScrollback)),
    (b"\x1b[?1049h", Boundary::AlternateEnter),
    (b"\x1b[?1049l", Boundary::AlternateExit),
    (b"\x1b[?1047h", Boundary::AlternateEnter),
    (b"\x1b[?1047l", Boundary::AlternateExit),
    (b"\x1b[?47h", Boundary::AlternateEnter),
    (b"\x1b[?47l", Boundary::AlternateExit),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochReason {
    ProcessRestart,
    TerminalReset,
    EraseScrollback,
    Resize,
}

impl EpochReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessRestart => "process_restart",
            Self::TerminalReset => "terminal_reset",
            Self::EraseScrollback => "erase_scrollback",
            Self::Resize => "resize",
        }
    }
}

/// A bounded forward page of stable rows.
#[derive(Debug, Default)]
pub struct StablePage {
    pub lines: Vec<String>,
    /// Absolute row ID of the last delivered row, or the requested watermark
    /// when nothing was delivered.
    pub next_after: u64,
    pub has_more: bool,
    /// The requested watermark predates the retained floor.
    pub gap: bool,
}

pub struct ScreenStore {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    stable: VecDeque<String>,
    /// Absolute row ID of `stable.front()`.
    floor_row_id: u64,
    /// Absolute row ID that the next promoted row will receive.
    next_row_id: u64,
    /// Rows of the emulator scrollback already promoted.
    consumed: usize,
    epoch: u64,
    last_reset: Option<EpochReason>,
    carry: Vec<u8>,
}

impl ScreenStore {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, TRANSFER_SCROLLBACK),
            rows,
            cols,
            stable: VecDeque::new(),
            floor_row_id: 1,
            next_row_id: 1,
            consumed: 0,
            epoch: 1,
            last_reset: None,
            carry: Vec::new(),
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn last_reset_reason(&self) -> Option<EpochReason> {
        self.last_reset
    }

    /// Absolute row ID of the newest promoted row. Zero before any row is
    /// promoted, so it doubles as the initial cursor watermark.
    pub const fn head_row_id(&self) -> u64 {
        self.next_row_id - 1
    }

    pub const fn floor_row_id(&self) -> u64 {
        self.floor_row_id
    }

    pub const fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Feed PTY bytes. A boundary splits the feed immediately *before* its own
    /// bytes, so everything the boundary is about to hide or destroy is
    /// promoted first; only then is the boundary applied and the promotion
    /// accounting re-anchored.
    pub fn feed(&mut self, data: &[u8]) {
        let mut start = 0;
        for (begin, end, boundary) in self.scan_boundaries(data) {
            self.feed_pieces(&data[start..begin]);
            self.parser.process(&data[begin..end]);
            match boundary {
                Boundary::Reset(reason) => {
                    self.resync();
                    self.bump_epoch(reason);
                }
                // The main grid was drained by feed_pieces above, before the
                // switch took effect.
                Boundary::AlternateEnter => {}
                Boundary::AlternateExit => {
                    self.resync();
                    self.drain();
                }
            }
            start = end;
        }
        self.feed_pieces(&data[start..]);
    }

    /// Re-anchor the promoted-row count to the emulator's own scrollback
    /// without re-promoting anything, after a boundary changed which grid the
    /// count refers to or dropped it entirely.
    fn resync(&mut self) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        self.parser.screen_mut().set_scrollback(usize::MAX);
        self.consumed = self.consumed.min(self.parser.screen().scrollback());
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.drain();
        // vt100 unwraps every row when the column count changes, so the
        // rendering of history is no longer continuous with the live grid.
        let reflowed = cols != self.cols;
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        if reflowed {
            self.bump_epoch(EpochReason::Resize);
        }
    }

    /// Start a new process generation. A new PTY is always a new terminal
    /// generation even when the provider-qualified Session ID is unchanged.
    pub fn restart(&mut self) {
        // A provider that died inside the alternate screen still has main-grid
        // history that never scrolled out. Leave the alternate screen
        // logically first so that history is reachable; alternate contents are
        // ephemeral by design and are dropped.
        if self.parser.screen().alternate_screen() {
            self.parser.process(b"\x1b[?1049l\x1b[?1047l\x1b[?47l");
            self.resync();
        }
        self.drain();
        // The dead process's last screen never scrolled out, so promote it
        // before the emulator is replaced or it disappears from history.
        if !self.parser.screen().alternate_screen() {
            let (rows, _) = self.live_rows(usize::MAX);
            for row in rows {
                self.push_stable(row);
            }
        }
        self.parser = vt100::Parser::new(self.rows, self.cols, TRANSFER_SCROLLBACK);
        self.consumed = 0;
        self.carry.clear();
        self.bump_epoch(EpochReason::ProcessRestart);
    }

    /// Forward page of stable rows after `after`.
    pub fn stable_page(&self, after: u64, limit: usize) -> StablePage {
        let head = self.head_row_id();
        if after >= head {
            return StablePage {
                next_after: after.min(head),
                ..StablePage::default()
            };
        }
        let gap = after + 1 < self.floor_row_id;
        let start = after.max(self.floor_row_id - 1);
        let available = usize::try_from(head - start).unwrap_or(usize::MAX);
        let take = available.min(limit);
        let offset = usize::try_from(start + 1 - self.floor_row_id).unwrap_or(0);
        let lines = self
            .stable
            .iter()
            .skip(offset)
            .take(take)
            .cloned()
            .collect::<Vec<_>>();
        let delivered = u64::try_from(lines.len()).unwrap_or(0);
        StablePage {
            lines,
            next_after: start + delivered,
            has_more: start + delivered < head,
            gap,
        }
    }

    /// Bounded tail of stable rows for a baseline snapshot.
    pub fn stable_tail(&self, limit: usize) -> StablePage {
        let head = self.head_row_id();
        let limit = u64::try_from(limit).unwrap_or(u64::MAX);
        self.stable_page(
            head.saturating_sub(limit),
            usize::try_from(limit).unwrap_or(0),
        )
    }

    /// Current live grid, trailing blank rows trimmed, cropped to `limit`.
    pub fn live_rows(&self, limit: usize) -> (Vec<String>, bool) {
        let mut rows = self
            .parser
            .screen()
            .rows(0, self.cols)
            .collect::<Vec<String>>();
        while rows.last().is_some_and(|row| row.trim().is_empty()) {
            rows.pop();
        }
        let truncated = rows.len() > limit;
        if truncated {
            rows.drain(..rows.len() - limit);
        }
        (rows, truncated)
    }

    fn feed_pieces(&mut self, data: &[u8]) {
        for piece in data.chunks(FEED_PIECE) {
            self.parser.process(piece);
            self.drain();
            // Rebase inside the loop: one large feed can otherwise push the
            // emulator past TRANSFER_SCROLLBACK and evict rows that were
            // never promoted.
            self.rebase();
        }
    }

    fn bump_epoch(&mut self, reason: EpochReason) {
        self.epoch += 1;
        self.last_reset = Some(reason);
    }

    /// Promote rows that scrolled out of the live grid.
    fn drain(&mut self) {
        // The alternate screen is a live-only overlay: vt100 gives it its own
        // grid with no scrollback, so nothing is ever promoted from it and the
        // main grid's promotion state must survive the round trip untouched.
        if self.parser.screen().alternate_screen() {
            return;
        }
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let history = self.parser.screen().scrollback();
        // A reset drops the emulator's own scrollback. Re-anchor rather than
        // skip every later row up to a now-stale count.
        self.consumed = self.consumed.min(history);
        if history > self.consumed {
            let cols = self.cols;
            for index in self.consumed..history {
                self.parser.screen_mut().set_scrollback(history - index);
                let row = self
                    .parser
                    .screen()
                    .rows(0, cols)
                    .next()
                    .unwrap_or_default();
                self.push_stable(row);
            }
            self.consumed = history;
        }
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Replace the emulator with one whose transfer buffer is empty while
    /// preserving the live grid, cursor, and input modes.
    ///
    /// `state_formatted` serializes only the *visible* grid. It cannot restore
    /// the main/alternate grid pair, and rebasing while the alternate screen is
    /// active would fold alternate contents into the main grid and leak them
    /// into stable history. Deferring is safe and bounded: the alternate grid
    /// has no scrollback, so `consumed` cannot grow while it is active.
    ///
    /// Accepted residual on the main screen, recorded in docs/design.md:
    /// scroll margins (DECSTBM), origin mode, the saved cursor, and pending
    /// wrap are not carried by `state_formatted` and are lost across a rebase. vt100 exposes none of
    /// them, so they cannot be preserved without forking the emulator. Full
    /// screen applications re-issue them on their next repaint, and a lost
    /// margin only affects live-grid rendering, never already-promoted rows.
    fn rebase(&mut self) {
        if self.consumed < REBASE_THRESHOLD || self.parser.screen().alternate_screen() {
            return;
        }
        let state = self.parser.screen().state_formatted();
        let mut parser = vt100::Parser::new(self.rows, self.cols, TRANSFER_SCROLLBACK);
        parser.process(&state);
        self.parser = parser;
        self.consumed = 0;
    }

    fn push_stable(&mut self, row: String) {
        self.stable.push_back(row);
        self.next_row_id += 1;
        while self.stable.len() > STABLE_ROW_RETENTION {
            self.stable.pop_front();
            self.floor_row_id += 1;
        }
    }

    /// Locate every boundary sequence, returning its byte range in `data`. A
    /// match that began in a previous feed reports an empty leading range,
    /// because those bytes were already delivered to the emulator. Matches
    /// split across feeds are recovered from the carry buffer.
    fn scan_boundaries(&mut self, data: &[u8]) -> Vec<(usize, usize, Boundary)> {
        let carry_len = self.carry.len();
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(data);
        let mut found = Vec::new();
        let mut index = 0;
        let mut matched_end = 0;
        while index < window.len() {
            if window[index] != 0x1b {
                index += 1;
                continue;
            }
            let matched = BOUNDARIES
                .iter()
                .find(|(pattern, _)| window[index..].starts_with(pattern));
            if let Some((pattern, boundary)) = matched {
                let begin = index.saturating_sub(carry_len).min(data.len());
                let end = (index + pattern.len())
                    .saturating_sub(carry_len)
                    .min(data.len());
                found.push((begin, end, *boundary));
                index += pattern.len();
                matched_end = index;
            } else {
                index += 1;
            }
        }
        // Bytes already consumed by a match must never be replayed into the
        // next scan, or the same boundary would be reported twice.
        let tail = window
            .len()
            .saturating_sub(BOUNDARY_CARRY)
            .max(matched_end)
            .min(window.len());
        self.carry = window[tail..].to_vec();
        found
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::{EpochReason, ScreenStore};

    fn feed_lines(store: &mut ScreenStore, count: usize) {
        for index in 0..count {
            store.feed(format!("line-{index}\r\n").as_bytes());
        }
    }

    #[test]
    fn rows_leaving_the_live_grid_receive_monotonic_absolute_ids() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 10);

        assert!(store.head_row_id() >= 6);
        let page = store.stable_page(0, 4);
        assert_eq!(page.lines.len(), 4);
        assert_eq!(page.lines[0], "line-0");
        assert_eq!(page.next_after, 4);
        assert!(page.has_more);
        let next = store.stable_page(page.next_after, 4);
        assert_eq!(next.lines[0], "line-4");
        assert!(!next.gap);
    }

    #[test]
    fn replaying_a_cursor_returns_the_same_window() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 10);

        let first = store.stable_page(2, 3);
        let replay = store.stable_page(2, 3);
        assert_eq!(first.lines, replay.lines);
        assert_eq!(first.next_after, replay.next_after);
    }

    #[test]
    fn live_grid_holds_the_newest_rows() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 10);

        let (live, _truncated) = store.live_rows(40);
        assert!(live.iter().any(|row| row == "line-9"));
        assert!(!live.iter().any(|row| row == "line-0"));
    }

    #[test]
    fn terminal_reset_and_erase_scrollback_advance_the_epoch() {
        let mut store = ScreenStore::new(4, 20);
        assert_eq!(store.epoch(), 1);

        store.feed(b"before\r\n\x1bc");
        assert_eq!(store.epoch(), 2);
        assert_eq!(store.last_reset_reason(), Some(EpochReason::TerminalReset));

        store.feed(b"after\r\n\x1b[3J");
        assert_eq!(store.epoch(), 3);
        assert_eq!(
            store.last_reset_reason(),
            Some(EpochReason::EraseScrollback)
        );
    }

    #[test]
    fn reset_sequences_split_across_feeds_still_advance_the_epoch() {
        let mut store = ScreenStore::new(4, 20);
        store.feed(b"x\x1b[");
        assert_eq!(store.epoch(), 1);
        store.feed(b"3J");
        assert_eq!(store.epoch(), 2);
        assert_eq!(
            store.last_reset_reason(),
            Some(EpochReason::EraseScrollback)
        );
    }

    #[test]
    fn only_a_column_change_reflows_history() {
        let mut store = ScreenStore::new(4, 20);
        store.resize(8, 20);
        assert_eq!(store.epoch(), 1);
        store.resize(8, 40);
        assert_eq!(store.epoch(), 2);
        assert_eq!(store.last_reset_reason(), Some(EpochReason::Resize));
        assert_eq!(store.size(), (8, 40));
    }

    #[test]
    fn a_new_process_generation_advances_the_epoch_without_losing_history() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 10);
        let head = store.head_row_id();
        let (last_screen, _) = store.live_rows(40);
        assert!(!last_screen.is_empty());

        store.restart();
        assert_eq!(store.epoch(), 2);
        assert_eq!(store.last_reset_reason(), Some(EpochReason::ProcessRestart));
        assert_eq!(store.stable_page(0, 1).lines[0], "line-0");
        // The dead process's final screen survives in history.
        assert_eq!(
            store.head_row_id(),
            head + u64::try_from(last_screen.len()).unwrap_or(0)
        );
        assert_eq!(store.stable_tail(last_screen.len()).lines, last_screen);
        assert!(store.live_rows(40).0.is_empty());
    }

    #[test]
    fn rebasing_the_emulator_keeps_promoting_rows_without_duplication() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 1_500);

        let head = store.head_row_id();
        assert!(head >= 1_496, "unexpected head row {head}");
        let page = store.stable_page(0, 3);
        assert_eq!(page.lines, ["line-0", "line-1", "line-2"]);
        let tail = store.stable_tail(3);
        assert_eq!(tail.lines.len(), 3);
        assert!(tail.lines.iter().all(|row| row.starts_with("line-")));
    }

    fn promoted(store: &ScreenStore) -> Vec<String> {
        let mut rows = Vec::new();
        let mut after = store.floor_row_id().saturating_sub(1);
        loop {
            let page = store.stable_page(after, 512);
            if page.lines.is_empty() {
                break;
            }
            after = page.next_after;
            rows.extend(page.lines);
        }
        rows
    }

    #[test]
    fn one_feed_larger_than_the_transfer_buffer_promotes_every_row_in_order() {
        let mut store = ScreenStore::new(4, 20);
        let mut burst = String::new();
        for index in 0..3_000 {
            let _ = writeln!(burst, "{}\r", format_args!("b-{index}"));
        }

        store.feed(burst.as_bytes());

        let rows = promoted(&store);
        assert_eq!(store.head_row_id(), 2_997, "burst lost rows");
        let expected = (0..2_997).map(|i| format!("b-{i}")).collect::<Vec<_>>();
        assert_eq!(rows, expected);
        let (live, _) = store.live_rows(40);
        assert_eq!(live, ["b-2997", "b-2998", "b-2999"]);
    }

    #[test]
    fn rows_scrolled_in_the_same_piece_as_a_reset_survive_it() {
        for (reset, reason) in [
            (&b"\x1bc"[..], EpochReason::TerminalReset),
            (&b"\x1b[3J"[..], EpochReason::EraseScrollback),
        ] {
            let mut store = ScreenStore::new(4, 20);
            let mut text = String::new();
            for index in 0..10 {
                let _ = writeln!(text, "{}\r", format_args!("p-{index}"));
            }
            let mut piece = text.into_bytes();
            piece.extend_from_slice(reset);
            assert!(
                piece.len() < super::FEED_PIECE,
                "reset must share one piece"
            );

            store.feed(&piece);

            assert_eq!(store.epoch(), 2);
            assert_eq!(store.last_reset_reason(), Some(reason));
            let rows = promoted(&store);
            assert!(
                rows.starts_with(&["p-0".to_owned(), "p-1".to_owned()]),
                "rows before the reset were wiped: {rows:?}"
            );
            assert_eq!(rows.len(), 7, "unexpected promoted rows: {rows:?}");

            // Rows produced after the reset are still promoted.
            feed_lines(&mut store, 20);
            let after = promoted(&store);
            assert!(
                after.iter().any(|row| row == "line-15"),
                "post-reset rows skipped: {after:?}"
            );
        }
    }

    #[test]
    fn a_reset_split_across_feeds_preserves_rows_on_both_sides() {
        for (head, tail, reason) in [
            (&b"\x1b"[..], &b"c"[..], EpochReason::TerminalReset),
            (&b"\x1b["[..], &b"3J"[..], EpochReason::EraseScrollback),
        ] {
            let mut store = ScreenStore::new(4, 20);
            feed_lines(&mut store, 10);
            let before = promoted(&store);
            assert_eq!(before.len(), 7);

            store.feed(head);
            assert_eq!(store.epoch(), 1);
            store.feed(tail);
            assert_eq!(store.epoch(), 2);
            assert_eq!(store.last_reset_reason(), Some(reason));
            assert_eq!(promoted(&store), before);

            feed_lines(&mut store, 20);
            assert!(promoted(&store).iter().any(|row| row == "line-15"));
        }
    }

    #[test]
    fn entering_the_alternate_screen_promotes_the_main_grid_first() {
        for enter in [&b"\x1b[?1049h"[..], &b"\x1b[?47h"[..]] {
            let mut store = ScreenStore::new(4, 20);
            let mut piece = String::new();
            for index in 0..10 {
                let _ = writeln!(piece, "{}\r", format_args!("m-{index}"));
            }
            let mut piece = piece.into_bytes();
            piece.extend_from_slice(enter);
            assert!(piece.len() < super::FEED_PIECE, "must share one piece");

            store.feed(&piece);

            // The rows must be observable immediately, not only once the
            // application leaves the alternate screen.
            let rows = promoted(&store);
            assert_eq!(rows.len(), 7, "pre-entry rows were hidden: {rows:?}");
            assert_eq!(rows[0], "m-0");
            assert_eq!(store.epoch(), 1, "an alternate switch is not a reset");
        }
    }

    #[test]
    fn a_provider_that_dies_in_the_alternate_screen_keeps_main_history() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, 10);
        store.feed(b"\x1b[?1049h");
        for index in 0..20 {
            store.feed(format!("alt-{index}\r\n").as_bytes());
        }

        store.restart();

        let rows = promoted(&store);
        assert!(rows.iter().any(|row| row == "line-0"));
        assert!(rows.iter().any(|row| row == "line-9"), "{rows:?}");
        assert!(
            !rows.iter().any(|row| row.starts_with("alt-")),
            "alternate contents leaked: {rows:?}"
        );
    }

    #[test]
    fn entering_and_leaving_the_alternate_screen_in_one_piece_keeps_history() {
        let mut store = ScreenStore::new(4, 20);
        let mut piece = String::new();
        for index in 0..10 {
            let _ = writeln!(piece, "{}\r", format_args!("m-{index}"));
        }
        let mut piece = piece.into_bytes();
        piece.extend_from_slice(b"\x1b[?1049hALT\x1b[?1049l");
        let mut tail = String::new();
        for index in 10..14 {
            let _ = writeln!(tail, "{}\r", format_args!("m-{index}"));
        }
        piece.extend_from_slice(tail.as_bytes());

        store.feed(&piece);

        let rows = promoted(&store);
        assert!(
            rows.starts_with(&["m-0".to_owned(), "m-1".to_owned()]),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|row| row.contains("ALT")), "{rows:?}");
        assert!(rows.iter().any(|row| row == "m-9"), "{rows:?}");
        assert_eq!(store.epoch(), 1);
    }

    #[test]
    fn alternate_screen_contents_never_reach_stable_history() {
        let mut store = ScreenStore::new(4, 20);
        // Push past the rebase threshold so the alternate screen is entered
        // while a rebase is pending.
        feed_lines(&mut store, 600);
        let main_rows = promoted(&store);
        let head = store.head_row_id();

        store.feed(b"\x1b[?1049h");
        for index in 0..100 {
            store.feed(format!("alt-{index}\r\n").as_bytes());
        }
        assert_eq!(store.head_row_id(), head, "alternate rows were promoted");
        let (live, _) = store.live_rows(40);
        assert!(live.iter().any(|row| row.starts_with("alt-")));

        store.feed(b"\x1b[?1049l");
        assert_eq!(promoted(&store), main_rows, "history changed across alt");

        feed_lines(&mut store, 20);
        let rows = promoted(&store);
        assert!(
            !rows.iter().any(|row| row.starts_with("alt-")),
            "alternate contents leaked into stable history"
        );
        assert!(rows.starts_with(&main_rows), "history was rewritten");
        assert!(rows.iter().any(|row| row == "line-15"));
    }

    #[test]
    fn a_cursor_older_than_the_retained_floor_reports_a_gap() {
        let mut store = ScreenStore::new(4, 20);
        feed_lines(&mut store, super::STABLE_ROW_RETENTION + 200);

        assert!(store.floor_row_id() > 1);
        let page = store.stable_page(0, 8);
        assert!(page.gap);
        assert_eq!(page.lines.len(), 8);
        assert!(!store.stable_page(store.floor_row_id(), 8).gap);
    }
}
