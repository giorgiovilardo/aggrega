// Progressive enhancements only: the page reads fine without JavaScript.
// No storage beyond the theme and language choices, no network requests.
// `LANG`, `t()` and `setLang()` come from i18n.js, loaded first.

const root = document.documentElement;
const dark = matchMedia("(prefers-color-scheme: dark)");

// Today's date in the dateline, like the app's header.
const dateline = document.querySelector("[data-today]");
const showDate = () => {
  if (!dateline) return;
  dateline.textContent = new Date().toLocaleDateString(LANG === "it" ? "it-IT" : "en-US", {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric",
  });
};
showDate();

// Language switch: the current flag and code; a click opens the list.
const lang = document.querySelector(".lang");
const langButton = lang.querySelector(".lang__button");
const langMenu = lang.querySelector(".lang__menu");
const showLang = () => {
  const current = langMenu.querySelector(`[data-lang="${LANG}"]`);
  langButton.querySelector(".flag").src = current.querySelector(".flag").src;
  langButton.querySelector(".lang__code").textContent = LANG.toUpperCase();
  for (const option of langMenu.querySelectorAll("[data-lang]")) {
    option.setAttribute("aria-current", String(option === current));
  }
};
const openLang = (open) => {
  langMenu.hidden = !open;
  langButton.setAttribute("aria-expanded", String(open));
};
langButton.addEventListener("click", () => openLang(langMenu.hidden));
langMenu.addEventListener("click", (e) => {
  const option = e.target.closest("[data-lang]");
  if (!option) return;
  openLang(false);
  langButton.focus();
  setLang(option.dataset.lang);
  try {
    localStorage.setItem("aggrega-lang", LANG);
  } catch {}
});
document.addEventListener("click", (e) => {
  if (!lang.contains(e.target)) openLang(false);
});
lang.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !langMenu.hidden) {
    openLang(false);
    langButton.focus();
  }
});
document.addEventListener("langchange", () => {
  showDate();
  showLang();
});
showLang();
lang.hidden = false;

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
