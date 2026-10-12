import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { libraryUrl, setupLibrary, shouldHandleLink } from "../static/js/library.js";
import { configureNavigation, fetchJson, startPolling } from "../static/js/shared.js";
import { levelClass, parseLine, setupLogsPage } from "../static/js/logs.js";
import { closeModal, confirmAction, openModal, setupDialogs } from "../static/js/dialogs.js";
import { setupNavigation } from "../static/js/shell.js";
import { setupSettings } from "../static/js/settings.js";

// Small DOM doubles exercise the navigation/polling contracts without starting
// a browser, an application, or an HTTP server.
class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    const callbacks = this.listeners.get(name) || new Set();
    callbacks.add(callback);
    this.listeners.set(name, callbacks);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) {
    for (const callback of this.listeners.get(name) || []) callback(event);
  }
  dispatchEvent(event) { this.emit(event.type, event); return true; }
}

class Element extends Events {
  attrs = new Map();
  inert = false;
  children = [];
  innerHTML = "";
  dataset = {};
  classes = new Set();
  classList = {
    add: (...values) => values.forEach((value) => this.classes.add(value)),
    remove: (...values) => values.forEach((value) => this.classes.delete(value)),
    contains: (value) => this.classes.has(value),
    toggle: (value, force) => {
      const include = force ?? !this.classes.has(value);
      if (include) this.classes.add(value);
      else this.classes.delete(value);
      return include;
    },
  };
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name); }
  removeAttribute(name) { this.attrs.delete(name); }
  hasAttribute(name) { return this.attrs.has(name); }
  querySelectorAll(selector) {
    const result = [];
    for (const child of this.children) {
      const matches = selector.split(",").some((part) => {
        const match = part.trim();
        if (match.startsWith(".")) return (child.className || "").split(" ").includes(match.slice(1));
        if (match === "[autofocus]") return child.hasAttribute("autofocus");
        return child.tagName === match.toUpperCase();
      });
      if (matches) result.push(child);
      result.push(...child.querySelectorAll(selector));
    }
    return result;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  append(...children) {
    for (const child of children) {
      if (child.fragment) { this.append(...[...child.children]); child.children = []; continue; }
      child.remove(); child.parentElement = this; child.isConnected = true;
      this.children.push(child);
    }
  }
  prepend(...children) {
    for (const child of children) { child.parentElement = this; child.isConnected = true; }
    this.children.unshift(...children);
  }
  remove() {
    if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
    this.parentElement = null;
    this.isConnected = false;
  }
  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove();
    this.children = []; this.innerHTML = ""; this.append(...children);
  }
  focus() { this.focused = true; if (globalThis.document) globalThis.document.activeElement = this; }
  closest(selector) {
    for (let element = this; element; element = element.parentElement) {
      if (selector === "[inert]" && element.inert) return element;
      if (selector === ".modal" && (element.className || "").split(" ").includes("modal")) return element;
      if (selector.includes("[data-modal-close]") && element.dataset.modalClose !== undefined) return element;
    }
    return null;
  }
  contains(target) {
    for (let element = target; element; element = element.parentElement) if (element === this) return true;
    return false;
  }
  getClientRects() { return [{}]; }
}

const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch, FormData: globalThis.FormData };
afterEach(() => {
  configureNavigation(null);
  Object.assign(globalThis, originals);
});

const settle = () => new Promise((resolve) => setImmediate(resolve));
const response = (body, ok = true, status = 200) => ({ ok, status, json: async () => body });

function libraryHarness() {
  const document = new Events();
  const elements = new Map(["library-content", "library-pagination", "library-message", "library-search-block"].map((id) => [id, new Element()]));
  document.getElementById = (id) => elements.get(id);
  document.createElement = () => new Element();
  const window = new Events();
  window.location = { href: "https://console.example/library", reloads: 0, reload() { this.reloads++; } };
  window.history = { pushState(_state, _unused, href) { window.location.href = href; } };
  const navigations = [];
  const fetch = () => assert.fail("the library feature must let the console router fetch views");
  Object.assign(globalThis, { window, document, fetch });
  configureNavigation({ navigate(url) { navigations.push(url); return Promise.resolve(true); } });
  const lifecycle = setupLibrary();
  const dispose = () => lifecycle.dispose();
  function click(href, changes = {}) {
    const link = { href: new URL(href, window.location.href).href, hasAttribute: () => false };
    const event = { button: 0, target: { closest: () => link }, preventDefault() { this.defaultPrevented = true; }, ...changes };
    document.emit("click", event);
    return event;
  }
  return { elements, window, document, navigations, click, dispose };
}

