// Small DOM helpers. Text always goes in as text nodes: nothing from the
// desktop is ever parsed as HTML.

/** h("button", {class: "x", onclick}, "Label", child…) */
export function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (key === "dataset") Object.assign(el.dataset, value);
    else if (key === "value" || key === "checked" || key === "disabled" || key === "hidden" || key === "selected") el[key] = value;
    else el.setAttribute(key, value === true ? "" : String(value));
  }
  append(el, children);
  return el;
}

/** el.replaceChildren(), skipping null and false (which the DOM would show as text). */
export function fill(el, ...children) {
  el.replaceChildren();
  append(el, children);
}

function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

const SVG = "http://www.w3.org/2000/svg";

// 24×24, drawn with a 1.75 stroke.
const ICONS = {
  chat: "M5 5h14a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1h-8l-4 3v-3H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1z",
  desktop: "M4 5h16v11H4zM9 20h6M12 16v4",
  calendar: "M5 6h14v13H5zM5 10h14M9 4v4M15 4v4",
  key: "M14.5 9.5a4 4 0 1 1-1.2-2.8M13.3 6.7 20 13.4M17.5 11l-2 2M20 13.4l-2 2",
  gear: "M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6zM12 3v2.5M12 18.5V21M3 12h2.5M18.5 12H21M5.6 5.6l1.8 1.8M16.6 16.6l1.8 1.8M5.6 18.4l1.8-1.8M16.6 7.4l1.8-1.8",
  menu: "M4 7h16M4 12h16M4 17h16",
  close: "M6 6l12 12M18 6 6 18",
  send: "M12 19V5M6 11l6-6 6 6",
  stop: "M7 7h10v10H7z",
  shield: "M12 3.5 19 6v5.5c0 4.2-2.9 7.6-7 9-4.1-1.4-7-4.8-7-9V6z",
  warning: "M12 4 21 19H3zM12 10v4M12 16.5v.5",
  clock: "M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16zM12 8v4.5l3 2",
  globe: "M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16zM4 12h16M12 4c2.2 2.3 3.3 5 3.3 8s-1.1 5.7-3.3 8M12 4C9.8 6.3 8.7 9 8.7 12s1.1 5.7 3.3 8",
  external: "M14 5h5v5M19 5l-8 8M17 14v5H5V7h5",
  lock: "M7 11V8a5 5 0 0 1 10 0v3M6 11h12v9H6z",
  keyboard: "M4 7h16v10H4zM7 10h.01M10 10h.01M13 10h.01M16 10h.01M8 14h8",
  click: "M9 4v3M4 9h3M5.5 5.5l2 2M12 11l8 3-3.5 1.5L15 19z",
  back: "M15 6l-6 6 6 6",
  chats: "M8 5h12a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1h-2v3l-3.5-3H8a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM4 9v8.5L6.5 15",
  compose: "M12 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-6M17.5 3.5a2.1 2.1 0 0 1 3 3L13 14l-4 1 1-4z",
  more: "M5.5 12h.01M12 12h.01M18.5 12h.01",
  chart: "M4 20h16M7 16v-4M12 16V6M17 16v-7",
  plus: "M12 5v14M5 12h14",
  list: "M9 7h11M9 12h11M9 17h11M4.5 7h.01M4.5 12h.01M4.5 17h.01",
  up: "M6 15l6-6 6 6",
  next: "M9 6l6 6-6 6",
  down: "M6 9l6 6 6-6",
  trash: "M5 7h14M10 7V5h4v2M7 7l1 12h8l1-12M10 11v5M14 11v5",
  terminal: "M4 5h16v14H4zM8 10l3 2-3 2M13 15h3",
  download: "M12 4v11M7 10l5 5 5-5M5 20h14",
};

export function icon(name, className = "icon") {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", className);
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", ICONS[name] || "");
  svg.append(path);
  return svg;
}

const timeFormat = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const dayFormat = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric" });

/** "9:00 AM" today, "Mon, Nov 2, 9:00 AM" otherwise. */
export function when(date) {
  const d = date instanceof Date ? date : new Date(date);
  if (Number.isNaN(d.getTime())) return "";
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  return sameDay ? timeFormat.format(d) : `${dayFormat.format(d)}, ${timeFormat.format(d)}`;
}

const shortDay = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
const weekday = new Intl.DateTimeFormat(undefined, { weekday: "short" });

/** For a list: "9:00 AM" today, "Mon" this week, "Nov 2" before. */
export function day(date) {
  const d = date instanceof Date ? date : new Date(date);
  if (Number.isNaN(d.getTime())) return "";
  const today = new Date();
  if (d.toDateString() === today.toDateString()) return timeFormat.format(d);
  const days = (new Date(today.toDateString()) - new Date(d.toDateString())) / 86_400_000;
  return days > 0 && days < 7 ? weekday.format(d) : shortDay.format(d);
}

export function time(date) {
  const d = date instanceof Date ? date : new Date(date);
  return Number.isNaN(d.getTime()) ? "" : timeFormat.format(d);
}

/** Runs an async action from a button, disabled meanwhile. */
export async function busyButton(button, work) {
  if (button.disabled) return;
  button.disabled = true;
  try {
    await work();
  } finally {
    button.disabled = false;
  }
}
