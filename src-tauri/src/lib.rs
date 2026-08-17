use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Url, WebviewUrl, WindowEvent,
};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW: run console-subsystem children without a visible window.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The substring that signals the DSH web server is (about to be) serving.
const READY_MARKER: &str = "127.0.0.1:3080";
const EVENT_OUTPUT: &str = "dsh-output";
const EVENT_READY: &str = "dsh-ready";
const EVENT_EXIT: &str = "dsh-exit";
/// Emitted when the currently-selected webview is shown or finishes loading,
/// so the frontend can hide its "loading" overlay.
const EVENT_PAGE_SHOWN: &str = "dsh-page-shown";

/// DSH web UI URL, shown in the child webview below the persistent top bar.
const DSH_URL: &str = "http://127.0.0.1:3080";
/// Prefix for child-webview labels, one per address (so every page keeps its
/// instance and cache until the user manually refreshes).
const WEBVIEW_PREFIX: &str = "dsh-";
/// Height (logical px) of the launcher top bar. Must match `frontend/src/styles.css`.
const TOP_BAR_HEIGHT: f64 = 36.0;
/// Wait after the ready marker before creating the child webview, so the
/// freshly-started server has a moment to actually accept requests.
const READY_NAV_DELAY: Duration = Duration::from_millis(700);

struct AppState {
    child: Arc<Mutex<Option<Child>>>,
    ready: Arc<AtomicBool>,
    /// Address the DSH webview should show (defaults to `DSH_URL`). Changed by
    /// the top-bar dropdown before/after the webview is created.
    target_url: Arc<Mutex<String>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            ready: Arc::new(AtomicBool::new(false)),
            target_url: Arc::new(Mutex::new(DSH_URL.to_string())),
        }
    }
}

/// Holds the tray icon so it stays alive for the whole app lifetime
/// (dropping the handle removes the icon from the system tray).
/// The field is intentionally never read — merely owning it keeps it alive.
#[allow(dead_code)]
struct TrayHandle(TrayIcon);

/// Kill-on-close Job Object: guarantees the whole child process tree dies
/// together with the launcher, no matter how the launcher exits (graceful
/// close, `taskkill /F`, or a crash).
///
/// The job handle is leaked on purpose so it stays open for the entire
/// process lifetime. When the launcher process dies, the OS closes the last
/// handle, which destroys the job and terminates every process in it.
#[cfg(target_os = "windows")]
mod job_object {
    use std::{
        ffi::c_void,
        os::windows::io::AsRawHandle,
        process::Child,
        ptr,
        sync::OnceLock,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::GetCurrentProcess,
        },
    };

    struct LeakedHandle(HANDLE);
    // SAFETY: the handle is only ever read; it is intentionally never closed
    // (leaked) so the job persists for the whole process lifetime.
    unsafe impl Send for LeakedHandle {}
    unsafe impl Sync for LeakedHandle {}

    static JOB: OnceLock<LeakedHandle> = OnceLock::new();

    /// Creates the job and assigns the launcher process itself to it, so every
    /// child spawned afterwards automatically joins the same job.
    pub fn init() {
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let _ = CloseHandle(job);
                return;
            }
            // If assigning ourselves fails (e.g. we were already placed inside
            // another job), keep the job anyway and assign children explicitly.
            let _ = AssignProcessToJobObject(job, GetCurrentProcess());
            let _ = JOB.set(LeakedHandle(job));
        }
    }

    /// Adds a freshly spawned child to the kill-on-close job. No-op if the job
    /// is unavailable or the child already joined it via inheritance.
    pub fn assign(child: &Child) {
        if let Some(LeakedHandle(job)) = JOB.get() {
            unsafe {
                let _ = AssignProcessToJobObject(*job, child.as_raw_handle() as HANDLE);
            }
        }
    }
}

fn build_command() -> Command {
    #[cfg(target_os = "windows")]
    {
        // On Windows, `npx` is a .cmd shim that must be run through cmd.exe.
        // CREATE_NO_WINDOW keeps the child (and its descendants) from opening
        // a visible console window.
        let mut cmd = Command::new("cmd");
        cmd.arg("/C")
            .arg("npx @deepseek-ai/dsh web")
            .creation_flags(CREATE_NO_WINDOW);
        cmd
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = Command::new("npx");
        cmd.args(["@deepseek-ai/dsh", "web"]);
        cmd
    }
}

