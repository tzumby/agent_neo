// The pages besides the chat: the conversation list, the desktop (take
// over), schedules, keys and tokens, settings, and usage. Each loads fresh
// when it opens.

import { MAIN, chatHash, lastLine, sessionBadge, sessionTitle } from "./conversations.js";
import { busyButton, day, fill, h, icon, when } from "./dom.js";
import { isValidName, normalizedName } from "./keys.js";
import { deviceLabel, fromBase64Url, iosOutsideHomeScreen, ownSubscription, pushState, pushSupported } from "./push.js";
import { axisMoney, budgetShare, chartDays, compact, kindRows, money, niceCeiling, shortDate, todayIn } from "./usage.js";

// --- Conversations -------------------------------------------------------------------

/**
 * Every conversation, most recently updated first: its last message and what
 * it's doing. New, rename, archive and delete; the archived ones underneath.
 */
export function mountChats(store, view, { newConversation }) {
  const body = view.querySelector(".page-body");
  let open = null; // the row showing its actions
  let renaming = null; // the row being renamed
  let archivedOpen = false;
  let drawn = [];

  function render(force = false) {
    if (view.hidden) return;
    const state = [store.sessions, store.archived, store.currentId, store.sessionsSupported, store.runningLimit, open, renaming, archivedOpen];
    if (!force && state.every((value, i) => value === drawn[i])) return;
    // Typing a new name: not redrawn under the person's fingers.
    if (!force && body.querySelector(".chat-rename")?.contains(document.activeElement)) return;
    drawn = state;
    const add = h("button", { class: "button button-primary", type: "button" }, icon("plus"), "New conversation");
    add.addEventListener("click", () => busyButton(add, newConversation));
    const sessions = store.sessions;
    fill(body,
      h("div", { class: "page-head" },
        h("p", { class: "lede" }, `Each has its own history and browser window. Up to ${store.runningLimit || 3} work at once; memory, keys and settings are shared.`),
        add),
      !sessions.length && store.sessionsSupported !== true
        ? h("p", { class: "muted" }, "Loading conversations…")
        : h("ul", { class: "list chat-list" }, sessions.map((s) => row(s, false))),
      archivedSection());
  }

  function row(s, archived) {
    const title = sessionTitle(s);
    if (renaming === s.id) return renameRow(s);
    const current = s.id === store.currentId;
    const shown = open === s.id;
    const more = h("button", {
      class: "icon-button chat-more", type: "button", "aria-expanded": String(shown),
      "aria-label": `Rename, archive or delete “${title}”`,
    }, icon("more", "icon icon-dots"));
    more.addEventListener("click", () => {
      open = shown ? null : s.id;
      render(true);
    });
    const updated = Number.isFinite(s.updated) ? new Date(s.updated * 1000) : null;
    return h("li", { class: `chat-row${current ? " is-current" : ""}${shown ? " is-open" : ""}` },
      h("a", { class: "chat-link", href: chatHash(s.id), "aria-current": current ? "page" : null },
        h("span", { class: "chat-line" },
          h("span", { class: "chat-title" }, title),
          updated ? h("time", { class: "chat-when", datetime: updated.toISOString() }, day(updated)) : null),
        h("span", { class: "chat-line" },
          h("span", { class: "chat-last" }, lastLine(s) || "No messages yet"),
          chatBadge(sessionBadge(s)))),
      more,
      shown ? tools(s, archived, title) : null);
  }

  function tools(s, archived, title) {
    // agentd refuses both while a task works or waits on a card.
    const engaged = s.state === "working" || s.state === "waiting";
    const rename = h("button", { class: "button", type: "button" }, "Rename");
    rename.addEventListener("click", () => {
      renaming = s.id;
      open = null;
      render(true);
      body.querySelector(".chat-rename input")?.focus();
    });
    const archive = h("button", { class: "button", type: "button", disabled: !archived && engaged }, archived ? "Unarchive" : "Archive");
    archive.addEventListener("click", () => busyButton(archive, async () => {
      if (await store.updateSession(s.id, { archived: !archived })) {
        open = null;
        render(true);
      }
    }));
    const remove = s.id === MAIN ? null : h("button", { class: "button button-quiet", type: "button", disabled: engaged }, "Delete");
    remove?.addEventListener("click", () => busyButton(remove, async () => {
      if (!confirm(`Delete “${title}”? Its messages, and any waiting their turn, go for good.`)) return;
      if (await store.deleteSession(s.id)) {
        open = null;
        store.notice(`Deleted “${title}”.`);
        render(true);
      }
    }));
    return h("div", { class: "chat-tools" },
      engaged ? h("p", { class: "muted" }, "It's working or waiting on you. Archive or delete it once that's done.") : null,
      s.id === MAIN ? h("p", { class: "muted" }, "The first conversation can be archived, not deleted.") : null,
      h("div", { class: "card-actions card-actions-start" }, rename, archive, remove));
  }

  function renameRow(s) {
    const input = h("input", { class: "field", value: s.title || "", placeholder: "New conversation", "aria-label": "The conversation's name", maxlength: "120" });
    const cancel = h("button", { class: "button button-quiet", type: "button" }, "Cancel");
    const save = h("button", { class: "button button-primary", type: "submit" }, "Save");
    const form = h("form", { class: "chat-rename" }, input, h("div", { class: "card-actions" }, cancel, save));
    const done = () => {
      renaming = null;
      render(true);
    };
    cancel.addEventListener("click", done);
    input.addEventListener("keydown", (event) => {
      if (event.key === "Escape") done();
    });
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const title = input.value.trim();
      if (!title) return input.focus();
      busyButton(save, async () => {
        if (await store.updateSession(s.id, { title })) done();
      });
    });
    return h("li", { class: "chat-row is-renaming" }, form);
  }

  function archivedSection() {
    const archived = store.archived;
    const content = !archivedOpen ? null
      : archived === null ? h("p", { class: "muted" }, "Loading…")
      : archived.length ? h("ul", { class: "list chat-list" }, archived.map((s) => row(s, true)))
      : h("p", { class: "muted" }, "Nothing archived.");
    const details = h("details", { class: "archived", open: archivedOpen }, h("summary", null, "Archived"), content);
    details.addEventListener("toggle", () => {
      if (details.open === archivedOpen) return; // drawn open, not opened by the person
      archivedOpen = details.open;
      if (archivedOpen) store.loadArchived();
      render(true);
    });
    return details;
  }

  store.subscribe(() => render());
  return {
    enter() {
      render(true);
      store.refreshSessions();
      if (archivedOpen) store.loadArchived();
    },
  };
}

