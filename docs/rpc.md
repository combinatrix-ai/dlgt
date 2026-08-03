# dlgt local RPC v1

This document is the normative programmatic interface. Command syntax and exit
statuses belong in [CLI](cli.md); provider integration and security rationale
belong in [Design](design.md).

## Transport and framing

dlgt uses newline-delimited JSON over a mode-0600 Unix socket. `dlgt rpc
--stdio` proxies only the public methods in this document. Each non-streaming
request produces exactly one response line.

Request:

```json
{"id":"req_1","method":"session.fetch","params":{"session":"codex:019f6307-341e-7e81-8a33-7ab61e804345","until":"result","wait_ms":900000}}
```

Success:

```json
{"id":"req_1","result":{"schema_version":1,"reason":"result","cursor":"f1.eyJ2IjoxLCJi...","sessions":[]}}
```

A successful non-streaming response may also carry an informational notice
without changing the result:

```json
{"id":"req_1","result":[],"info":{"code":"UPDATE_AVAILABLE","current_version":"0.1.4","latest_version":"0.2.0","command":"dlgt update"}}
```

`info` is advisory. Clients should present it separately from the result, and
must obtain user confirmation before acting on `UPDATE_AVAILABLE`.

Failure:

```json
{"id":"req_1","error":{"code":"CURSOR_EXPIRED","message":"cursor belongs to a previous daemon instance"}}
```

Raw RPC responses do not use the CLI's `ok:true` or `ok:false` wrapper. Blank
input lines are ignored. Invalid JSON, framing failures, and a closed transport
terminate the stdio proxy.

## Public methods

```text
session.create        Create a Session with its required initial prompt
session.restart       Replace a Session process and resume provider context
session.send          Accept work on an existing idle Session
session.fetch         Read everything new since a cursor, optionally long-polling
session.cancel        Interrupt active work, bounded by timeout_ms
session.list          List active or all Sessions
session.read          Read live Session state and latest retained result
session.stop          Stop the Harness process group
event.read            Read normalized versioned lifecycle events
event.subscribe       Stream normalized lifecycle events
scrollback.read       Read VT-rendered plain-text rows
transcript.read_raw   Read explicitly requested raw PTY pages
model.list            Discover Harness models
profile.list          List client-side Profile names
harness.list          Read Harness capabilities
```

`session` parameters accept a provider-qualified `codex:<thread-id>` or
`claude:<session-id>` Session ID, or the active human alias. The following
parameter shapes are stable for v1:

| Method | Parameters |
| --- | --- |
| `session.create` | `title`, optional `alias`, `harness`, `cwd`, optional `model`, optional `effort`, optional `harness_options`, optional `auto_approve` (default `true`), required non-empty `prompt`, optional `request_id`, `startup_timeout_ms`, launch `environment`, `rows`, `cols` |
| `session.restart` | `session` ID, `startup_timeout_ms`, fresh launch `environment`, `rows`, `cols` |
| `session.send` | `session`, `prompt`, optional `request_id`; with `resume:true`, the same provider-qualified Session ID and launch options are accepted |
| `session.fetch` | exactly one of `session` or `all:true`; optional `cursor`, `wait_ms` (max 86,400,000), `until` (`any` or `result`), `screen` (boolean or stable-line count), `max_bytes` |
| `session.cancel` | `session`, optional `timeout_ms` with a 30-second default |
| `session.list` | optional `all` boolean |
| `session.read` | `session` |
| `session.stop` | `session`, optional `force` boolean |
| `event.read` | optional `session`, optional global `after` sequence |
| `event.subscribe` | optional `session`, optional global `after` sequence |
| `scrollback.read` | `session`, optional `lines`, optional opaque `before` cursor |
| `transcript.read_raw` | `session`, optional byte offset `after`, optional `limit_bytes` |
| `model.list` | `harness`, optional `include_hidden` |
| `profile.list` | no required parameters |
| `harness.list` | optional `harness` |

Profiles are expanded by the client. `profile.list` is implemented by the
stdio proxy rather than delegated to the daemon, so the daemon does not reread
mutable client configuration.

`request_id` is an optional caller-chosen idempotency key. The daemon retains
the last 1,024 acceptance receipts for its lifetime. Repeating an ID with the
same payload returns the original receipt with `replayed: true`; repeating it
with a different payload fails with `INVALID_ARGUMENT`.

Payload identity is the canonical form of the RPC parameters:

- every parameter except `environment`, `rows`, `cols`, `correlation_id`, and
  `request_id`, which legitimately differ between retries of the same
  acceptance;
