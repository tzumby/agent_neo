// Agent Neo's website: header state, copy buttons, the docs' table of contents.
(() => {
  const header = document.querySelector(".site-header");
  if (header) {
    const onScroll = () => header.classList.toggle("is-scrolled", window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
  }

  // Theme toggle. Each page starts in its own default (data-theme on <html>);
  // a choice made here is saved and applies on every page from then on.
  const root = document.documentElement;
  const toggle = document.querySelector(".theme-toggle");
  const themeColor = document.querySelector('meta[name="theme-color"]');
  const label = () => {
    const next = root.dataset.theme === "dark" ? "light" : "dark";
    toggle.setAttribute("aria-label", `Switch to ${next} theme`);
    toggle.title = `Switch to ${next} theme`;
    if (themeColor) themeColor.content = root.dataset.theme === "dark" ? "#0c1e28" : "#edf0ee";
  };
  if (toggle) {
    toggle.hidden = false;
    label();
    toggle.addEventListener("click", () => {
      root.dataset.theme = root.dataset.theme === "dark" ? "light" : "dark";
      try { localStorage.setItem("neo-theme", root.dataset.theme); } catch {}
      label();
    });
  }

  // Short inline commands stay on one line (`ansible --version` shouldn't
  // break at its hyphen); long paths and URLs may still wrap.
  document.querySelectorAll(":not(pre) > code").forEach((code) => {
    if (code.textContent.length <= 26) code.classList.add("nw");
  });

  // Copy buttons. Comments (.c), prompts (.p) and output (.o) are left out, so
  // a pasted block runs as-is in zsh, which treats `#` as an argument by default.
  const copyText = (pre) => {
    const clone = pre.cloneNode(true);
    clone.querySelectorAll(".c, .p, .o").forEach((el) => el.remove());
    return clone.textContent
      .split("\n")
      .map((line) => line.replace(/\s+$/, ""))
      .filter((line) => line.length > 0)
      .join("\n");
  };
  const writeClipboard = async (text) => {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return;
    }
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  };
  document.querySelectorAll(".code[data-copy] pre").forEach((pre) => {
    const block = pre.parentElement;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy commands");
    button.addEventListener("click", async () => {
      try {
        await writeClipboard(copyText(pre));
        button.textContent = "Copied";
        button.classList.add("done");
      } catch {
        button.textContent = "Press ⌘C";
      }
      setTimeout(() => {
        button.textContent = "Copy";
        button.classList.remove("done");
      }, 1800);
    });
    block.classList.add("has-copy");
    block.appendChild(button);
  });

  // Docs: the contents list is open beside the page on wide screens and
  // folds into a disclosure on narrow ones.
  const toc = document.querySelector("details.toc");
  if (toc) {
    const wide = window.matchMedia("(min-width: 1040px)");
    const sync = () => { toc.open = wide.matches; };
    sync();
    wide.addEventListener("change", sync);
    toc.querySelectorAll("a").forEach((a) =>
      a.addEventListener("click", () => { if (!wide.matches) toc.open = false; })
    );

    const links = new Map();
    toc.querySelectorAll('a[href^="#"]').forEach((a) => {
      const target = document.getElementById(a.getAttribute("href").slice(1));
      if (target) links.set(target, a);
    });
    let current = null;
    const mark = (a) => {
      if (a === current) return;
      if (current) current.classList.remove("is-current");
      current = a;
      if (a) a.classList.add("is-current");
    };
    const sections = [...links.keys()];
    const pick = () => {
      const line = window.innerHeight * 0.3;
      let best = sections[0];
      for (const s of sections) {
        if (s.getBoundingClientRect().top <= line) best = s;
      }
      mark(links.get(best));
    };
    pick();
    window.addEventListener("scroll", pick, { passive: true });
    window.addEventListener("resize", pick);
  }
})();
