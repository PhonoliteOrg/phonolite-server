import { navigateConsole } from "./shared.js";
import { setupArtwork } from "./artwork.js";

export function libraryUrl(value, base) {
  try {
    const url = new URL(value, base);
    const origin = new URL(base).origin;
    return url.origin === origin && (url.pathname === "/library" || url.pathname.startsWith("/library/")) ? url : null;
  } catch {
    return null;
  }
}

export function shouldHandleLink(event, link) {
  return !event.defaultPrevented && event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey &&
    (!link.target || link.target === "_self") && !link.hasAttribute("download");
}

export function validLibraryFragment(data) {
  return data && ["content", "pagination", "message", "search_block"].every((key) => typeof data[key] === "string");
}

function replaceChangedHtml(element, html) {
  if (element && element.innerHTML !== html) element.innerHTML = html;
}

function updateAttributes(element, next, names) {
  for (const name of names) {
    const value = next.getAttribute(name);
    if (element.getAttribute(name) === value) continue;
    if (value === null) element.removeAttribute(name);
    else element.setAttribute(name, value);
  }
}

// The search form stays mounted while results change. Besides keeping keyboard
// focus and the caret intact, this protects text entered during a slow request.
export function updateLibrarySearch(block, html, inputAtRequest) {
  if (!block || block.innerHTML === html) return;
  const template = document.createElement("template");
  template.innerHTML = html;
  const incoming = template.content;
  const form = block.querySelector("form[data-library-search]");
  const nextForm = incoming.querySelector("form[data-library-search]");
  const input = form?.querySelector("input[name='search']");
  const nextInput = nextForm?.querySelector("input[name='search']");
  if (!form || !nextForm || !input || !nextInput) {
    replaceChangedHtml(block, html);
    return;
  }
  const value = nextInput.value;
  if (input.value === inputAtRequest && input.value !== value) input.value = value;
  if (input.defaultValue !== value) input.defaultValue = value;

  const filter = form.querySelector("input[name='filter']");
  const nextFilter = nextForm.querySelector("input[name='filter']");
  if (filter && nextFilter && filter.value !== nextFilter.value) filter.value = nextFilter.value;

  const clear = form.querySelector(".search-clear");
  const nextClear = nextForm.querySelector(".search-clear");
  if (clear && nextClear) updateAttributes(clear, nextClear, ["href", "hidden"]);
  else if (clear) clear.remove();
  else if (nextClear) form.querySelector(".search-icon-container")?.append(nextClear);

  for (const selector of [".snapshot-card", ".snapshot-tabs"]) {
    const existing = block.querySelector(selector);
    const next = incoming.querySelector(selector);
    if (!next) { existing?.remove(); continue; }
    if (!existing) {
      if (selector === ".snapshot-card") block.prepend(next);
      else block.append(next);
      continue;
    }
    if (selector === ".snapshot-tabs") updateAttributes(existing, next, ["hidden"]);
    if (existing.innerHTML === next.innerHTML) continue;
    if (selector === ".snapshot-tabs") {
      const links = existing.querySelectorAll("a[data-library-link]");
      const nextLinks = next.querySelectorAll("a[data-library-link]");
      if (links.length === nextLinks.length) {
        links.forEach((link, index) => updateAttributes(link, nextLinks[index], ["href", "class", "aria-current"]));
        continue;
      }
    }
    replaceChangedHtml(existing, next.innerHTML);
  }
}

