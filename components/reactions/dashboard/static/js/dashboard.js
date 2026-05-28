import {
  applyResultDiff,
  createDefaultWidget,
  createWidgetRuntime,
  renderWidget,
  detectFields,
  WIDGET_TYPE_META,
  getDefaultConfig,
  AGGREGATION_MODES,
  resizeAllCharts,
  disposeWidgetChart,
} from "./widgets.js";
import { openModal, closeModal, showToast } from "./modal.js";

export class DashboardDesigner {
  constructor(gridElementId) {
    this.gridElement = document.getElementById(gridElementId);
    this.grid = null;
    this.dashboard = null;
    this.runtimeByWidgetId = new Map();
    this.widgetBodyByWidgetId = new Map();
    this.changeHandlers = [];
    this.queryIds = [];
  }

  init() {
    this.grid = window.GridStack.init(
      { column: 12, cellHeight: 60, margin: 10, float: true },
      this.gridElement,
    );

    this.grid.on("change", (_event, items) => {
      for (const item of items) {
        const widgetId = item.id ?? item.el?.dataset?.widgetId;
        if (!widgetId) continue;
        const widget = this.getWidget(widgetId);
        if (!widget) continue;
        widget.grid = { x: item.x, y: item.y, w: item.w, h: item.h };
      }
      this.emitChanged();
    });

    // Resize ECharts when widgets are resized by the user
    this.grid.on("resizestop", () => {
      setTimeout(() => resizeAllCharts(), 50);
    });
  }

  setQueryIds(queryIds) {
    this.queryIds = queryIds || [];
  }

  onChanged(handler) { this.changeHandlers.push(handler); }

  emitChanged() {
    for (const h of this.changeHandlers) h(this.getDashboard());
  }

  loadDashboard(dashboard) {
    this.dashboard = structuredClone(dashboard);
    this.runtimeByWidgetId.clear();
    this.widgetBodyByWidgetId.clear();
    this.grid.removeAll(true);

    for (const widget of this.dashboard.widgets) {
      this.addWidgetToGrid(widget);
      this.runtimeByWidgetId.set(widget.id, createWidgetRuntime());
      this.renderWidget(widget.id);
    }

    // Let gridstack finish layout, then resize charts to match actual container dimensions
    setTimeout(() => resizeAllCharts(), 200);
  }

  getDashboard() {
    if (!this.dashboard) return null;
    return structuredClone(this.dashboard);
  }

  // Update dashboard metadata (id, name) without reloading widgets or wiping runtime data
  updateDashboardMeta(dashboard) {
    if (!this.dashboard) return;
    this.dashboard.id = dashboard.id;
    this.dashboard.name = dashboard.name;
  }

  setDashboardName(name) {
    if (!this.dashboard) return;
    this.dashboard.name = name;
    this.emitChanged();
  }

  // ─── Widget Picker Modal ────────────────────────────────

  openWidgetPicker() {
    if (!this.dashboard) return;

    const grid = document.createElement("div");
    grid.className = "widget-picker-grid";

    for (const [type, meta] of Object.entries(WIDGET_TYPE_META)) {
      const card = document.createElement("div");
      card.className = "widget-picker-card";
      card.innerHTML = `
        <div class="widget-picker-icon" style="color:${meta.color}">${meta.icon}</div>
        <div class="widget-picker-name">${meta.name}</div>
        <div class="widget-picker-desc">${meta.description}</div>
      `;
      card.addEventListener("click", () => {
        closeModal();
        this.openConfigPanel(type, null);
      });
      grid.appendChild(card);
    }

    openModal({ title: "Add Widget", body: grid, size: "lg" });
  }

  // ─── Config Panel Modal ─────────────────────────────────

