import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { logTailOverlap, setupLogsPage } from "../static/js/logs.js";
import { configureNavigation } from "../static/js/shared.js";

// Count DOM work across repeated polls without launching an application or
// browser. Fragments flatten into their parent, as they do in a real DOM.
class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) { for (const callback of [...this.listeners.get(name) || []]) callback(event); }
}

class Element extends Events {
  attrs = new Map();
  dataset = {};
  className = "";
  children = [];
  textContent = "";
  innerHTML = "";
  scrollTop = 0;
  clientHeight = 320;
  style = {};
  layoutReads = 0;
  mutations = 0;
  classList = {
    contains: (name) => this.className.split(" ").includes(name),
    add: (...names) => { this.className = [...new Set([...this.className.split(" ").filter(Boolean), ...names])].join(" "); },
    remove: (...names) => { this.className = this.className.split(" ").filter((name) => !names.includes(name)).join(" "); },
    toggle: (name, force) => {
      const include = force ?? !this.classList.contains(name);
      if (include) this.classList.add(name);
      else this.classList.remove(name);
      return include;
    },
  };
  get scrollHeight() { this.layoutReads++; return this.children.length * 33; }
  get firstChild() { return this.children[0] || null; }
  get lastChild() { return this.children.at(-1) || null; }
  get firstElementChild() { return this.firstChild; }
  get nextSibling() {
    if (!this.parentElement) return null;
    return this.parentElement.children[this.parentElement.children.indexOf(this) + 1] || null;
  }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  removeAttribute(name) { this.attrs.delete(name); }
  append(...nodes) {
    for (const node of nodes) {
      if (node.fragment) {
        const children = node.children;
        node.children = [];
        for (const child of children) { child.parentElement = this; this.children.push(child); }
        this.mutations += children.length;
        continue;
      }
      node.remove();
      node.parentElement = this;
      this.children.push(node);
      this.mutations++;
    }
  }
  appendChild(node) { this.append(node); return node; }
  prepend(node) { this.insertBefore(node, this.firstChild); }
  insertBefore(node, reference) {
    if (node === reference) return node;
    if (node.fragment) { for (const child of [...node.children]) this.insertBefore(child, reference); return node; }
    node.remove();
    const index = reference ? this.children.indexOf(reference) : this.children.length;
    assert.ok(index >= 0);
    node.parentElement = this;
    this.children.splice(index, 0, node);
    this.mutations++;
    return node;
  }
  replaceChildren(...nodes) { for (const child of [...this.children]) child.remove(); this.append(...nodes); }
  remove() {
    if (this.parentElement) {
      this.parentElement.mutations++;
      const index = this.parentElement.children.indexOf(this);
      this.parentElement.children.splice(index, 1);
    }
    this.parentElement = null;
  }
  removeChild(node) { assert.equal(node.parentElement, this); node.remove(); return node; }
  before(node) { this.parentElement?.insertBefore(node, this); }
  querySelectorAll(selector) {
    const nodes = this.children.flatMap((child) => [child, ...child.querySelectorAll(selector)]);
    if (selector === ".log-line") return nodes.filter((node) => node.classList.contains("log-line"));
    return [];
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest() { return null; }
}

const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch };
const cleanups = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});
const settle = () => new Promise((resolve) => setImmediate(resolve));

