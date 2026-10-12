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
  if (clear && nextClear) updateAttributes(clear, nextClear, ["href"]);
  else if (clear) clear.remove();
  else if (nextClear) form.querySelector(".search-icon-container")?.append(nextClear);

  const contributor = form.querySelector("input[name='include_contributors']");
  const nextContributor = nextForm.querySelector("input[name='include_contributors']");
  if (contributor && nextContributor && contributor.type === nextContributor.type) {
    if (contributor.checked !== nextContributor.checked) contributor.checked = nextContributor.checked;
    if (contributor.value !== nextContributor.value) contributor.value = nextContributor.value;
  } else {
    const previousControl = contributor?.closest("label") || contributor;
    previousControl?.remove();
    if (nextContributor) form.append(nextContributor.closest("label") || nextContributor);
  }

  for (const selector of [".snapshot-card", ".snapshot-tabs"]) {
    const existing = block.querySelector(selector);
    const next = incoming.querySelector(selector);
    if (!next) { existing?.remove(); continue; }
    if (!existing) { block.append(next); continue; }
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
  const listeners = [];
  function listen(event, callback) {
    document.addEventListener(event, callback);
    listeners.push(() => document.removeEventListener(event, callback));
  }

  // The console router owns cancellation, session redirects, and history. A
  // library-to-library request reuses this lifecycle and changes only regions.
  listen("click", (event) => {
    const link = event.target.closest("a[data-library-link]");
    if (!link || !shouldHandleLink(event, link)) return;
    const url = libraryUrl(link.href, window.location.href);
    if (!url) return;
    event.preventDefault();
    navigateConsole(url);
  });

  listen("submit", (event) => {
    const form = event.target;
    if (event.defaultPrevented || !form.matches("form[data-library-search]")) return;
    const url = libraryUrl(form.action || "/library", window.location.href);
    if (!url) return;
    event.preventDefault();
    url.search = new URLSearchParams(new FormData(form)).toString();
    navigateConsole(url);
  });

  listen("change", (event) => {
    if (event.target.matches("input[data-library-contributors]")) {
      event.target.closest("form[data-library-search]")?.requestSubmit();
    }
  });

  return {
    navigationFor(value) {
      const url = libraryUrl(value, window.location.href);
      if (!url || url.pathname !== "/library") return null;
      const inputAtRequest = document.getElementById("library-search")?.value;
      return {
        headers: { "X-Fragment": "true" },
        isValid: validLibraryFragment,
        busyElement: content,
        apply(data) {
          if (!validLibraryFragment(data)) throw new Error("The server returned an unexpected library view.");
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
          // Preserve input/tab focus. A removed result can hand focus to the
          // new results region without moving the rest of the page.
          return focused && focused.isConnected === false ? content : undefined;
        },
      };
    },
    dispose() {
      disposeArtwork();
      listeners.forEach((remove) => remove());
    },
  };
}
