// The chat: one conversation, scheduled runs as reports, the cards waiting
// for the person, a task's plan as a checklist, and a quiet line for each
// action an approved plan let go ahead without its card. Three dots while the agent
// works, with how long it has been at it; nothing else of what it does; then
// the messages queued behind it. Opens at its end; older pages load as the
// person scrolls up, without moving what's on screen. Another conversation
// starts over, at its end.

import { findKey } from "./keys.js";
import { modelName } from "./usage.js";
import { keyLabel, modelKeyForm, savedWords } from "./modelkeys.js";
import { parse, render, safeHref } from "./markdown.js";
import { busyButton, fill, h, icon, time, when } from "./dom.js";
import {
  PLAN_MAX_STEPS, PLAN_STEP_LENGTH, andList, appStepWords, clickTarget, coveredWords, duration, megabytes,
  planAnswer, planCovers, planPhrases, planProblem, rowKey, stepNeeds, withoutNeed, workingFor,
} from "./timeline.js";

export function mountChat(store, view, { openDesktop, openKeys }) {
  const scroll = view.querySelector(".chat-scroll");
  const list = view.querySelector(".rows");
  const queue = view.querySelector(".queue");
  const older = view.querySelector(".older");
  const working = view.querySelector(".working");
  const workingTime = view.querySelector(".working-time");
  const empty = view.querySelector(".empty");
  const splash = view.querySelector(".splash");
  const rows = new Map(); // row key → {sig, node}
  let queueSig = "";
  let shown = null; // the conversation on screen
  let revealed = false;
  let splashTimer;
  // At the end of the chat: new rows, and rows that grow (a picture, the
  // fonts), keep it there. Scrolled up to read, it stays put.
  let pinned = true;

  const ctx = { store, openDesktop };

  function nearBottom() {
    return scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 80;
  }

  // The fonts change every row's height: the chat appears once they're in,
  // so it opens at its end without a visible jump.
  let fontsReady = false;
  document.fonts.ready.then(() => {
    fontsReady = true;
    renderRows();
  });

  /** Another conversation: its rows from scratch, opening at its end once it's loaded. */
  function switchTo(id) {
    shown = id;
    for (const entry of rows.values()) entry.node.remove();
    rows.clear();
    queue.replaceChildren();
    queueSig = "";
    revealed = false;
    pinned = true;
    clearTimeout(splashTimer);
    splash.hidden = false;
    view.classList.remove("is-ready");
  }

  function renderRows() {
    if (store.currentId !== shown) switchTo(store.currentId);
    if (!store.loaded || !fontsReady) return;
    const items = store.timeline.visible();
    const queued = store.queue();
    const stick = !revealed || pinned;
    // The first row on screen keeps its place when rows go in above it.
    const top = scroll.getBoundingClientRect().top;
    const anchor = [...list.children].find((row) => row.getBoundingClientRect().bottom > top);
    const anchorTop = anchor?.getBoundingClientRect().top;

    const seen = new Set();
    items.forEach((item, index) => {
      const key = rowKey(item);
      seen.add(key);
      const sig = JSON.stringify(item);
      let entry = rows.get(key);
      if (!entry || entry.sig !== sig) {
        const node = renderRow(item, ctx);
        node.dataset.key = key;
        if (entry) entry.node.replaceWith(node);
        entry = { sig, node };
        rows.set(key, entry);
      }
      if (list.children[index] !== entry.node) list.insertBefore(entry.node, list.children[index] || null);
    });
    for (const [key, entry] of rows) {
      if (!seen.has(key)) {
        entry.node.remove();
        rows.delete(key);
        if (key.startsWith("approval-")) planDrafts.delete(key.slice("approval-".length)); // answered
      }
    }

    renderQueue(queued);
    working.hidden = !store.busy;
    updateWorking();
    // Suggestions only where they can be sent: not in an archived conversation.
    empty.hidden = items.length > 0 || queued.length > 0 || store.busy || store.currentSession?.archived === true;
    older.hidden = !store.hasOlder;

    // An empty chat holds the suggestions: it reads from its top.
    if (!items.length) scroll.scrollTop = 0;
    else if (stick) scroll.scrollTop = scroll.scrollHeight;
    else if (anchor?.isConnected) scroll.scrollTop += anchor.getBoundingClientRect().top - anchorTop;
    if (!revealed) {
      revealed = true;
      view.classList.add("is-ready"); // only the splash fades; the chat is already at its end
      splashTimer = setTimeout(() => (splash.hidden = true), 300);
      checkOlder();
    }
  }

  /** The queued messages, after the dots: each waits its turn, and its ✕ takes it back. */
  function renderQueue(queued) {
    const sig = JSON.stringify(queued);
    if (sig === queueSig) return;
    queueSig = sig;
    queue.replaceChildren(...queued.map((item) => queuedRow(item, store)));
    queue.hidden = !queued.length;
  }

  // Older pages: when the top of the chat comes into view.
  let olderVisible = false;
  async function checkOlder() {
    while (olderVisible && store.hasOlder && revealed) {
      older.classList.add("is-loading");
      const got = await store.loadOlder();
      older.classList.remove("is-loading");
      if (!got) break;
      await new Promise(requestAnimationFrame);
    }
  }
  new IntersectionObserver((entries) => {
    olderVisible = entries.some((e) => e.isIntersecting);
    checkOlder();
  }, { root: scroll, rootMargin: "200px 0px 0px 0px" }).observe(older);

  /** "Working · 24 min" beside the dots (nothing under a minute). */
  function updateWorking() {
    const since = store.busy && !store.stopping ? workingFor(store.timeline.workingSince) : "";
    workingTime.textContent = since ? `Working · ${since}` : "";
    workingTime.hidden = !since;
    working.setAttribute("aria-label", store.stopping ? "Stopping" : since ? `Working for ${since}` : "Working");
  }
  setInterval(() => {
    if (store.busy) updateWorking();
  }, 30_000);

  scroll.addEventListener("scroll", () => (pinned = nearBottom()), { passive: true });
  const keepAtEnd = new ResizeObserver(() => {
    if (pinned && revealed && (list.children.length || queue.children.length)) scroll.scrollTop = scroll.scrollHeight;
  });
  keepAtEnd.observe(list);
  keepAtEnd.observe(queue);

  store.subscribe(renderRows);
  mountComposer(store, view, {
    openKeys,
    onSent: () => {
      pinned = true;
      scroll.scrollTop = scroll.scrollHeight;
    },
  });

  // Suggestions in an empty chat go into the composer, to edit before sending.
  for (const button of empty.querySelectorAll("[data-suggest]")) {
    button.addEventListener("click", () => {
      const input = view.querySelector(".composer textarea");
      input.value = button.dataset.suggest;
      input.dispatchEvent(new Event("input"));
      input.focus();
    });
  }
  return { refresh: renderRows };
}

