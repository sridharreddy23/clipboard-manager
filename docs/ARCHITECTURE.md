# Architecture

This document records the technology evaluation and the resulting architecture for
`clipboard-manager`, a native Linux re-implementation of the Windows 10/11 Clipboard
History experience.

---

## 1. Technology evaluation

### 1.1 Candidates

| Option | Language | Toolkit |
|---|---|---|
| 1 | Rust | GTK4 + libadwaita |
| 2 | C++ | Qt 6 |
| 3 | Python | GTK4 / PyGObject |
| 4 | Rust | **GTK3 (gtk-rs)** |

### 1.2 Decision

> **Rust + GTK3 (`gtk-rs` 0.18) + SQLite (`rusqlite`, bundled), with raw X11
> (`x11rb`) and raw Wayland (`wayland-client`) platform layers.**

### 1.3 Why Rust

* Compiled, no runtime, no interpreter startup cost — the daemon must be cheap to
	launch during session login (§46) and cheap to keep resident (§45).
* No GC pauses, deterministic memory. Idle RSS is dominated by the toolkit, not by
	the language runtime.
* Memory safety matters here more than usual: this process handles untrusted
	clipboard payloads (arbitrary bytes from arbitrary MIME types, images from
	untrusted sources) in a long-lived background process. A parsing bug in C/C++
	would be a local privilege-escalation surface; in safe Rust it is an error value.
* `x11rb` and `wayland-client` are high-quality **pure-Rust** protocol
	implementations, so the X11 and Wayland layers do not need FFI shims and do not
	need `libxcb`/`libwayland` headers at build time.
* `rusqlite` with the `bundled` feature statically links SQLite with FTS5, which
	removes a runtime dependency on the distribution's `libsqlite3` version — a real
	concern when targeting Ubuntu 20.04 through 26.04 (§22).

Python was rejected: interpreter startup, higher idle RSS, and PyGObject adds a
large dependency chain to a `.deb` that must install cleanly with `apt` (§39).
C++/Qt was rejected primarily on memory-safety grounds for a process that parses
untrusted input continuously, and because Qt's `.deb` dependency footprint is
substantially larger than GTK's on a GNOME/GTK-based target (Ubuntu/Debian).
Electron was excluded by requirement §55.

### 1.4 Why GTK3 and not GTK4/libadwaita

This is the least obvious decision in the project, so it is justified in detail.

**(a) GTK4 cannot position a toplevel window. At all. On any backend.**

`gtk_window_move()` was removed in GTK4 with no replacement. GTK4 deliberately
exposes no API to place a toplevel at a chosen screen coordinate, because Wayland
does not allow it. The consequence is that on **X11** — where positioning *is*
possible and is what users expect — a GTK4 application still cannot open a popup
next to the mouse pointer or next to the focused window.

"Appear near the active window or cursor" is requirement §5, and it is central to
the Windows `Win+V` interaction model that §2 names as the primary UX reference.
GTK3 retains `gtk_window_move()`, `gtk_window_set_keep_above()` and
`GdkWindowTypeHint::PopupMenu`, all of which work correctly on X11. Choosing GTK4
would mean failing a core UX requirement on the backend where it is achievable.

**(b) Distribution reach (§22).**

| Release | GTK3 | GTK4 | libadwaita |
|---|---|---|---|
| Ubuntu 20.04 LTS | 3.24.20 | — | — |
| Debian 11 | 3.24.24 | — | — |
| Ubuntu 22.04 LTS | 3.24.33 | 4.6 | 1.1 |
| Debian 12 | 3.24.38 | 4.8 | 1.2 |
| Ubuntu 24.04 LTS | 3.24.41 | 4.14 | 1.5 |
| Debian 13 | 3.24.49 | 4.18 | 1.7 |
| Ubuntu 25.10 / 26.04 | 3.24.50+ | 4.20 | 1.8 |

