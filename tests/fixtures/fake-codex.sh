#!/bin/sh
# A stand-in for `codex app-server` speaking JSON-RPC lines on stdio, driven
# by prompt keywords:
#   hold*   stay busy until interrupted or steered
#   ask*    request approval for a command, then report the decision

if [ -n "${DLGT_FAKE_PID_LOG:-}" ]; then
  printf '%s\n' "$$" >>"$DLGT_FAKE_PID_LOG"
fi
if [ -n "${DLGT_FAKE_CODEX_LOG:-}" ]; then
  printf 'args: %s\n' "$*" >>"$DLGT_FAKE_CODEX_LOG"
fi
if [ -n "${DLGT_FAKE_CODEX_PID_FILE:-}" ]; then
  printf '%s\n' "$$" >"$DLGT_FAKE_CODEX_PID_FILE"
fi

thread=thread-$$
turn=
turns=0
holding=0
asking=0

reply() {
  printf '{"id":%s,"result":%s}\n' "$id" "$1"
}
notify() {
  printf '{"method":"%s","params":%s}\n' "$1" "$2"
}
text_of() {
  printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'
}
finish() {
  item="{\"type\":\"agentMessage\",\"id\":\"msg-$turns\",\"text\":\"$1\"}"
  notify item/completed "{\"threadId\":\"$thread\",\"turnId\":\"$turn\",\"item\":$item}"
  notify turn/completed "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"status\":\"completed\",\"items\":[$item]}}"
  holding=0
}
finish_empty() {
  notify turn/completed "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"status\":\"completed\",\"items\":[]}}"
  holding=0
}

while IFS= read -r line; do
  if [ -n "${DLGT_FAKE_CODEX_LOG:-}" ]; then
    case "$line" in
      *'"method":"thread/start"'* | *'"method":"thread/resume"'*)
        printf '%s\n' "$line" >>"$DLGT_FAKE_CODEX_LOG"
        ;;
      *'"method":"turn/steer"'* | *'"method":"turn/interrupt"'* | *'"id":"srv-'*'"result"'*)
        printf '%s\n' "$line" >>"$DLGT_FAKE_CODEX_LOG"
        ;;
    esac
  fi
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"result":{"decision":'* | *'"result":{"action":"accept"'*)
      asking=0
      notify serverRequest/resolved "{\"threadId\":\"$thread\",\"requestId\":\"srv-$turns\"}"
      case "$line" in
        *'"decision":"accept"'*) finish allowed ;;
        *'"action":"accept"'*)
          name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
          count=$(printf '%s' "$line" | sed -n 's/.*"count":\([0-9][0-9]*\).*/\1/p')
          finish "form:$name:$count"
          ;;
        *) finish denied ;;
      esac
      ;;
    *'"method":"initialize"'*) reply '{}' ;;
    *'"method":"thread/start"'*)
      reply "{\"thread\":{\"id\":\"$thread\"},\"model\":\"fake-codex\"}"
      notify thread/started "{\"thread\":{\"id\":\"$thread\"}}"
      ;;
    *'"method":"thread/resume"'*)
      thread=$(printf '%s' "$line" | sed -n 's/.*"threadId":"\([^"]*\)".*/\1/p')
      reply "{\"thread\":{\"id\":\"$thread\"},\"model\":\"fake-codex\"}"
      ;;
    *'"method":"thread/read"'*)
      if [ -n "${DLGT_FAKE_THREAD_READ_LOG:-}" ]; then echo read >>"$DLGT_FAKE_THREAD_READ_LOG"; fi
      reply '{"thread":{"status":{"type":"active"},"turns":[]}}'
      ;;
    *'"method":"model/list"'*) reply '{"data":[{"id":"fake-codex"}],"nextCursor":null}' ;;
    *'"method":"turn/start"'*)
      turns=$((turns + 1))
      turn=turn-$turns
      text=$(text_of)
      if [ -n "${DLGT_FAKE_DELAY_TURN_START_ACK:-}" ]; then
        if [ -n "${DLGT_FAKE_TURN_START_SEEN:-}" ]; then
          printf '%s\n' "$turn" >"${DLGT_FAKE_TURN_START_SEEN}"
        fi
        while [ ! -e "${DLGT_FAKE_DELAY_TURN_START_ACK}" ]; do sleep 0.02; done
        unset DLGT_FAKE_DELAY_TURN_START_ACK
      fi
      reply "{\"turn\":{\"id\":\"$turn\"}}"
      notify turn/started "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"items\":[]}}"
      case "$text" in
        hold*) holding=1 ;;
        crash-hold*)
          sleep 30 &
          printf '%s\n' "$!" >"${DLGT_FAKE_HOLDER_PID_FILE:-/dev/null}"
          exit 1
          ;;
        empty*) finish_empty ;;
        form*)
          asking=1
          notify item/started "{\"threadId\":\"$thread\",\"turnId\":\"$turn\",\"item\":{\"type\":\"mcpElicitation\",\"id\":\"form-$turns\"}}"
          printf '{"id":"srv-%s","method":"mcpServer/elicitation/request","params":{"threadId":"%s","turnId":"%s","serverName":"profile","mode":"form","message":"Complete the profile","requestedSchema":{"type":"object","properties":{"name":{"type":"string","minLength":1},"count":{"type":"integer","minimum":1}},"required":["name","count"]}}}\n' \
            "$turns" "$thread" "$turn"
          ;;
        ask*)
          asking=1
          notify item/started "{\"threadId\":\"$thread\",\"turnId\":\"$turn\",\"item\":{\"type\":\"commandExecution\",\"id\":\"cmd-$turns\",\"command\":\"touch outside.txt\"}}"
          printf '{"id":"srv-%s","method":"item/commandExecution/requestApproval","params":{"threadId":"%s","turnId":"%s","itemId":"cmd-%s","startedAtMs":1,"command":"touch outside.txt","cwd":"/work","reason":"writes outside the workspace"}}\n' \
            "$turns" "$thread" "$turn" "$turns"
          ;;
        *) finish "fake:$text" ;;
      esac
      ;;
    *'"method":"turn/steer"'*)
      reply "{\"turnId\":\"$turn\"}"
      [ "$holding" = 1 ] && finish "fake:$(text_of)"
      ;;
    *'"method":"turn/interrupt"'*)
      [ -z "${DLGT_FAKE_INTERRUPT_REPLY_LAST:-}" ] && reply '{}'
      if [ "$asking" = 1 ]; then
        notify serverRequest/resolved "{\"threadId\":\"$thread\",\"requestId\":\"srv-$turns\"}"
      fi
      asking=0
      holding=0
      notify turn/completed "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"status\":\"interrupted\",\"items\":[]}}"
      if [ -n "${DLGT_FAKE_INTERRUPT_REPLY_LAST:-}" ]; then sleep 0.2; reply '{}'; fi
      ;;
    *'"method":'*) [ -n "$id" ] && reply '{}' ;;
  esac
done