// --- Rows ----------------------------------------------------------------------

function renderRow(item, ctx) {
  switch (item.type) {
    case "user":
      return h("li", { class: "row row-user" }, h("div", { class: "bubble" }, item.text));
    case "assistant":
      return h("li", { class: "row row-agent" }, answer(item.text));
    case "report":
      return h("li", { class: "row row-report" }, report(item));
    case "image":
      return h("li", { class: "row row-agent" }, picture(item.url, item.caption));
    case "plan":
      return h("li", { class: "row row-plan" }, plan(item));
    case "approval":
      return h("li", { class: "row row-card" }, approvalCard(item, ctx));
    case "action":
      return h("li", { class: "row row-card" }, actionCard(item, ctx));
    case "covered":
      return h("li", { class: "row row-covered" }, coveredRow(item));
    default:
      return h("li", { class: "row" });
  }
}

/** A message waiting its turn: grey, with its place when there's more than one. */
function queuedRow(item, store) {
  const remove = h("button", { class: "queued-remove", type: "button", "aria-label": "Remove this queued message", title: "Remove" }, icon("close"));
  remove.addEventListener("click", () => busyButton(remove, () => store.unqueue(item)));
  const place = item.of > 1 ? `Queued · ${item.position} of ${item.of}` : "Queued";
  return h("li", { class: "row row-user row-queued", dataset: { key: `queued-${item.id}` } },
    h("div", { class: "queued" },
      h("div", { class: "bubble" }, item.text),
      h("p", { class: "queued-meta" }, icon("clock"), h("span", null, place), remove)));
}

function answer(text, className = "answer md") {
  const node = h("div", { class: className });
  node.append(render(parse(text)));
  return node;
}

/** Only the desktop's own saved screenshots. */
function picture(url, caption) {
  if (!url.startsWith("/api/v1/images/")) return h("p", { class: "muted" }, caption || "");
  return h("figure", { class: "shot" },
    h("img", { src: url, alt: caption || "A screenshot from your desktop", loading: "lazy" }),
    caption ? h("figcaption", null, caption) : null);
}

function report(item) {
  const runs = [...(item.earlier || []), item];
  const head = h("div", { class: "report-head" },
    icon("clock"),
    h("span", { class: "report-name" }, item.schedule),
    h("time", { datetime: item.ts }, runs.length > 1 ? `${time(runs[0].ts)}–${time(item.ts)}` : when(item.ts)));
  const instructions = h("details", { class: "instructions" },
    h("summary", null, "Instructions"),
    h("p", null, item.instructions));

  if (!item.answer) {
    return h("div", { class: "report" }, head, h("p", { class: "muted" }, "Running now"), instructions);
  }
  if (item.answer.quiet) {
    const line = runs.length > 1 ? `${runs.length} checks. ${item.answer.text}` : item.answer.text;
    const earlier = runs.length > 1
      ? h("details", { class: "instructions" },
          h("summary", null, runs.length === 2 ? "1 earlier check" : `${runs.length - 1} earlier checks`),
          h("ul", { class: "quiet-runs" }, runs.slice(0, -1).map((r) => h("li", null, h("time", null, time(r.ts)), " ", r.answer?.text || ""))))
      : null;
    return h("div", { class: "report report-quiet" }, head, h("p", { class: "quiet-line" }, line), earlier, instructions);
  }
  const copy = h("button", { class: "link-button", type: "button" }, "Copy");
  copy.addEventListener("click", () => navigator.clipboard?.writeText(item.answer.text).then(() => {
    copy.textContent = "Copied";
    setTimeout(() => (copy.textContent = "Copy"), 1500);
  }));
  return h("div", { class: "report report-full" }, head, answer(item.answer.text), h("div", { class: "report-foot" }, copy), instructions);
}

// --- A task's plan -----------------------------------------------------------------

const MARKS = { done: "✓", in_progress: "◐", pending: "○" };
const SAID = { done: "Done", in_progress: "In progress", pending: "To do" };

/** The agent's own steps, ticked off as it goes; folded once the task ends. */
function plan(item) {
  const done = item.steps.filter((s) => s.status === "done").length;
  const count = `${done} of ${item.steps.length}`;
  const steps = h("ol", { class: "plan-steps" }, item.steps.map((s) =>
    h("li", { class: `plan-step is-${s.status.replace("_", "-")}` },
      h("span", { class: "plan-mark", "aria-hidden": "true" }, MARKS[s.status]),
      h("span", { class: "visually-hidden" }, `${SAID[s.status]}: `),
      h("span", { class: "plan-text" }, s.step, needChips(s)))));
  if (item.finished) {
    return h("details", { class: "plan plan-finished" },
      h("summary", null, `Plan · ${count} done`), steps);
  }
  return h("section", { class: "plan", "aria-label": `Plan, ${count} done` },
    h("div", { class: "plan-head" }, h("span", { class: "plan-title" }, "Plan"), h("span", { class: "plan-count" }, count)),
    steps);
}

