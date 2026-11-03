// Markdown for the agent's answers, as the apps show it: headings,
// paragraphs, lists (ordered, tasks), quotes, code, tables with alignment and
// rules. Single newlines break lines, chat-style. Raw HTML stays literal text
// and images are never loaded (they become links). parse() is pure; render()
// builds DOM nodes with textContent only, never innerHTML.

const FENCE = /^ {0,3}(```|~~~)\s*([\w+-]*)\s*$/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RULE = /^ {0,3}([-*_])(\s*\1){2,}\s*$/;
const QUOTE = /^ {0,3}>\s?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])\s+(.*)$/;
const TABLE_SEPARATOR = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$/;

/** Markdown text → blocks. */
export function parse(text) {
  return blocks(text.replace(/\r\n?/g, "\n").split("\n"));
}

function blocks(lines) {
  const out = [];
  let i = 0;
  const paragraph = [];
  const flush = () => {
    if (paragraph.length) out.push({ type: "p", content: inline(paragraph.join("\n")) });
    paragraph.length = 0;
  };
  while (i < lines.length) {
    const line = lines[i];
    let m;
    if (!line.trim()) {
      flush();
      i++;
    } else if ((m = line.match(FENCE))) {
      flush();
      const fence = m[1];
      const code = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence)) code.push(lines[i++]);
      i++; // the closing fence (or the end)
      out.push({ type: "code", lang: m[2] || "", text: code.join("\n") });
    } else if ((m = line.match(HEADING))) {
      flush();
      out.push({ type: "h", level: m[1].length, content: inline(m[2]) });
      i++;
    } else if (RULE.test(line)) {
      flush();
      out.push({ type: "hr" });
      i++;
    } else if (QUOTE.test(line)) {
      flush();
      const inner = [];
      while (i < lines.length && (m = lines[i].match(QUOTE))) {
        inner.push(m[1]);
        i++;
      }
      out.push({ type: "quote", children: blocks(inner) });
    } else if (ITEM.test(line) && (paragraph.length === 0 || /^\s*(\d+[.)]|[-*+])\s/.test(line))) {
      flush();
      const [list, next] = parseList(lines, i);
      out.push(list);
      i = next;
    } else if (line.includes("|") && i + 1 < lines.length && TABLE_SEPARATOR.test(lines[i + 1]) && lines[i + 1].includes("-")) {
      flush();
      const head = cells(line);
      const align = cells(lines[i + 1]).map((c) => (c.startsWith(":") && c.endsWith(":") ? "center" : c.endsWith(":") ? "right" : c.startsWith(":") ? "left" : ""));
      const rows = [];
      i += 2;
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) rows.push(cells(lines[i++]));
      out.push({ type: "table", align, head: head.map(inline), rows: rows.map((r) => r.map(inline)) });
    } else {
      paragraph.push(line);
      i++;
    }
  }
  flush();
  return out;
}

function parseList(lines, start) {
  const first = lines[start].match(ITEM);
  const indent = first[1].length;
  const ordered = /\d/.test(first[2]);
  const list = { type: ordered ? "ol" : "ul", start: ordered ? parseInt(first[2], 10) : 1, items: [] };
  let i = start;
  while (i < lines.length) {
    const m = lines[i].match(ITEM);
    if (!m || m[1].length !== indent || /\d/.test(m[2]) !== ordered) break;
    const body = [m[3]];
    i++;
    // The item's other lines: indented deeper (a nested list, a wrapped line).
    while (i < lines.length && lines[i].trim() && (lines[i].match(/^ */)[0].length > indent)) {
      body.push(lines[i].slice(Math.min(indent + 2, lines[i].match(/^ */)[0].length)));
      i++;
    }
    let text = body[0];
    let checked = null;
    const task = text.match(/^\[([ xX])\]\s+(.*)$/);
    if (task) {
      checked = task[1] !== " ";
      text = task[2];
    }
    const rest = body.slice(1);
    const nested = rest.length && ITEM.test(rest[0]) ? blocks(rest) : null;
    const content = inline(nested ? text : [text, ...rest].join("\n"));
    list.items.push({ checked, content, children: nested || [] });
    if (i < lines.length && !lines[i].trim() && i + 1 < lines.length && lines[i + 1].match(ITEM)?.[1].length === indent) i++;
  }
  return [list, i];
}

function cells(row) {
  let r = row.trim();
  if (r.startsWith("|")) r = r.slice(1);
  if (r.endsWith("|") && !r.endsWith("\\|")) r = r.slice(0, -1);
  return r.split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, "|"));
}

// --- Inline ----------------------------------------------------------------

const INLINE = [
  { type: "code", re: /`([^`\n]+)`/ },
  { type: "image", re: /!\[([^\]\n]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/ },
  { type: "link", re: /\[([^\]\n]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/ },
  { type: "autolink", re: /<(https?:\/\/[^>\s]+)>/ },
  { type: "url", re: /\bhttps?:\/\/[^\s<>()]+[^\s<>().,;:!?'"\]]/ },
  { type: "strong", re: /\*\*(?=\S)([\s\S]+?)\*\*|__(?=\S)([\s\S]+?)__/ },
  { type: "del", re: /~~(?=\S)([\s\S]+?)~~/ },
  { type: "em", re: /\*(?=[^\s*])([^*]+?)\*|(?<![\w])_(?=\S)([^_]+?)_(?![\w])/ },
];

/** Inline Markdown → nodes: text, br, code, strong, em, del, link. */
export function inline(text) {
  const out = [];
  let rest = text;
  while (rest) {
    let best = null;
    for (const rule of INLINE) {
      const m = rule.re.exec(rest);
      if (m && (!best || m.index < best.m.index)) best = { rule, m };
    }
    if (!best) {
      pushText(out, rest);
      break;
    }
    const { rule, m } = best;
    if (m.index > 0) pushText(out, rest.slice(0, m.index));
    switch (rule.type) {
      case "code":
        out.push({ type: "code", text: m[1] });
        break;
      case "image": // never loaded: a link to it
        out.push(link(m[2], [{ type: "text", text: m[1] || m[2] }]));
        break;
      case "link":
        out.push(link(m[2], inline(m[1])));
        break;
      case "autolink":
      case "url": {
        const href = m[1] || m[0];
        out.push(link(href, [{ type: "text", text: href }]));
        break;
      }
      default:
        out.push({ type: rule.type, children: inline(m[1] ?? m[2]) });
    }
    rest = rest.slice(m.index + m[0].length);
  }
  return out;
}

/** A link only for web and mail addresses; anything else stays text. */
function link(href, children) {
  return safeHref(href) ? { type: "link", href, children } : { type: "span", children };
}

export function safeHref(href) {
  try {
    const u = new URL(href);
    return u.protocol === "https:" || u.protocol === "http:" || u.protocol === "mailto:";
  } catch {
    return false;
  }
}

function pushText(out, text) {
  const parts = text.split("\n");
  parts.forEach((part, i) => {
    if (i > 0) out.push({ type: "br" });
    if (part) out.push({ type: "text", text: part });
  });
}

// --- DOM -------------------------------------------------------------------

/** Blocks → a fragment. `onCopy(text)` is given to code blocks' Copy. */
export function render(parsed, doc = document) {
  const fragment = doc.createDocumentFragment();
  for (const block of parsed) fragment.append(renderBlock(block, doc));
  return fragment;
}

function el(doc, tag, className, children = []) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  node.append(...children);
  return node;
}

function renderBlock(block, doc) {
  switch (block.type) {
    case "p":
      return el(doc, "p", "", renderInline(block.content, doc));
    case "h":
      // Headings in an answer sit below the chat's own: h3 to h6.
      return el(doc, `h${Math.min(block.level + 2, 6)}`, "md-h", renderInline(block.content, doc));
    case "hr":
      return el(doc, "hr");
    case "quote":
      return el(doc, "blockquote", "", [...block.children.map((b) => renderBlock(b, doc))]);
    case "code": {
      const code = el(doc, "code");
      code.textContent = block.text;
      const copy = el(doc, "button", "md-copy");
      copy.type = "button";
      copy.textContent = "Copy";
      copy.addEventListener("click", () => {
        navigator.clipboard?.writeText(block.text).then(() => {
          copy.textContent = "Copied";
          setTimeout(() => (copy.textContent = "Copy"), 1500);
        });
      });
      const figure = el(doc, "div", "md-code", [el(doc, "pre", "", [code]), copy]);
      if (block.lang) figure.dataset.lang = block.lang;
      return figure;
    }
    case "ul":
    case "ol": {
      const list = el(doc, block.type);
      if (block.type === "ol" && block.start !== 1) list.start = block.start;
      for (const item of block.items) {
        const li = el(doc, "li");
        if (item.checked !== null) {
          li.className = "md-task";
          const box = el(doc, "span", item.checked ? "md-box md-box-on" : "md-box");
          box.setAttribute("aria-label", item.checked ? "done" : "not done");
          li.append(box);
        }
        li.append(...renderInline(item.content, doc), ...item.children.map((b) => renderBlock(b, doc)));
        list.append(li);
      }
      return list;
    }
    case "table": {
      const cell = (tag, content, i) => {
        const c = el(doc, tag, "", renderInline(content, doc));
        if (block.align[i]) c.style.textAlign = block.align[i];
        return c;
      };
      const head = el(doc, "thead", "", [el(doc, "tr", "", block.head.map((c, i) => cell("th", c, i)))]);
      const body = el(doc, "tbody", "", block.rows.map((r) => el(doc, "tr", "", r.map((c, i) => cell("td", c, i)))));
      return el(doc, "div", "md-table", [el(doc, "table", "", [head, body])]);
    }
    default:
      return doc.createTextNode("");
  }
}

function renderInline(nodes, doc) {
  return nodes.map((node) => {
    switch (node.type) {
      case "text":
        return doc.createTextNode(node.text);
      case "br":
        return el(doc, "br");
      case "code": {
        const code = el(doc, "code", "md-chip");
        code.textContent = node.text;
        return code;
      }
      case "link": {
        const a = el(doc, "a", "", renderInline(node.children, doc));
        a.href = node.href;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
        return a;
      }
      case "strong": case "em": case "del":
        return el(doc, node.type, "", renderInline(node.children, doc));
      default:
        return el(doc, "span", "", renderInline(node.children || [], doc));
    }
  });
}
