// contextdb-hook-plugin: ContextDB hooks for OpenCode 2.0 (ADR 0014).
// Written by `ctx hooks install --harness opencode`; reinstalling overwrites
// this file. Every event goes to `ctx-hook opencode <event>` as the
// Claude-shaped stdin it already parses, and its Claude-shaped answer is
// mapped back: a deny throws (OpenCode fails the tool call with the reason),
// a warning is appended to that tool's result, and the briefing and per-turn
// context are added to the system prompt of each model request.
import { spawn } from "node:child_process";

const CTX_HOOK = __CTX_HOOK__;
/// ctx-hook bounds its own daemon calls; this only guards a stuck process.
const KILL_AFTER_MS = 10_000;

/// Runs one hook event; resolves to its parsed stdout object, or null.
function hook(event, input) {
  return new Promise((resolve) => {
    let out = "";
    let child;
    try {
      child = spawn(CTX_HOOK, ["opencode", event], { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      resolve(null);
      return;
    }
    const timer = setTimeout(() => child.kill("SIGKILL"), KILL_AFTER_MS);
    child.stdout.on("data", (d) => (out += d));
    child.on("error", () => { clearTimeout(timer); resolve(null); });
    child.on("close", () => {
      clearTimeout(timer);
      try { resolve(out.trim() ? JSON.parse(out) : null); } catch { resolve(null); }
    });
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(input));
  });
}
/// What a Claude-shaped answer says to the model.
const said = (out) => [out?.hookSpecificOutput?.additionalContext, out?.systemMessage]
  .filter((s) => typeof s === "string" && s).join("\n");

export default {
  id: "contextdb",
  async setup(ctx) {
    const cwd = ctx.location.directory;
    /// Per session: the briefing, this turn's context, and notes for the
    /// next turn (a drift report from the end of the last one).
    const sessions = new Map();
    const state = (id) => {
      if (!sessions.has(id)) sessions.set(id, { started: false, briefing: "", turn: [], next: [] });
      return sessions.get(id);
    };
    /// Notes for one tool call, appended to its result.
    const notes = new Map();
    const base = (sessionID, event) => ({ session_id: sessionID, cwd, hook_event_name: event });
    const start = async (id) => {
      const s = state(id);
      if (s.started) return s;
      s.started = true;
      s.briefing = said(await hook("session-start", { ...base(id, "SessionStart"), source: "startup" }));
      return s;
    };

    ctx.session.hook("prompt", async (e) => {
      const id = e.sessionID;
      if (!id) return;
      const s = await start(id);
      const update = said(await hook("user-prompt-submit", {
        ...base(id, "UserPromptSubmit"),
        prompt: e.prompt?.text ?? "",
      }));
      s.turn = [...s.next, update].filter(Boolean);
      s.next = [];
    });
    const context = async (e) => {
      const id = e.sessionID;
      if (!id || !Array.isArray(e.system)) return;
      const s = await start(id);
      for (const text of [s.briefing, ...s.turn]) {
        if (text) e.system.push({ type: "text", text });
      }
    };
    ctx.session.hook("context", context);
    ctx.session.hook("compaction", async (e) => {
      await context(e);
      if (!e.sessionID) return;
      const note = said(await hook("pre-compact", { ...base(e.sessionID, "PreCompact"), trigger: "auto" }));
      if (note) state(e.sessionID).next.push(note);
    });

    ctx.tool.hook("execute.before", async (e) => {
      const out = await hook("pre-tool-use", {
        ...base(e.sessionID, "PreToolUse"),
        tool_name: e.tool,
        tool_input: e.input ?? {},
        tool_use_id: e.id,
      });
      const specific = out?.hookSpecificOutput;
      if (specific?.permissionDecision === "deny") {
        throw new Error(specific.permissionDecisionReason || "ContextDB denied this action");
      }
      const note = said(out);
      if (note) notes.set(e.id, note);
    });
    ctx.tool.hook("execute.after", async (e) => {
      // Recorded asynchronously, as Claude Code runs PostToolUse.
      hook("post-tool-use", {
        ...base(e.sessionID, "PostToolUse"),
        tool_name: e.tool,
        tool_input: e.input ?? {},
        tool_use_id: e.id,
      });
      const note = notes.get(e.id);
      notes.delete(e.id);
      if (note && e.status === "completed" && Array.isArray(e.result?.content)) {
        e.result.content.push({ type: "text", text: note });
      }
    });

    const controller = new AbortController();
    (async () => {
      try {
        for await (const ev of ctx.event.subscribe({ signal: controller.signal })) {
          const id = ev?.data?.sessionID;
          if (!id) continue;
          if (ev.type === "session.execution.succeeded" || ev.type === "session.execution.failed") {
            const note = said(await hook("stop", base(id, "Stop")));
            if (note) state(id).next.push(note);
          } else if (ev.type === "session.deleted") {
            sessions.delete(id);
            await hook("session-end", { ...base(id, "SessionEnd"), reason: "other" });
          }
        }
      } catch {
        // Unsubscribed on unload.
      }
    })();
    return () => controller.abort();
  },
};
