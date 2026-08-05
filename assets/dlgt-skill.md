---
name: dlgt
description: Delegate work to the competing harness - run Claude from Codex, or Codex from Claude, as a live session that accepts follow-ups. Use when the other model should review, implement, or give a second opinion, or when a subagent must stay alive and addressable across commands.
---

# dlgt

`dlgt` runs a Codex or Claude subagent in a dlgt-owned PTY that stays alive
between commands. Reach for it whenever the work should cross the provider
boundary.

On the harness you are already running, its own built-in subagent call is the
cheaper default. Use dlgt there too when the delegation needs something that
call cannot express:

- a model or `--effort` pinned to that one worker
- a Session that stays alive and keeps its context across later follow-ups
- `attach` or `fetch` to watch the worker's screen or answer a prompt on it
- a distinct `--cwd`, launch environment, or approval posture

Outside those, prefer the built-in subagent: dlgt costs a daemon, a process,
and a PTY.

Terms used below:

```text
Session   One Harness process and PTY, one controller, at most one active
          execution, no queue. The only public runtime object.
Harness   The provider adapter, codex or claude.
Title     A human description. Alias is a short human address derived from it.
Profile   A reusable client-side launch specification.
```

## What is required, what is optional

| Command | Required | Optional, with its default |
| --- | --- | --- |
| `new` | `--title`, `--request-id`, a Harness (`--harness` or a Profile), and the prompt (`--stdin` or `-- <PROMPT>`) | `--model` (provider default), `--effort` (harness default), `--cwd` (current directory), `--alias` (generated from the title), `--no-auto-approve` (default is auto-approved), `--startup-timeout` (60s), env options |
| `send` | the Session address, `--request-id`, and the prompt | `--resume`; launch options are accepted only with it |
| `fetch` | the Session address, or `--all` | `--cursor` (omit = bounded baseline snapshot), `--wait <DURATION>` (omit = return immediately; explicit duration up to 24h), `--until any\|result` (default `any`), `--screen[=N]`/`--no-screen` (screen on for one Session; off and rejected for `--all`), `--max-bytes` (32 KiB) |
| `stop`, `cancel`, `show`, `restart` | the Session address | `cancel --timeout` (30s), `stop --force` |

What you must retain between commands is exactly two values: the
provider-qualified `session.id` and the latest `cursor` number. The
`--request-id` for each acceptance must be invented BEFORE the first attempt
and reused verbatim on any retry — that is the entire mechanism that makes a
lost response recoverable.

## Delegate and read the answer

Delegation is two phases. `new` and `send` only accept work and return as soon
as the prompt is accepted; `fetch` is the only command that reads state,
results, events, and screen. Run them as two commands and parse the acceptance
before you wait on anything.

### Phase 1: accept the work

```bash
receipt=$(dlgt new \
  --title "counterpart review" \
  --harness claude \
  --cwd . \
  --request-id review-1 \
  -- "Review the uncommitted changes in this repository. Do not edit files and do not delegate again. Report findings and trade-offs only.")

session_id=$(printf '%s\n' "$receipt" | jq -er '.session.id')
cursor=$(printf '%s\n' "$receipt" | jq -er '.cursor')
```

Retain both before doing anything else:

- `session.id` is provider-qualified (`codex:<thread-id>` or
  `claude:<session-id>`) and is the single address for live commands, provider
  correlation, and later resume.
- `cursor` is a small number: this Session's observation position, taken
  immediately before the work you just accepted, so reading from it cannot
  miss output the provider produced in between. Responses that advance mint
  the next one, so they count up: 1, 2, 3. Carry the latest. A position is
  only meaningful while that daemon lives: after `RPC_UNAVAILABLE`,
  `SESSION_NOT_RUNNING`, or a restart, forget the number and re-enter with
  `send --resume` or one cursorless `fetch`.
- `--request-id` is required on every `new` and `send`, and makes the
  acceptance replayable. If you never see this response, re-run the identical
  command with the same ID: it returns the original receipt with
  `"replayed": true` instead of starting a second worker. It is required
  because a key invented after a response goes missing is already too late to
  deduplicate anything. Choose it before you run the command, and reuse it
  verbatim on any retry. An `--alias` is a human convenience and does not
  deduplicate.

