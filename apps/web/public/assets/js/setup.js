// Setup: Neo Agent's first run. Four cards: where updates go
// (Slack or Teams), Cloudflare, Datadog, and what it watches (scheduled
// checks to add with one choice). Shown in an empty chat the first time
// someone connects, and as the Setup page after that. Each card is done when
// the desktop has what it needs: a destination, the keys, a schedule.

import { busyButton, fill, h, icon } from "./dom.js";

const CLOUDFLARE_KEY = "CLOUDFLARE_API_TOKEN";
const DATADOG_KEYS = ["DD_API_KEY", "DD_APP_KEY"];

const DATADOG_SITES = [
  ["datadoghq.com", "US1 (app.datadoghq.com)"],
  ["us3.datadoghq.com", "US3 (us3.datadoghq.com)"],
  ["us5.datadoghq.com", "US5 (us5.datadoghq.com)"],
  ["datadoghq.eu", "EU (app.datadoghq.eu)"],
  ["ap1.datadoghq.com", "AP1, Japan (ap1.datadoghq.com)"],
  ["ap2.datadoghq.com", "AP2, Australia (ap2.datadoghq.com)"],
];

/** Cloudflare's read-only permissions, as its token page names them. */
const CLOUDFLARE_PERMISSIONS = [
  ["Account", "Account Analytics", "account_analytics"],
  ["Zone", "Zone", "zone"],
  ["Zone", "Analytics", "analytics"],
  ["Zone", "Firewall Services", "firewall_services"],
];
// Cloudflare's token template link: the token page with these filled in.
const CLOUDFLARE_TOKEN_URL = "https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys="
  + encodeURIComponent(JSON.stringify(CLOUDFLARE_PERMISSIONS.map(([, , key]) => ({ key, type: "read" }))))
  + "&accountId=*&zoneId=all&name=" + encodeURIComponent("Neo Agent");

/** The application key's scopes: reading only. */
const DATADOG_SCOPES = [
  ["logs_read_data", "read logs"],
  ["logs_read_index_data", "read logs from every index"],
  ["metrics_read", "read metrics"],
  ["monitors_read", "read monitors"],
  ["events_read", "read events"],
];

/** The site's web app, where its settings pages are: US1 and EU differ from their site name. */
function datadogApp(site) {
  return site === "datadoghq.com" ? "app.datadoghq.com" : site === "datadoghq.eu" ? "app.datadoghq.eu" : site;
}

/** Example checks. `needs`: the sources they read. Where they post isn't
 * named: each run sees the channels set up by then, so the order of the steps
 * doesn't matter. */
function examples(datadog) {
  const postIt = " If a channel is set up for updates, post the report there with post_update: severity warning if something needs action, info otherwise.";
  const alert = " post a short alert with post_update to the updates channel (if none is set up, say it here): what happened, from where, and what you'd do. If nothing stands out, post nothing and answer in one line.";
  return [
    {
      name: "Morning security report",
      cron: "0 8 * * 1-5",
      when: "Weekdays at 8:00",
      needs: ["cloudflare"],
      prompt: "Write this morning's security report for the last 24 hours (use the security-monitoring skill). Cloudflare: security events by action, source, country, ASN and targeted path, against the 24 hours before."
        + (datadog ? " Datadog: error and 5xx log counts by service, against the day before." : "")
        + " Lead with anything that needs action." + postIt,
    },
    {
      name: "Attack spike watch",
      cron: "0 * * * *",
      when: "Every hour",
      needs: ["cloudflare"],
      prompt: "Check Cloudflare security events for the last hour (use the security-monitoring skill), against the hour before and the same hour yesterday. If blocked requests jumped, a new attack pattern appeared, or something got through," + alert,
    },
    {
      name: "Error spike watch",
      cron: "*/30 * * * *",
      when: "Every 30 minutes",
      needs: ["datadog"],
      prompt: "Check Datadog for error and 5xx logs in the last 30 minutes by service (use the security-monitoring skill), against the 30 minutes before. If a service's errors more than doubled, a new error appeared, or they look like probing (odd paths, bursts of 401s or 404s from one source)," + alert,
    },
    {
      name: "Weekly security summary",
      cron: "0 9 * * 1",
      when: "Mondays at 9:00",
      needs: ["cloudflare"],
      prompt: "Summarize last week's security (use the security-monitoring skill): Cloudflare security events by day, the top attack sources and targeted paths"
        + (datadog ? ", Datadog errors by service" : "")
        + ", what changed from the week before, and the three things you'd fix first." + postIt,
    },
  ];
}

