// Setup: the security agent's first run. Four cards: where updates go
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

/** Example checks. `needs`: the sources they read. `post`: where to post, if anywhere. */
function examples(post, datadog) {
  const postIt = post ? ` Post it with post_update to ${post}: severity warning if something needs action, info otherwise.` : "";
  const alert = post
    ? ` post a short alert with post_update to ${post}: what happened, from where, and what you'd do. If nothing stands out, post nothing and answer in one line.`
    : " say so in a short alert: what happened, from where, and what you'd do. If nothing stands out, answer in one line.";
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

export function mountSetup(store, el, { openUpdates, openChat, intro = false, onProgress = () => {} }) {
  let state = { destinations: [], keys: new Set(), schedules: [] };

  async function load() {
    try {
      const [destinations, secrets, schedules] = await Promise.all([
        store.api.destinations().catch(() => []),
        store.api.secrets(),
        store.api.schedules(),
      ]);
      state = { destinations, keys: new Set(secrets.map((s) => s.name)), schedules };
    } catch (error) {
      fill(el, h("p", { class: "error" }, `Couldn't load your setup: ${error.message}`));
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

  function render() {
    const done = ["updates", "cloudflare", "datadog", "watching"].filter((step) => has[step]()).length;
    onProgress(done, 4);
    fill(el,
      intro
        ? h("div", { class: "setup-intro" },
            h("h2", null, "Let's set up your security agent"),
            h("p", { class: "muted" }, "Connect what it watches and where it reports. Each step takes a few minutes, and you can come back to them under Setup."))
        : h("p", { class: "lede" }, `${done} of 4 done. Your agent reads your Cloudflare and Datadog with read-only keys kept on its desktop, checks them on a schedule, and posts what it finds.`),
      h("ol", { class: "setup-steps" },
        step("Choose where updates go", has.updates(), updatesCard()),
        step("Connect Cloudflare", has.cloudflare(), cloudflareCard()),
        step("Connect Datadog", has.datadog(), datadogCard()),
        step("Choose what it watches", has.watching(), watchCard())));
  }

  function step(title, done, body) {
    return h("li", { class: done ? "setup-step is-done" : "setup-step" },
      h("div", { class: "setup-mark", "aria-hidden": "true" }, done ? "✓" : ""),
      h("section", { class: "card setup-card", "aria-label": title },
        h("h3", { class: "card-title" }, title, done ? h("span", { class: "tag" }, "Done") : null),
        body));
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
    slack.addEventListener("click", () => openUpdates("slack"));
    teams.addEventListener("click", () => openUpdates("teams"));
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
        h("li", null, "At ", link("https://dash.cloudflare.com/profile/api-tokens", "Cloudflare's API tokens page"), ", choose Create Token, then Custom token."),
        h("li", null, "Add these permissions, all Read: Account › Account Analytics, Zone › Zone, Zone › Analytics, Zone › Firewall Services."),
        h("li", null, "Under resources, pick your account and the zones (sites) to watch. Create the token and paste it here.")),
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
          await load();
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
    const form = h("form", { class: "secret-form" },
      h("ol", { class: "steps" },
        h("li", null, "Pick your Datadog site: it's in your Datadog address."),
        h("li", null, "In Datadog, Organization Settings › API Keys: create one and paste it below."),
        h("li", null, "Organization Settings › Application Keys: create one with the scopes logs_read_data, logs_read_index_data, metrics_read, monitors_read and events_read, and paste it below.")),
      h("label", { class: "label" }, "Site", site),
      h("label", { class: "label" }, "API key", apiKey),
      h("label", { class: "label" }, "Application key", appKey),
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
          await load();
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
    const post = state.destinations[0]?.name || "";
    const sources = { cloudflare: has.cloudflare(), datadog: has.datadog() };
    const names = new Set(state.schedules.map((s) => s.name));
    const list = examples(post, sources.datadog).map((example) => {
      const added = names.has(example.name);
      const missing = example.needs.filter((source) => !sources[source]);
      const add = h("button", { class: "button", type: "button", disabled: added || missing.length > 0 }, added ? "Added" : "Add");
      add.addEventListener("click", () => busyButton(add, async () => {
        try {
          await store.api.addSchedule({
            name: example.name,
            prompt: example.prompt,
            cron: example.cron,
            timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
            utc_offset: -new Date().getTimezoneOffset() * 60,
          });
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
          h("details", { class: "instructions" }, h("summary", null, "What it does"), h("p", null, example.prompt))),
        h("div", { class: "list-actions" }, add));
    });
    return [
      h("p", { class: "card-text" }, post
        ? `Checks your agent runs on its own, posting to ${post}. Each run costs a little in model calls; you can pause or cancel them under Schedules.`
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

  function link(href, text) {
    return h("a", { href, target: "_blank", rel: "noopener noreferrer" }, text, icon("external"));
  }

  return { enter: load, refresh: load, complete: () => ["updates", "cloudflare", "datadog", "watching"].every((s) => has[s]()) };
}
