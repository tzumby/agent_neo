// node --test apps/web/test: conversations (routing events by `session`, the
// list, the routes), queued messages, an older desktop without them, and the
// Usage page's figures (docs/conversations-and-usage.md).
import assert from "node:assert/strict";
import { test } from "node:test";

import { ApiError } from "../public/assets/js/api.js";
import {
  MAIN, chatHash, lastConversation, lastLine, parseRoute, refetchesSessions, rememberConversation, sessionBadge, sessionOf, sessionTitle,
} from "../public/assets/js/conversations.js";
import { Store } from "../public/assets/js/store.js";
import { Timeline } from "../public/assets/js/timeline.js";
import { axisMoney, budgetShare, chartDays, kindRows, modelName, money, niceCeiling, todayIn } from "../public/assets/js/usage.js";

let seq = 0;
const ev = (kind, data = {}) => ({ seq: ++seq, ts: "2026-11-24T08:00:00Z", kind, data });
const texts = (timeline) => timeline.visible().map((item) => item.text ?? item.type);

/**
 * A stand-in desktop: pages per conversation, the list, and what was asked
 * of it. `older` answers like agentd before 0.16 (no /sessions).
 */
function desktop({ older = false, pages = {}, sessions = [] } = {}) {
  const calls = [];
  const api = {
    calls,
    list: [...sessions],
    async status() {
      return {};
    },
    async sessions(archived) {
      calls.push(["sessions", archived ?? false]);
      if (older) throw new ApiError(404, "Not found");
      return { sessions: api.list.filter((s) => s.archived === (archived === true)), running_limit: 3 };
    },
    async getSession(id) {
      calls.push(["getSession", id]);
      const found = api.list.find((s) => s.id === id);
      if (!found) throw new ApiError(404, "No such conversation");
      return found;
    },
    async page(before, limit, kinds, session) {
      calls.push(["page", session ?? null, before ?? null]);
      const page = pages[session ?? "*"] || { events: [], more: false };
      return { last_seq: seq, ...page };
    },
    async send(text) {
      calls.push(["send", text]);
      return { task: "t-old" };
    },
    async sendTo(session, text) {
      calls.push(["sendTo", session, text]);
      return api.nextSend || { task: "t1" };
    },
    async stop() {
      calls.push(["stop"]);
      return { stopped: true };
    },
    async stopSession(session) {
      calls.push(["stopSession", session]);
      return { stopped: true, dropped: 2 };
    },
    async unqueue(session, id) {
      calls.push(["unqueue", session, id]);
      if (api.gone?.has(id)) throw new ApiError(404, "Not queued");
    },
  };
  return api;
}

/** A store as run() leaves it once connected, without the stream (the test feeds events in). */
async function connected(api, open = MAIN) {
  const store = new Store(api);
  await store.loadSessions();
  store.ready = true;
  store.open(open);
  await store.ensureLoaded(store.conversation);
  return store;
}

test("an event's conversation: its session, else main for conversation kinds, else global", () => {
  assert.equal(sessionOf(ev("message", { role: "user", text: "hi", session: "c1" })), "c1");
  assert.equal(sessionOf({ ...ev("plan", { task: "t1" }), session: "c2" }), "c2", "the stream's own field");
  for (const kind of ["message", "message.queued", "message.unqueued", "task.started", "run.finished", "approval.requested", "approval.plan", "action_needed", "action.resolved", "image", "plan"]) {
    assert.equal(sessionOf(ev(kind, {})), MAIN, `${kind} without a session is main's`);
  }
  assert.equal(sessionOf(ev("memory.saved", { task: "t1" })), MAIN, "saved during a task");
  for (const kind of ["secret.saved", "settings", "budget.alert", "schedule.created", "memory.forgotten", "session.updated"]) {
    assert.equal(sessionOf(ev(kind, { id: "x" })), null, `${kind} is global`);
  }
});

test("the list is fetched again on the kinds that change it, not on others", () => {
  for (const kind of ["session.created", "session.deleted", "message", "message.queued", "message.unqueued", "task.finished", "approval.resolved", "action_needed", "action.resolved", "budget.alert"]) {
    assert.ok(refetchesSessions(kind), kind);
  }
  // An action a plan covered changes nothing the list shows, and comes one per action.
  for (const kind of ["settings", "plan", "secret.saved", "image", "step", "approval.plan"]) assert.ok(!refetchesSessions(kind), kind);
});

