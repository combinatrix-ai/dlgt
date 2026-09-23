// dlgt lifecycle bridge for the interactive Pi TUI.
// Completion waits for agent_settled, the same signal RPC clients wait for.
// Inert unless this process is a dlgt child (DLGT_PI_LAUNCH).
import { spawnSync } from "node:child_process";

const launch = process.env.DLGT_PI_LAUNCH || "";
const bin = process.env.DLGT_PI_HOOK_BIN || "";

function emit(payload) {
  if (!launch || !bin) return;
  spawnSync(bin, ["hook", "emit", launch, "pi"], {
    input: JSON.stringify(payload),
    encoding: "utf8",
  });
}

function clean(text) {
  return String(text || "")
    .replace(/\u001b\[200~/g, "")
    .replace(/\u001b\[201~/g, "");
}

function sessionId(ctx) {
  const manager = ctx && ctx.sessionManager;
  if (!manager) return "";
  if (typeof manager.getSessionId === "function") {
    const direct = manager.getSessionId();
    if (typeof direct === "string" && direct) return direct;
  }
  const file =
    typeof manager.getSessionFile === "function" ? manager.getSessionFile() : manager.sessionFile;
  const match = String(file || "").match(
    /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i,
  );
  return match ? match[0] : "";
}

function assistantText(message) {
  if (!message || message.role !== "assistant") return "";
  if (typeof message.content === "string") return message.content;
  if (!Array.isArray(message.content)) return "";
  return message.content
    .filter((part) => part && part.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("");
}

export default function (pi) {
  if (!launch || !bin || !pi || typeof pi.on !== "function") return;
  let id = "";
  let cwd = "";
  let prompt = "";
  let assistant = "";
  let awaiting = false;
  let failed = false;
  let started = false;

  function currentId(ctx) {
    const found = sessionId(ctx);
    if (found) id = found;
    return id;
  }

  function emitStart(ctx) {
    const found = currentId(ctx);
    if (!found || started) return;
    started = true;
    cwd = (ctx && ctx.cwd) || cwd;
    emit({ hook_event_name: "SessionStart", session_id: found, cwd });
  }

  pi.on("session_start", async (_event, ctx) => {
    cwd = (ctx && ctx.cwd) || cwd;
    for (let attempt = 0; attempt < 50 && !currentId(ctx); attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    emitStart(ctx);
  });

  pi.on("input", (event, ctx) => {
    cwd = (ctx && ctx.cwd) || cwd;
    emitStart(ctx);
    const text = clean(event && event.text);
    const found = currentId(ctx);
    if (!text || !found) return;
    if (event && event.text !== text) {
      prompt = text;
      awaiting = true;
      failed = false;
      assistant = "";
      emit({ hook_event_name: "UserPromptSubmit", session_id: found, cwd, prompt: text });
      return { action: "transform", text };
    }
    prompt = text;
    awaiting = true;
    failed = false;
    assistant = "";
    emit({ hook_event_name: "UserPromptSubmit", session_id: found, cwd, prompt: text });
  });

  pi.on("message_end", (event) => {
    const text = assistantText(event && event.message);
    if (text) assistant = text;
  });

  pi.on("agent_before_settle", (event) => {
    failed = Boolean(event && event.outcome === "error");
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!awaiting) return;
    awaiting = false;
    cwd = (ctx && ctx.cwd) || cwd;
    const found = currentId(ctx);
    if (failed) {
      emit({
        hook_event_name: "StopFailure",
        session_id: found,
        cwd,
        prompt,
        last_assistant_message: assistant,
        error: "pi_error",
        error_details: assistant || "Pi agent run failed",
      });
      return;
    }
    emit({
      hook_event_name: "Stop",
      session_id: found,
      cwd,
      prompt,
      last_assistant_message: assistant,
    });
  });

  pi.on("session_shutdown", (_event, ctx) => {
    const found = currentId(ctx);
    if (!found) return;
    emit({
      hook_event_name: "SessionEnd",
      session_id: found,
      cwd: (ctx && ctx.cwd) || cwd,
    });
  });
}
