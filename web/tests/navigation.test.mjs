import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { consoleUrl, renderConsoleView, setupConsoleNavigation } from "../static/js/navigation.js";
import { beginBusy, configureNavigation, fetchJson, navigateConsole, refreshAfterSuccess } from "../static/js/shared.js";

// Exercise real navigation and request code with small DOM/history doubles.
// These tests do not launch a browser, an application, or an HTTP server.
class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) {
    for (const callback of [...(this.listeners.get(name) || [])]) callback(event);
  }
  dispatchEvent(event) { this.emit(event.type, event); return true; }
}

class Element extends Events {
  attrs = new Map();
  dataset = {};
  classes = new Set();
  children = [];
  innerHTML = "";
  textContent = "";
  classList = {
    add: (...names) => names.forEach((name) => this.classes.add(name)),
    remove: (...names) => names.forEach((name) => this.classes.delete(name)),
    contains: (name) => this.classes.has(name) || this.className?.split(" ").includes(name) || false,
    toggle: (name, force) => {
      const include = force ?? !this.classes.has(name);
      if (include) this.classes.add(name);
      else this.classes.delete(name);
      return include;
    },
  };
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  hasAttribute(name) { return this.attrs.has(name); }
  removeAttribute(name) { this.attrs.delete(name); }
  append(...elements) { this.children.push(...elements); }
  prepend(...elements) { this.children.unshift(...elements); }
  replaceChildren(...elements) { this.children = elements; this.innerHTML = ""; }
  remove() { this.removed = true; }
  focus() { this.focused = true; }
  querySelector() { return null; }
  querySelectorAll() { return []; }
  matches(selector) { return selector.split(",").some((tag) => tag.trim().toUpperCase() === this.tagName); }
  closest() { return null; }
}

const originals = {
  window: globalThis.window,
  document: globalThis.document,
  fetch: globalThis.fetch,
};
const cleanups = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});

const settle = () => new Promise((resolve) => setImmediate(resolve));
const view = (page, content = page) => ({
  title: page,
  page,
  page_class: `${page}-page`,
  content,
  nav: "navigation",
  footer: "footer",
});
const response = (body, { ok = true, status = 200, url = "", redirected = false } = {}) => ({
  ok, status, url, redirected, json: async () => body,
});

function harness({ href = "https://console.example/library", state = null, lifecycle, onDispose } = {}) {
  const document = new Events();
  const window = new Events();
  const main = new Element();
  main.innerHTML = "initial view";
  const navigationError = new Element();
  const body = new Element();
  body.dataset.page = new URL(href).pathname.slice(1) || "overview";
  document.body = body;
  document.documentElement = new Element();
  document.getElementById = (id) => ({ main, "navigation-error": navigationError })[id] || null;
  document.querySelector = (selector) => selector === ".content" ? main : null;
  document.createElement = (tag) => { const node = new Element(); node.tagName = tag.toUpperCase(); return node; };
  document.querySelectorAll = () => [];
  document.title = "Initial · Phonolite";
  window.location = {
    href,
    get origin() { return new URL(this.href).origin; },
    get pathname() { return new URL(this.href).pathname; },
    get search() { return new URL(this.href).search; },
    get hash() { return new URL(this.href).hash; },
    reloads: 0,
    assignments: [],
    reload() { this.reloads++; },
    assign(url) { this.assignments.push(url); this.href = new URL(url, this.href).href; },
  };
  const scrolls = [];
  window.scrollTo = (options) => scrolls.push(options);
  window.setTimeout = setTimeout;
  window.clearTimeout = clearTimeout;
  const entries = [{ state, href }];
  let entryIndex = 0;
  const history = {
    pushes: 0,
    replacements: 0,
    moves: [],
    get state() { return entries[entryIndex].state; },
    pushState(nextState, _title, nextUrl) {
      const nextHref = new URL(nextUrl, window.location.href).href;
      entries.splice(entryIndex + 1, Infinity, { state: nextState, href: nextHref });
      entryIndex++;
      window.location.href = nextHref;
      this.pushes++;
    },
    replaceState(nextState, _title, nextUrl) {
      const nextHref = new URL(nextUrl, window.location.href).href;
      entries[entryIndex] = { state: nextState, href: nextHref };
      window.location.href = nextHref;
      this.replacements++;
    },
    go(delta) {
      this.moves.push(delta);
      entryIndex += delta;
      assert.ok(entries[entryIndex], "history recovery must choose an existing entry");
      window.location.href = entries[entryIndex].href;
      window.emit("popstate", { state: entries[entryIndex].state });
    },
  };
  window.history = history;
  const requests = [];
  Object.assign(globalThis, {
    document, window,
    fetch: (url, options) => {
      const pending = Promise.withResolvers();
      requests.push({ url: String(url), options, ...pending });
      return pending.promise;
    },
  });
  const renders = [];
  const mounts = [];
  let disposals = 0;
  const router = setupConsoleNavigation({
    onDispose,
    renderView(nextView) { renders.push(nextView); main.innerHTML = nextView.content; body.dataset.page = nextView.page; },
    mountPage(nextView) {
      mounts.push(nextView);
      return lifecycle?.(nextView) || (() => { disposals++; });
    },
  });
  cleanups.push(() => router.dispose());
  return {
    document, window, main, navigationError, requests, renders, mounts, router, history, scrolls,
    get disposals() { return disposals; },
    back() { history.go(-1); },
    click(href, changes = {}, linkChanges = {}) {
      const link = new Element();
      link.href = new URL(href, window.location.href).href;
      link.setAttribute("href", href);
      link.target = "";
      if (linkChanges.target) link.target = linkChanges.target;
      for (const attribute of linkChanges.attributes || []) link.setAttribute(attribute, "");
      link.closest = (selector) => selector === "a[href]" ? link : null;
      const event = { button: 0, target: link, preventDefault() { this.defaultPrevented = true; }, ...changes };
      document.emit("click", event);
      return event;
    },
  };
}

