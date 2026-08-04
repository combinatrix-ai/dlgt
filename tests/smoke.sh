#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/dlgt"
state_dir=$(mktemp -d "${TMPDIR:-/tmp}/dlgt-smoke.XXXXXX")
server_pid=
old_server_pid=

cleanup() {
  if [ -n "$server_pid" ]; then
    DLGT_HOME="$state_dir" "$binary" server stop >/dev/null 2>&1 || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ -n "$old_server_pid" ]; then
    DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null 2>&1 || true
    wait "$old_server_pid" 2>/dev/null || true
  fi
  rm -rf "$state_dir"
}
trap cleanup EXIT INT TERM

export DLGT_HOME="$state_dir"
export HOME="$state_dir/home"
export DLGT_CLAUDE_BIN="$repo_root/tests/fixtures/fake-agent.sh"
export DLGT_FAKE_ARGS_FILE="$state_dir/fake-args.log"
mkdir -p "$HOME"
touch "$DLGT_FAKE_ARGS_FILE"
old_socket="$state_dir/run/old/dlgt.sock"

version=$("$binary" --version | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
test -n "$version" || exit 1

"$binary" server --foreground >"$state_dir/server.log" 2>&1 &
server_pid=$!
attempt=0
while [ ! -S "$state_dir/run/$version/dlgt.sock" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done

# `new` is readiness-bounded. Start it while the fixture emits the authoritative hook.
"$binary" new --title smoke --alias @smoke --harness claude --cwd "$repo_root" \
  --request-id smoke-1 --harness-option permission-mode=auto -- smoke-initial >"$state_dir/new.json" &
new_pid=$!
attempt=0
launch_id=
while [ -z "$launch_id" ]; do
  launch_id=$(sed -n "s/.*hook emit '\\(internal:[0-9A-Z]*\\)' 'claude'.*/\\1/p" \
    "$DLGT_FAKE_ARGS_FILE" | tail -1)
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session"}' \
  | "$binary" hook emit "$launch_id" claude
wait "$new_pid"
session_id=claude:provider-session
grep -q '"id":"claude:provider-session"' "$state_dir/new.json"
grep -q '"alias":"@smoke"' "$state_dir/new.json"
if grep -q '"provider_session_id"\|"resume_ref"\|"internal:' "$state_dir/new.json"; then exit 1; fi
grep -q -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE"
if grep -q -- '^--dangerously-skip-permissions$' "$DLGT_FAKE_ARGS_FILE"; then exit 1; fi
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session","turn_id":"provider-turn-1","user_prompt":"smoke-initial"}' \
  | "$binary" hook emit "$session_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session","turn_id":"provider-turn-1","last_assistant_message":"initial-done"}' \
  | "$binary" hook emit "$session_id" claude
"$binary" fetch "$session_id" --until result --wait 2s | grep -q '"execution_seq":1'

# Acceptance returns an observation position taken before the accepted work.
grep -q '"cursor":"[0-9]*"' "$state_dir/new.json"

# A cursorless fetch is the documented baseline recovery path.
baseline_json=$("$binary" fetch "$session_id")
printf '%s\n' "$baseline_json" | grep -q '"reason":"snapshot"'
printf '%s\n' "$baseline_json" | grep -q '"final_text":"initial-done"'
printf '%s\n' "$baseline_json" | grep -q '"final_text_source":"hook"'
printf '%s\n' "$baseline_json" | grep -q '"screen"'
baseline_cursor=$(printf '%s\n' "$baseline_json" \
  | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
test -n "$baseline_cursor"

# An exhausted cursor is a successful empty observation, not an error, and it
# keeps the caller's own position: nothing advanced, so there is nothing new to
# name and no retained position is spent.
empty_json=$("$binary" fetch "$session_id" --cursor "$baseline_cursor")
printf '%s\n' "$empty_json" | grep -q '"reason":"timeout"'
printf '%s\n' "$empty_json" | grep -q '"results":\[\]'
printf '%s\n' "$empty_json" | grep -q '"has_more":false'
printf '%s\n' "$empty_json" | grep -q "\"cursor\":\"$baseline_cursor\""
"$binary" fetch "$session_id" --cursor "$baseline_cursor" \
  | grep -q "\"cursor\":\"$baseline_cursor\""
# A leading-zero spelling names the same position but is canonicalized before
# publication, so the no-mint echo cannot smuggle an arbitrarily long
# caller-chosen spelling past the measured byte bound.
"$binary" fetch "$session_id" --cursor "000000000$baseline_cursor" \
  | grep -q "\"cursor\":\"$baseline_cursor\""

# A current client routes provider-qualified selectors to a live daemon on a
# different versioned socket instead of launching a duplicate locally.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server.log" 2>&1 &
old_server_pid=$!
attempt=0
while [ ! -S "$old_socket" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
DLGT_SOCKET="$old_socket" "$binary" new --title cross-version --alias @cross-version \
  --harness claude --cwd "$repo_root" --request-id cross-1 -- cross-version-initial >"$state_dir/cross-version.json" &
cross_version_new_pid=$!
attempt=0
cross_version_launch_id=
while [ -z "$cross_version_launch_id" ] || [ "$cross_version_launch_id" = "$launch_id" ]; do
  cross_version_launch_id=$(sed -n "s/.*hook emit '\\(internal:[0-9A-Z]*\\)' 'claude'.*/\\1/p" \
    "$DLGT_FAKE_ARGS_FILE" | tail -1)
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-cross-version"}' \
  | DLGT_SOCKET="$old_socket" "$binary" hook emit "$cross_version_launch_id" claude
wait "$cross_version_new_pid"
cross_version_id=claude:provider-cross-version
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-cross-version","turn_id":"provider-cross-version-1","user_prompt":"cross-version-initial"}' \
  | DLGT_SOCKET="$old_socket" "$binary" hook emit "$cross_version_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-cross-version","turn_id":"provider-cross-version-1","last_assistant_message":"cross-version-ready"}' \
  | DLGT_SOCKET="$old_socket" "$binary" hook emit "$cross_version_id" claude
DLGT_SOCKET="$old_socket" "$binary" fetch "$cross_version_id" --until result --wait 2s >/dev/null
cross_version_send=$("$binary" send claude:provider-cross-version --request-id cross-2 \
  -- cross-version-follow-up)
printf '%s\n' "$cross_version_send" | grep -q "\"id\":\"$cross_version_id\""
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-cross-version","turn_id":"provider-cross-version-2","user_prompt":"cross-version-follow-up"}' \
  | DLGT_SOCKET="$old_socket" "$binary" hook emit "$cross_version_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-cross-version","turn_id":"provider-cross-version-2","last_assistant_message":"cross-version-done"}' \
  | DLGT_SOCKET="$old_socket" "$binary" hook emit "$cross_version_id" claude
stale_cursor=$(DLGT_SOCKET="$old_socket" "$binary" fetch --all \
  | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
test -n "$stale_cursor"
DLGT_SOCKET="$old_socket" "$binary" stop "$cross_version_id" --force >/dev/null
DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null
wait "$old_server_pid"
old_server_pid=

# A position is meaningful only within one daemon lifetime. Nothing marks a
# number as belonging to a previous daemon, and nothing needs to: the state it
# described is gone, so there is nothing to be lost by reusing the number.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server-2.log" 2>&1 &
old_server_pid=$!
attempt=0
while [ ! -S "$old_socket" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
# Before this daemon has minted that far, the position is simply not held.
set +e
expired_json=$(DLGT_SOCKET="$old_socket" "$binary" fetch --all --cursor "$stale_cursor")
expired_status=$?
set -e
test "$expired_status" -eq 1
printf '%s\n' "$expired_json" | grep -q '"code":"CURSOR_EXPIRED"'
# Once it has, the same number names this daemon's window of that position:
# current data, never the previous daemon's world.
DLGT_SOCKET="$old_socket" "$binary" fetch --all | grep -q '"reason":"snapshot"'
reused_json=$(DLGT_SOCKET="$old_socket" "$binary" fetch --all --cursor "$stale_cursor")
printf '%s\n' "$reused_json" | grep -q '"ok":true'
if printf '%s\n' "$reused_json" | grep -q 'provider-cross-version'; then exit 1; fi
DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null
wait "$old_server_pid"
old_server_pid=

# Every acceptance must carry an idempotency key.
set +e
no_id_json=$("$binary" send "$session_id" -- no-key)
no_id_status=$?
set -e
test "$no_id_status" -eq 1
printf '%s\n' "$no_id_json" | grep -q '"code":"INVALID_ARGUMENT"'
printf '%s\n' "$no_id_json" | grep -q -- '--request-id'

# Bounded launch failures retain the failed audit Session ID for diagnostics.
set +e
launch_failure_json=$("$binary" new --title launch-failure --alias @launch-failure \
  --harness claude --cwd "$repo_root" --request-id launch-failure-1 --startup-timeout 50ms \
  -- launch-failure)
launch_failure_status=$?
set -e
test "$launch_failure_status" -eq 1
printf '%s\n' "$launch_failure_json" | grep -q '"code":"LAUNCH_FAILED"'
printf '%s\n' "$launch_failure_json" | grep -Eq '"launch_id":"internal:[0-9A-Z]{8}"'
launch_failure_id=$(printf '%s\n' "$launch_failure_json" \
  | sed -n 's/.*"launch_id":"\([^"]*\)".*/\1/p')

long_message=$(awk 'BEGIN { for (i = 0; i < 12000; i++) printf "x" }')
send_json=$("$binary" send "$session_id" --request-id smoke-2 -- "$long_message")
printf '%s\n' "$send_json" | grep -q '"execution_seq":2'

# A running execution must not hide the answer to the previous one.
busy_baseline=$("$binary" fetch "$session_id" --no-screen)
printf '%s\n' "$busy_baseline" | grep -q '"state":"busy"'
printf '%s\n' "$busy_baseline" | grep -q '"final_text":"initial-done"'
"$binary" show "$session_id" | grep -q '"final_text":"initial-done"'

set +e
busy_json=$("$binary" send "$session_id" --request-id smoke-busy -- second)
busy_status=$?
set -e
test "$busy_status" -eq 5
printf '%s\n' "$busy_json" | grep -q '"code":"SESSION_BUSY"'

printf '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session","turn_id":"provider-turn-2","user_prompt":"%s"}\n' "$long_message" \
  | "$binary" hook emit "$session_id" claude
printf '{"hook_event_name":"Stop","session_id":"provider-session","turn_id":"provider-turn-2","last_assistant_message":"done"}\n' \
  | "$binary" hook emit "$session_id" claude
wait_json=$("$binary" fetch "$session_id" --until result --wait 2s)
printf '%s\n' "$wait_json" | grep -q '"status":"completed"'
printf '%s\n' "$wait_json" | grep -q '"final_text":"done"'
printf '%s\n' "$wait_json" | grep -q '"execution_seq":2'
if printf '%s\n' "$wait_json" | grep -q 'turn_'; then exit 1; fi

set +e
plain_logs=$("$binary" logs "$session_id")
plain_status=$?
set -e
test "$plain_status" -eq 1
printf '%s\n' "$plain_logs" | grep -q '"code":"INVALID_ARGUMENT"'
"$binary" logs "$session_id" --raw --json | grep -q '"data_base64"'
"$binary" scrollback "$session_id" --lines 10 | grep -q '"lines"'
"$binary" events "$session_id" | grep -q '"schema_version":1'
# The public timeline is materialized against the provider-qualified ID; the
# pre-bind launch events are plumbing and are never published.
"$binary" events "$session_id" | grep -q '"type":"session.created"'
if "$binary" events "$session_id" | grep -q 'internal:'; then exit 1; fi
if "$binary" events | grep -q 'internal:'; then exit 1; fi
"$binary" events "$session_id" --follow >"$state_dir/follow.jsonl" &
follow_pid=$!
attempt=0
while [ ! -s "$state_dir/follow.jsonl" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
kill "$follow_pid"
wait "$follow_pid" 2>/dev/null || true
grep -q '"schema_version":1' "$state_dir/follow.jsonl"
# The response envelope echoes the request id, so the id is bounded.
long_rpc_id=$(awk 'BEGIN { for (i = 0; i < 300; i++) printf "i" }')
printf '{"id":"%s","method":"session.list","params":{}}\n' "$long_rpc_id" \
  | "$binary" rpc --stdio >"$state_dir/rpc-long-id.json"
grep -q '"code":"INVALID_ARGUMENT"' "$state_dir/rpc-long-id.json"
test "$(wc -c <"$state_dir/rpc-long-id.json" | tr -d ' ')" -lt 300

# A fetch through the stdio proxy still respects the result-document bound;
# only the JSONL envelope and its short id sit outside it.
printf '{"id":"r1","method":"session.fetch","params":{"session":"%s","max_bytes":2048,"screen":false}}\n' \
  "$session_id" | "$binary" rpc --stdio >"$state_dir/rpc-fetch.json"
grep -q '"cursor":"[0-9]*"' "$state_dir/rpc-fetch.json"
rpc_bytes=$(wc -c <"$state_dir/rpc-fetch.json" | tr -d ' ')
test "$rpc_bytes" -le 2148 || {
  printf 'rpc fetch line was %s bytes, over 2048 plus its envelope\n' "$rpc_bytes" >&2
  exit 1
}

# A position is a number, so raw RPC accepts it spelled either way. Absent and
# null mean "baseline"; anything that is not a position is a malformed request
# rather than a silent baseline.
rpc_fetch() {
  printf '{"id":"c1","method":"session.fetch","params":{"session":"%s","screen":false,%s}}\n' \
    "$session_id" "$1" | "$binary" rpc --stdio
}
rpc_fetch '"cursor":"1"' | grep -q '"cursor"'
rpc_fetch '"cursor":1' | grep -q '"cursor"'
rpc_fetch '"cursor":null' | grep -q '"reason":"snapshot"'
rpc_fetch '"max_bytes":32768' | grep -q '"reason":"snapshot"'
for shape in 'true' '{}' '[]' '"not-a-number"'; do
  rpc_fetch "\"cursor\":$shape" | grep -q '"code":"CURSOR_INVALID"' || {
    printf 'cursor shape %s was not rejected\n' "$shape" >&2
    exit 1
  }
done

# Acceptance over raw RPC needs the same idempotency key the CLI requires.
rpc_accept() {
  printf '{"id":"a1","method":"session.send","params":{"session":"%s","prompt":"x"%s}}\n' \
    "$session_id" "$1" | "$binary" rpc --stdio
}
rpc_accept '' | grep -q '"code":"INVALID_ARGUMENT"'
rpc_accept ',"request_id":""' | grep -q '"code":"INVALID_ARGUMENT"'
long_key=$(awk 'BEGIN { for (i = 0; i < 129; i++) printf "k" }')
rpc_accept ",\"request_id\":\"$long_key\"" | grep -q '"code":"INVALID_ARGUMENT"'
printf '{"id":"a2","method":"session.create","params":{"title":"t","harness":"claude","prompt":"x","cwd":"%s","environment":{}}}\n' \
  "$repo_root" | "$binary" rpc --stdio | grep -q '"code":"INVALID_ARGUMENT"'

"$binary" models --harness claude | grep -q '"id":"default"'
"$binary" harnesses | grep -q '"codex"'
"$binary" profiles | grep -q '"profiles"'
"$binary" skill | grep -q '^name: dlgt$'

# attach is an interactive takeover and must refuse a piped stdout.
set +e
attach_json=$("$binary" attach "$session_id")
attach_status=$?
set -e
test "$attach_status" -eq 1
printf '%s\n' "$attach_json" | grep -q '"code":"ATTACH_REQUIRES_TTY"'

# Restart interrupts active work while preserving identity, provider binding, and history.
"$binary" send "$session_id" --request-id smoke-3 -- interrupted-by-restart >/dev/null
option_count_before=$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")
"$binary" restart "$session_id" >"$state_dir/restart.json" &
restart_pid=$!
attempt=0
while ! "$binary" show "$session_id" | grep -q '"state":"starting"\|"state":"running"'; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session"}' \
  | "$binary" hook emit "$session_id" claude
wait "$restart_pid"
option_count_after=$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")
test "$option_count_after" -gt "$option_count_before"
grep -q "\"id\":\"$session_id\"" "$state_dir/restart.json"
"$binary" show "$session_id" | grep -q '"execution_seq":3'
"$binary" show "$session_id" | grep -q '"status":"interrupted"'
"$binary" send "$session_id" --request-id smoke-4 -- after-restart >/dev/null
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session","turn_id":"provider-turn-4","user_prompt":"after-restart"}' \
  | "$binary" hook emit "$session_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session","turn_id":"provider-turn-4","last_assistant_message":"resumed"}' \
  | "$binary" hook emit "$session_id" claude
"$binary" fetch "$session_id" --until result --wait 2s | grep -q '"execution_seq":4'
"$binary" events "$session_id" | grep -q '"type":"session.restarting"'
"$binary" stop "$session_id" --force >/dev/null
attempt=0
while "$binary" show @smoke >/dev/null 2>&1; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done

# Exact aliases are reusable after terminal stop, while the old Session ID remains readable.
"$binary" new --title reused --alias @smoke --harness claude --cwd "$repo_root" \
  --request-id reused-1 -- reused-initial >"$state_dir/reused.json" &
new_pid=$!
attempt=0
reused_launch_id=
while [ -z "$reused_launch_id" ] \
  || [ "$reused_launch_id" = "$cross_version_launch_id" ] \
  || [ "$reused_launch_id" = "$launch_failure_id" ]; do
  reused_launch_id=$(sed -n "s/.*hook emit '\\(internal:[0-9A-Z]*\\)' 'claude'.*/\\1/p" \
    "$DLGT_FAKE_ARGS_FILE" | tail -1)
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session-2"}' \
  | "$binary" hook emit "$reused_launch_id" claude
wait "$new_pid"
reused_id=claude:provider-session-2
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session-2","turn_id":"provider-turn-reused-1","user_prompt":"reused-initial"}' \
  | "$binary" hook emit "$reused_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session-2","turn_id":"provider-turn-reused-1","last_assistant_message":"reused-ready"}' \
  | "$binary" hook emit "$reused_id" claude
"$binary" fetch "$reused_id" --until result --wait 2s | grep -q '"execution_seq":1'
"$binary" show "$session_id" | grep -q '"state":"stopped"'

# A long poll completes on the authoritative Stop hook, and the delta carries
# the hydrated result exactly once.
poll_send=$("$binary" send "$reused_id" --request-id reused-2 -- long-poll)
poll_cursor=$(printf '%s\n' "$poll_send" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
test -n "$poll_cursor"
"$binary" fetch "$reused_id" --cursor "$poll_cursor" --until result --wait 10s \
  >"$state_dir/poll.json" &
poll_pid=$!
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session-2","turn_id":"provider-turn-reused-2","user_prompt":"long-poll"}' \
  | "$binary" hook emit "$reused_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session-2","turn_id":"provider-turn-reused-2","last_assistant_message":"poll-done"}' \
  | "$binary" hook emit "$reused_id" claude
wait "$poll_pid"
grep -q '"reason":"result"' "$state_dir/poll.json"
grep -q '"final_text":"poll-done"' "$state_dir/poll.json"
grep -q '"execution_seq":2' "$state_dir/poll.json"
advanced_cursor=$(sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p' "$state_dir/poll.json")

# Replaying the acceptance cursor replays the same immutable window, while the
# advanced cursor no longer redelivers it.
"$binary" fetch "$reused_id" --cursor "$poll_cursor" | grep -q '"final_text":"poll-done"'
"$binary" fetch "$reused_id" --cursor "$advanced_cursor" | grep -q '"results":\[\]'

# A retried acceptance replays its receipt instead of creating a second
# execution, and the same request ID with a different payload is rejected.
retry_first=$("$binary" send "$reused_id" --request-id retry-1 -- retry-payload)
printf '%s\n' "$retry_first" | grep -q '"execution_seq":3'
retry_replay=$("$binary" send "$reused_id" --request-id retry-1 -- retry-payload)
printf '%s\n' "$retry_replay" | grep -q '"replayed":true'
printf '%s\n' "$retry_replay" | grep -q '"execution_seq":3'
set +e
retry_mismatch=$("$binary" send "$reused_id" --request-id retry-1 -- other-payload)
retry_mismatch_status=$?
set -e
test "$retry_mismatch_status" -eq 1
printf '%s\n' "$retry_mismatch" | grep -q '"code":"INVALID_ARGUMENT"'
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session-2","turn_id":"provider-turn-reused-3","user_prompt":"retry-payload"}' \
  | "$binary" hook emit "$reused_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session-2","turn_id":"provider-turn-reused-3","last_assistant_message":"retry-done"}' \
  | "$binary" hook emit "$reused_id" claude
"$binary" fetch "$reused_id" --until result --wait 4s | grep -q '"final_text":"retry-done"'

# Retention overrun is a structured gap with a recoverable baseline, never a
# silent reset.
flood_send=$("$binary" send "$reused_id" --request-id reused-4 -- flood-the-screen)
flood_cursor=$(printf '%s\n' "$flood_send" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
attempt=0
until "$binary" fetch "$reused_id" --cursor "$flood_cursor" \
  | grep -q '"reason":"gap"'; do
  attempt=$((attempt + 1)); test "$attempt" -lt 400 || exit 1; sleep 0.05
done
gap_json=$("$binary" fetch "$reused_id" --cursor "$flood_cursor")
printf '%s\n' "$gap_json" | grep -q '"component":"screen","reason":"retention_overrun"'
printf '%s\n' "$gap_json" | grep -q '"stable":\["flood-'
gap_cursor=$(printf '%s\n' "$gap_json" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
"$binary" fetch "$reused_id" --cursor "$flood_cursor" --screen=5 | grep -q '"stable":\["flood-'
if "$binary" fetch "$reused_id" --cursor "$flood_cursor" --no-screen \
  | grep -q '"screen"'; then exit 1; fi
"$binary" scrollback "$reused_id" --lines 5 | grep -q '"truncated":true'

# A long final_text is chunked at a UTF-8 boundary and continued by cursor.
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session-2","turn_id":"provider-turn-reused-4","user_prompt":"flood-the-screen"}' \
  | "$binary" hook emit "$reused_id" claude
printf '{"hook_event_name":"Stop","session_id":"provider-session-2","turn_id":"provider-turn-reused-4","last_assistant_message":"%s"}\n' "$long_message" \
  | "$binary" hook emit "$reused_id" claude
chunk_json=$("$binary" fetch "$reused_id" --cursor "$gap_cursor" --no-screen --max-bytes 2048)
printf '%s\n' "$chunk_json" | grep -q '"final_text_complete":false'
printf '%s\n' "$chunk_json" | grep -q '"final_text_offset":0'
printf '%s\n' "$chunk_json" | grep -q '"has_more":true'
printf '%s\n' "$chunk_json" | grep -q '"reason":"page_full"'
# The budget went mostly to the result body, so the terminal event is left for
# the next page and the watermark must not have advanced past it.
if printf '%s\n' "$chunk_json" | grep -q '"type":"session.idle"'; then exit 1; fi
# The bound covers the complete compact response line the client prints,
# wrapper and trailing newline included.
chunk_bytes=$(printf '%s\n' "$chunk_json" | wc -c | tr -d ' ')
test "$chunk_bytes" -le 2048 || {
  printf 'squeezed fetch line was %s bytes, over its 2048 budget\n' "$chunk_bytes" >&2
  exit 1
}
chunk_cursor=$(printf '%s\n' "$chunk_json" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
rest_json=$("$binary" fetch "$reused_id" --cursor "$chunk_cursor" --no-screen)
printf '%s\n' "$rest_json" | grep -q '"final_text_complete":true'
if printf '%s\n' "$rest_json" | grep -q '"final_text_offset":0'; then exit 1; fi
printf '%s\n' "$rest_json" | grep -q '"type":"session.idle"'

# A position is a small number a caller can carry, and it counts up per
# Session from its first acceptance.
printf '%s\n' "$baseline_json" | grep -q '"cursor":"[0-9]*"'
test "$(printf '%s\n' "$baseline_json" | sed -n 's/.*"cursor":"\([0-9]*\)".*/\1/p')" -gt 0

# Positions are retained in bounded number: an evicted one expires, and the
# documented recovery is one cursorless baseline.
evicted_cursor=$("$binary" fetch "$reused_id" --no-screen \
  | sed -n 's/.*"cursor":"\([0-9]*\)".*/\1/p')
attempt=0
while [ "$attempt" -lt 80 ]; do
  "$binary" fetch "$reused_id" --no-screen >/dev/null
  attempt=$((attempt + 1))
done
set +e
expired_position=$("$binary" fetch "$reused_id" --cursor "$evicted_cursor" --no-screen)
expired_position_status=$?
set -e
test "$expired_position_status" -eq 1
printf '%s\n' "$expired_position" | grep -q '"code":"CURSOR_EXPIRED"'
"$binary" fetch "$reused_id" --no-screen | grep -q '"reason":"snapshot"'

# A non-numeric position is malformed rather than expired.
set +e
bad_position=$("$binary" fetch "$reused_id" --cursor not-a-number)
bad_position_status=$?
set -e
test "$bad_position_status" -eq 1
printf '%s\n' "$bad_position" | grep -q '"code":"CURSOR_INVALID"'

# fetch --all reports every Session of one daemon without a screen projection.
all_json=$("$binary" fetch --all)
printf '%s\n' "$all_json" | grep -q "\"id\":\"$reused_id\""
if printf '%s\n' "$all_json" | grep -q '"screen"'; then exit 1; fi
set +e
all_screen=$("$binary" fetch --all --screen)
all_screen_status=$?
set -e
test "$all_screen_status" -eq 1
printf '%s\n' "$all_screen" | grep -q '"code":"INVALID_ARGUMENT"'

# A default Session adds --permission-mode=auto beyond the one explicit
# harness option, and never the dangerous bypass flag.
test "$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")" -ge 2
if grep -q -- '^--dangerously-skip-permissions$' "$DLGT_FAKE_ARGS_FILE"; then exit 1; fi

# Restart never steals an alias that a newer active Session owns.
set +e
alias_json=$("$binary" restart "$session_id")
alias_status=$?
set -e
test "$alias_status" -eq 1 || {
  printf 'unexpected alias restart status %s: %s\n' "$alias_status" "$alias_json" >&2
  exit 1
}
printf '%s\n' "$alias_json" | grep -q '"code":"ALIAS_IN_USE"' || {
  printf 'unexpected alias restart error: %s\n' "$alias_json" >&2
  exit 1
}
"$binary" show "$session_id" | grep -q '"state":"stopped"'

# Unexpected provider death creates a durable failed result in bounded time.
"$binary" send "$reused_id" --request-id reused-5 -- crash >/dev/null
set +e
crash_json=$("$binary" fetch "$reused_id" --until result --wait 8s)
crash_status=$?
set -e
test "$crash_status" -eq 0 || {
  printf 'unexpected provider crash status %s: %s\n' "$crash_status" "$crash_json" >&2
  exit 1
}
printf '%s\n' "$crash_json" | grep -q '"status":"failed"' || {
  printf 'unexpected provider crash result: %s\n' "$crash_json" >&2
  exit 1
}

# A rotated provider Session ID keeps the pre-rekey address and its cursor
# usable, and resuming the conversation keeps the retained screen history.
"$binary" new --title rekey --alias @rekey --harness claude --cwd "$repo_root" \
  --request-id rekey-1 -- rekey-initial >"$state_dir/rekey.json" &
new_pid=$!
attempt=0
rekey_launch_id=
while [ -z "$rekey_launch_id" ] || [ "$rekey_launch_id" = "$reused_launch_id" ]; do
  rekey_launch_id=$(sed -n "s/.*hook emit '\\(internal:[0-9A-Z]*\\)' 'claude'.*/\\1/p" \
    "$DLGT_FAKE_ARGS_FILE" | tail -1)
  attempt=$((attempt + 1)); test "$attempt" -lt 200 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-rekey-1"}' \
  | "$binary" hook emit "$rekey_launch_id" claude
wait "$new_pid"
rekey_id=claude:provider-rekey-1
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-rekey-1","turn_id":"rekey-turn-1","user_prompt":"rekey-initial"}' \
  | "$binary" hook emit "$rekey_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-rekey-1","turn_id":"rekey-turn-1","last_assistant_message":"rekey-ready"}' \
  | "$binary" hook emit "$rekey_id" claude
rekey_cursor=$("$binary" fetch "$rekey_id" --until result --wait 4s \
  | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
test -n "$rekey_cursor"

"$binary" restart "$rekey_id" >"$state_dir/rekey-restart.json" &
restart_pid=$!
attempt=0
while ! "$binary" show "$rekey_id" | grep -q '"state":"starting"\|"state":"running"'; do
  attempt=$((attempt + 1)); test "$attempt" -lt 200 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-rekey-2"}' \
  | "$binary" hook emit "$rekey_id" claude
wait "$restart_pid"
rotated_id=claude:provider-rekey-2
grep -q "\"id\":\"$rotated_id\"" "$state_dir/rekey-restart.json"
# The pre-rekey address and a cursor taken before the rotation both still
# resolve to the same logical Session.
"$binary" fetch "$rekey_id" --cursor "$rekey_cursor" | grep -q "\"id\":\"$rotated_id\""
"$binary" scrollback "$rotated_id" --lines 200 | grep -q 'fake:rekey-initial'

"$binary" stop "$rotated_id" --force >/dev/null
attempt=0
while "$binary" show @rekey >/dev/null 2>&1; do
  attempt=$((attempt + 1)); test "$attempt" -lt 200 || exit 1; sleep 0.02
done
"$binary" send "$rotated_id" --resume --request-id rekey-2 -- resumed-prompt >"$state_dir/rekey-resume.json" &
resume_pid=$!
attempt=0
resume_launch_id=
while [ -z "$resume_launch_id" ] || [ "$resume_launch_id" = "$rekey_launch_id" ]; do
  resume_launch_id=$(sed -n "s/.*hook emit '\\(internal:[0-9A-Z]*\\)' 'claude'.*/\\1/p" \
    "$DLGT_FAKE_ARGS_FILE" | tail -1)
  attempt=$((attempt + 1)); test "$attempt" -lt 200 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-rekey-2"}' \
  | "$binary" hook emit "$resume_launch_id" claude
wait "$resume_pid"
grep -q "\"id\":\"$rotated_id\"" "$state_dir/rekey-resume.json"
# Resuming the same provider conversation continues one logical Session, so
# the screen history recorded before the resume is still readable.
"$binary" scrollback "$rotated_id" --lines 400 | grep -q 'fake:rekey-initial'
"$binary" fetch "$rekey_id" --cursor "$rekey_cursor" | grep -q "\"id\":\"$rotated_id\""
"$binary" stop "$rotated_id" --force >/dev/null

"$binary" server stop >/dev/null
wait "$server_pid"
server_pid=
echo "dlgt smoke test passed"
