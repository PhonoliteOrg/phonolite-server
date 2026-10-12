import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { libraryUrl, setupLibrary, updateLibrarySearch, validLibraryFragment } from "../static/js/library.js";

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
  before(element) { this.insertedBefore = element; }
  closest() { return null; }
  remove() { this.connected = false; }
}

const originals = { document: globalThis.document, window: globalThis.window };
afterEach(() => Object.assign(globalThis, originals));

function searchBlock({ search = "", filter = "artists", clear = false, counts = "3 artists", contributors = "checkbox" } = {}) {
  const block = new Element(`${search}/${filter}/${counts}`);
  const form = new Element();
  const input = new Element();
  input.value = input.defaultValue = search;
  const filterInput = new Element();
  filterInput.value = filter;
  const icons = new Element();
  const clearLink = clear ? new Element() : null;
  clearLink?.setAttribute("href", `/library?search=&filter=${filter}`);
  const contributor = contributors ? new Element() : null;
  if (contributor) { contributor.type = contributors; contributor.value = "true"; }
  form.selectors.set("input[name='search']", input);
  form.selectors.set("input[name='filter']", filterInput);
  form.selectors.set(".search-clear", clearLink);
  form.selectors.set(".search-icon-container", icons);
  form.selectors.set("input[name='include_contributors']", contributor);
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
  return { block, form, input, filterInput, icons, clearLink, snapshot, tabs, links, contributor };
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
  globalThis.window = { location: { href: "https://console.example/library" } };
  prepareContent(nodes.get("library-content"));
  const lifecycle = setupLibrary();
  return { ...initial, nodes, document, lifecycle, listeners, templates, get parses() { return parses; } };
}

const fragment = (search = "search-fragment") => ({
  content: "new albums",
  pagination: "next albums page",
  message: "",
  search_block: search,
});

test("library fragment navigation retains the form, focused search, and filter tabs", () => {
  const current = harness(searchBlock({ search: "moon", clear: true }));
  const next = searchBlock({ search: "moon", filter: "albums", clear: true, contributors: "hidden" });
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
  assert.equal(current.contributor.connected, false);
  assert.deepEqual(current.form.children, [next.contributor]);
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
