// node --test apps/web/test: the web app's pure parts (the chat's rules,
// Markdown, the key detector). The same cases as DesktopKit's tests where
// they overlap.
import assert from "node:assert/strict";
import { test } from "node:test";

import { findKey, isValidName, normalizedName } from "../public/assets/js/keys.js";
import { inline, parse, safeHref } from "../public/assets/js/markdown.js";
import { deviceLabel, fromBase64Url, iosOutsideHomeScreen, isIOS, ownSubscription, pushState, pushSupported } from "../public/assets/js/push.js";
import { ApiError, api } from "../public/assets/js/api.js";
import { Store } from "../public/assets/js/store.js";
import {
  KINDS, Timeline, andList, appStepWords, clickTarget, coveredWords, duration, megabytes, planAnswer, planCovers, planPhrases,
  planProblem, rowKey, stepNeeds, withoutNeed, workingFor,
} from "../public/assets/js/timeline.js";

let seq = 0;
const ev = (kind, data = {}) => ({ seq: ++seq, ts: "2026-11-03T08:00:00Z", kind, data });

test("the chat shows messages and waiting cards, never the agent's steps", () => {
  const t = new Timeline([
    ev("message", { role: "user", text: "Check Cloudflare", task: "t1" }),
    ev("task.started", { task: "t1" }),
    ev("step", { task: "t1", tool: "run_shell" }),
    ev("shell", { task: "t1", command: "curl …" }),
    ev("approval.requested", { id: "a1", task: "t1", kind: "shell", command: "rm -rf x", reason: "deletes files", summary: "Delete the old report" }),
    ev("task.paused", { task: "t1", approval: "a1" }),
  ]);
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "approval"]);
  assert.equal(t.busy, false, "parked: the desktop is free");
  t.apply(ev("approval.resolved", { id: "a1", approved: true }));
  t.apply(ev("task.resumed", { task: "t1" }));
  assert.equal(t.busy, true);
  t.apply(ev("message", { role: "assistant", text: "Done.", task: "t1" }));
  t.apply(ev("task.finished", { task: "t1", status: "done" }));
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "assistant"], "an answered card leaves the chat");
  assert.equal(t.busy, false);
});

test("card kinds keep their fields", () => {
  const t = new Timeline([
    ev("approval.requested", { id: "b", kind: "browser", action: "type", target: "Message", text: "hi", sends: true, site: "app.slack.com" }),
    ev("approval.requested", { id: "s", kind: "schedule", name: "Morning report", text: "Summarize", when: "Weekdays at 08:00" }),
    ev("approval.requested", { id: "k", kind: "secret", name: "CLOUDFLARE_API_TOKEN", label: "Cloudflare token", why: "to read events", how: "1. Go", url: "https://dash.cloudflare.com/profile/api-tokens", host: "dash.cloudflare.com" }),
    ev("action_needed", { id: "x", reason: "Sign in", page: "Login | https://x", image: "/api/v1/images/ab" }),
  ]);
  const [browser, schedule, secret, action] = t.visible();
  assert.equal(browser.browser.sends, true);
  assert.equal(browser.editable, false, "only a card that says it takes an edit offers one");
  const click = new Timeline([ev("approval.requested", {
    id: "c", kind: "browser", action: "click", target: "Confirm", role: "button", summary: "Pay the March invoice",
    page: "Invoices", url: "https://acme.com/invoices?month=march", destination: "https://acme.com/confirm",
    around: "March, $120 Confirm", image: "/api/v1/images/ab",
  })]).visible()[0];
  assert.equal(click.summary, "Pay the March invoice");
  assert.deepEqual(
    [click.browser.role, click.browser.page, click.browser.url, click.browser.destination, click.browser.around, click.browser.image],
    ["button", "Invoices", "https://acme.com/invoices?month=march", "https://acme.com/confirm", "March, $120 Confirm", "/api/v1/images/ab"]);
  assert.equal(new Timeline([ev("approval.requested", { id: "e", kind: "browser", action: "type", text: "hi", editable: true })]).visible()[0].editable, true);
  assert.equal(schedule.schedule.when, "Weekdays at 08:00");
  assert.equal(secret.secret.host, "dash.cloudflare.com");
  assert.equal(action.type, "action");
  t.apply(ev("secret.saved", { name: "CLOUDFLARE_API_TOKEN" }));
  assert.equal(t.visible()[2].secret.saved, true, "saved while busy: Continue");
  t.apply(ev("action.resolved", { id: "x", choice: "done" }));
  t.expireApproval("b");
  assert.deepEqual(t.visible().map((i) => i.id), ["s", "k"]);
});

