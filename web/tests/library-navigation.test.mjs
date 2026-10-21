import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { libraryUrl, setupLibrary, updateLibrarySearch, validLibraryFragment } from "../static/js/library.js";
import { configureNavigation } from "../static/js/shared.js";

// DOM doubles expose individual form/result nodes so identity, focus, and
// mutations can be verified without starting a browser or HTTP server.
class Element {
  attrs = new Map();
  selectors = new Map();
  listeners = new Map();
  dataset = {};
  className = "";
  classList = {
    contains: (name) => this.className.split(" ").includes(name),
    add: (name) => { this.className += ` ${name}`; },
  };
  writes = 0;
  value = "";
  defaultValue = "";
  checked = false;
  connected = true;
  children = [];
  constructor(html = "") { this.html = html; }
  get innerHTML() { return this.html; }
  set innerHTML(value) { this.html = value; this.writes++; this.onHtml?.(value); }
  get isConnected() { return this.connected; }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  hasAttribute(name) { return this.attrs.has(name); }
  get hidden() { return this.hasAttribute("hidden"); }
  set hidden(value) { if (value) this.setAttribute("hidden", ""); else this.removeAttribute("hidden"); }
  removeAttribute(name) { this.attrs.delete(name); }
  querySelector(selector) { return this.selectors.get(selector) || null; }
  querySelectorAll(selector) { return this.selectors.get(selector) || []; }
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  replaceWith(element) { this.replacedWith = element; this.onReplace?.(element); }
  append(element) { this.children.push(element); }
  prepend(element) { this.children.unshift(element); }
  focus() { document.activeElement = this; }
  before(element) { this.insertedBefore = element; }
  closest() { return null; }
  remove() { this.connected = false; }
}

const originals = { document: globalThis.document, window: globalThis.window };
afterEach(() => { configureNavigation(null); Object.assign(globalThis, originals); });

function searchBlock({ search = "", filter = "artists", clear = false, counts = "3 artists" } = {}) {
  const block = new Element(`${search}/${filter}/${counts}`);
  const form = new Element();
  form.action = "/library";
  form.matches = (selector) => selector === "form[data-library-search]";
  const input = new Element();
  input.value = input.defaultValue = search;
  const filterInput = new Element();
  filterInput.value = filter;
  const icons = new Element();
  const clearLink = clear ? new Element() : null;
  clearLink?.setAttribute("href", `/library?search=&filter=${filter}`);
  if (clearLink) {
    clearLink.href = `https://console.example/library?search=&filter=${filter}`;
    clearLink.className = "icon-button search-clear";
  }
  form.selectors.set("input[name='search']", input);
  form.selectors.set("input[name='filter']", filterInput);
  form.selectors.set(".search-clear", clearLink);
  form.selectors.set(".search-icon-container", icons);
  block.selectors.set("form[data-library-search]", form);
  const snapshot = new Element(counts);
  block.selectors.set(".snapshot-card", snapshot);
  const tabs = new Element(filter);
  const links = ["artists", "albums", "tracks"].map((kind) => {
    const link = new Element();
    link.setAttribute("href", `/library?search=${search}&filter=${kind}`);
    link.setAttribute("class", kind === filter ? "tab-chip active" : "tab-chip");
    if (kind === filter) link.setAttribute("aria-current", "page");
    return link;
  });
  tabs.selectors.set("a[data-library-link]", links);
  block.selectors.set(".snapshot-tabs", tabs);
  return { block, form, input, filterInput, icons, clearLink, snapshot, tabs, links };
}

function harness(initial = searchBlock(), prepareContent = () => {}) {
  const nodes = new Map([
    ["library-content", new Element("initial results")],
    ["library-pagination", new Element("initial page")],
    ["library-message", new Element("")],
    ["library-search-block", initial.block],
    ["library-search", initial.input],
  ]);
  const listeners = new Map();
  const templates = new Map();
  const timers = new Map();
  const navigations = [];
  let timerId = 0;
  let parses = 0;
  const document = {
    activeElement: initial.input,
    getElementById: (id) => nodes.get(id),
    addEventListener(name, callback) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name).add(callback);
    },
    removeEventListener(name, callback) { listeners.get(name)?.delete(callback); },
    createElement(tag) {
      if (tag !== "template") return new Element();
      parses++;
      return { set innerHTML(value) { this.content = templates.get(value); } };
    },
  };
  globalThis.document = document;
  const window = new Element();
  window.location = { href: "https://console.example/library" };
  window.setTimeout = (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; };
  window.clearTimeout = (id) => timers.delete(id);
  globalThis.window = window;
  configureNavigation({ navigate(url, options) { navigations.push({ url, options }); return Promise.resolve(true); } });
  prepareContent(nodes.get("library-content"));
  const lifecycle = setupLibrary();
  return {
    ...initial, nodes, document, window, lifecycle, listeners, templates, timers, navigations,
    emit(name, event) { for (const callback of listeners.get(name) || []) callback(event); },
    type(value, options = {}) { initial.input.value = value; for (const callback of listeners.get("input") || []) callback({ target: initial.input, ...options }); },
    flushSearch() { const pending = [...timers.values()]; timers.clear(); pending.forEach(({ callback }) => callback()); },
    get parses() { return parses; },
  };
}

