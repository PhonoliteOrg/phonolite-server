# Phonolite server console

The console is a single page application built from Axum-rendered views and native JavaScript modules. It has no frontend framework or separate frontend build step. The server renders the first view; subsequent navigation fetches a view and updates the existing shell without reloading the document. Every server build embeds `web/templates` and `web/static` in the executable; deployment does not need a separate `web` directory. Cargo rebuilds the bundle when these source files change, including additions and removals. Tests and this README are not embedded.

For console development, explicitly set `PHONOLITE_WEB_ROOT` to a complete source `web` directory to load its files from disk. Unset it or leave it empty to use the bundled console. Nearby directories are not auto-discovered. Overrides are limited to the build's known asset paths, and missing files fail rather than falling back to bundled content; rebuild after adding or removing files.

Use GET for reading views and data, and POST for actions that change state. A single page interface does not require changing reads into POST requests: the navigation controller keeps filters, searches, and pagination in browser history state while displaying the clean page path in the address bar. Back and Forward restore that state. Direct GET links continue to render a complete HTML page, including when JavaScript is unavailable.

The root route `/` opens Library at `/library` after the setup and login checks. Users lives at `/users`, and account mutations return there when JavaScript is unavailable.

## Design contract

The visual reference is `phonolite-app/lib/widgets/ui/obsidian_theme.dart`, with locally served fonts taken from `phonolite-website/web/static/fonts`.

| Role | Value |
| --- | --- |
| Canvas / raised surface | `#050505` / `#0A0A0D` |
| Primary accent | `#FFD700` |
| Text / muted text | `#F2F2F2` / `#8A8A9A` |
| Body / display type | Poppins / Rajdhani |
| Border | White at 12% opacity |
| Spacing | 4, 8, 12, 16, 24, 32, 48 px |

Gold indicates emphasis and selection. Success, warning, and destructive actions have separate semantic colors. Panels and actions use simple rectangular shapes without clipped corners; forms and tables favor readable density. Login and initial admin setup sit directly on the canvas without a card background, border, shadow, or gold top bar. Navigation becomes a labeled sidebar at 980 px. Smaller screens use a hamburger on the left to open that sidebar as an overlay drawer; the main content stays in place. The drawer starts closed before the first render, without an initial closing animation. The layout enables JavaScript styles in the head before loading the stylesheet; later user interactions retain their open and close transitions. Navigation remains visible if JavaScript is unavailable.

`static/css/admin.css` imports tokens, base rules, layout, components, and page styles in that order. Add shared primitives in `components.css`; keep feature-specific rules in `pages.css`. Do not introduce new hardcoded palettes in templates or Rust renderers.

Library artist, album, and track cards sit directly on the canvas without background boxes or visible outer borders. Artwork in library cards, album details, and thumbnails has no border. Hovering or focusing a card with the keyboard adds the listening app's gold artwork glow (32% opacity, 9 px blur, 1.5 px spread, 120 ms transition). The glow follows the artwork's shape and works for loaded images, loading frames, and placeholders. Reduced-motion preferences disable the transition.

The artist, album, and track counts at the top of Library also sit directly on the canvas without a background or visible outer border, retaining their spacing and vertical dividers. The search bar sits directly below these counts, followed by the Artists / Albums / Tracks selector. Fragment updates preserve this order and keep the search form mounted.

The Library search bar matches the app's 48 px `SearchHud`: square corners, a faint diagonal white gradient, a single bottom border, 24 px leading and 16 px trailing padding, and vertically centered Rajdhani text with 1 px letter spacing. Its locally bundled 24 px Lucide clear icon has a transparent 48 px target, muted idle color, and gold hover glow. Focus highlights the bottom border; the input and clear action retain their search and navigation behavior. Shared field styles live in `components.css`, with colors in `tokens.css`.

Library's centered Artists / Albums / Tracks selector matches the listening app's Local / Server toggle (`ObsidianToggleOption`): adjacent uppercase Rajdhani labels, muted inactive text, transparent backgrounds, and a 2 px gold underline for the current selection. Hover, keyboard focus, and press turn the label gold; keyboard focus retains its outline. Options keep 48 px touch targets and wrap with centered rows at narrow widths.

