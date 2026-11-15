#!/bin/sh
# A stand-in for `codex app-server` speaking JSON-RPC lines on stdio, driven
# by prompt keywords:
#   hold*   stay busy until interrupted or steered
#   ask*    request approval for a command, then report the decision

if [ -n "${DLGT_FAKE_CODEX_LOG:-}" ]; then
  printf 'args: %s\n' "$*" >>"$DLGT_FAKE_CODEX_LOG"
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

while IFS= read -r line; do
  if [ -n "${DLGT_FAKE_CODEX_LOG:-}" ]; then
    case "$line" in
      *'"method":"thread/start"'* | *'"method":"thread/resume"'*)
        printf '%s\n' "$line" >>"$DLGT_FAKE_CODEX_LOG"
        ;;
    esac
  fi
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"result":{"decision":'*)
      asking=0
      notify serverRequest/resolved "{\"threadId\":\"$thread\",\"requestId\":\"srv-$turns\"}"
      case "$line" in
        *'"decision":"accept"'*) finish allowed ;;
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
      reply '{"thread":{"status":{"type":"active"},"turns":[]}}'
      ;;
    *'"method":"model/list"'*) reply '{"data":[{"id":"fake-codex"}],"nextCursor":null}' ;;
    *'"method":"turn/start"'*)
      turns=$((turns + 1))
      turn=turn-$turns
      text=$(text_of)
      reply "{\"turn\":{\"id\":\"$turn\"}}"
      notify turn/started "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"items\":[]}}"
      case "$text" in
        hold*) holding=1 ;;
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
      reply '{}'
      if [ "$asking" = 1 ]; then
        notify serverRequest/resolved "{\"threadId\":\"$thread\",\"requestId\":\"srv-$turns\"}"
      fi
      asking=0
      holding=0
      notify turn/completed "{\"threadId\":\"$thread\",\"turn\":{\"id\":\"$turn\",\"status\":\"interrupted\",\"items\":[]}}"
      ;;
    *'"method":'*) [ -n "$id" ] && reply '{}' ;;
  esac
done
