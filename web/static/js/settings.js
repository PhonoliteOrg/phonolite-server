import { openModal, closeModal, confirmAction } from "./dialogs.js";
import { beginBusy, clearInlineError, showInlineError, toParams, fetchJson } from "./shared.js";
import { setupFolderBrowser } from "./folder-browser.js";

// Requests and listeners belong to one mounted settings view. A late result
// must never update another page after navigation.
function createSettingsScope(formError) {
  const controller = new AbortController();
  const listeners = [];
  const timers = new Set();
  const pending = new Set();
  const scope = {
    signal: controller.signal,
    listen(element, name, callback, capture = false) {
      if (!element) return;
      element.addEventListener(name, callback, capture);
      listeners.push(() => element.removeEventListener(name, callback, capture));
    },
    later(callback, delay) {
      const timer = window.setTimeout(() => {
        timers.delete(timer);
        if (!controller.signal.aborted) callback();
      }, delay);
      timers.add(timer);
    },
    async run(element, label, task, onError = (error) => showInlineError(formError, error.message)) {
      if (controller.signal.aborted || pending.has(element)) return;
      pending.add(element);
      clearInlineError(formError);
      const finish = beginBusy(element, label);
      try { return await task(); }
      catch (error) { if (!controller.signal.aborted && error.name !== "AbortError") onError(error); }
      finally { pending.delete(element); finish(); }
    },
    request(url, params) { return fetchJson(url, params, { signal: controller.signal }); },
    dispose() {
      controller.abort();
      listeners.forEach((remove) => remove());
      timers.forEach((timer) => window.clearTimeout(timer));
    },
  };
  return scope;
}

function insertSourceRow(list, html, row) {
  if (typeof html !== "string" || !html.trim()) throw new Error("The server returned an invalid settings row.");
  if (row) { row.insertAdjacentHTML("beforebegin", html); row.remove(); }
  else list.insertAdjacentHTML("beforeend", html);
}

function scanStarted() {
  document.dispatchEvent(new CustomEvent("library:scan-started"));
}

function setupMusicRoots(scope) {
  const list = document.getElementById("music-roots");
  const openAdd = document.getElementById("open-music-root-add");
  const modalAdd = document.getElementById("modal-music-root-add");
  const modalEdit = document.getElementById("modal-music-root-edit");
  const addForm = document.getElementById("form-music-root-add");
  const editForm = document.getElementById("form-music-root-edit");
  const addError = document.getElementById("music-root-add-error");
  const editError = document.getElementById("music-root-edit-error");
  if (!list || !openAdd || !modalAdd || !modalEdit || !addForm || !editForm) return;
  let editingRow;
  const folderBrowser = setupFolderBrowser(modalAdd, scope);
  const updatePlaceholder = () => document.getElementById("no-music-roots")?.classList.toggle("hidden", list.querySelectorAll(".source-row").length > 0);
  scope.listen(openAdd, "click", () => { clearInlineError(addError); addForm.reset(); openModal(modalAdd); folderBrowser?.start(); });
  scope.listen(list, "click", async (event) => {
    const button = event.target.closest("button[data-action]");
    const row = button?.closest(".source-row");
    if (!row) return;
    const rootId = row.dataset.rootId || "";
    if (button.dataset.action === "edit-root") {
      editingRow = row;
      editForm.querySelector("input[name=root_id]").value = rootId;
      editForm.querySelector("input[name=path]").value = row.dataset.path || "";
      clearInlineError(editError);
      openModal(modalEdit);
    } else if (button.dataset.action === "delete-root") {
      if (!(await confirmAction({ title: "Remove music folder?", message: "Removing this folder will reindex the library.", confirmLabel: "Remove folder", danger: true })) || scope.signal.aborted) return;
      scope.run(button, "Removing folder", async () => {
        await scope.request(`/settings/music-roots/${rootId}/delete`, new URLSearchParams());
        if (scope.signal.aborted) return;
        row.remove(); updatePlaceholder(); scanStarted();
      });
    }
  });
  scope.listen(addForm, "submit", (event) => {
    event.preventDefault(); clearInlineError(addError);
    if (folderBrowser && !folderBrowser.canSubmit()) return;
    scope.run(event.submitter || addForm, "Adding folder", async () => {
      // Capture the selected path before disabling the picker during the save.
      const params = toParams(addForm);
      folderBrowser?.setSubmitting(true);
      try {
        const data = await scope.request("/settings/music-roots/add", params);
        if (scope.signal.aborted) return;
        insertSourceRow(list, data.html); closeModal(modalAdd); updatePlaceholder(); scanStarted();
      } finally { folderBrowser?.setSubmitting(false); }
    }, (error) => showInlineError(addError, error.message));
  });
  scope.listen(editForm, "submit", (event) => {
    event.preventDefault(); clearInlineError(editError);
    const rootId = editForm.querySelector("input[name=root_id]").value;
    const row = editingRow;
    scope.run(event.submitter || editForm, "Saving folder", async () => {
      const data = await scope.request(`/settings/music-roots/${rootId}/update`, toParams(editForm));
      if (scope.signal.aborted) return;
      insertSourceRow(list, data.html, row); closeModal(modalEdit); scanStarted();
    }, (error) => showInlineError(editError, error.message));
  });
}