// What a step needs the person's OK for, each a chip: a site (clicks, typing
// and keys there), an app (input in it), commands that would ask, a saved key
// they use, an exact command (agentd before 0.19.1), an install.
const NEED_ICONS = { site: "globe", app: "desktop", shell: "terminal", key: "key", command: "terminal", install: "download" };

/** A step's chips; `onRemove(i)` gives each an × that takes the `i`th off. */
function needChips(step, onRemove) {
  const needs = stepNeeds(step);
  if (!needs.length) return null;
  return h("span", { class: "plan-needs" }, needs.map((need, i) => {
    const label = need.kind === "command" ? h("code", null, need.value)
      : need.kind === "key" ? ["Uses ", h("code", null, need.value)]
      : need.kind === "install" ? `Install ${need.value}` : need.value;
    let remove = null;
    if (onRemove) {
      const said = need.kind === "install" ? `installing ${need.value}`
        : need.kind === "shell" ? "running commands without asking"
        : need.kind === "key" ? `using the key ${need.value}` : `“${need.value}”`;
      remove = h("button", { class: "plan-need-remove", type: "button", "aria-label": `Take ${said} off this step`, title: "Take it off" }, icon("close"));
      remove.addEventListener("click", () => onRemove(i));
    }
    return h("span", { class: "plan-need" }, icon(NEED_ICONS[need.kind]), h("span", { class: "plan-need-text" }, label), remove);
  }));
}

// --- A plan to approve ---------------------------------------------------------------

// The person's version of each plan card's steps, by card, until it's
// answered: a row drawn again (or another conversation and back) keeps it.
const planDrafts = new Map();

// The task's plan, approved as a whole: what its steps need then goes ahead
// without cards of its own, for the rest of the task. The person can reword,
// delete, add and reorder steps, and take needs off, but not add one (agentd
// refuses that), so a new step has none.
function planCard(card, ctx) {
  const p = card.plan;
  const editable = card.editable;
  let steps = planDrafts.get(card.id) || p.steps;
  const change = (next) => {
    steps = next;
    planDrafts.set(card.id, next);
  };

  const list = h("ol", { class: "plan-edit" });
  const add = editable ? h("button", { class: "button button-quiet plan-add", type: "button" }, icon("plus"), "Add a step") : null;
  const summary = h("div", { class: "card-inset plan-summary" });
  const problem = h("p", { class: "card-text muted", hidden: true });
  const refusal = h("p", { class: "card-warning", role: "alert", hidden: true });
  const deny = h("button", { class: "button", type: "button" }, "Deny");
  const approve = h("button", { class: "button button-primary", type: "button" }, "Approve plan");

  /** The steps again, after a change to the list; `focus` picks what has focus after. */
  function draw(focus) {
    list.replaceChildren(...steps.map(stepRow));
    refresh();
    focus?.();
  }

  /** What depends on the steps: the summary, and whether it can be approved. */
  function refresh() {
    const phrases = planPhrases(planCovers(p.covers, steps));
    if (phrases.length) {
      const said = phrases.flatMap((phrase, i) => [i ? " · " : "", phrase.lead,
        andList(phrase.items, (item) => (phrase.code ? h("code", null, item) : item))]);
      fill(summary,
        h("p", null, "For this task, it won't ask again before: ", said, "."),
        h("p", { class: "muted" }, "Anything else still asks you."));
    } else {
      fill(summary, h("p", null, "Approving just starts the plan. Everything will ask you as usual."));
    }
    const why = planProblem(steps);
    problem.textContent = why;
    problem.hidden = !why;
    approve.disabled = Boolean(why);
    if (add) add.disabled = steps.length >= PLAN_MAX_STEPS;
    refusal.hidden = true; // about the version it refused
  }

  const focusIn = (index, selector) => () => {
    const row = list.children[Math.min(index, list.children.length - 1)];
    const target = [...(row?.querySelectorAll(selector) || [])].find((el) => !el.disabled) || row?.querySelector("input");
    (target || add)?.focus();
  };

  function stepRow(step, index) {
    const replace = (next) => change(steps.map((s, i) => (i === index ? next : s)));
    const chips = needChips(step, editable ? (need) => {
      replace(withoutNeed(steps[index], need)); // its words as typed since
      draw(focusIn(index, ".plan-need-remove"));
    } : null);
    const n = h("span", { class: "app-step-n" }, String(index + 1));
    const done = step.status === "done" ? h("span", { class: "visually-hidden" }, "Done: ") : null;
    if (!editable) {
      return h("li", { class: `plan-edit-step is-fixed is-${step.status.replace("_", "-")}` }, n,
        h("p", { class: "card-text" }, done, step.step), chips);
    }
    const input = h("input", {
      class: "field plan-edit-text", type: "text", value: step.step, maxlength: PLAN_STEP_LENGTH,
      "aria-label": `Step ${index + 1}${step.status === "done" ? ", done" : ""}`, placeholder: "What to do",
    });
    input.addEventListener("input", () => {
      replace({ ...steps[index], step: input.value });
      refresh();
    });
    const tool = (name, label, disabled, act) => {
      const button = h("button", { class: "plan-tool", type: "button", "aria-label": label, title: label, disabled }, icon(name));
      button.addEventListener("click", act);
      return button;
    };
    const move = (by) => {
      const next = [...steps];
      [next[index], next[index + by]] = [next[index + by], next[index]];
      change(next);
      draw(focusIn(index + by, `[aria-label="${by < 0 ? "Move up" : "Move down"}"]`));
    };
    return h("li", { class: `plan-edit-step is-${step.status.replace("_", "-")}` }, n, input, chips,
      h("div", { class: "plan-tools" },
        tool("up", "Move up", index === 0, () => move(-1)),
        tool("down", "Move down", index === steps.length - 1, () => move(1)),
        tool("trash", "Delete this step", false, () => {
          change(steps.filter((_, i) => i !== index));
          draw(focusIn(index, "input"));
        })));
  }

  add?.addEventListener("click", () => {
    change([...steps, { step: "", status: "pending" }]);
    draw(focusIn(steps.length - 1, "input"));
  });
  deny.addEventListener("click", () => busyButton(deny, () => ctx.store.answer(card, false)));
  approve.addEventListener("click", () => busyButton(approve, async () => {
    const refused = await ctx.store.answerPlan(card, true, planAnswer(p.steps, steps));
    if (refused) fill(refusal, icon("warning"), h("span", null, `Not approved: ${refused}`));
    refusal.hidden = !refused;
  }));
  draw();
  return h("section", { class: "card card-needs", "aria-label": "Approve this plan?" },
    h("h3", { class: "card-title" }, icon("list"), "Approve this plan?"),
    card.tainted ? warning("The agent wrote this plan after reading web pages or other outside content. Check each step is something you asked for.") : null,
    list, add, summary, problem, refusal, buttons(deny, approve));
}