test("console navigation accepts only supported same-origin views", () => {
  const base = "https://console.example/library";
  for (const route of ["/", "/users", "/library", "/settings", "/logs", "/login", "/setup"]) {
    assert.equal(consoleUrl(`${route}?message=saved`, base).pathname, route);
  }
  assert.equal(consoleUrl("/library?filter=albums#results", base).search, "?filter=albums");
  for (const route of ["https://other.example/users", "/assets/js/app.js", "/status/library", "/logs/tail", "/users/delete", "javascript:alert(1)"]) {
    assert.equal(consoleUrl(route, base), null);
  }
});

function regionHarness() {
  const region = new Element();
  region.innerHTML = "initial results";
  let disposals = 0;
  let applications = 0;
  const subject = harness({ lifecycle: () => ({
    navigationFor(url) {
      if (url.pathname !== "/library") return null;
      return {
        headers: { "X-Fragment": "true" },
        busyElement: region,
        isValid: (data) => typeof data?.content === "string",
        apply(data) { applications++; region.innerHTML = data.content; },
      };
    },
    dispose() { disposals++; },
  }) });
  return { ...subject, region, get pageDisposals() { return disposals; }, get applications() { return applications; } };
}

test("library navigation replaces only its region and keeps page focus, scroll, and history", async () => {
  const subject = regionHarness();
  const { router, requests, main, region, renders, mounts, scrolls, history, window } = subject;
  const navigation = router.navigate("/library?search=collab&filter=albums");
  await settle();
  assert.equal(requests[0].options.headers["X-Fragment"], "true");
  assert.equal(region.getAttribute("aria-busy"), "true");
  assert.equal(main.getAttribute("aria-busy"), "false");
  assert.equal(region.innerHTML, "initial results");
  requests[0].resolve(response({ content: "collaboration albums" }));
  assert.equal(await navigation, true);
  assert.equal(region.innerHTML, "collaboration albums");
  assert.equal(main.innerHTML, "initial view");
  assert.equal(main.focused, undefined);
  assert.equal(region.getAttribute("aria-busy"), "false");
  assert.equal(renders.length, 0);
  assert.equal(mounts.length, 1);
  assert.equal(subject.pageDisposals, 0);
  assert.equal(main.classList.contains("view-enter"), false);
  assert.deepEqual(scrolls, []);
  assert.equal(window.location.href, "https://console.example/library");
  assert.equal(history.state.phonolite.url, router.currentUrl);

  subject.back();
  await settle();
  assert.equal(requests[1].options.headers["X-Fragment"], "true");
  requests[1].resolve(response({ content: "initial results" }));
  await settle();
  assert.equal(region.innerHTML, "initial results");
  assert.equal(router.currentUrl, "https://console.example/library");
  assert.equal(mounts.length, 1);
});

