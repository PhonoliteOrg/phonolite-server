import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { libraryIndexStatusText, setupShell } from "../static/js/shell.js";
import { configureNavigation } from "../static/js/shared.js";
import { closeAllModals } from "../static/js/dialogs.js";

// Test the long-lived console shell without starting an app or HTTP listener.
class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) { return [...this.listeners.get(name) || []].map((callback) => callback(event)); }
  dispatchEvent(event) { this.emit(event.type, event); return true; }
}

class Element extends Events {
  attrs = new Map();
  children = [];
  dataset = {};
  className = "";
  inert = false;
  disabled = false;
  tabIndex = -1;
  textContent = "";
  constructor(tag = "div") { super(); this.tagName = tag.toUpperCase(); }
  classList = {
    contains: (name) => this.className.split(" ").includes(name),
    add: (...names) => { this.className = [...new Set([...this.className.split(" ").filter(Boolean), ...names])].join(" "); },
    remove: (...names) => { this.className = this.className.split(" ").filter((name) => !names.includes(name)).join(" "); },
    toggle: (name, force) => {
      const enabled = force ?? !this.classList.contains(name);
      if (enabled) this.classList.add(name); else this.classList.remove(name);
      return enabled;
    },
  };
  get isConnected() {
    for (let element = this; element; element = element.parentElement) if (element === globalThis.document?.body) return true;
    return false;
  }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  hasAttribute(name) { return this.attrs.has(name); }
  removeAttribute(name) { this.attrs.delete(name); }
  append(...children) { for (const child of children) { child.remove(); child.parentElement = this; this.children.push(child); } }
  prepend(...children) { for (const child of [...children].reverse()) { child.remove(); child.parentElement = this; this.children.unshift(child); } }
  remove() {
    if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
    this.parentElement = null;
  }
  matches(selector) {
    return selector.split(",").some((part) => {
      const query = part.trim();
      if (query === '[data-library-scanning="true"]') return this.dataset.libraryScanning === "true";
      if (query === 'form[action="/logout"]') return this.tagName === "FORM" && this.getAttribute("action") === "/logout";
      if (query === 'button[type="submit"]') return this.tagName === "BUTTON" && this.getAttribute("type") === "submit";
      if (query.startsWith(".")) return query.slice(1).split(".").every((name) => this.classList.contains(name));
      return query.toUpperCase() === this.tagName;
    });
  }
  querySelectorAll(selector) {
    return this.children.flatMap((child) => [...(child.matches(selector) ? [child] : []), ...child.querySelectorAll(selector)]);
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest(selector) {
    for (let element = this; element; element = element.parentElement) if (element.matches(selector)) return element;
    return null;
  }
  contains(target) { for (let element = target; element; element = element.parentElement) if (element === this) return true; return false; }
  focus() { document.activeElement = this; }
}

const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch, FormData: globalThis.FormData };
const cleanups = [];
const settle = () => new Promise((resolve) => setImmediate(resolve));
afterEach(() => {
  closeAllModals();
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});