test("library links stay within the library and preserve native modified-click behavior", () => {
  const base = "https://console.example/library";
  assert.equal(libraryUrl("/library?filter=albums", base).search, "?filter=albums");
  assert.equal(libraryUrl("/library/album/123", base).pathname, "/library/album/123");
  assert.equal(libraryUrl("https://other.example/library", base), null);
  assert.equal(libraryUrl("/library-admin", base), null);
  assert.equal(libraryUrl("/settings", base), null);
  const link = { hasAttribute: () => false };
  assert.equal(shouldHandleLink({ button: 0 }, link), true);
  for (const modifier of ["metaKey", "ctrlKey", "altKey", "shiftKey", "defaultPrevented"]) {
    assert.equal(shouldHandleLink({ button: 0, [modifier]: true }, link), false);
  }
  assert.equal(shouldHandleLink({ button: 1 }, link), false);
  assert.equal(shouldHandleLink({ button: 0 }, { ...link, target: "_blank" }), false);
});

test("library keeps the mounted view while delegating filtered navigation to the console router", () => {
  const { elements, window, navigations, click } = libraryHarness();
  const content = elements.get("library-content");
  content.innerHTML = "initial server content";
  assert.equal(navigations.length, 0);
  const albums = click("/library?filter=albums&page=2");
  const artists = click("/library?filter=artists");
  assert.equal(albums.defaultPrevented, true);
  assert.equal(artists.defaultPrevented, true);
  assert.deepEqual(navigations.map((url) => url.search), ["?filter=albums&page=2", "?filter=artists"]);
  assert.equal(content.innerHTML, "initial server content");
  assert.equal(window.location.href, "https://console.example/library", "the feature does not mutate history");
});

test("the library feature preserves native modified and external link behavior", () => {
  const { navigations, click } = libraryHarness();
  assert.equal(click("/library?filter=albums", { ctrlKey: true }).defaultPrevented, undefined);
  assert.equal(click("https://other.example/library").defaultPrevented, undefined);
  assert.equal(click("/settings").defaultPrevented, undefined);
  assert.equal(navigations.length, 0);
});

test("library remounts dispose delegated listeners and leave Back/Forward to the router", () => {
  const { window, document, navigations, click, dispose } = libraryHarness();
  dispose();
  for (const name of ["click", "submit", "change"]) assert.equal(document.listeners.get(name).size, 0);
  click("/library?filter=albums");
  assert.equal(navigations.length, 0);
  const libraryAgain = setupLibrary();
  click("/library?filter=artists");
  assert.equal(navigations.length, 1, "a remounted view handles a click once");
  window.location.href = "https://console.example/library?filter=artists";
  window.emit("popstate");
  assert.equal(navigations.length, 1);
  assert.equal(window.location.reloads, 0);
  libraryAgain.dispose();
});

test("JSON requests preserve form encoding and reject HTML session redirects", async () => {
  let sent;
  globalThis.fetch = async (url, options) => {
    sent = { url, options };
    return response({ ok: true });
  };
  const params = new URLSearchParams({ name: "A & B" });
  await fetchJson("/users", params);
  assert.equal(sent.options.method, "POST");
  assert.equal(sent.options.body.toString(), "name=A+%26+B");
  assert.equal(sent.options.credentials, "same-origin");
  assert.equal(sent.options.headers["X-Requested-With"], "fetch");
  globalThis.fetch = async () => ({ ok: true, json: async () => { throw new Error("HTML"); } });
  await assert.rejects(fetchJson("/status/library"), /sign in again/);
});

test("polling aborts when hidden, resumes when visible, and removes listeners on stop", async () => {
  const document = new Events();
  const window = new Events();
  const timers = new Map();
  let timerId = 0;
  window.setTimeout = (fn) => { timers.set(++timerId, fn); return timerId; };
  window.clearTimeout = (id) => timers.delete(id);
  Object.assign(globalThis, { document, window });
  const requests = [];
  const stop = startPolling((signal) => {
    const request = Promise.withResolvers();
    requests.push({ signal, ...request });
    return request.promise;
  }, 1000);
  assert.equal(requests.length, 1);
  document.hidden = true;
  document.emit("visibilitychange");
  assert.equal(requests[0].signal.aborted, true);
  requests[0].resolve();
  await settle();
  assert.equal(timers.size, 0);
  document.hidden = false;
  document.emit("visibilitychange");
  assert.equal(requests.length, 2);
  window.emit("pageshow");
  assert.equal(requests.length, 2, "a polling request cannot overlap");
  stop();
  assert.equal(requests[1].signal.aborted, true);
  requests[1].resolve();
  await settle();
  assert.equal(timers.size, 0);
  document.emit("visibilitychange");
  assert.equal(requests.length, 2);
});

test("logs preserve unstructured lines and normalize severity classes", () => {
  assert.deepEqual(parseLine("2026-09-29T12:00:00Z ERROR <script>bad</script>"), {
    time: "2026-09-29T12:00:00Z", level: "ERROR", message: "<script>bad</script>",
  });
  assert.deepEqual(parseLine("continuation without timestamp"), { time: "", level: "INFO", message: "continuation without timestamp" });
  assert.equal(levelClass("WARNING"), "warn");
  assert.equal(levelClass("CUSTOM"), "info");
});

