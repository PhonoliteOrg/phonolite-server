import { openModal, closeModal } from "./dialogs.js";
import { beginBusy, clearInlineError, showInlineError, toParams, fetchJson } from "./shared.js";

// Keep the table, edit controls, and unchanged accounts mounted. Server markup
// remains authoritative for role restrictions after each account mutation.
function parseUserRows(html) {
  if (typeof html !== "string" || !html.trim()) throw new Error("The server returned an invalid user list.");
  const next = document.createElement("tbody");
  next.innerHTML = html;
  if (!next.children.length || Array.from(next.children).some((row) => row.tagName !== "TR")) throw new Error("The server returned an invalid user list.");
  return Array.from(next.children);
}

function updateUserRows(rows, nextRows) {
  const selected = new Set(Array.from(rows.querySelectorAll(".row-select:checked"), (box) => box.closest(".user-row")?.dataset.userId));
  const focused = document.activeElement;
  const focusedId = focused?.closest(".user-row")?.dataset.userId;
  const focusedAction = focused?.dataset.action;
  const existing = new Map(Array.from(rows.children, (row) => [row.dataset.userId, row]));
  const retained = new Set();
  nextRows.forEach((row, index) => {
    const current = row.dataset.userId ? existing.get(row.dataset.userId) : undefined;
    const account = current?.outerHTML === row.outerHTML ? current : row;
    retained.add(account);
    if (rows.children[index] !== account) rows.insertBefore(account, rows.children[index] || null);
    const checkbox = account.querySelector(".row-select");
    if (checkbox) checkbox.checked = !checkbox.disabled && selected.has(account.dataset.userId);
  });
  Array.from(rows.children).forEach((row) => { if (!retained.has(row)) row.remove(); });
  if (focusedId && focused?.isConnected === false) {
    const row = Array.from(rows.children).find((account) => account.dataset.userId === focusedId);
    const replacement = focusedAction ? Array.from(row?.querySelectorAll("button[data-action]") || []).find((button) => button.dataset.action === focusedAction) : row?.querySelector(".row-select");
    replacement?.focus({ preventScroll: true });
  }
}