/** A spinner while it works; words for the rest. */
function chatBadge(badge) {
  if (!badge) return null;
  if (badge.tone === "working") {
    return h("span", { class: "chat-badge is-working", title: "Working" },
      h("span", { class: "spinner", "aria-hidden": "true" }), h("span", { class: "visually-hidden" }, "Working"));
  }
  return h("span", { class: `chat-badge is-${badge.tone}` }, badge.text);
}

// --- Desktop ---------------------------------------------------------------------

/** The desktop's screen (noVNC through tailnet-proxy), only while it's open. */
export function mountDesktop(store, view) {
  const frame = view.querySelector(".desk-frame");
  const bar = view.querySelector(".desk-bar");

  function renderBar() {
    const action = store.timeline.visible().find((item) => item.type === "action");
    bar.replaceChildren();
    if (!action) {
      bar.append(h("p", { class: "muted" }, "What you do here happens on your desktop. The agent carries on when you're back in the chat."));
      return;
    }
    const skip = h("button", { class: "button", type: "button" }, "Skip");
    const done = h("button", { class: "button button-primary", type: "button" }, "I'm done");
    skip.addEventListener("click", () => busyButton(skip, () => store.resolve(action, false)));
    done.addEventListener("click", () => busyButton(done, async () => {
      if (await store.resolve(action, true)) location.hash = chatHash(store.currentId);
    }));
    bar.append(h("p", { class: "desk-ask" }, icon("desktop"), h("span", null, action.reason)), h("div", { class: "card-actions" }, skip, done));
  }
  store.subscribe(() => !view.hidden && renderBar());

  return {
    enter() {
      renderBar();
      if (!frame.firstChild) {
        frame.append(h("iframe", {
          title: "Your desktop",
          src: "/viewer/vnc.html?autoconnect=1&resize=scale&reconnect=1&reconnect_delay=2000&path=websockify",
        }));
      }
    },
    leave() {
      frame.replaceChildren(); // the connection closes with it
    },
  };
}

// --- Schedules -----------------------------------------------------------------------

export function mountSchedules(store, view) {
  const body = view.querySelector(".page-body");

  async function load() {
    fill(body, h("p", { class: "muted" }, "Loading schedules…"));
    let schedules;
    try {
      schedules = await store.api.schedules();
    } catch (error) {
      fill(body, h("p", { class: "error" }, `Couldn't load the schedules: ${error.message}`));
      return;
    }
    if (!schedules.length) {
      fill(body,
        h("p", null, "No schedules yet."),
        h("p", { class: "muted" }, "Ask your agent in the chat, for example: “Send me a security report every weekday at 8:00.”"));
      return;
    }
    fill(body, h("ul", { class: "list" }, schedules.map(row)));
  }

  function row(s) {
    const pause = h("button", { class: "button", type: "button" }, s.paused ? "Resume" : "Pause");
    const cancel = h("button", { class: "button button-quiet", type: "button" }, "Cancel schedule");
    pause.addEventListener("click", () => busyButton(pause, async () => {
      await act(() => store.api.pauseSchedule(s.id, !s.paused));
    }));
    cancel.addEventListener("click", () => busyButton(cancel, async () => {
      if (confirm(`Cancel “${s.name}”? It won't run again.`)) await act(() => store.api.deleteSchedule(s.id));
    }));
    return h("li", { class: "list-row" },
      h("div", { class: "list-main" },
        h("p", { class: "strong" }, s.name, s.paused ? h("span", { class: "tag" }, "Paused") : null),
        h("p", null, s.description),
        h("p", { class: "muted" }, s.paused ? "Paused" : `Next run ${when(s.next_run * 1000)}`,
          s.last_run ? `. Last ran ${when(s.last_run * 1000)}` : ""),
        h("details", { class: "instructions" }, h("summary", null, "Instructions"), h("p", { class: "pre" }, s.prompt))),
      h("div", { class: "list-actions" }, pause, cancel));
  }

  async function act(work) {
    try {
      await work();
    } catch (error) {
      store.notice(error.message);
    }
    await load();
  }

  return { enter: load };
}