test("a click in a native app is a card of its own, with what the app shows", () => {
  const card = new Timeline([ev("approval.requested", {
    task: "t1", id: "c9", kind: "app", action: "click", app: "Mousepad", window: "~/notes.txt - Mousepad",
    target: "Save", role: "menuitem", around: "menu “File” › menuitem “Save” — Save the current document",
    summary: "Save the shopping list", reason: "may change or send something in the app", image: "/api/v1/images/ab12",
    binding: { pid: 4242 }, tainted: true,
  })]).visible()[0];
  assert.equal(card.kind, "app", "not the shell card");
  assert.deepEqual(
    [card.summary, card.reason, card.tainted, card.editable, card.browser],
    ["Save the shopping list", "may change or send something in the app", true, false, undefined]);
  assert.deepEqual(card.app, {
    action: "click", app: "Mousepad", window: "~/notes.txt - Mousepad", target: "Save", role: "menuitem",
    text: null, key: null, around: "menu “File” › menuitem “Save” — Save the current document", image: "/api/v1/images/ab12",
    steps: [], frame: null,
  }, "an older card has no steps");
  const bare = new Timeline([ev("approval.requested", { id: "c10", kind: "app", action: "click", target: null, around: null, summary: null, image: null })]).visible()[0];
  assert.deepEqual([bare.app.target, bare.app.around, bare.app.image, bare.summary], ["", "", null, null]);
  assert.deepEqual([clickTarget("menuitem", "Save"), clickTarget("push button", ""), clickTarget("", "OK")],
    ["Menuitem “Save”", "Push button without a name", "“OK”"]);
});

test("a batch in a native app lists every step, with all the text it writes", () => {
  const card = new Timeline([ev("approval.requested", {
    task: "t1", id: "c11", kind: "app", action: "act", app: "Mousepad", window: "*notes.txt - Mousepad",
    summary: "Add milk and save", reason: "may change or send something in the app", image: "/api/v1/images/ab12",
    steps: [
      { action: "type", target: "", role: "textbox", around: "tablist › textbox", text: "- milk\n- eggs ✓" },
      { action: "key", key: "CTRL+S", window: "*notes.txt - Mousepad" },
      { action: "click", target: "Save", role: "menuitem", around: "menu “File” › menuitem “Save”", pointer: true },
      { action: "wait", for: "a window “Save As”", gone: true },
      "odd", null,
    ],
  })]).visible()[0];
  const a = card.app;
  assert.deepEqual([a.action, a.app, a.window, a.target, a.text, a.frame, card.summary],
    ["act", "Mousepad", "*notes.txt - Mousepad", "", null, null, "Add milk and save"], "a batch's fields stay on its steps");
  assert.equal(a.steps.length, 4, "what isn't a step is left out");
  assert.deepEqual(a.steps[0], {
    action: "type", target: "", role: "textbox", around: "tablist › textbox", text: "- milk\n- eggs ✓", key: null,
    window: "", pointer: false, range: false, direction: "", pages: null, for: "", gone: false, button: "", points: [],
  });
  assert.deepEqual([a.steps[1].action, a.steps[1].key, a.steps[1].window], ["key", "CTRL+S", "*notes.txt - Mousepad"]);
  assert.deepEqual([a.steps[2].pointer, a.steps[3].for, a.steps[3].gone], [true, "a window “Save As”", true]);
  const words = a.steps.map((s) => appStepWords(s, { window: a.window }));
  assert.deepEqual(words[0], {
    text: "Write in Textbox:", key: null, quote: "- milk\n- eggs ✓", note: "tablist › textbox", window: null, quiet: false,
  }, "the full text, its lines kept");
  assert.deepEqual([words[1].text, words[1].key, words[1].window], ["Press", "CTRL+S", null], "the card's own window isn't repeated");
  assert.deepEqual([words[2].text, words[2].note], ["Click Menuitem “Save” with the mouse", "menu “File” › menuitem “Save”"]);
  assert.deepEqual([words[3].text, words[3].quiet], ["Wait for a window “Save As” to go away", true]);
  const say = (step) => appStepWords({ action: "", target: "", role: "", around: "", text: null, points: [], ...step }).text;
  assert.deepEqual([
    say({ action: "set_value", role: "spinbutton", target: "Size", text: "42" }),
    say({ action: "select", role: "textbox", text: "Line one" }),
    say({ action: "select", role: "listitem", target: "Blue" }),
    say({ action: "select", role: "cell", target: "A1", range: true }),
    say({ action: "scroll", role: "list", target: "Files", direction: "down", pages: 2 }),
    say({ action: "activate", window: "Save As" }),
    say({ action: "close", window: "Untitled 1 - Mousepad" }),
  ], [
    "Set Spinbutton “Size” to:", "Select “Line one” in Textbox", "Select Listitem “Blue”", "Select from Cell “A1”…",
    "Scroll down 2 pages in List “Files”", "Switch to the window “Save As”", "Close the window “Untitled 1 - Mousepad”",
  ]);
});