const fragment = (search = "search-fragment") => ({
  content: "new albums",
  pagination: "next albums page",
  message: "",
  search_block: search,
});

test("typing replaces browse content and debounces one search across the catalog", () => {
  const current = harness(searchBlock({ filter: "tracks", clear: true }));
  current.type("radio");
  assert.equal(current.navigations.length, 0);
  assert.equal(current.timers.size, 1);
  assert.equal([...current.timers.values()][0].delay, 450);
  assert.match(current.nodes.get("library-content").innerHTML, /Searching library/);
  assert.equal(current.tabs.hidden, true);
  assert.equal(current.nodes.get("library-pagination").hidden, true);
  assert.equal(current.clearLink.hidden, false);
  current.type("radiohed");
  assert.equal(current.timers.size, 1);
  current.flushSearch();
  assert.equal(current.navigations.length, 1);
  assert.equal(current.navigations[0].url.searchParams.get("search"), "radiohed");
  assert.equal(current.navigations[0].url.searchParams.get("filter"), "tracks");
  assert.equal(current.navigations[0].url.searchParams.has("offset"), false);
  assert.equal(current.navigations[0].options.focus, false);
  assert.equal(current.document.activeElement, current.input);
  current.lifecycle.dispose();
});

test("clear cancels queued and stale searches and restores the selected browse category", () => {
  const current = harness(searchBlock({ search: "radio", filter: "albums", clear: true }));
  const stale = current.lifecycle.navigationFor("/library?search=radio&filter=albums");
  current.type("radiohead");
  const event = { button: 0, target: { closest: () => current.clearLink }, preventDefault() { this.defaultPrevented = true; } };
  current.emit("click", event);
  assert.equal(event.defaultPrevented, true);
  assert.equal(current.timers.size, 0);
  assert.equal(current.input.value, "");
  assert.equal(current.clearLink.hidden, true);
  assert.equal(current.tabs.hidden, false);
  assert.equal(current.navigations.length, 1);
  assert.equal(current.navigations[0].url.searchParams.get("search"), "");
  assert.equal(current.navigations[0].url.searchParams.get("filter"), "albums");
  assert.equal(current.navigations[0].options.refresh, true, "clearing also restores a view replaced before the debounce fired");
  const pending = current.nodes.get("library-content").innerHTML;
  stale.apply(fragment());
  assert.equal(current.nodes.get("library-content").innerHTML, pending);
  const next = searchBlock({ filter: "albums", clear: true });
  current.templates.set("browse", next.block);
  current.lifecycle.navigationFor("/library?search=&filter=albums").apply({ ...fragment("browse"), content: "album grid", pagination: "album pages" });
  assert.equal(current.nodes.get("library-content").innerHTML, "album grid");
  assert.equal(current.nodes.get("library-pagination").hidden, false);
  assert.equal(current.document.activeElement, current.input);
  current.lifecycle.dispose();
});

test("edited queries ignore older results and clear-by-deletion restores browsing immediately", () => {
  const current = harness(searchBlock({ search: "radio", clear: true }));
  const older = current.lifecycle.navigationFor("/library?search=radio");
  current.type("paranoid");
  const pending = current.nodes.get("library-content").innerHTML;
  older.apply(fragment());
  assert.equal(current.nodes.get("library-content").innerHTML, pending);
  assert.equal(current.input.value, "paranoid");
  current.type("   ");
  assert.equal(current.timers.size, 0);
  assert.equal(current.navigations.length, 1);
  assert.equal(current.navigations[0].url.searchParams.get("search"), "");
  assert.equal(current.tabs.hidden, false);
  current.lifecycle.dispose();
});

