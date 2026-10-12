import { beginBusy, clearInlineError, fetchJson, navigateConsole, showInlineError, toParams } from "./shared.js";

export function setupAuth() {
  const form = document.querySelector(".auth-card form");
  if (!form) return;
  const formError = document.getElementById("auth-error");
  const controller = new AbortController();
  let submitting = false;
  async function submit(event) {
    event.preventDefault();
    if (submitting) return;
    clearInlineError(formError);
    submitting = true;
    const params = toParams(form);
    const finish = beginBusy(form.querySelector('button[type="submit"]'), "Signing in");
    try {
      const data = await fetchJson(form.action, params, { signal: controller.signal });
      if (controller.signal.aborted) return;
      if (!data.redirect) throw new Error("The server returned an unexpected sign-in response.");
      await navigateConsole(data.redirect, { replace: true, force: true });
    } catch (error) {
      if (error.name !== "AbortError" && !controller.signal.aborted) showInlineError(formError, error.message);
    } finally {
      submitting = false;
      finish();
    }
  }
  form.addEventListener("submit", submit);
  return () => {
    controller.abort();
    form.removeEventListener("submit", submit);
  };
}
