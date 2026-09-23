// dlgt lifecycle bridge for the OpenCode TUI.
// Inert unless this process is a dlgt child (DLGT_OPENCODE_LAUNCH).
import { spawnSync } from "node:child_process";

const launch = process.env.DLGT_OPENCODE_LAUNCH || "";
const bin = process.env.DLGT_OPENCODE_HOOK_BIN || "";
// The TUI worker does not see CLI argv. dlgt passes the provider id here
// because a resumed session publishes no session.* event until the next prompt.
const resumeId = (process.env.DLGT_OPENCODE_RESUME || "").trim();

function emit(payload) {
  if (!launch || !bin) return;
  spawnSync(bin, ["hook", "emit", launch, "opencode"], {
    input: JSON.stringify(payload),
    encoding: "utf8",
  });
}

function clean(text) {
  return String(text || "")
    .replace(/\u001b\[200~/g, "")
    .replace(/\u001b\[201~/g, "");
}

function partText(part) {
  if (!part || part.type !== "text") return "";
  if (typeof part.text === "string") return part.text;
  if (typeof part.content === "string") return part.content;
  return "";
}

function textFromParts(parts) {
  if (!Array.isArray(parts)) return "";
  return clean(parts.map(partText).join(""));
}

function sessionIdFrom(event) {
  const properties = event && event.properties ? event.properties : {};
  const info = properties.info || {};
  const part = properties.part || {};
  return properties.sessionID || info.sessionID || info.id || part.sessionID || "";
}

export const DlgtPlugin = async ({ client, directory }) => {
  if (!launch || !bin) return {};
  let bound = "";
  let awaiting = false;
  let prompt = "";
  let assistant = "";
  // Report the process cwd dlgt launched, not OpenCode's project directory.
  // A resumed session can belong to another directory than the launch cwd.
  const cwd = process.cwd() || directory || "";

  function bind(id) {
    if (!id || bound) return;
    bound = id;
    emit({ hook_event_name: "SessionStart", session_id: id, cwd });
  }

  if (resumeId) {
    // Defer until this factory returns so plugin startup is not blocked on
    // the daemon round-trip. Readiness still arrives before the TUI is idle.
    setTimeout(() => bind(resumeId), 0);
  }

  function notePrompt(sessionID, text) {
    const promptText = clean(text);
    if (!promptText) return;
    bind(sessionID);
    prompt = promptText;
    awaiting = true;
    emit({
      hook_event_name: "UserPromptSubmit",
      session_id: bound || sessionID,
      cwd,
      prompt: promptText,
    });
  }

  async function assistantText(sessionID) {
    if (assistant || !client || !client.session || !sessionID) return assistant;
    try {
      const result = await client.session.messages({ path: { id: sessionID } });
      const messages = (result && (result.data || result)) || [];
      const list = Array.isArray(messages) ? messages : [];
      for (let index = list.length - 1; index >= 0; index -= 1) {
        const item = list[index] || {};
        const info = item.info || item;
        if (info.role === "assistant") {
          const text = textFromParts(item.parts || info.parts);
          if (text) return text;
        }
      }
    } catch {
      // The message stream is a fallback. Completion still reports the text
      // captured from message.part.updated.
    }
    return assistant;
  }

  return {
    event: async ({ event }) => {
      if (!event || !event.type) return;
      const id = sessionIdFrom(event);
      if (
        event.type === "session.created" ||
        event.type === "session.updated" ||
        event.type === "session.status"
      ) {
        bind(id);
      }
      if (event.type === "message.updated") {
        const info = (event.properties && event.properties.info) || {};
        if (info.role === "user") {
          notePrompt(info.sessionID || id, textFromParts(event.properties.parts || info.parts));
        }
      }
      if (event.type === "message.part.updated") {
        const part = event.properties && event.properties.part;
        const text = partText(part);
        if (text && awaiting) assistant = clean(text);
      }
      if (event.type === "session.idle") {
        if (!awaiting) return;
        awaiting = false;
        const sessionID = id || bound;
        emit({
          hook_event_name: "Stop",
          session_id: sessionID,
          cwd,
          prompt,
          last_assistant_message: await assistantText(sessionID),
        });
      }
      if (event.type === "session.error") {
        if (!awaiting) return;
        awaiting = false;
        const error = (event.properties && event.properties.error) || {};
        const message =
          (error.data && error.data.message) || error.message || error.name || "OpenCode session error";
        emit({
          hook_event_name: "StopFailure",
          session_id: id || bound,
          cwd,
          prompt,
          last_assistant_message: assistant,
          error: "opencode_error",
          error_details: String(message),
        });
      }
    },
    "chat.message": async (input, output) => {
      notePrompt(input && input.sessionID, textFromParts(output && output.parts));
    },
  };
};