// --- What a plan covered -----------------------------------------------------------

// An action that went ahead without its card, under a plan the person
// approved: a quiet line of what it did, what it typed or ran folded under it.
function coveredRow(item) {
  const words = coveredWords(item);
  const where = words.where ? ` · ${words.where}` : "";
  const title = `Under your plan${where} — ${words.what}`;
  const line = [h("span", { class: "covered-mark", "aria-hidden": "true" }, "✓"),
    h("span", { class: "covered-line" }, "Under your plan", where, " — ", words.code ? h("code", null, words.what) : words.what)];
  if (!words.more) return h("p", { class: "covered", title }, line);
  return h("details", { class: "covered" },
    h("summary", { title }, line, icon("next", "icon covered-more")),
    h("div", { class: "covered-details" }, coveredDetails(item)));
}

function coveredDetails(item) {
  const b = item.browser;
  const a = item.app;
  if (b) {
    if (b.action === "type") {
      return [b.target ? h("p", { class: "card-text muted" }, `In “${b.target}”`) : null, h("blockquote", { class: "card-quote" }, b.text ?? "")];
    }
    if (b.action === "key") return h("p", { class: "card-text" }, h("kbd", null, b.key || "?"), b.target ? ` in “${b.target}”` : "");
    return h("p", { class: "card-text card-target" }, clickTarget(b.role, b.target));
  }
  if (a) {
    return [
      a.steps.length ? appSteps(a, false) : a.text !== null ? h("blockquote", { class: "card-quote" }, a.text) : null,
      a.window ? h("p", { class: "card-text muted card-value" }, a.window) : null,
    ];
  }
  if (item.kind === "install") return h("p", { class: "muted mono card-value" }, item.install.name);
  return h("pre", { class: "covered-command" }, h("code", null, item.command));
}

// --- Cards -----------------------------------------------------------------------

function buttons(...list) {
  return h("div", { class: "card-actions" }, ...list);
}

function answerButtons(card, ctx, edit, approvalDisabled = false) {
  const deny = h("button", { class: "button", type: "button" }, "Deny");
  const approve = h("button", { class: "button button-primary", type: "button", disabled: approvalDisabled }, "Approve");
  deny.addEventListener("click", () => busyButton(deny, () => ctx.store.answer(card, false)));
  approve.addEventListener("click", () => busyButton(approve, () => ctx.store.answer(card, true, edit?.changed())));
  // "Allow for this task": this, and what's like it, without asking again
  // until the task ends (a card a grant can cover offers it).
  let allow = null;
  let note = null;
  if (card.forTask) {
    allow = h("button", { class: "button", type: "button", disabled: approvalDisabled }, "Allow for this task");
    allow.addEventListener("click", () => busyButton(allow, () => ctx.store.answer(card, true, edit?.changed(), true)));
    note = h("p", { class: "card-note card-for-task" }, `Allow for this task also lets ${card.forTask} go ahead without asking, until this task ends.`);
  }
  if (!edit?.button) return [note, buttons(deny, allow, approve)];
  edit.onChange = () => {
    approve.disabled = approvalDisabled || edit.blank();
    if (allow) allow.disabled = approve.disabled;
  };
  return [note, buttons(edit.button, deny, allow, approve)];
}

// The text a card asks about, with Edit: what gets approved can be the person's
// own version. `shown` is how the card shows it until then. Only on a card that
// says it takes an edit: an older agent would ignore it and run its own words.
function textEdit(card, original, label, shown, { code = false } = {}) {
  if (!card.editable) return { button: null, input: null, changed: () => undefined };
  const input = h("textarea", {
    class: `field card-edit${code ? " card-edit-code" : ""}`, "aria-label": label, hidden: true,
    rows: Math.min(14, Math.max(3, original.split("\n").length + 1)), spellcheck: code ? "false" : "true",
  });
  const button = h("button", { class: "button", type: "button" }, "Edit");
  const edit = {
    button, input, onChange: () => {},
    blank: () => !input.hidden && !input.value.trim(),
    changed: () => (!input.hidden && input.value !== original ? input.value : undefined),
  };
  input.addEventListener("input", () => edit.onChange());
  button.addEventListener("click", () => {
    const opening = input.hidden;
    input.value = original;
    input.hidden = !opening;
    shown.hidden = opening;
    button.textContent = opening ? "Undo changes" : "Edit";
    if (opening) {
      // A command starts folded away.
      const details = input.closest("details");
      if (details) details.open = true;
      input.focus();
    }
    edit.onChange();
  });
  return edit;
}

function warning(text) {
  return h("p", { class: "card-warning" }, icon("warning"), h("span", null, text));
}