function harness({ page = "settings", scanning = false } = {}) {
  const document = new Events();
  document.documentElement = new Element("html");
  document.body = new Element("body");
  document.body.dataset.page = page;
  document.hidden = false;
  const elements = new Map();
  const main = new Element("main");
  main.className = "content";
  elements.set("main", main);
  document.body.append(main);
  document.activeElement = main;
  document.createElement = (tag) => new Element(tag);
  document.getElementById = (id) => elements.get(id) || null;
  document.querySelectorAll = (selector) => document.body.querySelectorAll(selector);
  document.querySelector = (selector) => document.body.querySelector(selector);
  const timers = new Map();
  const media = [];
  let timerId = 0;
  const window = new Events();
  window.location = { href: `https://console.example/${page}` };
  window.setTimeout = (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; };
  window.clearTimeout = (id) => timers.delete(id);
  window.matchMedia = () => { const query = new Events(); query.matches = true; media.push(query); return query; };
  Object.assign(globalThis, { document, window });

  function addNavigation() {
    const nav = new Element("nav");
    nav.className = "nav";
    document.body.append(nav);
    const nodes = {};
    for (const id of ["primary-nav", "navigation-toggle", "navigation-drawer", "navigation-backdrop", "navigation-close", "nav-menu-toggle", "nav-modal", "library-index-status"]) {
      const node = new Element(id.endsWith("toggle") || id.endsWith("close") ? "button" : "div");
      elements.set(id, node);
      nodes[id] = node;
      nav.append(node);
    }
    return { nav, nodes };
  }
  const navigation = addNavigation();
  let refreshes = 0;
  let navigations = 0;
  configureNavigation({ refresh: async () => { refreshes++; return true; }, navigate: async () => { navigations++; return true; } });
  let serverStatus = { status: scanning ? "scanning" : "ready", albums: 3, progress: { phase: "discovering" } };
  globalThis.fetch = async () => ({ ok: true, json: async () => serverStatus });
  return {
    document, window, elements, main, timers, media, navigation, addNavigation,
    get refreshes() { return refreshes; },
    get navigations() { return navigations; },
    removeNavigation() {
      navigation.nav.remove();
      Object.keys(navigation.nodes).forEach((id) => elements.delete(id));
    },
    async poll(data = { status: "ready", albums: 3 }) {
      await settle();
      serverStatus = data;
      const [id, timer] = [...timers].find(([, value]) => value.delay === 2000);
      timers.delete(id);
      await timer.callback();
    },
  };
}

test("synchronizing authenticated views retains the drawer and one library status watcher", async () => {
  const state = harness({ scanning: true });
  const shell = setupShell();
  cleanups.push(shell);
  await settle();
  const toggle = state.navigation.nodes["navigation-toggle"];
  toggle.setAttribute("aria-expanded", "true");
  const initialTimers = [...state.timers.keys()];
  for (let index = 0; index < 100; index++) shell.sync();
  assert.equal(toggle.getAttribute("aria-expanded"), "true", "sync must not reset persistent navigation state");
  assert.equal(state.media.length, 1);
  assert.equal(toggle.listeners.get("click").size, 1);
  assert.equal(state.document.listeners.get("library:scan-started").size, 1);
  assert.equal(state.document.listeners.get("visibilitychange").size, 1);
  assert.deepEqual([...state.timers.keys()], initialTimers);
  shell();
  assert.equal(toggle.listeners.get("click").size, 0);
  assert.equal(state.document.listeners.get("library:scan-started").size, 0);
  assert.equal(state.document.listeners.get("visibilitychange").size, 0);
  assert.equal(state.media[0].listeners.get("change").size, 0);
});

test("finishing a scan clears sidebar progress without refreshing settings or replacing the form", async () => {
  const state = harness({ scanning: true });
  const form = new Element("form");
  state.main.append(form);
  const shell = setupShell();
  cleanups.push(shell);
  await state.poll();
  assert.equal(state.refreshes, 0);
  assert.equal(form.parentElement, state.main);
  assert.deepEqual(state.main.children, [form], "scan completion leaves only the existing settings form");
  assert.equal(state.timers.size, 1, "continue checking for watcher-triggered indexing");
  assert.equal(state.document.listeners.get("visibilitychange").size, 1);
  assert.equal(state.elements.get("library-index-status").textContent, "");
  assert.equal(state.elements.get("library-index-status").hidden, true);
});

test("finishing a scan refreshes only the current library contents", async () => {
  const state = harness({ page: "library", scanning: true });
  const shell = setupShell();
  cleanups.push(shell);
  await state.poll();
  assert.equal(state.refreshes, 1);
  assert.deepEqual(state.main.children, [], "scan completion adds no page banners");
  shell.sync();
  assert.equal(state.media.length, 1);
  assert.equal(state.document.listeners.get("visibilitychange").size, 1);
});

