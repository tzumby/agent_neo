// agentd's API (docs/architecture.md §6.4), through front-door on the same
// host: Cloudflare Access's session cookie goes along by itself.

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
    // Offline, or Cloudflare sent the request to its sign-in page.
    throw new ApiError(0, "Can't reach your desktop. Check your connection, or reload to sign in again.");
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

export const api = {
  status: () => request("GET", "/api/v1/status"),
  session: () => request("GET", "/session"),
  page: (before, limit, kinds) =>
    request("GET", `/api/v1/events/page?limit=${limit}&kinds=${enc(kinds.join(","))}${before ? `&before=${before}` : ""}`),
  send(text) {
    const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    return request("POST", "/api/v1/messages", { text, timezone, utc_offset: -new Date().getTimezoneOffset() * 60 });
  },
  answer: (id, approved) => request("POST", `/api/v1/approvals/${enc(id)}`, { approved }),
  resolve: (id, done) => request("POST", `/api/v1/actions/${enc(id)}`, { choice: done ? "done" : "skip" }),
  stop: () => request("POST", "/api/v1/stop", {}),
  settings: () => request("GET", "/api/v1/settings"),
  updateSettings: (settings) => request("PUT", "/api/v1/settings", settings),
  schedules: async () => (await request("GET", "/api/v1/schedules")).schedules,
  addSchedule: (schedule) => request("POST", "/api/v1/schedules", schedule),
  setup: () => request("GET", "/api/v1/setup"),
  saveSetup: (skipped) => request("PUT", "/api/v1/setup", { skipped }),
  deleteSchedule: (id) => request("DELETE", `/api/v1/schedules/${enc(id)}`, {}),
  pauseSchedule: (id, paused) => request("POST", `/api/v1/schedules/${enc(id)}/${paused ? "pause" : "resume"}`, {}),
  secrets: async () => (await request("GET", "/api/v1/secrets")).secrets,
  saveSecret: (name, value, label, approval) =>
    request("PUT", `/api/v1/secrets/${enc(name)}`, { value, label, ...(approval ? { approval } : {}) }),
  deleteSecret: (name) => request("DELETE", `/api/v1/secrets/${enc(name)}`, {}),
  destinations: async () => (await request("GET", "/api/v1/destinations")).destinations,
  saveDestination: (name, body) => request("PUT", `/api/v1/destinations/${enc(name)}`, body),
  deleteDestination: (name) => request("DELETE", `/api/v1/destinations/${enc(name)}`, {}),
  testDestination: (name) => request("POST", `/api/v1/destinations/${enc(name)}/test`, {}),
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