test("Enter submits immediately and composition finishes before automatic search", () => {
  const current = harness(searchBlock({ clear: true }));
  current.emit("compositionstart", { target: current.input });
  current.type("東京", { isComposing: true });
  assert.equal(current.timers.size, 0);
  assert.equal(current.navigations.length, 0);
  current.emit("compositionend", { target: current.input });
  assert.equal(current.timers.size, 1);
  current.emit("submit", { target: current.form, preventDefault() {} });
  assert.equal(current.timers.size, 0);
  assert.equal(current.navigations.length, 1);
  assert.equal(current.navigations[0].url.searchParams.get("search"), "東京");
  current.lifecycle.dispose();
});

test("short searches show guidance and disposed or history-replaced pages cancel pending searches", () => {
  const current = harness(searchBlock({ clear: true }));
  current.type("r");
  assert.match(current.nodes.get("library-content").innerHTML, /at least 2 characters/);
  assert.equal(current.timers.size, 0);
  assert.equal(current.navigations.length, 1);
  current.type("radio");
  for (const callback of current.window.listeners.get("popstate")) callback();
  assert.equal(current.timers.size, 0);
  current.type("radiohead");
  current.lifecycle.dispose();
  assert.equal(current.timers.size, 0);
  for (const callbacks of current.listeners.values()) assert.equal(callbacks.size, 0);
  for (const callbacks of current.window.listeners.values()) assert.equal(callbacks.size, 0);
});

test("background browse refreshes cannot overwrite a pending search and mixed results keep input focus", () => {
  const current = harness(searchBlock({ filter: "tracks", clear: true }));
  current.type("radio");
  const background = current.lifecycle.navigationFor("/library?filter=tracks");
  background.apply(fragment());
  assert.equal(current.input.value, "radio");
  assert.equal(current.input.defaultValue, "");
  assert.equal(current.timers.size, 1);
  assert.match(current.nodes.get("library-content").innerHTML, /Searching library/);
  current.flushSearch();
  current.templates.set("all-matches", searchBlock({ search: "radio", filter: "tracks", clear: true }).block);
  const results = '<section class="library-search-results">Artist, Album, Track</section>';
  current.lifecycle.navigationFor(current.navigations[0].url).apply({ ...fragment("all-matches"), content: results, pagination: "" });
  assert.equal(current.nodes.get("library-content").innerHTML, results);
  assert.equal(current.nodes.get("library-pagination").innerHTML, "");
  assert.equal(current.tabs.hidden, true);
  assert.equal(current.clearLink.hidden, false);
  assert.equal(current.document.activeElement, current.input);
  current.lifecycle.dispose();
});

test("failed live searches stop the spinner, preserve the query, and can be retried with Enter", async () => {
  const current = harness(searchBlock({ clear: true }));
  let requests = 0;
  configureNavigation({ navigate() { return Promise.resolve(++requests > 1); } });
  current.type("radiohead");
  current.flushSearch();
  await Promise.resolve();
  assert.match(current.nodes.get("library-content").innerHTML, /Press Enter to retry/);
  assert.doesNotMatch(current.nodes.get("library-content").innerHTML, /class="spinner"/);
  assert.equal(current.input.value, "radiohead");
  current.emit("submit", { target: current.form, preventDefault() {} });
  assert.equal(requests, 2);
  current.lifecycle.dispose();
});

test("library fragment navigation retains the form, focused search, and filter tabs", () => {
  const current = harness(searchBlock({ search: "moon", clear: true }));
  const next = searchBlock({ search: "moon", filter: "albums", clear: true });
  current.templates.set("search-fragment", next.block);
  const navigation = current.lifecycle.navigationFor("/library?search=moon&filter=albums");
  assert.deepEqual(navigation.headers, { "X-Fragment": "true" });
  assert.equal(navigation.busyElement, current.nodes.get("library-content"));
  assert.equal(navigation.isValid(fragment()), true);
  assert.equal(navigation.apply(fragment()), undefined);
  assert.equal(current.nodes.get("library-content").innerHTML, "new albums");
  assert.equal(current.nodes.get("library-pagination").innerHTML, "next albums page");
  assert.equal(current.nodes.get("library-message").writes, 0);
  assert.equal(current.block.writes, 0, "the search region does not remount");
  assert.equal(current.form.writes, 0);
  assert.equal(current.document.activeElement, current.input);
  assert.equal(current.input.value, "moon");
  assert.equal(current.filterInput.value, "albums");
  assert.equal(current.links[0].getAttribute("aria-current"), null);
  assert.equal(current.links[1].getAttribute("aria-current"), "page");
  assert.equal(current.tabs.writes, 0, "the focused tab nodes survive");
  assert.equal(current.snapshot.writes, 0, "unchanged counts survive");
  assert.equal(current.clearLink.getAttribute("href"), "/library?search=&filter=albums");
});