export function mountSetup(store, el, { openUpdates, openChat, intro = false, onProgress = () => {}, onComplete = () => {} }) {
  // The first-run guide ends on "All set" once someone has gone through it;
  // a desktop set up before goes straight to the chat.
  let wasIncomplete = false;
  let finished = false;
  let state = { destinations: [], keys: new Set(), schedules: [], skipped: new Set() };
  // A step someone opened by hand (-1: none); null: the next one to do.
  let open = null;

  async function load() {
    try {
      const [destinations, secrets, schedules, setup] = await Promise.all([
        store.api.destinations().catch(() => []),
        store.api.secrets(),
        store.api.schedules(),
        store.api.setup().catch(() => ({ skipped: [] })),
      ]);
      state = { destinations, keys: new Set(secrets.map((s) => s.name)), schedules, skipped: new Set(setup.skipped) };
    } catch (error) {
      if (intro) onComplete(); // the chat, without the guide
      else fill(el, h("p", { class: "error" }, `Couldn't load your setup: ${error.message}`));
      return;
    }
    render();
  }

  const has = {
    updates: () => state.destinations.length > 0,
    cloudflare: () => state.keys.has(CLOUDFLARE_KEY),
    datadog: () => DATADOG_KEYS.every((k) => state.keys.has(k)),
    watching: () => state.schedules.length > 0,
  };

  const STEPS = [
    { id: "updates", title: "Choose where updates go", body: () => updatesCard(),
      doneText: () => state.destinations.map((d) => (d.kind === "slack" ? `Slack ${d.channel}` : "Teams")).join(", ") },
    { id: "cloudflare", title: "Connect Cloudflare", body: () => cloudflareCard(), doneText: () => "Token saved" },
    { id: "datadog", title: "Connect Datadog", body: () => datadogCard(), doneText: () => "Keys saved" },
    { id: "watching", title: "Choose what it watches", body: () => watchCard(),
      doneText: () => (state.schedules.length === 1 ? "1 check" : `${state.schedules.length} checks`) },
  ];

  function steps() {
    return STEPS.map((s) => {
      const done = has[s.id]();
      return { ...s, done, skipped: !done && state.skipped.has(s.id) };
    });
  }

  function render() {
    const all = steps();
    const handled = all.filter((s) => s.done || s.skipped).length;
    onProgress(handled, all.length);
    if (handled < all.length) wasIncomplete = true;
    if (intro && handled === all.length) {
      if (!wasIncomplete || finished) {
        onComplete();
        return;
      }
      const start = h("button", { class: "button button-primary", type: "button" }, "Start using your agent");
      start.addEventListener("click", () => {
        finished = true;
        onComplete();
        openChat();
      });
      fill(el,
        h("div", { class: "setup-intro" },
          h("h2", null, "All set"),
          h("p", { class: "muted" }, "Your agent is ready. You can change any of this under Setup.")),
        h("ul", { class: "setup-summary" }, all.map((s) =>
          h("li", null, h("span", { class: s.done ? "setup-ok" : "muted" }, s.done ? "✓" : "–"), " ", s.title, h("span", { class: "muted" }, `: ${s.done ? s.doneText() : "skipped"}`)))),
        h("div", { class: "card-actions card-actions-start" }, start));
      return;
    }
    const next = all.findIndex((s) => !s.done && !s.skipped);
    const shown = open ?? next;
    fill(el,
      intro
        ? h("div", { class: "setup-intro" },
            h("h2", null, "Let's set up Neo Agent"),
            h("p", { class: "muted" }, "Connect what it watches and where it reports, one step at a time. Skip what you don't use; you can come back to any step under Setup."))
        : h("p", { class: "lede" }, handled === all.length
            ? "All set. Your agent reads your Cloudflare and Datadog with read-only keys kept on its desktop, checks them on a schedule, and posts what it finds. Open a step to change it."
            : `${handled} of ${all.length} done. Your agent reads your Cloudflare and Datadog with read-only keys kept on its desktop, checks them on a schedule, and posts what it finds.`),
      h("ol", { class: "setup-steps" }, all.map((s, i) => stepItem(s, i, i === shown, i === next, all))));
  }

  function stepItem(s, i, isOpen, isNext, all) {
    const status = s.done ? s.doneText() : s.skipped ? "Skipped" : isNext ? "Next" : "";
    const head = h("button", { class: "setup-head", type: "button", "aria-expanded": String(isOpen) },
      h("span", { class: "card-title" }, s.title),
      h("span", { class: "setup-status" }, status));
    head.addEventListener("click", () => {
      open = isOpen ? -1 : i;
      render();
    });
    const classes = ["setup-step", s.done && "is-done", s.skipped && "is-skipped", isOpen && "is-open"].filter(Boolean).join(" ");
    return h("li", { class: classes },
      h("div", { class: "setup-mark", "aria-hidden": "true" }, s.done ? "✓" : s.skipped ? "–" : ""),
      h("section", { class: "card setup-card", "aria-label": s.title },
        head,
        isOpen ? h("div", { class: "setup-body" }, s.body()) : null,
        isOpen ? foot(s, i, all) : null));
  }

  /** Under the open step: skip it, keep it skipped, or go on to the next one. */
  function foot(s, i, all) {
    const later = all.slice(i + 1).some((t) => !t.done && !t.skipped);
    if (s.done) {
      if (s.id === "watching" || !later) return null;
      const next = h("button", { class: "button button-primary", type: "button" }, "Next step");
      next.addEventListener("click", () => {
        open = null;
        render();
      });
      return h("div", { class: "setup-foot" }, next);
    }
    if (s.skipped) {
      const keep = h("button", { class: "link-button", type: "button" }, "Keep it skipped");
      keep.addEventListener("click", () => {
        open = null;
        render();
      });
      return h("div", { class: "setup-foot" }, h("span", { class: "muted" }, "Skipped for now."), keep);
    }
    const skip = h("button", { class: "link-button", type: "button" }, "Skip this step");
    skip.addEventListener("click", () => busyButton(skip, async () => {
      try {
        await store.api.saveSetup([...state.skipped, s.id]);
      } catch (error) {
        store.notice(`Couldn't skip it: ${error.message}`);
        return;
      }
      await advance();
    }));
    return h("div", { class: "setup-foot" }, skip);
  }

  /** A step is done: on to the next one to do. */
  async function advance() {
    open = null;
    await load();
  }

  function saveKey(name, value, label) {
    return store.api.saveSecret(name, value, label);
  }

  // --- 1. Slack or Teams --------------------------------------------------------

  function updatesCard() {
    if (has.updates()) {
      const where = state.destinations.map((d) => (d.kind === "slack" ? `Slack ${d.channel}` : "a Teams channel")).join(", ");
      return h("p", { class: "card-text" }, `Updates go to ${where}. `, h("a", { href: "#/updates" }, "Manage"));
    }
    const slack = h("button", { class: "button button-primary", type: "button" }, "Set up Slack");
    const teams = h("button", { class: "button", type: "button" }, "Set up Microsoft Teams");
    slack.addEventListener("click", () => openUpdates("slack", advance));
    teams.addEventListener("click", () => openUpdates("teams", advance));
    return [
      h("p", { class: "card-text" }, "A channel where your agent posts its reports and alerts. It can post only where you add."),
      h("div", { class: "card-actions card-actions-start" }, slack, teams),
    ];
  }

  // --- 2. Cloudflare -------------------------------------------------------------

  function cloudflareCard() {
    if (has.cloudflare()) return checkButton("Check that my Cloudflare token works: list the zones it can see, and how many security events each had in the last 24 hours.");
    const token = h("input", { class: "field", type: "password", autocomplete: "off", spellcheck: "false", "aria-label": "Cloudflare API token" });
    const save = h("button", { class: "button button-primary", type: "submit" }, "Save the token");
    const problem = h("p", { class: "error", hidden: true });
    const form = h("form", { class: "secret-form" },
      h("ol", { class: "steps" },
        h("li", null,
          linkButton(CLOUDFLARE_TOKEN_URL, "Create the token in Cloudflare"),
          h("span", { class: "block muted" }, "Cloudflare opens a new token named Neo Agent with these permissions filled in. Check they're all there:"),
          copyList(CLOUDFLARE_PERMISSIONS.map(([scope, name]) => ({ text: name, note: `${scope}, Read` })))),
        h("li", null, "Under Zone Resources, keep all zones or pick the sites to watch. Choose Continue to summary, then Create Token."),
        h("li", null, "Copy the token Cloudflare shows (it's shown once) and paste it here.")),
      h("p", { class: "muted" }, "If the page opens without the permissions: choose Create Token, then Custom token, and add the four above."),
      h("label", { class: "label" }, "API token", token),
      problem,
      h("p", { class: "card-note" }, icon("lock"), "Read-only: your agent can see your security events but can't change anything in Cloudflare."),
      h("div", { class: "card-actions" }, save));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (!token.value.trim()) return token.focus();
      busyButton(save, async () => {
        try {
          await saveKey(CLOUDFLARE_KEY, token.value.trim(), "Cloudflare API token (read-only)");
          token.value = "";
          await advance();
        } catch (error) {
          problem.hidden = false;
          problem.textContent = `Couldn't save it: ${error.message}`;
        }
      });
    });
    return form;
  }

  // --- 3. Datadog ----------------------------------------------------------------

  function datadogCard() {
    if (has.datadog()) return checkButton("Check that my Datadog keys work, then tell me which services logged errors in the last 24 hours.");
    const site = h("select", { class: "field", "aria-label": "Datadog site" },
      DATADOG_SITES.map(([value, label]) => h("option", { value }, label)));
    const apiKey = h("input", { class: "field", type: "password", autocomplete: "off", spellcheck: "false", "aria-label": "Datadog API key" });
    const appKey = h("input", { class: "field", type: "password", autocomplete: "off", spellcheck: "false", "aria-label": "Datadog application key" });
    const save = h("button", { class: "button button-primary", type: "submit" }, "Save the keys");
    const problem = h("p", { class: "error", hidden: true });
    // Datadog's own pages for this site's keys: they follow the site chosen.
    const apiPage = linkButton("", "Open API keys");
    const appPage = linkButton("", "Open application keys");
    const personal = h("a", { target: "_blank", rel: "noopener noreferrer" }, "Personal Settings");
    const point = () => {
      const app = datadogApp(site.value);
      apiPage.href = `https://${app}/organization-settings/api-keys`;
      appPage.href = `https://${app}/organization-settings/application-keys`;
      personal.href = `https://${app}/personal-settings/application-keys`;
    };
    site.addEventListener("change", point);
    point();
    const form = h("form", { class: "secret-form" },
      h("ol", { class: "steps" },
        h("li", null, "Pick your Datadog site: it's the address you sign in at.", h("label", { class: "label setup-inline" }, "Site", site)),
        h("li", null,
          apiPage,
          h("span", { class: "block muted" }, "Choose New Key, name it Neo Agent, and copy the key."),
          h("label", { class: "label setup-inline" }, "API key", apiKey)),
        h("li", null,
          appPage,
          h("span", { class: "block muted" }, "Choose New Key, name it Neo Agent, and copy the key right away: Datadog shows it once. Then open the key, choose Edit Scopes, and add these (search for each):"),
          copyList(DATADOG_SCOPES.map(([scope, what]) => ({ text: scope, note: what }))),
          h("label", { class: "label setup-inline" }, "Application key", appKey))),
      h("p", { class: "muted" }, "Those pages need Datadog admin rights. Without them, ask an admin for the API key, and create the application key under your ", personal, " instead."),
      problem,
      h("p", { class: "card-note" }, icon("lock"), "Read-only scopes: your agent can read logs, metrics and monitors but can't change anything in Datadog."),
      h("div", { class: "card-actions" }, save));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (!apiKey.value.trim()) return apiKey.focus();
      if (!appKey.value.trim()) return appKey.focus();
      busyButton(save, async () => {
        try {
          // The site goes in the labels: the agent sees labels, never values.
          await saveKey("DD_API_KEY", apiKey.value.trim(), `Datadog API key (site ${site.value})`);
          await saveKey("DD_APP_KEY", appKey.value.trim(), `Datadog application key (site ${site.value})`);
          apiKey.value = "";
          appKey.value = "";
          await advance();
        } catch (error) {
          problem.hidden = false;
          problem.textContent = `Couldn't save them: ${error.message}`;
        }
      });
    });
    return form;
  }

  // --- 4. Scheduled checks -------------------------------------------------------

  function watchCard() {
    const destination = state.destinations[0];
    const post = destination ? (destination.kind === "slack" ? `Slack ${destination.channel}` : "your Teams channel") : "";
    const sources = { cloudflare: has.cloudflare(), datadog: has.datadog() };
    const names = new Set(state.schedules.map((s) => s.name));
    const list = examples(sources.datadog).map((example) => {
      const added = names.has(example.name);
      const missing = example.needs.filter((source) => !sources[source]);
      const add = h("button", { class: "button", type: "button", disabled: added || missing.length > 0 }, added ? "Added" : "Add");
      // Off unless the person ticks it; changeable later under Schedules.
      let runWithoutAsking = false;
      const auto = added ? null : autoApprove(false, (on) => { runWithoutAsking = on; });
      add.addEventListener("click", () => busyButton(add, async () => {
        try {
          await store.api.addSchedule({
            name: example.name,
            prompt: example.prompt,
            cron: example.cron,
            timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
            utc_offset: -new Date().getTimezoneOffset() * 60,
            auto_approve: runWithoutAsking,
          });
          open = STEPS.findIndex((t) => t.id === "watching");
          await load();
        } catch (error) {
          store.notice(`Couldn't add it: ${error.message}`);
        }
      }));
      const note = added ? null
        : missing.length ? h("p", { class: "muted" }, `Connect ${missing.map((m) => (m === "cloudflare" ? "Cloudflare" : "Datadog")).join(" and ")} first.`)
        : null;
      return h("li", { class: "list-row" },
        h("div", { class: "list-main" },
          h("p", { class: "strong" }, example.name),
          h("p", { class: "muted" }, example.when),
          note,
          missing.length ? null : auto,
          h("details", { class: "instructions" }, h("summary", null, "What it does"), h("p", null, example.prompt))),
        h("div", { class: "list-actions" }, add));
    });
    if (!sources.cloudflare && !sources.datadog) {
      return h("p", { class: "card-text" }, "These checks read Cloudflare or Datadog: connect one of them first, or skip this step and ask your agent for checks in the chat.");
    }
    return [
      h("p", { class: "card-text" }, post
        ? `Checks your agent runs on its own, posting what it finds to ${post}. Each run costs a little in model calls; you can pause or cancel them under Schedules.`
        : "Checks your agent runs on its own, reporting in the chat (and in Slack or Teams once you add a channel). You can pause or cancel them under Schedules."),
      h("ul", { class: "list setup-examples" }, list),
    ];
  }

  function checkButton(question) {
    const ask = h("button", { class: "button", type: "button" }, "Ask your agent to check it");
    ask.addEventListener("click", () => busyButton(ask, async () => {
      openChat();
      await store.send(question);
    }));
    return h("div", { class: "card-actions card-actions-start" }, h("p", { class: "card-text muted" }, "Saved on your desktop."), ask);
  }

  /** A button that opens someone else's page (Cloudflare, Datadog) in a new tab. */
  function linkButton(href, text) {
    return h("a", { class: "button setup-link", href, target: "_blank", rel: "noopener noreferrer" }, text, icon("external"));
  }

  function link(href, text) {
    return h("a", { href, target: "_blank", rel: "noopener noreferrer" }, text, icon("external"));
  }

  return { enter: load, refresh: load };
}