/// Deterministic, stable label for a child webview hosting a given address.
fn webview_label_for(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for b in url.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{WEBVIEW_PREFIX}{h:016x}")
}

/// Whether a webview label belongs to one of the address-hosted child webviews.
fn is_dsh_webview(label: &str) -> bool {
    label.starts_with(WEBVIEW_PREFIX)
}

/// Hides every address-hosted child webview (used when switching addresses,
/// when DSH exits, or during a restart) so the launcher's terminal / loading
/// overlay becomes visible. Instances are kept alive, not destroyed.
fn hide_all_webviews(app: &AppHandle) {
    for (label, webview) in app.webviews() {
        if is_dsh_webview(&label) {
            let _ = webview.hide();
        }
    }
}

/// Creates (hidden, below the persistent top bar) a child webview for `url`
/// if one does not already exist. It shows itself once its page finishes
/// loading and then emits `dsh-page-shown`. `add_child` must run off the main
/// thread (it blocks on `run_on_main_thread` + channel receive).
fn ensure_webview(app: &AppHandle, url: &str) -> Result<(), String> {
    let label = webview_label_for(url);
    if app.get_webview(&label).is_some() {
        return Ok(());
    }
    let Some(window) = app.get_window("main") else {
        return Err("无主窗口".into());
    };

    let label_c = label.clone();
    let url_c = url.to_string();
    let app_c = app.clone();
    std::thread::spawn(move || {
        let Some(parsed) = url_c.parse::<Url>().ok() else {
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let logical: LogicalSize<f64> = window.inner_size().unwrap_or_default().to_logical(scale);

        let builder =
            tauri::webview::WebviewBuilder::new(label_c.clone(), WebviewUrl::External(parsed))
                .on_page_load(move |webview, payload| {
                    if payload.event() == tauri::webview::PageLoadEvent::Finished {
                        let _ = webview.show();
                        let _ = app_c.emit(EVENT_PAGE_SHOWN, ());
                    }
                });
        if let Ok(webview) = window.add_child(
            builder,
            LogicalPosition::new(0.0, TOP_BAR_HEIGHT),
            LogicalSize::new(logical.width, (logical.height - TOP_BAR_HEIGHT).max(0.0)),
        ) {
            // Keep it hidden while its page loads; the launcher shows a
            // "loading" overlay until `dsh-page-shown` fires.
            let _ = webview.hide();
        }
    });
    Ok(())
}

/// Recomputes every address-hosted child webview's bounds so they always fill
/// the area below the top bar.
fn relayout_dsh_webview(window: &tauri::Window) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let logical: LogicalSize<f64> = window.inner_size().unwrap_or_default().to_logical(scale);
    for webview in window.webviews() {
        if is_dsh_webview(webview.label()) {
            let _ = webview.set_position(LogicalPosition::new(0.0, TOP_BAR_HEIGHT));
            let _ = webview.set_size(LogicalSize::new(
                logical.width,
                (logical.height - TOP_BAR_HEIGHT).max(0.0),
            ));
        }
    }
}

/// Hides every address-hosted child webview (e.g. when the DSH process exits),
/// so the terminal log and error status become visible again.
fn hide_dsh_webviews(app: &AppHandle) {
    hide_all_webviews(app);
}

/// Ensures the webview for the currently selected address exists (called when
/// DSH becomes ready, so the page appears over the terminal).
fn spawn_target_webview(app: &AppHandle) {
    let target = app
        .state::<AppState>()
        .target_url
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| DSH_URL.to_string());
    let _ = ensure_webview(app, &target);
}

