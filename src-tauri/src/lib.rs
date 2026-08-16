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

/// DSH web UI URL, shown in the child webview below the persistent top bar.
const DSH_URL: &str = "http://127.0.0.1:3080";
/// Label of the child webview that hosts the DSH page.
const WEBVIEW_LABEL: &str = "dsh";
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

/// Adds the child webview that hosts the DSH page, below the persistent top bar.
/// No-op if the child webview already exists or the main window is gone.
/// Navigates to whatever address the top-bar dropdown currently points at.
fn spawn_dsh_webview(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    if window.get_webview(WEBVIEW_LABEL).is_some() {
        return;
    }

    let target = app
        .state::<AppState>()
        .target_url
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| DSH_URL.to_string());
    let Ok(url) = Url::parse(&target) else {
        return;
    };

    let scale = window.scale_factor().unwrap_or(1.0);
    let logical: LogicalSize<f64> = window.inner_size().unwrap_or_default().to_logical(scale);

    let builder =
        tauri::webview::WebviewBuilder::new(WEBVIEW_LABEL, WebviewUrl::External(url));
    let _ = window.add_child(
        builder,
        LogicalPosition::new(0.0, TOP_BAR_HEIGHT),
        LogicalSize::new(logical.width, (logical.height - TOP_BAR_HEIGHT).max(0.0)),
    );
}

/// Recomputes the child webview bounds so it always fills the area below the top bar.
fn relayout_dsh_webview(window: &tauri::Window) {
    let Some(webview) = window.get_webview(WEBVIEW_LABEL) else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let logical: LogicalSize<f64> = window.inner_size().unwrap_or_default().to_logical(scale);
    let _ = webview.set_position(LogicalPosition::new(0.0, TOP_BAR_HEIGHT));
    let _ = webview.set_size(LogicalSize::new(
        logical.width,
        (logical.height - TOP_BAR_HEIGHT).max(0.0),
    ));
}

/// Closes the child webview (e.g. when the DSH process exits), so the terminal
/// log and error status become visible again.
fn close_dsh_webview(app: &AppHandle) {
    if let Some(webview) = app.get_webview(WEBVIEW_LABEL) {
        let _ = webview.close();
    }
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
                            spawn_dsh_webview(&nav_app);
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
                close_dsh_webview(&exit_app);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                *guard = None;
                drop(guard);
                let _ = exit_app.emit(EVENT_EXIT, None::<i32>);
                close_dsh_webview(&exit_app);
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

/// Kills the running dsh-web child process (and only its own process tree —
/// nothing else the launcher runs), then starts a fresh one. The DSH webview
/// is closed so the terminal log shows the new startup output; the page
/// re-opens once `127.0.0.1:3080` appears again.
#[tauri::command]
fn restart_dsh(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
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
    // breaks silently (no spurious `dsh-exit`). Reset readiness and hide the
    // webview so the terminal becomes visible again.
    state.ready.store(false, Ordering::SeqCst);
    close_dsh_webview(&app);
    spawn_dsh_child(&app, &state)
}

/// Force-reloads the DSH webview from the top bar's refresh button.
#[tauri::command]
fn force_reload(app: AppHandle) -> Result<(), String> {
    let webview = app.get_webview(WEBVIEW_LABEL).ok_or("DSH 网页尚未加载")?;
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
) -> Result<(), String> {
    let parsed = resolve_target(&state, &url)?;

    if app.get_webview(WEBVIEW_LABEL).is_none() {
        // `add_child` blocks on `run_on_main_thread` + channel receive, so it
        // must never run on the main thread.
        let tapp = app.clone();
        std::thread::spawn(move || spawn_dsh_webview(&tapp))
            .join()
            .map_err(|_| "创建子 WebView 线程异常".to_string())?;
    }

    let webview = app.get_webview(WEBVIEW_LABEL).ok_or("子 WebView 创建失败")?;
    webview.navigate(parsed).map_err(|e| e.to_string())?;
    Ok(())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "windows")]
    job_object::init();

    let app = tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            start_dsh,
            restart_dsh,
            force_reload,
            set_dsh_url,
            open_dsh_url,
            check_deps
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
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
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