### Phase 2: read the answer

```bash
answered=$(dlgt fetch "$session_id" --cursor "$cursor" --until result --wait 60s)
printf '%s\n' "$answered" | jq -er '.sessions[0].results[0].status'
printf '%s\n' "$answered" | jq -er '.sessions[0].results[0].final_text'
cursor=$(printf '%s\n' "$answered" | jq -er '.cursor')
```

The counterpart's answer is `sessions[0].results[0].final_text`. Check
`status` first: it is `completed`, `failed`, `canceled`, or `interrupted`, and
only `completed` means the text is a finished answer. Parse with a real JSON
tool, never a regex.

Every `fetch` exits 0 and says why it returned in `reason`: `result`,
`change`, `snapshot`, `blocked`, `page_full`, `gap`, or `timeout`. `timeout`
with empty `results` means the work is still running, never that it finished.
`page_full` means more is already waiting: call again immediately with the
returned cursor. A non-zero exit means the request itself was wrong.

Keep the returned `cursor` and pass it to the next `fetch --cursor`. That
makes each read a forward delta instead of a re-download of the same tail. It
is a plain number, short enough to keep in your own notes across turns.

Follow up on the same Session, then release it:

```bash
receipt=$(dlgt send "$session_id" --request-id review-2 \
  -- "Rank those findings by severity and name the one to fix first.")
cursor=$(printf '%s\n' "$receipt" | jq -er '.cursor')
dlgt fetch "$session_id" --cursor "$cursor" --until result --wait 60s \
  | jq -er '.sessions[0].results[0].final_text'

dlgt stop "$session_id"
```

Stopping a Session ends its process, not the provider conversation: the same
`session.id` still resumes. An Alias is a convenience only and may be reused by
a new Session after this one stops.

### Optimization: one call for a short task

When the task is short and the extra round trip matters, the two phases can
share one shell call. This is an optimization, not the default:

```bash
dlgt new --title "quick check" --harness claude --cwd . \
  --alias @quick --request-id quick-1 -- "..." \
  && dlgt fetch @quick --until result --wait 60s
```

The output is **two JSON documents, one per line**. Parse line 1 first:

```bash
out=$(dlgt new ... --alias @quick --request-id quick-1 -- "..." \
  && dlgt fetch @quick --until result --wait 60s)
receipt=$(printf '%s\n' "$out" | sed -n 1p)
answered=$(printf '%s\n' "$out" | sed -n 2p)
session_id=$(printf '%s\n' "$receipt" | jq -er '.session.id')
```

Line 1 is the acceptance receipt and is authoritative. If the second document
is missing, truncated, or the whole command was killed, the receipt still holds
the Session ID and cursor you need, and the work is still running. A missing
second line never means the acceptance failed, and it is never a reason to run
`new` again.

## Standard workflow when running as Claude Code

```bash
# 1. Accept in the foreground; it returns in seconds.
receipt=$(dlgt new --title "long refactor" --harness codex --cwd . \
  --request-id refactor-1 --stdin <<'PROMPT'
...
PROMPT
)
session_id=$(printf '%s\n' "$receipt" | jq -er '.session.id')
cursor=$(printf '%s\n' "$receipt" | jq -er '.cursor')

# 2. Observe with ONE long fetch, run through the Bash tool's
#    background mechanism (run_in_background), not in the foreground:
dlgt fetch "$session_id" --cursor "$cursor" --until result --wait 30m
```

The background task exits when the worker finishes, and the harness notifies
you — zero polling calls. The acceptance receipt is already safe in your
context, so nothing is lost even if the long read is killed. For a short task
(a minute or less), a foreground `--wait 60s` fits inside the default tool
budget and needs no background step. Never fuse acceptance into the long
fetch: that puts the receipt and a 30-minute wait in the same killable
command, which is exactly the failure the two-phase split removes.

## Standard workflow when running as Codex