test("a pixel batch keeps its points and the picture's size, for the marks", () => {
  const card = new Timeline([ev("approval.requested", {
    task: "t1", id: "c12", kind: "app", action: "gui", app: "Mousepad", window: "notes.txt - Mousepad",
    summary: "Open the File menu and drag the file", image: "/api/v1/images/cd34", frame: { width: 1229, height: 768 },
    steps: [
      { action: "click", x: 100, y: 120.4, button: "right", keys: ["CTRL"], key: "CTRL", window: "notes.txt - Mousepad", app: "Mousepad" },
      { action: "drag", path: [{ x: 10, y: 10 }, { x: "far", y: 5 }, { x: 200, y: 300 }], window: "Files", app: "Thunar" },
      { action: "keypress", keys: ["CTRL", "S"] },
      { action: "scroll", x: "400", y: 300, scroll_y: 3 },
      { action: "wait" },
    ],
  })]).visible()[0];
  const a = card.app;
  assert.deepEqual([a.action, a.frame, a.image], ["gui", { width: 1229, height: 768 }, "/api/v1/images/cd34"]);
  assert.deepEqual(a.steps.map((s) => s.points), [
    [{ x: 100, y: 120.4 }], [{ x: 10, y: 10 }, { x: 200, y: 300 }], [], [], [],
  ], "a point that isn't a number is dropped");
  assert.deepEqual([a.steps[0].button, a.steps[0].key, a.steps[2].key], ["right", "CTRL", "CTRL+S"]);
  const words = a.steps.map((s) => appStepWords(s, { pixels: true, window: a.window }));
  assert.deepEqual(words.map((w) => w.text), [
    "Click at 100, 120 (right button), holding CTRL", "Drag from 10, 10 to 200, 300", "Press", "Scroll", "Wait a second",
  ]);
  assert.deepEqual([words[0].window, words[1].window, words[2].key, words[4].quiet], [null, "In “Files”", "CTRL+S", true]);
  const odd = new Timeline([ev("approval.requested", {
    id: "c13", kind: "app", action: "gui", frame: { width: 0, height: "768" }, steps: { action: "click" },
  })]).visible()[0];
  assert.deepEqual([odd.app.frame, odd.app.steps], [null, []]);
});

test("an app install is a card of its own, with what the store says", () => {
  const card = new Timeline([ev("approval.requested", {
    task: "t1", id: "c7", kind: "install", source: "Snap Store", name: "slack", app: "Slack", publisher: "Slack",
    verified: true, version: "4.53.292", channel: "stable", confinement: "strict",
    summary: "Team communication for the 21st century.", size: 233066496,
    why: "So you can use Slack as a desktop app", reason: "installs an app on your desktop, as root", tainted: true,
  })]).visible()[0];
  assert.equal(card.kind, "install", "not the shell card");
  assert.deepEqual(
    [card.summary, card.reason, card.tainted, card.editable, card.app],
    [null, "installs an app on your desktop, as root", true, false, undefined], "the summary is the store's, not the agent's");
  assert.deepEqual(card.install, {
    source: "Snap Store", name: "slack", app: "Slack", publisher: "Slack", verified: true, version: "4.53.292",
    channel: "stable", confinement: "strict", summary: "Team communication for the 21st century.", size: 233066496,
    why: "So you can use Slack as a desktop app",
  });
  const bare = new Timeline([ev("approval.requested", { id: "c8", kind: "install", name: "slack", summary: "", size: null, why: null })]).visible()[0];
  assert.deepEqual(
    [bare.install.app, bare.install.verified, bare.install.summary, bare.install.size, bare.install.why, bare.tainted],
    ["", false, "", null, "", false]);
  assert.deepEqual([megabytes(233066496), megabytes(4_240_000), megabytes(4_000_000), megabytes(1200)], ["233 MB", "4.2 MB", "4 MB", "0.1 MB"]);
});

test("shell approval cards preserve the full command and its access details", () => {
  const command = "gh pr list\ngh issue list";
  const t = new Timeline([
    ev("approval.requested", { id: "key", kind: "shell", command, secrets: ["GITHUB_TOKEN"] }),
    ev("approval.requested", { id: "direct", kind: "shell", command: "git fetch", direct: true }),
  ]);
  const [key, direct] = t.visible();
  assert.equal(key.command, command);
  assert.deepEqual(key.savedKeys, ["GITHUB_TOKEN"]);
  assert.equal(key.direct, false);
  assert.equal(direct.direct, true);
  assert.deepEqual(direct.savedKeys, []);
});

test("scheduled runs are reports, and quiet runs of one schedule fold", () => {
  const run = (task, answer, quiet) => [
    ev("message", { role: "user", text: "Look for spikes", task, schedule: { id: 1, name: "Spike watch" } }),
    ev("message", { role: "assistant", text: answer, task, quiet }),
  ];
  const t = new Timeline([
    ...run("r1", "No spike.", true),
    ...run("r2", "No spike: 5.5k total.", true),
    ...run("r3", "No spike.", true),
    ev("message", { role: "user", text: "Thanks", task: "t9" }),
    ...run("r4", "A spike at 03:10:\n- 40k requests from one ASN", true),
  ]);
  const rows = t.visible();
  assert.deepEqual(rows.map((r) => r.type), ["report", "user", "report"]);
  assert.equal(rows[0].earlier.length, 2, "three quiet runs in one row");
  assert.equal(rowKey(rows[0]), `report-${rows[0].groupId}`);
  assert.equal(rows[2].answer.quiet, false, "a long answer isn't a quiet line");
});

