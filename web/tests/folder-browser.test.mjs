import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { setupFolderBrowser } from "../static/js/folder-browser.js";

// Exercise requests and keyboard controls without running a browser or server.
class Element {
  constructor(tagName = "div", id = "") { this.tagName = tagName.toUpperCase(); this.id = id; }
  children = [];
  dataset = {};
  attrs = new Map();
  listeners = new Map();
  value = "";
  disabled = false;
  textContent = "";
  innerHTML = "";
  classList = { add() {}, remove() {} };
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) { for (const callback of this.listeners.get(name) || []) callback(event); }
  setAttribute(name, value) { this.attrs.set(name, value); }
  append(child) { child.parentElement = this; this.children.push(child); }
  replaceChildren() { this.children = []; }
  querySelectorAll(selector) {
    return this.children.flatMap((child) => [
      ...(selector.startsWith("#") ? child.id === selector.slice(1) : child.tagName === selector.toUpperCase()) ? [child] : [],
      ...child.querySelectorAll(selector),
    ]);
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest() { return this.tagName === "BUTTON" && this.dataset.folderPath !== undefined ? this : this.parentElement?.closest(); }
  focus() { document.activeElement = this; }
}

const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch };
afterEach(() => Object.assign(globalThis, originals));
const settle = () => new Promise((resolve) => setImmediate(resolve));

function harness() {
  const modal = new Element();
  const ids = ["add-path", "add-selection", "browser-status", "browser-list", "add-submit", "add-error"];
  const elements = Object.fromEntries(ids.map((id) => {
    const element = new Element(id.includes("path") || id.includes("selection") ? "input" : id.includes("list") ? "ul" : "button", `music-root-${id}`);
    modal.append(element);
    return [id, element];
  }));
  const controller = new AbortController();
  const removers = [];
  const scope = {
    signal: controller.signal,
    listen(element, name, callback) {
      element.addEventListener(name, callback);
      removers.push(() => element.removeEventListener(name, callback));
    },
  };
  const requests = [];
  const timers = new Map();
  let timerId = 0;
  globalThis.document = { createElement: (tag) => new Element(tag) };
  globalThis.window = {
    location: { href: "https://console.example/settings" },
    setTimeout(callback, delay) { timers.set(++timerId, { callback, delay }); return timerId; },
    clearTimeout(id) { timers.delete(id); },
  };
  globalThis.fetch = (url, options) => {
    const pending = Promise.withResolvers();
    requests.push({ url, options, ...pending });
    return pending.promise;
  };
  const picker = setupFolderBrowser(modal, scope);
  const respond = (index, body, status = 200) => requests[index].resolve({ ok: status === 200, status, json: async () => body });
  const dispose = () => { controller.abort(); removers.forEach((remove) => remove()); };
  const buttons = () => elements["browser-list"].querySelectorAll("button");
  const type = (value) => { elements["add-path"].value = value; elements["add-path"].emit("input"); };
  const pause = () => { const pending = [...timers.values()]; timers.clear(); pending.forEach(({ callback }) => callback()); };
  return { picker, modal, elements, requests, respond, dispose, buttons, timers, type, pause };
}

test("Windows starts with drives, selects folders, and can return to the drive list", async () => {
  const h = harness();
  const started = h.picker.start();
  assert.equal(h.elements["add-submit"].disabled, true);
  assert.equal(h.requests[0].options.method, "GET");
  assert.equal(h.requests[0].options.credentials, "same-origin");
  assert.equal(new URL(h.requests[0].url, window.location.href).searchParams.get("path"), "");
  h.respond(0, { path: null, parent: null, folders: [{ name: "D:\\", path: "D:\\" }] });
  await started;
  assert.equal(h.picker.canSubmit(), false);
  h.elements["browser-list"].emit("click", { target: h.buttons()[0] });
  h.respond(1, { path: "D:\\", parent: "", folders: [{ name: "Music", path: "D:\\Music" }] });
  await settle();
  assert.equal(h.elements["add-selection"].value, "D:\\");
  assert.equal(h.picker.canSubmit(), true);
  assert.equal(document.activeElement, h.buttons()[0]);
  h.type("");
  h.pause();
  assert.equal(new URL(h.requests[2].url, window.location.href).searchParams.get("path"), "");
  h.respond(2, { path: null, parent: null, folders: [] });
  await settle();
  assert.equal(h.picker.canSubmit(), false);
  h.dispose();
});

test("POSIX root and empty folders remain selectable without a parent", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  assert.equal(h.elements["add-path"].value, "/");
  assert.equal(h.elements["browser-status"].textContent, "Folder found · No subfolders");
  assert.equal(h.picker.canSubmit(), true);
  h.dispose();
});