  openConfigPanel(widgetType, existingWidget) {
    const isEdit = !!existingWidget;
    const config = isEdit
      ? structuredClone(existingWidget.config ?? {})
      : getDefaultConfig(widgetType);
    const title = isEdit ? existingWidget.title : (WIDGET_TYPE_META[widgetType]?.name ?? widgetType);

    const runtime = isEdit
      ? this.runtimeByWidgetId.get(existingWidget.id)
      : null;
    const detectedFields = runtime ? detectFields(runtime) : [];

    const form = document.createElement("div");
    form.className = "modal-form";

    // Title field
    form.appendChild(this._formGroup("Title", this._textInput("cfg-title", title)));

    // Query selector
    const querySelect = this._selectInput("cfg-query", this.queryIds, config.queryId ?? "");
    form.appendChild(this._formGroup("Query", querySelect));

    // Type-specific fields
    this._buildTypeConfig(form, widgetType, config, detectedFields);

    // Auto-detect fields from snapshot when creating new widgets
    const updateFieldSelects = (fields) => {
      const opts = fields.length > 0 ? fields : ["(type data first)"];
      form.querySelectorAll("select.field-select").forEach((sel) => {
        const current = sel.value;
        sel.innerHTML = "";
        for (const f of opts) {
          const opt = document.createElement("option");
          opt.value = f;
          opt.textContent = f;
          if (f === current) opt.selected = true;
          sel.appendChild(opt);
        }
      });
    };

    const fetchFieldsForQuery = async (queryId) => {
      if (!queryId) return;
      try {
        const resp = await fetch(`/api/queries/${encodeURIComponent(queryId)}/snapshot`);
        if (!resp.ok) return;
        const snapshot = await resp.json();
        const rows = Array.isArray(snapshot.rows) ? snapshot.rows : [];
        if (rows.length > 0) {
          const fields = Object.keys(rows[0]).sort();
          updateFieldSelects(fields);
        } else if (snapshot.aggregation && typeof snapshot.aggregation === "object") {
          const fields = Object.keys(snapshot.aggregation).sort();
          updateFieldSelects(fields);
        }
      } catch (_) { /* ignore */ }
    };

    // Listen for query changes to re-detect fields
    querySelect.addEventListener("change", () => fetchFieldsForQuery(querySelect.value));

    // On open, if no fields detected yet, fetch from snapshot
    if (detectedFields.length === 0) {
      const initialQuery = config.queryId || this.queryIds[0] || "";
      fetchFieldsForQuery(initialQuery);
    }

    openModal({
      title: isEdit ? `Configure: ${title}` : `New ${WIDGET_TYPE_META[widgetType]?.name ?? widgetType}`,
      body: form,
      size: "md",
      actions: [
        { label: "Cancel", variant: "ghost", action: () => closeModal() },
        {
          label: isEdit ? "Apply" : "Add",
          variant: "primary",
          action: () => {
            const finalTitle = form.querySelector("#cfg-title")?.value?.trim() || title;
            const finalQueryId = form.querySelector("#cfg-query")?.value || "";
            const finalConfig = this._readTypeConfig(form, widgetType, { ...config, queryId: finalQueryId });

            if (isEdit) {
              existingWidget.title = finalTitle;
              existingWidget.config = finalConfig;
              this.updateWidgetHeader(existingWidget.id, finalTitle, existingWidget.type);
              this.renderWidget(existingWidget.id);
              this.emitChanged();
            } else {
              const widget = createDefaultWidget(widgetType, this.dashboard.widgets.length);
              widget.title = finalTitle;
              widget.config = finalConfig;
              this.dashboard.widgets.push(widget);
              this.runtimeByWidgetId.set(widget.id, createWidgetRuntime());
              this.addWidgetToGrid(widget);
              this.renderWidget(widget.id);
              this.emitChanged();
            }
            closeModal();
            showToast(isEdit ? "Widget updated" : "Widget added", "success");
          },
        },
      ],
    });
  }