GTK3 is present on **every** release in the five-year target window. GTK4 and
libadwaita are absent from Ubuntu 20.04 and Debian 11 entirely, so a GTK4 build
could not produce a `.deb` that installs on the oldest two targets without
bundling a parallel GTK stack.

**(c) libadwaita is actively counter-productive for this UI.**

libadwaita exists to enforce the GNOME *Adwaita* design language, and it hard-codes
its stylesheet. The brief (§6, §30) asks for a Windows 11-style panel: a specific
corner radius, a specific acrylic-like surface, a specific row metric and shadow.
That styling is written as CSS either way; with libadwaita we would be overriding a
stylesheet that is explicitly documented as not being an override target, and the
result would drift with every libadwaita release. The brief also requires the app to
look native on KDE, XFCE, Cinnamon and MATE (§21) — libadwaita makes it look like a
GNOME app everywhere.

**(d) What GTK3 costs us.**

GTK3 is in maintenance mode. It is not deprecated-and-removed: it ships in Ubuntu
26.04 and Debian 13, is still receiving releases, and a very large fraction of the
Linux desktop depends on it. The styling features we need (CSS, rounded corners via
an RGBA visual, box-shadow, transitions, `prefers-color-scheme`-equivalent via theme
detection) are all present in 3.24. HiDPI scaling and ATK accessibility work.

GTK3's own `GtkClipboard` is *not* used for monitoring (see §3), so GTK3's weaker
Wayland clipboard story does not affect us — the platform layer talks to the
display server directly.

**Conclusion:** GTK3 gives strictly more of the required behaviour (popup
positioning), strictly more distribution reach, and strictly more styling control.
GTK4's advantages (modern renderer, future-proofing) do not offset failing a core
interaction requirement.

---

## 2. Process and thread model

There is exactly **one** long-lived process per user session: the daemon. The GUI
and the CLI are not separate processes with their own clipboard monitors; the CLI is
a thin IPC client, and the GUI lives inside the daemon (§26, §27).

```
											 clipboard-managerd  (one per user session)
 ┌───────────────────────────────────────────────────────────────────────────┐
 │                                                                           │
 │  MAIN THREAD — GTK3 main loop                                             │
 │  ┌─────────────────────────────────────────────────────────────────────┐  │
 │  │ Core        : owns Store (SQLite), Settings, popup state            │  │
 │  │ ui::popup   : Windows-style clipboard panel                         │  │
 │  │ ui::settings: settings window                                       │  │
 │  │ event pump  : async_channel receiver -> Core::handle(Event)         │  │
 │  └─────────────────────────────────────────────────────────────────────┘  │
 │        ▲             ▲              ▲              ▲             ▲        │
 │        │ Event       │ Event        │ Event        │ Event       │        │
 │  ┌─────┴─────┐ ┌─────┴──────┐ ┌─────┴──────┐ ┌─────┴─────┐ ┌─────┴─────┐ │
 │  │ clipboard │ │ IPC server │ │ X11 hotkey │ │ portal /  │ │ tray      │ │
 │  │ monitor   │ │ (unix sock)│ │ XGrabKey   │ │ DE hotkey │ │ (SNI)     │ │
 │  │ thread    │ │ thread     │ │ thread     │ │ thread    │ │ thread    │ │
 │  └───────────┘ └────────────┘ └────────────┘ └───────────┘ └───────────┘ │
 │        │                                                                  │
 │  ┌─────▼──────────────────────────────────────────────────────────────┐   │
 │  │ platform::ClipboardBackend  (trait)                                │   │
 │  ├────────────────────────────┬───────────────────────────────────────┤   │
 │  │ platform::x11              │ platform::wayland                     │   │
 │  │  XFIXES SelectionNotify    │  zwlr_data_control_manager_v1         │   │
 │  │  (truly event-driven)      │  (truly event-driven)                 │   │
 │  └────────────────────────────┴───────────────────────────────────────┘   │
 └───────────────────────────────────────────────────────────────────────────┘
																			│
								 ┌────────────────────┴────────────────────┐
								 ▼                                        ▼
			$XDG_DATA_HOME/clipboard-manager/         $XDG_CONFIG_HOME/
				database.sqlite   (0600)                  clipboard-manager/
				blobs/<sha256>    (0600)                    config.toml  (0600)
				(dir 0700)
```