function approvalCard(card, ctx) {
  if (card.kind === "plan") return planCard(card, ctx);
  if (card.kind === "browser") return browserCard(card, ctx);
  if (card.kind === "app") return appCard(card, ctx);
  if (card.kind === "install") return installCard(card, ctx);
  if (card.kind === "schedule") return scheduleCard(card, ctx);
  if (card.kind === "secret") return secretCard(card, ctx);
  if (card.kind === "access") return accessCard(card, ctx);
  if (card.kind === "stuck") return stuckCard(card, ctx);
  const command = h("pre", null, h("code", null, card.command));
  const edit = textEdit(card, card.command, "The command", command, { code: true });
  return h("section", { class: "card card-needs", "aria-label": "Needs your OK" },
    h("p", { class: "card-kicker" }, icon("shield"), "Needs your OK"),
    h("h3", { class: "card-title" }, card.summary || "Run a command on your desktop"),
    card.reason ? h("p", { class: "card-text" }, `It ${card.reason}.`) : null,
    card.tainted ? warning("The agent read web pages or other outside content before asking. Check the command if this isn't what you asked for.") : null,
    card.direct ? warning("Direct internet access outside the proxy for this command.") : null,
    card.savedKeys?.length ? h("div", { class: "card-inset" },
      h("p", { class: "strong" }, "Saved keys this command can use"),
      h("p", { class: "card-value" }, h("code", null, card.savedKeys.join(", ")))) : null,
    h("details", { class: "command", open: true }, h("summary", null, "Command"), command, edit.input),
    answerButtons(card, ctx, edit));
}

function browserCard(card, ctx) {
  const b = card.browser;
  const legacySend = b.action === "type" && b.sends && !b.submit;
  const title = b.action === "type" ? (b.submit ? "Write this and press Enter?" : "Write this?")
    : b.action === "key" ? `Press ${b.key || "a key"}?` : "Click this?";
  let what;
  let edit;
  if (b.action === "type") {
    const quote = h("blockquote", { class: "card-quote" }, b.text || "");
    edit = textEdit(card, b.text || "", "What to write", quote);
    what = [b.target ? h("p", { class: "card-text muted" }, `In “${b.target}”`) : null, quote, edit.input];
  } else if (b.action === "key") {
    what = h("p", { class: "card-text" }, h("kbd", null, b.key || "?"), b.target ? ` in “${b.target}”` : "");
  } else {
    what = h("p", { class: "card-text card-target" }, clickTarget(b.role, b.target));
  }
  // A click or key says little by itself: the agent's reason first, then what the
  // page itself shows, which the agent's words can't change.
  const said = b.action !== "type" && card.summary ? h("p", { class: "card-text strong" }, `The agent says: ${card.summary}`) : null;
  const facts = [
    b.around ? h("div", { class: "card-inset" }, h("p", { class: "strong" }, "Around it on the page"), h("p", { class: "muted" }, b.around)) : null,
    b.page ? h("p", { class: "card-text muted" }, b.page) : null,
    b.url || b.site ? h("p", { class: "card-text muted card-value" }, b.url || b.site) : null,
    b.destination ? h("p", { class: "card-text muted card-value" }, `Destination: ${b.destination}`) : null,
    b.image ? picture(b.image, "The page, with the target outlined") : null,
  ];
  const note = legacySend ? warning("This older request bundled typing and sending. Deny it and ask again after updating the desktop.")
    : b.action === "type" && b.submit ? warning("Then presses Enter, which may send it.")
    : b.action !== "type" && card.reason ? h("p", { class: "card-text muted" }, `It ${card.reason}.`) : null;
  return h("section", { class: "card card-needs", "aria-label": title },
    h("div", { class: "card-head" },
      h("h3", { class: "card-title" }, icon(b.action === "click" ? "click" : "keyboard"), title),
      b.site ? h("span", { class: "card-site" }, icon("globe"), b.site) : null),
    said, what, facts, note, answerButtons(card, ctx, edit, legacySend));
}

// What an app card asks, by its action ("key" names its key).
const APP_ASKS = {
  type: "Write this", set_value: "Set this", select: "Select this", scroll: "Scroll",
  activate: "Switch windows", close: "Close this window", act: "Do this", gui: "Do this",
};

// The browser card's counterpart for the desktop's own apps, which the agent
// drives through their accessibility tree, or (with a model that sees the
// screen) by pixels of a screenshot. A batch lists every step, in order.
// Never editable.
function appCard(card, ctx) {
  const a = card.app;
  const where = a.app ? `in ${a.app}` : "in the app";
  const title = a.action === "key" ? `Press ${a.key || "a key"} ${where}?` : `${APP_ASKS[a.action] || "Click this"} ${where}?`;
  const listed = a.steps.length > 1 || a.action === "act" || a.action === "gui";
  const only = a.steps.length === 1 ? a.steps[0] : null;
  const marked = a.action === "gui" && a.frame !== null && typeof a.image === "string" && a.image.startsWith("/api/v1/images/");
  let what;
  if (listed) {
    what = appSteps(a, marked);
  } else if (a.action === "type") {
    what = [a.target ? h("p", { class: "card-text muted" }, `In “${a.target}”`) : null,
      h("blockquote", { class: "card-quote" }, a.text || "")];
  } else if (a.action === "key") {
    what = h("p", { class: "card-text" }, h("kbd", null, a.key || "?"), a.target ? ` in “${a.target}”` : "");
  } else if (["set_value", "select", "scroll", "activate", "close"].includes(a.action)) {
    // Its one step in words; where it is in the app follows, as for a click.
    what = stepLines({ ...appStepWords(only || a, { window: a.window }), note: null }, "card-target");
  } else {
    what = h("p", { class: "card-text card-target" }, clickTarget(a.role, a.target), only?.pointer ? " with the mouse" : "");
  }
  // The agent's reason first, then what the app itself shows, which the
  // agent's words can't change.
  const said = (listed || a.action !== "type") && card.summary
    ? h("p", { class: "card-text strong" }, `The agent says: ${card.summary}`) : null;
  const facts = [
    !listed && a.around ? h("div", { class: "card-inset" }, h("p", { class: "strong" }, "Where it is in the app"), h("p", { class: "muted" }, a.around)) : null,
    a.window ? h("p", { class: "card-text muted card-value" }, a.window) : null,
    a.image ? (marked ? markedPicture(a) : picture(a.image, "The desktop when the agent asked")) : null,
  ];
  return h("section", { class: "card card-needs", "aria-label": title },
    h("div", { class: "card-head" },
      h("h3", { class: "card-title" }, icon(["type", "key", "set_value"].includes(a.action) ? "keyboard" : "click"), title),
      a.app ? h("span", { class: "card-site" }, icon("desktop"), a.app) : null),
    said, what, facts,
    card.reason ? h("p", { class: "card-text muted" }, `It ${card.reason}.`) : null,
    answerButtons(card, ctx));
}