test("a scheduled run beside the chat leaves the chat free, and its report lands when it ends", () => {
  const t = new Timeline([
    ev("message", { role: "user", text: "What's new?", task: "t1" }),
    ev("task.started", { task: "t1" }),
    ev("run.started", { task: "r1", schedule: { id: 1, name: "Spike watch" } }),
    ev("step", { task: "r1", tool: "run_shell" }),
  ]);
  assert.equal(t.busy, true, "the chat's own task");
  t.apply(ev("message", { role: "assistant", text: "Nothing yet.", task: "t1" }));
  t.apply(ev("task.finished", { task: "t1", status: "done" }));
  assert.equal(t.busy, false, "free while the run goes on");
  t.apply(ev("message", { role: "user", text: "Look for spikes", task: "r1", schedule: { id: 1, name: "Spike watch" } }));
  t.apply(ev("message", { role: "assistant", text: "A spike at 03:10.", task: "r1" }));
  t.apply(ev("run.finished", { task: "r1", status: "done" }));
  assert.deepEqual(t.visible().map((r) => r.type), ["user", "assistant", "report"]);
  assert.equal(t.busy, false);
});

test("a site to allow is a card with its hosts", () => {
  const t = new Timeline([
    ev("approval.requested", { id: "w", kind: "access", hosts: ["status.example.com"], why: "to read the incident timeline", tainted: true }),
  ]);
  const [card] = t.visible();
  assert.equal(card.kind, "access");
  assert.deepEqual(card.access, { hosts: ["status.example.com"], why: "to read the incident timeline" });
  assert.equal(card.tainted, true);
});

test("an answer whose report is on an older page still shows", () => {
  const t = new Timeline([ev("message", { role: "assistant", text: "Report", task: "r0" })]);
  assert.equal(t.visible()[0].type, "assistant");
});

test("Markdown: blocks", () => {
  const blocks = parse("# Findings\n\nTwo **high** issues:\n1. SQL error\n2. Open redirect\n\n| Path | Hits |\n|:--|--:|\n| /login | 4 |\n\n```bash\ncurl x\n```\n> quoted\n\n- [x] done\n- [ ] not yet\n\n---");
  assert.deepEqual(blocks.map((b) => b.type), ["h", "p", "ol", "table", "code", "quote", "ul", "hr"]);
  assert.equal(blocks[2].items.length, 2);
  assert.deepEqual(blocks[3].align, ["left", "right"]);
  assert.equal(blocks[4].lang, "bash");
  assert.deepEqual(blocks[6].items.map((i) => i.checked), [true, false]);
});

test("Markdown: single newlines break lines; raw HTML stays text", () => {
  const [p] = parse("line one\nline <b>two</b>");
  assert.deepEqual(p.content.map((n) => n.type), ["text", "br", "text"]);
  assert.equal(p.content[2].text, "line <b>two</b>");
});

test("Markdown: only web and mail links; images become links", () => {
  assert.equal(inline("[x](javascript:alert(1))")[0].type, "span");
  assert.equal(inline("[docs](https://developers.cloudflare.com)")[0].href, "https://developers.cloudflare.com");
  const image = inline("![chart](https://evil.example/pixel.png)")[0];
  assert.equal(image.type, "link");
  assert.equal(image.children[0].text, "chart");
  assert.equal(inline("see https://example.com/a.")[1].href, "https://example.com/a");
  assert.equal(safeHref("data:text/html,x"), false);
  assert.deepEqual(inline("`a*b*` and *c*").map((n) => n.type), ["code", "text", "em"]);
  assert.equal(inline("snake_case_name")[0].type, "text");
});

test("keys pasted into a message are spotted", () => {
  assert.equal(findKey("here: ghp_" + "a".repeat(36)).name, "GITHUB_TOKEN");
  assert.equal(findKey("tgp_v1_" + "b".repeat(30)).label, "Together AI API key");
  assert.equal(findKey("the sha is 3f2a9c"), null);
  assert.equal(normalizedName("Cloudflare api-token"), "CLOUDFLARE_API_TOKEN");
  assert.equal(isValidName("DD_API_KEY"), true);
  assert.equal(isValidName("MY_PASSWORD"), false);
});

