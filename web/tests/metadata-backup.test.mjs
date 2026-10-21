import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { setupMetadataBackup } from "../static/js/metadata-backup.js";

const original = { document: globalThis.document, fetch: globalThis.fetch, window: globalThis.window };
const cleanups = [];
afterEach(() => { cleanups.splice(0).forEach((dispose) => dispose()); Object.assign(globalThis, original); });
async function settle() { for (let i = 0; i < 25; i++) await Promise.resolve(); }
const response = (data, status = 200) => ({ ok: status < 400, status, json: async () => data });
const report = { matched_artists: 3, matched_albums: 4, unmatched_artists: 1, unmatched_albums: 2,
  ambiguous_albums: 1, library_albums_without_backup: 5, warnings: [] };
function job(phase, extra = {}) {
  return { id: "restore-123", kind: "restore", phase, message: phase, received_parts: 1, total_parts: 1,
    started_downloads: 0, parts: [], ...(["checked", "complete"].includes(phase) ? { report } : {}), ...extra };
}
function exportJob(phase, started = 0) {
  return job(phase, { id: "export-123", kind: "export", total_parts: 2, started_downloads: started,
    parts: phase === "preparing" ? [] : [0, 1].map((index) => ({ index, name: `part-${index}.phonolite`, url: `/settings/backup/jobs/export-123/parts/${index}` })) });
}

class Element {
  disabled = false; textContent = ""; files = []; value = ""; dataset = {}; children = []; listeners = new Map();
  clicks = 0;
  click() { this.clicks++; }
  setAttribute() {}
  classList = { add() {}, remove() {} };
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
  async emit(type, event) { return this.listeners.get(type)?.(event); }
  remove() {}
}
function harness({ initialJobs = [], responder, limit } = {}) {
  const elements = new Map();
  for (const name of ["controls", "file", "upload", "download", "report", "error", "jobs"]) {
    elements.set(`metadata-backup-${name}`, new Element());
  }
  if (limit) elements.get("metadata-backup-controls").dataset = { maxUploadBytes: String(limit.bytes), uploadLimit: limit.label };
  const controller = new AbortController();
  const events = [];
  const downloads = [];
  const calls = [];
  const busy = new Set();
  globalThis.document = {
    getElementById: (id) => elements.get(id), dispatchEvent: (event) => events.push(event),
    body: { append() {} },
    createElement: () => Object.assign(new Element(), { click() { downloads.push(this.href); } }),
  };
  globalThis.window = { location: { href: "https://localhost:3002/settings" } };
  globalThis.fetch = async (url, options) => {
    calls.push({ url, options });
    if (url === "/settings/backup/jobs") return response({ jobs: initialJobs });
    if (!responder) throw new Error(`Unexpected request: ${url}`);
    return responder(url, options);
  };
  setupMetadataBackup({
    signal: controller.signal,
    listen(element, type, callback) { element?.listeners.set(type, callback); },
    later(callback) { queueMicrotask(() => { if (!controller.signal.aborted) callback(); }); },
    async run(element, _label, task, onError) {
      busy.add(element);
      try { await task(); } catch (error) { if (!controller.signal.aborted) onError(error); }
      finally { busy.delete(element); }
    },
  });
  const h = {
    get: (name) => elements.get(`metadata-backup-${name}`), events, downloads, calls, busy,
    ready: settle(),
    async choose(...files) { h.get("file").files = files; await h.get("file").emit("change"); },
    dispose() { controller.abort(); },
  };
  cleanups.push(h.dispose);
  return h;
}

