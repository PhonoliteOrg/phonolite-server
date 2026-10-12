import { setupShell } from "./shell.js";
import { setupUsers } from "./users.js";
import { setupSettings } from "./settings.js";
import { setupLibrary } from "./library.js";
import { setupLogsPage } from "./logs.js";
import { setupAuth } from "./auth.js";
import { setupConsoleNavigation } from "./navigation.js";
import { setupArtwork } from "./artwork.js";

const pages = {
  users: setupUsers,
  settings: setupSettings,
  library: setupLibrary,
  logs: setupLogsPage,
  login: setupAuth,
  setup: setupAuth,
};

const disposeShell = setupShell();
setupConsoleNavigation({
  onDispose: disposeShell,
  mountPage() {
    disposeShell.sync();
    const main = document.getElementById("main");
    // Library owns its artwork lifecycle because result fragments can change
    // while its page and search controls remain mounted.
    const disposeArtwork = document.body.dataset.page === "library" ? () => {} : setupArtwork(main);
    const page = pages[document.body.dataset.page]?.();
    return {
      canLeave: page?.canLeave,
      navigationFor: page?.navigationFor,
      dispose() {
        if (typeof page === "function") page();
        else page?.dispose?.();
        disposeArtwork();
      },
    };
  },
});
