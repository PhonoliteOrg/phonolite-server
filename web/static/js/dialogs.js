const stack = [];
const inertElements = new Map();
const closeSelector = "[data-close], [data-modal-close], [data-close-reindex], [data-close-unsaved]";
const focusSelector = 'button, a[href], input, select, textarea, [tabindex]:not([tabindex="-1"])';
let initialized = false;
let confirmationCount = 0;
let pendingConfirmation;

function focusable(modal) {
  return Array.from(modal.querySelectorAll(focusSelector)).filter(
    (element) => !element.disabled && element.tabIndex >= 0 && !element.closest("[inert]") && element.getClientRects().length > 0
  );
}

function refreshBackground() {
  inertElements.forEach((inert, element) => { element.inert = inert; });
  inertElements.clear();
  const active = stack.at(-1)?.modal;
  if (!active) return;
  let branch = active;
  while (branch.parentElement) {
    for (const sibling of branch.parentElement.children) {
      if (sibling === branch || sibling.tagName === "SCRIPT" || sibling.tagName === "STYLE") continue;
      inertElements.set(sibling, sibling.inert);
      sibling.inert = true;
    }
    branch = branch.parentElement;
    if (branch === document.body) break;
  }
}

function prepare(modal) {
  const card = modal.querySelector(".modal-card") || modal;
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");
  card.tabIndex = -1;
  const title = card.querySelector("h2, h3");
  if (title && !modal.hasAttribute("aria-labelledby")) {
    if (!title.id) title.id = `${modal.id}-title`;
    modal.setAttribute("aria-labelledby", title.id);
  }
  return card;
}

export function openModal(modal) {
  if (!modal || stack.some((entry) => entry.modal === modal)) return;
  setupDialogs();
  const previous = stack.at(-1)?.modal;
  if (previous) {
    previous.classList.remove("open");
    previous.setAttribute("aria-hidden", "true");
  }
  stack.push({ modal, trigger: document.activeElement });
  const card = prepare(modal);
  modal.classList.add("open");
  modal.setAttribute("aria-hidden", "false");
  refreshBackground();
  const preferred = modal.querySelector("[autofocus]");
  (preferred || focusable(modal)[0] || card).focus();
}

export function closeModal(modal) {
  if (!modal) return;
  const index = stack.findIndex((entry) => entry.modal === modal);
  if (index === -1) return;
  const wasActive = index === stack.length - 1;
  const [{ trigger }] = stack.splice(index, 1);
  modal.classList.remove("open");
  modal.setAttribute("aria-hidden", "true");
  const active = stack.at(-1)?.modal;
  if (active) {
    active.classList.add("open");
    active.setAttribute("aria-hidden", "false");
  }
  refreshBackground();
  if (wasActive) {
    if (trigger?.isConnected && !trigger.closest("[inert]")) trigger.focus();
    else if (active) (focusable(active)[0] || prepare(active)).focus();
  }
  modal.dispatchEvent(new CustomEvent("dialog:close"));
}

export function closeAllModals() {
  while (stack.length) closeModal(stack.at(-1).modal);
}

export function setupDialogs() {
  if (initialized) return;
  initialized = true;
  document.querySelectorAll(".modal").forEach((modal) => {
    prepare(modal);
    modal.setAttribute("aria-hidden", "true");
  });
  document.addEventListener("click", (event) => {
    const button = event.target.closest(closeSelector);
    if (button) closeModal(button.closest(".modal"));
  });
  document.addEventListener("keydown", (event) => {
    const active = stack.at(-1)?.modal;
    if (!active) return;
    if (event.key === "Escape") {
      event.preventDefault();
      closeModal(active);
    } else if (event.key === "Tab") {
      const items = focusable(active);
      const first = items[0] || prepare(active);
      const last = items.at(-1) || first;
      if (!items.length || !active.contains(document.activeElement)) {
        event.preventDefault();
        first.focus();
      } else if (event.shiftKey && (document.activeElement === first || document.activeElement === prepare(active))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
  });
}

export function confirmAction({ title, message, confirmLabel = "Confirm", danger = false }) {
  // Repeated activations must not stack hidden dialogs or resume multiple
  // destructive callers when the user confirms one action.
  if (pendingConfirmation) return Promise.resolve(false);
  let resolve;
  const promise = new Promise((finish) => { resolve = finish; });
  const pending = { promise };
  pendingConfirmation = pending;
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.id = `confirmation-${++confirmationCount}`;
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.dataset.modalClose = "";
  const card = document.createElement("div");
  card.className = "modal-card";
  const header = document.createElement("div");
  header.className = "modal-header";
  const heading = document.createElement("h3");
  heading.textContent = title;
  header.append(heading);
  const description = document.createElement("p");
  description.className = "muted";
  description.id = `${modal.id}-description`;
  description.textContent = message;
  modal.setAttribute("aria-describedby", description.id);
  const actions = document.createElement("div");
  actions.className = "row right";
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "button ghost";
  cancel.textContent = "Cancel";
  cancel.dataset.modalClose = "";
  cancel.setAttribute("autofocus", "");
  const confirm = document.createElement("button");
  confirm.type = "button";
  confirm.className = danger ? "button danger" : "button";
  confirm.textContent = confirmLabel;
  actions.append(cancel, confirm);
  card.append(header, description, actions);
  modal.append(backdrop, card);
  document.body.append(modal);
  let confirmed = false;
  confirm.addEventListener("click", () => {
    confirmed = true;
    closeModal(modal);
  });
  modal.addEventListener("dialog:close", () => {
    modal.remove();
    if (pendingConfirmation === pending) pendingConfirmation = undefined;
    resolve(confirmed);
  }, { once: true });
  openModal(modal);
  return promise;
}