/// Reads lines from a child pipe and forwards them to the frontend.
/// Emits `dsh-ready` exactly once when the ready marker is observed, and
/// schedules the DSH webview to appear shortly after.
fn stream_reader<R>(app: AppHandle, reader: R, ready: Arc<AtomicBool>)
where
    R: BufRead + Send + 'static,
{
    thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    let _ = app.emit(EVENT_OUTPUT, text.clone());
                    if text.contains(READY_MARKER) && !ready.swap(true, Ordering::SeqCst) {
                        let _ = app.emit(EVENT_READY, ());
                        let nav_app = app.clone();
                        thread::spawn(move || {
                            thread::sleep(READY_NAV_DELAY);
                            spawn_target_webview(&nav_app);
                        });
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Spawns a fresh `npx @deepseek-ai/dsh web` child, wires up its exit monitor
/// and output streams. Shared by `start_dsh` and `restart_dsh`.
fn spawn_dsh_child(app: &AppHandle, state: &State<'_, AppState>) -> Result<(), String> {
    let mut cmd = build_command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        format!("无法启动 npx（请确认已安装 Node.js 且 npx 位于 PATH 中）: {e}")
    })?;

    // Best-effort: if the launcher itself couldn't be put into the job (e.g.
    // it was already placed inside another job), add the child explicitly so
    // it still dies with the launcher.
    #[cfg(target_os = "windows")]
    job_object::assign(&child);

    let stdout = child.stdout.take().ok_or("无法读取子进程 stdout")?;
    let stderr = child.stderr.take().ok_or("无法读取子进程 stderr")?;

    *state.child.lock().map_err(|_| "状态锁异常".to_string())? = Some(child);

    // Monitor thread: detects child exit and notifies the frontend.
    let child_arc = state.child.clone();
    let exit_app = app.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(400));
        let mut guard = match child_arc.lock() {
            Ok(g) => g,
            Err(_) => break,
        };
        let child = match guard.as_mut() {
            Some(c) => c,
            None => break,
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                *guard = None;
                drop(guard);
                let _ = exit_app.emit(EVENT_EXIT, code);
                hide_dsh_webviews(&exit_app);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                *guard = None;
                drop(guard);
                let _ = exit_app.emit(EVENT_EXIT, None::<i32>);
                hide_dsh_webviews(&exit_app);
                break;
            }
        }
    });

    // Stream both stdout and stderr into the simulated terminal.
    stream_reader(app.clone(), BufReader::new(stdout), state.ready.clone());
    stream_reader(app.clone(), BufReader::new(stderr), state.ready.clone());

    Ok(())
}

#[tauri::command]
fn start_dsh(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    {
        let guard = state.child.lock().map_err(|_| "状态锁异常".to_string())?;
        if guard.is_some() {
            return Err("DSH 进程已在运行中".into());
        }
    }
    spawn_dsh_child(&app, &state)
}

/// Closes the DSH child webview so the terminal becomes visible, without
/// touching the running dsh-web process. Called before showing the restart
/// confirmation.
#[tauri::command]
async fn begin_restart(app: AppHandle) -> Result<(), String> {
    hide_dsh_webviews(&app);
    Ok(())
}

