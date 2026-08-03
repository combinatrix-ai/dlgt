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
  --harness-option permission-mode=auto -- smoke-initial >"$state_dir/new.json" &
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

# Acceptance returns an observation cursor positioned before the accepted work.
grep -q '"cursor":"f1\.' "$state_dir/new.json"

# A cursorless fetch is the documented baseline recovery path.
baseline_json=$("$binary" fetch "$session_id")
printf '%s\n' "$baseline_json" | grep -q '"reason":"snapshot"'
printf '%s\n' "$baseline_json" | grep -q '"final_text":"initial-done"'
printf '%s\n' "$baseline_json" | grep -q '"final_text_source":"hook"'
printf '%s\n' "$baseline_json" | grep -q '"screen"'
baseline_cursor=$(printf '%s\n' "$baseline_json" \
  | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
test -n "$baseline_cursor"

# An exhausted cursor is a successful empty observation, not an error.
empty_json=$("$binary" fetch "$session_id" --cursor "$baseline_cursor")
printf '%s\n' "$empty_json" | grep -q '"reason":"timeout"'
printf '%s\n' "$empty_json" | grep -q '"results":\[\]'
printf '%s\n' "$empty_json" | grep -q '"has_more":false'

# A current client routes provider-qualified selectors to a live daemon on a
# different versioned socket instead of launching a duplicate locally.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server.log" 2>&1 &
old_server_pid=$!
attempt=0
while [ ! -S "$old_socket" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
DLGT_SOCKET="$old_socket" "$binary" new --title cross-version --alias @cross-version \
  --harness claude --cwd "$repo_root" -- cross-version-initial >"$state_dir/cross-version.json" &
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
cross_version_send=$("$binary" send claude:provider-cross-version -- cross-version-follow-up)
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

# Runtime state is memory-only, so no cursor may survive a daemon instance.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server-2.log" 2>&1 &
old_server_pid=$!
attempt=0
while [ ! -S "$old_socket" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
set +e
expired_json=$(DLGT_SOCKET="$old_socket" "$binary" fetch --all --cursor "$stale_cursor")
expired_status=$?
set -e
test "$expired_status" -eq 1
printf '%s\n' "$expired_json" | grep -q '"code":"CURSOR_EXPIRED"'
DLGT_SOCKET="$old_socket" "$binary" fetch --all | grep -q '"reason":"snapshot"'
DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null
wait "$old_server_pid"
old_server_pid=

# Bounded launch failures retain the failed audit Session ID for diagnostics.
set +e
launch_failure_json=$("$binary" new --title launch-failure --alias @launch-failure \
  --harness claude --cwd "$repo_root" --startup-timeout 50ms -- launch-failure)
launch_failure_status=$?
set -e
test "$launch_failure_status" -eq 1
printf '%s\n' "$launch_failure_json" | grep -q '"code":"LAUNCH_FAILED"'
printf '%s\n' "$launch_failure_json" | grep -Eq '"launch_id":"internal:[0-9A-Z]{8}"'
launch_failure_id=$(printf '%s\n' "$launch_failure_json" \
  | sed -n 's/.*"launch_id":"\([^"]*\)".*/\1/p')

long_message=$(awk 'BEGIN { for (i = 0; i < 12000; i++) printf "x" }')
send_json=$("$binary" send "$session_id" -- "$long_message")
printf '%s\n' "$send_json" | grep -q '"execution_seq":2'

set +e
busy_json=$("$binary" send "$session_id" -- second)
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
"$binary" events "$session_id" --follow >"$state_dir/follow.jsonl" &
follow_pid=$!
attempt=0
while [ ! -s "$state_dir/follow.jsonl" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
kill "$follow_pid"
wait "$follow_pid" 2>/dev/null || true
grep -q '"schema_version":1' "$state_dir/follow.jsonl"
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
"$binary" send "$session_id" -- interrupted-by-restart >/dev/null
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
"$binary" send "$session_id" -- after-restart >/dev/null
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
  -- reused-initial >"$state_dir/reused.json" &
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
poll_send=$("$binary" send "$reused_id" -- long-poll)
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
flood_send=$("$binary" send "$reused_id" -- flood-the-screen)
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
chunk_json=$("$binary" fetch "$reused_id" --cursor "$gap_cursor" --no-screen --max-bytes 4096)
printf '%s\n' "$chunk_json" | grep -q '"final_text_complete":false'
printf '%s\n' "$chunk_json" | grep -q '"final_text_offset":0'
printf '%s\n' "$chunk_json" | grep -q '"has_more":true'
printf '%s\n' "$chunk_json" | grep -q '"reason":"page_full"'
chunk_cursor=$(printf '%s\n' "$chunk_json" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
rest_json=$("$binary" fetch "$reused_id" --cursor "$chunk_cursor" --no-screen)
printf '%s\n' "$rest_json" | grep -q '"final_text_complete":true'
if printf '%s\n' "$rest_json" | grep -q '"final_text_offset":0'; then exit 1; fi

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
"$binary" send "$reused_id" -- crash >/dev/null
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

"$binary" server stop >/dev/null
wait "$server_pid"
server_pid=
echo "dlgt smoke test passed"