// --- Keys and tokens -----------------------------------------------------------------

const SUGGESTED = [
  { label: "GitHub token", name: "GITHUB_TOKEN" },
  { label: "Together AI API key", name: "TOGETHER_API_KEY" },
  { label: "Anthropic API key", name: "ANTHROPIC_API_KEY" },
];

export function mountKeys(store, view) {
  const body = view.querySelector(".page-body");
  let prefill = null;

  async function load() {
    let secrets = [];
    let problem = null;
    try {
      secrets = await store.api.secrets();
    } catch (error) {
      problem = error.message;
    }
    const names = new Set(secrets.map((s) => s.name));
    fill(body,
      h("p", { class: "lede" }, "Keys the agent uses for your services, kept only on your desktop. The agent uses them by name and never sees them, and this page can't show them again."),
      problem ? h("p", { class: "error" }, `Couldn't load the keys: ${problem}`) : null,
      secrets.length ? h("ul", { class: "list" }, secrets.map(row)) : h("p", { class: "muted" }, "No keys saved yet."),
      addForm(names));
  }

  function row(s) {
    const replace = h("button", { class: "button", type: "button" }, "Replace");
    const remove = h("button", { class: "button button-quiet", type: "button" }, "Delete");
    replace.addEventListener("click", () => {
      prefill = { name: s.name, label: s.label, replace: true };
      load().then(() => body.querySelector(".key-value")?.focus());
    });
    remove.addEventListener("click", () => busyButton(remove, async () => {
      if (!confirm(`Delete ${s.label || s.name}? Tasks that use $${s.name} will ask for it again.`)) return;
      try {
        await store.api.deleteSecret(s.name);
      } catch (error) {
        store.notice(error.message);
      }
      await load();
    }));
    return h("li", { class: "list-row" },
      h("div", { class: "list-main" },
        h("p", { class: "strong" }, s.label || s.name),
        h("p", null, h("code", { class: "md-chip" }, `$${s.name}`)),
        h("p", { class: "muted" }, s.last_used ? `Last used ${when(s.last_used)}` : "Not used yet")),
      h("div", { class: "list-actions" }, replace, remove));
  }

  function addForm(names) {
    const p = prefill || {};
    prefill = null;
    const label = h("input", { class: "field", value: p.label || "", placeholder: "GitHub token", "aria-label": "What it is" });
    const name = h("input", { class: "field mono", value: p.name || "", placeholder: "GITHUB_TOKEN", "aria-label": "Name" });
    const value = h("input", { class: "field key-value", type: "password", autocomplete: "off", spellcheck: "false", value: p.value || "", "aria-label": "The key" });
    const save = h("button", { class: "button button-primary", type: "submit" }, p.replace ? "Replace key" : "Save key");
    const problem = h("p", { class: "error", hidden: true });
    let named = Boolean(p.name);
    name.addEventListener("input", () => (named = true));
    label.addEventListener("input", () => {
      if (!named) name.value = normalizedName(label.value);
    });
    const chips = SUGGESTED.filter((s) => !names.has(s.name)).map((s) => {
      const chip = h("button", { class: "chip", type: "button" }, s.label);
      chip.addEventListener("click", () => {
        label.value = s.label;
        name.value = s.name;
        named = true;
        value.focus();
      });
      return chip;
    });
    const form = h("form", { class: "key-form" },
      h("h2", { class: "page-h2" }, p.replace ? `A new ${p.label || p.name}` : "Add a key"),
      chips.length > 0 && !p.replace ? h("div", { class: "chips" }, chips) : null,
      h("label", { class: "label" }, "What it is", label),
      h("label", { class: "label" }, "Name the agent uses", name),
      h("label", { class: "label" }, "The key", value),
      problem,
      h("div", { class: "card-actions" }, save));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const n = name.value.trim();
      if (!isValidName(n)) {
        problem.hidden = false;
        problem.textContent = "The name is capitals, digits and underscores, starting with a letter (and not a password).";
        return name.focus();
      }
      if (!value.value) return value.focus();
      busyButton(save, async () => {
        try {
          await store.api.saveSecret(n, value.value, label.value.trim() || n);
          value.value = "";
          store.notice(`Saved ${label.value.trim() || n}.`);
          await load();
        } catch (error) {
          problem.hidden = false;
          problem.textContent = `Couldn't save it: ${error.message}`;
        }
      });
    });
    return form;
  }

  return {
    enter: load,
    /** From the chat: a key someone pasted, to save instead of sending. */
    prefill(key) {
      prefill = key;
    },
  };
}