function setupMetadataSources(scope) {
  const list = document.getElementById("metadata-sources");
  const openAdd = document.getElementById("open-metadata-add");
  const modalAdd = document.getElementById("modal-metadata-add");
  const modalEdit = document.getElementById("modal-metadata-edit");
  const addForm = document.getElementById("form-metadata-add");
  const editForm = document.getElementById("form-metadata-edit");
  const addError = document.getElementById("metadata-add-error");
  const editError = document.getElementById("metadata-edit-error");
  const addTestButton = document.getElementById("metadata-add-test");
  const testButton = document.getElementById("metadata-test");
  if (!list || !openAdd || !modalAdd || !modalEdit || !addForm || !editForm) return;
  let editingRow;
  let testSequence = 0;
  function setProviderFields(form) {
    const provider = form.querySelector("select[name=provider]")?.value;
    form.querySelectorAll(".provider-fields").forEach((section) => section.classList.toggle("active", section.dataset.provider === provider));
  }
  const updatePlaceholder = () => document.getElementById("no-metadata-sources")?.classList.toggle("hidden", list.querySelectorAll(".source-row").length > 0);
  scope.listen(openAdd, "click", () => {
    testSequence++;
    clearInlineError(addError); addForm.reset(); setProviderFields(addForm);
    if (addTestButton) addTestButton.textContent = "Test";
    openModal(modalAdd);
  });
  for (const form of [addForm, editForm]) scope.listen(form.querySelector("select[name=provider]"), "change", () => setProviderFields(form));
  scope.listen(list, "click", async (event) => {
    const button = event.target.closest("button[data-action]");
    const row = button?.closest(".source-row");
    if (!row) return;
    const sourceId = row.dataset.sourceId || "";
    if (button.dataset.action === "edit-source") {
      editingRow = row; testSequence++;
      editForm.querySelector("input[name=source_id]").value = sourceId;
      editForm.querySelector("select[name=provider]").value = row.dataset.provider || "theaudiodb";
      editForm.querySelector("input[name=api_key]").value = row.dataset.apiKey || "";
      editForm.querySelector("input[name=user_agent]").value = row.dataset.userAgent || "";
      setProviderFields(editForm); clearInlineError(editError);
      if (testButton) testButton.textContent = "Test";
      openModal(modalEdit);
    } else if (button.dataset.action === "delete-source") {
      if (!(await confirmAction({ title: "Delete metadata source?", message: "Are you sure you want to delete this source?", confirmLabel: "Delete source", danger: true })) || scope.signal.aborted) return;
      scope.run(button, "Deleting source", async () => {
        await scope.request(`/settings/metadata/${sourceId}/delete`, new URLSearchParams());
        if (scope.signal.aborted) return;
        row.remove(); updatePlaceholder(); scanStarted();
      });
    }
  });
  scope.listen(list, "change", (event) => {
    const toggle = event.target;
    if (!toggle.classList.contains("source-toggle")) return;
    const enabled = toggle.checked;
    const params = new URLSearchParams();
    if (enabled) params.set("enabled", "true");
    scope.run(toggle.closest(".source-actions") || toggle, "Updating source", async () => {
      toggle.disabled = true;
      try { await scope.request(`/settings/metadata/${toggle.dataset.sourceId}/toggle`, params); }
      finally { toggle.disabled = false; }
    }, (error) => { toggle.checked = !enabled; showInlineError(document.getElementById("settings-error"), error.message); });
  });
  scope.listen(addForm, "submit", (event) => {
    event.preventDefault(); clearInlineError(addError);
    scope.run(event.submitter || addForm, "Adding source", async () => {
      const data = await scope.request("/settings/metadata/add", toParams(addForm));
      if (scope.signal.aborted) return;
      insertSourceRow(list, data.html); closeModal(modalAdd); updatePlaceholder();
    }, (error) => showInlineError(addError, error.message));
  });
  scope.listen(editForm, "submit", (event) => {
    event.preventDefault(); clearInlineError(editError);
    const sourceId = editForm.querySelector("input[name=source_id]").value;
    const row = editingRow;
    scope.run(event.submitter || editForm, "Saving source", async () => {
      const data = await scope.request(`/settings/metadata/${sourceId}/update`, toParams(editForm));
      if (scope.signal.aborted) return;
      insertSourceRow(list, data.html, row); closeModal(modalEdit);
    }, (error) => showInlineError(editError, error.message));
  });
  for (const [button, form, errorElement] of [[addTestButton, addForm, addError], [testButton, editForm, editError]]) {
    scope.listen(button, "click", async () => {
      clearInlineError(errorElement);
      const current = ++testSequence;
      let result = false;
      await scope.run(button, "Testing source", async () => { await scope.request("/settings/metadata/test", toParams(form)); result = true; }, (error) => { if (current === testSequence) showInlineError(errorElement, error.message); });
      if (scope.signal.aborted || current !== testSequence) return;
      button.textContent = result ? "\u2713" : "X";
      scope.later(() => { if (current === testSequence) button.textContent = "Test"; }, 2000);
    });
  }
}

