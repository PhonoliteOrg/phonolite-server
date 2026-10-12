import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { configureNavigation, fetchJson } from "../static/js/shared.js";
import { closeAllModals, confirmAction } from "../static/js/dialogs.js";

// Long-lived resource contracts without launching a browser or an HTTP server.
class Events {
  listeners = new Map();
  addEventListener(name, callback, options = {}) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Map());
    this.listeners.get(name).set(callback, options);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) {
    for (const [callback, options] of [...this.listeners.get(name) || []]) {
      if (options.once) this.removeEventListener(name, callback);
      callback(event);
    }
  }
  dispatchEvent(event) { this.emit(event.type, event); return true; }
}

class Element extends Events {
  attrs = new Map();
  children = [];
  dataset = {};
  className = "";
  inert = false;
  tabIndex = -1;
  textContent = "";
  constructor(tag = "div") { super(); this.tagName = tag.toUpperCase(); }
  classList = {
    contains: (name) => this.className.split(" ").includes(name),
    add: (...names) => { this.className = [...new Set([...this.className.split(" ").filter(Boolean), ...names])].join(" "); },
    remove: (...names) => { this.className = this.className.split(" ").filter((name) => !names.includes(name)).join(" "); },
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
      if (query.startsWith(".")) return query.slice(1).split(".").every((name) => this.classList.contains(name));
      if (query === "[autofocus]") return this.hasAttribute("autofocus");
      return query.toUpperCase() === this.tagName;
    });
  }
  querySelectorAll(selector) {
    return this.children.flatMap((child) => [...(child.matches(selector) ? [child] : []), ...child.querySelectorAll(selector)]);
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest(selector) {
    for (let element = this; element; element = element.parentElement) {
      if (selector === "[inert]" && element.inert || element.matches(selector)) return element;
    }
    return null;
  }
  focus() { document.activeElement = this; }
  getClientRects() { return [{}]; }
  contains(target) { for (let element = target; element; element = element.parentElement) if (element === this) return true; return false; }
}

const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch };
const cleanups = [];
afterEach(() => {
  closeAllModals();
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});
const settle = () => new Promise((resolve) => setImmediate(resolve));
const expired = () => ({ ok: false, status: 401, json: async () => ({ error: "Session expired" }) });

function harness() {
  const document = new Events();
  document.body = new Element("body");
  const content = new Element();
  content.className = "content";
  document.body.append(content);
  document.activeElement = content;
  document.createElement = (tag) => new Element(tag);
  document.querySelectorAll = (selector) => document.body.querySelectorAll(selector);
  document.querySelector = (selector) => {
    return document.body.querySelector(selector);
  };
  const window = {
    location: { href: "https://console.example/library" },
  };
  Object.assign(globalThis, { document, window });
  return { document, content };
}

test("simultaneous expired requests share one pending authentication navigation", async () => {
  harness();
  const navigation = Promise.withResolvers();
  let calls = 0;
  configureNavigation({ navigate() { calls++; return navigation.promise; } });
  globalThis.fetch = async () => expired();
  const first = fetchJson("/logs/tail").catch((error) => error);
  const second = fetchJson("/status/library").catch((error) => error);
  await settle();
  assert.equal(calls, 1);
  navigation.resolve(true);
  assert.equal((await first).name, "AbortError");
  assert.equal((await second).name, "AbortError");
});

test("a replaced router ignores stale expiry and old completion cannot clear the newer redirect", async () => {
  harness();
  const oldNavigation = Promise.withResolvers();
  const newNavigation = Promise.withResolvers();
  const staleResponse = Promise.withResolvers();
  let oldCalls = 0;
  let newCalls = 0;
  configureNavigation({ navigate() { oldCalls++; return oldNavigation.promise; } });
  globalThis.fetch = async (url) => url === "/stale" ? staleResponse.promise : expired();
  const stale = fetchJson("/stale").catch((error) => error);
  const old = fetchJson("/status/library").catch((error) => error);
  await settle();
  assert.equal(oldCalls, 1);
  configureNavigation({ navigate() { newCalls++; return newNavigation.promise; } });
  const current = fetchJson("/logs/tail").catch((error) => error);
  await settle();
  staleResponse.resolve(expired());
  assert.equal((await stale).name, "AbortError");
  oldNavigation.resolve(false);
  assert.equal((await old).name, "AbortError");
  const another = fetchJson("/status/library").catch((error) => error);
  await settle();
  assert.equal(newCalls, 1, "old completion must leave the current coalesced redirect intact");
  newNavigation.resolve(true);
  assert.equal((await current).name, "AbortError");
  assert.equal((await another).name, "AbortError");
});

test("rapid destructive confirmations create one dialog and authorize only one caller", async () => {
  const { document } = harness();
  const options = { title: "Delete source?", message: "Delete this source?", danger: true };
  const first = confirmAction(options);
  const duplicates = Array.from({ length: 100 }, () => confirmAction(options));
  assert.equal(document.querySelectorAll(".modal").length, 1);
  assert.ok((await Promise.all(duplicates)).every((result) => result === false));
  const modal = document.querySelectorAll(".modal")[0];
  modal.querySelectorAll("button").at(-1).emit("click");
  assert.equal(await first, true);
  assert.equal(document.querySelectorAll(".modal").length, 0);
  const later = confirmAction(options);
  assert.equal(document.querySelectorAll(".modal").length, 1);
  closeAllModals();
  assert.equal(await later, false);
  assert.equal(document.querySelectorAll(".modal").length, 0);
});
