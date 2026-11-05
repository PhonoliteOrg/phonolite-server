import { clearInlineError, fetchJson, showInlineError, startPolling } from "./shared.js";

const WINDOW_MS = 300_000;
const GAP_MS = 6_000;
const REQUEST_TIMEOUT_MS = 5_000;
const MIB = 1024 * 1024;
const usable = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0;
const coordinate = (value) => Number(value.toFixed(2));

export function formatUptime(seconds) {
  if (!usable(seconds)) return "Unavailable";
  const total = Math.floor(seconds);
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  return `${days ? `${days}d ` : ""}${hours}h ${minutes}m ${total % 60}s`;
}

export function formatMemory(bytes) {
  return usable(bytes) ? `${(bytes / MIB).toLocaleString(undefined, { maximumFractionDigits: 1 })} MiB` : "Unavailable";
}

export function formatCpu(percent) {
  return usable(percent) ? `${percent.toLocaleString(undefined, { maximumFractionDigits: 1 })}%` : "Unavailable";
}

export function cpuCapacityPercent(percent, logicalCpus) {
  return usable(percent) && Number.isSafeInteger(logicalCpus) && logicalCpus > 0
    ? percent / logicalCpus : null;
}

export function formatBytes(bytes) {
  if (!usable(bytes)) return "Unavailable";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const index = bytes === 0 ? 0 : Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / 1024 ** Math.max(0, index)).toLocaleString(undefined, { maximumFractionDigits: 1 })} ${units[Math.max(0, index)]}`;
}

export function formatTrafficRate(bytesPerSecond) {
  return usable(bytesPerSecond) ? `${formatBytes(bytesPerSecond)}/s` : "Unavailable";
}

const formatTrafficAxis = (kibPerSecond) => `${kibPerSecond.toLocaleString(undefined, { maximumFractionDigits: 1 })} KiB/s`;

// Position samples by time, leaving real gaps when sampling stops or a reading
// is unavailable. Returning coordinates keeps rendering free of HTML parsing.
export function performanceGraph(history, key, { minimum = 1, divisor = 1 } = {}) {
  const ordered = history.filter((sample) => usable(sample?.timestamp_ms)).slice(-150)
    .sort((left, right) => left.timestamp_ms - right.timestamp_ms);
  const end = ordered.at(-1)?.timestamp_ms ?? 0;
  const samples = ordered.filter((sample) => sample.timestamp_ms >= end - WINDOW_MS);
  const values = samples.filter((sample) => usable(sample[key])).map((sample) => sample[key] / divisor);
  const maximum = Math.max(minimum, ...values);
  // Round up to a readable axis ceiling with headroom for the memory series.
  const step = 10 ** Math.floor(Math.log10(maximum));
  const ceiling = Math.ceil(maximum / step) * step;
  let previous;
  let path = "";
  let point;
  for (const sample of samples) {
    if (!usable(sample[key])) { previous = undefined; continue; }
    const x = coordinate(600 * (sample.timestamp_ms - end + WINDOW_MS) / WINDOW_MS);
    const y = coordinate(180 * (1 - sample[key] / divisor / ceiling));
    const connected = previous !== undefined && sample.timestamp_ms > previous && sample.timestamp_ms - previous <= GAP_MS;
    path += `${connected ? "L" : "M"}${x},${y}`;
    previous = sample.timestamp_ms;
    point = { x, y };
  }
  return {
    path, point, ceiling, count: values.length,
    minimum: values.length ? Math.min(...values) : null,
    average: values.length ? values.reduce((mean, value) => mean + value / values.length, 0) : null,
    maximum: values.length ? Math.max(...values) : null,
  };
}

export function setupProcessPerformance() {
  if (!document.getElementById("process-performance")) return;
  const node = (id) => document.getElementById(id);
  const error = node("performance-error");
  let disposed = false;
  let received = false;

  function text(id, value) {
    const element = node(id);
    if (element && element.textContent !== value) element.textContent = value;
  }

  function draw(name, graph, formatter) {
    node(`${name}-line`)?.setAttribute("d", graph.path);
    const marker = node(`${name}-point`);
    if (marker) {
      marker.toggleAttribute("hidden", !graph.point);
      if (graph.point) {
        marker.setAttribute("cx", graph.point.x);
        marker.setAttribute("cy", graph.point.y);
      }
    }
    text(`${name}-scale`, formatter(graph.ceiling));
    const summary = graph.count
      ? `Min ${formatter(graph.minimum)} · Avg ${formatter(graph.average)} · Max ${formatter(graph.maximum)}`
      : "Min — · Avg — · Max —";
    text(`${name}-summary`, summary);
    const label = { cpu: "Server share of total CPU capacity", memory: "Resident memory", sent: "Traffic sent to clients", received: "Traffic received from clients" }[name];
    node(`${name}-chart`)?.setAttribute("aria-label", `${label} over the most recent five minutes. ${summary}. ${graph.count} available samples.`);
  }

  const stop = startPolling(async (signal) => {
    const request = new AbortController();
    const cancel = () => request.abort();
    let timedOut = false;
    signal.addEventListener("abort", cancel, { once: true });
    const deadline = window.setTimeout(() => {
      timedOut = true;
      request.abort();
    }, REQUEST_TIMEOUT_MS);
    try {
      const data = await fetchJson("/performance/data", undefined, { signal: request.signal });
      if (signal.aborted || disposed) return;
      if (!Array.isArray(data?.history)) throw new Error("The server returned invalid performance data.");
      const history = data.history.slice(-150);
      const latest = history.at(-1);
      const cpuHistory = history.map((sample) => ({
        ...sample, cpu_capacity_percent: cpuCapacityPercent(sample?.cpu_percent, data.logical_cpus),
      }));
      const measuringCpu = latest?.cpu_percent === null && latest?.cpu_status === "measuring";
      text("process-pid", Number.isSafeInteger(data.pid) ? String(data.pid) : "Unavailable");
      text("process-uptime", formatUptime(data.uptime_secs));
      text("process-cpus", Number.isSafeInteger(data.logical_cpus) && data.logical_cpus > 0 ? String(data.logical_cpus) : "Unavailable");
      text("process-cpu", measuringCpu ? "Measuring…" : formatCpu(cpuCapacityPercent(latest?.cpu_percent, data.logical_cpus)));
      text("process-memory", formatMemory(latest?.memory_bytes));
      text("process-active-streams", Number.isSafeInteger(data.active_streams) && data.active_streams >= 0 ? String(data.active_streams) : "Unavailable");
      text("process-sent", latest?.sent_bytes_per_sec === null ? "Measuring…" : formatTrafficRate(latest?.sent_bytes_per_sec));
      text("process-received", latest?.received_bytes_per_sec === null ? "Measuring…" : formatTrafficRate(latest?.received_bytes_per_sec));
      draw("cpu", performanceGraph(cpuHistory, "cpu_capacity_percent", { minimum: 100 }), formatCpu);
      draw("memory", performanceGraph(history, "memory_bytes", { divisor: MIB }), (mib) => formatMemory(mib * MIB));
      draw("sent", performanceGraph(history, "sent_bytes_per_sec", { divisor: 1024 }), formatTrafficAxis);
      draw("received", performanceGraph(history, "received_bytes_per_sec", { divisor: 1024 }), formatTrafficAxis);
      text("performance-status", "Updates every two seconds while this page is visible. History is kept in memory for up to five minutes.");
      clearInlineError(error);
      received = true;
    } catch (failure) {
      if (signal.aborted || disposed || (!timedOut && failure.name === "AbortError")) return;
      text("performance-status", received ? "Updates interrupted. Displaying the last successful measurements." : "Waiting for live performance data.");
      showInlineError(error, `Could not update performance. ${timedOut ? "The request timed out." : failure.message} Retrying automatically.`);
    } finally {
      window.clearTimeout(deadline);
      signal.removeEventListener("abort", cancel);
    }
  }, 2000);
  return () => { disposed = true; stop(); };
}