function logsHarness() {
  const document = new Events();
  const window = new Events();
  const timers = new Map();
  let timerId = 0;
  window.setTimeout = (callback, delay) => {
    timers.set(++timerId, { callback, delay });
    return timerId;
  };
  window.clearTimeout = (id) => timers.delete(id);
  const nodes = Object.fromEntries([
    "log-view", "log-panel", "log-empty", "logs-refresh", "logs-clear",
    "log-spinner", "indexing-issues", "indexing-issue-count", "indexing-issues-card", "log-tabs",
  ].map((id) => [id, new Element()]));
  nodes["log-view"].scrollTop = 0;
  nodes["log-view"].scrollHeight = 100;
  nodes["log-view"].clientHeight = 100;
  nodes["log-panel"].prepend = (element) => nodes["log-panel"].children.unshift(element);
  const issueNotices = [];
  nodes["indexing-issues-card"].append(nodes["indexing-issues"]);
  nodes["indexing-issues"].before = (element) => {
    issueNotices.push(element);
    element.parentElement = nodes["indexing-issues-card"];
    nodes["indexing-issues-card"].children.unshift(element);
  };
  nodes["indexing-issues"].innerHTML = "Initial issues";
  nodes["indexing-issue-count"].textContent = "1 files";
  nodes["indexing-issues-card"].classList.add("hidden");
  const chips = new Map(["all", "info", "warnings", "errors", "issues", "activities", "debug"].map((view) => {
    const chip = new Element();
    chip.dataset.log = view;
    chip.closest = () => chip;
    if (view === "all") chip.classList.add("active");
    return [view, chip];
  }));
  nodes["log-tabs"].querySelector = () => [...chips.values()].find((chip) => chip.classList.contains("active"));
  nodes["log-tabs"].querySelectorAll = () => [...chips.values()];
  const logCard = new Element();
  logCard.dataset.logLimit = "1000";
  document.getElementById = (id) => nodes[id] || null;
  document.querySelector = (selector) => selector === ".log-card" ? logCard : null;
  document.createElement = () => new Element();
  document.createDocumentFragment = () => { const fragment = new Element(); fragment.fragment = true; return fragment; };
  const requests = [];
  Object.assign(globalThis, {
    document, window,
    fetch: (url, options) => {
      const pending = Promise.withResolvers();
      requests.push({ url, options, ...pending });
      return pending.promise;
    },
  });
  const dispose = setupLogsPage();
  return {
    document, window, nodes, logCard, requests, issueNotices, dispose, timers,
    select(view) { nodes["log-tabs"].emit("click", { target: chips.get(view) }); },
    messages() { return nodes["log-view"].querySelectorAll(".log-line").map((line) => line.children[2].textContent); },
    tick(delay) {
      const [id, timer] = [...timers].find(([, timer]) => timer.delay === delay) || [];
      assert.ok(timer, `expected polling timer at ${delay}ms`);
      timers.delete(id);
      timer.callback();
    },
  };
}

test("Indexing issues has its own Logs tab and only the selected view polls", async () => {
  const { window, nodes, logCard, requests, select, tick } = logsHarness();
  assert.equal(nodes["indexing-issues-card"].classList.contains("hidden"), true);
  assert.equal(logCard.classList.contains("hidden"), false);
  assert.ok(requests[0].url.endsWith("view=all"));
  requests[0].resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  tick(1000);
  assert.ok(requests.at(-1).url.endsWith("view=all"));
  requests.at(-1).resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  for (const view of ["info", "warnings", "errors", "activities", "debug", "all"]) {
    select(view);
    assert.ok(requests.at(-1).url.endsWith(`view=${view}`));
    requests.at(-1).resolve(response({ lines: [], updated_at: 1 }));
    await settle();
  }
  assert.ok(requests.every(({ url }) => url.startsWith("/logs/tail?")));
  select("issues");
  assert.equal(logCard.classList.contains("hidden"), true);
  assert.equal(nodes["indexing-issues-card"].classList.contains("hidden"), false);
  assert.equal(requests.at(-1).url, "/logs/issues");
  requests.at(-1).resolve(response({ html: "Issue details", total: 2 }));
  await settle();
  tick(5000);
  const oldIssues = requests.at(-1);
  assert.equal(oldIssues.url, "/logs/issues");
  select("errors");
  assert.equal(oldIssues.options.signal.aborted, true);
  assert.ok(requests.at(-1).url.endsWith("view=errors"));
  assert.equal(logCard.classList.contains("hidden"), false);
  assert.equal(nodes["indexing-issues-card"].classList.contains("hidden"), true);
  oldIssues.resolve(response({ html: "Stale issue details", total: 9 }));
  requests.at(-1).resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "Issue details");
  window.emit("pagehide");
});

