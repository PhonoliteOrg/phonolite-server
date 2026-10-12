export function clearInlineError(element) {
  if (!element) return;
  element.textContent = "";
  element.classList.remove("visible");
}

export function showInlineError(element, message) {
  if (!element) return;
  element.setAttribute("role", "alert");
  element.textContent = message;
  element.classList.add("visible");
}

export function toParams(form) {
  const params = new URLSearchParams();
  for (const [key, value] of new FormData(form)) params.append(key, value.toString());
  return params;
}

export async function fetchJson(url, params, options = {}) {
  const requestNavigation = navigation;
  const requestGeneration = navigationGeneration;
  const response = await fetch(url, {
    credentials: "same-origin",
    method: params === undefined ? "GET" : "POST",
    ...options,
    headers: {
      Accept: "application/json",
      "X-Requested-With": "fetch",
      ...(params === undefined ? {} : { "Content-Type": "application/x-www-form-urlencoded" }),
      ...options.headers,
    },
    ...(params === undefined ? {} : { body: params }),
  });
  const data = await response.json().catch(() => null);
  if (options.signal?.aborted) throw new DOMException("The request was canceled.", "AbortError");
  const redirectedPath = response.url ? new URL(response.url, window.location.href).pathname : "";
  const authRequest = requestNavigation && ["/login", "/setup", "/logout"].includes(new URL(url, window.location.href).pathname);
  const authRedirect = ["/login", "/setup"].includes(data?.redirect) &&
    !["/login", "/setup", "/logout"].includes(new URL(url, window.location.href).pathname);
  if (requestNavigation && !authRequest && (authRedirect || response.status === 401 || (response.redirected && ["/login", "/setup"].includes(redirectedPath)))) {
    if (requestNavigation !== navigation || requestGeneration !== navigationGeneration) throw new DOMException("The view has changed.", "AbortError");
    if (!authenticationNavigation) {
      const pending = { router: requestNavigation, generation: requestGeneration };
      authenticationNavigation = pending;
      const destination = authRedirect ? data.redirect : (["/login", "/setup"].includes(redirectedPath) ? redirectedPath : "/login");
      pending.promise = Promise.resolve().then(() => {
        if (pending.router !== navigation || pending.generation !== navigationGeneration || options.signal?.aborted) return false;
        return pending.router.navigate(destination, { replace: true, force: true });
      }).finally(() => { if (authenticationNavigation === pending) authenticationNavigation = undefined; });
    }
    await authenticationNavigation.promise;
    throw new DOMException("The session has ended.", "AbortError");
  }
  if (!response.ok) throw new Error(data?.error || `Request failed (${response.status}).`);
  // An expired session can redirect an AJAX request to the HTML login page.
  if (data === null) throw new Error("The server returned an unexpected response. Refresh the page and sign in again if needed.");
  return data;
}

let navigation;
let navigationGeneration = 0;
let authenticationNavigation;

export function configureNavigation(router) {
  if (navigation !== router) {
    navigationGeneration++;
    authenticationNavigation = undefined;
  }
  navigation = router;
}

export function navigateConsole(url, options) {
  return navigation ? navigation.navigate(url, options) : Promise.resolve(false);
}

export function refreshAfterSuccess(options) {
  return navigation ? navigation.refresh(options) : Promise.resolve(false);
}

const busyElements = new WeakMap();

// Each request owns its completion callback, so an old request cannot hide a
// spinner belonging to a newer request on the same control.
export function beginBusy(element, label = "Loading") {
  if (!element) return () => {};
  let state = busyElements.get(element);
  if (!state) {
    const indicator = document.createElement("span");
    indicator.className = "busy-indicator";
    indicator.setAttribute("role", "status");
    indicator.setAttribute("aria-label", label);
    const spinner = document.createElement("span");
    spinner.className = "spinner";
    spinner.setAttribute("aria-hidden", "true");
    indicator.append(spinner);
    state = { count: 0, indicator, disabled: element.disabled, busy: element.getAttribute("aria-busy") };
    busyElements.set(element, state);
    element.append(indicator);
    element.setAttribute("aria-busy", "true");
    element.classList.add("is-busy");
    if (element.tagName === "BUTTON" || element.tagName === "INPUT") element.disabled = true;
  }
  state.count++;
  let finished = false;
  return () => {
    if (finished) return;
    finished = true;
    if (--state.count > 0) return;
    state.indicator.remove();
    element.classList.remove("is-busy");
    if (state.busy == null) element.removeAttribute("aria-busy");
    else element.setAttribute("aria-busy", state.busy);
    if (element.tagName === "BUTTON" || element.tagName === "INPUT") element.disabled = state.disabled;
    busyElements.delete(element);
  };
}

// Poll only while this document is visible, never overlap requests, and stop
// requests when navigating away. A page restored from the back/forward cache resumes.
export function startPolling(callback, interval, { immediate = true } = {}) {
  let timer;
  let controller;
  let stopped = false;
  let suspended = false;
  let running = false;

  async function tick() {
    window.clearTimeout(timer);
    if (stopped || suspended || document.hidden || running) return;
    running = true;
    controller = new AbortController();
    try {
      await callback(controller.signal);
    } catch (error) {
      // Each caller decides how to present request failures. An interrupted
      // request is expected when a page is hidden or unloaded.
    } finally {
      running = false;
      if (!stopped && !suspended && !document.hidden) timer = window.setTimeout(tick, interval);
    }
  }

  function pause() {
    window.clearTimeout(timer);
    controller?.abort();
  }

  function visibilityChanged() {
    if (document.hidden) pause();
    else tick();
  }

  function pageHidden() {
    suspended = true;
    pause();
  }

  function pageShown() {
    suspended = false;
    tick();
  }

  document.addEventListener("visibilitychange", visibilityChanged);
  window.addEventListener("pagehide", pageHidden);
  window.addEventListener("pageshow", pageShown);
  if (immediate) tick();
  else timer = window.setTimeout(tick, interval);

  return () => {
    stopped = true;
    pause();
    document.removeEventListener("visibilitychange", visibilityChanged);
    window.removeEventListener("pagehide", pageHidden);
    window.removeEventListener("pageshow", pageShown);
  };
}