The web console always uses the standard artist browse scope (main artists and featured guests). It has no Include contributors control or contributor filter state in search, clear-search links, or pagination; legacy `include_contributors` query parameters are ignored by the console.

Typing in Library replaces the browse grid with a ranked list across artists, albums, and tracks, using the same shared search function as the app's `/library/search` API. Search starts at two characters after a 450 ms pause; Enter submits immediately. Results include artwork, type, and artist/album context, with the best 50 matches shown. The browse category selector and pagination hide while searching. Artist and album results open their details; track results open and highlight the track in its album. The search field stays mounted and retains focus and text while requests run. Query edits invalidate older responses, composition waits until completion, and queued searches are canceled on navigation or disposal. Clearing with the X or deleting the query immediately requests the selected browse category again. Direct GET searches render the same list without JavaScript.

Library pagination shows up to nine consecutive page numbers, centered around the current page where possible and shifted to keep nine visible near either end. Previous and Next use locally bundled Lucide chevrons and stay visible as native disabled buttons at the corresponding boundary. A single-page result shows page 1 with both chevrons disabled; empty results have no pagination. Enabled controls retain search and filter parameters and normal library navigation behavior.

Search-result artwork stays centered within its square thumbnail, including during loading. Artist logos use contain fitting; album and track artwork use centered cover fitting. A gold Lucide person, disc, or music-note badge overlaps the bottom-left corner and carries an accessible result-type label. Space is reserved for the badge outside the image. The type has no separate visible text beside the chevron. Thumbnails and badges use the app's 48/22 px desktop and 42/20 px compact sizes.

Artist and album detail sections use the same transparent treatment for biographies, summaries, album groups, collaboration and other credits, album metadata, and track lists. Biography and summary text use the full section width, matching the album groups. Artist banners have no border and align with that content width at desktop and mobile sizes. Section spacing and artwork retain their existing layout.

Album details list album genres and artist genres separately. Artist genres use the full stored genre lists for all album artists, combining duplicate labels and showing a dash when no artist genres are known.

Logs uses the remaining viewport height for its active tab, with scrolling inside the entries or indexing issue table. These views sit directly on the page without nested card backgrounds or borders. Their scrollbars occupy a reserved gutter beside the content, using a 16 px classic track in Chromium instead of overlaying rows.

Logs category selectors share Library's uppercase labels, transparent backgrounds, gold selection underline, hover and focus colors, and 48 px touch targets. The categories are centered, with adjacent options and centered rows when they wrap on narrow screens. The shared `.tab-chip` styles live in `components.css`.

Settings section headings align with the page content edge, with their fields, hints, music-folder rows, and metadata-source rows indented by 24 px (16 px on narrow screens). Full-width heading dividers make each section clear. Sections and rows retain transparent backgrounds and no outer borders. The save bar appears at the bottom of the viewport only while changes need saving or a save is pending. It follows the content width, with space reserved below the form so controls remain reachable. Reverting or saving all changes hides it; errors remain visible independently of the bar.

Settings text, number, and password fields, including folder and metadata dialogs, share Library search's 48 px height, square corners, subtle gradient, bottom underline, Rajdhani typography, and gold focus highlight through the shared `.obsidian-input` primitive. Invalid folder paths retain a red underline, while number inputs retain their native controls and validation limits.

Added music folders appear as a list with folder icons, subtle row separators, and edit/delete actions aligned to the right. The list is indented one additional level beneath Music folders (24 px on desktop, 16 px on narrow screens). Action icons have transparent backgrounds and borders, including on hover, and retain visible keyboard focus outlines.

Add music folder opens a lightweight browser of the server's filesystem. It starts at the drive list on Windows or `/` on Linux/macOS. Typing a path checks that it is a readable folder and opens it in the tree after a 350 ms pause; Enter checks immediately. Relative paths resolve beside the config file, and clearing the path returns to the starting location. Invalid or unreadable paths show an inline error and cannot be added. Folder rows align left, and opening a folder expands its children in an indented hierarchy while keeping siblings visible. Clicking an open folder collapses its branch; the selected folder is highlighted and added with Add this folder. `GET /settings/folders?path=…` lists immediate subfolders for authenticated administrators only; branches load on demand and remain cached until reopening the dialog resets the tree. Opening folders does not change configuration; Add this folder uses the existing music-root action and reindex flow. Closing the dialog or navigating away cancels pending path checks and browse requests.