test("fragment completion preserves text edited while a request was pending", () => {
  const current = harness(searchBlock({ search: "moon" }));
  const next = searchBlock({ search: "sun", clear: true });
  current.templates.set("search-fragment", next.block);
  const navigation = current.lifecycle.navigationFor("/library?search=sun");
  current.input.value = "sunflower";
  navigation.apply(fragment());
  assert.equal(current.input.value, "sunflower");
  assert.equal(current.input.defaultValue, "sun");
  assert.equal(current.document.activeElement, current.input);
  assert.deepEqual(current.icons.children, [next.clearLink], "clear search appears without a submit button");
});

test("history-selected search values update while repeated results avoid DOM writes", () => {
  const current = harness(searchBlock({ search: "moon", clear: true }));
  current.templates.set("search-fragment", searchBlock({ search: "", filter: "tracks" }).block);
  const navigation = current.lifecycle.navigationFor("/library?filter=tracks");
  navigation.apply(fragment());
  navigation.apply(fragment());
  assert.equal(current.input.value, "");
  assert.equal(current.clearLink.connected, false);
  assert.equal(current.nodes.get("library-content").writes, 1);
  assert.equal(current.nodes.get("library-pagination").writes, 1);
});

test("malformed fragments and other console views leave mounted library regions alone", () => {
  const current = harness();
  const navigation = current.lifecycle.navigationFor("/library?filter=albums");
  const malformed = { ...fragment(), search_block: null };
  assert.equal(validLibraryFragment(malformed), false);
  assert.equal(validLibraryFragment({ redirect: "/login" }), false);
  assert.throws(() => navigation.apply(malformed), /unexpected library view/);
  assert.equal(current.nodes.get("library-content").writes, 0);
  assert.equal(current.lifecycle.navigationFor("/settings"), null);
  assert.equal(current.lifecycle.navigationFor("https://other.example/library"), null);
  assert.equal(current.lifecycle.navigationFor("/library/album/123"), null);
  assert.equal(libraryUrl("http://[", "https://console.example/library"), null);
  current.lifecycle.dispose();
  for (const callbacks of current.listeners.values()) assert.equal(callbacks.size, 0);
});

test("an identical search region needs no parsing or mutation", () => {
  const current = harness();
  updateLibrarySearch(current.block, current.block.innerHTML, current.input.value);
  assert.equal(current.parses, 0);
  assert.equal(current.block.writes, 0);
});

test("identical fragment content retains decorated artwork and changed results release its listeners", () => {
  const image = new Element();
  image.className = "tile-cover";
  image.complete = false;
  const current = harness(searchBlock(), (content) => {
    content.selectors.set("img.tile-cover, img.cover-lg, img.cover-thumb", [image]);
    image.onReplace = () => { content.html = "locally decorated image frame and spinner"; };
    content.onHtml = () => content.selectors.set("img.tile-cover, img.cover-lg, img.cover-thumb", []);
  });
  const frame = image.replacedWith;
  assert.equal(image.className, "artwork-image");
  assert.equal(image.listeners.get("load").size, 1);
  const navigation = current.lifecycle.navigationFor("/library?filter=albums");
  navigation.apply({ content: "initial results", pagination: "initial page", message: "", search_block: current.block.innerHTML });
  assert.equal(current.nodes.get("library-content").writes, 0, "generated wrappers do not trigger replacement");
  assert.equal(image.replacedWith, frame);
  assert.equal(image.listeners.get("load").size, 1);
  navigation.apply({ content: "other results", pagination: "initial page", message: "", search_block: current.block.innerHTML });
  assert.equal(current.nodes.get("library-content").writes, 1);
  assert.equal(image.className, "tile-cover");
  assert.equal(image.listeners.get("load").size, 0);
  assert.equal(image.listeners.get("error").size, 0);
  current.lifecycle.dispose();
});

test("library errors update only when their server markup changes", () => {
  const current = harness();
  const message = current.nodes.get("library-message");
  const navigation = current.lifecycle.navigationFor("/library?filter=albums");
  const data = { content: "initial results", pagination: "initial page", message: '<p class="inline-error visible" role="alert">Folder unavailable</p>', search_block: current.block.innerHTML };
  navigation.apply(data);
  assert.equal(message.innerHTML, data.message);
  assert.equal(message.writes, 1);
  navigation.apply(data);
  assert.equal(message.writes, 1, "unchanged errors retain the existing markup");
  navigation.apply({ ...data, message: "" });
  assert.equal(message.innerHTML, "");
  assert.equal(message.writes, 2);
  current.lifecycle.dispose();
});