All SQLite access happens on the main thread, inside the GTK loop, behind no lock —
the `Connection` is owned by `Core`. Worker threads never touch the database. Large
payloads (images, file lists) are hashed and written to the **blob store** on the
monitor thread *before* the event is posted, so the main thread only ever inserts a
small row. This is what keeps the UI responsive under a 50 MB screenshot paste
(§33, §45).

Channels are `async_channel` + `glib::spawn_future_local`, which integrates with the
GTK main context without the deprecated `glib::MainContext::channel`.

---

## 3. Clipboard monitoring

Polling is forbidden (§9). Both supported backends are genuinely event-driven.

### 3.1 X11 — XFIXES

`XFixesSelectSelectionInput(root, CLIPBOARD, SetSelectionOwnerNotifyMask)` makes the
X server send a `XFixesSelectionNotifyEvent` whenever the CLIPBOARD owner changes.
The monitor thread blocks in `wait_for_event()`. Idle cost is zero.

On notify we run an ICCCM conversion against a dedicated, unmapped 1×1 window:
`TARGETS` first, then the best available target in preference order
(§3.3). `INCR` transfers are supported for payloads larger than the maximum
request size.

PRIMARY (middle-click selection) is monitored only when
`clipboard.monitor_primary` is enabled; it is **off** by default and stored
separately, so PRIMARY is never confused with CLIPBOARD (§19).

### 3.2 Wayland — `zwlr_data_control_manager_v1`

This is the only Wayland protocol that lets an **unfocused** client observe
clipboard changes. It is implemented by KDE Plasma (≥ 5.25), Sway, Hyprland,
Wayfire, river, and other wlroots-based compositors. We bind it directly with
`wayland-client` — no `wl-paste` subprocess, no polling.

### 3.3 Wayland on GNOME — the real limitation

**Mutter (GNOME) does not implement `zwlr_data_control_manager_v1`, and GNOME has
not shipped any replacement protocol.** Under a GNOME Wayland session there is no
sanctioned Wayland mechanism for a background process to read the clipboard.

We do not work around this by asking the user to weaken their session (§54). Instead
we use the compositor's **own** X11 bridge: mutter runs Xwayland and actively keeps
the X11 `CLIPBOARD` selection synchronised with the Wayland clipboard in both
directions. The daemon therefore connects to the Xwayland display as an ordinary
X11 client and uses the XFIXES path from §3.1. Clipboard changes made by
Wayland-native applications *are* observed, because mutter mirrors them into X11.

This is detected automatically at startup:

```
session = Wayland
	├── zwlr_data_control_manager_v1 advertised  → native Wayland backend
	├── else, $DISPLAY (Xwayland) reachable      → X11/XFIXES backend via Xwayland
	└── else                                     → monitoring unavailable;
																								 the daemon still runs, serves
																								 stored history, and reports
																								 `degraded` from `status`
```

The capability actually in effect is always reported by
`clipboard-manager status` and shown in Settings → Advanced. The application never
claims the feature works when it does not (§60).

### 3.4 Normalisation

A clipboard change produces a set of offered MIME types. We pick one representation
and classify it:

| Class | Preferred MIME order |
|---|---|
| Files | `text/uri-list`, `x-special/gnome-copied-files`, `x-special/nautilus-clipboard` |
| Image | `image/png`, `image/webp`, `image/jpeg`, `image/tiff`, `image/bmp`, `image/gif` |
| Text  | `text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`, `STRING` |

Files are checked before images because file managers offer both a URI list and a
thumbnail. A text payload that parses as a single `http(s)`/`ftp`/`mailto` URI is
re-classified as `Url` for display purposes but stored as text, so pasting is
byte-identical.