test("routes: #/chat is main, #/chat/<id> its conversation, nothing at all the last one", () => {
  assert.deepEqual(parseRoute("#/chat"), { view: "chat", id: MAIN });
  assert.deepEqual(parseRoute("#/chat/"), { view: "chat", id: MAIN });
  assert.deepEqual(parseRoute("#/chat/s%201"), { view: "chat", id: "s 1" });
  assert.deepEqual(parseRoute("#/chat/%E0%A4%A"), { view: "chat", id: MAIN }, "a broken link opens main");
  assert.deepEqual(parseRoute(""), { view: "chat", id: null });
  assert.deepEqual(parseRoute("#/"), { view: "chat", id: null });
  assert.deepEqual(parseRoute("#/chats"), { view: "chats", id: null });
  assert.deepEqual(parseRoute("#/usage"), { view: "usage", id: null });
  assert.equal(chatHash(MAIN), "#/chat");
  assert.equal(chatHash("s 1"), "#/chat/s%201");
  assert.equal(parseRoute(chatHash("q/x")).id, "q/x");
});

test("the last conversation is remembered per device, and storage that fails is no problem", () => {
  assert.equal(lastConversation(), MAIN, "no storage at all: main");
  rememberConversation("c9"); // no storage: nothing thrown
  const kept = new Map();
  globalThis.localStorage = { getItem: (k) => kept.get(k) ?? null, setItem: (k, v) => kept.set(k, String(v)) };
  try {
    assert.equal(lastConversation(), MAIN, "the first time: main");
    rememberConversation("c9");
    assert.equal(lastConversation(), "c9");
    globalThis.localStorage = { getItem() { throw new Error("blocked"); }, setItem() { throw new Error("blocked"); } };
    rememberConversation("c10");
    assert.equal(lastConversation(), MAIN);
  } finally {
    delete globalThis.localStorage;
  }
});

test("the list: titles, last lines and badges", () => {
  assert.equal(sessionTitle({ title: "" }), "New conversation");
  assert.equal(sessionTitle({ title: "  Plan the trip " }), "Plan the trip");
  assert.equal(lastLine({ last: { role: "user", text: "Find flights\nto Lisbon" } }), "You: Find flights");
  assert.equal(lastLine({ last: { role: "assistant", text: "Done." } }), "Done.");
  assert.equal(lastLine({ last: null }), "");
  assert.equal(sessionBadge({ state: "working", needs_you: null, queued: [{ id: "q" }] }).tone, "working", "a spinner wins");
  assert.equal(sessionBadge({ state: "waiting", needs_you: "approval", queued: [] }).text, "Needs you");
  assert.equal(sessionBadge({ state: "idle", needs_you: "action", queued: [] }).text, "Needs you", "Action needed, its card still open");
  assert.equal(sessionBadge({ state: "paused", needs_you: null, queued: [{ id: "q" }] }).text, "Paused");
  assert.equal(sessionBadge({ state: "queued", needs_you: null, queued: [{ id: "a" }, { id: "b" }] }).text, "2 queued");
  assert.equal(sessionBadge({ state: "idle", needs_you: null, queued: [] }), null);
});

test("queued bubbles: in order with their places, gone when started, removed or stopped", () => {
  const t = new Timeline([
    ev("message", { role: "user", text: "First", task: "t1" }),
    ev("task.started", { task: "t1" }),
    ev("message.queued", { id: "q1", text: "Second", by: "you@example.com", created: 100, position: 1 }),
    ev("message.queued", { id: "q2", text: "Third", created: 101, position: 2 }),
    ev("message.queued", { id: "q3", text: "Fourth", created: 102, position: 3 }),
  ]);
  assert.deepEqual(t.queue().map((q) => [q.text, q.position, q.of]), [["Second", 1, 3], ["Third", 2, 3], ["Fourth", 3, 3]]);
  assert.deepEqual(texts(t), ["First"], "queued messages aren't the chat's rows: they show after it");
  // The task ends; the next one starts, and its message follows.
  t.apply(ev("task.finished", { task: "t1" }));
  t.apply(ev("message.unqueued", { id: "q1", reason: "started", task: "t2" }));
  t.apply(ev("message", { role: "user", text: "Second", task: "t2" }));
  t.apply(ev("task.started", { task: "t2" }));
  assert.deepEqual(t.queue().map((q) => [q.text, q.position, q.of]), [["Third", 1, 2], ["Fourth", 2, 2]]);
  assert.deepEqual(texts(t), ["First", "Second"]);
  t.apply(ev("message.unqueued", { id: "q3", reason: "removed" }));
  assert.deepEqual(t.queue().map((q) => q.text), ["Third"]);
  assert.equal(t.queue()[0].of, 1, "one left: no place to show");
  t.apply(ev("task.stopping", { task: "t2" }));
  t.apply(ev("message.unqueued", { id: "q2", reason: "stopped" }));
  assert.deepEqual(t.queue(), []);
  // A later page with an older copy of the list doesn't bring them back.
  assert.deepEqual(t.queue([{ id: "q2", text: "Third", created: 101 }, { id: "q9", text: "Earlier", created: 90 }]).map((q) => q.id), ["q9"],
    "listed by the desktop but not in the loaded events: shown, oldest first");
});

