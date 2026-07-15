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
{"id":"req_1","method":"session.wait","params":{"session":"ses_7K3M9Q2X","timeout_ms":900000}}
```

Success:

```json
{"id":"req_1","result":{"execution_seq":1,"result":{"status":"completed","final_text":"done"}}}
```

Failure:

```json
{"id":"req_1","error":{"code":"WAIT_TIMEOUT","message":"wait timed out; execution continues"}}
```

Raw RPC responses do not use the CLI's `ok:true` or `ok:false` wrapper. Blank
input lines are ignored. Invalid JSON, framing failures, and a closed transport
terminate the stdio proxy.

## Public methods

```text
session.create        Create a Session, optionally with an initial prompt
session.restart       Replace a Session process and resume provider context
session.send          Accept work on an existing idle Session
session.wait          Wait for the bound current or latest execution
session.cancel        Interrupt active work, bounded by timeout_ms
session.list          List active or all Sessions
session.read          Read Session state and latest durable result
session.stop          Stop the Harness process group
event.read            Read normalized versioned lifecycle events
event.subscribe       Stream normalized lifecycle events
scrollback.read       Read VT-rendered plain-text rows
transcript.read_raw   Read explicitly requested raw PTY pages
model.list            Discover Harness models
profile.list          List client-side Profile names
harness.list          Read Harness capabilities
```

`session` parameters accept an immutable `ses_XXXXXXXX` ID or the active human
alias. The following parameter shapes are stable for v1:

| Method | Parameters |
| --- | --- |
| `session.create` | `title`, optional `alias`, `harness`, `cwd`, optional `model`, optional `effort`, optional `prompt`, `startup_timeout_ms`, launch `environment`, `rows`, `cols` |
| `session.restart` | `session` ID, `startup_timeout_ms`, fresh launch `environment`, `rows`, `cols` |
| `session.send` | `session`, `prompt` |
| `session.wait` | `session`, positive `timeout_ms` |
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

## Session and result schemas

A public Session contains its immutable ID, active alias and title, Harness,
working directory, model selection, state, timing, and the nullable
`provider_session_id`. The provider session ID is the Codex thread ID or Claude
session ID and correlates dlgt activity with provider-native logs and resume
tools. Provider turn IDs and internal execution row IDs are excluded.

The public state set is:

```text
starting  idle  busy  blocked  canceling  stopping  restarting  stopped  failed
```

Every accepted execution receives a per-Session monotonic `execution_seq`.
This is correlation data, never an RPC selector or public resource ID.
The returned Session state is a snapshot taken when the response is built; use
lifecycle events or a later `session.read` for current state.

A durable result has this shape:

```json
{
  "execution_seq": 2,
  "status": "completed",
  "final_text": "Review result...",
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

`session.wait` binds to the active execution and sequence at request time. If
the Session is idle, it returns the latest durable result; if no execution has
ever been accepted, it fails with `NO_RESULT`. A timeout does not cancel work.

## Lifecycle events

`event.read` returns a JSON array. `event.subscribe` returns an initial response
and then one normalized NDJSON event per line until interrupted or the
connection closes.

```jsonl
{"schema_version":1,"seq":101,"type":"session.created","session_id":"ses_7K3M9Q2X"}
{"schema_version":1,"seq":102,"type":"session.ready","session_id":"ses_7K3M9Q2X"}
{"schema_version":1,"seq":103,"type":"session.busy","session_id":"ses_7K3M9Q2X","execution_seq":1}
{"schema_version":1,"seq":104,"type":"provider.retrying","session_id":"ses_7K3M9Q2X","execution_seq":1,"attempt":1}
{"schema_version":1,"seq":105,"type":"session.blocked","session_id":"ses_7K3M9Q2X","execution_seq":1,"reason":"user_input"}
{"schema_version":1,"seq":106,"type":"session.resumed","session_id":"ses_7K3M9Q2X","execution_seq":1}
{"schema_version":1,"seq":107,"type":"session.idle","session_id":"ses_7K3M9Q2X","execution_seq":1,"result_status":"completed"}
{"schema_version":1,"seq":108,"type":"session.stopped","session_id":"ses_7K3M9Q2X"}
{"schema_version":1,"seq":109,"type":"session.restarting","session_id":"ses_7K3M9Q2X"}
{"schema_version":1,"seq":110,"type":"session.ready","session_id":"ses_7K3M9Q2X"}
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
generated output is observed through `scrollback.read`, `transcript.read_raw`,
or interactive attach.

## Output readers

`scrollback.read` returns the VT-rendered screen and history:

```json
{
  "session_id": "ses_7K3M9Q2X",
  "screen": {"rows":24,"cols":120},
  "lines": ["Review complete.","","Main concerns:"],
  "truncated": true,
  "before": "scr_84A2"
}
```

The default is the latest 100 rows. Reads are clamped to 1 through 10,000 rows,
and `before` is an opaque cursor for older pages.

`transcript.read_raw` is an explicit diagnostic method. It returns a bounded
base64 page and byte cursor:

```json
{
  "session_id":"ses_7K3M9Q2X",
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
WAIT_TIMEOUT           Wait expired; execution continues
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
PROVIDER_FAILED        Provider terminalized work as failed
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt invariant or persistence failure
```

Methods may add contextual error fields but must not overload a code with a
different retry or human-action policy. `session.create` launch failures include
the created `session_id` and include `provider_session_id` when the Harness
assigned one before failing. CLI exit-status mapping is defined in
[CLI](cli.md#exit-statuses).

## Security boundary

`rpc --stdio` uses a fixed allowlist. Internal methods such as provider hooks,
terminal input, resize, and private execution operations are unavailable even
if a caller names them directly.

Provider turn IDs and internal execution row IDs never appear in public
responses or normalized events. Launch environment values travel in RPC memory
but are not directly serialized into Session metadata, errors, Profiles, or
events. Results and terminal output remain untrusted and potentially sensitive
because a provider can echo its environment.

Raw transcript pages are deliberately separate from rendered scrollback. They
may contain control bytes, redraw noise, and provider-emitted secrets; clients
should request and retain them only for explicit diagnosis.