test("typing invalidates selection; Enter opens encoded paths without submitting", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  const path = h.elements["add-path"];
  path.value = "//server/Music & more/#albums";
  path.emit("input");
  assert.equal(h.picker.canSubmit(), false);
  assert.equal(h.elements["add-selection"].value, "");
  const event = { key: "Enter", preventDefault() { this.prevented = true; } };
  path.emit("keydown", event);
  assert.equal(event.prevented, true);
  assert.equal(h.timers.size, 0, "Enter cancels the pending automatic check");
  assert.equal(new URL(h.requests[1].url, window.location.href).searchParams.get("path"), path.value);
  h.respond(1, { path: path.value, parent: "//server/Music & more", folders: [{ name: "<script> & music", path: `${path.value}/<script> & music` }] });
  await settle();
  assert.equal(h.picker.canSubmit(), true);
  const child = h.buttons().find((button) => button.dataset.folderPath.endsWith("/<script> & music"));
  assert.equal(child.children[0].textContent, "<script> & music");
  assert.equal(child.innerHTML.includes("<script>"), false);
  h.dispose();
});

test("permission errors prevent adding and a verified typed path recovers", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { error: "This folder cannot be read." }, 403);
  await started;
  assert.equal(h.elements["add-error"].textContent, "This folder cannot be read.");
  assert.equal(h.elements["add-submit"].disabled, true);
  assert.equal(h.elements["add-path"].attrs.get("aria-invalid"), "true");
  h.type("/music");
  h.pause();
  assert.equal(h.elements["add-error"].textContent, "");
  h.respond(1, { path: "/music", parent: "/", folders: [] });
  await settle();
  assert.equal(h.picker.canSubmit(), true);
  assert.equal(h.elements["add-path"].attrs.get("aria-invalid"), "false");
  h.dispose();
});

test("new navigation cancels old requests and ignores late results", async () => {
  const h = harness();
  const first = h.picker.start();
  h.type("/music");
  h.pause();
  assert.equal(h.requests[0].options.signal.aborted, true);
  h.respond(1, { path: "/music", parent: "/", folders: [] });
  await settle();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await first;
  assert.equal(h.elements["add-selection"].value, "/music");
  h.dispose();
});

test("closing or disposing cancels requests and reopening always starts at root", async () => {
  const h = harness();
  const first = h.picker.start();
  h.modal.emit("dialog:close");
  assert.equal(h.requests[0].options.signal.aborted, true);
  const reopened = h.picker.start();
  h.respond(0, { path: "/stale", parent: "/", folders: [] });
  await first;
  assert.equal(h.elements["add-selection"].value, "");
  h.dispose();
  assert.equal(h.requests[1].options.signal.aborted, true);
  h.respond(1, { path: "/", parent: null, folders: [] });
  await reopened;
  assert.equal(h.elements["add-selection"].value, "");
  assert.equal(h.elements["add-path"].listeners.get("input").size, 0);
});

test("saving locks navigation and restores selection after a failed add", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/music", parent: "/", folders: [{ name: "Albums", path: "/music/Albums" }] });
  await started;
  h.picker.setSubmitting(true);
  assert.equal(h.picker.canSubmit(), false);
  for (const id of ["add-path", "add-submit"]) assert.equal(h.elements[id].disabled, true);
  assert.equal(h.buttons()[0].disabled, true);
  h.elements["browser-list"].emit("click", { target: h.buttons()[0] });
  assert.equal(h.requests.length, 1);
  h.picker.setSubmitting(false);
  assert.equal(h.picker.canSubmit(), true);
  assert.equal(h.elements["add-selection"].value, "/music");
  h.dispose();
});

test("folders expand beneath their parents while siblings stay visible and collapse without refetching", async () => {
  const h = harness();
  const button = (path) => h.buttons().find((entry) => entry.dataset.folderPath === path);
  const click = (path) => h.elements["browser-list"].emit("click", { target: button(path) });
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [{ name: "Music", path: "/Music" }, { name: "Other", path: "/Other" }] });
  await started;
  click("/Music");
  h.respond(1, { path: "/Music", parent: "/", folders: [{ name: "Albums", path: "/Music/Albums" }] });
  await settle();
  assert.ok(button("/Other"), "opening a folder retains its siblings");
  assert.equal(button("/Music/Albums").parentElement.parentElement.parentElement, button("/Music").parentElement);
  assert.equal(button("/Music").attrs.get("aria-expanded"), "true");
  click("/Music/Albums");
  h.respond(2, { path: "/Music/Albums", parent: "/Music", folders: [{ name: "Artist", path: "/Music/Albums/Artist" }] });
  await settle();
  assert.equal(button("/Music/Albums/Artist").parentElement.parentElement.parentElement, button("/Music/Albums").parentElement);
  assert.equal(h.elements["add-selection"].value, "/Music/Albums");
  assert.equal(button("/Music/Albums").attrs.get("aria-current"), "location");
  click("/Music");
  assert.equal(button("/Music/Albums"), undefined);
  assert.equal(button("/Music").attrs.get("aria-expanded"), "false");
  assert.equal(h.elements["add-selection"].value, "/Music");
  assert.equal(document.activeElement, button("/Music"));
  click("/Music");
  assert.ok(button("/Music/Albums/Artist"));
  assert.equal(h.requests.length, 3, "previously loaded branches expand without another request");
  h.dispose();
});