test("the Indexing issues tab refreshes only issues and retains rows when refresh fails", async () => {
  const { window, nodes, logCard, requests, issueNotices, select } = logsHarness();
  requests[0].resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  select("issues");
  const oldIssues = requests.at(-1);
  const beforeRefresh = requests.length;
  nodes["logs-refresh"].emit("click");
  assert.equal(requests.length, beforeRefresh + 1);
  assert.equal(oldIssues.options.signal.aborted, true);
  const latestIssues = requests.at(-1);
  assert.equal(latestIssues.url, "/logs/issues");
  latestIssues.resolve(response({ html: "New issues", total: 2 }));
  await settle();
  oldIssues.resolve(response({ html: "Old issues", total: 1 }));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "New issues");
  assert.equal(nodes["indexing-issue-count"].textContent, "2 files");
  nodes["logs-refresh"].emit("click");
  requests.at(-1).resolve(response({ error: "Cannot read issues" }, false, 500));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "New issues");
  assert.equal(nodes["indexing-issue-count"].textContent, "2 files");
  assert.ok(issueNotices[0].textContent.includes("previous results are still shown"));
  assert.equal(issueNotices[0].classList.contains("hidden"), false);
  assert.equal(issueNotices[0].parentElement, nodes["indexing-issues-card"]);
  select("all");
  assert.equal(nodes["indexing-issues-card"].classList.contains("hidden"), true, "the issue error notice hides with its panel");
  assert.equal(logCard.classList.contains("hidden"), false);
  window.emit("pagehide");
});

test("tab switches ignore old log responses and repaint logs even with the same timestamp", async () => {
  const { window, requests, select, messages } = logsHarness();
  const staleLogs = requests[0];
  select("issues");
  assert.equal(staleLogs.options.signal.aborted, true);
  requests.at(-1).resolve(response({ html: "Issue details", total: 1 }));
  await settle();
  select("all");
  requests.at(-1).resolve(response({ lines: ["now INFO Current log view"], updated_at: 1 }));
  await settle();
  assert.deepEqual(messages(), ["Current log view"]);
  staleLogs.resolve(response({ lines: ["now INFO Stale log view"], updated_at: 1 }));
  await settle();
  assert.deepEqual(messages(), ["Current log view"]);
  select("issues");
  requests.at(-1).resolve(response({ html: "Issue details", total: 1 }));
  await settle();
  select("all");
  requests.at(-1).resolve(response({ lines: ["now INFO Reentered log view"], updated_at: 1 }));
  await settle();
  assert.deepEqual(messages(), ["Reentered log view"]);
  window.emit("pagehide");
});

test("clearing the Indexing issues tab refreshes issues and page visibility pauses that view", async () => {
  const { document, window, nodes, requests, select } = logsHarness();
  requests[0].resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  select("issues");
  const oldIssues = requests.at(-1);
  nodes["logs-clear"].emit("click");
  assert.equal(oldIssues.options.signal.aborted, true);
  const clearRequest = requests.at(-1);
  assert.equal(clearRequest.url, "/logs/clear");
  assert.equal(clearRequest.options.method, "POST");
  oldIssues.resolve(response({ html: "Stale issues", total: 99 }));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "Initial issues");
  const beforeClear = requests.length;
  clearRequest.resolve(response({ ok: true }));
  await settle();
  assert.equal(requests.length, beforeClear + 1);
  const issueRequest = requests.at(-1);
  assert.equal(issueRequest.url, "/logs/issues");
  assert.equal(nodes["logs-clear"].disabled, false);
  document.hidden = true;
  document.emit("visibilitychange");
  assert.equal(issueRequest.options.signal.aborted, true);
  issueRequest.resolve(response({ html: "Hidden result", total: 4 }));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "Initial issues");
  document.hidden = false;
  document.emit("visibilitychange");
  const resumedIssues = requests.at(-1);
  assert.equal(resumedIssues.url, "/logs/issues");
  resumedIssues.resolve(response({ html: "No indexing issues detected.", total: 0 }));
  await settle();
  assert.equal(nodes["indexing-issues"].innerHTML, "No indexing issues detected.");
  assert.equal(nodes["indexing-issue-count"].textContent, "0 files");
  nodes["logs-refresh"].emit("click");
  const beforePagehide = requests.at(-1);
  window.emit("pagehide");
  assert.equal(beforePagehide.options.signal.aborted, true);
  beforePagehide.resolve(response({ html: "Stale page result", total: 8 }));
  await settle();
  window.emit("pageshow");
  assert.equal(requests.at(-1).url, "/logs/issues");
  assert.notEqual(requests.at(-1), beforePagehide);
  assert.equal(nodes["indexing-issue-count"].textContent, "0 files");
  window.emit("pagehide");
});