  _buildTypeConfig(form, type, config, fields) {
    const fieldOpts = fields.length > 0 ? fields : ["(type data first)"];

    switch (type) {
      case "line_chart":
        form.appendChild(this._formGroup("X Axis Field", this._fieldSelect("cfg-xField", fieldOpts, config.xField)));
        form.appendChild(this._formGroup("Y Axis Fields (comma separated)", this._textInput("cfg-yFields", (config.yFields || []).join(", "))));
        form.appendChild(this._formGroup("Max Points", this._numberInput("cfg-maxPoints", config.maxPoints ?? 100)));
        break;
      case "bar_chart":
        form.appendChild(this._formGroup("Category Field", this._fieldSelect("cfg-catField", fieldOpts, config.categoryField)));
        form.appendChild(this._formGroup("Value Fields (comma separated)", this._textInput("cfg-valFields", (config.valueFields || []).join(", "))));
        break;
      case "pie_chart":
        form.appendChild(this._formGroup("Name Field", this._fieldSelect("cfg-nameField", fieldOpts, config.nameField)));
        form.appendChild(this._formGroup("Value Field", this._fieldSelect("cfg-valField", fieldOpts, config.valueField)));
        break;
      case "table":
        form.appendChild(this._formGroup("Columns (comma separated, empty = auto)", this._textInput("cfg-columns", (config.columns || []).join(", "))));
        break;
      case "gauge": {
        const row = document.createElement("div");
        row.className = "form-row";
        row.appendChild(this._formGroup("Min", this._numberInput("cfg-min", config.min ?? 0)));
        row.appendChild(this._formGroup("Max", this._numberInput("cfg-max", config.max ?? 100)));
        form.appendChild(row);
        form.appendChild(this._formGroup("Value Field", this._fieldSelect("cfg-valField", fieldOpts, config.valueField)));
        form.appendChild(this._aggregationGroup(config, fieldOpts));
        break;
      }
      case "kpi":
        form.appendChild(this._formGroup("Value Field", this._fieldSelect("cfg-valField", fieldOpts, config.valueField)));
        form.appendChild(this._formGroup("Label", this._textInput("cfg-label", config.label ?? "KPI")));
        form.appendChild(this._aggregationGroup(config, fieldOpts));
        break;
      case "text": {
        const templateArea = this._textareaInput("cfg-template", config.template ?? "");
        templateArea.style.minHeight = "160px";
        templateArea.style.fontFamily = "monospace";
        templateArea.style.fontSize = "0.8rem";
        templateArea.placeholder = `## My Dashboard Section

**{{count}}** items in result set

{{#each rows}}
- **{{this.name}}**: {{format this.value "currency"}}
{{/each}}

---
**Total**: {{format (sum "value") "currency"}}
**Average**: {{format (avg "value") "currency"}}`;
        form.appendChild(this._formGroup("Handlebars Template (Markdown)", templateArea));

        // Collapsible help section
        const helpDetails = document.createElement("details");
        helpDetails.className = "template-help";
        const summary = document.createElement("summary");
        summary.textContent = "Template Reference";
        helpDetails.appendChild(summary);
        helpDetails.innerHTML += `
          <div class="template-help-body">
            <h4>Context Variables</h4>
            <table class="help-table">
              <tr><td><code>{{rows}}</code></td><td>Array of all result rows</td></tr>
              <tr><td><code>{{count}}</code></td><td>Number of rows</td></tr>
              <tr><td><code>{{latest}}</code></td><td>Most recently changed row</td></tr>
              <tr><td><code>{{latest.fieldName}}</code></td><td>Field from latest row</td></tr>
            </table>
            <h4>Loops &amp; Conditionals</h4>
            <table class="help-table">
              <tr><td><code>{{#each rows}}...{{/each}}</code></td><td>Iterate all rows</td></tr>
              <tr><td><code>{{this.fieldName}}</code></td><td>Access field inside loop</td></tr>
              <tr><td><code>{{#if condition}}...{{/if}}</code></td><td>Conditional block</td></tr>
              <tr><td><code>{{#if (gt this.price 100)}}...{{/if}}</code></td><td>Comparison in condition</td></tr>
            </table>
            <h4>Aggregation Helpers</h4>
            <table class="help-table">
              <tr><td><code>{{sum "field"}}</code></td><td>Sum of field across all rows</td></tr>
              <tr><td><code>{{avg "field"}}</code></td><td>Average value</td></tr>
              <tr><td><code>{{min "field"}}</code></td><td>Minimum value</td></tr>
              <tr><td><code>{{max "field"}}</code></td><td>Maximum value</td></tr>
              <tr><td><code>{{count}}</code></td><td>Number of rows</td></tr>
            </table>
            <h4>Formatting Helpers</h4>
            <table class="help-table">
              <tr><td><code>{{format val "currency"}}</code></td><td>$1,234.56</td></tr>
              <tr><td><code>{{format val "percent"}}</code></td><td>12.34%</td></tr>
              <tr><td><code>{{format val "compact"}}</code></td><td>1.2M, 3.4K</td></tr>
            </table>
            <h4>Comparison Helpers</h4>
            <table class="help-table">
              <tr><td><code>{{eq a b}}</code></td><td>Equal</td></tr>
              <tr><td><code>{{gt a b}}</code> / <code>{{lt a b}}</code></td><td>Greater / Less than</td></tr>
              <tr><td><code>{{gte a b}}</code> / <code>{{lte a b}}</code></td><td>Greater/Less or equal</td></tr>
            </table>
            <h4>Links, Sorting &amp; HTML</h4>
            <table class="help-table">
              <tr><td><code>{{link url "text"}}</code></td><td>Hyperlink opening in new tab</td></tr>
              <tr><td><code>{{#sortBy rows "field"}}...{{/sortBy}}</code></td><td>Sort rows ascending</td></tr>
              <tr><td><code>{{#sortBy rows "field" "desc"}}...{{/sortBy}}</code></td><td>Sort rows descending</td></tr>
              <tr><td><code>{{#groupBy rows "field"}}{{@key}}...{{/groupBy}}</code></td><td>Group rows by field value</td></tr>
              <tr><td><code>{{html myVar}}</code></td><td>Render raw HTML (sanitized by DOMPurify)</td></tr>
            </table>
            <h4>Markdown</h4>
            <p>Supports full Markdown: <code>**bold**</code>, <code>*italic*</code>, headings, lists, tables, code blocks, and horizontal rules.</p>
          </div>`;
        form.appendChild(helpDetails);
        break;
      }
      case "map": {
        const row = document.createElement("div");
        row.className = "form-row";
        row.appendChild(this._formGroup("Lat Field", this._fieldSelect("cfg-latField", fieldOpts, config.latField)));
        row.appendChild(this._formGroup("Lng Field", this._fieldSelect("cfg-lngField", fieldOpts, config.lngField)));
        form.appendChild(row);
        form.appendChild(this._formGroup("Value Field", this._fieldSelect("cfg-valField", fieldOpts, config.valueField)));
        break;
      }
    }
  }

