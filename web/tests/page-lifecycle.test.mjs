import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { setupAuth } from "../static/js/auth.js";
import { setupArtwork } from "../static/js/artwork.js";
import { configureNavigation, fetchJson } from "../static/js/shared.js";

class Events {
  listeners = new Map();
  addEventListener(name, callback, options = {}) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Map());
    this.listeners.get(name).set(callback, options);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) {
    return Promise.all([...this.listeners.get(name) || []].map(([callback, options]) => {
      if (options.once) this.removeEventListener(name, callback);
      return callback(event);
    }));
  }
}

class Element extends Events {
  attrs = new Map();
  children = [];
  dataset = {};
  className = "";
  hidden = false;
  textContent = "";
  classList = {
    contains: (name) => this.className.split(" ").includes(name),
    add: (...names) => { this.className = [...new Set([...this.className.split(" ").filter(Boolean), ...names])].join(" "); },
    remove: (...names) => { this.className = this.className.split(" ").filter((name) => !names.includes(name)).join(" "); },
  };
  constructor(tag = "div") { super(); this.tagName = tag.toUpperCase(); }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  removeAttribute(name) { this.attrs.delete(name); }
  append(...children) {
    for (const child of children) {
      child.remove();
      child.parentElement = this;
      this.children.push(child);
    }
  }
  prepend(child) { child.remove(); child.parentElement = this; this.children.unshift(child); }
  remove() {
    if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
    this.parentElement = null;
  }
  replaceWith(replacement) {
    if (!this.parentElement) return;
    replacement.remove();
    const parent = this.parentElement;
    parent.children.splice(parent.children.indexOf(this), 1, replacement);
    replacement.parentElement = parent;
    this.parentElement = null;
  }
  querySelector(selector) {
    return this.children.find((child) => selector === 'button[type="submit"]' && child.tagName === "BUTTON") || null;
  }
}

const originals = {
  window: globalThis.window,
  document: globalThis.document,
  fetch: globalThis.fetch,
  FormData: globalThis.FormData,
};
const cleanups = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});

const response = (body, { ok = true, status = 200, url = "", redirected = false } = {}) => ({
  ok, status, url, redirected, json: async () => body,
});

function authHarness(action = "/login") {
  const form = new Element("form");
  form.action = `https://console.example${action}`;
  form.entries = action === "/setup" ? [["username", "new admin"], ["password", "secret"], ["confirm", "secret"]] :
    [["username", "admin"], ["password", "secret"]];
  const button = new Element("button");
  button.disabled = false;
  button.textContent = action === "/setup" ? "Create admin" : "Sign in";
  const formError = new Element();
  formError.className = "inline-error";
  form.append(formError, button);
  const main = new Element();
  main.append(form);
  const document = {
    getElementById: (id) => id === "auth-error" ? formError : null,
    querySelector: (selector) => selector === ".auth-card form" ? form : selector === ".content" ? main : null,
    createElement: (tag) => new Element(tag),
  };
  const window = { location: { href: form.action }, setTimeout, clearTimeout };
  const requests = [];
  const navigations = [];
  Object.assign(globalThis, {
    document, window,
    FormData: class { constructor(source) { this.source = source; } [Symbol.iterator]() { return this.source.entries[Symbol.iterator](); } },
    fetch: (url, options) => {
      const pending = Promise.withResolvers();
      requests.push({ url, options, ...pending });
      return pending.promise;
    },
  });
  configureNavigation({ navigate: async (...args) => { navigations.push(args); return true; } });
  const dispose = setupAuth();
  cleanups.push(dispose);
  return {
    form, formError, button, requests, navigations, main, dispose,
    submit() {
      const event = { preventDefault() { this.defaultPrevented = true; } };
      return { event, pending: form.emit("submit", event) };
    },
  };
}

test("Login submits encoded POST once, displays a spinner, and navigates in place", async () => {
  const { submit, requests, button, navigations } = authHarness();
  const first = submit();
  const second = submit();
  assert.equal(first.event.defaultPrevented, true);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.method, "POST");
  assert.equal(requests[0].options.body.toString(), "username=admin&password=secret");
  assert.equal(button.disabled, true);
  assert.equal(button.getAttribute("aria-busy"), "true");
  assert.equal(button.children[0].children[0].className, "spinner");
  requests[0].resolve(response({ redirect: "/" }));
  await first.pending;
  await second.pending;
  assert.deepEqual(navigations, [["/", { replace: true, force: true }]]);
  assert.equal(button.disabled, false);
  assert.equal(button.getAttribute("aria-busy"), null);
  assert.equal(button.children.length, 0);
});

test("Setup preserves confirmation fields and reaches the authenticated console", async () => {
  const { submit, requests, navigations } = authHarness("/setup");
  const { pending } = submit();
  assert.equal(requests[0].options.body.toString(), "username=new+admin&password=secret&confirm=secret");
  requests[0].resolve(response({ redirect: "/" }));
  await pending;
  assert.deepEqual(navigations, [["/", { replace: true, force: true }]]);
});