test("a scan finishing before the first poll refreshes Library once without a banner", async () => {
  const state = harness({ page: "library" });
  const workspace = new Element();
  workspace.dataset.libraryScanning = "true";
  state.main.append(workspace);
  const shell = setupShell();
  cleanups.push(shell);
  await settle();
  assert.equal(state.refreshes, 1);
  await state.poll();
  assert.equal(state.refreshes, 1, "the initial scan marker cannot trigger repeated refreshes");
  assert.deepEqual(state.main.children, [workspace]);
});

test("a failed scan updates only the sidebar without creating a page banner", async () => {
  const state = harness({ scanning: true });
  globalThis.fetch = async () => ({ ok: true, json: async () => ({ status: "error", message: "The folder is unavailable." }) });
  const shell = setupShell();
  cleanups.push(shell);
  await state.poll();
  assert.equal(state.refreshes, 0);
  assert.deepEqual(state.main.children, [], "scan failures add no page banners");
  assert.equal(state.elements.get("library-index-status").textContent, "Indexing failed");
});

test("sidebar progress detects scans on other views and changes no page content", async () => {
  const state = harness({ page: "users" });
  const shell = setupShell();
  cleanups.push(shell);
  await settle();
  const status = state.elements.get("library-index-status");
  assert.equal(status.textContent, "");
  assert.equal(status.hidden, true);
  const pageNodes = [...state.main.children];
  await state.poll({ status: "scanning", progress: { phase: "reading_tracks", completed: 10, total: 30 } });
  assert.equal(status.textContent, "Reading 10/30 tracks");
  assert.equal(status.hidden, false);
  await state.poll({ status: "scanning", progress: { phase: "indexing_albums", completed: 2, total: 6 } });
  assert.equal(status.textContent, "Indexing 2/6 albums");
  assert.equal(status.dataset.status, "scanning");
  await state.poll({ status: "scanning", progress: { phase: "committing" } });
  assert.equal(status.textContent, "Saving library…");
  await state.poll({ status: "ready", albums: 6 });
  assert.equal(status.textContent, "");
  assert.equal(status.hidden, true);
  assert.deepEqual(state.main.children, pageNodes);
  assert.equal(state.refreshes, 0);
  assert.equal(state.timers.size, 1);
});

test("sidebar progress handles discovery, empty libraries, and missing or invalid counts", () => {
  assert.equal(libraryIndexStatusText({ status: "scanning", progress: { phase: "discovering" } }), "Finding albums…");
  assert.equal(libraryIndexStatusText({ status: "scanning", progress: { phase: "indexing_albums", completed: 9, total: 3 } }), "Indexing 3/3 albums");
  assert.equal(libraryIndexStatusText({ status: "scanning", progress: { phase: "reading_tracks", completed: -1, total: 3 } }), "Indexing library…");
  assert.equal(libraryIndexStatusText({ status: "scanning" }), "Indexing library…");
  assert.equal(libraryIndexStatusText({ status: "ready", albums: 0 }), "");
  assert.equal(libraryIndexStatusText({ status: "ready", albums: 1 }), "");
  assert.equal(libraryIndexStatusText({ status: "unconfigured" }), "");
  assert.equal(libraryIndexStatusText({ status: "missing" }), "Music folder unavailable");
});