test("refresh and clear on a log tab never fetch issue details and defer hidden refreshes", async () => {
  const { document, window, nodes, requests } = logsHarness();
  requests[0].resolve(response({ lines: [], updated_at: 1 }));
  await settle();
  nodes["logs-refresh"].emit("click");
  const refreshedLog = requests.at(-1);
  assert.ok(refreshedLog.url.endsWith("view=all"));
  nodes["logs-clear"].emit("click");
  assert.equal(refreshedLog.options.signal.aborted, true);
  const clearRequest = requests.at(-1);
  assert.equal(clearRequest.url, "/logs/clear");
  document.hidden = true;
  document.emit("visibilitychange");
  refreshedLog.resolve(response({ lines: ["now INFO Stale"], updated_at: 2 }));
  clearRequest.resolve(response({ ok: true }));
  await settle();
  assert.equal(requests.at(-1), clearRequest, "clearing while hidden must not start a refresh");
  document.hidden = false;
  document.emit("visibilitychange");
  assert.ok(requests.at(-1).url.endsWith("view=all"));
  assert.equal(requests.some(({ url }) => url === "/logs/issues"), false);
  assert.equal(nodes["indexing-issues"].innerHTML, "Initial issues");
  window.emit("pagehide");
});

test("a delayed poll and pageshow cannot create duplicate timer chains", async () => {
  const document = new Events();
  const window = new Events();
  const timers = new Map();
  let timerId = 0;
  let calls = 0;
  window.setTimeout = (fn) => { timers.set(++timerId, fn); return timerId; };
  window.clearTimeout = (id) => timers.delete(id);
  Object.assign(globalThis, { document, window });
  const stop = startPolling(async () => { calls++; }, 5000, { immediate: false });
  assert.equal(timers.size, 1);
  window.emit("pageshow");
  await settle();
  assert.equal(calls, 1);
  assert.equal(timers.size, 1);
  stop();
});

test("leaving Logs disposes polling, listeners, and in-flight actions without painting stale results", async () => {
  const { document, window, nodes, requests, select, timers, dispose } = logsHarness();
  const logRequest = requests[0];
  select("issues");
  const issuesRequest = requests.at(-1);
  nodes["logs-clear"].emit("click");
  const clearRequest = requests.at(-1);
  dispose();
  assert.equal(logRequest.options.signal.aborted, true);
  assert.equal(issuesRequest.options.signal.aborted, true);
  assert.equal(clearRequest.options.signal.aborted, true);
  logRequest.resolve(response({ lines: ["now INFO Stale logs"], updated_at: 1 }));
  issuesRequest.resolve(response({ html: "Stale issues", total: 99 }));
  clearRequest.resolve(response({ ok: true }));
  await settle();
  const count = requests.length;
  window.emit("pageshow");
  document.emit("visibilitychange");
  nodes["logs-refresh"].emit("click");
  assert.equal(requests.length, count);
  assert.equal(timers.size, 0);
  assert.equal(nodes["indexing-issues"].innerHTML, "Initial issues");
  assert.equal(nodes["indexing-issue-count"].textContent, "1 files");
  for (const name of ["pageshow", "pagehide"]) assert.equal(window.listeners.get(name).size, 0);
  assert.equal(document.listeners.get("visibilitychange").size, 0);
});