test("a task's plan: after its message, the latest wins, folded once the task ends", () => {
  const steps = (...statuses) => statuses.map((status, i) => ({ step: `Step ${i + 1}`, status }));
  const t = new Timeline([
    ev("message", { role: "user", text: "Migrate the blog", task: "t1" }),
    ev("task.started", { task: "t1" }),
    ev("approval.requested", { id: "a1", task: "t1", kind: "shell", command: "npm i", reason: "installs" }),
    ev("plan", { task: "t1", items: steps("in_progress", "pending", "pending") }),
  ]);
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "plan", "approval"], "right after its message");
  t.apply(ev("plan", { task: "t1", items: steps("done", "done", "in_progress") }));
  const plan = t.visible()[1];
  assert.deepEqual(plan.steps.map((s) => s.status), ["done", "done", "in_progress"], "each event is the whole plan");
  assert.equal(plan.finished, false);
  assert.equal(rowKey(plan), "plan-t1");
  // Another task's plan doesn't touch this one.
  t.apply(ev("approval.resolved", { id: "a1", approved: true }));
  t.apply(ev("message", { role: "assistant", text: "Done.", task: "t1" }));
  t.apply(ev("task.finished", { task: "t1", status: "done" }));
  t.apply(ev("message", { role: "user", text: "Next", task: "t2" }));
  t.apply(ev("task.started", { task: "t2" }));
  t.apply(ev("plan", { task: "t2", items: steps("pending") }));
  const [, first, , , second] = t.visible();
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "plan", "assistant", "user", "plan"]);
  assert.equal(first.finished, true, "folded once its task ended");
  assert.equal(first.steps.length, 3);
  assert.equal(second.finished, false);
  // Bad statuses become pending; blank steps are dropped; no steps, no row.
  t.apply(ev("plan", { task: "t2", items: [{ step: "Build", status: "weird" }, { step: "  ", status: "done" }] }));
  assert.deepEqual(t.visible()[4].steps, [{ step: "Build", status: "pending" }]);
  t.apply(ev("plan", { task: "t2", items: [] }));
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "plan", "assistant", "user"]);
});

// The plan card agentd sends (docs: approval.requested, kind "plan").
const PLAN_CARD = {
  id: "a1b2c3d4e5f6", task: "t1", kind: "plan", editable: true, tainted: false,
  steps: [
    { step: "Open notes.txt in Mousepad", status: "pending", app: "Mousepad" },
    { step: "Write the list and save it", status: "pending", app: "Mousepad" },
    { step: "Post it in #general", status: "pending", site: "app.slack.com" },
    { step: "Push the branch", status: "pending", commands: ["git push -u origin fix"] },
    { step: "Install Slack", status: "pending", install: "slack" },
    { step: "Tell the user what changed", status: "pending" },
  ],
  covers: { sites: ["app.slack.com"], apps: ["Mousepad"], commands: ["git push -u origin fix"], installs: ["slack"] },
};

test("a plan card: its steps with what each needs, and what approving covers", () => {
  const t = new Timeline([
    ev("message", { role: "user", text: "Tidy up and tell Slack", task: "t1" }),
    ev("approval.requested", PLAN_CARD),
    ev("task.paused", { task: "t1", approval: PLAN_CARD.id }),
  ]);
  const card = t.visible()[1];
  assert.deepEqual([card.type, card.kind, card.editable, card.tainted, card.command], ["approval", "plan", true, false, ""]);
  assert.deepEqual(card.plan.steps, PLAN_CARD.steps, "a need is there only when the step has it");
  assert.deepEqual(card.plan.covers, PLAN_CARD.covers);
  assert.deepEqual(stepNeeds(card.plan.steps[3]), [{ kind: "command", value: "git push -u origin fix" }]);
  assert.deepEqual(stepNeeds(card.plan.steps[5]), []);
  // The summary, as the card says it.
  const said = planPhrases(planCovers(card.plan.covers, card.plan.steps))
    .map((p) => p.lead + andList(p.items, (item) => (p.code ? `\`${item}\`` : item)).join("")).join(" · ");
  assert.equal(said, "clicking, typing and pressing keys on app.slack.com · input in Mousepad · running `git push -u origin fix` · installing slack");
  assert.deepEqual([andList(["a"]), andList(["a", "b"]).join(""), andList(["a", "b", "c"]).join("")], [["a"], "a and b", "a, b and c"]);

  // Odd fields are left out; missing lists are empty; repeats go.
  const odd = new Timeline([ev("approval.requested", {
    id: "p2", kind: "plan", steps: [{ step: "Go", status: "weird", site: "", app: 7, commands: ["ls", "", 3], install: null }, { step: " " }, "x"],
    covers: { sites: ["a.com", "a.com"], apps: null },
  })]).visible()[0];
  assert.deepEqual(odd.plan.steps, [{ step: "Go", status: "pending", commands: ["ls"] }]);
  assert.deepEqual(odd.plan.covers, { sites: ["a.com"], apps: [], commands: [], installs: [] });
  assert.equal(odd.editable, false, "only a card that says it takes an edit offers one");

  t.apply(ev("approval.resolved", { task: "t1", id: PLAN_CARD.id, approved: true, by: "someone@example.com", edited: true, plan: [] }));
  assert.deepEqual(t.visible().map((i) => i.type), ["user"], "answered, it leaves the chat");
});

test("the plan checklist keeps each step's needs", () => {
  const t = new Timeline([ev("plan", { task: "t1", items: PLAN_CARD.steps.map((s, i) => ({ ...s, status: i < 2 ? "done" : s.status })) })]);
  const [plan] = t.visible();
  assert.deepEqual(plan.steps[2], { step: "Post it in #general", status: "pending", site: "app.slack.com" });
  assert.deepEqual(plan.steps[0].status, "done");
  assert.deepEqual(plan.steps[5], { step: "Tell the user what changed", status: "pending" });
});