- object keys in sorted order, so a client that emits parameters in a
  different order still matches;
- array values in the order given, so `harness_options` must be repeated in
  the same order;
- `prompt` compared byte for byte. A trailing newline is significant, so
  `--stdin` from a heredoc and the same text passed after `--` are different
  payloads.

An acceptance reserves its `request_id` before it runs. A second call arriving
while the first is still launching blocks until the winner settles and then
replays its receipt, so two concurrent calls can never create two Sessions.
A different payload is rejected against an in-flight reservation as well as a
stored receipt. A failed acceptance stores no receipt, so the same ID may be
retried.

`harness_options` is an array of explicit `KEY=VALUE` Claude Code CLI options.
The daemon converts each entry to `--KEY=VALUE`, rejects dlgt-managed arguments,
and retains the array so `session.restart` reuses the same launch behavior.
When the array carries no `permission-mode` entry and `auto_approve` is true,
dlgt adds `--permission-mode=auto`; `auto_approve: false` keeps Claude Code's
own permission default. Codex Harness options are not currently supported.

## Session and result schemas

A public Session contains its provider-qualified ID, active alias and title,
Harness, working directory, model selection, state, and timing. The suffix of
`session.id` is the Codex thread ID or Claude session ID, so the same value
correlates provider-native logs, addresses a live Session, and explicitly
resumes it after daemon exit. Provider turn IDs, internal launch IDs, and
internal execution row IDs are excluded.

When `state` is exactly `busy`, the Session snapshot also includes two integer
diagnostics:

```json
{
  "state": "busy",
  "busy_for_ms": 183000,
  "pty_quiet_for_ms": 72000
}
```

`busy_for_ms` is elapsed time since the current turn was reserved (its local
creation timestamp), so provider startup or binding cannot make it move
backwards. `pty_quiet_for_ms` is elapsed time since the latest PTY output,
clamped to the current busy interval; if no output has been observed it equals
`busy_for_ms`. These values are diagnostic only and never transition a Session
or permit another send. Both fields are omitted for every non-`busy` state.

CLI `send` scans live versioned daemon sockets before dispatch. Raw JSONL RPC
is intentionally scoped to the selected socket; callers using RPC directly
must select the owning versioned socket themselves.

The public state set is:

```text
starting  idle  busy  blocked  canceling  stopping  restarting  stopped  failed
```

Every accepted execution receives a per-Session monotonic `execution_seq`.
This is correlation data, never an RPC selector or public resource ID.
The returned Session state is a snapshot taken when the response is built; use
lifecycle events or a later `session.read` for current state.

A retained result has this shape:

```json
{
  "execution_seq": 2,
  "status": "completed",
  "final_text": "Review result...",
  "final_text_source": "hook",
  "error": null,
  "started_at_ms": 1784024104395,
  "completed_at_ms": 1784024252019,
  "usage": null
}
```

`status` is `completed`, `failed`, `canceled`, or `interrupted`.
`final_text` is always a string for a completed execution, although it may be
empty. Other terminal states may include partial text and provide a structured
error. Usage is nullable because provider support differs.

`final_text_source` is `hook` when the Harness lifecycle event reported the
text, `transcript` when the Harness reported nothing and dlgt recovered the
text from that Session's own provider transcript within this execution's
boundary, and `missing` when no text was recovered. A failed recovery never
changes the execution status.

`session.create` and `session.send` return `{session, execution_seq, cursor}`.
The `cursor` is captured under the runtime lock immediately before the
acceptance is recorded, so the first `session.fetch` from it cannot miss output
produced between acceptance and the caller's next request.

## Composite reads

`session.fetch` returns one document per request. There is no streaming
variant and no partial output.

```json
{
  "schema_version": 1,
  "runtime": {"version":"0.4.0","instance_id":"1f2c…"},
  "reason": "result",
  "has_more": false,
  "gaps": [],
  "cursor": "f1.eyJ2IjoxLCJi...",
  "sessions": [
    {
      "session": {"id":"claude:8bc7859c","state":"idle"},
      "events": [
        {"schema_version":1,"seq":104,"type":"session.idle","session_id":"claude:8bc7859c","execution_seq":7,"result_status":"completed"}
      ],
      "results": [
        {"execution_seq":7,"status":"completed","final_text":"Review result...","final_text_source":"hook","final_text_offset":0,"final_text_complete":true,"error":null,"started_at_ms":1784024104395,"completed_at_ms":1784024252019,"usage":null}
      ],
      "screen": {"epoch":3,"reset":false,"reset_reason":null,"stable":["Checking tests..."],"live":["Writing final review..."],"live_truncated":false},
      "gaps": []
    }
  ]
}
```