  _readTypeConfig(form, type, base) {
    const val = (id) => form.querySelector(`#${id}`)?.value ?? "";
    const csv = (id) => val(id).split(",").map((s) => s.trim()).filter(Boolean);

    switch (type) {
      case "line_chart": return { ...base, xField: val("cfg-xField"), yFields: csv("cfg-yFields"), maxPoints: Number(val("cfg-maxPoints")) || 100 };
      case "bar_chart": return { ...base, categoryField: val("cfg-catField"), valueFields: csv("cfg-valFields") };
      case "pie_chart": return { ...base, nameField: val("cfg-nameField"), valueField: val("cfg-valField") };
      case "table": return { ...base, columns: csv("cfg-columns") };
      case "gauge": return { ...base, valueField: val("cfg-valField"), min: Number(val("cfg-min")) || 0, max: Number(val("cfg-max")) || 100, aggregation: val("cfg-aggregation") || "last", filterField: val("cfg-filterField"), filterValue: val("cfg-filterValue") };
      case "kpi": return { ...base, valueField: val("cfg-valField"), label: val("cfg-label") || "KPI", aggregation: val("cfg-aggregation") || "last", filterField: val("cfg-filterField"), filterValue: val("cfg-filterValue") };
      case "text": return { ...base, template: val("cfg-template") };
      case "map": return { ...base, latField: val("cfg-latField"), lngField: val("cfg-lngField"), valueField: val("cfg-valField") };
      default: return base;
    }
  }

  // ─── Form helpers ───────────────────────────────────────

  _formGroup(labelText, inputEl) {
    const group = document.createElement("div");
    group.className = "form-group";
    const label = document.createElement("label");
    label.className = "form-label";
    label.textContent = labelText;
    group.appendChild(label);
    group.appendChild(inputEl);
    return group;
  }

  _textInput(id, value = "") {
    const input = document.createElement("input");
    input.type = "text";
    input.id = id;
    input.className = "form-input";
    input.value = value;
    return input;
  }

  _numberInput(id, value = 0) {
    const input = document.createElement("input");
    input.type = "number";
    input.id = id;
    input.className = "form-input";
    input.value = value;
    return input;
  }

  _textareaInput(id, value = "") {
    const ta = document.createElement("textarea");
    ta.id = id;
    ta.className = "form-textarea";
    ta.value = value;
    return ta;
  }