The original MIME type is recorded and is what we re-offer when the item is
restored, so round-tripping does not degrade the data (§52).

---

## 4. Global shortcut: `Ctrl + \``

`GlobalShortcut` is a trait with several implementations tried in order. The first
one that *verifiably* registers wins, and the method in effect is reported to the
user.

```
shortcuts::Manager
 ├── 1. x11::X11Shortcut        — X11 sessions only. XGrabKey on the root window.
 ├── 2. portal::PortalShortcut  — org.freedesktop.portal.GlobalShortcuts (Wayland)
 ├── 3. kde::KdeShortcut        — KGlobalAccel / kglobalshortcutsrc (KDE Wayland)
 ├── 4. gnome::GnomeShortcut    — GSettings custom-keybinding (GNOME Wayland)
 ├── 5. xfce::XfceShortcut      — xfconf-query command keybinding
 └── 6. Manual                  — notify + document `clipboard-manager toggle`
```

### 4.1 Keyboard-layout correctness (§3)

We never assume a keycode. We read the server keymap and collect **every** keycode
that produces the `grave` keysym in **any** group/level, then grab all of them. This
makes `Ctrl+\`` work on layouts where backtick is not on the key above Tab (e.g.
several non-US layouts), and it keeps working after a layout switch because we also
listen for `MappingNotify` and re-grab.

Each keycode is grabbed with the cross-product of the lock modifiers
(`Num Lock`, `Caps Lock`, `Scroll Lock`) so the shortcut fires regardless of lock
state — the standard X11 idiom.

### 4.2 Wayland

Wayland deliberately has no protocol for a client to grab a key globally; that is a
security property, not a bug (§54). The portal interface
`org.freedesktop.portal.GlobalShortcuts` is the sanctioned replacement and is used
when the running portal implementation provides it. When it does not, we register
the shortcut through the desktop environment's own configuration (options 3–5),
which binds the **CLI** command:

```
clipboard-manager toggle
```

This is the same mechanism a user would use by hand, performed for them. If even
that fails we surface a notification and documentation rather than failing silently.

---

## 5. Storage

SQLite (`bundled`, FTS5) in WAL mode.

```sql
clipboard_items(
	id INTEGER PRIMARY KEY,
	kind TEXT NOT NULL,              -- text | url | image | files
	mime_type TEXT NOT NULL,         -- the MIME we will re-offer
	content_hash TEXT NOT NULL UNIQUE,
	text_content TEXT,               -- inline, for text/url
	blob_hash TEXT,                  -- -> blobs/<sha256>, for image/files
	preview TEXT NOT NULL,           -- what the list shows; always searchable
	metadata TEXT,                   -- JSON: dimensions, file names, source app
	size_bytes INTEGER NOT NULL,
	created_at INTEGER NOT NULL,     -- unix ms
	updated_at INTEGER NOT NULL,
	last_used_at INTEGER NOT NULL,
	is_pinned INTEGER NOT NULL DEFAULT 0
)
```

* `content_hash` is `sha256(kind || mime || payload)` and is **UNIQUE**, which makes
	duplicate suppression (§10) an `INSERT … ON CONFLICT DO UPDATE` that bumps
	`last_used_at` and preserves `is_pinned`. There is no read-then-write race.
* Blobs are content-addressed at `blobs/<sha256>` (§15). A blob is deleted only
	after a `NOT EXISTS` check against `clipboard_items.blob_hash`, so a blob shared
	by a pinned and an unpinned item survives (§48).
* `clipboard_items_fts` is an FTS5 external-content table over `preview` and
	`text_content`, kept in sync by triggers. Search is a single indexed FTS query,
	not a table scan, so it stays fast at 100 000 items (§47). Fuzzy ranking is
	applied *only* to the FTS candidate set.
* Retention, history-limit trimming, blob GC and `VACUUM` run on an idle timeout
	after startup, never during it (§46, §48).