function harness({ limit = 10000, following = true } = {}) {
  const document = new Events();
  document.hidden = false;
  const window = new Events();
  window.location = { href: "https://console.example/logs" };
  const timers = new Map();
  let timerId = 0;
  window.setTimeout = (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; };
  window.clearTimeout = (id) => timers.delete(id);
  let creations = 0;
  document.createElement = (tag) => { creations++; const node = new Element(); node.tagName = tag.toUpperCase(); return node; };
  document.createDocumentFragment = () => { const node = new Element(); node.fragment = true; return node; };
  const nodes = Object.fromEntries([
    "log-view", "log-panel", "log-empty", "log-tabs", "logs-follow", "logs-refresh", "logs-clear", "log-spinner",
    "indexing-issues", "indexing-issues-card", "indexing-issue-count",
  ].map((id) => [id, new Element()]));
  nodes["logs-follow"].checked = following;
  const card = new Element();
  card.dataset.logLimit = String(limit);
  const main = new Element();
  main.append(card, nodes["indexing-issues-card"]);
  card.append(nodes["log-panel"]);
  nodes["log-panel"].append(nodes["log-view"]);
  nodes["indexing-issues-card"].append(nodes["indexing-issues"]);
  const tabs = new Map(["all", "info", "errors"].map((view) => {
    const tab = new Element();
    tab.dataset.log = view;
    tab.className = view === "all" ? "active" : "";
    tab.closest = () => tab;
    return [view, tab];
  }));
  nodes["log-tabs"].querySelector = () => [...tabs.values()].find((tab) => tab.classList.contains("active"));
  nodes["log-tabs"].querySelectorAll = () => [...tabs.values()];
  document.getElementById = (id) => nodes[id] || null;
  document.querySelector = (selector) => selector === ".log-card" ? card : selector === ".content" ? main : null;
  const requests = [];
  Object.assign(globalThis, {
    window, document,
    fetch: (url, options) => {
      const pending = Promise.withResolvers();
      requests.push({ url, options, ...pending });
      return pending.promise;
    },
  });
  const dispose = setupLogsPage();
  cleanups.push(dispose);
  const rows = () => nodes["log-view"].querySelectorAll(".log-line");
  return {
    nodes, document, window, timers, requests, rows, dispose,
    get creations() { return creations; },
    messages: () => rows().map((row) => row.children[2].textContent),
    select(view) { nodes["log-tabs"].emit("click", { target: tabs.get(view) }); },
    async resolveData(data) {
      requests.at(-1).resolve({ ok: true, status: 200, json: async () => data });
      await settle();
    },
    resolve(lines, timestamp) { return this.resolveData({ lines, updated_at: timestamp }); },
    tick() {
      const [id, timer] = [...timers].find(([, timer]) => timer.delay === 1000) || [];
      assert.ok(timer, "one live poll timer should remain");
      timers.delete(id);
      timer.callback();
      return requests.at(-1);
    },
    async poll(lines, timestamp) { this.tick(); await this.resolve(lines, timestamp); },
    async pollData(data) { this.tick(); await this.resolveData(data); },
  };
}

const line = (index) => `2026-09-30T12:00:00Z INFO Message ${index}`;

test("tail overlap handles repeated lines and finds the longest retained suffix", () => {
  assert.equal(logTailOverlap([], []), 0);
  assert.equal(logTailOverlap(["old"], ["new"]), 0);
  assert.equal(logTailOverlap(["a", "b", "c"], ["b", "c", "d"]), 2);
  assert.equal(logTailOverlap(["a", "b", "a", "b", "a"], ["a", "b", "a", "c"]), 3);
  assert.equal(logTailOverlap(["same", "same", "same"], ["same", "same", "same"]), 3);
  assert.equal(logTailOverlap(["a", "b", "a", "b"], ["a", "b", "a", "b", "c"]), 4);
  assert.equal(logTailOverlap(["a", "b"], ["b"]), 1);
});

test("finding overlap stays linear when long repeated tails do not overlap", () => {
  const size = 10000;
  let reads = 0;
  function countReads(array) {
    return new Proxy(array, {
      get(target, property, receiver) {
        if (typeof property === "string" && /^\d+$/.test(property)) {
          reads++;
          assert.ok(reads <= size * 20, "repeated lines must not cause a quadratic overlap scan");
        }
        return Reflect.get(target, property, receiver);
      },
    });
  }
  const previous = countReads([...Array(size).fill("repeat"), "old end"]);
  const next = countReads([...Array(size).fill("repeat"), "new end"]);
  assert.equal(logTailOverlap(previous, next), 0);
  assert.ok(reads < size * 20);
});

test("unchanged log tails retain their rows without new DOM work despite changing timestamps", async () => {
  const subject = harness();
  const lines = Array.from({ length: 10000 }, (_, index) => line(index));
  await subject.resolve(lines, 1);
  const initialRows = subject.rows();
  const initialCreations = subject.creations;
  const initialMutations = subject.nodes["log-view"].mutations;
  const initialLayoutReads = subject.nodes["log-view"].layoutReads;
  for (let poll = 2; poll <= 30; poll++) await subject.poll([...lines], poll);
  assert.equal(subject.creations, initialCreations, "unchanged text must not allocate another set of rows");
  assert.equal(subject.nodes["log-view"].mutations, initialMutations, "timestamp changes alone must not rebuild the log DOM");
  assert.equal(subject.nodes["log-view"].layoutReads, initialLayoutReads, "unchanged text does not need another scroll layout read");
  assert.deepEqual(subject.rows(), initialRows);
  assert.equal(subject.timers.size, 1);
});