  _selectInput(id, options, selected = "") {
    const select = document.createElement("select");
    select.id = id;
    select.className = "form-select";
    for (const opt of options) {
      const option = document.createElement("option");
      option.value = opt;
      option.textContent = opt;
      if (opt === selected) option.selected = true;
      select.appendChild(option);
    }
    return select;
  }

  _fieldSelect(id, fields, selected = "") {
    const el = this._selectInput(id, fields, selected);
    el.classList.add("field-select");
    return el;
  }

  _aggregationGroup(config, fieldOpts) {
    const wrapper = document.createElement("div");
    wrapper.className = "aggregation-group";

    // Aggregation mode labels for display
    const modeLabels = {
      last: "Last Updated Row",
      first: "First Row",
      sum: "Sum of All Rows",
      avg: "Average of All Rows",
      count: "Row Count",
      min: "Minimum Value",
      max: "Maximum Value",
      filter: "Filter by Field Value",
    };

    const modeSelect = document.createElement("select");
    modeSelect.id = "cfg-aggregation";
    modeSelect.className = "form-select";
    for (const mode of AGGREGATION_MODES) {
      const opt = document.createElement("option");
      opt.value = mode;
      opt.textContent = modeLabels[mode] || mode;
      if (mode === (config.aggregation || "last")) opt.selected = true;
      modeSelect.appendChild(opt);
    }
    wrapper.appendChild(this._formGroup("Reduce Mode", modeSelect));

    // Filter fields (shown only when mode is "filter")
    const filterRow = document.createElement("div");
    filterRow.className = "form-row filter-fields";
    filterRow.style.display = (config.aggregation === "filter") ? "" : "none";
    filterRow.appendChild(this._formGroup("Filter Field", this._fieldSelect("cfg-filterField", fieldOpts, config.filterField ?? "")));
    filterRow.appendChild(this._formGroup("Filter Value", this._textInput("cfg-filterValue", config.filterValue ?? "")));
    wrapper.appendChild(filterRow);

    // Toggle filter fields visibility
    modeSelect.addEventListener("change", () => {
      filterRow.style.display = modeSelect.value === "filter" ? "" : "none";
    });

    // Help text
    const helpText = document.createElement("div");
    helpText.className = "form-help";
    helpText.textContent = "Controls how multiple result rows are reduced to a single value.";
    wrapper.appendChild(helpText);

    return wrapper;
  }

  // ─── Widget CRUD ────────────────────────────────────────

  addWidget(widgetType) {
    this.openWidgetPicker();
  }

  removeWidget(widgetId) {
    if (!this.dashboard) return;
    const idx = this.dashboard.widgets.findIndex((w) => w.id === widgetId);
    if (idx < 0) return;

    this.dashboard.widgets.splice(idx, 1);
    this.runtimeByWidgetId.delete(widgetId);

    // Dispose any ECharts instance before removing the DOM element
    const body = this.widgetBodyByWidgetId.get(widgetId);
    if (body) disposeWidgetChart(body);
    this.widgetBodyByWidgetId.delete(widgetId);

    const el = this.gridElement.querySelector(`[data-widget-id="${widgetId}"]`)
      || this.grid.engine.nodes.find((n) => n.id === widgetId)?.el;
    if (el) this.grid.removeWidget(el, true);

    this.emitChanged();
    showToast("Widget removed", "info");
  }

  configureWidget(widgetId) {
    const widget = this.getWidget(widgetId);
    if (!widget) return;
    this.openConfigPanel(widget.type, widget);
  }

  handleQueryResult(message) {
    if (!this.dashboard || !Array.isArray(message.results)) return;

    for (const widget of this.dashboard.widgets) {
      if ((widget.config?.queryId ?? "") !== message.query_id) continue;

      const runtime = this.runtimeByWidgetId.get(widget.id) ?? createWidgetRuntime();
      for (const diff of message.results) applyResultDiff(runtime, diff);
      this.runtimeByWidgetId.set(widget.id, runtime);
      this.renderWidget(widget.id);
    }
  }