// --- Settings ----------------------------------------------------------------------

export function mountSettings(store, view, session) {
  const body = view.querySelector(".page-body");

  function load() {
    const status = store.status;
    if (!status) {
      fill(body, h("p", { class: "muted" }, "Connecting to your desktop…"));
      return;
    }
    const settings = status.settings || {};
    const autoApproveDescription = "Eligible shell commands and schedules can run without asking. Browser and app changes, saved keys, direct internet access, and changes to the proxy allowlist still require approval. So do commands flagged for possible data transfer after reading outside content.";
    const models = [...new Set([settings.model, ...(status.known_models || [])])].filter(Boolean);
    const model = h("select", { class: "field" }, models.map((m) => h("option", { value: m, selected: m === settings.model }, m)));
    const actions = h("input", { type: "checkbox", checked: settings.allow_actions === true });
    const autoApprove = h("input", {
      type: "checkbox", checked: settings.auto_approve === true,
      disabled: !settings.auto_approve && settings.auto_approve_version === undefined,
    });
    // The page isn't redrawn while it has focus, so each control is set back
    // to what the desktop has once it answers (a failed change shows as one).
    const save = async (update, sync) => {
      try {
        await update();
        store.notice("Settings saved.");
      } catch (error) {
        store.notice(error.message);
      }
      if (await store.reloadStatus()) sync(store.status.settings || {});
    };
    model.addEventListener("change", () => save(() => store.api.updateSettings({ model: model.value }),
      (now) => { if (now.model) model.value = now.model; }));
    actions.addEventListener("change", () => save(() => store.api.updateSettings({ allow_actions: actions.checked }),
      (now) => { actions.checked = now.allow_actions === true; }));
    autoApprove.addEventListener("change", async () => {
      const enabled = autoApprove.checked;
      if (enabled && !confirm(`Turn on Auto approve?\n\n${autoApproveDescription}`)) {
        autoApprove.checked = false;
        return;
      }
      autoApprove.disabled = true;
      const { on, error } = await store.setAutoApprove(enabled);
      autoApprove.checked = on;
      autoApprove.disabled = false;
      store.notice(error ? error.message : `Auto approve is ${on ? "on" : "off"}.`);
    });
    let current = "light";
    try {
      current = localStorage.getItem("theme") || "light";
    } catch {}
    const theme = h("select", { class: "field" },
      [["light", "Light"], ["dark", "Dark"], ["system", "Match this device"]].map(([value, label]) =>
        h("option", { value, selected: value === current }, label)));
    theme.addEventListener("change", () => {
      try {
        localStorage.setItem("theme", theme.value);
      } catch {}
      window.applyTheme?.();
    });
    fill(body,
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "Agent"),
        h("label", { class: "label" }, "Model", model),
        h("label", { class: "toggle" }, actions,
          h("span", null, h("span", { class: "strong" }, "Browser actions"),
            h("span", { class: "muted block" }, "Lets the agent ask before typing, clicking, or pressing a key in your desktop's browser and apps."))),
        h("label", { class: "toggle" }, autoApprove,
          h("span", null, h("span", { class: "strong" }, "Auto approve"),
            h("span", { class: "muted block" }, autoApproveDescription)))),
      usageLink(store),
      pushSection(store),
      sitesSection(store),
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "Appearance"),
        h("label", { class: "label" }, "Theme", theme),
        h("p", { class: "muted" }, "Kept in this browser only.")),
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "Desktop"),
        h("dl", { class: "facts" },
          h("dt", null, "Name"), h("dd", null, status.desktop || ""),
          h("dt", null, "Agent version"), h("dd", null, status.version || ""))),
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "You"),
        h("dl", { class: "facts" }, h("dt", null, "Signed in as"), h("dd", null, session.email || "")),
        h("p", { class: "muted" }, "Signed in by this device's Tailscale. To sign out, log out of Tailscale.")));
  }

  return { enter: load };
}

/** Settings → Usage: this month so far, and the way to the page. Not on a desktop from before usage. */
function usageLink(store) {
  if (store.sessionsSupported === false) return null;
  const budget = store.status?.budget;
  const share = budgetShare(budget);
  const said = share ? `${money(budget.spent)} of your ${money(budget.monthly_usd)} monthly budget used this month.`
    : budget ? `${money(budget.spent)} this month. No monthly budget set.`
    : "What the model calls cost, by day, kind, model and conversation.";
  return h("section", { class: "settings-group" },
    h("h2", { class: "page-h2" }, "Usage"),
    h("p", { class: share?.tone === "over" ? "budget-said is-over" : "muted" }, said),
    share ? meter(share) : null,
    h("a", { class: "button", href: "#/usage" }, icon("chart"), "Usage and budget"));
}

