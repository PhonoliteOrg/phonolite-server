import test, { afterEach } from "node:test";
import assert from "node:assert/strict";
import { formatBytes, formatCpu, formatMemory, formatTrafficRate, formatUptime, performanceGraph, setupProcessPerformance } from "../static/js/process-performance.js";
import { configureNavigation } from "../static/js/shared.js";

const MIB = 1024 * 1024;
const sample = (timestamp_ms, cpu_percent, memory_bytes = MIB, sent_bytes_per_sec = null, received_bytes_per_sec = null) => ({
  timestamp_ms, cpu_percent, cpu_status: cpu_percent === null ? "unavailable" : "available",
  memory_bytes, sent_bytes_per_sec, received_bytes_per_sec, active_streams: 0,
});

test("graphs preserve elapsed time, break missing readings and pauses, and allow multicore CPU", () => {
  const graph = performanceGraph([
    sample(100000, 10), sample(102000, 20), sample(110000, 30),
    sample(112000, null), sample(114000, 50),
  ], "cpu_percent", { minimum: 100 });
  assert.equal(graph.path, "M572,162L576,144M592,126M600,90");
  assert.deepEqual(graph.point, { x: 600, y: 90 });
  assert.equal(graph.count, 4);
  assert.equal(graph.minimum, 10);
  assert.equal(graph.average, 27.5);
  assert.equal(graph.maximum, 50);
  const multicore = performanceGraph([sample(1000, 325)], "cpu_percent", { minimum: 100 });
  assert.equal(multicore.ceiling, 400);
  assert.deepEqual(multicore.point, { x: 600, y: 33.75 });
});

test("graphs are bounded by time and sample count and never plot invalid values as zero", () => {
  const history = Array.from({ length: 200 }, (_, index) => sample(index * 2000, index));
  assert.equal(performanceGraph(history, "cpu_percent").count, 150);
  const graph = performanceGraph([
    sample(1, 99), sample(400000, null), sample(402000, -1),
    sample(404000, Infinity), sample(406000, "4"), sample(408000, 0),
  ], "cpu_percent", { minimum: 100 });
  assert.equal(graph.count, 1);
  assert.equal(graph.path, "M600,180");
  assert.equal(performanceGraph([], "cpu_percent").point, undefined);
});

test("averages use available values in the bounded window and the graph's display units", () => {
  const graph = performanceGraph([
    sample(1, null, 999 * MIB), sample(400000, null, null),
    sample(402000, null, -MIB), sample(404000, null, Infinity),
    sample(406000, null, 0), sample(408000, null, 2 * MIB), sample(410000, null, 10 * MIB),
  ], "memory_bytes", { divisor: MIB });
  assert.equal(graph.count, 3);
  assert.equal(graph.minimum, 0);
  assert.equal(graph.average, 4);
  assert.equal(graph.maximum, 10);

  const history = Array.from({ length: 200 }, (_, index) => sample(index * 2000, index));
  const bounded = performanceGraph(history, "cpu_percent");
  assert.equal(bounded.count, 150);
  assert.ok(Math.abs(bounded.average - 124.5) < 1e-10);
  assert.equal(performanceGraph([], "cpu_percent").average, null);
  assert.equal(performanceGraph([sample(1000, null)], "cpu_percent").average, null);
  assert.equal(performanceGraph([sample(1000, 0)], "cpu_percent").average, 0);
});

test("memory axes use MiB and readable ceilings and summaries keep missing data explicit", () => {
  const graph = performanceGraph([sample(1000, null, 3.5 * MIB)], "memory_bytes", { divisor: MIB });
  assert.equal(graph.ceiling, 4);
  assert.equal(graph.maximum, 3.5);
  assert.equal(formatMemory(3.5 * MIB), "3.5 MiB");
  assert.equal(formatMemory(null), "Unavailable");
  assert.equal(formatCpu(null), "Unavailable");
  assert.equal(formatCpu(250), "250%");
  assert.equal(formatUptime(90061), "1d 1h 1m 1s");
});