```bash
# 1. Accept — completes within the exec yield window; the receipt
#    arrives inline in this same tool call.
dlgt new --title "long refactor" --harness claude --cwd . \
  --request-id refactor-1 --stdin < prompt.md

# 2. Observe with ONE long fetch. Expect the exec cell to yield after
#    ~10-30 seconds; that is normal, not a failure.
dlgt fetch "$session_id" --cursor "$cursor" --until result --wait 30m

# 3. Issue a single LONG cell wait on that yielded cell rather than
#    repeated short waits. Output printed before the yield is retained
#    and delivered when the wait resolves.
```

Two Codex-specific traps:

- Wait on the cell ID from the "Script running with cell ID N" line. A
  `chunk_id` in a JSON-shaped yield is not a cell ID and the wait will fail
  with "cell not found".
- Some Codex environments kill a backgrounded cell after tens of seconds: the
  cell reports "completed" with EMPTY output even though `fetch --wait 30m`
  could not have exited silently. That empty completion means your harness
  killed the process, not that the work finished. Do not run a cursorless
  baseline to re-orient — your cursor is still valid, because an observation
  that returned nothing advanced nothing. Re-issue the SAME
  `fetch --cursor <same N> --until result` in the foreground with a `--wait`
  short enough to survive, around 45s, and repeat. Positions replay safely,
  so the same cursor can be retried any number of times.

One long poll replaces a loop of short polls on either harness: use
`--wait 30m` once rather than fifteen `--wait 2m` calls. Where a long wait is
impossible, poll forward instead of re-reading:

```bash
dlgt fetch "$session_id" --cursor "$cursor" --wait 60s
```

`--until any` (the default) wakes on any new event, result, or completed
screen line. A spinner repainting the live screen never wakes it on its own.

## Recover a lost response

If a tool result was killed, truncated, or never arrived, do not re-issue the
work. Recover instead:

| Lost | Recovery |
| --- | --- |
| The `new` or `send` response | Re-run the identical command with the same `--request-id`. It replays the original receipt with `"replayed": true` and never creates a second Session or execution. |
| A `fetch` response or its cursor | Run `dlgt fetch <session.id\|@alias>` with no `--cursor`. That returns a bounded baseline: current state, the latest retained result, a screen tail, and a fresh position. The same applies to `CURSOR_EXPIRED`, which means the daemon no longer holds that position. |
| The Session ID itself | `dlgt list` finds it, but prefer a stable `--alias` on `new` so you never need to search. |

Never re-issue a bare `dlgt new` because you are unsure whether the first one
landed. That is how duplicate workers get created. Every `new` and `send`
carries a `--request-id` for exactly this reason: retry the identical command
with the identical key instead.

## Clean up provider history

A dlgt Session is also a real provider conversation and may appear in the
Codex or Claude history with a title beginning `[dlgt]`. `dlgt stop` releases
the live Harness process and PTY but deliberately preserves that conversation
for later `send --resume`.

Treat provider-history cleanup as a separate, explicit user action. Do not
archive provider history automatically when ordinary delegated work finishes.

- For a Codex Session, remove the `codex:` qualifier from `session.id` and run
  `codex archive <thread-id>`. Use `codex unarchive <thread-id>` to restore it.
- For a Claude Session, Claude Code has no equivalent public archive command.
  Use the runtime's available macOS UI-control tool to operate Claude Desktop:
  search for the full `[dlgt] <title>`, identify the matching conversation,
  choose **Archive** from its session menu, then search again and verify that
  the result is marked archived.
- A title is not a unique identifier. If search returns multiple plausible
  Claude conversations, open candidates and compare the project or cwd,
  initial prompt, and recency. If the target still cannot be identified
  unambiguously, do not guess; report that cleanup needs user selection.
- If the Claude result is already marked archived, make no change. If Claude
  Desktop or suitable UI control is unavailable, report the limitation rather
  than editing provider storage directly.
- Never rewrite Claude transcript JSONL fields such as `entrypoint`, and never
  edit Claude Desktop's private metadata files to imitate its Archive action.