test("superseded and malformed region responses cannot change content or history", async () => {
  const subject = regionHarness();
  const { router, requests, region, history } = subject;
  const older = router.navigate("/library?search=older");
  await settle();
  const newer = router.navigate("/library?search=newer");
  await settle();
  assert.equal(requests[0].options.signal.aborted, true);
  requests[1].resolve(response({ content: "new results" }));
  assert.equal(await newer, true);
  requests[0].resolve(response({ content: "stale results" }));
  assert.equal(await older, false);
  assert.equal(region.innerHTML, "new results");
  assert.equal(subject.applications, 1);
  const pushes = history.pushes;
  const invalid = router.navigate("/library?search=invalid");
  await settle();
  requests[2].resolve(response({ unexpected: "payload" }));
  assert.equal(await invalid, false);
  assert.equal(subject.applications, 1);
  assert.equal(region.innerHTML, "new results");
  assert.equal(history.pushes, pushes);
  assert.equal(region.getAttribute("aria-busy"), "false");
});

test("expired region navigation switches to a complete authentication view", async () => {
  const subject = regionHarness();
  const { router, requests, main, region, mounts, renders } = subject;
  const navigation = router.navigate("/library?filter=albums");
  await settle();
  requests[0].resolve(response({ redirect: "/login" }));
  await settle();
  assert.equal(requests[1].options.headers["X-Fragment"], undefined);
  assert.equal(main.getAttribute("aria-busy"), "true");
  assert.equal(region.getAttribute("aria-busy"), "false");
  requests[1].resolve(response({ ...view("login", "sign in"), nav: "", footer: "" }));
  assert.equal(await navigation, true);
  assert.equal(main.innerHTML, "sign in");
  assert.equal(renders.length, 1);
  assert.equal(mounts.length, 2);
  assert.equal(subject.pageDisposals, 1);
  assert.equal(subject.applications, 0);
  assert.equal(main.classList.contains("view-enter"), false);
});

test("selecting the current view does no work while explicit refresh still updates its region", async () => {
  const { router, requests, region } = regionHarness();
  assert.equal(await router.navigate(router.currentUrl), true);
  assert.equal(requests.length, 0);
  const refresh = router.refresh();
  await settle();
  assert.equal(requests.length, 1);
  requests[0].resolve(response({ content: "refreshed results" }));
  assert.equal(await refresh, true);
  assert.equal(region.innerHTML, "refreshed results");
});

test("a deferred scan refresh follows the latest library selection without interrupting navigation", async () => {
  const { router, requests, region, mounts, history } = regionHarness();
  const navigation = router.navigate("/library?search=latest&filter=albums");
  await settle();
  assert.equal(await router.refresh({ defer: true }), false);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.signal.aborted, false);
  requests[0].resolve(response({ content: "scan still running" }));
  assert.equal(await navigation, true);
  await settle();
  assert.equal(requests.length, 2);
  assert.equal(requests[1].url, "https://console.example/library?search=latest&filter=albums");
  assert.equal(requests[1].options.headers["X-Fragment"], "true");
  requests[1].resolve(response({ content: "finished scan results" }));
  await settle();
  assert.equal(region.innerHTML, "finished scan results");
  assert.equal(mounts.length, 1);
  assert.equal(history.pushes, 1);
});

test("a deferred library refresh is discarded when navigation selects another page", async () => {
  const { router, requests, main } = regionHarness();
  const navigation = router.navigate("/settings");
  await settle();
  await router.refresh({ defer: true });
  requests[0].resolve(response(view("settings", "settings form")));
  assert.equal(await navigation, true);
  await settle();
  assert.equal(requests.length, 1);
  assert.equal(main.innerHTML, "settings form");
});