test("a queued message from Send shows at once, once, and its ✕ takes it back", async () => {
  const api = desktop({ pages: { [MAIN]: { events: [ev("task.started", { task: "t1", session: MAIN })], more: false } } });
  const store = await connected(api);
  assert.equal(store.busy, true);
  api.nextSend = { queued: { id: "q1", position: 1 } };
  assert.equal(await store.send("After this"), true);
  assert.deepEqual(api.calls.at(-1), ["sendTo", MAIN, "After this"]);
  assert.deepEqual(store.queue().map((q) => [q.id, q.text]), [["q1", "After this"]], "before the event");
  store.receive(ev("message.queued", { session: MAIN, id: "q1", text: "After this", created: Date.now() / 1000, position: 1 }));
  assert.equal(store.queue().length, 1, "the event doesn't add a second bubble");
  api.nextSend = { queued: { id: "q2", position: 2 } };
  await store.send("And this");
  assert.deepEqual(store.queue().map((q) => `${q.position} of ${q.of}`), ["1 of 2", "2 of 2"]);

  assert.equal(await store.unqueue(store.queue()[1]), true);
  assert.deepEqual(api.calls.at(-1), ["unqueue", MAIN, "q2"]);
  assert.deepEqual(store.queue().map((q) => q.id), ["q1"], "gone before agentd's event");
  store.receive(ev("message.unqueued", { session: MAIN, id: "q2", reason: "removed" }));
  // Already started: the ✕ finds it gone, and so does the chat.
  api.gone = new Set(["q1"]);
  const notices = [];
  store.onNotice((text) => notices.push(text));
  assert.equal(await store.unqueue(store.queue()[0]), true);
  assert.deepEqual(store.queue(), []);
  assert.equal(notices.length, 1);
  // A page loaded again (older events) keeps it gone.
  store.conversation.rebuild();
  assert.deepEqual(store.queue(), []);
});

test("Stop drops the conversation's queued messages too", async () => {
  const api = desktop({
    pages: {
      c1: {
        events: [
          ev("task.started", { task: "t1", session: "c1" }),
          ev("message.queued", { session: "c1", id: "q1", text: "a", created: 1 }),
          ev("message.queued", { session: "c1", id: "q2", text: "b", created: 2 }),
        ],
      },
    },
    sessions: [{ id: "c1", archived: false, state: "working", queued: [] }],
  });
  const store = await connected(api, "c1");
  assert.equal(store.queue().length, 2);
  await store.stop();
  assert.deepEqual(api.calls.at(-1), ["stopSession", "c1"]);
  assert.deepEqual(store.queue(), [], "dropped at once, before the events");
});