If the user asks only to finish, stop, or clean up the live worker, run
`dlgt stop`. Archive the provider conversation only when the user explicitly
asks to clean up its retained history or the visible `[dlgt]` conversation.

## Pass long prompts on stdin

A self-contained delegation prompt is usually multi-line, so prefer `--stdin`
over a shell-quoted argument. It reads standard input as the exact prompt and
is mutually exclusive with a prompt after `--`.

```bash
receipt=$(dlgt new --title "counterpart review" --harness claude --cwd . \
  --request-id review-1 --stdin <<'PROMPT'
Review the uncommitted changes in this repository.
Do not edit files and do not delegate again.
Report findings and trade-offs only.
PROMPT
)
dlgt fetch "$(printf '%s\n' "$receipt" | jq -er '.session.id')" \
  --cursor "$(printf '%s\n' "$receipt" | jq -er '.cursor')" --until result --wait 60s
```

Quote the heredoc delimiter as `<<'PROMPT'`. Left unquoted, the shell expands
`$` and backticks inside the prompt before dlgt ever sees it. `--stdin` also
keeps the prompt out of argv, so use it for anything sensitive.

## Choose the harness and model

- To cross the provider boundary, select the other harness: from Codex use
  `--harness claude`, from Claude use `--harness codex`. When you are using
  dlgt for the control it gives rather than to change providers, stay on the
  harness you are already running.
- Pass exactly the model the user named. Never silently substitute another
  model; if the named one is unavailable, say so instead.
- If the user named no model, omit `--model` and let the provider default
  apply. Discover valid values with `dlgt models --harness codex|claude`.
- Pass `--effort` only when the user explicitly requested an effort level.
  Otherwise omit it so the harness default applies.
- If the user, the repository, or a standing instruction file defines an
  explicit routing contract for which counterpart handles which work, follow
  that contract instead of this default.
- Give the worker a self-contained prompt: project path, goal, deliverables,
  checks, edit and commit policy, and the required final response. Tell it not
  to delegate again.
- Run one counterpart unless the user explicitly asks for more. The leader
  inspects the returned `final_text` and the actual filesystem diff, and
  remains responsible for final verification.

## Workers start auto-approved

By default dlgt marks the Session cwd trusted in the Harness's local state and
launches the worker with its approval prompts bypassed. The worker can edit
files and run commands under that cwd without asking. Choose `--cwd`
deliberately, constrain the worker in the prompt, and pass `--no-auto-approve`
when a delegation must keep the Harness's own permission prompts.

## Choose the lifecycle operation

| Situation | Operation |
| --- | --- |
| Start work with no existing provider conversation | `new` with its required first prompt |
| Follow up on a live, idle Session | `send <session.id\|@alias>` |
| Replace the provider process but keep history and conversation | `restart <session.id>` |
| Continue after the owning daemon or Session is gone | `send <session.id> --resume --request-id <ID> -- "<prompt>"` |

- `new` requires its first prompt and a `--request-id`, and atomically starts
  the Harness and accepts that prompt. It returns as soon as the prompt is accepted; it never waits for
  the answer.
- Plain `send` never launches, restarts, resumes, or queues. It submits work
  only to a live, idle Session; rejection is side-effect-free, and accepted
  work moves the Session to busy.
- `restart` interrupts active work and preserves the alias, retained history,
  and provider conversation.
- `send --resume` relaunches the saved provider conversation and atomically
  submits the follow-up. An already-live matching Session is reused rather than
  duplicated. Claude may return a new canonical `session.id`; retain it.
- Keep one active execution per Session. `SESSION_BUSY` means retry after the
  current execution reaches a terminal state. dlgt never queues prompts.

## Rules

- `--cwd` accepts absolute and relative paths. The client resolves a relative
  value against its own working directory before sending and fails fast when
  the path does not exist. Omitting `--cwd` uses the client's current
  directory.
- `fetch --wait` needs an explicit duration and accepts up to 24h. A timeout
  never cancels the work; it only ends that observation.
- Use provider lifecycle state and the retained result, not PTY silence, as
  completion proof. `reason: "timeout"` is not completion.