test("navigation drawers and dialogs trap focus, restore nested focus, and handle dismissal and resizing", async () => {
  const document = new Events();
  const body = new Element();
  const content = new Element();
  const originallyInert = new Element();
  originallyInert.inert = true;
  const trigger = new Element();
  trigger.parentElement = content;
  trigger.isConnected = true;
  content.children = [trigger];
  function modal(id) {
    const element = new Element();
    element.id = id;
    const card = new Element();
    const title = new Element();
    const first = new Element();
    const last = new Element();
    for (const control of [first, last]) {
      control.tabIndex = 0;
      control.parentElement = card;
      control.isConnected = true;
    }
    element.parentElement = body;
    card.parentElement = element;
    element.children = [card];
    card.children = [first, last];
    element.querySelector = (selector) => selector === ".modal-card" ? card : null;
    element.querySelectorAll = () => [first, last];
    card.querySelector = () => title;
    return { element, card, first, last };
  }
  const firstDialog = modal("first");
  const secondDialog = modal("second");
  const serverDialog = modal("nav-modal");
  const drawer = modal("navigation-drawer");
  const nav = new Element();
  nav.inert = false;
  const navigationToggle = new Element();
  const mobileBrand = new Element();
  const backdrop = new Element();
  const links = new Element();
  links.querySelector = () => drawer.last;
  drawer.element.append(backdrop);
  nav.append(navigationToggle, mobileBrand, drawer.element);
  navigationToggle.closest = (selector) => selector === ".nav" ? nav : Element.prototype.closest.call(navigationToggle, selector);
  document.documentElement = new Element();
  document.getElementById = (id) => ({
    "navigation-toggle": navigationToggle, "primary-nav": links,
    "navigation-drawer": drawer.element, "navigation-backdrop": backdrop,
    "navigation-close": drawer.first, "nav-modal": serverDialog.element,
  })[id];
  const desktop = new Events();
  desktop.matches = false;
  const window = new Events();
  window.matchMedia = () => desktop;
  body.children = [nav, content, originallyInert, firstDialog.element, secondDialog.element, serverDialog.element];
  nav.parentElement = body;
  content.parentElement = body;
  originallyInert.parentElement = body;
  document.body = body;
  document.createElement = (tag) => {
    const element = new Element();
    element.tagName = tag.toUpperCase();
    element.tabIndex = tag === "button" ? 0 : -1;
    return element;
  };
  document.querySelectorAll = () => [firstDialog.element, secondDialog.element, serverDialog.element];
  document.activeElement = trigger;
  Object.assign(globalThis, { document, window });
  setupDialogs();
  setupNavigation();
  assert.equal(drawer.element.inert, true);
  assert.equal(drawer.element.getAttribute("aria-hidden"), "true");
  navigationToggle.focus();
  navigationToggle.emit("click");
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "true");
  assert.equal(navigationToggle.getAttribute("aria-label"), "Close navigation");
  assert.equal(drawer.element.inert, false);
  assert.equal(drawer.element.classList.contains("open"), true);
  assert.equal(document.activeElement, drawer.first);
  assert.equal(content.inert, true);
  assert.equal(document.documentElement.classList.contains("navigation-open"), true);
  drawer.last.focus();
  const drawerTab = { key: "Tab", preventDefault() { this.defaultPrevented = true; } };
  document.emit("keydown", drawerTab);
  assert.equal(drawerTab.defaultPrevented, true);
  assert.equal(document.activeElement, drawer.first);
  drawer.element.emit("click", { defaultPrevented: true, target: { closest: () => drawer.last } });
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "true");
  drawer.last.focus();
  openModal(serverDialog.element);
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "true", "Escape closes the nested dialog first");
  assert.equal(drawer.element.classList.contains("open"), true);
  assert.equal(document.activeElement, drawer.last);
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "false");
  assert.equal(drawer.element.inert, true);
  assert.equal(document.activeElement, navigationToggle);
  assert.equal(content.inert, false);
  assert.equal(originallyInert.inert, true);
  assert.equal(document.documentElement.classList.contains("navigation-open"), false);
  navigationToggle.emit("click");
  backdrop.emit("click");
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "false");
  assert.equal(document.activeElement, navigationToggle);
  navigationToggle.emit("click");
  drawer.first.emit("click");
  assert.equal(drawer.element.classList.contains("open"), false);
  navigationToggle.emit("click");
  drawer.element.emit("click", { defaultPrevented: false, target: { closest: () => drawer.last } });
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "false");
  navigationToggle.emit("click");
  desktop.matches = true;
  desktop.emit("change");
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "false");
  assert.equal(drawer.element.inert, false);
  assert.equal(drawer.element.hasAttribute("aria-hidden"), false);
  assert.equal(drawer.element.hasAttribute("role"), false);
  assert.equal(content.inert, false);
  assert.equal(document.activeElement, drawer.last);
  navigationToggle.emit("click");
  assert.equal(drawer.element.classList.contains("open"), false, "desktop navigation stays inline");
  desktop.matches = false;
  desktop.emit("change");
  assert.equal(drawer.element.inert, true);
  assert.equal(document.activeElement, navigationToggle);
  navigationToggle.emit("click");
  drawer.last.focus();
  openModal(serverDialog.element);
  desktop.matches = true;
  desktop.emit("change");
  assert.equal(navigationToggle.getAttribute("aria-expanded"), "false");
  assert.equal(serverDialog.element.classList.contains("open"), true);
  assert.equal(content.inert, true, "resizing keeps the server dialog modal");
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(content.inert, false, "closing the resized dialog restores the page");
  assert.equal(nav.inert, false);
  assert.equal(drawer.element.inert, false);
  desktop.matches = false;
  desktop.emit("change");
  trigger.focus();
  openModal(firstDialog.element);
  assert.equal(document.activeElement, firstDialog.first);
  assert.equal(firstDialog.element.getAttribute("role"), "dialog");
  assert.equal(firstDialog.card.getAttribute("role"), undefined, "dialog semantics must not be nested");
  assert.equal(content.inert, true);
  firstDialog.last.focus();
  const tab = { key: "Tab", preventDefault() { this.defaultPrevented = true; } };
  document.emit("keydown", tab);
  assert.equal(tab.defaultPrevented, true);
  assert.equal(document.activeElement, firstDialog.first);
  document.emit("keydown", { key: "Tab", shiftKey: true, preventDefault() {} });
  assert.equal(document.activeElement, firstDialog.last);
  openModal(secondDialog.element);
  assert.equal(firstDialog.element.classList.contains("open"), false);
  assert.equal(document.activeElement, secondDialog.first);
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(firstDialog.element.classList.contains("open"), true);
  assert.equal(document.activeElement, firstDialog.last);
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(document.activeElement, trigger);
  assert.equal(Boolean(content.inert), false);
  assert.equal(originallyInert.inert, true);

  const cancelPromise = confirmAction({ title: "Delete source?", message: "<b>Keep this literal</b>", danger: true });
  const cancelDialog = body.children.at(-1);
  const cancelCard = cancelDialog.children[1];
  const cancelButton = cancelCard.children[2].children[0];
  assert.equal(document.activeElement, cancelButton, "the safe action receives initial focus");
  assert.equal(cancelCard.children[1].textContent, "<b>Keep this literal</b>");
  document.emit("click", { target: cancelButton });
  assert.equal(await cancelPromise, false);
  assert.equal(body.children.includes(cancelDialog), false);
  assert.equal(document.activeElement, trigger);

  const escapePromise = confirmAction({ title: "Restart?", message: "Restart this server?" });
  document.emit("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(await escapePromise, false);

  openModal(firstDialog.element);
  const confirmPromise = confirmAction({ title: "Restart?", message: "Restart this server?", confirmLabel: "Restart" });
  const confirmDialog = body.children.at(-1);
  const confirmButton = confirmDialog.children[1].children[2].children[1];
  assert.equal(confirmButton.textContent, "Restart");
  confirmButton.emit("click");
  assert.equal(await confirmPromise, true);
  assert.equal(body.children.includes(confirmDialog), false);
  assert.equal(firstDialog.element.classList.contains("open"), true);
  document.emit("keydown", { key: "Escape", preventDefault() {} });
});