/// Hides the main window to the tray without emitting a close request.
///
/// The top bar's close button calls this instead of `window.close()`, because
/// on Windows a real close request destroys the main WebView2 (wry wires
/// WebView2's `WindowCloseRequested` to destroy the window), which would leave
/// the tray-restored window without its chrome.
#[tauri::command]
fn hide_to_tray(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())
    } else if let Some(w) = app.get_window("main") {
        w.hide().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Kills the running dsh-web child process (and only its own process tree —
/// nothing else the launcher runs), then starts a fresh one. The DSH webview
/// is closed so the terminal log shows the new startup output; the page
/// re-opens once `127.0.0.1:3080` appears again.
#[tauri::command]
async fn restart_dsh(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let had_child = {
        let mut guard = state.child.lock().map_err(|_| "状态锁异常".to_string())?;
        match guard.take() {
            Some(mut child) => {
                drop(guard);
                terminate_child(&mut child);
                true
            }
            None => false,
        }
    };
    if !had_child {
        return Err("DSH 未在运行".into());
    }

    // The old child was taken out of state, so the old exit-monitor thread
    // breaks silently (no spurious `dsh-exit`). Reset readiness, hide every
    // page so the terminal becomes visible again, and drop the target page so
    // it re-spawns fresh once the restarted DSH is ready. Other addresses'
    // pages are kept alive (hidden).
    state.ready.store(false, Ordering::SeqCst);
    let target = state
        .target_url
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| DSH_URL.to_string());
    hide_all_webviews(&app);
    if let Some(wv) = app.get_webview(&webview_label_for(&target)) {
        let _ = wv.close();
    }
    spawn_dsh_child(&app, &state)
}

/// Force-reloads the currently selected webview from the top bar's refresh
/// button. The webview is hidden first so the launcher's "loading" overlay
/// shows; `on_page_load` (Finished) re-shows it and emits `dsh-page-shown`.
#[tauri::command]
fn force_reload(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let target = state
        .target_url
        .lock()
        .map_err(|_| "状态锁异常".to_string())?
        .clone();
    let webview = app
        .get_webview(&webview_label_for(&target))
        .ok_or("网页尚未加载")?;
    let _ = webview.hide();
    webview
        .eval("window.location.reload(true)")
        .map_err(|e| e.to_string())
}

/// Prepends a scheme when missing, so `127.0.0.1:3080` becomes a navigable URL.
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// Validates a dropdown address and stores it as the DSH webview target.
fn resolve_target(state: &State<'_, AppState>, url: &str) -> Result<Url, String> {
    let normalized = normalize_url(url);
    let parsed = Url::parse(&normalized).map_err(|e| format!("地址无效：{e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("仅支持 http/https 地址".into());
    }
    *state.target_url.lock().map_err(|_| "状态锁异常".to_string())? = normalized;
    Ok(parsed)
}

/// Remembers the address the dropdown points at (used at startup, before the
/// webview exists). The DSH webview spawns there once DSH is ready.
#[tauri::command]
fn set_dsh_url(state: State<'_, AppState>, url: String) -> Result<(), String> {
    resolve_target(&state, &url).map(|_| ())
}

/// Opens the given address in the DSH webview now. Creates the child webview
/// on demand, so connecting to a remote machine's `ip:3080` works even when
/// the local DSH isn't installed.
#[tauri::command]
async fn open_dsh_url(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
) -> Result<bool, String> {
    let _ = resolve_target(&state, &url)?;
    let normalized = state
        .target_url
        .lock()
        .map_err(|_| "状态锁异常".to_string())?
        .clone();
    let label = webview_label_for(&normalized);

    // Switching addresses: hide every page (the old one disappears right away;
    // the launcher shows a "loading" overlay until `dsh-page-shown` fires).
    hide_all_webviews(&app);

    if let Some(webview) = app.get_webview(&label) {
        // Page already exists — restore its preserved instance instantly.
        let _ = webview.show();
        let _ = webview.set_focus();
        let _ = app.emit(EVENT_PAGE_SHOWN, ());
        return Ok(true);
    }

    // New address — create its webview (hidden until the page finishes
    // loading; then it shows itself and emits `dsh-page-shown`).
    ensure_webview(&app, &normalized)?;
    Ok(false)
}

// ---------------------------------------------------------------------------
// Persistent app config (visited addresses + last used), stored as a JSON file
// in the per-user cache directory — the mainstream "cache folder in the user
// directory" approach. The WebView2 profile (HTTP cache, cookies, per-page
// localStorage) already lives in the same AppData folder and survives restarts.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AppConfig {
    urls: Vec<String>,
    last_url: Option<String>,
}

fn config_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("无法获取缓存目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建缓存目录：{e}"))?;
    Ok(dir.join("config.json"))
}

fn read_config(app: &AppHandle) -> AppConfig {
    let Ok(path) = config_path(app) else {
        return AppConfig::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_config(app: &AppHandle, config: &AppConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[tauri::command]
fn load_addresses(app: AppHandle) -> Vec<String> {
    read_config(&app).urls
}

#[tauri::command]
fn save_addresses(app: AppHandle, urls: Vec<String>) -> Result<(), String> {
    let mut config = read_config(&app);
    config.urls = urls;
    write_config(&app, &config)
}

#[tauri::command]
fn load_last_url(app: AppHandle) -> Option<String> {
    read_config(&app).last_url
}

#[tauri::command]
fn save_last_url(app: AppHandle, url: String) -> Result<(), String> {
    let mut config = read_config(&app);
    config.last_url = Some(url);
    write_config(&app, &config)
}

fn run_output(cmdline: &str) -> Option<std::process::Output> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmdline).creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmdline);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
}