## Interaction contracts

- `layout.html` supplies `body[data-page]` for page module dispatch in `static/js/app.js`. `navigation.js` handles console navigation, updates the shell, and mounts the current page module.
- Navigation requests use GET with `X-Console-View: true`. Successful JSON responses contain `title`, `page`, `page_class`, `content`, `nav`, and `footer`, rendered from the same templates as full HTML responses. Existing endpoint URLs, cookie authentication, and role checks apply to both formats.
- Page setup functions return cleanup callbacks or lifecycle objects with `dispose`, optional `canLeave`, and optional `navigationFor(url)` for regional updates. Replacing a page disposes its listeners and in-flight work; regional updates retain the mounted lifecycle. Navigation controls and library scan polling persist across authenticated pages. Unsaved Settings changes must pass the leave guard.
- Navigation preserves modified clicks, downloads, external links, and anchor links. Only supported console views are handled in place. Loading keeps the current view visible without fading or dimming it and marks only the requested region busy, without a floating loading notice. Actions use the shared gold Phonolite spinner beside their controls. Failed requests retain the current content and its corresponding history state. Selecting the already active destination makes no request; explicit refresh still fetches current data.
- Library navigation uses `data-library-link` and `data-library-search`. These behavior hooks are independent of styling classes.
- Full library pages and `X-Fragment: true` responses share one rendered view. Fragment JSON retains `content`, `pagination`, `message`, and `search_block`. Search, filters, pagination, details, and Back/Forward update only changed library regions while retaining the search form, focus, caret, and text edited during a request. Unchanged result markup keeps its artwork nodes and listeners.
- Forms submit state changes as POST requests through the shared JSON helper. User mutations return authoritative row markup and reconcile the table by account ID, retaining unchanged rows, edit mode, selected remaining accounts, and table scroll. Settings shows “No unsaved changes” with a neutral disabled Save button until a field changes, and reports “Settings saved” only after a successful save. Saves update the saved baseline and status in place while retaining edits made during the save. Login, Setup, and expired-session recovery use the same navigation controller.
- Keep form names, endpoint URLs, cookie authentication, role protections, and JSON negotiation intact when changing presentation.
- Dialogs use the shared controller in `dialogs.js`. Each modal has a labeled title, focus target, and explicit close control. Background content becomes inert while a dialog is open.
- The narrow-screen navigation drawer uses the same dialog controller for focus containment, nested server-action dialogs, Escape dismissal, and focus restoration. Backdrop, close-button, and link clicks dismiss it; resizing to desktop restores the inline sidebar.
- Settings retain separate immediate actions for folders, metadata sources, and debug logging. `data-ignore-dirty` excludes immediate controls from the main save state.
- Use shared polling for live data so requests pause when hidden and cannot overlap. New navigation requests must cancel or ignore superseded responses.
- A dedicated text line above the sidebar's server-actions button shows indexing and metadata activity. A small gold spinner follows the text while indexing, saving, or metadata gathering runs and respects reduced-motion preferences. Local phases show discovery, completed/total track reads, completed/total album folders, and saving. Disc folders count as one album group; duplicate copies are processed separately and can merge in the final catalog total. After indexing, the label shows the sweep position and current artist or artist/album, such as “Artist sweep [8/24]: Björk” or “Album credits sweep [123/149]: Björk — Homogenic”. The position is one-based for the item currently being processed, matching the album-credit sweep logs. Skipped, failed, and unmatched checks advance the sweep; the position resets for each phase/pass and does not depend on stored album completeness. Names remain visible through provider requests, retries, rate-limit waits, and storage, then clear when that check ends or the phase changes. Between items the label retains the last position. Overlapping jobs keep separate positions and targets; finishing the newest job restores the other active job's progress. When indexing and metadata work stop, the progress line and spinner hide, including when some albums remain incomplete. Later work reveals the progress line again.
- The shell checks library status every two seconds while an authenticated view is visible, retaining one polling chain across navigation and pausing when hidden. Indexing and metadata completion update the sidebar in place and refresh library result regions only when Library is active, leaving other pages mounted. If navigation is pending, the refresh waits for the latest selection and is discarded when another page is selected.
- Logs retains existing row nodes when its bounded tail advances and skips DOM/layout work for unchanged content. Subsequent polls send the last `revision`; an unchanged response contains `unchanged: true` and omits the full lines payload. Explicit refresh, filter changes, and clearing logs fetch a fresh snapshot. Successful console polls are excluded from routine request/profiling logs so polling does not generate its own log traffic; failures and server actions remain logged.
- The console does not create notification banners or reserve space for them, including setup, indexing, missing-folder, and library-error notices. Indexing and metadata progress appear only in the sidebar. Failed actions display plain inline error text by their controls; successful actions update their existing content or save status. Error pages show plain content on the canvas. Concurrent session expiry shares one authentication navigation, and repeated confirmation activation cannot stack dialogs.
- Each Logs category shows its complete log file path, updated when its tab is selected. Paths are rendered using the server's file selection mapping. Logs shows indexing issue details only in the Indexing issues tab and refreshes the selected view. Issue details refresh through `/logs/issues` and use the dedicated `issues.log`, with persisted tag-error files as a fallback when that file is unavailable. Issue events write only to `issues.log`; other log views also omit historical ISSUE entries. Activity events remain available through the Activities log filter.

