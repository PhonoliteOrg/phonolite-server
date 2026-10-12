import { beginBusy, clearInlineError, fetchJson, showInlineError, startPolling } from "./shared.js";

export function parseLine(line) {
  const match = line.match(/^(\S+)\s+([A-Z]+)\s+(.*)$/);
  return match ? { time: match[1], level: match[2], message: match[3] } : { time: "", level: "INFO", message: line };
}

export function levelClass(level) {
  const key = (level || "").toLowerCase();
  if (key === "warning") return "warn";
  return ["issue", "warn", "error", "debug", "activity"].includes(key) ? key : "info";
}

// Find the retained part of a rolling tail in linear time. Repeated identical
// log lines are valid, so matching only the first line cannot identify it.
export function logTailOverlap(previous, next) {
  if (!previous.length || !next.length) return 0;
  const prefix = new Uint32Array(next.length);
  for (let i = 1, matched = 0; i < next.length; i++) {
    while (matched && next[i] !== next[matched]) matched = prefix[matched - 1];
    if (next[i] === next[matched]) matched++;
    prefix[i] = matched;
  }
  let matched = 0;
  for (const line of previous) {
    if (matched === next.length) matched = prefix[matched - 1];
    while (matched && line !== next[matched]) matched = prefix[matched - 1];
    if (line === next[matched]) matched++;
  }
  return matched;
}

