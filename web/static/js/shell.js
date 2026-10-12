import { closeModal, confirmAction, openModal, setupDialogs } from "./dialogs.js";
import { beginBusy, clearInlineError, fetchJson, navigateConsole, refreshAfterSuccess, showInlineError, startPolling, toParams } from "./shared.js";

export function libraryIndexStatusText(data) {
  const count = (value) => Number.isSafeInteger(value) && value >= 0;
  if (data?.status === "scanning") {
    const progress = data.progress;
    if (progress?.phase === "discovering") return "Finding albums…";
    if (progress?.phase === "committing") return "Saving library…";
    if (count(progress?.completed) && count(progress?.total)) {
      const completed = Math.min(progress.completed, progress.total).toLocaleString();
      const total = progress.total.toLocaleString();
      if (progress.phase === "reading_tracks") return `Reading ${completed}/${total} tracks`;
      if (progress.phase === "indexing_albums") return `Indexing ${completed}/${total} albums`;
    }
    return "Indexing library…";
  }
  if (data?.status === "ready") {
    const metadata = data.metadata;
    if (!metadata?.phase) return "";
    let label = {
      artists: "Artist sweep",
      album_artists: "Album credits sweep",
      albums: "Album sweep",
    }[metadata.phase] || "Metadata sweep";
    const progress = metadata.progress?.phase === metadata.phase ? metadata.progress : undefined;
    const current = progress?.current;
    const hasPosition = count(progress?.checked) && count(progress?.total);
    if (hasPosition) {
      // Logs number the item being processed, before its lookup finishes.
      const position = Math.min(Math.min(progress.checked, progress.total) + (current ? 1 : 0), progress.total);
      label += ` [${position}/${progress.total}]`;
    }
    const artist = typeof current?.artist === "string" ? current.artist.trim() : "";
    const album = typeof current?.album === "string" ? current.album.trim() : "";
    const target = metadata.phase === "artists" ? artist : [artist, album].filter(Boolean).join(" — ");
    return target ? `${label}: ${target}` : `${label}${hasPosition ? "" : "…"}`;
  }
  if (data?.status === "unconfigured") return "";
  if (data?.status === "missing") return "Music folder unavailable";
  if (data?.status === "error") return "Indexing failed";
  return "Library status unavailable";
}

export function setupNavigation() {
  const listeners = [];
  function listen(target, name, callback) {
    if (!target) return;
    target.addEventListener(name, callback);
    listeners.push(() => target.removeEventListener(name, callback));
  }
  const navigationToggle = document.getElementById("navigation-toggle");
  const navigation = document.getElementById("primary-nav");
  const drawer = document.getElementById("navigation-drawer");
  const backdrop = document.getElementById("navigation-backdrop");
  const close = document.getElementById("navigation-close");
  const nav = navigationToggle?.closest(".nav");
  document.documentElement.classList.add("js");
  if (!navigationToggle || !drawer || !nav) return () => {};
  const desktop = window.matchMedia("(min-width: 980px)");

  function markExpanded(expanded) {
    navigationToggle?.setAttribute("aria-expanded", String(expanded));
    navigationToggle?.setAttribute("aria-label", expanded ? "Close navigation" : "Open navigation");
    nav?.classList.toggle("nav-expanded", expanded);
    document.documentElement.classList.toggle("navigation-open", expanded);
  }

  function closeNavigation() {
    closeModal(drawer);
  }

  function syncLayout() {
    const wasExpanded = navigationToggle.getAttribute("aria-expanded") === "true";
    const hadDrawerFocus = drawer.contains(document.activeElement);
    closeNavigation();
    markExpanded(false);
    drawer.inert = !desktop.matches;
    if (desktop.matches) {
      for (const attribute of ["role", "aria-modal", "aria-hidden", "tabindex"]) drawer.removeAttribute(attribute);
      if (wasExpanded && !nav.inert) {
        (navigation?.querySelector('a[aria-current="page"]') || navigation?.querySelector("a[href]"))?.focus();
      }
    } else {
      drawer.setAttribute("aria-hidden", "true");
      if (hadDrawerFocus && !nav.inert) navigationToggle.focus();
    }
  }

  listen(navigationToggle, "click", () => {
    if (desktop.matches) return;
    if (navigationToggle.getAttribute("aria-expanded") === "true") closeNavigation();
    else {
      drawer.inert = false;
      markExpanded(true);
      openModal(drawer);
      close?.focus();
    }
  });
  listen(backdrop, "click", closeNavigation);
  listen(close, "click", closeNavigation);
  listen(drawer, "click", (event) => {
    if (event.defaultPrevented) return;
    if (event.target.closest("a[href]")) closeNavigation();
  });
  listen(drawer, "dialog:close", () => {
    markExpanded(false);
    drawer.inert = !desktop.matches;
    if (!desktop.matches && !nav.inert) navigationToggle.focus();
  });
  listen(document.getElementById("nav-modal"), "dialog:close", () => {
    if (!desktop.matches && navigationToggle.getAttribute("aria-expanded") !== "true") navigationToggle.focus();
  });
  listen(desktop, "change", syncLayout);
  syncLayout();
  return () => {
    closeNavigation();
    markExpanded(false);
    listeners.forEach((remove) => remove());
  };
}