  /// Fetch the current snapshot for a query and populate all widgets bound to it.
  async fetchSnapshot(queryId) {
    if (!queryId || !this.dashboard) return;
    try {
      const response = await fetch(`/api/queries/${encodeURIComponent(queryId)}/snapshot`);
      if (!response.ok) return;
      const snapshot = await response.json();
      const rows = Array.isArray(snapshot.rows) ? snapshot.rows : [];
      const aggregation = snapshot.aggregation ?? null;
      if (rows.length === 0 && aggregation === null) return;

      for (const widget of this.dashboard.widgets) {
        if ((widget.config?.queryId ?? "") !== queryId) continue;
        const runtime = this.runtimeByWidgetId.get(widget.id) ?? createWidgetRuntime();
        // Only populate if the runtime has no data yet
        if (runtime.rows.length === 0 && runtime.aggregation === null) {
          for (const row of rows) {
            applyResultDiff(runtime, { op: "add", data: row });
          }
          if (aggregation !== null) {
            applyResultDiff(runtime, { op: "aggregation", after: aggregation });
          }
          this.runtimeByWidgetId.set(widget.id, runtime);
          this.renderWidget(widget.id);
        }
      }
    } catch (e) {
      // Non-critical — widgets will still populate from WebSocket
    }
  }

  /// Fetch snapshots for all queries bound to current widgets.
  async fetchAllSnapshots() {
    const queryIds = this.subscribedQueryIds();
    await Promise.all(queryIds.map((q) => this.fetchSnapshot(q)));
    // Resize charts after data populates — containers may have changed
    setTimeout(() => resizeAllCharts(), 100);
  }

  subscribedQueryIds() {
    if (!this.dashboard) return [];
    const set = new Set();
    for (const w of this.dashboard.widgets) {
      const q = w.config?.queryId;
      if (typeof q === "string" && q.trim()) set.add(q.trim());
    }
    return Array.from(set);
  }

  getWidget(id) {
    return this.dashboard?.widgets.find((w) => w.id === id) ?? null;
  }

  // ─── Grid rendering ────────────────────────────────────

  addWidgetToGrid(widget) {
    const opts = {
      id: widget.id,
      x: widget.grid?.x ?? 0,
      y: widget.grid?.y ?? 0,
      w: widget.grid?.w ?? 4,
      h: widget.grid?.h ?? 3,
      content: this.widgetMarkup(widget),
    };
    if (widget.grid?.autoPosition) opts.autoPosition = true;
    const el = this.grid.addWidget(opts);

    el.dataset.widgetId = widget.id;
    const body = el.querySelector(".widget-body");
    if (body) this.widgetBodyByWidgetId.set(widget.id, body);

    el.querySelector(`[data-action="configure"]`)?.addEventListener("click", () => this.configureWidget(widget.id));
    el.querySelector(`[data-action="remove"]`)?.addEventListener("click", () => this.removeWidget(widget.id));

    // Sync actual position back after auto-placement
    const node = el.gridstackNode;
    if (node) {
      widget.grid = { x: node.x, y: node.y, w: node.w, h: node.h };
    }
  }

  widgetMarkup(widget) {
    const meta = WIDGET_TYPE_META[widget.type] || {};
    return `
      <div class="grid-stack-item-content">
        <div class="widget-header">
          <div style="display:flex;align-items:center">
            <div class="widget-type-indicator type-${widget.type}"></div>
            <span data-role="title">${widget.title}</span>
          </div>
          <div class="widget-actions">
            <button type="button" class="btn-icon" data-action="configure" title="Configure">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>
            </button>
            <button type="button" class="btn-icon" data-action="remove" title="Remove">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
            </button>
          </div>
        </div>
        <div class="widget-body"></div>
      </div>
    `;
  }

  updateWidgetHeader(widgetId, title, type) {
    const node = this.grid.engine.nodes.find((n) => n.id === widgetId);
    const titleEl = node?.el?.querySelector('[data-role="title"]');
    if (titleEl) titleEl.textContent = title;
  }

  renderWidget(widgetId) {
    const widget = this.getWidget(widgetId);
    const body = this.widgetBodyByWidgetId.get(widgetId);
    if (!widget || !body) return;
    const runtime = this.runtimeByWidgetId.get(widgetId) ?? createWidgetRuntime();
    this.runtimeByWidgetId.set(widgetId, runtime);
    renderWidget(widget, runtime, body);
  }
}