// A batch's steps, numbered. A pixel step's number is also on the picture.
function appSteps(a, marked) {
  const pixels = a.action === "gui";
  return h("ol", { class: "app-steps" }, a.steps.map((s, i) => {
    const words = appStepWords(s, { pixels, window: a.window });
    return h("li", { class: words.quiet ? "app-step is-quiet" : "app-step" },
      h("span", { class: marked && s.points.length ? "app-step-n is-marked" : "app-step-n" }, String(i + 1)),
      h("div", { class: "app-step-body" }, stepLines(words)));
  }));
}

function stepLines(words, className = "") {
  return [
    h("p", { class: `card-text ${className}`.trim() }, words.text, words.key ? [" ", h("kbd", null, words.key)] : null),
    words.note ? h("p", { class: "app-step-note muted" }, words.note) : null,
    words.quote !== null ? h("blockquote", { class: "card-quote" }, words.quote) : null,
    words.window ? h("p", { class: "app-step-note muted card-value" }, words.window) : null,
  ];
}

// The screenshot a pixel batch was planned on: a ring where each step acts,
// which leaves what it acts on in sight, with the step's number beside it,
// and a drag's path drawn. The marks are placed in percent of the frame, so
// they stay put as the picture scales.
function markedPicture(a) {
  const { width, height } = a.frame;
  const percent = (value) => `${Math.min(100, Math.max(0, value * 100))}%`;
  const mark = (p, label, className = "shot-mark") => {
    const el = h("span", { class: className, "aria-hidden": "true" }, label ? h("span", { class: "shot-mark-n" }, label) : null);
    // CSSOM: the page's policy allows no style attributes.
    el.style.left = percent(p.x / width);
    el.style.top = percent(p.y / height);
    return el;
  };
  const paths = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  paths.setAttribute("class", "shot-paths");
  paths.setAttribute("viewBox", `0 0 ${width} ${height}`);
  paths.setAttribute("preserveAspectRatio", "none");
  paths.setAttribute("aria-hidden", "true");
  const marks = [];
  a.steps.forEach((s, i) => {
    if (!s.points.length) return;
    if (s.points.length > 1) {
      const line = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
      line.setAttribute("points", s.points.map((p) => `${p.x},${p.y}`).join(" "));
      line.setAttribute("vector-effect", "non-scaling-stroke");
      paths.append(line);
      marks.push(mark(s.points[s.points.length - 1], "", "shot-mark is-end"));
    }
    marks.push(mark(s.points[0], String(i + 1)));
  });
  const img = h("img", { src: a.image, alt: "A screenshot from your desktop, numbered where each step acts", loading: "lazy" });
  return h("figure", { class: "shot" },
    h("div", { class: "shot-marks" }, img, paths.childNodes.length ? paths : null, marks),
    h("figcaption", null, "The desktop when the agent asked, numbered by step"));
}

// An app from a store (the Snap Store), installed on the desktop as root. The
// agent's reason first, then what the store says about it, which the agent's
// words can't change. Never editable.
function installCard(card, ctx) {
  const i = card.install;
  const title = `Install ${i.app || i.name || "this app"}?`;
  const details = [i.version ? `Version ${i.version}` : "", i.channel, i.size ? megabytes(i.size) : ""]
    .filter(Boolean).join(" · ");
  const facts = [
    i.publisher ? h("p", { class: "strong" }, i.publisher,
      i.verified ? h("span", { class: "card-verified" }, "✓ Verified publisher") : null) : null,
    details ? h("p", null, details) : null,
    i.summary ? h("p", { class: "muted" }, i.summary) : null,
    i.name ? h("p", { class: "muted mono card-value" }, i.name) : null,
  ];
  return h("section", { class: "card card-needs", "aria-label": title },
    h("div", { class: "card-head" },
      h("h3", { class: "card-title" }, icon("plus"), title),
      i.source ? h("span", { class: "card-site" }, icon("desktop"), i.source) : null),
    i.why ? h("p", { class: "card-text strong" }, `The agent says: ${i.why}`) : null,
    facts.some(Boolean) ? h("div", { class: "card-inset" }, facts) : null,
    i.confinement === "strict"
      ? h("p", { class: "card-text muted" }, "Strictly confined: it runs in the Snap sandbox with a snap's usual access, which includes your desktop user's files.")
      : null,
    card.tainted ? warning("The agent read web pages or other outside content before asking. Check this is an app you asked for.") : null,
    card.reason ? h("p", { class: "card-text muted" }, `It ${card.reason}.`) : null,
    answerButtons(card, ctx));
}

function scheduleCard(card, ctx) {
  const s = card.schedule;
  const prompt = h("p", { class: "muted pre" }, s.prompt);
  const edit = textEdit(card, s.prompt, "What to do each time", prompt);
  return h("section", { class: "card card-needs", "aria-label": "Schedule this?" },
    h("h3", { class: "card-title" }, icon("calendar"), "Schedule this?"),
    h("div", { class: "card-inset" },
      h("p", { class: "strong" }, s.name),
      h("p", null, s.when),
      prompt, edit.input),
    h("p", { class: "card-text muted" }, "The agent asked for this after reading outside content. Approve only if you asked for it."),
    answerButtons(card, ctx, edit));
}