test("traffic units distinguish byte totals from rates and retain zero and missing readings", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1536), "1.5 KiB");
  assert.equal(formatBytes(3 * MIB), "3 MiB");
  assert.equal(formatBytes(3.5 * 1024 * MIB), "3.5 GiB");
  assert.equal(formatTrafficRate(0), "0 B/s");
  assert.equal(formatTrafficRate(1536), "1.5 KiB/s");
  assert.equal(formatTrafficRate(3 * MIB), "3 MiB/s");
  for (const invalid of [null, undefined, -1, Infinity, "1024"]) {
    assert.equal(formatBytes(invalid), "Unavailable");
    assert.equal(formatTrafficRate(invalid), "Unavailable");
  }
  const graph = performanceGraph([
    sample(2000, null, MIB, null), sample(4000, null, MIB, 0), sample(6000, null, MIB, 4096),
  ], "sent_bytes_per_sec", { divisor: 1024 });
  assert.equal(graph.ceiling, 4);
  assert.equal(graph.count, 2);
  assert.equal(graph.minimum, 0);
  assert.equal(graph.path, "M596,180L600,0");
  const idle = performanceGraph([sample(2000, null, MIB, 0)], "sent_bytes_per_sec", { divisor: 1024 });
  assert.equal(idle.ceiling, 1);
  assert.equal(idle.path, "M600,180");
  assert.ok(!idle.path.includes("NaN"));
});

class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(callback);
  }
  removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
  emit(name) { for (const callback of this.listeners.get(name) || []) callback(); }
  countListeners() { return [...this.listeners.values()].reduce((count, listeners) => count + listeners.size, 0); }
}
class Element {
  textContent = "";
  attrs = new Map();
  classes = new Set();
  classList = { add: (name) => this.classes.add(name), remove: (name) => this.classes.delete(name) };
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  toggleAttribute(name, force) { if (force) this.attrs.set(name, ""); else this.attrs.delete(name); }
}
const originals = { document: globalThis.document, window: globalThis.window, fetch: globalThis.fetch };
const cleanups = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()();
  configureNavigation(null);
  Object.assign(globalThis, originals);
});
const settle = () => new Promise((resolve) => setImmediate(resolve));
const response = (body, status = 200) => ({ ok: status === 200, status, json: async () => body });
const snapshot = (cpu = 200, memory = 80 * MIB) => ({
  pid: 42, uptime_secs: 3700, logical_cpus: 8, interval_ms: 2000,
  active_streams: 3, sent_bytes: 5 * MIB, received_bytes: MIB,
  history: [sample(10000, null, 70 * MIB), sample(12000, cpu, memory, 4 * 1024, 2 * 1024)],
});

function harness() {
  const nodes = Object.fromEntries([
    "process-performance", "performance-error", "performance-status", "process-pid", "process-uptime", "process-cpus", "process-cpu", "process-memory",
    "process-active-streams", "process-sent", "process-received",
    ...["cpu", "memory", "sent", "received"].flatMap((name) => ["line", "point", "scale", "summary", "chart"].map((part) => `${name}-${part}`)),
  ].map((id) => [id, new Element()]));
  const document = Object.assign(new Events(), { hidden: false, getElementById: (id) => nodes[id] });
  const timers = new Map();
  let nextTimer = 0;
  const window = Object.assign(new Events(), {
    location: { href: "https://console.example/performance" },
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(id) { timers.delete(id); },
  });
  const requests = [];
  Object.assign(globalThis, {
    document, window,
    fetch(url, options) {
      const deferred = Promise.withResolvers();
      requests.push({ url, options, ...deferred });
      return deferred.promise;
    },
  });
  const dispose = setupProcessPerformance();
  cleanups.push(dispose);
  return {
    nodes, document, window, requests, timers, dispose,
    poll() {
      const [id, timer] = timers.entries().next().value;
      timers.delete(id);
      assert.equal(timer.delay, 2000);
      timer.callback();
    },
  };
}

