// Conversations (docs/conversations-and-usage.md): which one an event is
// about, what the list shows for each, the routes, and the one this device
// used last. Pure (localStorage aside, which may be missing), so node can
// test it.

/** The first conversation: everything from before there were others. */
export const MAIN = "main";

// The kinds about a conversation (§3.3). agentd puts a `session` on them; an
// older desktop (or the old log) doesn't, and then they're main's.
const CONVERSATION_KINDS = new Set([
  "message", "message.queued", "message.unqueued", "step", "plan", "review", "shell", "shell.output",
  "action_needed", "action.resolved", "image",
]);
const CONVERSATION_PREFIXES = ["task.", "run.", "approval."];

/**
 * The conversation an event is about, or null for a global one (settings, a
 * key saved, a budget alert…), which applies to every conversation.
 */
export function sessionOf(event) {
  const d = event?.data || {};
  const id = event?.session ?? d.session;
  if (typeof id === "string" && id) return id;
  const kind = String(event?.kind || "");
  // memory.* and schedule.* are a conversation's only when a task made them.
  if (CONVERSATION_KINDS.has(kind) || CONVERSATION_PREFIXES.some((p) => kind.startsWith(p)) || d.task != null) return MAIN;
  return null;
}

/**
 * Events after which the list is fetched again: its states, queues and last
 * messages change with them. Not an action a plan covered: it changes none.
 */
export function refetchesSessions(kind) {
  return /^(session\.|message|task\.|approval\.(?!plan$)|action|budget\.alert$)/.test(String(kind || ""));
}

export function sessionTitle(session) {
  return String(session?.title || "").trim() || "New conversation";
}

/** The newest message's first line, "You: …" for the person's own. */
export function lastLine(session) {
  const last = session?.last;
  const line = String(last?.text || "").trim().split("\n")[0].trim();
  if (!line) return "";
  return last.role === "user" ? `You: ${line}` : line;
}

/** Whether it waits on the person: a card, or Action needed. */
export function needsYou(session) {
  return session?.state === "waiting" || Boolean(session?.needs_you);
}

/** The list's badge: working › needs you › paused (budget) › queued; null when idle. */
export function sessionBadge(session) {
  if (!session) return null;
  if (session.state === "working") return { tone: "working", text: "Working" };
  if (needsYou(session)) return { tone: "needs", text: "Needs you" };
  if (session.state === "paused") return { tone: "paused", text: "Paused" };
  const queued = Array.isArray(session.queued) ? session.queued.length : 0;
  if (session.state === "queued" || queued) return { tone: "queued", text: `${Math.max(queued, 1)} queued` };
  return null;
}

/**
 * "#/chat/<id>" → {view: "chat", id}; "#/chat" alone is main (older
 * notifications link there); no route at all is {view: "chat", id: null}:
 * the conversation used last.
 */
export function parseRoute(hash) {
  const path = String(hash || "").replace(/^#?\/?/, "");
  if (!path) return { view: "chat", id: null };
  const [view, ...rest] = path.split("/");
  if (view !== "chat") return { view, id: null };
  let id = rest.join("/");
  try {
    id = decodeURIComponent(id);
  } catch {
    id = "";
  }
  return { view, id: id || MAIN };
}

export function chatHash(id) {
  return !id || id === MAIN ? "#/chat" : `#/chat/${encodeURIComponent(id)}`;
}

// The conversation this device opened last (per device, so not on the desktop).
const LAST = "conversation";

export function rememberConversation(id) {
  try {
    localStorage.setItem(LAST, id);
  } catch {}
}

export function lastConversation() {
  try {
    return localStorage.getItem(LAST) || MAIN;
  } catch {
    return MAIN;
  }
}