for (const limit of [1000, 10000]) test(`a long-running ${limit}-row log tail reuses retained rows and bounds allocations and timers`, async () => {
  const subject = harness({ limit });
  let lines = Array.from({ length: limit }, (_, index) => line(index));
  await subject.resolve(lines, 1);
  const retainedRow = subject.rows()[500];
  const initialCreations = subject.creations;
  const initialMutations = subject.nodes["log-view"].mutations;
  const initialLayoutReads = subject.nodes["log-view"].layoutReads;
  const polls = 100;
  for (let poll = 1; poll <= polls; poll++) {
    lines = [...lines.slice(1), line(limit + poll - 1)];
    await subject.poll(lines, poll + 1);
    assert.equal(subject.rows().length, limit);
    assert.equal(subject.timers.size, 1, "elapsed polls must not multiply timer chains");
  }
  assert.ok(subject.creations - initialCreations <= polls * 4, "only incoming rows may allocate new DOM nodes");
  assert.ok(subject.nodes["log-view"].mutations - initialMutations <= polls * 4, "one-line churn must not detach and reinsert the entire tail");
  assert.ok(subject.nodes["log-view"].layoutReads - initialLayoutReads <= polls * 2, "scroll work stays constant per changed response");
  assert.equal(subject.rows()[400], retainedRow);
  assert.equal(subject.messages()[0], "Message 100");
  assert.equal(subject.messages().at(-1), `Message ${limit + polls - 1}`);
  assert.equal(subject.nodes["log-view"].children.length, limit);
  subject.dispose();
  assert.equal(subject.timers.size, 0);
  assert.equal(subject.document.listeners.get("visibilitychange").size, 0);
  assert.equal(subject.window.listeners.get("pageshow").size, 0);
});

test("duplicate log lines remain separate rows and changed tails preserve exact ordering", async () => {
  const subject = harness({ limit: 10 });
  await subject.resolve([line(1), line(1), line(2)], 1);
  assert.equal(subject.rows().length, 3);
  assert.notEqual(subject.rows()[0], subject.rows()[1]);
  await subject.poll([line(1), line(2), line(1), line(3)], 2);
  assert.deepEqual(subject.messages(), ["Message 1", "Message 2", "Message 1", "Message 3"]);
  assert.equal(new Set(subject.rows()).size, 4);
  await subject.poll(["continuation <script>literal</script>"], 3);
  assert.deepEqual(subject.messages(), ["continuation <script>literal</script>"]);
  await subject.poll([], 4);
  assert.equal(subject.rows().length, 0);
  assert.equal(subject.nodes["log-empty"].classList.contains("hidden"), false);
});

test("oversized server responses cannot grow the rendered tail beyond its configured limit", async () => {
  const subject = harness({ limit: 1000 });
  await subject.resolve(Array.from({ length: 1200 }, (_, index) => line(index)), 1);
  assert.equal(subject.rows().length, 1000);
  assert.equal(subject.messages()[0], "Message 200");
  assert.equal(subject.messages().at(-1), "Message 1199");
  await subject.poll(Array.from({ length: 1200 }, (_, index) => line(index + 10)), 2);
  assert.equal(subject.rows().length, 1000);
  assert.equal(subject.messages()[0], "Message 210");
  assert.equal(subject.messages().at(-1), "Message 1209");
});

test("disabled Follow retains reading position and existing rows as logs grow", async () => {
  const subject = harness({ limit: 1000, following: false });
  const lines = Array.from({ length: 100 }, (_, index) => line(index));
  await subject.resolve(lines, 1);
  subject.nodes["log-view"].scrollTop = 200;
  const readingRow = subject.rows()[8];
  await subject.poll([...lines, line(100)], 2);
  assert.equal(subject.nodes["log-view"].scrollTop, 200);
  assert.equal(subject.rows()[8], readingRow);
  assert.equal(subject.rows().length, 101);
});

test("changing log filters removes the previous tail and retains only one polling chain", async () => {
  const subject = harness({ limit: 1000 });
  await subject.resolve(Array.from({ length: 1000 }, (_, index) => line(index)), 1);
  const previousRows = subject.rows();
  subject.select("errors");
  assert.ok(subject.requests.at(-1).url.endsWith("view=errors"));
  await subject.resolve(["2026-09-30T12:00:00Z ERROR Only error"], 1);
  assert.deepEqual(subject.messages(), ["Only error"]);
  assert.ok(previousRows.every((row) => row.parentElement === null));
  assert.equal(subject.nodes["log-view"].children.length, 1);
  assert.equal(subject.timers.size, 1);
  subject.select("all");
  await subject.resolve([line(1000)], 1);
  assert.deepEqual(subject.messages(), ["Message 1000"]);
  assert.equal(subject.nodes["log-view"].children.length, 1);
  assert.equal(subject.timers.size, 1);
});

