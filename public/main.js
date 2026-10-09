// Progressive enhancements only: the page reads fine without JavaScript.
// No storage beyond the theme choice, no network requests. `LANG` and `t()`
// come from i18n.js, loaded first.

const root = document.documentElement;
const dark = matchMedia("(prefers-color-scheme: dark)");

// Today's date in the dateline, like the app's header.
const dateline = document.querySelector("[data-today]");
if (dateline) {
  dateline.textContent = new Date().toLocaleDateString(LANG === "it" ? "it-IT" : "en-US", {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric",
  });
}

// Theme switch: follows the system until the visitor picks one.
const toggle = document.querySelector(".theme-switch");
const isDark = () => (root.dataset.theme ?? (dark.matches ? "dark" : "light")) === "dark";
const syncToggle = () => {
  toggle.setAttribute("aria-pressed", String(isDark()));
  document.querySelectorAll('meta[name="theme-color"]').forEach((m) => {
    if (root.dataset.theme) m.content = isDark() ? "#131211" : "#f7f4ee";
  });
};

toggle.addEventListener("click", () => {
  const next = isDark() ? "light" : "dark";
  const apply = () => {
    root.dataset.theme = next;
    syncToggle();
  };
  if (document.startViewTransition && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
    document.startViewTransition(apply);
  } else {
    apply();
  }
  try {
    localStorage.setItem("aggrega-theme", next);
  } catch {}
});
dark.addEventListener("change", syncToggle);
syncToggle();

// Hairline under the top bar once the page scrolls beneath it.
const topbar = document.querySelector(".topbar");
const sentinel = document.createElement("div");
sentinel.setAttribute("aria-hidden", "true");
topbar.before(sentinel);
new IntersectionObserver(([e]) => topbar.classList.toggle("is-stuck", !e.isIntersecting)).observe(sentinel);

// The masthead only joins the top bar once the hero wordmark has scrolled away.
const wordmark = document.querySelector(".front__wordmark");
const masthead = topbar.querySelector(".masthead");
if (wordmark) {
  new IntersectionObserver(
    ([e]) => {
      topbar.classList.toggle("hide-brand", e.isIntersecting);
      masthead.inert = e.isIntersecting;
    },
    // Measured below the sticky bar, so the swap happens as the word slides under it.
    { rootMargin: `-${topbar.offsetHeight}px 0px 0px 0px` },
  ).observe(wordmark);
}

// Underline the nav item for the section on screen.
const links = new Map(
  [...document.querySelectorAll(".nav a[href^='#']")].map((a) => [a.hash.slice(1), a]),
);
const spy = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      links.forEach((a, id) => a.setAttribute("aria-current", String(id === e.target.id)));
    }
  },
  { rootMargin: "-45% 0px -50% 0px" },
);
links.forEach((_, id) => {
  const el = document.getElementById(id);
  if (el) spy.observe(el);
});

// Copy buttons on code blocks.
for (const button of document.querySelectorAll(".copy")) {
  button.addEventListener("click", async () => {
    const code = button.parentElement.querySelector("code").textContent;
    try {
      await navigator.clipboard.writeText(code);
      button.textContent = t("copied", "Copied");
      button.classList.add("is-done");
      setTimeout(() => {
        button.textContent = t("copy", "Copy");
        button.classList.remove("is-done");
      }, 1600);
    } catch {
      button.textContent = t("copy.fallback", "Select & copy");
    }
  });
}