test("router disposal releases its persistent shell exactly once", () => {
  let disposals = 0;
  const { router } = harness({ onDispose: () => { disposals++; } });
  router.dispose();
  router.dispose();
  assert.equal(disposals, 1);
});

test("first server view mounts immediately and initial query parameters move into history state", () => {
  const { router, mounts, requests, window, history } = harness({ href: "https://console.example/library?filter=albums" });
  assert.equal(mounts.length, 1);
  assert.equal(requests.length, 0, "the first server-rendered view needs no duplicate fetch");
  assert.equal(router.currentUrl, "https://console.example/library?filter=albums");
  assert.equal(window.location.href, "https://console.example/library");
  assert.equal(history.state.phonolite.url, router.currentUrl);
});

test("navigation preserves the view while loading and only the latest response mounts", async () => {
  const subject = harness();
  const { router, requests, main, window, history, mounts } = subject;
  const older = router.navigate("/library?filter=albums");
  await settle();
  assert.equal(main.innerHTML, "initial view");
  assert.equal(main.getAttribute("aria-busy"), "true");
  assert.equal(subject.disposals, 0, "do not dispose a usable view until the next one is ready");
  const newer = router.navigate("/users?message=created");
  await settle();
  assert.equal(requests[0].options.signal.aborted, true);
  assert.equal(requests[1].options.method, "GET");
  assert.equal(requests[1].options.headers["X-Console-View"], "true");
  assert.equal(requests[1].options.credentials, "same-origin");
  requests[1].resolve(response(view("users", "latest users")));
  assert.equal(await newer, true);
  requests[0].resolve(response(view("library", "old albums")));
  assert.equal(await older, false);
  assert.equal(main.innerHTML, "latest users");
  assert.equal(main.getAttribute("aria-busy"), "false");
  assert.equal(main.focused, true);
  assert.equal(subject.disposals, 1);
  assert.equal(mounts.length, 2);
  assert.equal(window.location.href, "https://console.example/users");
  assert.equal(history.state.phonolite.url, "https://console.example/users?message=created");
  assert.equal(history.pushes, 1);
  assert.equal(window.location.reloads, 0);
});

test("failed or malformed view responses preserve content, lifecycle, and history", async () => {
  const subject = harness();
  const { router, requests, main, history, window } = subject;
  const initialState = history.state;
  for (const result of [
    response({ error: "Unavailable" }, { ok: false, status: 503 }),
    response({ content: "missing view metadata" }),
  ]) {
    const navigation = router.navigate("/settings");
    await settle();
    requests.at(-1).resolve(result);
    assert.equal(await navigation, false);
    assert.equal(main.innerHTML, "initial view");
    assert.equal(main.getAttribute("aria-busy"), "false");
    assert.equal(subject.disposals, 0);
    assert.equal(subject.mounts.length, 1);
    assert.deepEqual(history.state, initialState);
    assert.equal(window.location.href, "https://console.example/library");
  }
  assert.match(subject.navigationError.textContent, /Could not load this view/);
  assert.equal(subject.navigationError.classList.contains("visible"), true);
  assert.deepEqual(main.children, []);
});

test("unsaved changes share one leave decision and allow only the latest destination", async () => {
  const decision = Promise.withResolvers();
  let decisions = 0;
  let disposals = 0;
  const { router, requests, renders } = harness({
    href: "https://console.example/settings",
    lifecycle: () => ({
      canLeave() { decisions++; return decision.promise; },
      dispose() { disposals++; },
    }),
  });
  const users = router.navigate("/users");
  const library = router.navigate("/library?filter=albums");
  assert.equal(requests.length, 0);
  assert.equal(decisions, 1);
  decision.resolve(true);
  await settle();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "https://console.example/library?filter=albums");
  requests[0].resolve(response(view("library")));
  assert.equal(await users, false);
  assert.equal(await library, true);
  assert.equal(renders.length, 1);
  assert.equal(disposals, 1);
});

