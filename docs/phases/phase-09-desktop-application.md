# Phase 9: Desktop application

[All phases](README.md) · [← Phase 8: Performance, query language and analysis](phase-08-performance-query-language-and-analysis.md) · [Phase 10: Hardening →](phase-10-hardening.md)

**Status:** done, in five steps.

Grafeno as a desktop app with its own installers, a new design and a graph you can handle directly.

Steps:

- [9.1 Namespaces as things you create (done)](#91-namespaces-as-things-you-create-done)
- [9.2 Graph lab design system (done)](#92-graph-lab-design-system-done)
- [9.3 A graph you can handle (done)](#93-a-graph-you-can-handle-done)
- [9.4 Desktop application (Tauri) (done)](#94-desktop-application-tauri-done)
- [9.5 Installers and icons (done)](#95-installers-and-icons-done)

The guidelines (2026-11-04): a desktop application with `.exe` and Linux
installers; a complete redesign with new typography and an avant-garde
look; a graph that moves (click, drag freely, interact with its nodes);
namespaces that behave predictably; and the Grafeno icon on every
executable and installer. Shell: Tauri (Rust, the engine runs inside the
app). Direction: *graph lab*. Each step ends with tests, documentation and
its own commit.

## 9.1 Namespaces as things you create (done)

Before, a namespace existed only
while it holds a node: the Studio's picker "opens" a name without creating
anything, so an empty `demo` disappears from the list as soon as another
one is chosen, and choosing `demo` again cannot show whether it was
created or only switched to. Namespaces become records of their own.

- A `namespaces` table (layout version 6) with the name, creation time
  and an optional description; the upgrade registers every namespace that
  has data. Writing to an unknown namespace still registers it, so
  libraries and imports keep working; exports carry namespace records
  (export version 2), so empty namespaces survive a backup.
- `ReadTx::namespaces` lists registered namespaces, empty ones included,
  with their counts. `WriteTx::create_namespace` (a `Conflict` error if it
  exists; names without `/`, `\`, `?`, `#`, `%`, control characters or
  outer spaces, up to 128 characters), `describe_namespace` and
  `drop_namespace` (deletes everything in it, after a confirmation in
  every UI). Renaming is left out: an export imported under another name
  does it.
- HTTP `POST /v1/namespaces` (409 `conflict` if it exists), `GET`,
  `PATCH` and `DELETE /v1/namespaces/{ns}`; the shell (`\ns create`,
  `\ns drop NAME --yes`; `\use` says when a namespace does not exist
  yet); GQ `SHOW NAMESPACES`, which MCP agents reach through `query`.
- The Studio separates *switching* (a list of every namespace, the current
  one marked) from *creating* (an explicit "New namespace" with a name,
  which fails visibly if it exists), and says which happened.

## 9.2 Graph lab design system (done)

A full redesign of the Studio, which the
desktop app shares.

- Dark first (with a light theme), the graph canvas filling the window
  and the tools in floating panels with blur; a command bar instead of the
  top bar; views as workspaces on the left rail.
- Typography: Space Grotesk for the interface and headings, JetBrains Mono
  for data, ids and code; both bundled (SIL OFL), so the desktop app works
  offline.
- New tokens (color, space, radius, elevation, motion), components rebuilt
  on them (buttons, inputs, menus, tables, dialogs, toasts, inspector), and
  the dataviz palette re-validated for both themes.
- Every view redone on it: overview, explorer, graph, scripts, search,
  memory, administration.

As built: one stylesheet of tokens (`studio/src/styles.css`); dark is the
default theme (`data-theme` is `dark`, `light` or `system`). The rail and
the command bar float as glass over a floor of two soft glows and a dot
grid; the graph view runs under both, its toolbar, inspector and status
floating over the canvas. Interface text in Space Grotesk, data, ids,
code, table heads and section labels in JetBrains Mono, both from
`@fontsource-variable` packages bundled in the build. Nodes get a halo in
their own color. The chart palette kept its slots: the validator passes
both themes on the new surfaces (in light, three slots stay under 3:1
against the surface, relieved by the legend and the table view).

## 9.3 A graph you can handle (done)

Cytoscape's static layouts give way to a
live physics simulation (d3-force) drawn on a canvas of its own.

- Drag any node and the rest reacts; release to let it settle, or pin it
  where it is (double-click unpins). The simulation cools down when
  nothing moves and wakes up on any change.
- Hover lights a node's neighborhood; click selects and opens the
  inspector; double-click expands its neighbors, which grow out of it;
  lasso and shift-click select several nodes to move, pin, hide or delete
  together; a context menu on nodes and edges (expand, connect, pin, hide,
  delete, end a fact).
- Connect two nodes by dragging from a node's handle onto another; edges
  drawn with arrows, labels on hover, curved when parallel.
- Zoom and pan with inertia, fit, a minimap, search to fly to a node;
  labels and colors by label, community or property; sizes by centrality
  (phase 8.5); clusters can be collapsed into one node.
- Smooth at a few thousand nodes on screen (canvas, spatial index for
  hits, labels only when legible).

As built: `studio/src/graph/engine.ts` is a framework-free engine (one
canvas, d3-force for links, charge, collision and a weak pull to the
center); the graph view and the result graph of Scripts both use it, and
Cytoscape is gone. Dragging pins what was moved (a dot marks it), and the
simulation reheats while dragging and cools after. Hover and a single
selection light the node's neighborhood and dim the rest; shift-click,
ctrl-click and shift-drag (lasso) select several, which then move,
pin, hide or delete together. A selected node shows a `+` handle: dragging
it onto another node opens the new-fact form with both ends. Right-click
opens a menu for a node, several nodes, a fact or the background.
Parallel facts curve apart, self-loops loop, facts that ended are dashed,
labels are drawn in screen space and only when legible. Zoom follows the
cursor, panning keeps its momentum, fitting keeps clear of the floating
panels, and a minimap shows and moves the view. With communities on
(8.5), a community collapses into one node with its count, and facts in
and out of it merge into one edge per pair; double-click expands it.
Hit testing scans the nodes and edges under the pointer (linear, fine at
the few thousand nodes a view draws). Keys: Delete, Esc, F (frame), H
(hide), P (pin), + and -.

## 9.4 Desktop application (Tauri) (done)

A new crate, `grafeno-desktop`
(`apps/desktop`), wraps the Studio in a native window.

- Local mode: the app opens a database file (create, open, recent files)
  and runs the engine inside its own process, serving the HTTP API on a
  random loopback port with a random token that only its window knows; the
  window loads the Studio from it. No service or terminal needed.
- Remote mode: connect to a Grafeno server (the installed service or
  another machine) with its URL and token; saved connections.
- Native menus and shortcuts, file dialogs for import and export, one
  instance per database, window size and position kept, links opened in
  the system browser.

As built (`apps/desktop`, see its README): the window loads the Studio
from Tauri's bundled assets, which keeps one origin (and the Studio's
saved settings and scripts) across launches. Instead of a loopback port,
the Studio's `fetch` goes through one app command, `request`: addresses
under `http://grafeno.local` reach the open database's HTTP router in the
same process (`tower::ServiceExt::oneshot`), and other addresses are sent
by the app, so remote servers need no CORS for it and nothing listens on
the machine. Opening a file holds redb's lock, so a file open in a service
or another program is refused with a clear message. One app instance
(tauri-plugin-single-instance): a second launch focuses the window and
opens its file there. Recent databases live in the app's config folder.
`grafeno-server` gained a `studio` feature (on by default) so the app
does not embed the Studio twice.

## 9.5 Installers and icons (done)

One source for every icon: the graphene logo
rendered to `.ico` (16-256 px), `.png` sizes and the Linux icon theme.

- Desktop app installers from the Tauri bundler: Windows NSIS `.exe` (and
  `.msi`), Linux `.deb`, `.rpm` and `.AppImage`, each with the Grafeno
  icon on the installer, the executable, the shortcut, the task bar and the
  application menu (`.desktop` entry).
- The server packages keep their role (service, headless machines) and
  get the icon too: the setup `.exe` and `grafeno.exe` on Windows, a
  `.desktop` entry and icons in the `.deb` and `.rpm`.
- The release workflow builds both families on tags.

As built: `npm run icons` in `apps/desktop` renders `docs/assets/logo.svg`
with `tauri icon` and copies the results to `packaging/windows/grafeno.ico`
and `packaging/linux/icons/`. The app is named **Grafeno Desktop**
(package `grafeno-desktop`), so its Linux packages do not clash with the
server's `grafeno`; its NSIS setup installs for the current user without
administrator rights (the MSI is there for managed machines), and both
installers and the Linux packages register `.redb` files to open in it.
The server's `.deb` and `.rpm` add a **Grafeno Studio** menu entry
(`xdg-open` on the service's Studio) with the icon in the hicolor theme.
The release workflow gained `desktop-windows` and `desktop-linux` jobs
(Ubuntu 22.04, so the AppImage runs on older systems).

Phase 9 is complete.