test("one stream, routed by session: each conversation gets its own, global events reach all", async () => {
  seq = 1000;
  const api = desktop({
    pages: {
      [MAIN]: { events: [ev("message", { role: "user", text: "In main", task: "t1", session: MAIN })] },
      c1: {
        events: [
          ev("message", { role: "user", text: "In c1", task: "t2", session: "c1" }),
          ev("approval.requested", { id: "k1", kind: "secret", name: "GITHUB_TOKEN", label: "GitHub token", session: "c1" }),
        ],
      },
    },
    sessions: [{ id: MAIN, archived: false, state: "idle", queued: [] }, { id: "c1", archived: false, state: "waiting", queued: [] }],
  });
  const store = await connected(api);
  assert.equal(store.sessionsSupported, true);
  assert.deepEqual(api.calls.filter((c) => c[0] === "page"), [["page", MAIN, null]], "main's page, by session");
  // c1 isn't open: its events wait for its first page.
  store.receive(ev("message", { role: "assistant", text: "c1 answer before it's opened", task: "t2", session: "c1" }));
  assert.equal(store.conversations.has("c1"), false);
  store.receive(ev("message", { role: "assistant", text: "Main's answer", task: "t1", session: MAIN }));
  store.receive(ev("message", { role: "assistant", text: "No session: main's", task: "t1" }));
  assert.deepEqual(texts(store.timeline), ["In main", "Main's answer", "No session: main's"]);

  // Opened: its page by session, and what streams in while it loads lands after it.
  store.open("c1");
  assert.equal(store.currentId, "c1");
  const loading = store.conversation.loading;
  store.receive(ev("message", { role: "assistant", text: "Streamed while loading", task: "t2", session: "c1" }));
  await loading;
  assert.deepEqual(api.calls.filter((c) => c[0] === "page").at(-1), ["page", "c1", null]);
  assert.deepEqual(texts(store.timeline), ["In c1", "approval", "Streamed while loading"]);
  // A key saved (global): the request card in c1 shows it saved.
  store.receive(ev("secret.saved", { name: "GITHUB_TOKEN" }));
  assert.equal(store.timeline.visible()[1].secret.saved, true);
  // Main kept up meanwhile, and didn't get c1's.
  store.receive(ev("message", { role: "assistant", text: "Main again", task: "t1", session: MAIN }));
  store.open(MAIN);
  assert.deepEqual(texts(store.timeline), ["In main", "Main's answer", "No session: main's", "Main again"]);
  // The stream's position is the whole log's; an event seen twice is applied once.
  const again = { ...ev("message", { role: "assistant", text: "Twice", session: MAIN }) };
  store.receive(again);
  store.receive(again);
  assert.equal(texts(store.timeline).filter((t) => t === "Twice").length, 1);
});

test("busy, and so the Stop button, is per conversation", async () => {
  const api = desktop({
    pages: { [MAIN]: { events: [] }, c1: { events: [] } },
    sessions: [{ id: MAIN, archived: false }, { id: "c1", archived: false }],
  });
  const store = await connected(api, "c1");
  store.open(MAIN);
  await store.ensureLoaded(store.conversation);
  store.receive(ev("task.started", { task: "t9", session: "c1" }));
  assert.equal(store.busy, false, "main is free while c1 works");
  store.open("c1");
  assert.equal(store.busy, true);
  await store.stop();
  assert.deepEqual(api.calls.at(-1), ["stopSession", "c1"]);
});

test("the list: fetched once per burst of events, and a deleted conversation on screen gives way to main", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const api = desktop({
    pages: { [MAIN]: { events: [] }, c1: { events: [] } },
    sessions: [{ id: MAIN, archived: false, state: "idle", queued: [] }, { id: "c1", archived: false, state: "idle", queued: [] }],
  });
  const store = await connected(api, "c1");
  const before = api.calls.filter((c) => c[0] === "sessions").length;
  store.receive(ev("message.queued", { session: "c1", id: "q1", text: "x", created: 1 }));
  store.receive(ev("task.started", { session: "c1", task: "t1" }));
  store.receive(ev("plan", { session: "c1", task: "t1", items: [] }));
  t.mock.timers.tick(299);
  assert.equal(api.calls.filter((c) => c[0] === "sessions").length, before, "not yet");
  t.mock.timers.tick(1);
  assert.equal(api.calls.filter((c) => c[0] === "sessions").length, before + 1, "once for the burst");
  await Promise.resolve();

  const notices = [];
  store.onNotice((text) => notices.push(text));
  store.receive(ev("session.deleted", { id: "c1" }));
  assert.equal(store.currentId, MAIN);
  assert.deepEqual(notices, ["That conversation was deleted."]);
  assert.ok(!store.sessions.some((s) => s.id === "c1"));
});

test("a link to a conversation that's gone opens main", async () => {
  const api = desktop({ pages: { [MAIN]: { events: [] } }, sessions: [{ id: MAIN, archived: false }] });
  const store = await connected(api, "gone");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(store.currentId, MAIN);
  // An archived one is looked up, and is the one on screen.
  api.list.push({ id: "old", archived: true, title: "Old trip" });
  store.open("old");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(store.currentId, "old");
  assert.equal(store.currentSession.archived, true);
});

