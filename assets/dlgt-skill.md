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
- `attach` or `scrollback` to watch the worker's screen or answer a prompt on it
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

## Delegate and read the answer

```bash
created=$(dlgt new \
  --title "counterpart review" \
  --harness claude \
  --cwd . \
  --wait --timeout 15m \
  -- "Review the uncommitted changes in this repository. Do not edit files and do not delegate again. Report findings and trade-offs only.")

printf '%s\n' "$created" | jq -er '.result.status'
printf '%s\n' "$created" | jq -er '.result.final_text'
```

The counterpart's answer is `result.final_text`. Check `result.status` first:
it is `completed`, `failed`, `canceled`, or `interrupted`, and only `completed`
means the text is a finished answer. A failed or timed-out command also exits
non-zero. Parse with a real JSON tool, never a regex.

Follow up on the same Session, then release it:

```bash
session_id=$(printf '%s\n' "$created" | jq -er '.session.id')

dlgt send "$session_id" --wait --timeout 15m \
  -- "Rank those findings by severity and name the one to fix first." \
  | jq -er '.result.final_text'

dlgt stop "$session_id"
```

Retain `session.id`. It is provider-qualified (`codex:<thread-id>` or
`claude:<session-id>`) and is the single address for live commands, provider
correlation, and later resume. Stopping a Session ends its process, not the
provider conversation: the same `session.id` still resumes. An Alias is a
convenience only and may be reused by a new Session after this one stops.

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
dlgt new --title "counterpart review" --harness claude --cwd . \
  --wait --timeout 15m --stdin <<'PROMPT'
Review the uncommitted changes in this repository.
Do not edit files and do not delegate again.
Report findings and trade-offs only.
PROMPT
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
| Continue after the owning daemon or Session is gone | `send <session.id> --resume -- "<prompt>"` |

- `new` requires its first prompt and atomically starts the Harness and accepts
  that prompt.
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
- Always give `new --wait`, `send --wait`, and `wait` an explicit `--timeout`.
  A timeout does not cancel the work.
- Use provider lifecycle state and `wait`, not PTY silence, as completion proof.
- Use `scrollback` for bounded plain-text observation. Raw PTY bytes require
  the explicit diagnostic command `logs --raw`.
- If a Session stays `starting` or `busy` without the expected lifecycle event,
  inspect `events` and `scrollback`, then `attach` when the screen shows a
  first-run, authentication, trust, theme, or permission-mode prompt. Complete
  the prompt, detach, and retry the delegated work in a fresh Session.
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
| `SESSION_BUSY` | Do not resend. Run `wait <session.id> --timeout <duration>`, or explicitly `cancel` the active work. |
| `SESSION_BLOCKED` | Inspect `events` and `scrollback`, `attach`, answer the visible prompt, detach with `Ctrl-b d`, then `wait` again. |
| `SESSION_NOT_RUNNING` | Use the saved `session.id` with `send <session.id> --resume -- <prompt>`. |
| `SESSION_ATTACHED` / `ALREADY_ATTACHED` | Coordinate with the active controller. Use `--steal` only for a known stale attach client. |
| `WAIT_TIMEOUT` | Work continues. Wait again, inspect it, or cancel explicitly. Do not report completion. |
| `CANCEL_TIMEOUT` | Cancellation continues. Inspect `events` and wait for a terminal result. |
| `ALIAS_IN_USE` | The exact alias belongs to a non-terminal Session. Choose another alias or address the existing Session by ID. |
| `LAUNCH_FAILED` / `PROVIDER_FAILED` | Inspect `events`, `scrollback`, and `show`, and only then the sensitive `logs --raw`. If present, retain `error.launch_id` for startup diagnostics only. |

## Observation and control

```bash
dlgt wait "$session_id" --timeout 15m
dlgt cancel "$session_id"
dlgt restart "$session_id"
dlgt events "$session_id"
dlgt scrollback "$session_id" --lines 100
dlgt attach "$session_id"
dlgt show "$session_id"
dlgt list --all-versions
```

Control-plane commands return compact JSON with `ok:true` or a structured
`ok:false` error. `events --follow` is NDJSON; `attach` and `logs --raw` are raw
streams. `rpc --stdio` exposes only the public Session-based v1 methods.

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
