import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { setupUsers } from "../static/js/users.js";
import { configureNavigation } from "../static/js/shared.js";

// Exercise account mutations with DOM doubles, without launching a browser or
// HTTP server. Each row has the same data attributes as the server template.
class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name, event = {}) { for (const callback of this.listeners.get(name) || []) callback(event); }
}

class Element extends Events {
  attrs = new Map();
  children = [];
  dataset = {};
  className = "";
  disabled = false;
  checked = false;
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
    for (let element = this; element; element = element.parentElement) if (element === document.body) return true;
    return false;
  }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  removeAttribute(name) { this.attrs.delete(name); }
  append(...children) { for (const child of children) { child.remove(); child.parentElement = this; this.children.push(child); } }
  insertBefore(child, target) {
    child.remove(); child.parentElement = this;
    this.children.splice(target ? this.children.indexOf(target) : this.children.length, 0, child);
  }
  remove() {
    if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
    this.parentElement = null;
  }
  matches(selector) {
    if (selector === "button[data-action]") return this.tagName === "BUTTON" && this.dataset.action !== undefined;
    if (selector.startsWith("input[name=")) return this.tagName === "INPUT" && this.name === selector.match(/name=["']?([^"'\]]+)/)?.[1];
    if (selector.startsWith("select[name=")) return this.tagName === "SELECT" && this.name === selector.match(/name=["']?([^"'\]]+)/)?.[1];
    if (selector === "button[type=submit]" || selector === 'button[type="submit"]') return this.tagName === "BUTTON" && this.type === "submit";
    if (selector === ".row-select:checked") return this.classList.contains("row-select") && this.checked;
    if (selector.startsWith(".")) return this.classList.contains(selector.slice(1));
    return this.tagName === selector.toUpperCase();
  }
  querySelectorAll(selector) { return this.children.flatMap((child) => [...(child.matches(selector) ? [child] : []), ...child.querySelectorAll(selector)]); }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest(selector) { for (let element = this; element; element = element.parentElement) if (element.matches(selector)) return element; return null; }
  focus() { document.activeElement = this; }
  set innerHTML(html) {
    this.children.forEach((child) => { child.parentElement = null; });
    this.children = [];
    for (const markup of html.match(/<tr\b[^>]*>[\s\S]*?<\/tr>/g) || []) {
      const row = new Element("tr");
      row.outerHTML = markup;
      row.className = "user-row";
      for (const [name, key] of [["user-id", "userId"], ["username", "username"], ["role", "role"], ["protected", "protected"]]) {
        row.dataset[key] = markup.match(new RegExp(`data-${name}="([^"]*)"`))?.[1];
      }
      const checkbox = new Element("input");
      checkbox.className = "row-select";
      checkbox.disabled = markup.includes('data-protected="1"');
      const edit = new Element("button"); edit.dataset.action = "edit";
      const remove = new Element("button"); remove.dataset.action = "delete";
      const revoke = new Element("button"); revoke.dataset.action = "revoke-sessions";
      const sessions = new Element("td"); sessions.className = "user-sessions";
      row.append(checkbox, edit, remove, revoke, sessions);
      this.append(row);
    }
  }
}

const rowHtml = (id, username, role = "user", protectedUser = false) => `<tr class="user-row" data-user-id="${id}" data-username="${username}" data-role="${role}" data-protected="${protectedUser ? "1" : "0"}"><td>${username}</td></tr>`;
const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch, FormData: globalThis.FormData };
const cleanups = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});
const settle = () => new Promise((resolve) => setImmediate(resolve));
const response = (body, ok = true, status = 200) => ({ ok, status, json: async () => body });