test("one upload button opens the picker and selecting multiple files uploads and restores automatically", async () => {
  let uploaded = 0;
  const h = harness({ responder: async (url) => {
    if (url === "/settings/backup/restore") return response(++uploaded === 1 ? job("waiting", { total_parts: 2 }) : job("restoring", { received_parts: 2, total_parts: 2 }));
    return response(job("complete"));
  } });
  await h.ready;
  await h.get("upload").emit("click");
  assert.equal(h.get("file").clicks, 1);
  assert.equal(h.calls.length, 1, "opening the picker does not send an upload");
  const files = [{ name: "part-2.phonolite", size: 128 }, { name: "part-1.phonolite", size: 128 }];
  await h.choose(...files);
  assert.equal(h.get("upload").disabled, false);
  assert.match(h.get("report").textContent, /4 albums/);
  assert.match(h.get("report").textContent, /2 albums have no library match/);
  const uploads = h.calls.filter((call) => call.options.method === "POST");
  assert.deepEqual(uploads.map((call) => call.options.body), files);
  assert.ok(uploads.every((call) => call.url === "/settings/backup/restore"));
  assert.equal(uploads[0].options.credentials, "same-origin");
  assert.equal(uploads[0].options.headers["Content-Type"], "application/vnd.phonolite.metadata");
  assert.match(h.get("report").textContent, /^Restored missing fields/);
  assert.ok(h.events.some((event) => event.type === "library:backup-updated" && event.detail.phase === "restoring"));
});

test("parts can be selected in separate visits and incomplete sets cannot restore", async () => {
  let uploaded = 0;
  const h = harness({ initialJobs: [job("waiting", { received_parts: 1, total_parts: 3, message: "Backup received: 1/3 parts" })],
    responder: async (url) => response(url === "/settings/backup/restore"
      ? ++uploaded === 1 ? job("waiting", { received_parts: 2, total_parts: 3, message: "Backup received: 2/3 parts" }) : job("restoring")
      : job("complete")) });
  await h.ready;
  await h.choose({ name: "part-3.phonolite", size: 128 });
  assert.match(h.get("report").textContent, /2\/3 parts/);
  assert.ok(!h.events.some((event) => event.detail?.phase === "restoring"));
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
  await h.choose({ name: "part-1.phonolite", size: 128 });
  assert.match(h.get("report").textContent, /^Restored missing fields/);
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 2);
});

test("an oversized individual file is rejected locally but the combined selection can exceed the per-file limit", async () => {
  const h = harness({ responder: async () => response(job("waiting")) });
  await h.ready;
  await h.choose({ name: "one.phonolite", size: 10 * 1024 ** 3 }, { name: "two.phonolite", size: 10 * 1024 ** 3 });
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 2);
  await h.choose({ name: "too-large.phonolite", size: 16 * 1024 ** 3 + 1 });
  assert.equal(h.get("upload").disabled, false);
  assert.equal(h.calls.length, 3, "the oversized file is never sent");
  assert.match(h.get("error").textContent, /16 GiB/);
});

test("the per-file upload limit follows server configuration", async () => {
  const h = harness({ limit: { bytes: 32 * 1024 ** 3, label: "32 GiB" }, responder: async () => response(job("waiting")) });
  await h.ready;
  await h.choose({ name: "larger.phonolite", size: 20 * 1024 ** 3 });
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
  await h.choose({ name: "too-large.phonolite", size: 33 * 1024 ** 3 });
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
  assert.match(h.get("error").textContent, /32 GiB/);
});

test("download spinner lasts through preparation and until a browser download reaches the server", async () => {
  const prepared = Promise.withResolvers();
  const started = Promise.withResolvers();
  let polled = 0;
  const h = harness({ responder: async (url, options) => {
    if (url.endsWith("/export")) { assert.equal(options.method, "POST"); return response(exportJob("preparing")); }
    return ++polled === 1 ? prepared.promise : started.promise;
  } });
  await h.ready;
  const pending = h.get("download").emit("click");
  await settle();
  assert.equal(h.busy.has(h.get("download")), true);
  assert.deepEqual(h.downloads, []);
  prepared.resolve(response(exportJob("ready")));
  await settle();
  assert.deepEqual(h.downloads, ["/settings/backup/jobs/export-123/parts/0", "/settings/backup/jobs/export-123/parts/1"]);
  assert.equal(h.busy.has(h.get("download")), true, "creating anchors alone does not end the spinner");
  started.resolve(response(exportJob("ready", 1)));
  await pending;
  assert.equal(h.busy.size, 0);
  assert.match(h.get("report").textContent, /multiple downloads/);
  assert.equal(h.get("jobs").children[0].children[1].children.filter((element) => element.href).length, 2);
});