export function setupUsers() {
  const usersCard = document.getElementById("users-card");
  if (!usersCard) {
    return;
  }
  const controller = new AbortController();
  const listeners = [];
  const rows = document.getElementById("user-rows");
  let submitting = false;
  function listen(element, event, callback) {
    if (!element) return;
    element.addEventListener(event, callback);
    listeners.push(() => element.removeEventListener(event, callback));
  }
  async function submitAction(url, params, button, error, modal) {
    if (controller.signal.aborted || submitting) return;
    submitting = true;
    const finish = beginBusy(button || usersCard, "Saving");
    try {
      const data = await fetchJson(url, params, { signal: controller.signal });
      if (controller.signal.aborted) return;
      if (!rows) throw new Error("The user list is unavailable.");
      const nextRows = parseUserRows(data.html);
      closeModal(modal);
      const scroll = usersCard.querySelector(".table-scroll");
      const scrollTop = scroll?.scrollTop;
      const scrollLeft = scroll?.scrollLeft;
      updateUserRows(rows, nextRows);
      if (scroll) { scroll.scrollTop = scrollTop; scroll.scrollLeft = scrollLeft; }
      updateBulkCount();
      setPageMessage("Users updated.");
    } catch (err) {
      if (!controller.signal.aborted && err.name !== "AbortError") showInlineError(error, err.message);
    } finally {
      submitting = false;
      finish();
    }
  }

  const currentUserId = usersCard.dataset.currentUserId || "";

  const modalAdd = document.getElementById("modal-add");
  const modalEdit = document.getElementById("modal-edit");
  const modalDelete = document.getElementById("modal-delete");
  const addButton = document.getElementById("open-add");
  const editToggle = document.getElementById("toggle-edit");
  const bulkBar = document.getElementById("bulk-bar");
  const bulkCount = document.getElementById("bulk-count");
  const bulkDeleteButton = document.getElementById("bulk-delete");
  const inlineMessage = document.getElementById("users-inline-message");

  const addForm = document.getElementById("form-add-user");
  const editForm = document.getElementById("form-edit-user");
  const deleteConfirm = document.getElementById("confirm-delete");
  const deleteMessage = document.getElementById("delete-message");
  const addError = document.getElementById("add-user-error");
  const editError = document.getElementById("edit-user-error");
  const deleteError = document.getElementById("delete-user-error");

  let deleteTargets = [];

  function setPageMessage(message) {
    if (!inlineMessage) return;
    inlineMessage.setAttribute("role", "status");
    if (message) {
      inlineMessage.textContent = message;
      inlineMessage.classList.add("visible");
    } else {
      inlineMessage.textContent = "";
      inlineMessage.classList.remove("visible");
    }
  }

  function updateBulkCount() {
    const selected = usersCard.querySelectorAll(".row-select:checked").length;
    if (bulkCount) {
      bulkCount.textContent = `${selected} selected`;
    }
    if (bulkDeleteButton) bulkDeleteButton.disabled = selected === 0;
    return selected;
  }

  listen(usersCard, "click", (event) => {
    const button = event.target.closest("button[data-action]");
    if (!button) return;
    const row = button.closest(".user-row");
    if (!row) return;

    const userId = row.dataset.userId || "";
    const username = row.dataset.username || "";
    const role = row.dataset.role || "user";
    const protectedUser = row.dataset.protected === "1";

    clearInlineError(editError);
    clearInlineError(deleteError);

    if (button.dataset.action === "edit") {
      if (protectedUser && userId !== currentUserId) {
        setPageMessage("Superadmin can only edit its own account.");
        return;
      }
      setPageMessage("");
      const usernameInput = editForm.querySelector("input[name=\"username\"]");
      const passwordInput = editForm.querySelector("input[name=\"password\"]");
      const roleSelect = editForm.querySelector("select[name=\"role\"]");
      const userIdInput = editForm.querySelector("input[name=\"user_id\"]");

      if (usernameInput) usernameInput.value = username;
      if (passwordInput) passwordInput.value = "";
      if (roleSelect) {
        roleSelect.value = role;
        roleSelect.disabled = protectedUser;
      }
      if (userIdInput) userIdInput.value = userId;
      openModal(modalEdit);
      return;
    }

    if (button.dataset.action === "delete") {
      if (protectedUser) {
        setPageMessage("Superadmin cannot be deleted.");
        return;
      }
      setPageMessage("");
      deleteTargets = [userId];
      deleteMessage.textContent = `Delete ${username}? This cannot be undone.`;
      openModal(modalDelete);
    }
  });

  listen(usersCard, "change", (event) => {
    if (event.target.classList.contains("row-select")) {
      updateBulkCount();
    }
  });

  if (addButton) {
    listen(addButton, "click", () => {
      clearInlineError(addError);
      addForm.reset();
      openModal(modalAdd);
    });
  }

  if (editToggle) {
    listen(editToggle, "click", () => {
      const editing = usersCard.classList.toggle("editing");
      if (bulkBar) {
        bulkBar.classList.toggle("hidden", !editing);
      }
      const label = editing ? "Exit edit mode" : "Edit mode";
      editToggle.setAttribute("aria-label", label);
      editToggle.setAttribute("title", label);
      editToggle.setAttribute("aria-pressed", String(editing));
      if (!editing) {
        usersCard.querySelectorAll(".row-select").forEach((box) => {
          box.checked = false;
        });
        updateBulkCount();
      }
    });
  }

  if (bulkDeleteButton) {
    listen(bulkDeleteButton, "click", () => {
      const selected = Array.from(
        usersCard.querySelectorAll(".row-select:checked")
      ).map((box) => box.closest(".user-row")?.dataset.userId || "");
      const cleaned = selected.filter((id) => id);
      if (cleaned.length === 0) {
        setPageMessage("Select at least one user to delete.");
        return;
      }
      setPageMessage("");
      deleteTargets = cleaned;
      deleteMessage.textContent = `Delete ${cleaned.length} selected users? This cannot be undone.`;
      openModal(modalDelete);
    });
  }

  updateBulkCount();

  listen(addForm, "submit", (event) => {
    event.preventDefault();
    clearInlineError(addError);
    submitAction("/users", toParams(addForm), event.submitter || addForm.querySelector("button[type=submit]"), addError, modalAdd);
  });

  listen(editForm, "submit", (event) => {
    event.preventDefault();
    clearInlineError(editError);
    const userId =
      editForm.querySelector("input[name=\"user_id\"]")?.value || "";
    const params = toParams(editForm);
    const roleSelect = editForm.querySelector("select[name=\"role\"]");
    if (roleSelect && roleSelect.disabled) {
      params.set("role", roleSelect.value);
    }
    submitAction(`/users/${userId}/update`, params, event.submitter || editForm.querySelector("button[type=submit]"), editError, modalEdit);
  });

  listen(deleteConfirm, "click", () => {
    clearInlineError(deleteError);
    if (deleteTargets.length === 0) {
      showInlineError(deleteError, "No users selected.");
      return;
    }
    if (deleteTargets.length === 1) {
      const userId = deleteTargets[0];
      submitAction(`/users/${userId}/delete`, new URLSearchParams(), deleteConfirm, deleteError, modalDelete);
    } else {
      const params = new URLSearchParams();
      params.append("user_ids", deleteTargets.join(","));
      submitAction("/users/bulk-delete", params, deleteConfirm, deleteError, modalDelete);
    }
  });
  return () => {
    controller.abort();
    listeners.forEach((remove) => remove());
  };
}
