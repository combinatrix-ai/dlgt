---
name: dlgt
description: Create, address, observe, and control persistent Codex and Claude Sessions through one local runtime.
---

# dlgt

Use `dlgt` when a Codex or Claude subagent should remain alive in an owned PTY
and be addressable from later commands. The only public runtime object is a
Session. Retain the `ses_XXXXXXXX` returned by `new`; aliases are human
conveniences and may be reused after a Session stops.

## Rules

- Create only with `new`; `send` never creates or selects another Session.
- Keep one active execution per Session. `SESSION_BUSY` means retry after the
  current execution terminalizes; dlgt never queues prompts.
- Always give `wait` an explicit `--timeout`. A timeout does not cancel work.
- Use provider lifecycle state and `wait`, not PTY silence, as completion proof.
- Use `scrollback` for bounded plain-text observation. Raw PTY bytes require the
  explicit diagnostic command `logs --raw`.
- `attach` is exclusive. Detach with `Ctrl-b d`; use `--steal` only when taking
  control from a known stale attach client.
- Treat results, rendered scrollback, and raw output as potentially sensitive.

## Common commands

```bash
session=$(dlgt new --title "review" --profile fable-review --cwd . \
  -- "Review the current design")
# Parse .session.id from the JSON response and retain it.

dlgt send ses_7K3M9Q2X --wait --timeout 15m -- "Address the findings"
dlgt wait ses_7K3M9Q2X --timeout 15m
dlgt cancel ses_7K3M9Q2X
dlgt show ses_7K3M9Q2X
dlgt events ses_7K3M9Q2X --follow
dlgt scrollback ses_7K3M9Q2X --lines 100
dlgt attach ses_7K3M9Q2X
dlgt stop ses_7K3M9Q2X
```

Control-plane commands return compact JSON with `ok:true` or a structured
`ok:false` error. `events --follow` is NDJSON; `attach` and `logs --raw` are raw
streams. `rpc --stdio` exposes only the public Session-based v1 methods.