`reason` is `snapshot`, `change`, `result`, `blocked`, `page_full`, `gap`, or
`timeout`. Every one of them is a successful response; `results[].status`
remains the authority on whether an execution succeeded.

Rules:

- Without `cursor`, the response is a bounded baseline: current state, the
  latest retained result, a stable tail, the live screen, and a fresh cursor.
- `until:"result"` binds to the execution active, or latest, at the first
  evaluation. A later execution never extends the bind, and blocked input, a
  page-full response, a gap, or the deadline returns early.
- Replaying a cursor replays the same immutable events, results, and stable
  rows. The Session snapshot and live screen are current snapshots and may be
  newer on replay. Nothing is advanced or consumed server-side.
- A live-screen repaint alone never completes a wait.
- `all:true` covers only the addressed daemon, pages at 32 Sessions, and
  rejects both screen aggregation and `until:"result"`. A cursorless call
  enumerates every Session, carries its enumeration position in the cursor,
  and stays in baseline mode with `has_more: true` until it finishes; later
  calls return only changed Sessions.

Bounds are part of the contract: 32 KiB serialized by default and 256 KiB hard,
64 events, 4 results, 128 stable lines (512 on request), and 40 live rows per
response. `has_more: true` means data already exists beyond the returned
cursor, and the next request returns immediately even with `wait_ms`.

`max_bytes` is a hard bound on the complete compact response line a client
prints, wrapper and newline included; the daemon reserves that wrapper before
committing content. An optional `info` notice and pretty-printing are outside
the bound. The cursor is derived from what the document actually carries, so it
can never advance past omitted content. Progress under a tight budget comes from chunking: a long
`final_text` is chunked at a UTF-8 boundary and continued with
`final_text_offset` and `final_text_complete`, and a screen row wider than the
remaining budget is chunked mid-line. Every `has_more` response advances at
least one watermark. A `max_bytes` too small for the envelope plus one chunk
fails with `INVALID_ARGUMENT` naming the smallest workable value instead of
emitting an oversized document. That value is measured by rendering the
minimal response, so retrying at exactly it succeeds.

`screen.stable` contains complete rows only. A split row is carried in one of
two ordered slots, each shaped
`{"row_id":8412,"offset":4096,"text":"...","complete":false}`:

- `screen.fragment_before` continues the row the previous response split. It
  precedes `stable[0]` and is the only slot where `complete` can be true.
- `screen.fragment_after` is the row this response split. It follows the last
  entry of `stable` and is never complete.

The screen delta reads in that order: `fragment_before`, `stable`,
`fragment_after`. Clients must retain every piece and concatenate the pieces
for a `row_id` in `offset` order until `complete` is true; a completing piece
carries only its own tail, so discarding earlier pieces loses data. A row never
appears in both `stable` and a fragment slot.

Lifecycle events are committed as a strictly ascending prefix across every
Session in the response, so a Session that did not fit the page can never park
an earlier event behind a later one that keeps redelivering.

Cursors are opaque, prefixed `f1.`, and carry the codec version, the daemon
boot identity, the addressed scope, and per-Session watermarks. They bind to an
internal Session identity, so a Claude provider-ID rotation keeps them valid.
An `all` cursor keeps entries only for Sessions that still exist and carry
state, and addresses at most 256 of them; a daemon holding more rejects `all`
with `INVALID_ARGUMENT` and must be read one Session at a time.

Retention is bounded to 10,000 stable rows per Session, 50,000 lifecycle events
per daemon, and 128 results or 16 MiB of result bodies per Session. A cursor
that predates an eviction returns `reason:"gap"`, a `gaps` entry of
`{"component":"screen"|"events"|"results","reason":"retention_overrun"}`, a
bounded baseline, and a fresh cursor. Per-Session components (`screen`,
`results`) are reported on the Session bucket and scope-wide components
(`events`) in the top-level `gaps` array. dlgt never silently resets a cursor.

Lifecycle events are materialized when they are recorded and are scoped by an
internal Session identity, so replaying a cursor returns byte-identical events
even after the execution they describe has been evicted or the public Session
ID has rotated. `session_id` on an event is the ID that was published when the
event happened.

## Lifecycle events

