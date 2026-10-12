import { closeAllModals } from "./dialogs.js";
import { clearInlineError, configureNavigation, showInlineError } from "./shared.js";

const routes = new Set(["/", "/users", "/library", "/settings", "/logs", "/login", "/setup"]);

export function consoleUrl(value, base) {
  try {
    const url = new URL(value, base);
    return url.origin === new URL(base).origin && routes.has(url.pathname) ? url : null;
  } catch {
    return null;
  }
}

function validView(view) {
  return view && ["title", "page", "page_class", "content", "nav", "footer"].every((key) => typeof view[key] === "string");
}

export function renderConsoleView(view) {
  const navigation = document.getElementById("console-navigation");
  // Keep the sidebar and its animation state across authenticated views. Only
  // signing in/out changes the shell's navigation markup.
  if (navigation && Boolean(navigation.querySelector(".nav")) !== Boolean(view.nav)) navigation.innerHTML = view.nav;
  navigation?.querySelectorAll(".nav-links > a").forEach((link) => {
    const page = { "/": "library", "/users": "users", "/library": "library", "/settings": "settings", "/logs": "logs" }[new URL(link.href).pathname];
    if (page === view.page) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  const footer = document.getElementById("console-footer");
  if (footer && footer.innerHTML !== view.footer) footer.innerHTML = view.footer;
  document.body.className = view.page_class;
  document.body.dataset.page = view.page;
  document.title = `${view.title} · Phonolite`;
  document.getElementById("main").innerHTML = view.content;
}

export function setupConsoleNavigation({ mountPage = () => {}, renderView = renderConsoleView, onDispose = () => {} } = {}) {
  const main = document.getElementById("main");
  let currentUrl = new URL(window.location.href);
  let index = Number(window.history.state?.phonolite?.index) || 0;
  let lifecycle = mountPage();
  let request;
  let sequence = 0;
  let disposed = false;
  let restoringHistory = false;
  let guardPromise;
  let activeNavigation = false;
  let busyElement;
  let deferredRefresh;
  const listeners = [];

  function listen(target, name, callback) {
    target.addEventListener(name, callback);
    listeners.push(() => target.removeEventListener(name, callback));
  }

  function stateFor(url, entryIndex = index) {
    return { phonolite: { url: url.href, index: entryIndex } };
  }

  function visibleUrl(url) {
    return `${url.pathname}${url.hash}`;
  }

  function setLoading(loadingState, region = main) {
    // Loading belongs to the latest navigation request.
    busyElement?.setAttribute("aria-busy", "false");
    busyElement = loadingState ? region : undefined;
    main?.setAttribute("aria-busy", String(loadingState && region === main));
    busyElement?.setAttribute("aria-busy", "true");
  }

  function finishNavigation() {
    activeNavigation = false;
    setLoading(false);
    const pending = deferredRefresh;
    deferredRefresh = undefined;
    if (!pending || disposed) return;
    queueMicrotask(() => {
      // A completed scan can refresh the user's latest library selection after
      // navigation finishes, but must not reload a newly selected other page.
      if (!disposed && currentUrl.pathname === pending.pathname) router.refresh(pending.options);
    });
  }

  function disposePage() {
    if (typeof lifecycle === "function") lifecycle();
    else lifecycle?.dispose?.();
    lifecycle = undefined;
    closeAllModals();
  }

  function restoreHistory() {
    const targetIndex = window.history.state?.phonolite?.index;
    if (Number.isInteger(targetIndex) && targetIndex !== index) {
      restoringHistory = true;
      window.history.go(index - targetIndex);
    } else {
      window.history.replaceState(stateFor(currentUrl), "", visibleUrl(currentUrl));
    }
  }

  async function mayLeave(force) {
    if (force || !lifecycle?.canLeave) return true;
    // Rapid clicks share one unsaved-changes decision. Only the latest target
    // may proceed after that decision, just as with overlapping fetches.
    guardPromise ||= Promise.resolve(lifecycle.canLeave()).finally(() => { guardPromise = undefined; });
    return guardPromise;
  }

  async function navigate(value, { replace = false, history = false, force = false, focus = true, refresh = false } = {}) {
    let url = consoleUrl(value, currentUrl.href);
    if (!url || disposed) return false;
    if (url.href === currentUrl.href && !refresh && !history && !activeNavigation) return true;
    clearInlineError(document.getElementById("navigation-error"));
    const current = ++sequence;
    const historyIndex = window.history.state?.phonolite?.index;
    activeNavigation = true;
    request?.abort();
    setLoading(false);
    let allowed;
    try {
      allowed = await mayLeave(force);
    } catch (error) {
      if (!disposed && current === sequence) {
        finishNavigation();
        if (history) restoreHistory();
        showInlineError(document.getElementById("navigation-error"), `Could not leave this view. ${error.message}`);
      }
      return false;
    }
    if (!allowed || disposed || current !== sequence) {
      if (history && current === sequence) restoreHistory();
      if (current === sequence) finishNavigation();
      return false;
    }
    request = new AbortController();
    const signal = request.signal;
    let fragment;
    try {
      fragment = lifecycle?.navigationFor?.(url);
      setLoading(true, fragment?.busyElement || main);
      let view;
      for (let redirects = 0; redirects < 8; redirects++) {
        const response = await fetch(url.href, {
          method: "GET",
          credentials: "same-origin",
          cache: "no-store",
          signal,
          headers: { Accept: "application/json", "X-Requested-With": "fetch", "X-Console-View": "true", ...fragment?.headers },
        });
        const data = await response.json().catch(() => null);
        if (disposed || current !== sequence || signal.aborted) return false;
        if (data?.redirect) {
          const destination = consoleUrl(data.redirect, url.href);
          if (!destination) throw new Error("The server returned an invalid destination.");
          url = destination;
          // Authentication and other redirected views need a full page view,
          // even when the original request only asked for library regions.
          fragment = undefined;
          setLoading(true);
          // Session changes replace protected history instead of keeping it
          // as a successful destination for browser Back.
          if (["/login", "/setup"].includes(url.pathname)) replace = true;
          continue;
        }
        if (!response.ok) throw new Error(data?.error || `Request failed (${response.status}).`);
        if (!(fragment ? fragment.isValid(data) : validView(data))) throw new Error("The server returned an unexpected view. Please try again.");
        view = data;
        break;
      }
      if (!view) throw new Error("The server redirected too many times.");
      if (disposed || current !== sequence || signal.aborted) return false;
      const previousUrl = currentUrl;
      let focusTarget;
      if (fragment) focusTarget = fragment.apply(view, url);
      else {
        disposePage();
        renderView(view);
      }
      currentUrl = url;
      if (history && Number.isInteger(historyIndex)) index = historyIndex;
      if (history && !replace) {
        window.history.replaceState(stateFor(url), "", visibleUrl(url));
      } else if (replace || url.href === previousUrl.href) {
        window.history.replaceState(stateFor(url), "", visibleUrl(url));
      } else {
        window.history.pushState(stateFor(url, ++index), "", visibleUrl(url));
      }
      if (!fragment) lifecycle = mountPage(view);
      if (url.hash) {
        let anchor = url.hash.slice(1);
        try { anchor = decodeURIComponent(anchor); } catch { /* Keep malformed fragments literal. */ }
        document.getElementById(anchor)?.scrollIntoView?.();
      }
      else if (focus && !fragment) window.scrollTo?.({ top: 0, behavior: "instant" });
      if (focus) (fragment ? focusTarget : main)?.focus({ preventScroll: true });
      return true;
    } catch (error) {
      if (error.name === "AbortError" || disposed || current !== sequence) return false;
      if (history) restoreHistory();
      showInlineError(document.getElementById("navigation-error") || document.getElementById("auth-error"), `Could not load this view. ${error.message}`);
      return false;
    } finally {
      if (current === sequence) {
        finishNavigation();
      }
    }
  }

  const router = {
    navigate,
    refresh(options = {}) {
      if (activeNavigation) {
        if (options.defer) deferredRefresh = { pathname: currentUrl.pathname, options };
        return Promise.resolve(false);
      }
      return navigate(currentUrl.href, { replace: true, focus: false, ...options, refresh: true });
    },
    get currentUrl() { return currentUrl.href; },
    dispose() {
      if (disposed) return;
      disposed = true;
      sequence++;
      request?.abort();
      activeNavigation = false;
      deferredRefresh = undefined;
      disposePage();
      listeners.forEach((remove) => remove());
      setLoading(false);
      configureNavigation(undefined);
      onDispose();
    },
  };
  configureNavigation(router);

  const savedUrl = consoleUrl(window.history.state?.phonolite?.url, currentUrl.href);
  window.history.replaceState(stateFor(currentUrl), "", visibleUrl(currentUrl));
  listen(document, "click", (event) => {
    const link = event.target.closest("a[href]");
    if (!link || link.hasAttribute("data-library-link") || event.defaultPrevented || event.button !== 0 ||
        event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || link.hasAttribute("download") ||
        (link.target && link.target !== "_self")) return;
    // Anchors within the current view keep their normal scroll/focus behavior.
    if (link.getAttribute("href")?.startsWith("#")) return;
    const url = consoleUrl(link.href, currentUrl.href);
    if (!url) return;
    event.preventDefault();
    navigate(url.href);
  });
  listen(window, "popstate", (event) => {
    if (restoringHistory) {
      restoringHistory = false;
      return;
    }
    const selected = consoleUrl(event.state?.phonolite?.url || window.location.href, window.location.href);
    if (selected?.href === currentUrl.href) {
      sequence++;
      request?.abort();
      finishNavigation();
      closeAllModals();
      index = Number(event.state?.phonolite?.index) || index;
      return;
    }
    if (selected) navigate(selected.href, { history: true });
  });
  listen(window, "pagehide", () => {
    sequence++;
    request?.abort();
    activeNavigation = false;
    deferredRefresh = undefined;
    setLoading(false);
  });
  if (savedUrl && savedUrl.pathname === currentUrl.pathname && savedUrl.href !== currentUrl.href) {
    navigate(savedUrl.href, { replace: true, force: true, focus: false });
  }
  return router;
}
