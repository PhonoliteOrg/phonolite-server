import { clearInlineError, showInlineError, fetchJson } from "./shared.js";

function folderTrail(value) {
  const windows = /^[a-z]:[\\/]/i.test(value) || value.startsWith("\\\\");
  const separator = windows ? "\\" : "/";
  const normalized = windows ? value.replaceAll("/", "\\") : value;
  let root = "/";
  if (windows) {
    root = normalized.startsWith("\\\\")
      ? `${normalized.match(/^\\\\[^\\]+\\[^\\]+/)[0]}\\`
      : normalized.slice(0, 3);
  }
  const trail = [{ name: root, path: root }];
  for (const name of normalized.slice(root.length).split(separator).filter(Boolean)) {
    const previous = trail.at(-1).path;
    trail.push({ name, path: `${previous.endsWith(separator) ? previous : previous + separator}${name}` });
  }
  // Preserve the server's spelling, including UNC share roots without a trailing slash.
  trail.at(-1).path = value;
  return trail;
}

function folderKey(value) {
  return /^[a-z]:[\\/]/i.test(value) || value.startsWith("\\\\")
    ? value.replaceAll("/", "\\").replace(/\\$/, "").toLowerCase()
    : value;
}

// Browse the server's filesystem; the browser's own OS and file APIs are irrelevant.
export function setupFolderBrowser(modal, scope) {
  const find = (id) => modal.querySelector(`#${id}`);
  const path = find("music-root-add-path");
  const selection = find("music-root-add-selection");
  const status = find("music-root-browser-status");
  const list = find("music-root-browser-list");
  const add = find("music-root-add-submit");
  const error = find("music-root-add-error");
  if (![path, selection, status, list, add].every(Boolean)) return null;

  let request;
  let pathTimer;
  let generation = 0;
  let loading = false;
  let submitting = false;
  let roots = [];
  const nodes = new Map();

  function nodeFor(folder) {
    const key = folderKey(folder.path);
    if (!nodes.has(key)) nodes.set(key, { ...folder, children: [], loaded: false, expanded: false });
    return nodes.get(key);
  }

  function renderTree(focusPath) {
    list.replaceChildren();
    let focused;
    let selected;
    function render(node, container) {
      const item = document.createElement("li");
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.folderPath = node.path;
      button.className = "folder-browser-entry";
      button.disabled = submitting;
      button.setAttribute("aria-expanded", String(node.expanded && node.children.length > 0));
      if (node.loaded && !node.children.length) button.dataset.empty = "true";
      if (folderKey(node.path) === folderKey(selection.value)) {
        button.setAttribute("aria-current", "location");
        selected = button;
      }
      button.innerHTML = '<svg class="folder-browser-toggle" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="m9 5 7 7-7 7"/></svg><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M3 7V5a1 1 0 0 1 1-1h6l2 3h8a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1Z"/></svg>';
      const name = document.createElement("span");
      name.textContent = node.name;
      button.append(name);
      item.append(button);
      container.append(item);
      if (focusPath && folderKey(node.path) === folderKey(focusPath)) focused = button;
      if (node.expanded && node.children.length) {
        const children = document.createElement("ul");
        children.className = "folder-browser-children";
        children.setAttribute("aria-label", `Subfolders of ${node.name}`);
        item.append(children);
        node.children.forEach((child) => render(child, children));
      }
    }
    roots.forEach((node) => render(node, list));
    if (focusPath) (focused || path).focus();
    else selected?.scrollIntoView?.({ block: "nearest" });
  }

  function applyListing(data) {
    if (data.path === null) {
      roots = data.folders.map(nodeFor);
      return;
    }
    let siblings = roots;
    let node;
    for (const folder of folderTrail(data.path)) {
      node = nodeFor(folder);
      if (!siblings.includes(node)) siblings.push(node);
      node.expanded = true;
      siblings = node.children;
    }
    node.path = data.path;
    node.children = data.folders.map(nodeFor);
    node.loaded = true;
  }

  function describe(folders, drives = false) {
    const count = folders.length ? `${folders.length} ${drives ? "drives" : "folders"}` : drives ? "No drives found" : "No subfolders";
    status.textContent = drives ? count : `Folder found · ${count}`;
  }

  function updateControls() {
    path.disabled = submitting;
    add.disabled = submitting || loading || !selection.value;
    list.querySelectorAll("button").forEach((button) => { button.disabled = submitting; });
    list.setAttribute("aria-busy", String(loading));
  }

  function stop() {
    generation++;
    request?.abort();
    window.clearTimeout(pathTimer);
    pathTimer = undefined;
  }

  async function browse(value = "", focusList = false, reset = false) {
    if (submitting || scope.signal.aborted) return;
    stop();
    const current = generation;
    const controller = new AbortController();
    request = controller;
    loading = true;
    selection.value = "";
    if (reset) { roots = []; nodes.clear(); list.replaceChildren(); }
    status.textContent = "Loading folders…";
    clearInlineError(error);
    path.setAttribute("aria-invalid", "false");
    updateControls();
    try {
      const query = new URLSearchParams({ path: value });
      const data = await fetchJson(`/settings/folders?${query}`, undefined, { signal: controller.signal });
      if (scope.signal.aborted || current !== generation) return;
      path.value = data.path ?? "";
      path.placeholder = data.path === null ? "Drives" : "Enter a folder path";
      selection.value = data.path ?? "";
      applyListing(data);
      describe(data.folders, data.path === null);
      renderTree(focusList ? data.path : undefined);
    } catch (failure) {
      if (scope.signal.aborted || current !== generation || failure.name === "AbortError") return;
      status.textContent = "Unable to open folder";
      path.setAttribute("aria-invalid", "true");
      showInlineError(error, failure.message);
    } finally {
      if (!scope.signal.aborted && current === generation) {
        loading = false;
        updateControls();
      }
    }
  }

  scope.listen(list, "click", (event) => {
    const button = event.target.closest("button[data-folder-path]");
    if (!button || submitting || scope.signal.aborted) return;
    const node = nodes.get(folderKey(button.dataset.folderPath));
    if (!node?.loaded) { browse(button.dataset.folderPath, true); return; }
    stop();
    loading = false;
    path.value = selection.value = node.path;
    node.expanded = !node.expanded;
    clearInlineError(error);
    path.setAttribute("aria-invalid", "false");
    describe(node.children);
    renderTree(node.path);
    updateControls();
  });
  scope.listen(path, "keydown", (event) => {
    if (event.key === "Enter" && !event.isComposing) { event.preventDefault(); browse(path.value); }
  });
  function checkTypedPath(event) {
    if (submitting || scope.signal.aborted) return;
    stop();
    loading = false;
    selection.value = "";
    list.replaceChildren();
    status.textContent = "Checking folder…";
    clearInlineError(error);
    path.setAttribute("aria-invalid", "false");
    updateControls();
    if (!event.isComposing) pathTimer = window.setTimeout(() => browse(path.value), 350);
  }
  scope.listen(path, "input", checkTypedPath);
  scope.listen(path, "compositionend", checkTypedPath);
  scope.listen(modal, "dialog:close", stop);
  scope.listen(scope.signal, "abort", stop);
  return {
    start() { path.value = ""; path.placeholder = "Enter a folder path"; return browse("", false, true); },
    canSubmit() { return !!selection.value && !loading && !submitting; },
    setSubmitting(value) { submitting = value; if (!scope.signal.aborted) updateControls(); },
  };
}