test("a plan card's edits: needs only come off, the summary follows, and only a change is sent", () => {
  const original = new Timeline([ev("approval.requested", PLAN_CARD)]).visible()[0].plan.steps;
  const covers = PLAN_CARD.covers;
  assert.equal(planAnswer(original, original), undefined, "as the agent wrote it: nothing to send");
  assert.equal(planAnswer(original, original.map((s) => ({ ...s, step: `${s.step}  ` }))), undefined, "spaces around aren't a change");

  // Take Mousepad off one step: still on the other, so still covered.
  let mine = original.map((s, i) => (i === 0 ? withoutNeed(s, 0) : s));
  assert.deepEqual(mine[0], { step: "Open notes.txt in Mousepad", status: "pending" });
  assert.deepEqual(planCovers(covers, mine).apps, ["Mousepad"]);
  mine = mine.map((s, i) => (i === 1 ? withoutNeed(s, 0) : s));
  assert.deepEqual(planCovers(covers, mine), { sites: ["app.slack.com"], apps: [], commands: ["git push -u origin fix"], installs: ["slack"] });

  // One command at a time.
  const two = { step: "Ship", status: "pending", commands: ["git push", "npm publish"], site: "github.com" };
  assert.deepEqual(stepNeeds(two).map((n) => n.kind), ["site", "command", "command"]);
  assert.deepEqual(withoutNeed(two, 1), { step: "Ship", status: "pending", site: "github.com", commands: ["npm publish"] });
  assert.deepEqual(withoutNeed(withoutNeed(two, 1), 1), { step: "Ship", status: "pending", site: "github.com" });

  // Reword, delete, add, reorder.
  mine = [
    { ...mine[3], step: "Push the fix branch " }, mine[2], mine[4],
    { step: "Check CI", status: "pending" },
  ];
  assert.deepEqual(planAnswer(original, mine), [
    { step: "Push the fix branch", status: "pending", commands: ["git push -u origin fix"] },
    { step: "Post it in #general", status: "pending", site: "app.slack.com" },
    { step: "Install Slack", status: "pending", install: "slack" },
    { step: "Check CI", status: "pending" },
  ], "the whole plan, its words trimmed");
  assert.deepEqual(planAnswer(original, [...original].reverse()).length, 6, "a new order is a change");

  // Nothing left to cover: approving only starts the plan.
  const bare = mine.map((s) => ({ step: s.step, status: s.status }));
  assert.deepEqual(planPhrases(planCovers(covers, bare)), []);
  // A later card covers only what's new, though its steps are the whole plan.
  assert.deepEqual(planPhrases(planCovers({ sites: [], apps: [], commands: [], installs: ["slack"] }, original)),
    [{ lead: "installing ", items: ["slack"] }]);

  // What agentd would refuse can't be approved.
  assert.equal(planProblem(original), "");
  assert.match(planProblem([]), /needs a step/);
  assert.match(planProblem([...original, { step: "  ", status: "pending" }]), /empty step/);
  assert.match(planProblem(Array.from({ length: 21 }, () => ({ step: "x", status: "pending" }))), /at most 20/);
});

test("an action the approved plan covered is a quiet line, with what it did", () => {
  assert.ok(KINDS.includes("approval.plan"), "the app asks for it");
  const covered = (card) => ev("approval.plan", { task: "t1", approval: "a1b2c3d4e5f6", by: "someone@example.com", card });
  const t = new Timeline([
    ev("message", { role: "user", text: "Tidy up", task: "t1" }),
    covered({ kind: "app", action: "act", app: "Mousepad", window: "notes.txt - Mousepad", summary: "Add the milk line and save",
      steps: [{ action: "type", text: "- milk" }, { action: "key", key: "CTRL+S" }] }),
    covered({ kind: "browser", action: "type", site: "app.slack.com", target: "Message", text: "Done: notes are tidy", summary: "" }),
    covered({ kind: "browser", action: "click", site: "app.slack.com", role: "button", target: "Send", summary: "Send it" }),
    covered({ kind: "shell", command: "git push -u origin fix", summary: "" }),
    covered({ kind: "shell", command: "git push -u origin fix", summary: "Push the branch" }),
    covered({ kind: "install", name: "slack", app: "Slack" }),
    covered(null),
  ]);
  const rows = t.visible();
  assert.deepEqual(rows.map((r) => r.type), ["user", ...Array(7).fill("covered")]);
  assert.equal(rowKey(rows[1]), `covered-${rows[1].id}`);
  assert.deepEqual([rows[1].task, rows[1].approval, rows[1].by], ["t1", "a1b2c3d4e5f6", "someone@example.com"]);
  assert.deepEqual(rows[1].app.steps.map((s) => [s.action, s.text, s.key]), [["type", "- milk", null], ["key", null, "CTRL+S"]]);
  assert.deepEqual(rows.slice(1).map(coveredWords), [
    { where: "Mousepad", what: "Add the milk line and save", code: false, more: true },
    { where: "app.slack.com", what: "Write in “Message”", code: false, more: true },
    { where: "app.slack.com", what: "Send it", code: false, more: true },
    { where: "", what: "git push -u origin fix", code: true, more: false },
    { where: "", what: "Push the branch", code: false, more: true },
    { where: "", what: "Install Slack", code: false, more: true },
    { where: "", what: "An action", code: false, more: false },
  ]);
});