function harness() {
  const card = new Element(); card.dataset.currentUserId = "admin";
  const rows = new Element("tbody");
  const scroll = new Element(); scroll.className = "table-scroll";
  scroll.scrollTop = 145; scroll.scrollLeft = 24;
  scroll.append(rows); card.append(scroll);
  const bulk = new Element(); bulk.className = "hidden";
  const count = new Element();
  const message = new Element();
  const toggle = new Element("button");
  const bulkDelete = new Element("button");
  const confirm = new Element("button");
  const confirmSessions = new Element("button");
  const sessionsMessage = new Element();
  const sessionsError = new Element();
  function form() {
    const result = new Element("form");
    for (const [name, value, tag] of [["user_id", "one", "input"], ["username", "Alice", "input"], ["password", "", "input"], ["role", "user", "select"]]) {
      const field = new Element(tag); Object.assign(field, { name, value }); result.append(field);
    }
    const button = new Element("button"); button.type = "submit"; result.append(button);
    return result;
  }
  const addForm = form(); const editForm = form();
  const addError = new Element(); const editError = new Element(); const deleteError = new Element();
  const nodes = {
    "users-card": card, "user-rows": rows, "toggle-edit": toggle, "bulk-bar": bulk,
    "bulk-count": count, "bulk-delete": bulkDelete, "users-inline-message": message,
    "form-add-user": addForm, "form-edit-user": editForm, "confirm-delete": confirm,
    "confirm-sessions": confirmSessions, "sessions-message": sessionsMessage, "sessions-error": sessionsError,
    "delete-message": new Element(), "add-user-error": addError, "edit-user-error": editError, "delete-user-error": deleteError,
  };
  const body = new Element("body"); body.append(card, toggle, bulk, count, message, addForm, editForm, addError, editError, deleteError);
  const requests = [];
  const timers = new Map();
  let timerId = 0;
  const document = Object.assign(new Events(), { body, activeElement: body, getElementById: (id) => nodes[id], createElement: (tag) => new Element(tag) });
  Object.assign(globalThis, {
    document,
    window: Object.assign(new Events(), {
      location: { href: "https://console.example/users" },
      setTimeout(callback) { timers.set(++timerId, callback); return timerId; },
      clearTimeout(id) { timers.delete(id); },
    }),
    FormData: class { constructor(source) { this.values = source.children.filter((child) => child.name && !child.disabled).map((child) => [child.name, child.value]); } *[Symbol.iterator]() { yield* this.values; } },
    fetch(url, options) { const deferred = Promise.withResolvers(); requests.push({ url, options, ...deferred }); return deferred.promise; },
  });
  configureNavigation({ refresh: () => assert.fail("account updates must not refresh the page") });
  rows.innerHTML = rowHtml("one", "Alice") + rowHtml("two", "Bob") + rowHtml("admin", "Administrator", "superadmin", true);
  const dispose = setupUsers(); cleanups.push(dispose);
  return {
    card, rows, scroll, toggle, bulk, count, message, addForm, editForm, requests, confirm, confirmSessions, sessionsMessage, sessionsError, bulkDelete, editError, dispose, timers,
    poll() { const [id, callback] = timers.entries().next().value; timers.delete(id); callback(); },
    submit(target = editForm) { target.emit("submit", { preventDefault() {}, submitter: target.querySelector("button[type=submit]") }); },
    select(...ids) { rows.children.forEach((row) => { row.querySelector(".row-select").checked = ids.includes(row.dataset.userId); }); card.emit("change", { target: rows.children[0].querySelector(".row-select") }); },
  };
}

test("account edits update changed rows and retain edit mode, selections, scroll, and unchanged nodes", async () => {
  const subject = harness();
  subject.toggle.emit("click"); subject.select("one", "two");
  const oldRows = [...subject.rows.children];
  subject.submit();
  subject.requests[0].resolve(response({ status: "ok", html: rowHtml("two", "Bob") + rowHtml("one", "Alice renamed", "admin") + rowHtml("admin", "Administrator", "superadmin", true) }));
  await settle();
  assert.equal(subject.rows.children[0], oldRows[1], "unchanged accounts keep their DOM nodes");
  assert.equal(subject.rows.children[2], oldRows[2]);
  assert.notEqual(subject.rows.children[1], oldRows[0]);
  assert.equal(subject.rows.children[1].dataset.role, "admin");
  assert.equal(subject.card.classList.contains("editing"), true);
  assert.equal(subject.toggle.getAttribute("aria-pressed"), "true");
  assert.equal(subject.toggle.getAttribute("aria-label"), "Exit edit mode");
  assert.equal(subject.toggle.getAttribute("title"), "Exit edit mode");
  assert.equal(subject.toggle.textContent, "", "toggling edit mode keeps the control icon-only");
  assert.equal(subject.count.textContent, "2 selected");
  assert.equal(subject.rows.children[2].querySelector(".row-select").disabled, true);
  assert.equal(subject.scroll.scrollTop, 145);
  assert.equal(subject.scroll.scrollLeft, 24);
  assert.equal(subject.message.textContent, "Users updated.");
  assert.equal(subject.editForm.querySelector("button[type=submit]").disabled, false);
  subject.toggle.emit("click");
  assert.equal(subject.toggle.getAttribute("aria-pressed"), "false");
  assert.equal(subject.toggle.getAttribute("aria-label"), "Edit mode");
  assert.equal(subject.toggle.getAttribute("title"), "Edit mode");
  assert.equal(subject.toggle.textContent, "");
});

test("bulk deletion removes deleted accounts while retaining selections made during the request", async () => {
  const subject = harness();
  subject.rows.innerHTML = rowHtml("one", "Alice") + rowHtml("two", "Bob") + rowHtml("three", "Charlie") + rowHtml("admin", "Administrator", "superadmin", true);
  subject.toggle.emit("click"); subject.select("one", "two");
  subject.bulkDelete.emit("click"); subject.confirm.emit("click");
  assert.equal(subject.requests[0].url, "/users/bulk-delete");
  assert.equal(subject.requests[0].options.body.get("user_ids"), "one,two");
  subject.select("three");
  subject.requests[0].resolve(response({ html: rowHtml("three", "Charlie") + rowHtml("admin", "Administrator", "superadmin", true) }));
  await settle();
  assert.deepEqual(subject.rows.children.map((row) => row.dataset.userId), ["three", "admin"]);
  assert.equal(subject.count.textContent, "1 selected");
  assert.equal(subject.rows.children[0].querySelector(".row-select").checked, true);
  assert.equal(subject.card.classList.contains("editing"), true);
});

