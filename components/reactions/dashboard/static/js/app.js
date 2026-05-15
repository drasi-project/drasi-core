import { DashboardDesigner } from "./dashboard.js";
import { DashboardSocket } from "./websocket.js";
import { promptModal, confirmModal, showToast } from "./modal.js";
import { initTheme, toggleTheme } from "./theme.js";
import { reThemeAllCharts } from "./widgets.js";

// ─── Theme ──────────────────────────────────────────────────
initTheme();
document.getElementById("theme-toggle").addEventListener("click", () => {
  toggleTheme();
  reThemeAllCharts();
});

// ─── DOM References ─────────────────────────────────────────
const listView = document.getElementById("dashboard-list-view");
const editorView = document.getElementById("dashboard-editor-view");
const dashboardListElement = document.getElementById("dashboard-list");
const editorTitleInput = document.getElementById("editor-title");
const queryListElement = document.getElementById("query-list");
const socketStatusElement = document.getElementById("socket-status");

const newDashboardButton = document.getElementById("new-dashboard-btn");
const backButton = document.getElementById("back-btn");
const saveButton = document.getElementById("save-dashboard-btn");
const addWidgetButton = document.getElementById("add-widget-btn");

// ─── State ──────────────────────────────────────────────────
const designer = new DashboardDesigner("dashboard-grid");
const socket = new DashboardSocket("/ws");
let activeSubscriptions = new Set();
let currentDashboard = null;
let queryIds = [];

// ─── Helpers ────────────────────────────────────────────────
function normalizeDashboard(raw) {
  return {
    id: raw.id ?? null,
    name: raw.name ?? "Untitled Dashboard",
    createdAt: raw.createdAt ?? new Date().toISOString(),
    updatedAt: raw.updatedAt ?? new Date().toISOString(),
    gridOptions: raw.gridOptions ?? { columns: 12, rowHeight: 60, margin: 10 },
    widgets: Array.isArray(raw.widgets) ? raw.widgets : [],
  };
}

async function apiJson(url, options = {}) {
  const response = await fetch(url, {
    headers: { "Content-Type": "application/json", ...(options.headers ?? {}) },
    ...options,
  });
  if (response.status === 204) return null;
  const body = await response.json().catch(() => null);
  if (!response.ok) throw new Error(body?.error ?? `${response.status} ${response.statusText}`);
  return body;
}

// ─── View Navigation ────────────────────────────────────────
function showListView() {
  listView.classList.add("active");
  editorView.classList.remove("active");
}

function showEditorView() {
  editorView.classList.add("active");
  listView.classList.remove("active");
}

// ─── Socket Status ──────────────────────────────────────────
function setSocketStatus(status) {
  const textEl = socketStatusElement.querySelector(".status-text");
  if (textEl) textEl.textContent = status;
  socketStatusElement.classList.toggle("connected", status === "Connected");
}

// ─── Query List ─────────────────────────────────────────────
async function refreshQueryList() {
  const response = await apiJson("/api/queries", { method: "GET" });
  queryIds = response?.queryIds ?? [];
  designer.setQueryIds(queryIds);

  queryListElement.innerHTML = queryIds.length > 0
    ? queryIds.map((q) => `<span class="query-chip">${q}</span>`).join(" ")
    : `<span class="hint">No queries available</span>`;
}

// ─── Dashboard List ─────────────────────────────────────────
function renderDashboardList(dashboards) {
  dashboardListElement.innerHTML = "";

  if (!dashboards || dashboards.length === 0) {
    dashboardListElement.innerHTML = `
      <div class="empty-state">
        <svg class="empty-state-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round">
          <rect x="3" y="3" width="7" height="7" rx="1.5"/>
          <rect x="14" y="3" width="7" height="7" rx="1.5"/>
          <rect x="3" y="14" width="7" height="7" rx="1.5"/>
          <rect x="14" y="14" width="7" height="7" rx="1.5"/>
        </svg>
        <h3>No dashboards yet</h3>
        <p>Create your first dashboard to start visualizing real-time query data.</p>
        <button id="empty-state-create-btn" type="button" class="btn btn-primary">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>
          Create Dashboard
        </button>
      </div>`;
    document.getElementById("empty-state-create-btn")?.addEventListener("click", createNewDashboard);
    return;
  }

  for (const dashboard of dashboards) {
    const widgetCount = dashboard.widgets?.length ?? 0;
    const card = document.createElement("article");
    card.className = "dashboard-card";
    card.innerHTML = `
      <h3>${dashboard.name}</h3>
      <div class="meta">
        <span>${new Date(dashboard.updatedAt).toLocaleDateString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}</span>
        <span class="widget-count-badge">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/></svg>
          ${widgetCount} widget${widgetCount !== 1 ? "s" : ""}
        </span>
      </div>
      <div class="actions">
        <button type="button" class="btn btn-ghost" data-action="open">Open</button>
        <button type="button" class="btn btn-ghost" data-action="delete" style="color:var(--danger)">Delete</button>
      </div>
    `;

    card.querySelector('[data-action="open"]').addEventListener("click", () => openDashboard(dashboard.id));
    card.querySelector('[data-action="delete"]').addEventListener("click", async () => {
      const ok = await confirmModal({
        title: "Delete Dashboard",
        message: `Are you sure you want to delete <strong>"${dashboard.name}"</strong>? This action cannot be undone.`,
        confirmLabel: "Delete",
        confirmVariant: "danger",
      });
      if (!ok) return;
      try {
        await apiJson(`/api/dashboards/${dashboard.id}`, { method: "DELETE" });
        showToast("Dashboard deleted", "info");
        await refreshDashboards();
      } catch (e) {
        showToast(`Delete failed: ${e.message}`, "error");
      }
    });

    dashboardListElement.appendChild(card);
  }
}