test("metadata phases stay visible across pages and hide when work stops", async () => {
  const state = harness({ page: "users" });
  const shell = setupShell();
  cleanups.push(shell);
  const status = state.elements.get("library-index-status");
  const pageNodes = [...state.main.children];
  await state.poll({ status: "scanning", progress: { phase: "indexing_albums", completed: 6, total: 6 } });
  for (const [phase, label] of [["artists", "Artist sweep"], ["album_artists", "Album credits sweep"], ["albums", "Album sweep"]]) {
    await state.poll({ status: "ready", albums: 6, metadata: { phase, completed: 2, total: 6 } });
    assert.equal(status.textContent, `${label}…`);
    assert.equal(status.dataset.status, "metadata");
    assert.equal(status.hidden, false);
  }
  await state.poll({ status: "ready", metadata: { phase: "albums", completed: 3, total: 6 } });
  assert.equal(status.textContent, "Album sweep…");
  state.document.body.dataset.page = "settings";
  shell.sync();
  await state.poll({ status: "ready", metadata: { phase: null, completed: 3, total: 6 } });
  assert.equal(status.textContent, "", "ending a sweep hides progress even when some albums remain incomplete");
  assert.equal(status.hidden, true);
  assert.equal(status.dataset.status, "ready");
  assert.deepEqual(state.main.children, pageNodes);
  assert.equal(state.refreshes, 0);
  assert.equal(state.timers.size, 1);
});

test("metadata completion refreshes only an active Library view once", async () => {
  const state = harness({ page: "library" });
  const shell = setupShell();
  cleanups.push(shell);
  await state.poll({ status: "ready", metadata: { phase: "albums", completed: 1, total: 2 } });
  assert.equal(state.refreshes, 0);
  await state.poll({ status: "ready", metadata: { phase: null, completed: 2, total: 2 } });
  assert.equal(state.refreshes, 1);
  await state.poll({ status: "ready", metadata: { phase: null, completed: 2, total: 2 } });
  assert.equal(state.refreshes, 1);
  assert.equal(libraryIndexStatusText({ status: "ready", metadata: { completed: 5, total: 3 } }), "");
  assert.equal(libraryIndexStatusText({ status: "ready", metadata: { phase: "albums", completed: -1, total: 3 } }), "Album sweep…");
  assert.equal(libraryIndexStatusText({ status: "scanning", metadata: { phase: "albums", completed: 1, total: 2 } }), "Indexing library…", "local indexing takes priority over obsolete metadata work");
});

test("metadata status follows the current artist or album without showing completion metrics", async () => {
  const state = harness({ page: "users" });
  const shell = setupShell();
  cleanups.push(shell);
  const status = state.elements.get("library-index-status");
  for (const [phase, label] of [["artists", "Artist sweep"], ["album_artists", "Album credits sweep"], ["albums", "Album sweep"]]) {
    for (const [checked, [artist, album]] of [["Björk", "Homogenic"], ["Miles Davis", "Kind of Blue"]].entries()) {
      const current = { artist, album: phase === "artists" ? null : album };
      await state.poll({ status: "ready", metadata: { phase, completed: 0, total: 6, progress: { phase, checked, total: 6, current } } });
      const target = phase === "artists" ? artist : `${artist} — ${album}`;
      assert.equal(status.textContent, `${label} [${checked + 1}/6]: ${target}`);
      assert.equal(status.hidden, false);
      assert.equal(status.dataset.status, "metadata");
    }
    shell.sync();
    assert.equal(state.timers.size, 1);
  }
  await state.poll({ status: "ready", metadata: { phase: "albums", completed: 1, total: 6, progress: { phase: "albums", checked: 3, total: 6, current: null } } });
  assert.equal(status.textContent, "Album sweep [3/6]", "a finished lookup does not leave its name behind or advance the position twice");
  await state.poll({ status: "ready", metadata: { phase: null, completed: 1, total: 6, progress: null } });
  assert.equal(status.hidden, true, "partial completion does not keep the spinner running");
  assert.equal(state.refreshes, 0);
});

test("metadata status ignores obsolete targets and handles missing names safely", () => {
  const metadata = { phase: "artists", completed: 0, total: 6 };
  for (const progress of [undefined, { phase: "artists", current: null }, { phase: "artists", current: { artist: 42 } }, { phase: "artists", current: { artist: "  " } }, { phase: "albums", current: { artist: "Obsolete artist", album: "Obsolete album" } }]) {
    assert.equal(libraryIndexStatusText({ status: "ready", metadata: { ...metadata, progress } }), "Artist sweep…");
  }
  assert.equal(libraryIndexStatusText({ status: "ready", metadata: { phase: "albums", progress: { phase: "albums", current: { artist: "", album: "Unknown performer’s album" } } } }), "Album sweep: Unknown performer’s album");
  assert.equal(libraryIndexStatusText({ status: "ready", metadata: { ...metadata, progress: { phase: "artists", current: { artist: "<b>Artist & Friends</b>" } } } }), "Artist sweep: <b>Artist & Friends</b>");
});