export function setupLogsPage() {
  const logView = document.getElementById("log-view");
  const logPanel = document.getElementById("log-panel");
  if (!logView || !logPanel) return;
  const actions = new AbortController();
  const listeners = [];
  let disposed = false;
  let updatingDebug = false;
  function listen(element, name, callback) {
    if (!element) return;
    element.addEventListener(name, callback);
    listeners.push(() => element.removeEventListener(name, callback));
  }
  const logEmpty = document.getElementById("log-empty");
  const logCard = document.querySelector(".log-card");
  const limit = Math.max(1, Math.min(10000, Math.floor(Number(logCard?.dataset.logLimit) || 1000)));
  const tabs = document.getElementById("log-tabs");
  const follow = document.getElementById("logs-follow");
  const debug = document.getElementById("logs-debug");
  const refresh = document.getElementById("logs-refresh");
  const clear = document.getElementById("logs-clear");
  const spinner = document.getElementById("log-spinner");
  const issues = document.getElementById("indexing-issues");
  const issueCard = document.getElementById("indexing-issues-card");
  const issueCount = document.getElementById("indexing-issue-count");
  const logFilePath = document.getElementById("log-file-path");
  const errorNotice = document.createElement("div");
  errorNotice.className = "inline-error visible hidden";
  errorNotice.setAttribute("role", "status");
  logPanel.prepend(errorNotice);
  const issueErrorNotice = document.createElement("div");
  issueErrorNotice.className = "inline-error visible hidden";
  issueErrorNotice.setAttribute("role", "status");
  issues?.before(issueErrorNotice);
  let view = tabs?.querySelector("[data-log].active")?.dataset.log || "all";
  let renderedLines = [];
  let renderedRows = [];
  let renderedView;
  let revision;
  let request;
  let sequence = 0;
  let fetching = false;
  let loaded = false;
  let clearing = false;
  let issueRequest;
  let issueSequence = 0;
  let fetchingIssues = false;
  let issueHtml = issues?.innerHTML;
  let issuesLoaded = false;
  let stopPolling;
  let suspended = false;

  function render(lines) {
    const next = lines.slice(-limit).map(String);
    if (loaded && renderedView === view && next.length === renderedLines.length && next.every((line, i) => line === renderedLines[i])) return;
    const previousScroll = logView.scrollTop;
    const following = !follow || follow.checked;
    const atBottom = following && previousScroll + logView.clientHeight >= logView.scrollHeight - 20;
    const overlap = renderedView === view ? logTailOverlap(renderedLines, next) : 0;
    const dropped = renderedRows.length - overlap;
    // Retained nodes keep the browser's scroll anchor intact when old rows
    // roll out. Writing the old absolute offset would undo that adjustment.
    renderedRows.slice(0, dropped).forEach((row) => row.remove());
    const rows = renderedRows.slice(dropped);
    const fragment = document.createDocumentFragment();
    for (let i = overlap; i < next.length; i++) {
      const { time, level, message } = parseLine(next[i]);
      const row = document.createElement("div");
      row.className = "log-line";
      const timestamp = document.createElement("span");
      timestamp.className = "log-time";
      timestamp.textContent = time || "—";
      const tag = document.createElement("span");
      tag.className = `log-level ${levelClass(level)}`;
      tag.textContent = level;
      const text = document.createElement("span");
      text.className = "log-message";
      text.textContent = message;
      row.append(timestamp, tag, text);
      fragment.append(row);
      rows.push(row);
    }
    if (next.length > overlap) logView.append(fragment);
    logView.classList.toggle("hidden", next.length === 0);
    logEmpty?.classList.toggle("hidden", next.length !== 0);
    if (following && (atBottom || !loaded)) logView.scrollTop = logView.scrollHeight;
    else if (!overlap) logView.scrollTop = previousScroll;
    renderedLines = next;
    renderedRows = rows;
    renderedView = view;
    loaded = true;
  }

  async function fetchLogs(force = false, signal) {
    if (disposed || view === "issues" || clearing || suspended || document.hidden || (fetching && !force)) return;
    request?.abort();
    const controller = new AbortController();
    request = controller;
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    const current = ++sequence;
    const requestView = view;
    const requestRevision = !force && loaded && renderedView === view ? revision : undefined;
    fetching = true;
    logPanel.setAttribute("aria-busy", "true");
    if (force || !loaded) spinner?.classList.remove("hidden");
    try {
      const revisionQuery = requestRevision ? `&revision=${encodeURIComponent(requestRevision)}` : "";
      const data = await fetchJson(`/logs/tail?lines=${limit}${revisionQuery}&view=${encodeURIComponent(requestView)}`, undefined, { signal: controller.signal });
      if (disposed || current !== sequence || controller.signal.aborted || requestView !== view) return;
      if (data?.unchanged === true) {
        if (!requestRevision || data.revision !== requestRevision) throw new Error("The server returned an invalid log revision.");
        errorNotice.classList.add("hidden");
        return;
      }
      if (!Array.isArray(data?.lines)) throw new Error("The server returned an invalid log response.");
      errorNotice.classList.add("hidden");
      // File timestamps can change for records outside the selected view.
      // Compare actual lines and touch only the rows added or dropped.
      render(data.lines);
      revision = typeof data.revision === "string" ? data.revision : undefined;
    } catch (error) {
      if (current !== sequence || controller.signal.aborted || error.name === "AbortError") return;
      revision = undefined;
      errorNotice.textContent = loaded
        ? `Could not refresh this log view; previous results are still shown. ${error.message}`
        : `Could not load logs. ${error.message}`;
      errorNotice.classList.remove("hidden");
    } finally {
      signal?.removeEventListener("abort", abort);
      if (request === controller) request = undefined;
      if (current === sequence) {
        fetching = false;
        logPanel.setAttribute("aria-busy", "false");
        spinner?.classList.add("hidden");
      }
    }
  }

  async function fetchIssues(force = false, signal) {
    if (disposed || !issues || view !== "issues" || clearing || suspended || document.hidden || (fetchingIssues && !force)) return;
    issueRequest?.abort();
    const controller = new AbortController();
    issueRequest = controller;
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    const current = ++issueSequence;
    fetchingIssues = true;
    const finish = force || !issuesLoaded ? beginBusy(issueCard, "Loading indexing issues") : () => {};
    issues.setAttribute("aria-busy", "true");
    try {
      const data = await fetchJson("/logs/issues", undefined, { signal: controller.signal });
      if (disposed || current !== issueSequence || controller.signal.aborted || view !== "issues") return;
      if (typeof data?.html !== "string" || !Number.isInteger(data.total) || data.total < 0) {
        throw new Error("The server returned an invalid indexing issues response.");
      }
      issueErrorNotice.classList.add("hidden");
      if (data.html !== issueHtml) {
        issues.innerHTML = data.html;
        issueHtml = data.html;
      }
      if (issueCount) issueCount.textContent = `${data.total} files`;
      issuesLoaded = true;
    } catch (error) {
      if (current !== issueSequence || controller.signal.aborted || error.name === "AbortError") return;
      issueErrorNotice.textContent = `Could not refresh indexing issues; previous results are still shown. ${error.message}`;
      issueErrorNotice.classList.remove("hidden");
    } finally {
      signal?.removeEventListener("abort", abort);
      if (issueRequest === controller) issueRequest = undefined;
      finish();
      if (current === issueSequence) {
        fetchingIssues = false;
        issues.setAttribute("aria-busy", "false");
      }
    }
  }

  function updateTabs() {
    logCard?.classList.toggle("hidden", view === "issues");
    issueCard?.classList.toggle("hidden", view !== "issues");
    tabs?.querySelectorAll("button[data-log]").forEach((button) => {
      const active = button.dataset.log === view;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
      if (active && logFilePath) logFilePath.textContent = button.dataset.logPath || "";
    });
  }

  function fetchActiveView(force = false, signal) {
    return view === "issues" ? fetchIssues(force, signal) : fetchLogs(force, signal);
  }

  function startActivePolling() {
    stopPolling?.();
    stopPolling = startPolling((signal) => fetchActiveView(false, signal), view === "issues" ? 5000 : 1000);
  }

  function abortRequests() {
    request?.abort();
    issueRequest?.abort();
    sequence++;
    issueSequence++;
    fetching = false;
    fetchingIssues = false;
    logPanel.setAttribute("aria-busy", "false");
    issues?.setAttribute("aria-busy", "false");
    spinner?.classList.add("hidden");
  }

  listen(tabs, "click", (event) => {
    const button = event.target.closest("button[data-log]");
    if (!button || button.dataset.log === view) return;
    abortRequests();
    view = button.dataset.log || "all";
    revision = undefined;
    loaded = false;
    errorNotice.classList.add("hidden");
    updateTabs();
    startActivePolling();
  });
  listen(refresh, "click", () => fetchActiveView(true));
  listen(follow, "change", () => {
    if (follow.checked) logView.scrollTop = logView.scrollHeight;
  });
  listen(debug, "change", async () => {
    if (disposed || updatingDebug) return;
    updatingDebug = true;
    clearInlineError(document.getElementById("logs-action-error"));
    const enabled = debug.checked;
    const params = new URLSearchParams();
    if (enabled) params.set("enabled", "true");
    const finish = beginBusy(debug.closest("label") || debug.parentElement, "Updating logging");
    debug.disabled = true;
    try {
      await fetchJson("/settings/logging/debug", params, { signal: actions.signal });
    } catch (error) {
      if (disposed || error.name === "AbortError") return;
      debug.checked = !enabled;
      showInlineError(document.getElementById("logs-action-error"), error.message);
    } finally {
      finish();
      updatingDebug = false;
      debug.disabled = false;
    }
  });
  listen(clear, "click", async () => {
    if (disposed || clearing) return;
    clearInlineError(document.getElementById("logs-action-error"));
    const finish = beginBusy(clear, "Clearing logs");
    clear.disabled = true;
    clearing = true;
    abortRequests();
    try {
      await fetchJson("/logs/clear", new URLSearchParams(), { signal: actions.signal });
      if (disposed || actions.signal.aborted) return;
      revision = undefined;
      render([]);
    } catch (error) {
      if (!disposed && error.name !== "AbortError") showInlineError(document.getElementById("logs-action-error"), error.message);
    } finally {
      finish();
      clearing = false;
      clear.disabled = false;
      if (!disposed) fetchActiveView(true);
    }
  });
  listen(window, "pagehide", () => {
    suspended = true;
    abortRequests();
  });
  listen(window, "pageshow", () => { suspended = false; });
  listen(document, "visibilitychange", () => {
    if (document.hidden) abortRequests();
  });
  updateTabs();
  startActivePolling();
  return () => {
    disposed = true;
    stopPolling?.();
    abortRequests();
    actions.abort();
    listeners.forEach((remove) => remove());
    renderedLines = [];
    renderedRows = [];
    revision = undefined;
    errorNotice.remove();
    issueErrorNotice.remove();
  };
}