// The supervisor thinks the task is going in circles: carry on, or stop it
// (it then ends with what's done and what's left). Never answered for you.
function stuckCard(card, ctx) {
  const s = card.stuck;
  const facts = [
    s.minutes >= 1 ? `Working for ${duration(s.minutes)}` : null,
    s.steps ? `${s.steps} ${s.steps === 1 ? "step" : "steps"}` : null,
    s.plan ? `plan ${s.plan.done} of ${s.plan.total} done` : null,
  ].filter(Boolean).join(" · ");
  const stop = h("button", { class: "button", type: "button" }, "Stop");
  const keep = h("button", { class: "button button-primary", type: "button" }, "Keep going");
  stop.addEventListener("click", () => busyButton(stop, () => ctx.store.answer(card, false)));
  keep.addEventListener("click", () => busyButton(keep, () => ctx.store.answer(card, true)));
  return h("section", { class: "card card-needs", "aria-label": "Looks stuck" },
    h("p", { class: "card-kicker" }, icon("clock"), "Checked on your task"),
    h("h3", { class: "card-title" }, "Looks stuck"),
    s.reason ? h("p", { class: "card-text" }, s.reason) : null,
    facts ? h("p", { class: "card-text muted" }, facts) : null,
    h("p", { class: "card-note" }, "Stop ends it with what's done and what's left. Keep going lets it carry on, with a hint."),
    buttons(stop, keep));
}

// The agent's commands hit a site the desktop's proxy doesn't let through.
function accessCard(card, ctx) {
  const { hosts, why } = card.access;
  const one = hosts.length === 1;
  const deny = h("button", { class: "button", type: "button" }, "Don't allow");
  const allow = h("button", { class: "button button-primary", type: "button" }, "Allow");
  deny.addEventListener("click", () => busyButton(deny, () => ctx.store.answer(card, false)));
  allow.addEventListener("click", () => busyButton(allow, () => ctx.store.answer(card, true)));
  const title = one ? "Let your agent reach this site?" : "Let your agent reach these sites?";
  return h("section", { class: "card card-needs", "aria-label": title },
    h("h3", { class: "card-title" }, icon("globe"), title),
    h("ul", { class: "card-inset card-hosts" }, hosts.map((host) => h("li", null, h("code", null, host)))),
    why ? h("p", { class: "card-text" }, why) : null,
    h("p", { class: "card-note" }, `Its commands can reach ${one ? "it" : "them"} from now on, scheduled checks included. You can remove sites in Settings.`),
    card.tainted ? warning(`Asked for after the agent read a page or command output. Allow only if you asked for something that needs ${one ? "this site" : "these sites"}.`) : null,
    buttons(deny, allow));
}

function secretCard(card, ctx) {
  const s = card.secret;
  const label = s.replace ? `A new ${s.label}` : s.label;
  if (s.saved) {
    const go = h("button", { class: "button", type: "button" }, "Continue");
    go.addEventListener("click", () => busyButton(go, () => ctx.store.answer(card, true)));
    return h("section", { class: "card card-needs" },
      h("h3", { class: "card-title" }, icon("key"), label),
      h("p", { class: "card-ok" }, icon("lock"), "Saved on your desktop"),
      buttons(go));
  }
  const input = h("input", {
    type: "password", class: "field", autocomplete: "off", spellcheck: "false",
    placeholder: `Paste your ${s.label}`, "aria-label": s.label,
  });
  const later = h("button", { class: "button", type: "button" }, "Not now");
  const save = h("button", { class: "button button-primary", type: "submit" }, "Save");
  later.addEventListener("click", () => busyButton(later, () => ctx.store.answer(card, false)));
  const form = h("form", { class: "secret-form" }, input, buttons(later, save));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const value = input.value.trim();
    if (!value) return input.focus();
    busyButton(save, async () => {
      if (await ctx.store.saveRequested(card, value)) input.value = "";
    });
  });
  const link = s.url && safeHref(s.url) && s.url.startsWith("https://")
    ? h("a", { class: "card-link", href: s.url, target: "_blank", rel: "noopener noreferrer" },
        `Open ${s.host || new URL(s.url).host}`, icon("external"))
    : null;
  return h("section", { class: "card card-needs", "aria-label": label },
    h("h3", { class: "card-title" }, icon("key"), label),
    s.why ? h("p", { class: "card-text" }, s.why) : null,
    s.how ? answer(s.how, "card-how md") : null,
    link, form,
    h("p", { class: "card-note" }, icon("lock"), "Kept only on your desktop. The agent uses it by name and never sees it."),
    card.tainted ? warning("Asked for after the agent read a page or command output. Only go on if you asked for something that needs this key.") : null);
}

function actionCard(card, ctx) {
  const skip = h("button", { class: "button", type: "button" }, "Skip");
  const done = h("button", { class: "button", type: "button" }, "I'm done");
  const take = h("button", { class: "button button-primary", type: "button" }, "Take over");
  skip.addEventListener("click", () => busyButton(skip, () => ctx.store.resolve(card, false)));
  done.addEventListener("click", () => busyButton(done, () => ctx.store.resolve(card, true)));
  take.addEventListener("click", () => ctx.openDesktop());
  const title = card.page.split(" | ")[0];
  return h("section", { class: "card card-needs", "aria-label": "Action needed" },
    h("p", { class: "card-kicker" }, icon("desktop"), "Action needed"),
    h("h3", { class: "card-title" }, card.reason),
    card.image ? picture(card.image, title) : null,
    title && !card.image ? h("p", { class: "card-text muted" }, title) : null,
    buttons(skip, done, take),
    h("p", { class: "card-note" }, "You sign in inside your desktop's browser and the session stays there. The site may email you about a new device."));
}

// --- Composer ----------------------------------------------------------------------