test("blocked automatic downloads leave individual save links and end the bounded wait", async () => {
  const h = harness({ responder: async () => response(exportJob("ready")) });
  await h.ready;
  await h.get("download").emit("click");
  assert.equal(h.downloads.length, 2);
  assert.equal(h.busy.size, 0);
  assert.match(h.get("report").textContent, /blocked automatic downloads/);
  assert.equal(h.get("jobs").children[0].children[1].children.filter((element) => element.href).length, 2);
});

test("opening Settings does not apply previously checked backups without a new upload", async () => {
  const h = harness({ initialJobs: [job("checked")] });
  await h.ready;
  assert.equal(h.get("upload").disabled, false);
  await h.get("upload").emit("click");
  await h.choose();
  assert.equal(h.calls.length, 1, "opening or cancelling the picker does not restore staged backups");
});

test("server validation errors show inline and leave the upload button ready to retry", async () => {
  const h = harness({ responder: async () => response({ error: "Backup part integrity check failed" }, 400) });
  await h.ready;
  await h.choose({ name: "damaged.phonolite", size: 128 });
  assert.equal(h.get("upload").disabled, false);
  assert.equal(h.get("file").value, "");
  assert.match(h.get("error").textContent, /integrity/);
});

test("a failed later file retries without resending previously staged parts", async () => {
  let calls = 0;
  const h = harness({ responder: async () => ++calls === 2 ? response({ error: "Upload interrupted" }, 400) : response(job(calls === 1 ? "waiting" : "complete")) });
  await h.ready;
  const first = { name: "one.phonolite", size: 128 };
  const second = { name: "two.phonolite", size: 128 };
  await h.choose(first, second);
  assert.match(h.get("error").textContent, /interrupted/);
  await h.choose(second);
  assert.deepEqual(h.calls.filter((call) => call.options.body).map((call) => call.options.body), [first, second, second]);
  assert.match(h.get("report").textContent, /^Restored missing fields/);
});

test("leaving Settings aborts an in-flight upload and ignores its late result", async () => {
  const deferred = Promise.withResolvers();
  const h = harness({ responder: () => deferred.promise });
  await h.ready;
  const pending = h.choose({ name: "backup.phonolite", size: 128 });
  const before = h.get("report").textContent;
  h.dispose();
  assert.equal(h.calls.at(-1).options.signal.aborted, true);
  deferred.resolve(response(job("complete")));
  await pending;
  assert.equal(h.get("report").textContent, before);
  assert.deepEqual(h.events, []);
});

test("leaving Settings during server processing ends polling without cancelling the accepted job", async () => {
  const deferred = Promise.withResolvers();
  const h = harness({ responder: (url) => url === "/settings/backup/restore" ? response(job("restoring")) : deferred.promise });
  await h.ready;
  const pending = h.choose({ name: "backup.phonolite", size: 128 });
  await settle();
  assert.equal(h.busy.size, 1);
  const before = h.events.length;
  h.dispose();
  deferred.resolve(response(job("complete")));
  await pending;
  assert.equal(h.events.length, before);
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1, "leaving the page sends no further requests");
});

test("staged files can be removed without applying metadata", async () => {
  const h = harness({ initialJobs: [job("waiting")], responder: async (url) => {
    assert.equal(url, "/settings/backup/jobs/restore-123/remove");
    return response({ ok: true });
  } });
  await h.ready;
  const button = h.get("jobs").children[0].children[1].children[0];
  await h.get("jobs").emit("click", { target: { closest: () => button } });
  assert.equal(h.get("jobs").children.length, 0);
  assert.equal(h.get("upload").disabled, false);
});