export function setupShell() {
  setupDialogs();
  let navigationNode;
  let disposeBindings;
  let stopStatus;
  let libraryStatus;
  let disposed = false;

  function bindNavigation() {
    const disposeNavigation = setupNavigation();
    const controller = new AbortController();
    const listeners = [];
    function listen(target, name, callback) {
      if (!target) return;
      target.addEventListener(name, callback);
      listeners.push(() => target.removeEventListener(name, callback));
    }
    const toggle = document.getElementById("nav-menu-toggle");
    const modal = document.getElementById("nav-modal");
    const actionError = document.getElementById("server-action-error");
    listen(toggle, "click", () => { clearInlineError(actionError); openModal(modal); });
    listen(modal, "submit", async (event) => {
      const form = event.target;
      if (event.defaultPrevented || !form.matches('form[action="/logout"]')) return;
      event.preventDefault();
      const button = form.querySelector('button[type="submit"]');
      if (button?.disabled) return;
      clearInlineError(actionError);
      const finish = beginBusy(button, "Signing out");
      try {
        const data = await fetchJson("/logout", toParams(form), { signal: controller.signal });
        if (!controller.signal.aborted) await navigateConsole(data.redirect || "/login", { replace: true, force: true });
      } catch (error) {
        if (error.name !== "AbortError" && !controller.signal.aborted) showInlineError(actionError, error.message);
      } finally {
        finish();
      }
    });
    listen(modal, "click", async (event) => {
      const button = event.target.closest("button[data-action]");
      if (!button || button.disabled) return;
      const action = button.dataset.action;
      if (action !== "restart" && action !== "shutdown") return;
      const label = action === "restart" ? "restart" : "shut down";
      if (!(await confirmAction({
        title: action === "restart" ? "Restart server?" : "Shut down server?",
        message: `Are you sure you want to ${label} the server?`,
        confirmLabel: action === "restart" ? "Restart server" : "Shut down",
        danger: true,
      }))) return;
      if (controller.signal.aborted) return;
      clearInlineError(actionError);
      const finish = beginBusy(button, action === "restart" ? "Restarting server" : "Shutting down server");
      try {
        await fetchJson(`/actions/${action}`, new URLSearchParams(), { signal: controller.signal });
        if (controller.signal.aborted) return;
        closeModal(modal);
      } catch (error) {
        if (error.name !== "AbortError" && !controller.signal.aborted) showInlineError(actionError, error.message);
      } finally {
        finish();
      }
    });
    return () => {
      controller.abort();
      listeners.forEach((remove) => remove());
      disposeNavigation();
    };
  }

  function stopWatchingLibrary() {
    const stop = stopStatus;
    stopStatus = undefined;
    stop?.();
  }

  function updateIndexStatus(data) {
    const status = document.getElementById("library-index-status");
    if (!status) return;
    const text = libraryIndexStatusText(data);
    if (status.textContent !== text) status.textContent = text;
    const hidden = text.length === 0;
    if (status.hidden !== hidden) status.hidden = hidden;
    const state = data?.status === "ready" && data.metadata?.phase ? "metadata" : data?.status || "unavailable";
    if (status.dataset.status !== state) status.dataset.status = state;
  }

  function watchLibrary() {
    if (disposed || !navigationNode || stopStatus) return;
    stopStatus = startPolling(async (signal) => {
      try {
        const data = await fetchJson("/status/library", undefined, { signal });
        if (signal.aborted || disposed || !navigationNode) return;
        // The initial view can be rendered during a scan that finishes before
        // the first poll. Keep that state as layout-free page metadata.
        const wasScanning = libraryStatus?.status === "scanning" ||
          (!libraryStatus && Boolean(document.querySelector('[data-library-scanning="true"]')));
        const wasGatheringMetadata = Boolean(libraryStatus?.metadata?.phase);
        libraryStatus = data;
        updateIndexStatus(data);
        const scanFinished = wasScanning && data?.status && data.status !== "scanning";
        // Refresh Library when stored data changes; retain other mounted pages.
        if ((scanFinished || (data?.status === "ready" && wasGatheringMetadata && !data.metadata?.phase)) && document.body.dataset.page === "library") await refreshAfterSuccess();
      } catch (error) {
        if (error.name !== "AbortError" && !signal.aborted && !disposed) updateIndexStatus({ status: "unavailable" });
      }
    }, 2000);
  }
  function scanStarted() {
    if (disposed || !navigationNode) return;
    libraryStatus = { status: "scanning", progress: { phase: "discovering" } };
    updateIndexStatus(libraryStatus);
    stopWatchingLibrary();
    watchLibrary();
  }
  document.addEventListener("library:scan-started", scanStarted);

  function sync() {
    if (disposed) return;
    const navigation = document.getElementById("primary-nav");
    if (navigation !== navigationNode) {
      stopWatchingLibrary();
      libraryStatus = undefined;
      disposeBindings?.();
      navigationNode = navigation;
      disposeBindings = navigation ? bindNavigation() : undefined;
    }
    if (!navigationNode) stopWatchingLibrary();
    else watchLibrary();
  }
  function dispose() {
    if (disposed) return;
    disposed = true;
    stopWatchingLibrary();
    document.removeEventListener("library:scan-started", scanStarted);
    disposeBindings?.();
  }
  dispose.sync = sync;
  sync();
  return dispose;
}
