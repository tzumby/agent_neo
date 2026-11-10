// The pages besides the chat: the desktop (take over), schedules, keys and
// tokens, and settings. Each loads fresh when it opens.

import { busyButton, fill, h, icon, when } from "./dom.js";
import { isValidName, normalizedName } from "./keys.js";

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
      if (await store.resolve(action, true)) location.hash = "#/chat";
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
    const models = [...new Set([settings.model, ...(status.known_models || [])])].filter(Boolean);
    const model = h("select", { class: "field" }, models.map((m) => h("option", { value: m, selected: m === settings.model }, m)));
    const actions = h("input", { type: "checkbox", checked: settings.allow_actions === true });
    const autoApprove = h("input", { type: "checkbox", checked: settings.auto_approve === true });
    const save = async () => {
      try {
        await store.api.updateSettings({ model: model.value, allow_actions: actions.checked, auto_approve: autoApprove.checked });
        store.refreshStatus();
        store.notice("Settings saved.");
      } catch (error) {
        store.notice(error.message);
      }
    };
    model.addEventListener("change", save);
    actions.addEventListener("change", save);
    autoApprove.addEventListener("change", () => {
      if (autoApprove.checked && !confirm("Turn on Auto approve? Nothing will wait for your OK, including commands as root and changes in your logged-in browser, even if a page or log has fooled the agent.")) {
        autoApprove.checked = false;
        return;
      }
      save();
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
            h("span", { class: "muted block" }, "Lets the agent click, type and press keys in your desktop's browser. It still asks you before anything that sends or changes things. Off, it only reads pages."))),
        h("label", { class: "toggle" }, autoApprove,
          h("span", null, h("span", { class: "strong" }, "Auto approve"),
            h("span", { class: "muted block" }, "Nothing waits for your OK, in chats or scheduled runs. Commands as root, direct connections, browser changes, new sites and schedules all go ahead, even if a page or log has fooled the agent. Keys still come from you, and what the agent is never allowed to do stays refused.")))),
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