test("sweep positions match the current item number in logs rather than saved album counts", () => {
  const metadata = { phase: "album_artists", completed: 0, total: 149 };
  const current = { artist: "Björk", album: "Homogenic" };
  const text = (checked, target = current, total = 149) => libraryIndexStatusText({ status: "ready", metadata: { ...metadata, progress: { phase: "album_artists", checked, total, current: target } } });
  assert.equal(text(0), "Album credits sweep [1/149]: Björk — Homogenic");
  assert.equal(text(122), "Album credits sweep [123/149]: Björk — Homogenic");
  assert.equal(text(123, null), "Album credits sweep [123/149]");
  assert.equal(text(148), "Album credits sweep [149/149]: Björk — Homogenic");
  assert.equal(text(149, null), "Album credits sweep [149/149]");
  assert.equal(text(160), "Album credits sweep [149/149]: Björk — Homogenic");
  assert.equal(text(0, null, 0), "Album credits sweep [0/0]");
  for (const [checked, total] of [[-1, 149], [122, null], [1.5, 149], [122, -1]]) {
    assert.equal(text(checked, current, total), "Album credits sweep: Björk — Homogenic");
  }
});

test("starting a scan uses only the sidebar and aborts superseded status requests", async () => {
  const state = harness();
  const requests = [];
  globalThis.fetch = (url, options) => {
    const pending = Promise.withResolvers();
    requests.push({ signal: options.signal, ...pending });
    return pending.promise;
  };
  const shell = setupShell();
  cleanups.push(shell);
  const status = state.elements.get("library-index-status");
  state.document.dispatchEvent({ type: "library:scan-started" });
  assert.equal(status.textContent, "Finding albums…");
  assert.equal(requests[0].signal.aborted, true);
  requests[0].resolve({ ok: true, json: async () => ({ status: "ready", albums: 99 }) });
  requests[1].resolve({ ok: true, json: async () => ({ status: "scanning", progress: { phase: "indexing_albums", completed: 1, total: 4 } }) });
  await settle();
  assert.equal(status.textContent, "Indexing 1/4 albums");
  assert.deepEqual(state.main.children, []);
  assert.equal(state.timers.size, 1);
});

test("authentication navigation removes old bindings and aborts pending shell work", async () => {
  const state = harness({ scanning: true });
  const pending = Promise.withResolvers();
  let signal;
  globalThis.fetch = async (url, options) => { signal = options.signal; return pending.promise; };
  globalThis.FormData = class { *[Symbol.iterator]() {} };
  const shell = setupShell();
  cleanups.push(shell);
  const modal = state.navigation.nodes["nav-modal"];
  const form = new Element("form");
  form.setAttribute("action", "/logout");
  const button = new Element("button");
  button.setAttribute("type", "submit");
  form.append(button);
  modal.append(form);
  const [request] = modal.emit("submit", { target: form, preventDefault() {} });
  assert.equal(signal.aborted, false);
  state.removeNavigation();
  shell.sync();
  assert.equal(signal.aborted, true);
  assert.equal(modal.listeners.get("submit").size, 0);
  assert.equal(state.document.listeners.get("visibilitychange").size, 0);
  pending.resolve({ ok: true, json: async () => ({ redirect: "/login" }) });
  await request;
  assert.equal(state.navigations, 0);
  state.addNavigation();
  shell.sync();
  assert.equal(state.media.length, 2);
  assert.equal(state.elements.get("nav-menu-toggle").listeners.get("click").size, 1);
});
