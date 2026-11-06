// node --test apps/web/test: the web app's pure parts (the chat's rules,
// Markdown, the key detector). The same cases as DesktopKit's tests where
// they overlap.
import assert from "node:assert/strict";
import { test } from "node:test";

import { findKey, isValidName, normalizedName } from "../public/assets/js/keys.js";
import { inline, parse, safeHref } from "../public/assets/js/markdown.js";
import { Timeline, rowKey } from "../public/assets/js/timeline.js";

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
