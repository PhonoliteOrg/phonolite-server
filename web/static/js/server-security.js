import { openModal, closeModal } from "./dialogs.js";
import { clearInlineError, showInlineError, toParams } from "./shared.js";

export function setupServerSecurity(scope, canLeave) {
  const open = document.getElementById("open-rotate-keys");
  const modal = document.getElementById("modal-rotate-keys");
  const form = document.getElementById("form-rotate-keys");
  if (!open || !modal || !form) return;
  const password = document.getElementById("rotate-keys-password");
  const confirmation = document.getElementById("rotate-keys-confirm");
  const submit = document.getElementById("confirm-rotate-keys");
  const error = document.getElementById("rotate-keys-error");
  const status = document.getElementById("server-security-status");
  const pageError = document.getElementById("server-security-error");
  let pending = false;
  let rotated = false;
  const clearSecrets = () => { password.value = ""; confirmation.checked = false; };
  scope.onDispose(() => { clearSecrets(); closeModal(modal); });
  scope.listen(modal, "dialog:close", clearSecrets);
  scope.listen(open, "click", async () => {
    if (pending || rotated || !(await canLeave()) || scope.signal.aborted) return;
    clearSecrets(); clearInlineError(error); clearInlineError(pageError);
    openModal(modal);
  });
  scope.listen(form, "submit", async (event) => {
    event.preventDefault();
    if (pending || rotated || scope.signal.aborted || !form.reportValidity()) return;
    const params = toParams(form);
    pending = true;
    clearInlineError(error);
    await scope.run(submit, "Regenerating server keys", async () => {
      // Never retry this mutation. A lost reply can still mean it committed.
      const data = await scope.request("/settings/security/rotate-keys", params);
      if (scope.signal.aborted) return;
      rotated = true;
      closeModal(modal);
      open.disabled = true;
      status.textContent = `${data.message} New public-key fingerprint (SHA-256): ${data.fingerprint}`;
    }, (failure) => {
      showInlineError(modal.classList.contains("open") ? error : pageError, failure.message);
    });
    params.delete("password");
    clearSecrets();
    pending = false;
    if (rotated) submit.disabled = true;
  });
}
