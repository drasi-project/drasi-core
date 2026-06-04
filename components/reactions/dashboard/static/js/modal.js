// Modal and toast notification system for Drasi Dashboard.
// No dependencies — vanilla JS.

let activeModal = null;
let toastContainer = null;

// ─── Modal ───────────────────────────────────────────────

/**
 * Open a modal dialog.
 * @param {object} options
 * @param {string} options.title
 * @param {string|HTMLElement} options.body - HTML string or DOM element
 * @param {Array<{label:string, variant?:string, action:function}>} [options.actions]
 * @param {function} [options.onClose]
 * @param {string} [options.size] - 'sm' | 'md' | 'lg' | 'xl'  (default 'md')
 * @returns {{ el: HTMLElement, close: function }}
 */
export function openModal({ title, body, actions = [], onClose, size = "md" }) {
  closeModal();

  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";

  const dialog = document.createElement("div");
  dialog.className = `modal-dialog modal-${size}`;
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", title);

  // Header
  const header = document.createElement("div");
  header.className = "modal-header";

  const titleEl = document.createElement("h2");
  titleEl.className = "modal-title";
  titleEl.textContent = title;
  header.appendChild(titleEl);

  const closeBtn = document.createElement("button");
  closeBtn.className = "modal-close-btn";
  closeBtn.type = "button";
  closeBtn.setAttribute("aria-label", "Close");
  closeBtn.innerHTML = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>`;
  closeBtn.addEventListener("click", () => closeModal());
  header.appendChild(closeBtn);

  dialog.appendChild(header);

  // Body
  const bodyEl = document.createElement("div");
  bodyEl.className = "modal-body";
  if (typeof body === "string") {
    bodyEl.innerHTML = body;
  } else if (body instanceof HTMLElement) {
    bodyEl.appendChild(body);
  }
  dialog.appendChild(bodyEl);

  // Footer actions
  if (actions.length > 0) {
    const footer = document.createElement("div");
    footer.className = "modal-footer";
    for (const { label, variant = "ghost", action, id } of actions) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = `btn btn-${variant}`;
      btn.textContent = label;
      if (id) btn.id = id;
      btn.addEventListener("click", () => {
        if (action) action();
      });
      footer.appendChild(btn);
    }
    dialog.appendChild(footer);
  }

  backdrop.appendChild(dialog);
  document.body.appendChild(backdrop);

  // Animate in
  requestAnimationFrame(() => {
    backdrop.classList.add("modal-visible");
  });

  // Close on backdrop click
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeModal();
  });

  // Close on Escape
  const escController = new AbortController();
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeModal();
  }, { signal: escController.signal });

  activeModal = { el: backdrop, escController, onClose };

  return { el: bodyEl, close: closeModal };
}

/**
 * Close the currently open modal.
 */
export function closeModal() {
  if (!activeModal) return;
  const { el, escController, onClose } = activeModal;
  escController.abort();
  el.classList.remove("modal-visible");
  el.classList.add("modal-hiding");
  el.addEventListener("transitionend", () => el.remove(), { once: true });
  // Fallback removal if transition doesn't fire
  setTimeout(() => { if (el.parentNode) el.remove(); }, 400);
  if (onClose) onClose();
  activeModal = null;
}

/**
 * Open a confirmation dialog.
 * @returns {Promise<boolean>}
 */
export function confirmModal({ title = "Confirm", message, confirmLabel = "Confirm", confirmVariant = "danger" }) {
  return new Promise((resolve) => {
    let settled = false;
    const messageEl = document.createElement("p");
    messageEl.className = "modal-message";
    messageEl.textContent = message;
    openModal({
      title,
      body: messageEl,
      size: "sm",
      actions: [
        { label: "Cancel", variant: "ghost", action: () => { settled = true; closeModal(); resolve(false); } },
        { label: confirmLabel, variant: confirmVariant, action: () => { settled = true; closeModal(); resolve(true); } },
      ],
      onClose: () => { if (!settled) resolve(false); },
    });
  });
}

/**
 * Open a prompt dialog (replacement for window.prompt).
 * @returns {Promise<string|null>}
 */
export function promptModal({ title, label = "", defaultValue = "", placeholder = "" }) {
  return new Promise((resolve) => {
    let settled = false;
    const form = document.createElement("div");
    form.className = "modal-form";

    if (label) {
      const labelEl = document.createElement("label");
      labelEl.className = "form-label";
      labelEl.textContent = label;
      form.appendChild(labelEl);
    }

    const input = document.createElement("input");
    input.type = "text";
    input.className = "form-input";
    input.value = defaultValue;
    input.placeholder = placeholder;
    form.appendChild(input);

    const { close } = openModal({
      title,
      body: form,
      size: "sm",
      actions: [
        { label: "Cancel", variant: "ghost", action: () => { settled = true; close(); resolve(null); } },
        {
          label: "OK",
          variant: "primary",
          id: "modal-prompt-ok",
          action: () => {
            settled = true;
            const val = input.value.trim();
            close();
            resolve(val.length > 0 ? val : null);
          },
        },
      ],
      onClose: () => { if (!settled) resolve(null); },
    });

    // Focus and Enter to submit
    requestAnimationFrame(() => input.focus());
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        document.getElementById("modal-prompt-ok")?.click();
      }
    });
  });
}

// ─── Toast Notifications ─────────────────────────────────

function ensureToastContainer() {
  if (toastContainer) return toastContainer;
  toastContainer = document.getElementById("toast-container");
  if (!toastContainer) {
    toastContainer = document.createElement("div");
    toastContainer.id = "toast-container";
    toastContainer.className = "toast-container";
    document.body.appendChild(toastContainer);
  }
  return toastContainer;
}

/**
 * Show a toast notification.
 * @param {string} message
 * @param {'success'|'error'|'info'|'warning'} [type='info']
 * @param {number} [durationMs=3500]
 */
export function showToast(message, type = "info", durationMs = 3500) {
  const container = ensureToastContainer();

  const toast = document.createElement("div");
  toast.className = `toast toast-${type}`;

  const icons = {
    success: `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="20 6 9 17 4 12"/></svg>`,
    error: `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>`,
    warning: `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>`,
    info: `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>`,
  };

  toast.innerHTML = `
    <span class="toast-icon">${icons[type] || icons.info}</span>
    <span class="toast-message">${message}</span>
  `;

  container.appendChild(toast);
  requestAnimationFrame(() => toast.classList.add("toast-visible"));

  setTimeout(() => {
    toast.classList.remove("toast-visible");
    toast.classList.add("toast-hiding");
    toast.addEventListener("transitionend", () => toast.remove(), { once: true });
    setTimeout(() => { if (toast.parentNode) toast.remove(); }, 400);
  }, durationMs);
}