Permissions: the data directory is created `0700`, the database and every blob
`0600`, and the config file `0600` (§18, §37). The process sets its umask to `0077`
before creating any of them so that even a mid-write crash cannot leave a
world-readable file.

---

## 6. IPC and single instance (§26, §27)

A `SOCK_STREAM` Unix domain socket at
`$XDG_RUNTIME_DIR/clipboard-manager/<display-id>.sock`, directory `0700`, socket
`0600`. `XDG_RUNTIME_DIR` is already per-user and `0700`, so the socket is not
reachable by other users and nothing is exposed on the network (§18).

Single-instance is enforced by an advisory `flock()` on a lock file next to the
socket, not by a PID file, so a crashed daemon releases it automatically. A second
`clipboard-manager` invocation detects the lock, forwards `show` over IPC, and
exits.

Messages are newline-delimited JSON (`ipc::protocol`). Each request is answered and
the connection closed; there is no long-lived subscription, which keeps the server a
simple blocking accept loop in one thread.

---

## 7. Privacy (§16, §17)

* No network code of any kind is linked into the binary. There is no HTTP client, no
	telemetry, and no account.
* Clipboard **content is never logged**, at any log level. The logging layer has no
	path that can receive a payload; call sites pass only `kind`, `size_bytes` and a
	hash prefix. `--debug` raises verbosity, not sensitivity.
* `privacy::secrets` applies conservative heuristics to drop likely credentials
	before storage when "Do not store passwords" is on: offered MIME types used by
	password managers (`x-kde-passwordManagerHint`,
	`application/x-keepassxc-password`), plus shape-based detection of API-key-like
	and PEM-like payloads.
* Source-application exclusion is best effort and is documented as such. On X11 we
	resolve the selection owner window to a PID via `_NET_WM_PID` and read
	`/proc/<pid>/{comm,exe}`; `WM_CLASS` is used as a fallback. Under Wayland,
	`zwlr_data_control` does **not** identify the source client and there is no
	protocol that does, so exclusion by application cannot work there. The Settings UI
	states this inline rather than offering a control that silently does nothing.

---

## 8. Module map

```
src/
	main.rs            entry point; CLI dispatch vs daemon start
	cli/               clap definitions, IPC client commands, export/import
	config/            Settings (serde + TOML), defaults, atomic save
	model/             ClipKind, ClipItem, ClipPayload, Metadata
	storage/           Store, migrations, blobs, maintenance, import/export
	search/            FTS5 query builder + fuzzy re-ranking
	clipboard/         ClipboardEvent, normalisation, monitor supervisor
	platform/          session detection; x11/ and wayland/ backends; source app
	shortcuts/         GlobalShortcut trait, keys parsing, 6 implementations
	ipc/               protocol, server, client, single-instance lock
	daemon/            Core: wires everything, owns Store + Settings
	ui/                popup, item card, settings window, dialogs, theme, css, tray
	privacy/           secret heuristics, application exclusion
	notify.rs          desktop notifications over D-Bus
	logging.rs         tracing -> journald/stderr
```

No file contains the whole application (§24).

---

## 9. Known limitations (carried into the README)

| Limitation | Cause | Mitigation in this project |
|---|---|---|
| No background clipboard monitoring on GNOME Wayland via a Wayland protocol | Mutter implements no `data-control` equivalent | Automatic Xwayland/XFIXES bridge (§3.3); reported by `status` |
| Popup cannot be positioned on Wayland | No Wayland protocol allows a client to place its own toplevel | Compositor-centred popup; X11 sessions get cursor-anchored placement |
| Global `Ctrl+\`` grab unavailable on Wayland | By design | Portal, then DE-level binding of `clipboard-manager toggle` |
| Source-application exclusion on Wayland | Protocol carries no client identity | Documented; control disabled with an explanation |
| Tray icon absent on stock GNOME | GNOME removed `StatusNotifierItem` support | Tray is optional; all functions reachable from popup/CLI. Works on Ubuntu, KDE, XFCE, Cinnamon, MATE |