function settingsHarness() {
  const document = new Events();
  document.body = new Element();
  const content = new Element();
  const form = new Element();
  form.setAttribute("action", "/settings");
  const port = new Element();
  Object.assign(port, { name: "port", type: "number", value: "8000", disabled: false });
  form.elements = [port];
  form.append(port);
  const save = new Element();
  save.tagName = "BUTTON";
  save.id = "save-settings";
  const status = new Element();
  const formError = new Element();
  const saveBar = new Element();
  saveBar.append(save, status);
  const unsaved = new Element();
  unsaved.className = "modal";
  unsaved.id = "modal-unsaved";
  const card = new Element();
  card.className = "modal-card";
  const discard = new Element();
  const stay = new Element();
  for (const button of [discard, stay]) { button.tagName = "BUTTON"; button.tabIndex = 0; }
  card.append(discard, stay);
  unsaved.append(card);
  content.append(form, saveBar, formError);
  document.body.append(content, unsaved);
  const nodes = { settings: form, "settings-save-bar": saveBar, "save-settings": save, "settings-save-status": status, "settings-error": formError, "modal-unsaved": unsaved, "unsaved-discard": discard, "unsaved-stay": stay };
  document.getElementById = (id) => nodes[id] || null;
  document.querySelector = (selector) => selector === ".content" ? content : null;
  document.querySelectorAll = () => [unsaved];
  document.createElement = () => new Element();
  const window = new Events();
  window.location = { href: "https://console.example/settings" };
  window.setTimeout = () => 1;
  window.clearTimeout = () => {};
  const requests = [];
  let refreshes = 0;
  Object.assign(globalThis, {
    document, window,
    fetch(url, options) { const deferred = Promise.withResolvers(); requests.push({ url, options, ...deferred }); return deferred.promise; },
    FormData: class {
      constructor(target) { this.values = target.elements.filter((element) => element.name && !element.disabled).map((element) => [element.name, element.value]); }
      *[Symbol.iterator]() { yield* this.values; }
    },
  });
  configureNavigation({ refresh() { refreshes++; return Promise.resolve(true); } });
  const lifecycle = setupSettings();
  return {
    document, window, content, form, formError, port, save, saveBar, status, unsaved, discard, stay, requests, lifecycle,
    refreshes: () => refreshes,
    edit(value) { port.value = value; form.emit("input"); },
    submit() { form.emit("submit", { submitter: save, preventDefault() {} }); },
  };
}