test("answering a plan card sends the plan only when edited, and a refused edit says why", async (t) => {
  // The request body, through the real client.
  const bodies = [];
  let reply = { status: 200, body: { ok: true } };
  t.mock.method(globalThis, "fetch", async (url, init) => {
    bodies.push([url, JSON.parse(init.body)]);
    return new Response(JSON.stringify(reply.body), { status: reply.status });
  });
  const plan = [{ step: "Push the branch", status: "pending", commands: ["git push -u origin fix"] }, { step: "Check CI", status: "pending" }];
  await api.answer("a1b2c3d4e5f6", true, undefined, undefined);
  await api.answer("a1b2c3d4e5f6", true, undefined, plan);
  await api.answer("a1b2c3d4e5f6", false);
  assert.deepEqual(bodies, [
    ["/api/v1/approvals/a1b2c3d4e5f6", { approved: true }],
    ["/api/v1/approvals/a1b2c3d4e5f6", { approved: true, plan }],
    ["/api/v1/approvals/a1b2c3d4e5f6", { approved: false }],
  ]);

  // agentd refuses an edit: its words come back to the card, not as a notice.
  const store = new Store(api);
  const notices = [];
  store.onNotice((text) => notices.push(text));
  const card = { id: "a1b2c3d4e5f6" };
  reply = { status: 400, body: { error: "step 2 adds a need the plan didn't have" } };
  assert.equal(await store.answerPlan(card, true, plan), "step 2 adds a need the plan didn't have");
  assert.deepEqual(notices, []);
  reply = { status: 200, body: { ok: true } };
  assert.equal(await store.answerPlan(card, true, plan), null);
  // Gone: expired, as any card.
  reply = { status: 404, body: { error: "not found" } };
  assert.equal(await store.answerPlan(card, true), null);
  assert.deepEqual(notices, ["That request has expired."]);
  assert.ok(store.conversation.expiredApprovals.has(card.id));
});

test("a plan whose message is on an older page still shows", () => {
  const t = new Timeline([
    ev("task.resumed", { task: "t9" }),
    ev("plan", { task: "t9", items: [{ step: "Deploy", status: "in_progress" }] }),
  ]);
  assert.deepEqual(t.visible().map((i) => i.type), ["plan"]);
});

test("the stuck card: shown with its facts, gone once answered", () => {
  const t = new Timeline([
    ev("message", { role: "user", text: "Fix the build", task: "t1" }),
    ev("task.started", { task: "t1" }),
    ev("approval.requested", {
      id: "s1", task: "t1", kind: "stuck", reason: "It has run the same failing build 9 times.",
      minutes: 34, steps: 120, plan: { done: 2, total: 5 },
    }),
    ev("task.paused", { task: "t1", approval: "s1" }),
  ]);
  const card = t.visible()[1];
  assert.equal(card.kind, "stuck");
  assert.deepEqual(card.stuck, { reason: "It has run the same failing build 9 times.", minutes: 34, steps: 120, plan: { done: 2, total: 5 } });
  assert.equal(t.busy, false, "parked on it: the desktop is free");
  t.apply(ev("approval.resolved", { id: "s1", approved: false }));
  t.apply(ev("task.resumed", { task: "t1" }));
  t.apply(ev("message", { role: "assistant", text: "Done so far: …", task: "t1" }));
  t.apply(ev("task.finished", { task: "t1", status: "stuck" }));
  assert.deepEqual(t.visible().map((i) => i.type), ["user", "assistant"]);
  // No plan: none on the card.
  const bare = new Timeline([ev("approval.requested", { id: "s2", kind: "stuck", reason: "x", minutes: 0, steps: 60, plan: null })]);
  assert.equal(bare.visible()[0].stuck.plan, null);
});

test("how long the task has worked", () => {
  assert.equal(duration(0.5), "");
  assert.equal(duration(1), "1 min");
  assert.equal(duration(24.9), "24 min");
  assert.equal(duration(65), "1 h 5 min");
  assert.equal(duration(120), "2 h");
  const start = "2026-11-03T08:00:00Z";
  assert.equal(workingFor(start, Date.parse("2026-11-03T08:00:40Z")), "");
  assert.equal(workingFor(start, Date.parse("2026-11-03T08:24:10Z")), "24 min");
  assert.equal(workingFor(null), "");
  const t = new Timeline([{ seq: 1, ts: start, kind: "task.started", data: { task: "t1" } }]);
  assert.equal(t.workingSince, start);
  t.apply({ seq: 2, ts: "2026-11-03T08:05:00Z", kind: "task.paused", data: { task: "t1" } });
  assert.equal(t.workingSince, null, "waiting on you: not working");
  t.apply({ seq: 3, ts: "2026-11-03T08:09:00Z", kind: "task.resumed", data: { task: "t1" } });
  assert.equal(t.workingSince, start, "counts from when it started");
  t.apply({ seq: 4, ts: "2026-11-03T08:10:00Z", kind: "task.finished", data: { task: "t1" } });
  assert.equal(t.workingSince, null);
});