// It stays on while the agent works: a message sent then waits its turn (on a
// desktop from before queued messages, which refuses it, Send waits instead).
// Stop has a button of its own. Each conversation keeps its own draft.
function mountComposer(store, view, { openKeys, onSent }) {
  const form = view.querySelector(".composer");
  const input = form.querySelector("textarea");
  const send = form.querySelector(".send");
  const stop = form.querySelector(".stop");
  const model = form.querySelector(".model");
  const keyNote = form.querySelector(".key-note");
  const budgetNote = form.querySelector(".budget-note");
  const archivedNote = form.querySelector(".archived-note");
  const modelKeyNote = form.querySelector(".model-key-note");
  const box = form.querySelector(".box");
  let modelKeyFor = null; // the provider the note asks a key for
  let allowKey = null; // the key the person chose to send anyway
  const drafts = new Map(); // conversation → what was typed there
  let draftFor = store.currentId;

  send.replaceChildren(icon("send"));
  stop.replaceChildren(icon("stop"));

  function autosize() {
    input.style.height = "auto";
    // Not on screen (another page is), it has no height to measure: left as one line.
    if (input.scrollHeight) input.style.height = `${Math.min(input.scrollHeight, 220)}px`;
  }

  function showKeyNote() {
    const found = findKey(input.value);
    keyNote.replaceChildren();
    keyNote.hidden = !found || found.value === allowKey;
    if (keyNote.hidden) return;
    const save = h("button", { class: "link-button", type: "button" }, "Save it as a key instead");
    const anyway = h("button", { class: "link-button", type: "button" }, "Send anyway");
    save.addEventListener("click", () => {
      input.value = input.value.replace(found.value, `$${found.name}`).trim();
      autosize();
      keyNote.hidden = true;
      openKeys({ name: found.name, label: found.label, value: found.value });
    });
    anyway.addEventListener("click", () => {
      allowKey = found.value;
      keyNote.hidden = true;
    });
    keyNote.append(icon("key"),
      h("span", null, `That looks like a ${found.label}. Anything sent here goes to the model and stays in the chat.`),
      save, anyway);
  }

  /**
   * The chat model's provider has no key (a new desktop, or one removed): the
   * key is asked for right here, so the first message can go. Drawn once per
   * provider, so a half-pasted key isn't wiped by an update.
   */
  function showModelKeyNote(needs) {
    const id = needs?.id || null;
    modelKeyNote.hidden = !id;
    if (id === modelKeyFor) return;
    modelKeyFor = id;
    if (!id) {
      modelKeyNote.replaceChildren();
      return;
    }
    const provider = (store.status?.model_providers || []).find((p) => p.id === id) || needs;
    modelKeyNote.replaceChildren(
      h("p", { class: "strong" }, icon("key"), `Your agent needs your ${keyLabel(provider)} to work.`),
      h("p", { class: "muted" }, "Paste it once and it's used from your next message on. Or pick another model in Settings."),
      modelKeyForm(store, provider, { onSaved: (result) => store.notice(savedWords(provider, result)) }));
  }

  /** Another conversation on screen: its draft back in the box. */
  function swapDraft() {
    if (store.currentId === draftFor) return;
    drafts.set(draftFor, input.value);
    draftFor = store.currentId;
    input.value = drafts.get(draftFor) || "";
    allowKey = null;
    autosize();
    showKeyNote();
  }

  function update() {
    swapDraft();
    const busy = store.busy;
    const offline = store.connection !== "live";
    const older = store.sessionsSupported === false;
    const queued = store.queue().length;
    // Sent while it works, a message queues (an older desktop would refuse it).
    const needsKey = store.status?.needs_model_key || null;
    send.hidden = busy && (older || !input.value.trim());
    send.disabled = offline || !input.value.trim() || Boolean(needsKey);
    stop.hidden = !busy;
    stop.classList.toggle("is-alone", send.hidden);
    stop.disabled = offline || store.stopping;
    const label = store.stopping ? "Stopping" : queued ? `Stop, and drop ${queuedWords(queued)}` : "Stop";
    stop.setAttribute("aria-label", label);
    stop.title = label;
    input.placeholder = busy && !older ? "Queue a message for after this" : "Ask your agent";
    // An archived conversation takes no messages until it's back in the list.
    const archived = store.currentSession?.archived === true;
    archivedNote.hidden = !archived;
    box.hidden = archived;
    budgetNote.hidden = store.status?.budget?.paused !== true || archived;
    showModelKeyNote(archived ? null : needsKey);
    const status = store.status;
    if (status && document.activeElement !== model) {
      const current = status.settings?.model || "";
      const models = [...new Set([current, ...(status.known_models || [])])].filter(Boolean);
      if (model.dataset.models !== models.join(",")) {
        model.replaceChildren(...models.map((m) => h("option", { value: m }, modelName(m))));
        model.dataset.models = models.join(",");
      }
      model.value = current;
      model.hidden = models.length === 0;
    }
  }

  async function submit() {
    const text = input.value.trim();
    if (!text || store.connection !== "live" || send.hidden) return;
    const found = findKey(text);
    if (found && found.value !== allowKey) {
      showKeyNote();
      return;
    }
    send.disabled = true;
    const to = store.currentId;
    if (await store.send(text)) {
      // Switched away meanwhile: that conversation's draft was this message.
      if (store.currentId === to) input.value = "";
      else drafts.set(to, "");
      allowKey = null;
      autosize();
      onSent();
    }
    update();
  }

  stop.addEventListener("click", async () => {
    const queued = store.queue().length;
    if (queued && !confirm(`Stop the task? It also drops ${queuedWords(queued)}.`)) return;
    await store.stop();
  });
  input.addEventListener("input", () => {
    autosize();
    showKeyNote();
    update();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      submit();
    }
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    submit();
  });
  const unarchive = archivedNote.querySelector("button");
  unarchive.addEventListener("click", () =>
    busyButton(unarchive, () => store.updateSession(store.currentId, { archived: false })));
  model.addEventListener("change", async () => {
    if (!store.status?.settings) return;
    try {
      await store.api.updateSettings({ model: model.value });
      store.refreshStatus();
    } catch (error) {
      store.notice(error.message);
    }
  });
  store.subscribe(update);
  update();
}

function queuedWords(count) {
  return count === 1 ? "the queued message" : `the ${count} queued messages`;
}
