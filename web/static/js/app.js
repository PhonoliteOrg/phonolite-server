(() => {
  function initMessage(el) {
    if (!el) return;
    const close = el.querySelector("[data-close-message]");
    if (close) {
      close.addEventListener("click", () => {
        el.remove();
      });
    }
    if (el.dataset.autohide !== undefined) {
      setTimeout(() => {
        if (el.isConnected) {
          el.remove();
        }
      }, 3500);
    }
  }

  function setupMessages() {
    document.querySelectorAll(".msg").forEach(initMessage);
  }

  function showToast(message, isError) {
    const container = document.querySelector(".content");
    if (!container) return;
    const el = document.createElement("div");
    el.className = isError ? "msg error" : "msg";
    el.dataset.autohide = "true";
    const span = document.createElement("span");
    span.textContent = message;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "msg-close";
    button.dataset.closeMessage = "true";
    button.textContent = "x";
    el.appendChild(span);
    el.appendChild(button);
    container.prepend(el);
    initMessage(el);
  }

  function setupNavMenu() {
    const toggle = document.getElementById("nav-menu-toggle");
    const modal = document.getElementById("nav-modal");
    if (!toggle || !modal) return;
    const closeButtons = modal.querySelectorAll("[data-modal-close]");

    toggle.addEventListener("click", (event) => {
      event.preventDefault();
      modal.classList.add("open");
    });

    closeButtons.forEach((button) => {
      button.addEventListener("click", () => {
        modal.classList.remove("open");
      });
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        modal.classList.remove("open");
      }
    });

    modal.addEventListener("click", (event) => {
      if (event.target && event.target.matches("[data-modal-close]")) {
        modal.classList.remove("open");
      }
    });

    modal.addEventListener("click", (event) => {
      const action = event.target.closest("button[data-action]");
      if (!action) return;
      const type = action.dataset.action;
      const path =
        type === "restart"
          ? "/actions/restart"
          : type === "shutdown"
          ? "/actions/shutdown"
          : "";
      if (!path) return;
      const label = type === "restart" ? "restart" : "shut down";
      if (!confirm(`Are you sure you want to ${label} the server?`)) {
        return;
      }
      fetch(path, {
        method: "POST",
        headers: { Accept: "application/json", "X-Requested-With": "fetch" },
        credentials: "same-origin",
      })
        .then(async (resp) => {
          if (!resp.ok) {
            const data = await resp.json().catch(() => ({}));
            throw new Error(data.error || "request failed");
          }
        })
        .then(() => {
          showToast(`Server ${label} requested.`, false);
          modal.classList.remove("open");
        })
        .catch((err) => {
          showToast(err.message, true);
        });
    });
  }

  setupMessages();
  setupNavMenu();
  setupActivityBadge();
  setupActivityPage();
  setupUsers();
  setupMetadataSources();
  setupSettings();
  setupLibraryStatusPolling();

  function toParams(form) {
    const data = new FormData(form);
    const params = new URLSearchParams();
    for (const [key, value] of data.entries()) {
      params.append(key, value.toString());
    }
    return params;
  }

  function jsonHeaders() {
    return {
      Accept: "application/json",
      "Content-Type": "application/x-www-form-urlencoded",
      "X-Requested-With": "fetch",
    };
  }

  function fetchJson(url, params) {
    return fetch(url, {
      method: "POST",
      headers: jsonHeaders(),
      body: params,
      credentials: "same-origin",
    }).then(async (resp) => {
      const data = await resp.json().catch(() => ({}));
      if (!resp.ok) {
        const message = data.error || "Request failed";
        throw new Error(message);
      }
      return data;
    });
  }

  function refreshAfterSuccess() {
    window.location.reload();
  }

  function setupUsers() {
    const usersCard = document.getElementById("users-card");
    if (!usersCard) {
      return;
    }

    const currentUserId = usersCard.dataset.currentUserId || "";
    const currentUserRole = usersCard.dataset.currentUserRole || "";

    const modalAdd = document.getElementById("modal-add");
    const modalEdit = document.getElementById("modal-edit");
    const modalDelete = document.getElementById("modal-delete");
    const addButton = document.getElementById("open-add");
    const editToggle = document.getElementById("toggle-edit");
    const bulkBar = document.getElementById("bulk-bar");
    const bulkCount = document.getElementById("bulk-count");
    const bulkDeleteButton = document.getElementById("bulk-delete");
    const inlineMessage = document.getElementById("users-inline-message");

    const addForm = document.getElementById("form-add-user");
    const editForm = document.getElementById("form-edit-user");
    const deleteConfirm = document.getElementById("confirm-delete");
    const deleteMessage = document.getElementById("delete-message");
    const addError = document.getElementById("add-user-error");
    const editError = document.getElementById("edit-user-error");
    const deleteError = document.getElementById("delete-user-error");

    let deleteTargets = [];

    function openModal(modal) {
      if (!modal) return;
      modal.classList.add("open");
    }

    function closeModal(modal) {
      if (!modal) return;
      modal.classList.remove("open");
    }

    function closeAllModals() {
      [modalAdd, modalEdit, modalDelete].forEach(closeModal);
    }

    function clearInlineError(el) {
      if (!el) return;
      el.textContent = "";
      el.classList.remove("visible");
    }

    function showInlineError(el, message) {
      if (!el) return;
      el.textContent = message;
      el.classList.add("visible");
    }

    function setPageMessage(message) {
      if (!inlineMessage) return;
      if (message) {
        inlineMessage.textContent = message;
        inlineMessage.classList.add("visible");
      } else {
        inlineMessage.textContent = "";
        inlineMessage.classList.remove("visible");
      }
    }

    function updateBulkCount() {
      const selected = usersCard.querySelectorAll(".row-select:checked").length;
      if (bulkCount) {
        bulkCount.textContent = `${selected} selected`;
      }
      return selected;
    }

    usersCard.addEventListener("click", (event) => {
      const button = event.target.closest("button[data-action]");
      if (!button) return;
      const row = button.closest(".user-row");
      if (!row) return;

    const userId = row.dataset.userId || "";
    const username = row.dataset.username || "";
    const role = row.dataset.role || "user";
    const protectedUser = row.dataset.protected === "1";

    clearInlineError(editError);
    clearInlineError(deleteError);

    if (button.dataset.action === "edit") {
      if (protectedUser && userId !== currentUserId) {
        setPageMessage("Superadmin can only edit its own account.");
        return;
      }
      setPageMessage("");
      const usernameInput = editForm.querySelector("input[name=\"username\"]");
      const passwordInput = editForm.querySelector("input[name=\"password\"]");
      const roleSelect = editForm.querySelector("select[name=\"role\"]");
      const userIdInput = editForm.querySelector("input[name=\"user_id\"]");

      if (usernameInput) usernameInput.value = username;
      if (passwordInput) passwordInput.value = "";
      if (roleSelect) {
        roleSelect.value = role;
        roleSelect.disabled = protectedUser;
      }
      if (userIdInput) userIdInput.value = userId;
      openModal(modalEdit);
      return;
    }

    if (button.dataset.action === "delete") {
      if (protectedUser) {
        setPageMessage("Superadmin cannot be deleted.");
        return;
      }
      setPageMessage("");
      deleteTargets = [userId];
      deleteMessage.textContent = `Delete ${username}? This cannot be undone.`;
      openModal(modalDelete);
    }
  });

    usersCard.addEventListener("change", (event) => {
      if (event.target.classList.contains("row-select")) {
        updateBulkCount();
      }
    });

    if (addButton) {
      addButton.addEventListener("click", () => {
        clearInlineError(addError);
        addForm.reset();
        openModal(modalAdd);
      });
    }

    if (editToggle) {
      editToggle.addEventListener("click", () => {
        const editing = usersCard.classList.toggle("editing");
        if (bulkBar) {
          bulkBar.classList.toggle("hidden", !editing);
        }
        editToggle.textContent = editing ? "Exit edit mode" : "Edit mode";
        if (!editing) {
          usersCard.querySelectorAll(".row-select").forEach((box) => {
            box.checked = false;
          });
          updateBulkCount();
        }
      });
    }

    if (bulkDeleteButton) {
      bulkDeleteButton.addEventListener("click", () => {
        const selected = Array.from(
          usersCard.querySelectorAll(".row-select:checked")
        ).map((box) => box.closest(".user-row")?.dataset.userId || "");
        const cleaned = selected.filter((id) => id);
        if (cleaned.length === 0) {
          setPageMessage("Select at least one user to delete.");
          return;
        }
        setPageMessage("");
        deleteTargets = cleaned;
        deleteMessage.textContent = `Delete ${cleaned.length} selected users? This cannot be undone.`;
        openModal(modalDelete);
      });
    }

    addForm.addEventListener("submit", (event) => {
      event.preventDefault();
      clearInlineError(addError);
      fetchJson("/users", toParams(addForm))
        .then(() => {
          closeModal(modalAdd);
          refreshAfterSuccess();
        })
        .catch((err) => {
          showInlineError(addError, err.message);
        });
    });

    editForm.addEventListener("submit", (event) => {
      event.preventDefault();
      clearInlineError(editError);
      const userId =
        editForm.querySelector("input[name=\"user_id\"]")?.value || "";
      const params = toParams(editForm);
      const roleSelect = editForm.querySelector("select[name=\"role\"]");
      if (roleSelect && roleSelect.disabled) {
        params.set("role", roleSelect.value);
      }
      fetchJson(`/users/${userId}/update`, params)
        .then(() => {
          closeModal(modalEdit);
          refreshAfterSuccess();
        })
        .catch((err) => {
          showInlineError(editError, err.message);
        });
    });

    deleteConfirm.addEventListener("click", () => {
      clearInlineError(deleteError);
      if (deleteTargets.length === 0) {
        showInlineError(deleteError, "No users selected.");
        return;
      }
      if (deleteTargets.length === 1) {
        const userId = deleteTargets[0];
        fetchJson(`/users/${userId}/delete`, new URLSearchParams())
          .then(() => {
            closeModal(modalDelete);
            refreshAfterSuccess();
          })
          .catch((err) => {
            showInlineError(deleteError, err.message);
          });
      } else {
        const params = new URLSearchParams();
        params.append("user_ids", deleteTargets.join(","));
        fetchJson("/users/bulk-delete", params)
          .then(() => {
            closeModal(modalDelete);
            refreshAfterSuccess();
          })
          .catch((err) => {
            showInlineError(deleteError, err.message);
          });
      }
    });

    document.addEventListener("click", (event) => {
      if (event.target.matches("[data-close]")) {
        closeAllModals();
      }
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        closeAllModals();
      }
    });
  }

  function setupMetadataSources() {
    const metadataCard = document.getElementById("metadata-card");
    const sourcesList = document.getElementById("metadata-sources");
    const openAdd = document.getElementById("open-metadata-add");
    const modalAdd = document.getElementById("modal-metadata-add");
    const modalEdit = document.getElementById("modal-metadata-edit");
    const addForm = document.getElementById("form-metadata-add");
    const editForm = document.getElementById("form-metadata-edit");
    const addError = document.getElementById("metadata-add-error");
    const editError = document.getElementById("metadata-edit-error");
    const testButton = document.getElementById("metadata-test");
    if (!metadataCard || !sourcesList || !openAdd || !modalAdd || !modalEdit) {
      return;
    }

    function openModal(modal) {
      if (!modal) return;
      modal.classList.add("open");
    }

    function closeModal(modal) {
      if (!modal) return;
      modal.classList.remove("open");
    }

    function setProviderFields(form, provider) {
      if (!form) return;
      form.querySelectorAll(".provider-fields").forEach((section) => {
        const match = section.dataset.provider === provider;
        section.classList.toggle("active", match);
      });
    }

    function clearInlineError(el) {
      if (!el) return;
      el.textContent = "";
      el.classList.remove("visible");
    }

    function showInlineError(el, message) {
      if (!el) return;
      el.textContent = message;
      el.classList.add("visible");
    }

    function resetTestButton(result) {
      if (!testButton) return;
      testButton.disabled = false;
      testButton.textContent = result || "Test";
    }

    function setTestResult(ok) {
      if (!testButton) return;
      testButton.textContent = ok ? "\u2713" : "X";
      setTimeout(() => {
        resetTestButton("Test");
      }, 2000);
    }

    function closeAllModals() {
      [modalAdd, modalEdit].forEach(closeModal);
    }

    openAdd.addEventListener("click", () => {
      clearInlineError(addError);
      if (addForm) {
        addForm.reset();
        const select = addForm.querySelector("select[name=\"provider\"]");
        const provider = select ? select.value : "theaudiodb";
        setProviderFields(addForm, provider);
      }
      openModal(modalAdd);
    });

    const addProviderSelect = document.getElementById("metadata-add-provider");
    if (addProviderSelect) {
      addProviderSelect.addEventListener("change", (event) => {
        setProviderFields(addForm, event.target.value);
      });
    }

    const editProviderSelect = document.getElementById("metadata-edit-provider");
    if (editProviderSelect) {
      editProviderSelect.addEventListener("change", (event) => {
        setProviderFields(editForm, event.target.value);
      });
    }

    sourcesList.addEventListener("click", (event) => {
      const button = event.target.closest("button[data-action]");
      if (!button) return;
      const row = button.closest(".source-row");
      if (!row) return;

      const sourceId = row.dataset.sourceId || "";
      if (button.dataset.action === "edit-source") {
        const provider = row.dataset.provider || "theaudiodb";
        const apiKey = row.dataset.apiKey || "";
        const userAgent = row.dataset.userAgent || "";
        const sourceIdInput = editForm.querySelector("input[name=\"source_id\"]");
        const providerSelect = editForm.querySelector("select[name=\"provider\"]");
        const apiKeyInput = editForm.querySelector("input[name=\"api_key\"]");
        const userAgentInput = editForm.querySelector("input[name=\"user_agent\"]");

        if (sourceIdInput) sourceIdInput.value = sourceId;
        if (providerSelect) providerSelect.value = provider;
        if (apiKeyInput) apiKeyInput.value = apiKey;
        if (userAgentInput) userAgentInput.value = userAgent;
        setProviderFields(editForm, provider);
        clearInlineError(editError);
        resetTestButton("Test");
        openModal(modalEdit);
      } else if (button.dataset.action === "delete-source") {
        if (!confirm("Are you sure you want to delete this source?")) {
          return;
        }
        fetchJson(`/settings/metadata/${sourceId}/delete`, new URLSearchParams())
          .then(() => {
            row.remove();
            const placeholder = document.getElementById("no-metadata-sources");
            if (placeholder && sourcesList.children.length === 0) {
              placeholder.classList.remove("hidden");
            }
          })
          .catch((err) => {
            showToast(err.message, true);
          });
      }
    });

    sourcesList.addEventListener("change", (event) => {
      if (!event.target.classList.contains("source-toggle")) return;
      const sourceId = event.target.dataset.sourceId || "";
      const params = new URLSearchParams();
      if (event.target.checked) {
        params.set("enabled", "true");
      }
      fetchJson(`/settings/metadata/${sourceId}/toggle`, params).catch((err) => {
        showToast(err.message, true);
        event.target.checked = !event.target.checked;
      });
    });

    if (addForm) {
      addForm.addEventListener("submit", (event) => {
        event.preventDefault();
        clearInlineError(addError);
        fetch("/settings/metadata/add", {
          method: "POST",
          headers: {
            "Content-Type": "application/x-www-form-urlencoded",
            "X-Requested-With": "fetch",
          },
          body: toParams(addForm),
          credentials: "same-origin",
        })
          .then(async (resp) => {
            if (!resp.ok) {
              const data = await resp.json().catch(() => ({}));
              throw new Error(data.error || "Request failed");
            }
            return resp.text();
          })
          .then((html) => {
            closeModal(modalAdd);
            const placeholder = document.getElementById("no-metadata-sources");
            if (placeholder) {
              placeholder.classList.add("hidden");
            }
            sourcesList.insertAdjacentHTML("beforeend", html);
          })
          .catch((err) => {
            showInlineError(addError, err.message);
          });
      });
    }

    if (editForm) {
      editForm.addEventListener("submit", (event) => {
        event.preventDefault();
        clearInlineError(editError);
        const sourceId =
          editForm.querySelector("input[name=\"source_id\"]")?.value || "";
        fetchJson(`/settings/metadata/${sourceId}/update`, toParams(editForm))
          .then(() => {
            closeModal(modalEdit);
            refreshAfterSuccess();
          })
          .catch((err) => {
            showInlineError(editError, err.message);
          });
      });
    }

    if (testButton) {
      testButton.addEventListener("click", () => {
        if (!editForm) return;
        clearInlineError(editError);
        testButton.disabled = true;
        testButton.textContent = "Testing...";
        fetchJson("/settings/metadata/test", toParams(editForm))
          .then(() => {
            setTestResult(true);
          })
          .catch((err) => {
            showInlineError(editError, err.message);
            setTestResult(false);
          });
      });
    }

    document.addEventListener("click", (event) => {
      if (event.target.matches("[data-close]")) {
        closeAllModals();
      }
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        closeAllModals();
      }
    });
  }

  function setupSettings() {
    const form = document.getElementById("settings");
    if (!form) return;

    const saveButton = document.getElementById("save-settings");
    const reindexButton = document.getElementById("open-reindex");
    const reindexModal = document.getElementById("modal-reindex");
    const reindexConfirm = document.getElementById("confirm-reindex");
    const reindexClose = reindexModal?.querySelectorAll("[data-close-reindex]");
    const unsavedModal = document.getElementById("modal-unsaved");
    const discardButton = document.getElementById("unsaved-discard");
    const stayButton = document.getElementById("unsaved-stay");
    const closeUnsaved = unsavedModal?.querySelectorAll("[data-close-unsaved]");

    let initialState = serializeForm(form);
    let dirty = false;
    let submitting = false;
    let pending = null;

    function serializeForm(target) {
      const entries = [];
      Array.from(target.elements).forEach((el) => {
        if (!el.name || el.disabled) return;
        if (el.closest && el.closest("#metadata-sources")) return;
        if (el.dataset && el.dataset.ignoreDirty === "true") return;
        const type = (el.type || "").toLowerCase();
        if (type === "checkbox" || type === "radio") {
          entries.push(`${el.name}:${el.checked ? "1" : "0"}`);
          return;
        }
        if (type === "select-multiple") {
          const values = Array.from(el.selectedOptions)
            .map((option) => option.value)
            .join(",");
          entries.push(`${el.name}:${values}`);
          return;
        }
        entries.push(`${el.name}:${el.value}`);
      });
      entries.sort();
      return entries.join("|");
    }

    function setDirty(next) {
      dirty = next;
      if (saveButton) {
        saveButton.disabled = !dirty;
      }
    }

    function updateDirty() {
      setDirty(serializeForm(form) !== initialState);
    }

    function openUnsavedModal() {
      if (!unsavedModal) return;
      unsavedModal.classList.add("open");
    }

    function closeUnsavedModal() {
      if (!unsavedModal) return;
      unsavedModal.classList.remove("open");
    }

    function openReindexModal() {
      if (!reindexModal) return;
      reindexModal.classList.add("open");
    }

    function closeReindexModal() {
      if (!reindexModal) return;
      reindexModal.classList.remove("open");
    }

    function proceedPending() {
      const action = pending;
      pending = null;
      if (!action) return;
      setDirty(false);
      if (action.type === "link") {
        window.location.href = action.href;
        return;
      }
      if (action.type === "submit") {
        const { form, submitter } = action;
        if (submitter && form.requestSubmit) {
          form.requestSubmit(submitter);
        } else {
          form.submit();
        }
      }
    }

    updateDirty();

    form.addEventListener("input", updateDirty);
    form.addEventListener("change", updateDirty);

    if (reindexButton) {
      reindexButton.addEventListener("click", (event) => {
        event.preventDefault();
        openReindexModal();
      });
    }

    if (reindexConfirm) {
      reindexConfirm.addEventListener("click", () => {
        if (!reindexButton) return;
        closeReindexModal();
        if (form.requestSubmit) {
          form.requestSubmit(reindexButton);
        } else {
          form.submit();
        }
      });
    }

    if (reindexClose) {
      reindexClose.forEach((btn) => {
        btn.addEventListener("click", () => {
          closeReindexModal();
        });
      });
    }

    form.addEventListener("submit", (event) => {
      const submitter = event.submitter;
      const isSave = submitter && submitter.id === "save-settings";
      if (!isSave && dirty) {
        event.preventDefault();
        pending = { type: "submit", form, submitter };
        openUnsavedModal();
        return;
      }
      submitting = true;
    });

    document.addEventListener(
      "submit",
      (event) => {
        if (!dirty || submitting) return;
        if (event.target === form) return;
        event.preventDefault();
        pending = {
          type: "submit",
          form: event.target,
          submitter: event.submitter,
        };
        openUnsavedModal();
      },
      true
    );

    document.addEventListener(
      "click",
      (event) => {
        if (!dirty || submitting) return;
        const link = event.target.closest("a[href]");
        if (!link) return;
        const href = link.getAttribute("href");
        if (!href || href.startsWith("#")) return;
        event.preventDefault();
        pending = { type: "link", href };
        openUnsavedModal();
      },
      true
    );

    window.addEventListener("beforeunload", (event) => {
      if (!dirty || submitting) return;
      event.preventDefault();
      event.returnValue = "";
    });

    if (discardButton) {
      discardButton.addEventListener("click", () => {
        closeUnsavedModal();
        proceedPending();
      });
    }

    if (stayButton) {
      stayButton.addEventListener("click", () => {
        pending = null;
        closeUnsavedModal();
      });
    }

    if (closeUnsaved) {
      closeUnsaved.forEach((btn) => {
        btn.addEventListener("click", () => {
          pending = null;
          closeUnsavedModal();
        });
      });
    }

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        closeReindexModal();
      }
    });
  }

  function setupLibraryStatusPolling() {
    const scanning = document.querySelector('.status[data-status="scanning"]');
    if (!scanning) return;
    let retries = 0;
    const timer = setInterval(() => {
      fetch("/status/library", {
        method: "GET",
        headers: { Accept: "application/json", "X-Requested-With": "fetch" },
        credentials: "same-origin",
      })
        .then(async (resp) => {
          if (!resp.ok) {
            throw new Error("status request failed");
          }
          return resp.json();
        })
        .then((data) => {
          if (data && data.status && data.status !== "scanning") {
            clearInterval(timer);
            window.location.reload();
          }
        })
        .catch(() => {
          retries += 1;
          if (retries > 6) {
            clearInterval(timer);
          }
        });
    }, 5000);
  }

  function setupActivityBadge() {
    const badge = document.getElementById("activity-badge");
    if (!badge) return;

    function updateBadge() {
      fetch("/status/activity", {
        method: "GET",
        headers: { Accept: "application/json", "X-Requested-With": "fetch" },
        credentials: "same-origin",
      })
        .then(async (resp) => {
          if (!resp.ok) {
            throw new Error("activity status request failed");
          }
          return resp.json();
        })
        .then((data) => {
          const count = Number(data?.count || 0);
          badge.textContent = count.toString();
          badge.hidden = count === 0;
        })
        .catch(() => {
          badge.hidden = true;
        });
    }

    updateBadge();
    setInterval(updateBadge, 10000);
  }

  function setupActivityPage() {
    const clearButton = document.getElementById("clear-activity");
    if (!clearButton) return;
    const count = Number(clearButton.dataset.count || 0);
    clearButton.disabled = count === 0;

    clearButton.addEventListener("click", () => {
      fetch("/activity/clear", {
        method: "POST",
        headers: { Accept: "application/json", "X-Requested-With": "fetch" },
        credentials: "same-origin",
      })
        .then(async (resp) => {
          if (!resp.ok) {
            const data = await resp.json().catch(() => ({}));
            throw new Error(data.error || "request failed");
          }
        })
        .then(() => {
          window.location.reload();
        })
        .catch((err) => {
          showToast(err.message, true);
        });
    });
  }

  function setupLibrary() {
    const libraryContent = document.getElementById("library-content");
    if (!libraryContent) return;

    const paginationContainer = document.getElementById("library-pagination");
    const messageContainer = document.getElementById("library-message");
    const searchBlockContainer = document.getElementById("library-search-block");

    function loadLibrary(url, shouldUpdateHistory = true) {
        libraryContent.style.opacity = "0.5";
        fetch(url, {
            headers: { "X-Fragment": "true", "X-Requested-With": "fetch" },
            credentials: "same-origin"
        })
        .then(r => r.json())
        .then(data => {
            libraryContent.innerHTML = data.content;
            if (paginationContainer) paginationContainer.innerHTML = data.pagination;
            if (messageContainer && data.message) messageContainer.innerHTML = data.message;
            if (searchBlockContainer && data.search_block) searchBlockContainer.innerHTML = data.search_block;
            libraryContent.style.opacity = "1";
            
            if (shouldUpdateHistory && url !== window.location.href) {
                window.history.pushState({}, "", url);
            }
        })
        .catch(err => {
            console.error(err);
            libraryContent.style.opacity = "1";
        });
    }

    const initialUrl = window.location.href;
    loadLibrary(initialUrl);

    document.addEventListener("click", e => {
        const link = e.target.closest("a");
        if (!link) return;
        
        if (link.closest("#library-pagination") || link.closest(".filter-pill") || link.closest(".button.ghost") || link.closest(".tile")) {
             const href = link.getAttribute("href");
             if (href && href.startsWith("/library")) {
                 e.preventDefault();
                 loadLibrary(href);
             }
        }
    });

    document.addEventListener("submit", e => {
        if (e.target.matches(".search-form")) {
            e.preventDefault();
            const params = new URLSearchParams(new FormData(e.target));
            loadLibrary(`/library?${params.toString()}`);
        }
    });

    window.addEventListener("popstate", () => {
        loadLibrary(window.location.href, false);
    });
  }
  setupLibrary();
})();