/** How much of the budget is used: dark, orange from 80%, red once it's used up. */
function meter(share) {
  const used = h("span", { class: "meter-fill" });
  used.style.width = `${share.fill * 100}%`; // CSSOM: the page's policy allows no style attributes
  return h("div", {
    class: `meter is-${share.tone}`, role: "progressbar", "aria-label": "Budget used this month",
    "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": String(Math.min(share.percent, 100)),
  }, used);
}

// --- Usage -------------------------------------------------------------------------

/**
 * What the model calls cost: the totals, the budget (set or remove it), the
 * last 30 days as a chart, and this month by kind, model, conversation and
 * schedule. Estimates from token counts, and it says so.
 */
export function mountUsage(store, view) {
  const body = view.querySelector(".page-body");
  let usage = null;

  async function load() {
    // Loaded before: kept on screen, dimmed, until the new figures are in.
    if (usage) body.classList.add("is-refreshing");
    else fill(body, back(), h("p", { class: "muted" }, "Loading usage…"));
    try {
      usage = await store.api.usage(30);
    } catch (error) {
      fill(body, back(), h("p", { class: "error" }, `Couldn't load usage: ${error.message}`));
      return;
    } finally {
      body.classList.remove("is-refreshing");
    }
    fill(body,
      back(),
      h("p", { class: "lede" }, "What your agent's model calls cost. These are estimates from token counts at list prices, so your provider's bill may differ a little."),
      totals(usage),
      budgetSection(store, usage.budget, load),
      daysSection(usage),
      breakdowns(usage));
  }

  return { enter: load };
}

function back() {
  return h("a", { class: "back-link", href: "#/settings" }, icon("back"), "Settings");
}

function totals(u) {
  const tile = (label, value) => h("div", { class: "stat" },
    h("p", { class: "stat-label" }, label), h("p", { class: "stat-value" }, money(value)));
  return h("section", { class: "stats", "aria-label": "Totals" },
    tile("Today", u.today), tile("This month", u.this_month), tile("Last month", u.last_month), tile("All time", u.all_time));
}

function budgetSection(store, budget, reload) {
  const share = budgetShare(budget);
  const input = h("input", {
    class: "field", type: "number", inputmode: "decimal", min: "0.01", max: "100000", step: "0.01",
    value: budget?.monthly_usd ?? "", placeholder: "50", "aria-label": "Budget per month, in US dollars",
  });
  const save = h("button", { class: "button button-primary", type: "submit" }, share ? "Save" : "Set budget");
  const remove = share ? h("button", { class: "button button-quiet", type: "button" }, "Remove budget") : null;
  const problem = h("p", { class: "error", hidden: true });
  const fail = (text) => {
    problem.hidden = false;
    problem.textContent = text;
  };
  const change = (value, said) => async () => {
    try {
      await store.api.updateSettings({ monthly_budget_usd: value });
    } catch (error) {
      fail(`Couldn't change it: ${error.message}`);
      return;
    }
    store.notice(said);
    await store.reloadStatus();
    await reload();
  };
  const form = h("form", { class: "budget-form" },
    h("label", { class: "label" }, "Budget per month",
      h("span", { class: "money-field" }, h("span", { class: "money-sign", "aria-hidden": "true" }, "$"), input)),
    problem,
    h("div", { class: "card-actions card-actions-start" }, save, remove));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const value = Math.round(Number(input.value) * 100) / 100;
    if (!(value > 0 && value <= 100_000)) {
      fail("A budget is an amount from $0.01 to $100,000.");
      return input.focus();
    }
    busyButton(save, change(value, `Budget set to ${money(value)} a month.`));
  });
  remove?.addEventListener("click", () => busyButton(remove, change(null, "Budget removed.")));
  const state = share
    ? [
        h("p", { class: "budget-figures" },
          h("span", { class: "strong" }, `${money(budget.spent)} of ${money(budget.monthly_usd)}`),
          h("span", { class: "muted" }, `${share.percent}%`)),
        meter(share),
        share.tone === "over"
          ? h("p", { class: "budget-said is-over" }, icon("warning"), h("span", null, "Used up. New messages and scheduled runs wait until you raise it or the month turns over."))
          : h("p", { class: "muted" }, "You get a notification at 80% and at 100%. At 100%, new tasks wait until you raise it or the month turns over."),
      ]
    : h("p", { class: "muted" }, "No monthly budget. With one, you get a notification at 80%, and new tasks wait once it's used up.");
  return h("section", { class: "settings-group usage-group" }, h("h2", { class: "page-h2" }, "Monthly budget"), state, form);
}

