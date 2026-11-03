// The theme, applied before the page paints: light unless the person chose
// Dark or "Match this device" in Settings (kept in this browser only). A
// plain script, not a module, so it runs before the first paint.
(function () {
  var dark = window.matchMedia("(prefers-color-scheme: dark)");
  function apply() {
    var choice = "light";
    try {
      choice = localStorage.getItem("theme") || "light";
    } catch (e) {}
    var theme = choice === "system" ? (dark.matches ? "dark" : "light") : choice;
    document.documentElement.dataset.theme = theme === "dark" ? "dark" : "light";
  }
  apply();
  dark.addEventListener("change", apply);
  window.applyTheme = apply;
})();