test("a desktop from before conversations: one conversation, main, through the old endpoints", async () => {
  seq = 2000;
  const api = desktop({
    older: true,
    pages: { "*": { events: [ev("message", { role: "user", text: "Old log", task: "t1" }), ev("task.started", { task: "t1" })] } },
  });
  const store = await connected(api, "c1");
  assert.equal(store.sessionsSupported, false, "GET /sessions was a 404");
  assert.equal(store.currentId, MAIN, "a link to another conversation opens main");
  assert.deepEqual(api.calls.filter((c) => c[0] === "page"), [["page", null, null]], "the whole log, as before");
  assert.deepEqual(texts(store.timeline), ["Old log"]);
  store.open("c2");
  assert.equal(store.currentId, MAIN);
  // Its events have no session: all of them are main's.
  store.receive(ev("message", { role: "assistant", text: "Old answer", task: "t1" }));
  store.receive(ev("secret.saved", { name: "X" }));
  assert.deepEqual(texts(store.timeline), ["Old log", "Old answer"]);
  await store.send("Hello");
  await store.stop();
  assert.deepEqual(api.calls.slice(-2), [["send", "Hello"], ["stop"]]);
  // No list to keep up.
  const asked = api.calls.filter((c) => c[0] === "sessions").length;
  store.refreshSessions();
  assert.equal(store.sessionsTimer, null);
  assert.equal(api.calls.filter((c) => c[0] === "sessions").length, asked);
  assert.deepEqual(store.queue(), []);
});

test("usage: the chart has every day, zeros included, ending on the newest", () => {
  const days = chartDays([
    { date: "2026-11-22", cost: 0.42, runs: 7 },
    { date: "2026-11-24", cost: 1.5, runs: 3 },
    { date: "2026-11-24", cost: 0.5, runs: 1 },
    { date: "not a day", cost: 9, runs: 9 },
    null,
  ], { count: 5, today: "2026-11-30" });
  assert.deepEqual(days, [
    { date: "2026-11-20", cost: 0, runs: 0 },
    { date: "2026-11-21", cost: 0, runs: 0 },
    { date: "2026-11-22", cost: 0.42, runs: 7 },
    { date: "2026-11-23", cost: 0, runs: 0 },
    { date: "2026-11-24", cost: 2, runs: 4 },
  ], "the desktop's newest day ends it, not this device's");
  // Across a month (and a DST change), from nothing listed: today and the days before it.
  const empty = chartDays([], { count: 30, today: "2026-11-05" });
  assert.equal(empty.length, 30);
  assert.equal(empty[0].date, "2026-10-07");
  assert.equal(empty.at(-1).date, "2026-11-05");
  assert.ok(empty.every((d) => d.cost === 0 && d.runs === 0));
  assert.equal(new Set(empty.map((d) => d.date)).size, 30, "no day twice, none missing");
  assert.deepEqual(chartDays(undefined, {}), [], "nothing to go by");
  assert.equal(todayIn("Asia/Tokyo", new Date("2026-11-24T20:00:00Z")), "2026-11-25");
  assert.equal(todayIn("America/Toronto", new Date("2026-11-24T03:00:00Z")), "2026-11-23");
  assert.match(todayIn("Not/AZone", new Date("2026-11-24T12:00:00Z")), /^\d{4}-\d{2}-\d{2}$/);
});

test("usage: the scale, money and the budget", () => {
  assert.deepEqual([0, 0.42, 1, 1.01, 3.2, 7, 24, 180].map(niceCeiling), [1, 0.5, 1, 2, 5, 10, 25, 200]);
  assert.deepEqual([0, 0.25, 2.5, 10].map(axisMoney), ["$0", "$0.25", "$2.50", "$10"]);
  assert.equal(money(41.2), "$41.20");
  assert.equal(money(0.001), "<$0.01");
  assert.equal(money(0), "$0.00");
  assert.equal(money(undefined), "$0.00");
  assert.equal(budgetShare({ monthly_usd: null, spent: 3 }), null);
  const near = budgetShare({ monthly_usd: 50, spent: 41.2 });
  assert.deepEqual([near.share.toFixed(3), near.fill.toFixed(3), near.percent, near.tone], ["0.824", "0.824", 82, "near"]);
  assert.equal(budgetShare({ monthly_usd: 50, spent: 10 }).tone, "ok");
  const over = budgetShare({ monthly_usd: 50, spent: 61, paused: true });
  assert.deepEqual([over.fill, over.percent, over.tone], [1, 122, "over"]);
  assert.deepEqual(kindRows({ chat: 30.1, scheduled: 9.8, tidy_up: 1.3, other: 0 }).map((k) => k.name),
    ["Chat", "Scheduled runs", "Memory tidy-up", "other"]);
});

test("models have names for people", () => {
  assert.equal(modelName("claude-opus-4-6"), "Claude Opus 4.6");
  assert.equal(modelName("claude-opus-5-5"), "Claude Opus 5.5");
  assert.equal(modelName("claude-haiku-5-5"), "Claude Haiku 5.5");
  assert.equal(modelName("zai-org/GLM-5.3"), "GLM-5.3");
  assert.equal(modelName(""), "");
});