/** The last 30 days, a column each, zeros included. Pointing at a day (or the arrow keys) reads it out. */
function daysSection(u) {
  const today = todayIn(u.timezone);
  const days = chartDays(u.daily, { count: 30, today });
  if (!days.length) return null;
  const top = niceCeiling(Math.max(0, ...days.map((d) => d.cost)));
  const total = days.reduce((sum, d) => sum + d.cost, 0);
  const readout = h("p", { class: "chart-readout", "aria-live": "polite" });
  const columns = days.map((d) => {
    const mark = h("span", { class: "bar-mark" });
    if (d.cost > 0) mark.style.height = `${(d.cost / top) * 100}%`; // a zero day is a flat stroke
    return h("span", { class: `bar${d.cost > 0 ? "" : " is-zero"}` }, mark);
  });
  const plot = h("div", {
    class: "bars", tabindex: "0", role: "group",
    "aria-label": `Cost per day, the last ${days.length} days: ${money(total)} in all. The arrow keys read each day.`,
  }, columns);
  let selected = days.length - 1;
  function select(index) {
    selected = Math.max(0, Math.min(days.length - 1, index));
    columns.forEach((column, i) => column.classList.toggle("is-selected", i === selected));
    const d = days[selected];
    fill(readout, h("strong", null, money(d.cost)), " ",
      h("span", { class: "muted" }, `${d.date === today ? "today" : shortDate(d.date)} · ${d.runs} ${d.runs === 1 ? "run" : "runs"}`));
  }
  const pick = (event) => {
    const rect = plot.getBoundingClientRect();
    select(Math.floor(((event.clientX - rect.left) / rect.width) * days.length));
  };
  plot.addEventListener("pointermove", pick);
  plot.addEventListener("pointerdown", pick);
  plot.addEventListener("pointerleave", (event) => {
    if (event.pointerType === "mouse") select(days.length - 1);
  });
  plot.addEventListener("keydown", (event) => {
    const to = { ArrowLeft: selected - 1, ArrowRight: selected + 1, Home: 0, End: days.length - 1 }[event.key];
    if (to === undefined) return;
    event.preventDefault();
    select(to);
  });
  select(selected);
  const tick = (value, at) => h("span", { class: `tick tick-${at}` }, h("span", { class: "tick-label" }, axisMoney(value)));
  const table = h("details", { class: "instructions" },
    h("summary", null, "Each day as a table"),
    h("div", { class: "md-table" }, h("table", null,
      h("thead", null, h("tr", null, h("th", null, "Day"), h("th", null, "Cost"), h("th", null, "Runs"))),
      h("tbody", null, [...days].reverse().map((d) =>
        h("tr", null, h("td", null, shortDate(d.date)), h("td", null, money(d.cost)), h("td", null, String(d.runs))))))));
  return h("section", { class: "settings-group usage-group" },
    h("h2", { class: "page-h2" }, `Last ${days.length} days`),
    readout,
    h("div", { class: "chart" },
      h("div", { class: "chart-plot" },
        h("div", { class: "chart-grid", "aria-hidden": "true" }, tick(top, "top"), tick(top / 2, "mid"), tick(0, "zero")),
        plot),
      h("div", { class: "chart-x", "aria-hidden": "true" },
        h("span", null, shortDate(days[0].date)), h("span", null, shortDate(days[Math.floor(days.length / 2)].date)), h("span", null, "Today"))),
    table);
}

/** This month, most expensive first: each row's share of its group's largest as a thin bar. */
function breakdowns(u) {
  const runs = (n) => `${n ?? 0} ${n === 1 ? "run" : "runs"}`;
  const group = (title, items, empty) => {
    const top = Math.max(0, ...items.map((item) => item.cost));
    return h("div", { class: "breakdown" },
      h("h3", { class: "breakdown-title" }, title),
      items.length
        ? h("ul", { class: "breakdown-list" }, items.map((item) => {
            const share = h("span", { class: "share-fill" });
            share.style.width = `${top > 0 ? (item.cost / top) * 100 : 0}%`;
            return h("li", { class: "breakdown-row" },
              h("span", { class: "breakdown-name", title: item.full || null }, item.href ? h("a", { href: item.href }, item.name) : item.name),
              h("span", { class: "breakdown-cost" }, money(item.cost)),
              h("span", { class: "share", "aria-hidden": "true" }, share),
              item.detail ? h("span", { class: "breakdown-detail" }, item.detail) : null);
          }))
        : h("p", { class: "muted" }, empty));
  };
  const list = (value) => (Array.isArray(value) ? value : []);
  const kinds = kindRows(u.by_kind).filter((k) => k.cost > 0);
  const models = list(u.by_model).map((m) => {
    const tokens = [`${compact(m.input_tokens)} in`, `${compact(m.output_tokens)} out`];
    if (m.cached_tokens > 0) tokens.push(`${compact(m.cached_tokens)} cached`);
    return { name: String(m.model || "").split("/").pop() || "Unknown model", full: m.model, cost: m.cost ?? 0, detail: `${runs(m.runs)} · ${tokens.join(" · ")} tokens` };
  });
  const sessions = list(u.by_session).map((s) => ({ name: sessionTitle(s), href: chatHash(s.id), cost: s.cost ?? 0, detail: runs(s.runs) }));
  const schedules = list(u.by_schedule).map((s) => ({ name: s.name || "A deleted schedule", cost: s.cost ?? 0, detail: runs(s.runs) }));
  return h("section", { class: "settings-group usage-group" },
    h("h2", { class: "page-h2" }, "This month"),
    h("div", { class: "breakdowns" },
      group("By kind", kinds, "Nothing yet this month."),
      group("By model", models, "No model calls yet this month."),
      group("By conversation", sessions, "No chats yet this month."),
      group("By schedule", schedules, "No scheduled runs this month.")));
}

