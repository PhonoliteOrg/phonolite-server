// Lazy artwork shares the console's loading treatment. Keep each image's
// dimensions on its frame so loading and failures never shift the tile grid.
export function setupArtwork(root = document) {
  const cleanups = [];
  root.querySelectorAll("img.tile-cover, img.cover-lg, img.cover-thumb").forEach((image) => {
    if (image.complete && image.naturalWidth > 0) return;
    const originalClass = image.className;
    const frame = document.createElement("span");
    frame.className = `${originalClass} artwork-frame`;
    const spinner = document.createElement("span");
    spinner.className = "spinner";
    spinner.setAttribute("role", "status");
    spinner.setAttribute("aria-label", "Loading artwork");
    image.replaceWith(frame);
    image.className = "artwork-image";
    frame.append(image, spinner);
    function loaded() {
      spinner.remove();
      frame.classList.add("artwork-ready");
    }
    function failed() {
      spinner.remove();
      image.hidden = true;
      const placeholder = document.createElement("span");
      placeholder.className = "artwork-unavailable";
      placeholder.textContent = "No artwork";
      frame.append(placeholder);
    }
    image.addEventListener("load", loaded, { once: true });
    image.addEventListener("error", failed, { once: true });
    if (image.complete) image.naturalWidth > 0 ? loaded() : failed();
    cleanups.push(() => {
      image.removeEventListener("load", loaded);
      image.removeEventListener("error", failed);
      image.className = originalClass;
      image.hidden = false;
      frame.replaceWith(image);
    });
  });
  return () => cleanups.forEach((cleanup) => cleanup());
}