export function setupSettings() {
  const form = document.getElementById("settings");
  if (!form) return;
  const formError = document.getElementById("settings-error");
  const scope = createSettingsScope(formError);
  setupMusicRoots(scope); setupMetadataSources(scope);
  const saveButton = document.getElementById("save-settings");
  const saveBar = document.getElementById("settings-save-bar");
  const saveBarObserver = saveBar && typeof ResizeObserver === "function" ? new ResizeObserver(() => {
    document.body.style.setProperty("--settings-save-height", `${saveBar.hidden ? 0 : saveBar.getBoundingClientRect().height}px`);
  }) : null;
  saveBarObserver?.observe(saveBar);
  const debugToggle = document.getElementById("log-debug-toggle");
  const reindexButton = document.getElementById("open-reindex");
  const reindexModal = document.getElementById("modal-reindex");
  const unsavedModal = document.getElementById("modal-unsaved");
  let initialState = serializeForm();
  let initialValues = captureValues();
  let dirty = false;
  let saved = false;
  let submitting = false;
  let leavePromise;
  let resolveLeave;
  function trackedElements() {
    return Array.from(form.elements).filter((element) => element.name && !element.disabled && !element.closest("#metadata-sources, #music-roots") && element.dataset.ignoreDirty !== "true");
  }
  function captureValues() {
    return trackedElements().map((element) => ({ element, value: element.value, checked: element.checked, selected: element.selectedOptions ? Array.from(element.selectedOptions, (option) => option.value) : null }));
  }
  function serializeForm() {
    const entries = trackedElements().map((element) => {
      const type = (element.type || "").toLowerCase();
      const value = type === "checkbox" || type === "radio" ? (element.checked ? "1" : "0") : type === "select-multiple" ? Array.from(element.selectedOptions, (option) => option.value).join(",") : element.value;
      return `${element.name}:${value}`;
    });
    return entries.sort().join("|");
  }
  function setDirty(next) {
    dirty = next;
    if (dirty) saved = false;
    document.body.dataset.settingsDirty = String(dirty);
    const visible = dirty || submitting;
    if (saveBar) saveBar.hidden = !visible;
    document.body.dataset.settingsSaveVisible = String(visible);
    const status = document.getElementById("settings-save-status");
    if (status) status.textContent = dirty ? "Unsaved changes" : saved ? "Settings saved" : "No unsaved changes";
    if (saveButton) saveButton.disabled = !dirty || submitting;
  }
  function updateDirty() { setDirty(serializeForm() !== initialState); }
  function finishLeave(discard) {
    const resolve = resolveLeave;
    resolveLeave = null; leavePromise = null;
    if (discard) {
      initialValues.forEach(({ element, value, checked, selected }) => {
        element.value = value;
        if (typeof checked === "boolean") element.checked = checked;
        if (selected && element.type === "select-multiple") Array.from(element.options).forEach((option) => { option.selected = selected.includes(option.value); });
      });
      setDirty(false);
    }
    closeModal(unsavedModal);
    resolve?.(discard);
  }
  function canLeave() {
    if (scope.signal.aborted || !dirty) return Promise.resolve(true);
    if (leavePromise) return leavePromise;
    if (!unsavedModal) return Promise.resolve(false);
    leavePromise = new Promise((resolve) => { resolveLeave = resolve; });
    openModal(unsavedModal);
    return leavePromise;
  }
  scope.listen(document.getElementById("unsaved-discard"), "click", () => finishLeave(true));
  scope.listen(document.getElementById("unsaved-stay"), "click", () => finishLeave(false));
  scope.listen(unsavedModal, "dialog:close", () => { if (resolveLeave) finishLeave(false); });
  scope.listen(window, "beforeunload", (event) => { if (!dirty) return; event.preventDefault(); event.returnValue = ""; });
  scope.listen(form, "input", updateDirty);
  scope.listen(form, "change", updateDirty);
  scope.listen(debugToggle, "change", () => {
    const enabled = debugToggle.checked;
    const params = new URLSearchParams();
    if (enabled) params.set("enabled", "true");
    scope.run(debugToggle.closest("label") || debugToggle, "Updating logging", async () => {
      debugToggle.disabled = true;
      try { await scope.request("/settings/logging/debug", params); }
      finally { debugToggle.disabled = false; }
    }, (error) => { debugToggle.checked = !enabled; showInlineError(formError, error.message); });
  });
  scope.listen(reindexButton, "click", (event) => { event.preventDefault(); openModal(reindexModal); });
  scope.listen(document.getElementById("confirm-reindex"), "click", () => { closeModal(reindexModal); submitSettings(reindexButton); });
  async function submitSettings(submitter) {
    if (submitting || scope.signal.aborted) return;
    const action = submitter?.getAttribute("formaction") || form.getAttribute("action") || "/settings";
    const isSave = action === "/settings";
    if (isSave && !dirty) return;
    if (!isSave && dirty && !(await canLeave())) return;
    if (submitting || scope.signal.aborted) return;
    const savedState = serializeForm();
    const savedValues = captureValues();
    const params = toParams(form);
    submitting = true; setDirty(dirty);
    await scope.run(submitter || saveButton, isSave ? "Saving settings" : "Starting scan", async () => {
      const data = await scope.request(action, params);
      if (scope.signal.aborted) return;
      if (isSave) {
        initialState = savedState; initialValues = savedValues; saved = true; updateDirty();
      } else if (data.status === "indexing") scanStarted();
    });
    if (!scope.signal.aborted) { submitting = false; updateDirty(); }
  }
  scope.listen(form, "submit", (event) => { event.preventDefault(); submitSettings(event.submitter); });
  // Actions outside the settings form share the router's discard dialog.
  scope.listen(document, "submit", async (event) => {
    if (!dirty || event.target === form || event.target.matches("#form-music-root-add, #form-music-root-edit, #form-metadata-add, #form-metadata-edit")) return;
    event.preventDefault(); event.stopImmediatePropagation();
    if (!(await canLeave()) || scope.signal.aborted) return;
    event.target.requestSubmit?.(event.submitter || undefined);
  }, true);
  updateDirty();
  return {
    canLeave,
    dispose() {
      scope.dispose(); resolveLeave?.(false); resolveLeave = null;
      saveBarObserver?.disconnect();
      document.body.style?.removeProperty("--settings-save-height");
      delete document.body.dataset.settingsDirty;
      delete document.body.dataset.settingsSaveVisible;
    },
  };
}
