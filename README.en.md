# DSH Launcher

> [中文](README.md) · **English**

A **DSH desktop launcher** built with [Tauri 2](https://tauri.app): it starts the local `@deepseek-ai/dsh` web service with one click and shows the DSH page below a persistent top bar. When no local DSH is available, you can still connect to another machine's `ip:3080`.

<img src="icon-source.svg" width="307" height="307" alt="DSH Launcher icon">

## Features

- 🚀 **One-click start** — checks for `npx` and the `@deepseek-ai/dsh` package at startup; if missing, prompts you to install manually instead of failing silently
- 🖥️ **Simulated terminal** — streams the DSH child process stdout/stderr in real time, then opens the web page once ready
- 🧭 **Frameless top bar** — drag anywhere to move, double-click to maximize, with minimize / maximize / close buttons on the right
- 🔗 **Address dropdown** (top-bar centre) — defaults to `127.0.0.1:3080` (cannot be removed); add and remember other machines' `ip:3080`, switch on selection, and it restores the last used address on next launch
- ⟳ **Force refresh** — reload the page anytime from the top bar (disabled while DSH is not ready)
- 🔄 **Restart DSH** — when the page is the local `127.0.0.1:3080`, a red ▷ button appears left of the refresh button; click it (with a confirmation dialog) to restart dsh-web in one go
- 📌 **Tray resident** — closing the window keeps the app running in the background (DSH keeps serving); only quitting from the tray icon exits the app
- 🛡️ **Process-tree management** — on Windows a Job Object guarantees the DSH child process dies with the launcher no matter how it exits (including force-kill or crash), leaving no orphaned node processes
- ⚙️ **Cross-platform CI** — a built-in GitHub Actions workflow builds Windows, macOS, and Linux installers on push / tag

## Install & use (no dependencies needed)

Just download the installer / executable for your platform and run it — **Node.js and Rust are NOT required to use the app**.

- Windows: run the NSIS / MSI installer, or run `dsh-launcher.exe` directly
- macOS / Linux: run the corresponding installer
- On Windows, [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/) is required (usually pre-installed on Win10/11)

## Development & building (dependencies needed)

The following are **only for developing and building from source**, unrelated to installing the app:

- [Node.js](https://nodejs.org) (with `npx` on `PATH`)
- [Rust](https://rustup.rs) (with `cargo`)
- On Windows, [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/) is needed to run the dev build (usually pre-installed on Win10/11)

## Development

```bash
npm install
npm run dev        # equivalent to tauri dev (debug mode)
```

## Build

```bash
npm run build       # full bundle (NSIS/MSI installers on Windows)
npm run build:exe   # executable only, no installer (tauri build --no-bundle) — the default deliverable
```

Artifacts are written to `src-tauri/target/release/`. The default deliverable is the single-file `dsh-launcher.exe`; `npm run build` (NSIS/MSI installers) is only for when an installer is needed.

### Build all three platforms via CI

The repository ships a [GitHub Actions](.github/workflows/release.yml) workflow:

```bash
git push origin main        # triggers a build; artifacts land on the Actions run
git tag v0.1.0 && git push origin v0.1.0   # on a tag, creates a draft release with installers
```

## Usage

- **Top bar**: title on the left, address dropdown in the middle, ⟳ refresh + minimize / maximize / close on the right.
- **Address dropdown**:
  - Defaults to `127.0.0.1:3080`, **cannot be deleted**;
  - Click **＋** to add another address (e.g. `192.168.1.50:3080` or `http://...`), press Enter to confirm — it is added and opened, and remembered afterwards;
  - Click **－** to delete the currently selected non-default address;
  - On the next launch the last used address is restored.
- **Remote connections**: even without `@deepseek-ai/dsh` installed locally, you can add and open another machine's `ip:3080`.
- **Force refresh**: click ⟳ while the DSH page is shown to reload it (disabled until ready).
- **Restart DSH**: only when the current page is `127.0.0.1:3080` does a red ▷ button appear left of the refresh button. Clicking it asks for confirmation to restart dsh-web; on confirm it kills and restarts DSH, the terminal shows the new startup log, and the page re-opens once ready.
- **Tray resident**: the window close button only hides the window — the app keeps running in the background (DSH keeps serving). Right-click the whale tray icon → "Show main window" or "Exit"; **only "Exit" in the tray menu quits the program**.

## How it works

- **Multi-WebView shell**: the main WebView (top bar + simulated terminal) never navigates away; the DSH page lives in a child WebView below it, so the top-bar controls always stay available.
- **Startup check**: `check_deps` probes `npx --version` for npx and does an offline filesystem check for `@deepseek-ai/dsh` (npx `_npx` cache / global install dir) — no network dependency.
- **Child process**: `start_dsh` spawns `npx @deepseek-ai/dsh web` via `std::process::Command` (through `cmd /C` on Windows); stdout/stderr are streamed line by line to the terminal via the `dsh-output` event.
- **Readiness detection**: once `127.0.0.1:3080` appears in the output, the backend emits a single `dsh-ready`, then after a short delay creates the child WebView and loads the target address.
- **Address switching**: `open_dsh_url` stores the target and navigates after creating the child WebView on demand; the address list and last-used item persist in localStorage.
- **Process tree**: on Windows a Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) makes the child process die together with the launcher.

## Project structure

```
dsh-start/
├── frontend/             # frontend (plain HTML/JS/CSS, no bundler)
│   ├── index.html
│   └── src/
│       ├── main.js       # events, terminal rendering, address bar, window controls
│       └── styles.css
├── src-tauri/            # Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   └── src/
│       ├── main.rs
│       └── lib.rs        # child process, Job Object, check_deps, WebView management
├── .github/workflows/    # cross-platform CI build
├── icon-source.svg       # icon source (regenerate with tauri icon)
└── package.json
```

## Notes

- Readiness detection matches the `127.0.0.1:3080` string in the terminal output; if that port is already taken by another process, DSH exits with an error shown in the terminal.
- On first run `npx` may need to download the `@deepseek-ai/dsh` package; the time depends on your network.