test("canceling a leave guard keeps the current view and makes no request", async () => {
  const { router, requests, history, main } = harness({
    href: "https://console.example/settings",
    lifecycle: () => ({ canLeave: async () => false, dispose() {} }),
  });
  assert.equal(await router.navigate("/users"), false);
  assert.equal(requests.length, 0);
  assert.equal(history.pushes, 0);
  assert.equal(main.innerHTML, "initial view");
});

test("a rejected leave guard keeps content and reports the failure without fetching", async () => {
  const { router, requests, history, main, navigationError } = harness({
    href: "https://console.example/settings",
    lifecycle: () => ({ canLeave: async () => { throw new Error("Could not confirm discard"); }, dispose() {} }),
  });
  assert.equal(await router.navigate("/users"), false);
  assert.equal(requests.length, 0);
  assert.equal(history.pushes, 0);
  assert.equal(main.innerHTML, "initial view");
  assert.equal(main.getAttribute("aria-busy"), "false");
  assert.match(navigationError.textContent, /Could not confirm discard/);
  assert.deepEqual(main.children, []);
});

test("background refresh cannot interrupt a pending user navigation", async () => {
  const { router, requests, main } = harness();
  const navigation = router.navigate("/users");
  await settle();
  assert.equal(await router.refresh(), false);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.signal.aborted, false);
  requests[0].resolve(response(view("users", "requested users")));
  assert.equal(await navigation, true);
  assert.equal(main.innerHTML, "requested users");
});

test("background refresh cannot replace the destination while a leave guard is pending", async () => {
  const decision = Promise.withResolvers();
  const { router, requests, main } = harness({
    href: "https://console.example/settings",
    lifecycle: () => ({ canLeave: () => decision.promise, dispose() {} }),
  });
  const navigation = router.navigate("/users");
  const refresh = router.refresh();
  decision.resolve(true);
  await settle();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "https://console.example/users");
  requests[0].resolve(response(view("users", "requested users")));
  assert.equal(await navigation, true);
  assert.equal(await refresh, false);
  assert.equal(main.innerHTML, "requested users");
});

test("Back restores hidden query state and a failed Back returns to the rendered entry", async () => {
  const { router, requests, main, history, window, back } = harness();
  const albums = router.navigate("/library?filter=albums&page=2");
  await settle();
  requests.at(-1).resolve(response(view("library", "album page 2")));
  assert.equal(await albums, true);
  const users = router.navigate("/users");
  await settle();
  requests.at(-1).resolve(response(view("users", "users")));
  assert.equal(await users, true);
  back();
  await settle();
  assert.equal(requests.at(-1).url, "https://console.example/library?filter=albums&page=2");
  requests.at(-1).resolve(response(view("library", "restored album page 2")));
  await settle();
  assert.equal(main.innerHTML, "restored album page 2");
  assert.equal(window.location.href, "https://console.example/library");
  assert.equal(history.state.phonolite.url, "https://console.example/library?filter=albums&page=2");
  assert.equal(history.pushes, 2, "Back replaces the selected entry instead of pushing another one");
  back();
  await settle();
  assert.equal(requests.at(-1).url, "https://console.example/library");
  const requestsBeforeFailure = requests.length;
  requests.at(-1).reject(new Error("offline"));
  await settle();
  assert.equal(main.innerHTML, "restored album page 2");
  assert.equal(history.state.phonolite.url, "https://console.example/library?filter=albums&page=2");
  assert.equal(router.currentUrl, history.state.phonolite.url);
  assert.equal(history.moves.at(-1), 1);
  assert.equal(requests.length, requestsBeforeFailure, "recovery does not refetch the already rendered entry");
  assert.equal(window.location.reloads, 0);
});

test("Forward to the current rendered view cancels a pending Back response", async () => {
  const { router, requests, main, history, back, window } = harness();
  const users = router.navigate("/users");
  await settle();
  requests.at(-1).resolve(response(view("users", "current users")));
  await users;
  back();
  await settle();
  const pendingBack = requests.at(-1);
  assert.equal(main.getAttribute("aria-busy"), "true");
  history.go(1);
  assert.equal(pendingBack.options.signal.aborted, true);
  assert.equal(main.getAttribute("aria-busy"), "false");
  pendingBack.resolve(response(view("library", "late library")));
  await settle();
  assert.equal(main.innerHTML, "current users");
  assert.equal(router.currentUrl, "https://console.example/users");
  assert.equal(window.location.href, router.currentUrl);
});