// --- Notifications (Web Push) -----------------------------------------------------

const SW_SCOPE = "/assets/";

/** This browser's service worker registration, once it's active (subscribe needs that). */
async function activeRegistration() {
  const registration = (await navigator.serviceWorker.getRegistration(SW_SCOPE))
    || (await navigator.serviceWorker.register("/assets/sw.js", { scope: SW_SCOPE }));
  if (registration.active) return registration;
  const worker = registration.installing || registration.waiting;
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("the service worker didn't start")), 10000);
    worker?.addEventListener("statechange", () => {
      if (worker.state === "activated") {
        clearTimeout(timer);
        resolve();
      }
    });
  });
  return registration;
}

/** This browser's push subscription, if it has one. */
async function currentSubscription() {
  try {
    const registration = await navigator.serviceWorker.getRegistration(SW_SCOPE);
    return (await registration?.pushManager.getSubscription()) || null;
  } catch {
    return null;
  }
}

function pushSection(store) {
  const section = h("section", { class: "settings-group", "aria-label": "Notifications" });
  const heading = () => h("h2", { class: "page-h2" }, "Notifications");
  const what = () => h("p", { class: "muted" },
    "On the devices you turn them on for: approvals and other cards that need you, a task that looks stuck, Action needed, a long task finishing, and scheduled runs that found something.");
  const problem = h("p", { class: "error", hidden: true });
  const fail = (text) => {
    problem.hidden = false;
    problem.textContent = text;
  };

  async function render() {
    problem.hidden = true;
    let info;
    try {
      info = await store.api.push();
    } catch (error) {
      fill(section, heading(), h("p", { class: "error" }, `Couldn't load notifications: ${error.message}`));
      return;
    }
    const subscriptions = info.subscriptions || [];
    const supported = pushSupported(window);
    const mine = supported ? await currentSubscription() : null;
    const own = ownSubscription(subscriptions, mine?.endpoint);
    const state = pushState({
      supported,
      ios: iosOutsideHomeScreen(window),
      permission: supported ? Notification.permission : "default",
      subscribed: Boolean(own),
    });
    const others = [
      ...subscriptions.filter((s) => s !== own).map((s) => ({ ...s, remove: store.api.deletePush })),
      ...(info.devices || []).map((device) => ({ ...device, remove: store.api.deleteDevice })),
    ];
    fill(section, heading(), what(), thisDevice(state, own, info, mine), problem, othersList(others));
  }

  function thisDevice(state, own, info, mine) {
    if (state === "ios-home-screen") {
      return h("p", null, "On iPhone or iPad: tap Share → Add to Home Screen, open Agent Neo from there, then turn notifications on here.");
    }
    if (state === "unsupported") return h("p", null, "This browser can't get notifications from the web app.");
    if (state === "denied") {
      return h("p", null, "Notifications are blocked for this site. Allow them in your browser's site settings (and for the browser in your system's notification settings), then reload this page.");
    }
    if (state === "on") {
      const test = h("button", { class: "button", type: "button" }, "Send a test");
      const off = h("button", { class: "button button-quiet", type: "button" }, "Turn off");
      test.addEventListener("click", () => busyButton(test, async () => {
        try {
          await store.api.testPush(own.id);
          store.notice("Sent. It should arrive in a few seconds.");
        } catch (error) {
          fail(`Couldn't send a test: ${error.message}`);
        }
      }));
      off.addEventListener("click", () => busyButton(off, async () => {
        try {
          await mine?.unsubscribe().catch(() => {});
          await store.api.deletePush(own.id);
          store.notice("Notifications are off for this device.");
        } catch (error) {
          fail(`Couldn't turn them off: ${error.message}`);
          return;
        }
        await render();
      }));
      return h("div", { class: "card-actions card-actions-start" },
        h("p", { class: "card-text" }, `On for this device (${own.label || deviceLabel(navigator.userAgent, navigator.maxTouchPoints)}).`),
        test, off);
    }
    const on = h("button", { class: "button button-primary", type: "button" }, "Turn on notifications on this device");
    on.addEventListener("click", () => busyButton(on, async () => {
      try {
        // First, while the tap still counts as one: iOS asks only then.
        const permission = await Notification.requestPermission();
        if (permission !== "granted") {
          await render();
          return;
        }
        const registration = await activeRegistration();
        // One from before (another key, or the desktop no longer has it): start over.
        const stale = await registration.pushManager.getSubscription();
        if (stale) await stale.unsubscribe().catch(() => {});
        const subscription = await registration.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: fromBase64Url(info.public_key),
        });
        await store.api.subscribePush({
          ...subscription.toJSON(),
          label: deviceLabel(navigator.userAgent, navigator.maxTouchPoints),
        });
        store.notice("Notifications are on for this device.");
      } catch (error) {
        fail(`Couldn't turn them on: ${error.message}`);
        return;
      }
      await render();
    }));
    return h("div", { class: "card-actions card-actions-start" }, on);
  }

  function othersList(others) {
    if (!others.length) return null;
    return h("div", { class: "push-others" },
      h("p", { class: "strong" }, "Other devices"),
      h("ul", { class: "list" }, others.map((s) => {
        const remove = h("button", { class: "button button-quiet", type: "button" }, "Remove");
        remove.addEventListener("click", () => busyButton(remove, async () => {
          if (!confirm(`Stop notifying ${s.label || "that device"}?`)) return;
          try {
            await s.remove(s.id);
          } catch (error) {
            fail(`Couldn't remove it: ${error.message}`);
            return;
          }
          await render();
        }));
        const added = s.created ? `Added ${when(new Date(s.created * 1000))}` : "";
        const sent = s.last_sent ? `last notified ${when(new Date(s.last_sent * 1000))}` : "not notified yet";
        return h("li", { class: "list-row" },
          h("div", { class: "list-main" },
            h("p", null, s.label || "A device"),
            h("p", { class: "muted" }, [added, sent].filter(Boolean).join(", "))),
          h("div", { class: "list-actions" }, remove));
      })));
  }

  fill(section, heading(), h("p", { class: "muted" }, "Loading…"));
  render();
  return section;
}

