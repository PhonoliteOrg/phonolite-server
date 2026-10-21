import { clearInlineError, showInlineError, fetchJson } from "./shared.js";

const DEFAULT_MAX_UPLOAD = 16 * 1024 * 1024 * 1024;
const WORKING = new Set(["preparing", "checking", "restoring", "uploading"]);

function describeReport(report, restored) {
  return `${restored ? "Restored missing fields for" : "Backup matches"} ${report.matched_artists} artists and ${report.matched_albums} albums. ` +
    `${report.unmatched_artists} artists and ${report.unmatched_albums} albums have no library match; ${report.ambiguous_albums} albums need a clearer match. ` +
    `${restored ? "These records are retained for future scans." : "These records will be retained for future scans."} ` +
    `${report.library_albums_without_backup} library albums have no matching backup and keep their current metadata. ` +
    `${report.restored_credit_resolutions || 0} saved artist-credit repairs ${restored ? "restored" : "available"}.` +
    (report.warnings?.length ? ` ${report.warnings.join(" ")}` : "");
}

export function setupMetadataBackup(scope) {
  const controls = document.getElementById("metadata-backup-controls");
  const input = document.getElementById("metadata-backup-file");
  const upload = document.getElementById("metadata-backup-upload");
  const download = document.getElementById("metadata-backup-download");
  const report = document.getElementById("metadata-backup-report");
  const jobList = document.getElementById("metadata-backup-jobs");
  const errorElement = document.getElementById("metadata-backup-error");
  if (!controls || !input || !upload || !download || !report) return;
  const configuredLimit = Number(controls.dataset?.maxUploadBytes);
  const maxUpload = Number.isSafeInteger(configuredLimit) && configuredLimit > 0 ? configuredLimit : DEFAULT_MAX_UPLOAD;
  const uploadLimit = controls.dataset?.uploadLimit || "16 GiB";
  const jobs = new Map();
  let pending = false;
  let renderedJobs;

  function request(url, options = {}) {
    return fetchJson(url, undefined, { ...options, signal: scope.signal });
  }
  function pause(milliseconds) {
    return new Promise((resolve) => {
      const finish = () => { scope.signal.removeEventListener("abort", finish); resolve(); };
      if (scope.signal.aborted) return finish();
      scope.signal.addEventListener("abort", finish, { once: true });
      scope.later(finish, milliseconds);
    });
  }
  function buttons() {
    input.disabled = pending;
    download.disabled = pending;
    upload.disabled = pending;
  }
  function render() {
    buttons();
    if (!jobList) return;
    const serialized = JSON.stringify({ pending, jobs: [...jobs.values()] });
    if (serialized === renderedJobs) return;
    renderedJobs = serialized;
    jobList.replaceChildren();
    for (const job of jobs.values()) {
      const section = document.createElement("div");
      section.className = "stack";
      const text = document.createElement("p");
      text.className = "hint";
      text.textContent = `${job.kind === "export" ? "Download" : "Restore"} ${(job.set_id || job.id).slice(0, 8)}: ` +
        (job.report && ["checked", "complete"].includes(job.phase) ? describeReport(job.report, job.phase === "complete") : job.message);
      section.append(text);
      const actions = document.createElement("div");
      actions.className = "row wrap";
      for (const part of job.parts || []) {
        const link = document.createElement("a");
        link.className = "button ghost";
        link.href = part.url;
        link.download = part.name;
        link.textContent = job.parts.length === 1 ? "Save backup" : `Save part ${part.index + 1} of ${job.parts.length}`;
        actions.append(link);
      }
      if (!WORKING.has(job.phase)) {
        const remove = document.createElement("button");
        remove.type = "button";
        remove.className = "button ghost";
        remove.dataset.removeBackup = job.id;
        remove.textContent = "Remove staged files";
        remove.disabled = pending;
        actions.append(remove);
      }
      section.append(actions);
      jobList.append(section);
    }
  }
  function update(job) {
    if (scope.signal.aborted) return;
    if (!job?.id || !job.phase || !["export", "restore"].includes(job.kind)) throw new Error("The server returned an invalid backup status.");
    const previous = jobs.get(job.id);
    jobs.set(job.id, job);
    const text = job.report && ["checked", "complete"].includes(job.phase)
      ? describeReport(job.report, job.phase === "complete") : job.message;
    if (report.textContent !== text) report.textContent = text;
    render();
    if (previous?.phase !== job.phase || previous?.message !== job.message) {
      document.dispatchEvent(new CustomEvent("library:backup-updated", { detail: job }));
    }
  }
  async function watch(job) {
    update(job);
    while (WORKING.has(job.phase) && !scope.signal.aborted) {
      await pause(750);
      if (scope.signal.aborted) return;
      job = await request(`/settings/backup/jobs/${job.id}`);
      update(job);
    }
    if (job.phase === "failed") throw new Error(job.message);
    return job;
  }
  function run(button, label, task) {
    if (pending || scope.signal.aborted) return;
    pending = true;
    clearInlineError(errorElement);
    return scope.run(button, label, async () => {
      render();
      try { await task(); }
      finally { pending = false; }
    }, (error) => showInlineError(errorElement, error.message)).finally(() => {
      pending = false;
      renderedJobs = undefined;
      if (!scope.signal.aborted) render();
    });
  }
  scope.listen(upload, "click", () => {
    if (!pending && !scope.signal.aborted) input.click();
  });
  scope.listen(input, "change", () => {
    if (pending || scope.signal.aborted) return;
    const files = Array.from(input.files || []);
    // Allow choosing the same files again after a failed or interrupted upload.
    input.value = "";
    if (!files.length) return;
    clearInlineError(errorElement);
    if (files.some((file) => file.size > maxUpload)) {
      showInlineError(errorElement, `Each backup file must be no larger than ${uploadLimit}.`);
      return;
    }
    return run(upload, "Uploading and restoring", async () => {
      for (let index = 0; index < files.length; index++) {
        report.textContent = `Uploading file ${index + 1} of ${files.length}: ${files[index].name}`;
        // Send each File directly; neither upload nor download is buffered as a Blob.
        const job = await request("/settings/backup/restore", {
          method: "POST", body: files[index], headers: { "Content-Type": "application/vnd.phonolite.metadata" },
        });
        if (scope.signal.aborted) return;
        await watch(job);
        if (scope.signal.aborted) return;
      }
    });
  });
  scope.listen(download, "click", () => run(download, "Creating backup", async () => {
    let job = await watch(await request("/settings/backup/export", { method: "POST" }));
    if (scope.signal.aborted) return;
    if (job.phase !== "ready" || !job.parts?.length) throw new Error("The backup has no download files.");
    for (const part of job.parts) {
      const link = document.createElement("a");
      link.href = part.url;
      link.download = part.name;
      document.body.append(link);
      try { link.click(); } finally { link.remove(); }
      await pause(150);
      if (scope.signal.aborted) return;
    }
    // Wait until a download request reaches the server. Blocked browsers have
    // visible per-file links and a bounded wait instead of an endless spinner.
    for (let attempt = 0; attempt < 20 && !job.started_downloads; attempt++) {
      await pause(750);
      if (scope.signal.aborted) return;
      job = await request(`/settings/backup/jobs/${job.id}`);
      update(job);
    }
    report.textContent = job.started_downloads
      ? `${job.parts.length} backup file(s) sent to your browser. Allow multiple downloads if prompted; the links below can save any blocked files.`
      : "Backup files are ready. Your browser may have blocked automatic downloads; use the download links below.";
  }));
  scope.listen(jobList, "click", (event) => {
    const button = event.target.closest("button[data-remove-backup]");
    if (!button || pending) return;
    const id = button.dataset.removeBackup;
    return run(button, "Removing staged files", async () => {
      await request(`/settings/backup/jobs/${id}/remove`, { method: "POST" });
      if (scope.signal.aborted) return;
      jobs.delete(id);
      report.textContent = "Staged backup files removed.";
      render();
    });
  });
  // Rediscover partial sets and running jobs when Settings is opened again.
  request("/settings/backup/jobs").then(async (data) => {
    if (scope.signal.aborted) return;
    const resumed = (data.jobs || []).filter((job) => !jobs.has(job.id));
    for (const job of resumed) jobs.set(job.id, job);
    render();
    for (const job of resumed.filter((job) => WORKING.has(job.phase))) {
      await watch(job);
      if (scope.signal.aborted) return;
    }
  }).catch((error) => {
    if (!scope.signal.aborted && error.name !== "AbortError") showInlineError(errorElement, error.message);
  });
  buttons();
}