test("native anchor history keeps the current hidden query state without fetching or guarding", async () => {
  let guards = 0;
  const { router, requests, history, back, window } = harness({
    href: "https://console.example/library?search=blue+sky",
    lifecycle: () => ({ canLeave() { guards++; return false; }, dispose() {} }),
  });
  history.pushState(history.state, "", "#main");
  back();
  await settle();
  assert.equal(requests.length, 0);
  assert.equal(guards, 0);
  assert.equal(router.currentUrl, "https://console.example/library?search=blue+sky");
  assert.equal(history.state.phonolite.url, router.currentUrl);
  assert.equal(window.location.href, "https://console.example/library");
});

test("rejecting a Back leave guard returns to the current rendered history entry", async () => {
  const { router, requests, main, history, back, navigationError } = harness({
    lifecycle: (nextView) => nextView?.page === "users" ? {
      canLeave: async () => { throw new Error("Could not confirm discard"); }, dispose() {},
    } : undefined,
  });
  const users = router.navigate("/users");
  await settle();
  requests.at(-1).resolve(response(view("users", "current users")));
  await users;
  const previousRequests = requests.length;
  back();
  await settle();
  assert.equal(requests.length, previousRequests);
  assert.equal(main.innerHTML, "current users");
  assert.equal(history.state.phonolite.url, "https://console.example/users");
  assert.equal(history.moves.at(-1), 1);
  assert.match(navigationError.textContent, /Could not confirm discard/);
  assert.deepEqual(main.children, []);
});

test("reloading a clean URL restores matching saved search state in place", async () => {
  const href = "https://console.example/library";
  const saved = `${href}?search=blue+sky&page=3`;
  const { requests, main, history, window, router } = harness({
    href, state: { phonolite: { url: saved, index: 4 } },
  });
  await settle();
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, saved);
  requests[0].resolve(response(view("library", "restored search")));
  await settle();
  assert.equal(main.innerHTML, "restored search");
  assert.equal(router.currentUrl, saved);
  assert.equal(history.state.phonolite.index, 4);
  assert.equal(history.pushes, 0);
  assert.equal(window.location.href, href);
  assert.equal(main.focused, undefined, "state restoration does not steal focus");
});

test("the root route follows the Library redirect in place", async () => {
  const { router, requests, window, main } = harness({ href: "https://console.example/users" });
  const navigation = router.navigate("/");
  await settle();
  assert.equal(requests[0].url, "https://console.example/");
  requests[0].resolve(response({ redirect: "/library" }));
  await settle();
  assert.equal(requests[1].url, "https://console.example/library");
  requests[1].resolve(response(view("library", "library content")));
  assert.equal(await navigation, true);
  assert.equal(main.innerHTML, "library content");
  assert.equal(window.location.href, "https://console.example/library");
  assert.equal(window.location.reloads, 0);
});

test("protected-view redirects load Login in place and replace the selected history entry", async () => {
  const { router, requests, window, history, main, mounts } = harness({ href: "https://console.example/users" });
  const navigation = router.navigate("/settings");
  await settle();
  requests[0].resolve(response({ redirect: "/login" }, { ok: false, status: 401 }));
  await settle();
  assert.equal(requests[1].url, "https://console.example/login");
  assert.equal(requests[1].options.headers["X-Console-View"], "true");
  requests[1].resolve(response(view("login", "sign in")));
  assert.equal(await navigation, true);
  assert.equal(main.innerHTML, "sign in");
  assert.equal(window.location.href, "https://console.example/login");
  assert.equal(history.pushes, 0);
  assert.equal(mounts.at(-1).page, "login");
  assert.equal(window.location.reloads, 0);
  assert.deepEqual(window.location.assignments, []);
});