/** Names to type or search for somewhere else (scopes, permissions): one per
 * row, each with Copy, and Copy all. */
/** "Run without asking" for a schedule: what it allows, said plainly. */
export function autoApprove(checked, onChange) {
  const input = h("input", { type: "checkbox", checked, class: "auto-approve" });
  input.addEventListener("change", () => onChange(input.checked));
  return h("label", { class: "toggle" }, input,
    h("span", null, h("span", { class: "strong" }, "Run without asking"),
      h("span", { class: "muted block" },
        "Its runs go ahead with commands that would otherwise wait for your OK, including ones that could send data after reading logs or pages. Commands as root and changes in the browser are still skipped. Calls to Cloudflare and Datadog always use your own saved keys.")));
}

export function copyList(items) {
  const copied = (button, label) => (ok) => {
    button.textContent = ok ? "Copied" : "Select and copy it";
    setTimeout(() => (button.textContent = label), 1500);
  };
  const rows = items.map(({ text, note }) => {
    const copy = h("button", { class: "link-button copy-one", type: "button", "aria-label": `Copy ${text}` }, "Copy");
    copy.addEventListener("click", () => copyText(text).then(copied(copy, "Copy")));
    return h("li", { class: "copy-row" }, h("code", null, text), note ? h("span", { class: "muted" }, note) : null, copy);
  });
  const all = h("button", { class: "link-button", type: "button" }, "Copy all");
  all.addEventListener("click", () => copyText(items.map((i) => i.text).join("\n")).then(copied(all, "Copy all")));
  return h("div", { class: "copy-list" }, h("ul", null, rows), h("div", { class: "copy-all" }, all));
}

/** Copies text; where the clipboard API is refused, the older way. Whether it worked. */
export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.className = "copy-scratch";
    document.body.append(area);
    area.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    area.remove();
    return ok;
  }
}
