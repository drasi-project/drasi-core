// ─── Theme switcher (dark/light) ────────────────────────────
const STORAGE_KEY = "drasi-dashboard-theme";

function getSystemPreference() {
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

function getStoredTheme() {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function applyTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
}

export function getCurrentTheme() {
  return document.documentElement.getAttribute("data-theme") || "dark";
}

export function initTheme() {
  const stored = getStoredTheme();
  const theme = stored || getSystemPreference();
  applyTheme(theme);

  // Listen for OS preference changes (only if no stored preference)
  window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", (e) => {
    if (!getStoredTheme()) {
      applyTheme(e.matches ? "light" : "dark");
      window.dispatchEvent(new CustomEvent("themechange", { detail: { theme: getCurrentTheme() } }));
    }
  });
}

export function toggleTheme() {
  const current = getCurrentTheme();
  const next = current === "dark" ? "light" : "dark";
  applyTheme(next);
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch { /* ignore */ }
  window.dispatchEvent(new CustomEvent("themechange", { detail: { theme: next } }));
  return next;
}