test("authentication redirects during Back retain the selected history entry's position", async () => {
  const { router, requests, history, back, main } = harness();
  const albums = router.navigate("/library?filter=albums");
  await settle();
  requests.at(-1).resolve(response(view("library", "albums")));
  await albums;
  const users = router.navigate("/users");
  await settle();
  requests.at(-1).resolve(response(view("users")));
  await users;
  back();
  await settle();
  requests.at(-1).resolve(response({ redirect: "/login" }));
  await settle();
  requests.at(-1).resolve(response(view("login", "sign in")));
  await settle();
  assert.equal(history.state.phonolite.index, 1);
  assert.equal(router.currentUrl, "https://console.example/login");
  back();
  await settle();
  requests.at(-1).reject(new Error("offline"));
  await settle();
  assert.equal(history.moves.at(-1), 1);
  assert.equal(history.state.phonolite.index, 1);
  assert.equal(main.innerHTML, "sign in");
});

test("console links preserve native modified clicks, external links, downloads, and anchors", async () => {
  const { click, requests } = harness();
  for (const changes of [
    { ctrlKey: true }, { metaKey: true }, { shiftKey: true }, { altKey: true },
    { button: 1 }, { defaultPrevented: true },
  ]) {
    const event = click("/users", changes);
    assert.equal(event.defaultPrevented, changes.defaultPrevented);
  }
  assert.equal(click("https://other.example/users").defaultPrevented, undefined);
  assert.equal(click("#main").defaultPrevented, undefined);
  assert.equal(click("/logs/tail").defaultPrevented, undefined);
  assert.equal(click("/users", {}, { target: "_blank" }).defaultPrevented, undefined);
  assert.equal(click("/users", {}, { attributes: ["download"] }).defaultPrevented, undefined);
  assert.equal(click("/library?filter=albums", {}, { attributes: ["data-library-link"] }).defaultPrevented, undefined);
  assert.equal(requests.length, 0);
  const event = click("/users");
  assert.equal(event.defaultPrevented, true);
  await settle();
  assert.equal(requests.length, 1);
  requests[0].resolve(response(view("users")));
  await settle();
});

test("view rendering keeps authenticated navigation mounted and updates page semantics", () => {
  const { document, main } = harness();
  const navigation = new Element();
  navigation.innerHTML = "existing sidebar";
  const users = new Element();
  users.href = "https://console.example/users";
  const library = new Element();
  library.href = "https://console.example/";
  library.setAttribute("aria-current", "page");
  navigation.querySelector = () => navigation.innerHTML ? navigation : null;
  navigation.querySelectorAll = () => navigation.innerHTML ? [users, library] : [];
  const footer = new Element();
  footer.innerHTML = "old footer";
  document.getElementById = (id) => ({ main, "console-navigation": navigation, "console-footer": footer })[id] || null;
  renderConsoleView(view("users", "users content"));
  assert.equal(navigation.innerHTML, "existing sidebar", "cross-page navigation preserves the mounted sidebar and its animation state");
  assert.equal(users.getAttribute("aria-current"), "page");
  assert.equal(library.hasAttribute("aria-current"), false);
  assert.equal(main.innerHTML, "users content");
  assert.equal(footer.innerHTML, "footer");
  assert.equal(document.title, "users · Phonolite");
  assert.equal(document.body.dataset.page, "users");
  assert.equal(document.body.className, "users-page");
  renderConsoleView(view("library", "library content"));
  assert.equal(library.getAttribute("aria-current"), "page");
  assert.equal(users.hasAttribute("aria-current"), false);
  renderConsoleView({ ...view("login", "sign in"), nav: "", footer: "" });
  assert.equal(navigation.innerHTML, "");
  assert.equal(footer.innerHTML, "");
  assert.equal(main.innerHTML, "sign in");
});

test("disposing navigation aborts pending work and removes router event listeners", async () => {
  const subject = harness();
  const { router, requests, document, window, main } = subject;
  const navigation = router.navigate("/users");
  await settle();
  router.dispose();
  assert.equal(requests[0].options.signal.aborted, true);
  assert.equal(document.listeners.get("click").size, 0);
  assert.equal(window.listeners.get("popstate").size, 0);
  assert.equal(window.listeners.get("pagehide").size, 0);
  requests[0].resolve(response(view("users", "disposed result")));
  assert.equal(await navigation, false);
  assert.equal(main.innerHTML, "initial view");
  assert.equal(subject.disposals, 1);
  assert.equal(await router.navigate("/settings"), false);
});