/** "*.github.com" for the proxy's ".github.com": a domain and everything under it. */
function siteLabel(entry) {
  return entry.startsWith(".") ? `*${entry}` : entry;
}

// The sites the agent's commands can reach: the deployment's, and the ones
// you allowed (from a card in the chat, or here).
function sitesSection(store) {
  const section = h("section", { class: "settings-group" });
  const heading = () => h("h2", { class: "page-h2" }, "Sites your agent can reach");
  const intro = h("p", { class: "muted" },
    "Its commands reach the internet only through your desktop's proxy, which lets these sites through. When it needs another, it asks you in the chat.");

  async function render() {
    let access;
    try {
      access = await store.api.access();
    } catch (error) {
      fill(section, heading(), h("p", { class: "error" }, `Couldn't load the sites: ${error.message}`));
      return;
    }
    const base = access.base || [];
    const always = base.length
      ? h("details", { class: "instructions" },
          h("summary", null, `Always allowed by your deployment (${base.length})`),
          h("ul", { class: "card-hosts" }, base.map((entry) => h("li", null, h("code", null, siteLabel(entry))))))
      : null;
    if (!access.enabled) {
      fill(section, heading(), intro, h("p", { class: "muted" }, "On this desktop, the list is set by whoever deploys it."), always);
      return;
    }
    const allowed = access.allowed || [];
    const list = allowed.length
      ? h("ul", { class: "list" }, allowed.map(row))
      : h("p", { class: "muted" }, "You haven't allowed any others yet.");
    fill(section, heading(), intro, list, addForm(), always);
  }

  function row(site) {
    const remove = h("button", { class: "button button-quiet", type: "button" }, "Remove");
    remove.addEventListener("click", () => busyButton(remove, async () => {
      if (!confirm(`Remove ${site.host}? Your agent will have to ask again to reach it.`)) return;
      try {
        await store.api.removeSite(site.host);
        store.notice(`Removed ${site.host}.`);
      } catch (error) {
        store.notice(error.message);
      }
      await render();
    }));
    const added = when(new Date(site.added * 1000));
    return h("li", { class: "list-row" },
      h("div", { class: "list-main" },
        h("p", null, h("code", { class: "md-chip" }, site.host)),
        site.why ? h("p", null, site.why) : null,
        h("p", { class: "muted" }, site.by ? `Allowed ${added} by ${site.by}` : `Allowed ${added}`)),
      h("div", { class: "list-actions" }, remove));
  }

  function addForm() {
    const host = h("input", { class: "field mono", placeholder: "api.example.com", spellcheck: "false", autocapitalize: "off", "aria-label": "Site" });
    const add = h("button", { class: "button", type: "submit" }, "Allow site");
    const problem = h("p", { class: "error", hidden: true });
    const form = h("form", { class: "key-form" }, h("label", { class: "label" }, "Allow another site", host), problem, h("div", { class: "card-actions" }, add));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (!host.value.trim()) return host.focus();
      busyButton(add, async () => {
        try {
          const { host: allowed } = await store.api.allowSite(host.value.trim());
          store.notice(`Allowed ${allowed}.`);
          await render();
        } catch (error) {
          problem.hidden = false;
          problem.textContent = error.message;
        }
      });
    });
    return form;
  }

  fill(section, heading(), h("p", { class: "muted" }, "Loading…"));
  render();
  return section;
}