- Never pass `--pretty`. It only costs tokens; the compact document is the
  contract.
- Never run `dlgt server stop` as cleanup. Session state, results, events, and
  screen history live in that daemon's memory and are gone the moment it
  exits, which breaks every cursor and every live Session on the machine.
  `dlgt stop <session.id>` releases one worker; the daemon exits on its own
  once it has been idle and empty.
- Use `fetch` for observation. `scrollback` is the human debugging view, and
  raw PTY bytes require the explicit diagnostic command `logs --raw`.
- If a Session stays `starting` or `busy` without the expected lifecycle event,
  read `fetch <session.id>` and look at `screen.live`, then `attach` when the
  screen shows a first-run, authentication, trust, theme, or permission-mode
  prompt. Complete the prompt, detach, and retry the delegated work.
- `attach` is exclusive. Detach with `Ctrl-b d`; use `--steal` only when taking
  control from a known stale attach client.
- Treat results, rendered scrollback, and raw output as potentially sensitive.
- If a successful response contains `info.code: UPDATE_AVAILABLE`, tell the
  user the current and latest versions and ask whether to run `dlgt update`.
  Do not replace the binary and its embedded Skills without explicit
  confirmation. If the user already requested the update, do not ask again.

## Recover from structured errors

| Error | Required action |
| --- | --- |
| `SESSION_BUSY` | Do not resend. Run `fetch <session.id> --until result --wait <duration>`, or explicitly `cancel` the active work. |
| `SESSION_BLOCKED` | `fetch` reports this as `reason: "blocked"` with the live screen. Read the question, `attach`, answer it, detach with `Ctrl-b d`, then `fetch` again. |
| `SESSION_NOT_RUNNING` | Use the saved `session.id` with `send <session.id> --resume --request-id <ID> -- <prompt>`. |
| `SESSION_ATTACHED` / `ALREADY_ATTACHED` | Coordinate with the active controller. Use `--steal` only for a known stale attach client. |
| `ATTACH_REQUIRES_TTY` | You have no terminal. Use `fetch <session.id>` and read `screen.live` instead. |
| `CURSOR_EXPIRED` / `CURSOR_INVALID` | Drop the cursor and run one cursorless `fetch <session.id>` to rebase. |
| `CANCEL_TIMEOUT` | Cancellation continues. Keep fetching until a terminal result appears. |
| `ALIAS_IN_USE` | The exact alias belongs to a non-terminal Session. Choose another alias or address the existing Session by ID. |
| `LAUNCH_FAILED` | Read `fetch` and `show`, and only then the sensitive `logs --raw`. If present, retain `error.launch_id` for startup diagnostics only. A failed execution is not an error: it is reported as `results[].status`. |

## Observation and control

```bash
dlgt fetch "$session_id" --until result --wait 30m
dlgt fetch "$session_id" --cursor "$cursor"
dlgt fetch "$session_id" --no-screen
dlgt fetch --all
dlgt cancel "$session_id"
dlgt restart "$session_id"
dlgt attach "$session_id"
dlgt show "$session_id"
dlgt list --all-versions
```

`fetch --all` reports every Session of one daemon in a single call, with
lifecycle events and results but no screens. Use it to check several workers at
once.

Control-plane commands return compact JSON with `ok:true` or a structured
`ok:false` error. `events --follow` is NDJSON; `attach` and `logs --raw` are raw
streams. `rpc --stdio` exposes only the public Session-based v1 methods.

A `fetch` response is bounded to about 32 KiB. `has_more: true` means more is
already waiting: call again with the returned cursor and it returns
immediately.

## Command reference

`dlgt help <command>` is authoritative for the installed binary. Use it
whenever a flag or its syntax is unclear, and use discovery rather than
guessing:

```bash
dlgt harnesses
dlgt models --harness codex
dlgt profiles list
```

Fetch the full hosted reference only when `dlgt help` does not answer the
question:

```bash
curl -fsSL https://combinatrix.ai/dlgt/cli.md
```

The hosted reference may describe a newer release. When it conflicts with the
installed binary, follow that binary's help and tell the user an update may be
available.