fn run_ok(cmdline: &str) -> bool {
    run_output(cmdline)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn stdout_of(cmdline: &str) -> Option<String> {
    run_output(cmdline).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Whether `@deepseek-ai/dsh` is available to npx: either cached under the npm
/// `_npx` directory by a previous run, or installed globally. Offline check.
fn dsh_package_installed() -> bool {
    let cache_root = stdout_of("npm config get cache")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            #[cfg(target_os = "windows")]
            {
                std::env::var("LOCALAPPDATA").ok().map(|p| {
                    std::path::Path::new(&p)
                        .join("npm-cache")
                        .to_string_lossy()
                        .into_owned()
                })
            }
            #[cfg(not(target_os = "windows"))]
            {
                None
            }
        });
    if let Some(root) = cache_root {
        let npx_dir = std::path::Path::new(&root).join("_npx");
        if let Ok(entries) = std::fs::read_dir(&npx_dir) {
            for entry in entries.flatten() {
                let pkg = entry
                    .path()
                    .join("node_modules")
                    .join("@deepseek-ai")
                    .join("dsh");
                if pkg.is_dir() {
                    return true;
                }
            }
        }
    }
    if let Some(root) = stdout_of("npm root -g") {
        let pkg = std::path::Path::new(root.trim())
            .join("@deepseek-ai")
            .join("dsh");
        if pkg.is_dir() {
            return true;
        }
    }
    false
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DepsStatus {
    npx_ok: bool,
    dsh_ok: bool,
}

/// Startup environment check: reports whether `npx` and the `@deepseek-ai/dsh`
/// package are available locally, so the UI can prompt for a manual install
/// instead of failing silently.
#[tauri::command]
fn check_deps() -> DepsStatus {
    let npx_ok = run_ok("npx --version");
    DepsStatus {
        npx_ok,
        dsh_ok: npx_ok && dsh_package_installed(),
    }
}

#[cfg(target_os = "windows")]
fn terminate_child(child: &mut Child) {
    // Kill the whole process tree so no orphaned node/server survives.
    let pid = child.id();
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

#[cfg(not(target_os = "windows"))]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Shows the main window from the tray, un-minimizing and focusing it.
///
/// Note: after a prevented window close on Windows, the *main webview* is
/// dropped from Tauri's manager while the OS window (and any child webviews)
/// survive. Fall back to the raw `Window` in that case — showing it still
/// brings the whole UI back.
fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    } else if let Some(w) = app.get_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "windows")]
    job_object::init();

    let app = tauri::Builder::default()
        // Single instance: re-running the exe while the app is already running
        // (e.g. hidden to the tray) focuses the existing window instead of
        // starting a second instance — so previously-visited pages are reused,
        // not reloaded.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            start_dsh,
            begin_restart,
            restart_dsh,
            force_reload,
            set_dsh_url,
            open_dsh_url,
            check_deps,
            hide_to_tray,
            load_addresses,
            save_addresses,
            load_last_url,
            save_last_url
        ])
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().ok_or("缺少窗口图标")?)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("DSH 启动器")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    match event {
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                        | TrayIconEvent::DoubleClick {
                            button: MouseButton::Left,
                            ..
                        } => show_main_window(tray.app_handle()),
                        _ => {}
                    }
                })
                .build(app)?;
            // Keep the tray alive for the whole app lifetime.
            app.manage(TrayHandle(tray));
            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                WindowEvent::Resized(_) => relayout_dsh_webview(window),
                WindowEvent::CloseRequested { api, .. } => {
                    // Closing hides to the tray; the app keeps running in the
                    // background. Only the tray menu's "退出" quits the app.
                    api.prevent_close();
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            let state = app_handle.state::<AppState>();
            let mut guard = state.child.lock().unwrap();
            if let Some(mut child) = guard.take() {
                drop(guard);
                terminate_child(&mut child);
            }
        }
    });
}