`event.read` returns a JSON array. `event.subscribe` returns an initial response
and then one normalized NDJSON event per line until interrupted or the
connection closes.

```jsonl
{"schema_version":1,"seq":101,"type":"session.created","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":1,"seq":102,"type":"session.ready","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":1,"seq":103,"type":"session.busy","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1}
{"schema_version":1,"seq":104,"type":"provider.retrying","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"attempt":1}
{"schema_version":1,"seq":105,"type":"session.blocked","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"reason":"user_input"}
{"schema_version":1,"seq":106,"type":"session.resumed","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1}
{"schema_version":1,"seq":107,"type":"session.idle","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"result_status":"completed"}
{"schema_version":1,"seq":108,"type":"session.stopped","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":1,"seq":109,"type":"session.restarting","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":1,"seq":110,"type":"session.ready","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
```

The complete v1 event type set is:

```text
session.created
session.restarting
session.ready
session.busy
session.blocked
session.resumed
session.canceling
session.idle
session.stopping
session.stopped
session.failed
provider.retrying
```

Every event contains `schema_version`, a global monotonic `seq`, `type`, and
when applicable `session_id` and `execution_seq`. Type-specific fields include
`attempt`, `reason`, and `result_status`.

The stream contains lifecycle and actionable state, not token or terminal text
deltas. `event.subscribe` is the extension point for notification adapters;
`session.fetch` is the read path for agents, and raw output is observed through
`scrollback.read`, `transcript.read_raw`, or interactive attach.

## Output readers

`scrollback.read` returns the VT-rendered screen and history:

```json
{
  "session_id": "codex:019f6307-341e-7e81-8a33-7ab61e804345",
  "screen": {"rows":24,"cols":120},
  "lines": ["Review complete.","","Main concerns:"],
  "truncated": true,
  "before": "scr_84A2"
}
```

The default is the latest 100 rows. Reads are clamped to 1 through 10,000
rows, and `before` is an opaque cursor for older pages. Rows come from the
persistent per-Session stable-row store that also backs `session.fetch`, so a
read no longer re-renders the retained raw ring. `before` cursors are opaque
and are not comparable across daemon instances.

`transcript.read_raw` is an explicit diagnostic method. It returns a bounded
base64 page and byte cursor:

```json
{
  "session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345",
  "data_base64":"...",
  "byte_len":4096,
  "next_after":8192,
  "has_more":true
}
```

The default raw page limit is 1 MiB and the server caps it at 8 MiB. Callers
must follow `next_after` while `has_more` is true.

## Error contract

RPC failures contain a stable `code` and human-readable `message`. The v1 code
families are:

```text
INVALID_ARGUMENT       Request cannot be retried unchanged
NOT_FOUND              Session or configuration object does not exist
NO_RESULT              Session has never accepted work
ALIAS_IN_USE           Exact alias belongs to a non-terminal Session
SESSION_BUSY           Active execution; retry after it terminalizes
SESSION_BLOCKED        Human input is required
SESSION_ATTACHED       Exclusive attach lease prevents semantic send
SESSION_UNAVAILABLE    Session state cannot accept the operation
ALREADY_ATTACHED       Another client owns the attach lease
CURSOR_VERSION_UNSUPPORTED  Cursor codec is not understood
CURSOR_EXPIRED         Cursor belongs to a previous daemon instance
CURSOR_SCOPE_MISMATCH  Cursor addresses a different Session or scope
CURSOR_INVALID         Cursor payload is not decodable
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt runtime invariant failure
```

Methods may add contextual error fields but must not overload a code with a
different retry or human-action policy. A `session.create` failure before
provider binding includes an `internal:<short-id>` as `launch_id`, never as
`session_id`. A failure after binding includes the canonical `session_id`.
CLI exit-status mapping is defined in [CLI](cli.md#exit-statuses).

## Security boundary

`rpc --stdio` uses a fixed allowlist. Internal methods such as provider hooks,
terminal input, resize, and private execution operations are unavailable even
if a caller names them directly.

Provider turn IDs, internal Session UIDs, and internal execution row IDs never
appear in public responses or normalized events; cursors carry the internal
Session identity only in opaque form. Launch environment values travel in RPC memory
but are not directly serialized into Session metadata, errors, Profiles, or
events. Results and terminal output remain untrusted and potentially sensitive
because a provider can echo its environment.

Raw transcript pages are deliberately separate from rendered scrollback. They
may contain control bytes, redraw noise, and provider-emitted secrets; clients
should request and retain them only for explicit diagnosis.