test("direct Windows drive and network paths show their ancestor hierarchy", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: null, parent: null, folders: [{ name: "D:\\", path: "D:\\" }] });
  await started;
  for (const [index, path, parent] of [
    [1, "D:\\Music\\Albums", "D:\\Music"],
    [2, "\\\\nas\\share\\Music", "\\\\nas\\share\\"],
  ]) {
    h.elements["add-path"].value = path;
    h.elements["add-path"].emit("input");
    h.pause();
    h.respond(index, { path, parent, folders: [] });
    await settle();
    const selected = h.buttons().find((button) => button.dataset.folderPath === path);
    const parentButton = h.buttons().find((button) => button.dataset.folderPath === parent);
    assert.equal(selected.parentElement.parentElement.parentElement, parentButton.parentElement);
    assert.equal(h.elements["add-selection"].value, path);
  }
  h.dispose();
});

test("typing checks only the latest path after a pause and opens it without moving input focus", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  h.elements["add-path"].focus();
  h.type("/mus");
  h.type("/music");
  assert.equal(h.requests.length, 1, "typing does not send a request for every keystroke");
  assert.equal(h.timers.size, 1);
  assert.equal([...h.timers.values()][0].delay, 350);
  assert.equal(h.picker.canSubmit(), false, "the previous folder is invalidated immediately");
  h.pause();
  assert.equal(new URL(h.requests[1].url, window.location.href).searchParams.get("path"), "/music");
  h.respond(1, { path: "/music", parent: "/", folders: [{ name: "Albums", path: "/music/Albums" }] });
  await settle();
  assert.equal(h.picker.canSubmit(), true);
  assert.equal(h.elements["add-selection"].value, "/music");
  assert.ok(h.buttons().some((button) => button.dataset.folderPath === "/music/Albums"));
  assert.equal(document.activeElement, h.elements["add-path"]);
  assert.match(h.elements["browser-status"].textContent, /^Folder found/);
  h.dispose();
});

test("an invalid typed path stays unselectable and editing cancels in-flight validation immediately", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  h.type("/music");
  h.pause();
  h.type("/missing");
  assert.equal(h.requests[1].options.signal.aborted, true);
  h.respond(1, { path: "/music", parent: "/", folders: [] });
  await settle();
  assert.equal(h.elements["add-selection"].value, "");
  h.pause();
  h.respond(2, { error: "This folder could not be found." }, 404);
  await settle();
  assert.equal(h.picker.canSubmit(), false);
  assert.equal(h.elements["add-submit"].disabled, true);
  assert.equal(h.elements["add-path"].attrs.get("aria-invalid"), "true");
  assert.equal(h.elements["add-error"].textContent, "This folder could not be found.");
  h.dispose();
});

test("closing or disposing cancels scheduled checks before any request is sent", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  h.type("/music");
  h.modal.emit("dialog:close");
  assert.equal(h.timers.size, 0);
  h.pause();
  assert.equal(h.requests.length, 1);
  const reopened = h.picker.start();
  h.respond(1, { path: "/", parent: null, folders: [] });
  await reopened;
  h.type("/music");
  h.dispose();
  assert.equal(h.timers.size, 0);
  h.pause();
  assert.equal(h.requests.length, 2);
});

test("input composition finishes before automatic path verification", async () => {
  const h = harness();
  const started = h.picker.start();
  h.respond(0, { path: "/", parent: null, folders: [] });
  await started;
  h.elements["add-path"].value = "/音楽";
  h.elements["add-path"].emit("input", { isComposing: true });
  h.elements["add-path"].emit("keydown", { key: "Enter", isComposing: true, preventDefault() { assert.fail("composition Enter should keep its native behavior"); } });
  h.pause();
  assert.equal(h.requests.length, 1);
  h.elements["add-path"].emit("compositionend");
  h.pause();
  assert.equal(new URL(h.requests[1].url, window.location.href).searchParams.get("path"), "/音楽");
  h.respond(1, { path: "/音楽", parent: "/", folders: [] });
  await settle();
  assert.equal(h.picker.canSubmit(), true);
  h.dispose();
});
