// What the Usage page shows, from agentd's GET /usage
// (docs/conversations-and-usage.md §3.5): the chart's days (every one, zeros
// included), its scale, money, and how much of the budget is used. Pure, so
// node can test it.

const DAY = 86_400_000;

/** "2026-11-03" → that day's UTC midnight in ms; NaN if it isn't a date. */
function dayMs(date) {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(String(date || ""));
  return m ? Date.UTC(Number(m[1]), Number(m[2]) - 1, Number(m[3])) : NaN;
}

function isoDay(ms) {
  return new Date(ms).toISOString().slice(0, 10);
}

function amount(value) {
  return Number.isFinite(value) ? value : 0;
}

/** Today in the person's time zone ("YYYY-MM-DD"); this device's when it's unknown. */
export function todayIn(timeZone, now = new Date()) {
  let parts;
  try {
    parts = new Intl.DateTimeFormat("en-US", { timeZone: timeZone || undefined, year: "numeric", month: "2-digit", day: "2-digit" })
      .formatToParts(now);
  } catch {
    parts = new Intl.DateTimeFormat("en-US", { year: "numeric", month: "2-digit", day: "2-digit" }).formatToParts(now);
  }
  const part = (type) => parts.find((p) => p.type === type)?.value;
  return `${part("year")}-${part("month")}-${part("day")}`;
}

/**
 * The chart's days: `count` of them, oldest first, ending on the desktop's
 * newest day (or `today` when it listed none), each with its cost and runs,
 * zero on a day it didn't list.
 */
export function chartDays(daily, { count = 30, today } = {}) {
  const byDate = new Map();
  for (const entry of Array.isArray(daily) ? daily : []) {
    if (!entry || !Number.isFinite(dayMs(entry.date))) continue;
    const seen = byDate.get(entry.date) || { cost: 0, runs: 0 };
    byDate.set(entry.date, { cost: seen.cost + amount(entry.cost), runs: seen.runs + amount(entry.runs) });
  }
  const dates = [...byDate.keys()].sort();
  const end = dates.length ? dayMs(dates.at(-1)) : dayMs(today);
  if (!Number.isFinite(end)) return [];
  return Array.from({ length: count }, (_, i) => {
    const date = isoDay(end - (count - 1 - i) * DAY);
    const found = byDate.get(date);
    return { date, cost: found?.cost ?? 0, runs: found?.runs ?? 0 };
  });
}

/** The chart's top: the highest day rounded up to 1, 2, 2.5 or 5 × a power of ten ($1 when all are zero). */
export function niceCeiling(value) {
  if (!(value > 0)) return 1;
  const power = 10 ** Math.floor(Math.log10(value));
  for (const step of [1, 2, 2.5, 5, 10]) {
    if (value <= step * power * (1 + 1e-9)) return step * power;
  }
  return 10 * power;
}

const dollars = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
const wholeDollars = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", maximumFractionDigits: 0 });

/** "$41.20"; "<$0.01" for a fraction of a cent, which would read as nothing. */
export function money(value) {
  const n = amount(value);
  return n > 0 && n < 0.005 ? "<$0.01" : dollars.format(n);
}

/** An axis tick: "$0", "$2.50", "$10". */
export function axisMoney(value) {
  const n = amount(value);
  return Number.isInteger(n) ? wholeDollars.format(n) : dollars.format(n);
}

/** "Nov 3" for a "YYYY-MM-DD" day (a calendar date: no time zone shifts it). */
export function shortDate(date) {
  const ms = dayMs(date);
  return Number.isFinite(ms) ? new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" }) : "";
}

/** "1.2M", "30K" for token counts. */
export function compact(value) {
  return new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 }).format(amount(value));
}

/**
 * The budget's state: how much is used (0–1 for the bar, the true share for
 * the words), and its tone (`over` at 100%, `near` from 80%, else `ok`).
 * Null without a budget.
 */
export function budgetShare(budget) {
  const limit = budget?.monthly_usd;
  if (!(limit > 0)) return null;
  const share = amount(budget.spent) / limit;
  return {
    share,
    fill: Math.min(Math.max(share, 0), 1),
    percent: Math.floor(share * 100),
    tone: budget.paused === true || share >= 1 ? "over" : share >= 0.8 ? "near" : "ok",
  };
}

const KINDS = { chat: "Chat", scheduled: "Scheduled runs", tidy_up: "Memory tidy-up" };

/** This month by kind, most expensive first, with names a person reads. */
export function kindRows(byKind) {
  return Object.entries(byKind && typeof byKind === "object" ? byKind : {})
    .map(([kind, cost]) => ({ kind, name: KINDS[kind] || kind, cost: amount(cost) }))
    .sort((a, b) => b.cost - a.cost);
}

/** A model's name for people: "zai-org/GLM-5.3" is "GLM-5.3", "claude-opus-4-6" is "Claude Opus 4.6". */
export function modelName(id) {
  const claude = /^claude-([a-z]+)-(\d+)(?:-(\d+))?$/.exec(id || "");
  if (claude) {
    const [, family, major, minor] = claude;
    return `Claude ${family[0].toUpperCase()}${family.slice(1)} ${minor ? `${major}.${minor}` : major}`;
  }
  return String(id || "").split("/").pop();
}