export function setupLibrary() {
  const content = document.getElementById("library-content");
  if (!content) return;
  // Artwork adds frames and spinners to the DOM. Compare future responses with
  // the last server markup, so those local decorations never force a repaint.
  let contentHtml = content.innerHTML;
  let disposeArtwork = setupArtwork(content);
  const message = document.getElementById("library-message");
  let messageHtml = message?.innerHTML;
  let searchTimer;
  let composing = false;
  let disposed = false;
  let editVersion = 0;
  const listeners = [];
  function listen(event, callback, target = document) {
    target.addEventListener?.(event, callback);
    listeners.push(() => target.removeEventListener?.(event, callback));
  }

  function cancelQueuedSearch() {
    if (searchTimer !== undefined) window.clearTimeout(searchTimer);
    searchTimer = undefined;
  }

  function searchForm() {
    return document.getElementById("library-search-block")?.querySelector("form[data-library-search]");
  }

  function syncSearchControls() {
    const input = document.getElementById("library-search");
    if (!input) return;
    const searching = Boolean(input.value.trim());
    const clear = searchForm()?.querySelector(".search-clear");
    if (clear) clear.hidden = !searching;
    const tabs = document.getElementById("library-search-block")?.querySelector(".snapshot-tabs");
    if (tabs) tabs.hidden = searching;
    const pagination = document.getElementById("library-pagination");
    if (pagination) pagination.hidden = searching;
  }

  function showSearchStatus(text, loading = false) {
    const html = `<div class="library-search-status" role="status">${loading ? '<span class="spinner" aria-hidden="true"></span>' : ""}<span>${text}</span></div>`;
    if (contentHtml === html) return;
    disposeArtwork();
    disposeArtwork = () => {};
    content.innerHTML = contentHtml = html;
  }

  function showSearchPending(query) {
    const tooShort = query && Array.from(query).length < 2;
    showSearchStatus(tooShort ? "Type at least 2 characters to search." : (query ? "Searching library…" : "Loading library…"), !tooShort);
  }

  function navigateSearch({ replace, refresh = true } = {}) {
    const form = searchForm();
    const input = document.getElementById("library-search");
    if (!form || !input || disposed) return;
    const url = libraryUrl(form.action || "/library", window.location.href);
    if (!url) return;
    url.search = "";
    url.searchParams.set("search", input.value.trim());
    url.searchParams.set("filter", form.querySelector("input[name='filter']")?.value || "artists");
    const version = editVersion;
    Promise.resolve(navigateConsole(url, { replace: replace ?? Boolean(input.defaultValue?.trim()), focus: false, refresh })).then((loaded) => {
      if (loaded === false && !disposed && version === editVersion) {
        showSearchStatus("Could not load results. Press Enter to retry.");
      }
    });
  }

  function queueSearch(input, isComposing = false) {
    cancelQueuedSearch();
    editVersion++;
    syncSearchControls();
    const query = input.value.trim();
    showSearchPending(query);
    if (isComposing || composing) return;
    if (Array.from(query).length < 2) {
      navigateSearch();
      return;
    }
    searchTimer = window.setTimeout(() => {
      searchTimer = undefined;
      navigateSearch();
    }, 450);
  }

  // The console router owns cancellation, session redirects, and history. A
  // library-to-library request reuses this lifecycle and changes only regions.
  listen("click", (event) => {
    const link = event.target.closest("a[data-library-link]");
    if (!link) {
      const destination = event.target.closest("a[href]");
      if (destination && shouldHandleLink(event, destination)) cancelQueuedSearch();
      return;
    }
    if (!shouldHandleLink(event, link)) return;
    const url = libraryUrl(link.href, window.location.href);
    if (!url) return;
    event.preventDefault();
    cancelQueuedSearch();
    if (link.classList?.contains("search-clear")) {
      const input = document.getElementById("library-search");
      if (input) { input.value = ""; input.focus(); }
      editVersion++;
      composing = false;
      syncSearchControls();
      showSearchPending("");
      navigateSearch({ replace: true });
    } else navigateConsole(url);
  });

  listen("submit", (event) => {
    const form = event.target;
    if (event.defaultPrevented || !form.matches("form[data-library-search]")) return;
    event.preventDefault();
    cancelQueuedSearch();
    editVersion++;
    syncSearchControls();
    navigateSearch({ replace: false });
  });

  listen("input", (event) => {
    if (event.target === document.getElementById("library-search")) queueSearch(event.target, event.isComposing);
  });
  listen("compositionstart", (event) => {
    if (event.target !== document.getElementById("library-search")) return;
    composing = true;
    editVersion++;
    cancelQueuedSearch();
  });
  listen("compositionend", (event) => {
    if (event.target !== document.getElementById("library-search")) return;
    composing = false;
    queueSearch(event.target);
  });
  listen("popstate", cancelQueuedSearch, window);
  listen("pagehide", cancelQueuedSearch, window);
  syncSearchControls();

  return {
    navigationFor(value) {
      const url = libraryUrl(value, window.location.href);
      if (!url || url.pathname !== "/library") return null;
      const inputAtRequest = document.getElementById("library-search")?.value;
      const versionAtRequest = editVersion;
      return {
        headers: { "X-Fragment": "true" },
        isValid: validLibraryFragment,
        busyElement: content,
        apply(data) {
          if (!validLibraryFragment(data)) throw new Error("The server returned an unexpected library view.");
          if (disposed || versionAtRequest !== editVersion) return;
          if (searchTimer !== undefined && (url.searchParams.get("search") || "") !== document.getElementById("library-search")?.value.trim()) return;
          const focused = document.activeElement;
          if (data.content !== contentHtml) {
            disposeArtwork();
            content.innerHTML = data.content;
            contentHtml = data.content;
            disposeArtwork = setupArtwork(content);
          }
          replaceChangedHtml(document.getElementById("library-pagination"), data.pagination);
          if (message && messageHtml !== data.message) {
            message.innerHTML = data.message;
            messageHtml = data.message;
          }
          updateLibrarySearch(document.getElementById("library-search-block"), data.search_block, inputAtRequest);
          syncSearchControls();
          // Preserve input/tab focus. A removed result can hand focus to the
          // new results region without moving the rest of the page.
          return focused && focused.isConnected === false ? content : undefined;
        },
      };
    },
    dispose() {
      disposed = true;
      cancelQueuedSearch();
      disposeArtwork();
      listeners.forEach((remove) => remove());
    },
  };
}