## Verification

Settings has a separate **Server security** section with **Regenerate server
keys**. A focused dialog requires the current administrator password and an
acknowledgement that everyone will be disconnected and must sign back in. Unsaved Settings
edits pass through the existing discard guard first. The action posts once to
`/settings/security/rotate-keys`, shows the replacement public-key fingerprint,
and explains the restart/sign-out. Apps refresh the TLS pin during password
sign-in only after verifying their remembered OPAQUE identity. Passwords are cleared on completion, cancel,
and disposal. This action rotates the TLS identity shared by the API, console,
and audio stream; it does not reset account passwords or OPAQUE credentials.

Settings includes a separate **Metadata backup** section with download, file
selection, checking and additive restore. A single **Upload backup files** button
opens the multi-file picker and starts uploading, validation and restore once
files are selected. These controls do not participate in
the main settings form's unsaved-change state. The styled file picker accepts
multiple files and applies the server-rendered limit to each file (16 GiB by
default). Uploads send raw File bodies sequentially; complete sets are checked
from disk before applying metadata automatically. Parts can also be uploaded separately in any
order. Settings rediscovers staged sets and jobs on remount. Page requests share
the settings view's abort/disposal lifecycle; accepted server jobs continue
independently. Results/errors appear beside the controls, and the persistent
sidebar shows upload, missing-part, checking, restore, and completion status.
Exports prepare asynchronously, split at the per-file limit, and use the browser's
native download manager without a whole-file JavaScript Blob. The button keeps
its spinner through preparation until the server observes a download request.
Browsers that block multiple downloads have individual save links and a bounded
wait. Removing staged files frees server disk space without undoing restored data.
The matching report distinguishes matched, missing and ambiguous records and
library albums absent from the backup. See the main README for format, limits,
retention, migration and encryption details.

Compilation and in-process tests do not require a running server:

```powershell
cargo test --offline -p server --bin phonolite-server admin::
cargo test --offline -p server --test source_api_and_admin_contract_test --test source_metadata_and_auth_contract_test
node --test web/tests/*.test.mjs
```

When the user runs the server, review Library, Users, Settings, Logs, Login, and Setup at narrow mobile and desktop widths. Check indexing issue details, missing artwork, empty/error states, long names/paths, keyboard-only dialogs, mobile navigation, spinners, search/filter/pagination history, Settings discard/save flows, and session expiry. Verify that cross-page navigation retains the document and that Back and Forward restore the selected view and filters. Browser visual verification requires a user-started instance; do not start an application or server as part of automated verification.