test("matching log revisions skip payload rendering and keep polling resources bounded", async () => {
  const subject = harness({ limit: 1000 });
  const revision = "r /+?#1";
  await subject.resolveData({ lines: Array.from({ length: 1000 }, (_, index) => line(index)), revision });
  const initialRows = subject.rows();
  const initialCreations = subject.creations;
  const initialMutations = subject.nodes["log-view"].mutations;
  const initialReads = subject.nodes["log-view"].layoutReads;
  for (let poll = 0; poll < 100; poll++) {
    const request = subject.tick();
    assert.ok(request.url.includes(`&revision=${encodeURIComponent(revision)}&view=all`));
    assert.equal(new URL(request.url, subject.window.location.href).searchParams.get("revision"), revision);
    await subject.resolveData({ unchanged: true, revision });
    assert.equal(subject.timers.size, 1);
  }
  assert.equal(subject.creations, initialCreations);
  assert.equal(subject.nodes["log-view"].mutations, initialMutations);
  assert.equal(subject.nodes["log-view"].layoutReads, initialReads);
  assert.deepEqual(subject.rows(), initialRows);
  assert.equal(subject.nodes["log-panel"].getAttribute("aria-busy"), "false");
  subject.dispose();
  assert.equal(subject.timers.size, 0);
  assert.equal(subject.document.listeners.get("visibilitychange").size, 0);
});

test("force refresh and filter changes fetch full log tails before adopting new revisions", async () => {
  const subject = harness();
  await subject.resolveData({ lines: [line(1)], revision: "all-r1" });
  subject.nodes["logs-refresh"].emit("click");
  const forced = new URL(subject.requests.at(-1).url, subject.window.location.href);
  assert.equal(forced.searchParams.has("revision"), false);
  await subject.resolveData({ lines: [line(1), line(2)], revision: "all-r2" });
  assert.deepEqual(subject.messages(), ["Message 1", "Message 2"]);
  const resumed = subject.tick();
  assert.ok(resumed.url.includes("revision=all-r2&view=all"));
  await subject.resolveData({ unchanged: true, revision: "all-r2" });
  subject.select("errors");
  const filtered = new URL(subject.requests.at(-1).url, subject.window.location.href);
  assert.equal(filtered.searchParams.get("view"), "errors");
  assert.equal(filtered.searchParams.has("revision"), false);
  await subject.resolveData({ lines: ["now ERROR failure"], revision: "errors-r1" });
  const next = subject.tick();
  assert.ok(next.url.includes("revision=errors-r1&view=errors"));
  await subject.resolveData({ unchanged: true, revision: "errors-r1" });
  assert.deepEqual(subject.messages(), ["failure"]);
  assert.equal(subject.timers.size, 1);
});

test("mismatched unchanged revisions preserve rows, show failure, and reset the next poll", async () => {
  const subject = harness();
  await subject.resolveData({ lines: [line(1), line(2)], revision: "r1" });
  const previousRows = subject.rows();
  const initialCreations = subject.creations;
  await subject.pollData({ unchanged: true, revision: "different" });
  assert.deepEqual(subject.rows(), previousRows);
  assert.deepEqual(subject.messages(), ["Message 1", "Message 2"]);
  assert.equal(subject.creations, initialCreations);
  const notice = subject.nodes["log-panel"].children[0];
  assert.match(notice.textContent, /previous results are still shown.*invalid log revision/);
  assert.equal(notice.classList.contains("hidden"), false);
  const retry = subject.tick();
  assert.equal(new URL(retry.url, subject.window.location.href).searchParams.has("revision"), false);
  await subject.resolveData({ lines: [line(2), line(3)], revision: "r2" });
  assert.deepEqual(subject.messages(), ["Message 2", "Message 3"]);
  assert.equal(notice.classList.contains("hidden"), true);
  assert.equal(subject.timers.size, 1);
});

test("unchanged responses without a requested revision cannot silently skip initial loading", async () => {
  const subject = harness();
  await subject.resolveData({ unchanged: true, revision: "unsolicited" });
  assert.equal(subject.rows().length, 0);
  const notice = subject.nodes["log-panel"].children[0];
  assert.match(notice.textContent, /Could not load logs.*invalid log revision/);
  assert.equal(notice.classList.contains("hidden"), false);
  const retry = subject.tick();
  assert.equal(new URL(retry.url, subject.window.location.href).searchParams.has("revision"), false);
  await subject.resolveData({ lines: [line(1)] });
  assert.deepEqual(subject.messages(), ["Message 1"]);
  assert.equal(notice.classList.contains("hidden"), true);
});