async function refreshDashboards() {
  try {
    const dashboards = await apiJson("/api/dashboards", { method: "GET" });
    renderDashboardList(dashboards ?? []);
  } catch (e) {
    showToast(`Failed to load dashboards: ${e.message}`, "error");
  }
}

// ─── Subscriptions ──────────────────────────────────────────
function syncSubscriptions() {
  const next = new Set(designer.subscribedQueryIds());
  const toSub = Array.from(next).filter((q) => !activeSubscriptions.has(q));
  const toUnsub = Array.from(activeSubscriptions).filter((q) => !next.has(q));
  if (toSub.length > 0) socket.subscribe(toSub);
  if (toUnsub.length > 0) socket.unsubscribe(toUnsub);
  activeSubscriptions = next;
}

// ─── Dashboard Name (inline edit) ───────────────────────────
function setEditorTitle() {
  editorTitleInput.value = currentDashboard?.name ?? "Dashboard";
}

editorTitleInput?.addEventListener("input", () => {
  if (currentDashboard) {
    currentDashboard.name = editorTitleInput.value;
    designer.setDashboardName(editorTitleInput.value);
  }
});

// ─── Dashboard CRUD ─────────────────────────────────────────
async function openDashboard(id) {
  try {
    const dashboard = await apiJson(`/api/dashboards/${id}`, { method: "GET" });
    currentDashboard = normalizeDashboard(dashboard);
    designer.loadDashboard(currentDashboard);
    setEditorTitle();
    syncSubscriptions();
    showEditorView();
    await designer.fetchAllSnapshots();
  } catch (e) {
    showToast(`Failed to open dashboard: ${e.message}`, "error");
  }
}

async function createNewDashboard() {
  const name = await promptModal({
    title: "New Dashboard",
    label: "Dashboard Name",
    placeholder: "e.g. Stock Market Monitor",
    defaultValue: "New Dashboard",
  });
  if (!name) return;

  currentDashboard = normalizeDashboard({ id: null, name, widgets: [] });
  designer.loadDashboard(currentDashboard);
  setEditorTitle();
  syncSubscriptions();
  showEditorView();
}

async function saveCurrentDashboard() {
  const dashboard = designer.getDashboard();
  if (!dashboard) return;

  try {
    const payload = {
      name: dashboard.name,
      gridOptions: dashboard.gridOptions,
      widgets: dashboard.widgets,
    };

    let saved;
    if (dashboard.id) {
      saved = await apiJson(`/api/dashboards/${dashboard.id}`, { method: "PUT", body: JSON.stringify(payload) });
    } else {
      saved = await apiJson("/api/dashboards", { method: "POST", body: JSON.stringify(payload) });
    }

    // Update the dashboard ID without reloading (preserves runtime data/values)
    currentDashboard = normalizeDashboard(saved);
    designer.updateDashboardMeta(currentDashboard);
    setEditorTitle();
    await refreshDashboards();
    showToast("Dashboard saved", "success");
  } catch (e) {
    showToast(`Save failed: ${e.message}`, "error");
  }
}

// ─── Event Bindings ─────────────────────────────────────────
function bindEvents() {
  newDashboardButton.addEventListener("click", createNewDashboard);
  backButton.addEventListener("click", async () => {
    showListView();
    await refreshDashboards();
  });
  saveButton.addEventListener("click", saveCurrentDashboard);
  addWidgetButton.addEventListener("click", () => {
    designer.openWidgetPicker();
  });

  designer.onChanged(async (dashboard) => {
    if (!dashboard) return;
    currentDashboard = dashboard;
    setEditorTitle();
    syncSubscriptions();
    await designer.fetchAllSnapshots();
  });
}

// ─── WebSocket ──────────────────────────────────────────────
function bootSocket() {
  socket.onStatus(setSocketStatus);
  socket.onQueryResult((msg) => designer.handleQueryResult(msg));
  socket.start();
}

// ─── Init ───────────────────────────────────────────────────
async function init() {
  designer.init();
  bindEvents();
  bootSocket();
  await refreshQueryList();
  await refreshDashboards();
}

init().catch((error) => {
  console.error("Failed to initialize dashboard UI", error);
  showToast(`Initialization failed: ${error.message}`, "error");
});
