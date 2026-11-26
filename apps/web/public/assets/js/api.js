// agentd's API (docs/architecture.md §6.4), through tailnet-proxy on the same
// host, which knows who's calling from their Tailscale identity.

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(method, path, body) {
  const init = { method, headers: { Accept: "application/json" }, credentials: "same-origin" };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch {
    // Offline, or this device dropped off the tailnet.
    throw new ApiError(0, "Can't reach your desktop. Check your connection and that Tailscale is on.");
  }
  const text = await response.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!response.ok) {
    throw new ApiError(response.status, data?.error || text.trim() || `Your desktop answered ${response.status}.`);
  }
  return data;
}

const enc = encodeURIComponent;

/** A message, with where the person is (for "tomorrow at 8"). */
function withTime(text) {
  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return { text, timezone, utc_offset: -new Date().getTimezoneOffset() * 60 };
}

export const api = {
  status: () => request("GET", "/api/v1/status"),
  session: () => request("GET", "/session"),
  // One conversation's events (and the global ones) with `session`; without
  // it, everything (a desktop from before conversations has only that).
  page: (before, limit, kinds, session) =>
    request("GET", `/api/v1/events/page?limit=${limit}&kinds=${enc(kinds.join(","))}${before ? `&before=${before}` : ""}${session ? `&session=${enc(session)}` : ""}`),
  // The old endpoints: main's, on any desktop.
  send: (text) => request("POST", "/api/v1/messages", withTime(text)),
  stop: () => request("POST", "/api/v1/stop", {}),
  // `text`, `plan`: the person's version of the card's text, or of a plan
  // card's steps; each left out (undefined) unless they changed it. `note`:
  // what they added to a plan card in their own words (agentd 0.21.0).
  answer: (id, approved, text, plan, forTask, note) =>
    request("POST", `/api/v1/approvals/${enc(id)}`, { approved, text, plan, for_task: forTask || undefined, note: note || undefined }),
  resolve: (id, done) => request("POST", `/api/v1/actions/${enc(id)}`, { choice: done ? "done" : "skip" }),
  // Conversations (agentd 0.16): 404 on an older desktop.
  sessions: (archived) => request("GET", `/api/v1/sessions${archived ? "?archived=1" : ""}`),
  getSession: (id) => request("GET", `/api/v1/sessions/${enc(id)}`),
  createSession: () => request("POST", "/api/v1/sessions", {}),
  updateSession: (id, change) => request("PATCH", `/api/v1/sessions/${enc(id)}`, change),
  deleteSession: (id) => request("DELETE", `/api/v1/sessions/${enc(id)}`, {}),
  sendTo: (session, text) => request("POST", `/api/v1/sessions/${enc(session)}/messages`, withTime(text)),
  unqueue: (session, id) => request("DELETE", `/api/v1/sessions/${enc(session)}/queue/${enc(id)}`, {}),
  stopSession: (session) => request("POST", `/api/v1/sessions/${enc(session)}/stop`, {}),
  usage: (days = 30) => request("GET", `/api/v1/usage?days=${days}`),
  settings: () => request("GET", "/api/v1/settings"),
  updateSettings: (settings) => request("PUT", "/api/v1/settings", settings),
  setAutoApprove: (enabled, version) => version === undefined && !enabled
    ? request("PUT", "/api/v1/settings", { auto_approve: false })
    : request("PUT", "/api/v1/settings/auto-approve", { enabled, expected_version: version }),
  schedules: async () => (await request("GET", "/api/v1/schedules")).schedules,
  deleteSchedule: (id) => request("DELETE", `/api/v1/schedules/${enc(id)}`, {}),
  pauseSchedule: (id, paused) => request("POST", `/api/v1/schedules/${enc(id)}/${paused ? "pause" : "resume"}`, {}),
  // The model providers' keys (agentd 0.20.0): the value goes once, never back.
  saveModelKey: (provider, value) => request("PUT", `/api/v1/model-keys/${enc(provider)}`, { value }),
  deleteModelKey: (provider) => request("DELETE", `/api/v1/model-keys/${enc(provider)}`, {}),
  secrets: async () => (await request("GET", "/api/v1/secrets")).secrets,
  saveSecret: (name, value, label, approval) =>
    request("PUT", `/api/v1/secrets/${enc(name)}`, { value, label, ...(approval ? { approval } : {}) }),
  deleteSecret: (name) => request("DELETE", `/api/v1/secrets/${enc(name)}`, {}),
  // Notifications (Web Push): the desktop's key, and the devices it notifies.
  push: () => request("GET", "/api/v1/push"),
  subscribePush: (subscription) => request("POST", "/api/v1/push/subscriptions", subscription),
  deletePush: (id) => request("DELETE", `/api/v1/push/subscriptions/${enc(id)}`, {}),
  deleteDevice: (id) => request("DELETE", `/api/v1/devices/${enc(id)}`, {}),
  testPush: (id) => request("POST", "/api/v1/push/test", id ? { id } : {}),
  access: () => request("GET", "/api/v1/access"),
  allowSite: (host) => request("POST", "/api/v1/access", { host }),
  removeSite: (host) => request("DELETE", `/api/v1/access/${enc(host)}`, {}),
};

/**
 * Streams events after `after` (Server-Sent Events over fetch, so the app
 * chooses where to resume). Calls onEvent for each; resolves when the stream
 * ends, rejects on an error or 60 s without even a keep-alive (agentd sends
 * one every 15 s).
 */
export async function stream(after, kinds, onEvent, signal) {
  const idle = new AbortController();
  const abort = () => idle.abort();
  signal?.addEventListener("abort", abort, { once: true });
  let timer = setTimeout(abort, 60_000);
  try {
    const response = await fetch(`/api/v1/events?after=${after}&kinds=${enc(kinds.join(","))}`, {
      headers: { Accept: "text/event-stream" },
      credentials: "same-origin",
      signal: idle.signal,
    });
    if (!response.ok || !response.body) throw new ApiError(response.status, `The event stream answered ${response.status}.`);
    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      clearTimeout(timer);
      timer = setTimeout(abort, 60_000);
      buffer += value;
      const lines = buffer.split("\n");
      buffer = lines.pop();
      for (const line of lines) {
        if (!line.startsWith("data:")) continue; // comments (keep-alives), id:, event:
        const json = line.slice(5).trim();
        if (json) onEvent(JSON.parse(json));
      }
    }
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", abort);
  }
}