test("live readings show a first sample, retain good graphs on failure and clear stale state on recovery", async () => {
  const subject = harness();
  assert.equal(subject.requests[0].url, "/performance/data");
  assert.equal(subject.requests[0].options.method, "GET");
  subject.requests[0].resolve(response({ ...snapshot(), history: [{ ...sample(10000, null, 70 * MIB), cpu_status: "measuring" }] }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "Measuring…");
  assert.equal(subject.nodes["process-memory"].textContent, "70 MiB");
  assert.equal(subject.nodes["memory-point"].getAttribute("cx"), "600");
  assert.equal(subject.nodes["cpu-point"].getAttribute("hidden"), "");
  assert.equal(subject.nodes["process-sent"].textContent, "Measuring…");
  assert.equal(subject.nodes["process-active-streams"].textContent, "3");
  assert.equal(subject.nodes["cpu-summary"].textContent, "Min — · Avg — · Max —");
  assert.equal(subject.nodes["memory-summary"].textContent, "Min 70 MiB · Avg 70 MiB · Max 70 MiB");
  assert.equal(subject.nodes["sent-summary"].textContent, "Min — · Avg — · Max —");
  assert.equal(subject.nodes["received-summary"].textContent, "Min — · Avg — · Max —");
  const lastPath = subject.nodes["memory-line"].getAttribute("d");
  subject.poll();
  subject.requests[1].resolve(response({ error: "Server busy." }, 503));
  await settle();
  assert.equal(subject.nodes["memory-line"].getAttribute("d"), lastPath);
  assert.equal(subject.nodes["memory-summary"].textContent, "Min 70 MiB · Avg 70 MiB · Max 70 MiB");
  assert.match(subject.nodes["performance-status"].textContent, /last successful/);
  assert.match(subject.nodes["performance-error"].textContent, /Server busy.*Retrying/);
  subject.poll();
  subject.requests[2].resolve(response(snapshot()));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "25%");
  assert.equal(subject.nodes["process-memory"].textContent, "80 MiB");
  assert.equal(subject.nodes["process-pid"].textContent, "42");
  assert.equal(subject.nodes["performance-error"].textContent, "");
  assert.match(subject.nodes["cpu-chart"].getAttribute("aria-label"), /Server share of total CPU capacity.*Min 25% · Avg 25% · Max 25%/);
  assert.equal(subject.nodes["cpu-summary"].textContent, "Min 25% · Avg 25% · Max 25%");
  assert.equal(subject.nodes["cpu-scale"].textContent, "100%");
  assert.equal(subject.nodes["cpu-point"].getAttribute("cy"), "135");
  assert.equal(subject.nodes["memory-summary"].textContent, "Min 70 MiB · Avg 75 MiB · Max 80 MiB");
  assert.equal(subject.nodes["sent-summary"].textContent, "Min 4 KiB/s · Avg 4 KiB/s · Max 4 KiB/s");
  assert.equal(subject.nodes["received-summary"].textContent, "Min 2 KiB/s · Avg 2 KiB/s · Max 2 KiB/s");
  assert.equal(subject.nodes["process-sent"].textContent, "4 KiB/s");
  assert.equal(subject.nodes["process-received"].textContent, "2 KiB/s");
  assert.equal(subject.nodes["sent-scale"].textContent, "4 KiB/s");
  assert.match(subject.nodes["received-chart"].getAttribute("aria-label"), /Traffic received from clients/);
});

test("CPU capacity readings and history use total available cores", async () => {
  const subject = harness();
  subject.requests[0].resolve(response({ ...snapshot(100.2), logical_cpus: 4 }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "25.1%");
  assert.equal(subject.nodes["cpu-summary"].textContent, "Min 25.1% · Avg 25.1% · Max 25.1%");
  assert.equal(subject.nodes["cpu-point"].getAttribute("cy"), "134.91");

  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(400), logical_cpus: 4 }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "100%");
  assert.equal(subject.nodes["cpu-point"].getAttribute("cy"), "0");

  for (const logical_cpus of [0, -1, null, "4", 1.5, Infinity]) {
    subject.poll();
    subject.requests.at(-1).resolve(response({ ...snapshot(100.2), logical_cpus }));
    await settle();
    assert.equal(subject.nodes["process-cpu"].textContent, "Unavailable");
    assert.equal(subject.nodes["cpu-line"].getAttribute("d"), "");
    assert.equal(subject.nodes["cpu-summary"].textContent, "Min — · Avg — · Max —");
    assert.equal(subject.nodes["cpu-point"].getAttribute("hidden"), "");
  }

  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(null), logical_cpus: 4 }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "Unavailable");

  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(0), logical_cpus: 4 }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "0%");
  assert.equal(subject.nodes["cpu-point"].getAttribute("cy"), "180");
});

test("CPU baseline resets show measuring with retained history and recover without filling graph gaps", async () => {
  const subject = harness();
  subject.requests[0].resolve(response(snapshot()));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "25%");

  const history = [
    sample(10000, 200),
    { ...sample(20000, null), cpu_status: "measuring" },
  ];
  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(), history }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "Measuring…");
  assert.equal(subject.nodes["cpu-summary"].textContent, "Min 25% · Avg 25% · Max 25%");

  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(), history: [...history, sample(22000, 0)] }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "0%");
  assert.equal(subject.nodes["cpu-line"].getAttribute("d"), "M576,135M600,180");
  assert.equal(subject.nodes["performance-error"].textContent, "");
});