test("Settings uses one asynchronous dirty confirmation, restores discarded values, and removes guards on dispose", async () => {
  const { document, window, port, save, saveBar, status, unsaved, discard, stay, lifecycle, edit } = settingsHarness();
  assert.equal(document.body.dataset.settingsDirty, "false");
  assert.equal(status.textContent, "No unsaved changes");
  assert.equal(save.disabled, true);
  assert.equal(saveBar.hidden, true, "the initial save bar occupies no space");
  assert.equal(await lifecycle.canLeave(), true);
  edit("9000");
  assert.equal(saveBar.hidden, false, "editing shows the save bar");
  edit("8000");
  assert.equal(saveBar.hidden, true, "reverting every edit hides the bar");
  assert.equal(status.textContent, "No unsaved changes");
  assert.equal(save.disabled, true, "restoring the original values needs no save");
  edit("9000");
  assert.equal(document.body.dataset.settingsDirty, "true");
  assert.equal(save.disabled, false);
  const leave = lifecycle.canLeave();
  assert.equal(lifecycle.canLeave(), leave, "repeat navigation shares the pending confirmation");
  const unload = { preventDefault() { this.prevented = true; } };
  window.emit("beforeunload", unload);
  assert.equal(unload.prevented, true);
  stay.emit("click");
  assert.equal(await leave, false);
  assert.equal(port.value, "9000");
  const discardLeave = lifecycle.canLeave();
  discard.emit("click");
  assert.equal(await discardLeave, true);
  assert.equal(port.value, "8000");
  assert.equal(document.body.dataset.settingsDirty, "false");
  assert.equal(saveBar.hidden, true, "discarding hides the bar");
  assert.equal(status.textContent, "No unsaved changes");
  assert.equal(save.disabled, true);
  edit("9500");
  const pendingLeave = lifecycle.canLeave();
  lifecycle.dispose();
  assert.equal(await pendingLeave, false);
  assert.equal(document.body.dataset.settingsDirty, undefined);
  assert.equal(document.body.dataset.settingsSaveVisible, undefined);
  assert.equal(window.listeners.get("beforeunload").size, 0);
  assert.equal(document.listeners.get("submit").size, 0);
  closeModal(unsaved);
});

test("Settings POST suppresses duplicate saves and keeps edits made while the request is in flight", async () => {
  const { document, content, requests, lifecycle, edit, submit, refreshes, status, save, saveBar } = settingsHarness();
  const initialNodes = [...content.children];
  submit();
  assert.equal(requests.length, 0, "an unchanged form never sends a save request");
  assert.equal(status.textContent, "No unsaved changes");
  edit("9000");
  submit(); submit();
  assert.equal(saveBar.hidden, false, "the bar stays visible during saving");
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "/settings");
  assert.equal(requests[0].options.method, "POST");
  assert.equal(requests[0].options.body.get("port"), "9000");
  edit("9500");
  requests[0].resolve(response({ ok: true, message: "Settings saved." }));
  await settle();
  assert.equal(refreshes(), 0);
  assert.equal(document.body.dataset.settingsDirty, "true");
  assert.equal(saveBar.hidden, false, "edits made during saving still need the bar");
  assert.equal(status.textContent, "Unsaved changes");
  assert.deepEqual(content.children, initialNodes);
  submit();
  requests[1].resolve(response({ ok: true }));
  await settle();
  assert.equal(refreshes(), 0, "saving retains the mounted settings form");
  assert.equal(document.body.dataset.settingsDirty, "false");
  assert.equal(saveBar.hidden, true, "saving all changes hides the bar");
  assert.equal(status.textContent, "Settings saved");
  assert.equal(save.disabled, true);
  assert.deepEqual(content.children, initialNodes);
  submit();
  assert.equal(requests.length, 2, "a completed save cannot be resubmitted without changes");
  edit("10000");
  edit("9500");
  assert.equal(status.textContent, "No unsaved changes", "undoing a later edit is not a new save");
  assert.equal(save.disabled, true);
  lifecycle.dispose();
});

test("failed Settings saves retain edits, report errors inline, and allow a retry", async () => {
  const { document, formError, port, save, saveBar, requests, lifecycle, edit, submit } = settingsHarness();
  edit("9000"); submit();
  requests[0].resolve(response({ error: "Port is already in use" }, false, 400));
  await settle();
  assert.equal(port.value, "9000");
  assert.equal(document.body.dataset.settingsDirty, "true");
  assert.equal(save.disabled, false);
  assert.equal(saveBar.hidden, false, "a failed save keeps the unsaved bar available");
  assert.equal(formError.textContent, "Port is already in use");
  assert.equal(formError.classList.contains("visible"), true);
  submit();
  assert.equal(formError.classList.contains("visible"), false);
  requests[1].resolve(response({ ok: true }));
  await settle();
  assert.equal(document.body.dataset.settingsDirty, "false");
  assert.equal(formError.textContent, "");
  lifecycle.dispose();
});

test("disposed Settings requests abort and cannot refresh another mounted view", async () => {
  const { requests, lifecycle, edit, submit, refreshes } = settingsHarness();
  edit("9000"); submit();
  lifecycle.dispose();
  assert.equal(requests[0].options.signal.aborted, true);
  requests[0].resolve(response({ ok: true }));
  await settle();
  assert.equal(refreshes(), 0);
});