test("invalid Login credentials stay on the form and show the server's error", async () => {
  const { submit, requests, navigations, main, button, form, formError } = authHarness();
  const { pending } = submit();
  requests[0].resolve(response({ error: "invalid credentials" }, {
    ok: false, status: 401, url: "https://console.example/login",
  }));
  await pending;
  assert.deepEqual(navigations, []);
  assert.equal(formError.textContent, "invalid credentials");
  assert.equal(formError.classList.contains("visible"), true);
  assert.deepEqual(main.children, [form]);
  assert.equal(main.children.at(-1), form);
  assert.equal(button.disabled, false);
  assert.equal(form.entries[0][1], "admin");
});

test("authentication errors can be retried and cleanup aborts a pending submission", async () => {
  const { submit, requests, navigations, main, button, form, formError, dispose } = authHarness("/setup");
  const first = submit();
  requests[0].resolve(response({ error: "passwords do not match" }, { ok: false, status: 400 }));
  await first.pending;
  assert.equal(formError.textContent, "passwords do not match");
  assert.deepEqual(main.children, [form]);
  assert.equal(button.disabled, false);
  const second = submit();
  assert.equal(formError.textContent, "");
  assert.equal(formError.classList.contains("visible"), false);
  assert.equal(requests.length, 2);
  dispose();
  assert.equal(requests[1].options.signal.aborted, true);
  assert.equal(form.listeners.get("submit").size, 0);
  requests[1].resolve(response({ redirect: "/" }));
  await second.pending;
  assert.deepEqual(navigations, []);
  assert.equal(button.disabled, false);
});

test("an aborted old API response cannot trigger session navigation", async () => {
  const { requests, navigations } = authHarness();
  const controller = new AbortController();
  const request = fetchJson("/status/library", undefined, { signal: controller.signal });
  controller.abort();
  requests[0].resolve(response({ redirect: "/login" }));
  await assert.rejects(request, { name: "AbortError" });
  assert.deepEqual(navigations, []);
});

function artworkHarness({ complete = false, naturalWidth = 0, className = "tile-cover artist-cover-img" } = {}) {
  const root = new Element();
  const image = new Element("img");
  image.className = className;
  image.complete = complete;
  image.naturalWidth = naturalWidth;
  root.append(image);
  root.querySelectorAll = () => [image];
  globalThis.document = { createElement: (tag) => new Element(tag) };
  const dispose = setupArtwork(root);
  cleanups.push(dispose);
  return { root, image, dispose };
}

test("pending artwork keeps frame dimensions and fades in after loading", async () => {
  const { root, image, dispose } = artworkHarness();
  const frame = root.children[0];
  assert.equal(frame.className, "tile-cover artist-cover-img artwork-frame");
  assert.equal(image.className, "artwork-image");
  assert.equal(frame.children[1].className, "spinner");
  assert.equal(frame.children[1].getAttribute("aria-label"), "Loading artwork");
  await image.emit("load");
  assert.equal(frame.children.length, 1);
  assert.equal(frame.classList.contains("artwork-ready"), true);
  dispose();
  assert.equal(root.children[0], image);
  assert.equal(image.className, "tile-cover artist-cover-img");
  assert.equal(image.listeners.get("load").size, 0);
  assert.equal(image.listeners.get("error").size, 0);
});

test("artwork errors replace the spinner with an accessible stable placeholder", async () => {
  const { root, image, dispose } = artworkHarness({ className: "cover-lg" });
  const frame = root.children[0];
  await image.emit("error");
  assert.equal(frame.className, "cover-lg artwork-frame");
  assert.equal(image.hidden, true);
  assert.equal(frame.children[1].textContent, "No artwork");
  assert.equal(frame.children[1].className, "artwork-unavailable");
  assert.equal(frame.children.some((child) => child.className === "spinner"), false);
  dispose();
  assert.equal(image.hidden, false);
  assert.equal(root.children[0], image);
});

test("cached artwork has no loading treatment while completed broken artwork shows a placeholder", () => {
  const cached = artworkHarness({ complete: true, naturalWidth: 200 });
  assert.equal(cached.root.children[0], cached.image);
  assert.equal(cached.image.className, "tile-cover artist-cover-img");
  assert.equal(cached.image.listeners.size, 0);
  const broken = artworkHarness({ complete: true, naturalWidth: 0 });
  assert.equal(broken.image.hidden, true);
  assert.equal(broken.root.children[0].children[1].textContent, "No artwork");
});

test("disposing artwork before completion removes handlers and restores the original image", async () => {
  const { root, image, dispose } = artworkHarness();
  dispose();
  await image.emit("load");
  await image.emit("error");
  assert.equal(root.children[0], image);
  assert.equal(root.children.length, 1);
  assert.equal(image.className, "tile-cover artist-cover-img");
  assert.equal(image.hidden, false);
});