test("push: the desktop's key becomes bytes for the browser", () => {
  // 65 bytes, uncompressed P-256 (0x04 first), base64url without padding.
  const bytes = new Uint8Array(65).map((_, i) => (i === 0 ? 4 : i * 3));
  const b64url = Buffer.from(bytes).toString("base64url");
  assert.ok(!b64url.includes("=") && /^[A-Za-z0-9_-]+$/.test(b64url));
  assert.deepEqual([...fromBase64Url(b64url)], [...bytes]);
  assert.deepEqual([...fromBase64Url("_-8")], [0xff, 0xef]);
});

test("push: a device's label from its browser", () => {
  const cases = [
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36", 0, "Mac · Chrome"],
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15", 0, "Mac · Safari"],
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15", 5, "iPad · Safari"],
    ["Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1", 5, "iPhone · Safari"],
    ["Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0", 0, "Windows · Edge"],
    ["Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0", 0, "Linux · Firefox"],
    ["Mozilla/5.0 (Linux; Android 16; Pixel 10) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36", 5, "Android · Chrome"],
    ["", 0, "This device"],
  ];
  for (const [ua, touch, label] of cases) assert.equal(deviceLabel(ua, touch), label, ua);
  assert.ok(isIOS("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", 5) && !isIOS("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", 0));
});

test("push: what the browser can do, and what Settings shows", () => {
  const iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1";
  const win = (nav, extra = {}, standaloneQuery = false) => ({
    navigator: nav, matchMedia: () => ({ matches: standaloneQuery }), ...extra,
  });
  assert.ok(pushSupported(win({ serviceWorker: {} }, { PushManager: {}, Notification: {} })));
  assert.ok(!pushSupported(win({ serviceWorker: {} }, { Notification: {} })));
  assert.ok(!pushSupported(undefined));
  // Safari on iPhone, in a tab: only a Home Screen app gets notifications.
  assert.ok(iosOutsideHomeScreen(win({ userAgent: iphone, maxTouchPoints: 5 })));
  assert.ok(!iosOutsideHomeScreen(win({ userAgent: iphone, maxTouchPoints: 5, standalone: true })));
  assert.ok(!iosOutsideHomeScreen(win({ userAgent: iphone, maxTouchPoints: 5 }, {}, true)));
  assert.ok(!iosOutsideHomeScreen(win({ userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", maxTouchPoints: 0 })));

  assert.equal(pushState({ ios: true, supported: false }), "ios-home-screen");
  assert.equal(pushState({ supported: false }), "unsupported");
  assert.equal(pushState({ supported: true, permission: "denied", subscribed: true }), "denied");
  assert.equal(pushState({ supported: true, permission: "granted", subscribed: true }), "on");
  assert.equal(pushState({ supported: true, permission: "default", subscribed: false }), "off");

  const subs = [{ id: "a1", endpoint: "https://fcm.googleapis.com/fcm/send/x" }, { id: "b2", endpoint: "https://web.push.apple.com/y" }];
  assert.equal(ownSubscription(subs, "https://web.push.apple.com/y").id, "b2");
  assert.equal(ownSubscription(subs, "https://elsewhere/z"), null);
  assert.equal(ownSubscription(subs, undefined), null);
});

test("Auto approve confirms the desktop's current revision and reports what it has", async () => {
  // A stand-in desktop: turning it on needs the revision it has now.
  let server = { auto_approve: false, auto_approve_version: "false:b" };
  let reachable = true;
  const sent = [];
  const api = {
    async setAutoApprove(enabled, version) {
      sent.push([enabled, version]);
      if (!reachable) throw new ApiError(0, "Can't reach your desktop.");
      if (enabled && version !== server.auto_approve_version) throw new ApiError(409, "Auto approve changed.");
      server = { auto_approve: enabled, auto_approve_version: `${enabled}:${sent.length}` };
      return server;
    },
    async status() {
      if (!reachable) throw new ApiError(0, "Can't reach your desktop.");
      return { settings: { ...server } };
    },
  };
  const store = new Store(api);
  // What the page was drawn from, before another device changed it.
  store.status = { settings: { auto_approve: false, auto_approve_version: "initial" } };

  let result = await store.setAutoApprove(true);
  assert.equal(result.error.status, 409);
  assert.equal(result.on, false, "a refused change shows as off");
  // The status was read again, so the next try confirms the current revision.
  result = await store.setAutoApprove(true);
  assert.deepEqual([result.on, result.error], [true, null]);
  assert.deepEqual(sent.map(([, version]) => version), ["initial", "false:b"]);
  // Off, then on again from the same page: no stale revision.
  assert.equal((await store.setAutoApprove(false)).on, false);
  assert.equal((await store.setAutoApprove(true)).on, true);
  assert.equal(sent.at(-1)[1], "false:3", "the revision the turn-off left");

  // Unreachable: the setting is assumed unchanged.
  reachable = false;
  result = await store.setAutoApprove(false);
  assert.equal(result.on, true);
  assert.equal(result.error.status, 0);
});
