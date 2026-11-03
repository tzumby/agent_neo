// The pages besides the chat: the desktop (take over), schedules, keys and
// tokens, and settings. Each loads fresh when it opens.

import { busyButton, fill, h, icon, when } from "./dom.js";
import { isValidName, normalizedName } from "./keys.js";

// --- Desktop ---------------------------------------------------------------------

/** The desktop's screen (noVNC through front-door), only while it's open. */
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
  { label: "Cloudflare API token", name: "CLOUDFLARE_API_TOKEN" },
  { label: "Datadog API key", name: "DD_API_KEY" },
  { label: "Datadog application key", name: "DD_APP_KEY" },
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
    const label = h("input", { class: "field", value: p.label || "", placeholder: "Cloudflare API token", "aria-label": "What it is" });
    const name = h("input", { class: "field mono", value: p.name || "", placeholder: "CLOUDFLARE_API_TOKEN", "aria-label": "Name" });
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
    const save = async () => {
      try {
        await store.api.updateSettings({ model: model.value, allow_actions: actions.checked });
        store.refreshStatus();
        store.notice("Settings saved.");
      } catch (error) {
        store.notice(error.message);
      }
    };
    model.addEventListener("change", save);
    actions.addEventListener("change", save);
    fill(body,
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "Agent"),
        h("label", { class: "label" }, "Model", model),
        h("label", { class: "toggle" }, actions,
          h("span", null, h("span", { class: "strong" }, "Browser actions"),
            h("span", { class: "muted block" }, "Lets the agent click, type and press keys in your desktop's browser. It still asks you before anything that sends or changes things. Off, it only reads pages.")))),
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "Desktop"),
        h("dl", { class: "facts" },
          h("dt", null, "Name"), h("dd", null, status.desktop || ""),
          h("dt", null, "Agent version"), h("dd", null, status.version || ""))),
      h("section", { class: "settings-group" },
        h("h2", { class: "page-h2" }, "You"),
        h("dl", { class: "facts" }, h("dt", null, "Signed in as"), h("dd", null, session.email || "")),
        session.sign_out
          ? h("a", { class: "button", href: session.sign_out }, "Sign out")
          : h("p", { class: "muted" }, "Signed in by this device's Tailscale. To sign out, log out of Tailscale.")));
  }

  return { enter: load };
}