test("a failed first CPU measurement stays unavailable until a successful baseline is established", async () => {
  const subject = harness();
  subject.requests[0].resolve(response({ ...snapshot(), history: [sample(10000, null)] }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "Unavailable");

  subject.poll();
  subject.requests.at(-1).resolve(response({ ...snapshot(), history: [
    sample(10000, null), { ...sample(12000, null), cpu_status: "measuring" },
  ] }));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "Measuring…");
  assert.equal(subject.nodes["cpu-line"].getAttribute("d"), "");

  subject.poll();
  subject.requests.at(-1).resolve(response(snapshot(0)));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "0%");
});

test("traffic rates and playing-device counts refresh in place and stale replies preserve them", async () => {
  const subject = harness();
  subject.requests[0].resolve(response(snapshot()));
  await settle();
  const previous = subject.nodes["sent-line"].getAttribute("d");
  const previousSummary = subject.nodes["sent-summary"].textContent;
  subject.poll();
  subject.requests[1].resolve(response({ error: "Offline" }, 503));
  await settle();
  assert.equal(subject.nodes["sent-line"].getAttribute("d"), previous);
  assert.equal(subject.nodes["sent-summary"].textContent, previousSummary);
  assert.equal(subject.nodes["process-active-streams"].textContent, "3");
  subject.poll();
  subject.requests[2].resolve(response({ ...snapshot(), active_streams: 0, sent_bytes: 6 * MIB,
    history: [sample(20000, 0, MIB, null, null), sample(22000, 0, MIB, 0, 0)] }));
  await settle();
  assert.equal(subject.nodes["process-active-streams"].textContent, "0");
  assert.equal(subject.nodes["process-sent"].textContent, "0 B/s");
  assert.equal(subject.nodes["process-received"].textContent, "0 B/s");
  assert.equal(subject.nodes["sent-summary"].textContent, "Min 0 KiB/s · Avg 0 KiB/s · Max 0 KiB/s");
  assert.equal(subject.nodes["received-summary"].textContent, "Min 0 KiB/s · Avg 0 KiB/s · Max 0 KiB/s");
  assert.equal(subject.nodes["performance-error"].textContent, "");
});

test("a stalled performance request times out and live polling recovers automatically", async () => {
  const subject = harness();
  const pending = subject.requests[0];
  pending.options.signal.addEventListener("abort", () => {
    pending.reject(new DOMException("Aborted", "AbortError"));
  }, { once: true });
  const [id, deadline] = subject.timers.entries().next().value;
  assert.equal(deadline.delay, 5000);
  subject.timers.delete(id);
  deadline.callback();
  await settle();
  assert.equal(pending.options.signal.aborted, true);
  assert.match(subject.nodes["performance-error"].textContent, /timed out.*Retrying automatically/);

  subject.poll();
  subject.requests[1].resolve(response({ ...snapshot(), active_streams: 2,
    history: [sample(20000, 0, MIB, 0, 0)] }));
  await settle();
  assert.equal(subject.nodes["process-active-streams"].textContent, "2");
  assert.equal(subject.nodes["process-sent"].textContent, "0 B/s");
  assert.equal(subject.nodes["performance-error"].textContent, "");
});

test("hidden pages abort requests, visible pages resume without overlaps and disposal blocks late updates", async () => {
  const subject = harness();
  subject.document.hidden = true;
  subject.document.emit("visibilitychange");
  assert.equal(subject.requests[0].options.signal.aborted, true);
  subject.requests[0].resolve(response(snapshot()));
  await settle();
  assert.equal(subject.timers.size, 0);
  assert.equal(subject.nodes["process-cpu"].textContent, "");
  subject.document.hidden = false;
  subject.document.emit("visibilitychange");
  subject.document.emit("visibilitychange");
  assert.equal(subject.requests.length, 2);
  subject.dispose();
  assert.equal(subject.requests[1].options.signal.aborted, true);
  subject.requests[1].resolve(response(snapshot()));
  await settle();
  assert.equal(subject.nodes["process-cpu"].textContent, "");
  assert.equal(subject.nodes["performance-error"].textContent, "");
  assert.equal(subject.timers.size, 0);
  assert.equal(subject.document.countListeners(), 0);
  assert.equal(subject.window.countListeners(), 0);
});