test("overlapping mutations are serialized and failures retain the current account rows", async () => {
  const subject = harness(); const originalRows = [...subject.rows.children];
  subject.submit(); subject.submit(subject.addForm);
  assert.equal(subject.requests.length, 1);
  subject.requests[0].resolve(response({ error: "superadmin can only edit its own account" }, false, 403));
  await settle();
  assert.deepEqual(subject.rows.children, originalRows);
  assert.equal(subject.editError.textContent, "superadmin can only edit its own account");
  subject.submit(); assert.equal(subject.requests.length, 2, "errors can be retried");
  subject.requests[1].resolve(response({ html: "unexpected text" }));
  await settle();
  assert.deepEqual(subject.rows.children, originalRows);
  assert.equal(subject.editError.textContent, "The server returned an invalid user list.");
});

test("disposing an in-flight account mutation aborts it and prevents stale row changes", async () => {
  const subject = harness(); const originalRows = [...subject.rows.children];
  subject.submit(); subject.dispose();
  assert.equal(subject.requests[0].options.signal.aborted, true);
  subject.requests[0].resolve(response({ html: rowHtml("one", "Late result") }));
  await settle();
  assert.deepEqual(subject.rows.children, originalRows);
  assert.equal(subject.card.listeners.get("click").size, 0);
  assert.equal(subject.editForm.listeners.get("submit").size, 0);
});

test("live session polling only updates counts and disposes pending requests", async () => {
  const subject = harness();
  subject.toggle.emit("click"); subject.select("one");
  const originalRows = [...subject.rows.children];
  subject.poll();
  assert.equal(subject.requests[0].url, "/users/activity");
  subject.requests[0].resolve(response({ sessions: { one: 2, two: 0, admin: 1 } }));
  await settle();
  assert.deepEqual(subject.rows.children, originalRows);
  assert.equal(subject.rows.children[0].querySelector(".user-sessions").textContent, "2");
  assert.equal(subject.rows.children[1].querySelector(".user-sessions").textContent, "0");
  assert.equal(subject.rows.children[0].querySelector(".row-select").checked, true);
  assert.equal(subject.scroll.scrollTop, 145);
  subject.poll();
  subject.dispose();
  assert.equal(subject.requests[1].options.signal.aborted, true);
  subject.requests[1].resolve(response({ sessions: { one: 9 } }));
  await settle();
  assert.equal(subject.rows.children[0].querySelector(".user-sessions").textContent, "2");
  assert.equal(subject.timers.size, 0);
});


test("sign out all app sessions names the selected account and only submits after confirmation", async () => {
  const subject = harness();
  const row = subject.rows.children[0];
  const button = row.children.find((element) => element.dataset.action === "revoke-sessions");
  subject.card.emit("click", { target: button });
  assert.equal(subject.requests.length, 0);
  assert.match(subject.sessionsMessage.textContent, /Alice/);
  assert.match(subject.sessionsMessage.textContent, /Admin console sessions will stay signed in/);
  subject.confirmSessions.emit("click"); subject.confirmSessions.emit("click");
  assert.equal(subject.requests.length, 1);
  assert.equal(subject.requests[0].url, "/users/one/sessions/revoke-all");
  subject.requests[0].resolve(response({ error: "Permission changed" }, false, 403));
  await settle();
  assert.equal(subject.sessionsError.textContent, "Permission changed");
  assert.equal(subject.rows.children[0], row);
  subject.confirmSessions.emit("click");
  assert.equal(subject.requests.length, 2);
  subject.requests[1].resolve(response({ html: rowHtml("one", "Alice") + rowHtml("two", "Bob") + rowHtml("admin", "Administrator", "superadmin", true) }));
  await settle();
  assert.equal(subject.rows.children[0], row);
});

test("signing out the current administrator's apps preserves the console in the confirmation and response", async () => {
  const subject = harness();
  const row = subject.rows.children.find((element) => element.dataset.userId === "admin");
  const button = row.children.find((element) => element.dataset.action === "revoke-sessions");
  subject.card.emit("click", { target: button });
  assert.equal(subject.sessionsMessage.textContent, "Sign out all app sessions for Administrator? Their apps will need to sign in again. Admin console sessions will stay signed in.");
  assert.equal(subject.requests.length, 0);
  subject.confirmSessions.emit("click");
  assert.equal(subject.requests[0].url, "/users/admin/sessions/revoke-all");
  subject.requests[0].resolve(response({ html: rowHtml("one", "Alice") + rowHtml("two", "Bob") + rowHtml("admin", "Administrator", "superadmin", true) }));
  await settle();
  assert.equal(subject.rows.children[2], row);
  assert.equal(subject.sessionsError.textContent, "");
  assert.equal(subject.message.textContent, "Users updated.");
});
