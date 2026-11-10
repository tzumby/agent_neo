// The chat: only the conversation, scheduled runs as reports, and the cards
// waiting for the person. Three dots while the agent works; nothing else of
// what it does. Opens at its end; older pages load as the person scrolls up,
// without moving what's on screen.

import { findKey } from "./keys.js";
import { parse, render, safeHref } from "./markdown.js";
import { busyButton, h, icon, site, time, when } from "./dom.js";
import { rowKey } from "./timeline.js";

export function mountChat(store, view, { openDesktop, openKeys }) {
  const scroll = view.querySelector(".chat-scroll");
  const list = view.querySelector(".rows");
  const older = view.querySelector(".older");
  const working = view.querySelector(".working");
  const empty = view.querySelector(".empty");
  const rows = new Map(); // row key → {sig, node}
  let revealed = false;
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

  function renderRows() {
    if (!store.loaded || !fontsReady) return;
    const items = store.timeline.visible();
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
      }
    }

    working.hidden = !store.busy;
    working.setAttribute("aria-label", store.stopping ? "Stopping" : "Working");
    empty.hidden = items.length > 0 || store.busy;
    older.hidden = !store.hasOlder;

    // An empty chat holds the suggestions: it reads from its top.
    if (!items.length) scroll.scrollTop = 0;
    else if (stick) scroll.scrollTop = scroll.scrollHeight;
    else if (anchor?.isConnected) scroll.scrollTop += anchor.getBoundingClientRect().top - anchorTop;
    if (!revealed) {
      revealed = true;
      view.classList.add("is-ready"); // only the splash fades; the chat is already at its end
      setTimeout(() => (view.querySelector(".splash").hidden = true), 300);
      checkOlder();
    }
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

  scroll.addEventListener("scroll", () => (pinned = nearBottom()), { passive: true });
  new ResizeObserver(() => {
    if (pinned && revealed && list.children.length) scroll.scrollTop = scroll.scrollHeight;
  }).observe(list);

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
    case "approval":
      return h("li", { class: "row row-card" }, approvalCard(item, ctx));
    case "action":
      return h("li", { class: "row row-card" }, actionCard(item, ctx));
    default:
      return h("li", { class: "row" });
  }
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

// --- Cards -----------------------------------------------------------------------

function buttons(...list) {
  return h("div", { class: "card-actions" }, ...list);
}

function answerButtons(card, ctx, edit) {
  const deny = h("button", { class: "button", type: "button" }, "Deny");
  const approve = h("button", { class: "button button-primary", type: "button" }, "Approve");
  deny.addEventListener("click", () => busyButton(deny, () => ctx.store.answer(card, false)));
  approve.addEventListener("click", () => busyButton(approve, () => ctx.store.answer(card, true, edit?.changed())));
  if (!edit?.button) return buttons(deny, approve);
  edit.onChange = () => (approve.disabled = edit.blank());
  return buttons(edit.button, deny, approve);
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
  if (card.kind === "browser") return browserCard(card, ctx);
  if (card.kind === "schedule") return scheduleCard(card, ctx);
  if (card.kind === "secret") return secretCard(card, ctx);
  if (card.kind === "access") return accessCard(card, ctx);
  const command = h("pre", null, h("code", null, card.command));
  const edit = textEdit(card, card.command, "The command", command, { code: true });
  return h("section", { class: "card card-needs", "aria-label": "Needs your OK" },
    h("p", { class: "card-kicker" }, icon("shield"), "Needs your OK"),
    h("h3", { class: "card-title" }, card.summary || "Run a command on your desktop"),
    card.reason ? h("p", { class: "card-text" }, `It ${card.reason}.`) : null,
    card.tainted ? warning("The agent read web pages or other outside content before asking. Check the command if this isn't what you asked for.") : null,
    h("details", { class: "command" }, h("summary", null, "Show the command"), command, edit.input),
    answerButtons(card, ctx, edit));
}

function browserCard(card, ctx) {
  const b = card.browser;
  const title = b.action === "type" ? (b.submit || b.sends ? "Write and send this?" : "Write this?")
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
    const name = b.target ? `“${b.target}”` : "without a name";
    const role = b.role ? `${b.role[0].toUpperCase()}${b.role.slice(1)} ` : "";
    what = h("p", { class: "card-text card-target" }, `${role}${name}`);
  }
  // A click or key says little by itself: the agent's reason first, then what the
  // page itself shows, which the agent's words can't change.
  const said = b.action !== "type" && card.summary ? h("p", { class: "card-text strong" }, `The agent says: ${card.summary}`) : null;
  const facts = b.action === "type" ? null : [
    b.around ? h("div", { class: "card-inset" }, h("p", { class: "strong" }, "Around it on the page"), h("p", { class: "muted" }, b.around)) : null,
    b.page || b.url ? h("p", { class: "card-text muted" }, [b.page, b.url].filter(Boolean).join(", ")) : null,
    b.image ? picture(b.image, "The page, with what it would click outlined") : null,
  ];
  const note = b.action === "type" && b.submit ? warning("Then presses Enter, which may send it.")
    : b.action === "type" && b.sends ? warning("Approving also lets it send this.")
    : b.action !== "type" && card.reason ? h("p", { class: "card-text muted" }, `It ${card.reason}.`) : null;
  return h("section", { class: "card card-needs", "aria-label": title },
    h("div", { class: "card-head" },
      h("h3", { class: "card-title" }, icon(b.action === "click" ? "click" : "keyboard"), title),
      b.site ? h("span", { class: "card-site" }, icon("globe"), site(b.site)) : null),
    said, what, facts, note, answerButtons(card, ctx, edit));
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

function mountComposer(store, view, { openKeys, onSent }) {
  const form = view.querySelector(".composer");
  const input = form.querySelector("textarea");
  const send = form.querySelector(".send");
  const model = form.querySelector(".model");
  const keyNote = form.querySelector(".key-note");
  let allowKey = null; // the key the person chose to send anyway

  function autosize() {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 220)}px`;
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

  function update() {
    const busy = store.busy;
    const offline = store.connection !== "live";
    send.replaceChildren(icon(busy ? "stop" : "send"));
    send.setAttribute("aria-label", busy ? (store.stopping ? "Stopping" : "Stop") : "Send");
    send.classList.toggle("is-stop", busy);
    send.disabled = offline || store.stopping || (!busy && !input.value.trim());
    const status = store.status;
    if (status && document.activeElement !== model) {
      const current = status.settings?.model || "";
      const models = [...new Set([current, ...(status.known_models || [])])].filter(Boolean);
      if (model.dataset.models !== models.join(",")) {
        model.replaceChildren(...models.map((m) => h("option", { value: m }, m.split("/").pop())));
        model.dataset.models = models.join(",");
      }
      model.value = current;
      model.hidden = models.length === 0;
    }
  }

  async function submit() {
    if (store.busy) {
      await store.stop();
      return;
    }
    const text = input.value.trim();
    if (!text || store.connection !== "live") return;
    const found = findKey(text);
    if (found && found.value !== allowKey) {
      showKeyNote();
      return;
    }
    send.disabled = true;
    if (await store.send(text)) {
      input.value = "";
      allowKey = null;
      autosize();
      onSent();
    }
    update();
  }

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
  model.addEventListener("change", async () => {
    const settings = store.status?.settings;
    if (!settings) return;
    try {
      await store.api.updateSettings({ model: model.value, allow_actions: settings.allow_actions });
      store.refreshStatus();
    } catch (error) {
      store.notice(error.message);
    }
  });
  store.subscribe(update);
  update();
}