test("busy controls share one accessible spinner and restore their prior state", () => {
  const { document } = harness();
  const button = document.createElement("button");
  button.textContent = "Save settings";
  button.disabled = false;
  button.setAttribute("aria-busy", "false");
  const first = beginBusy(button, "Saving settings");
  const second = beginBusy(button, "Saving settings");
  assert.equal(button.disabled, true);
  assert.equal(button.getAttribute("aria-busy"), "true");
  assert.equal(button.children.length, 1);
  assert.equal(button.children[0].getAttribute("role"), "status");
  assert.equal(button.children[0].getAttribute("aria-label"), "Saving settings");
  assert.equal(button.children[0].children[0].className, "spinner");
  assert.equal(button.textContent, "Save settings");
  first();
  first();
  assert.equal(button.disabled, true, "duplicate completion cannot finish another request");
  assert.equal(button.children[0].removed, undefined);
  second();
  assert.equal(button.disabled, false);
  assert.equal(button.getAttribute("aria-busy"), "false");
  assert.equal(button.children[0].removed, true);
  assert.equal(button.classList.contains("is-busy"), false);

  const disabled = document.createElement("button");
  disabled.disabled = true;
  beginBusy(disabled)();
  assert.equal(disabled.disabled, true);
  assert.equal(disabled.hasAttribute("aria-busy"), false);
});

test("shared action helpers refresh and navigate through the mounted router", async () => {
  const { window } = harness();
  const calls = [];
  configureNavigation({
    navigate: async (...args) => { calls.push(["navigate", ...args]); return true; },
    refresh: async (...args) => { calls.push(["refresh", ...args]); return true; },
  });
  assert.equal(await navigateConsole("/users", { replace: true }), true);
  assert.equal(await refreshAfterSuccess({ force: true }), true);
  assert.deepEqual(calls, [
    ["navigate", "/users", { replace: true }],
    ["refresh", { force: true }],
  ]);
  assert.equal(window.location.reloads, 0);
  configureNavigation(null);
  assert.equal(await refreshAfterSuccess(), false);
});

test("JSON reads use GET, mutations use POST, and cookie credentials remain intact", async () => {
  harness();
  const requests = [];
  globalThis.fetch = async (url, options) => {
    requests.push({ url, options });
    return response({ ok: true });
  };
  await fetchJson("/status/library");
  const params = new URLSearchParams({ name: "A & B" });
  await fetchJson("/users", params);
  assert.equal(requests[0].options.method, "GET");
  assert.equal(requests[0].options.body, undefined);
  assert.equal(requests[1].options.method, "POST");
  assert.equal(requests[1].options.body.toString(), "name=A+%26+B");
  assert.equal(requests[1].options.headers["Content-Type"], "application/x-www-form-urlencoded");
  assert.ok(requests.every(({ options }) => options.credentials === "same-origin"));
});

test("expired API sessions navigate to Login even when the 401 response URL is an API", async () => {
  const { window } = harness();
  const navigations = [];
  configureNavigation({ navigate: async (...args) => { navigations.push(args); return true; } });
  globalThis.fetch = async () => response({ error: "Session expired" }, {
    ok: false, status: 401, url: "https://console.example/status/library",
  });
  await assert.rejects(fetchJson("/status/library"), { name: "AbortError" });
  assert.deepEqual(navigations, [["/login", { replace: true, force: true }]]);
  assert.equal(window.location.reloads, 0);
});

test("HTML authentication redirects recover in place without displaying an HTML parsing error", async () => {
  harness();
  const navigations = [];
  configureNavigation({ navigate: async (...args) => { navigations.push(args); return true; } });
  globalThis.fetch = async () => ({
    ok: true, status: 200, redirected: true, url: "https://console.example/setup",
    json: async () => { throw new Error("HTML response"); },
  });
  await assert.rejects(fetchJson("/status/library"), { name: "AbortError" });
  assert.deepEqual(navigations, [["/setup", { replace: true, force: true }]]);
});
