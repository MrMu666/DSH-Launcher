// Uses the Tauri global API (enabled via `app.withGlobalTauri: true`),
// so no bundler / npm runtime dependency is required on the frontend.
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;

const logEl = document.getElementById("log");
const statusDot = document.getElementById("status-dot");
const statusText = document.getElementById("status-text");
const refreshBtn = document.getElementById("refresh-btn");

// Window controls (native title bar is hidden via `decorations: false`).
const { getCurrentWindow } = window.__TAURI__.window;
const appWindow = getCurrentWindow();
const maxBtn = document.getElementById("max-btn");
const maxIcon = document.getElementById("max-icon");
const restoreIcon = document.getElementById("restore-icon");

async function updateMaxIcon() {
  const maximized = await appWindow.isMaximized();
  maxIcon.style.display = maximized ? "none" : "";
  restoreIcon.style.display = maximized ? "" : "none";
  maxBtn.title = maximized ? "还原" : "最大化";
  maxBtn.setAttribute("aria-label", maxBtn.title);
}

document.getElementById("min-btn").addEventListener("click", () => appWindow.minimize());
maxBtn.addEventListener("click", async () => {
  await appWindow.toggleMaximize();
  await updateMaxIcon();
});
document.getElementById("close-btn").addEventListener("click", () => appWindow.close());
// Keep the maximize/restore icon in sync when maximized via the drag region's
// double-click or the OS (Win+↑ etc.).
window.addEventListener("resize", () => updateMaxIcon());

// --- Address dropdown (top-bar centre) --------------------------------
const DEFAULT_URL = "127.0.0.1:3080";
const STORAGE_KEY = "dsh-urls";
const urlSelect = document.getElementById("url-select");
const urlAdd = document.getElementById("url-add");
const urlDel = document.getElementById("url-del");
const urlInput = document.getElementById("url-input");
let urlEditing = false;

function loadUrls() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [DEFAULT_URL];
    const arr = JSON.parse(raw);
    if (!Array.isArray(arr)) return [DEFAULT_URL];
    const cleaned = arr
      .filter((s) => typeof s === "string" && s.trim().length > 0)
      .map((s) => s.trim());
    // Default is always present and always first; dedupe the rest.
    return [...new Set([DEFAULT_URL, ...cleaned.filter((s) => s !== DEFAULT_URL)])];
  } catch {
    return [DEFAULT_URL];
  }
}

function saveUrls(urls) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(urls));
  } catch (err) {
    // Storage unavailable — the list just won't persist this run.
    console.warn("保存地址失败:", err);
  }
}

function renderUrls(urls, selected) {
  urlSelect.innerHTML = "";
  for (const u of urls) {
    const opt = document.createElement("option");
    opt.value = u;
    opt.textContent = u;
    urlSelect.appendChild(opt);
  }
  urlSelect.value = selected;
  urlDel.disabled = selected === DEFAULT_URL;
}

function rememberUrl(url) {
  try {
    localStorage.setItem("dsh-last-url", url);
  } catch {}
}

// Set the webview target without opening it yet (used at startup; the webview
// will spawn there once DSH is ready).
async function applyUrl(url) {
  rememberUrl(url);
  try {
    await invoke("set_dsh_url", { url });
  } catch (err) {
    setStatus("error", "地址设置失败");
    appendLine("✘ 地址设置失败：" + String(err));
  }
}

// Open an address in the DSH webview right now (creates the webview on demand,
// so a remote ip:3080 works even without local DSH).
async function openUrl(url) {
  rememberUrl(url);
  try {
    await invoke("open_dsh_url", { url });
  } catch (err) {
    setStatus("error", "地址打开失败");
    appendLine("✘ 地址打开失败：" + String(err));
  }
}

function showUrlInput() {
  urlEditing = true;
  urlSelect.hidden = true;
  urlInput.hidden = false;
  urlAdd.textContent = "✓";
  urlAdd.title = "确认添加";
  urlAdd.setAttribute("aria-label", "确认添加");
  urlInput.focus();
}

function hideUrlInput() {
  urlEditing = false;
  urlInput.hidden = true;
  urlSelect.hidden = false;
  urlAdd.textContent = "＋";
  urlAdd.title = "添加地址";
  urlAdd.setAttribute("aria-label", "添加地址");
}

function confirmAddUrl() {
  const value = urlInput.value.trim();
  hideUrlInput();
  urlInput.value = "";
  if (!value) return;
  const urls = loadUrls();
  if (!urls.includes(value)) {
    urls.push(value);
    saveUrls(urls);
  }
  renderUrls(urls, value);
  openUrl(value);
}

urlAdd.addEventListener("click", () => (urlEditing ? confirmAddUrl() : showUrlInput()));
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") confirmAddUrl();
  else if (e.key === "Escape") {
    hideUrlInput();
    urlInput.value = "";
  }
});
urlSelect.addEventListener("change", () => {
  const selected = urlSelect.value;
  urlDel.disabled = selected === DEFAULT_URL;
  openUrl(selected);
});
urlDel.addEventListener("click", () => {
  const selected = urlSelect.value;
  if (selected === DEFAULT_URL) return; // default is protected
  const urls = loadUrls().filter((u) => u !== selected);
  saveUrls(urls);
  renderUrls(urls, DEFAULT_URL);
  openUrl(DEFAULT_URL);
});

// Initialise the dropdown with remembered addresses, restoring the last used one.
const storedLast = (() => {
  try {
    return localStorage.getItem("dsh-last-url");
  } catch {
    return null;
  }
})();
const initialUrl = loadUrls().includes(storedLast) ? storedLast : DEFAULT_URL;
renderUrls(loadUrls(), initialUrl);
applyUrl(initialUrl);

function appendLine(text) {
  const div = document.createElement("div");
  div.className = "line";
  div.textContent = text == null ? "" : String(text);
  logEl.appendChild(div);
  logEl.scrollTop = logEl.scrollHeight;
}

function setStatus(state, text) {
  statusDot.className = "status-dot " + state;
  statusText.textContent = text;
}

async function boot() {
  try {
    await listen("dsh-output", (event) => appendLine(event.payload));

    await listen("dsh-ready", () => {
      setStatus("ready", "服务已就绪，网页已打开");
      appendLine("");
      appendLine("✔ 检测到 127.0.0.1:3080 开始正常服务");
      appendLine("→ 网页已在顶栏下方打开");
      refreshBtn.disabled = false;
    });

    await listen("dsh-exit", (event) => {
      const code = event.payload;
      setStatus("error", "进程已退出（code: " + (code == null ? "—" : code) + "）");
      appendLine("✘ npx 进程已退出（code=" + (code == null ? "未知" : code) + "）");
      refreshBtn.disabled = true;
    });

    refreshBtn.addEventListener("click", async () => {
      try {
        await invoke("force_reload");
      } catch (err) {
        setStatus("error", "刷新失败");
        appendLine("✘ 刷新失败：" + String(err));
      }
    });

    appendLine("$ npx @deepseek-ai/dsh web");
    appendLine("");

    // Check the local environment before starting: npx and the DSH package.
    const deps = await invoke("check_deps");

    if (!deps.npxOk) {
      setStatus("error", "未检测到 npx");
      appendLine("✘ 未检测到 npx，请先安装 Node.js（https://nodejs.org）后重启启动器");
      appendLine("  才能使用本机 127.0.0.1:3080");
      appendLine("  也可以在上方地址栏选择 / 添加其他机器的 ip:3080");
      if (urlSelect.value !== DEFAULT_URL) openUrl(urlSelect.value);
      return;
    }
    if (!deps.dshOk) {
      setStatus("error", "未检测到 @deepseek-ai/dsh");
      appendLine("✘ 未检测到 @deepseek-ai/dsh 包，请先手动安装后重启启动器：");
      appendLine("    npm install -g @deepseek-ai/dsh");
      appendLine("  即可使用本机 127.0.0.1:3080");
      appendLine("  也可以在上方地址栏选择 / 添加其他机器的 ip:3080");
      if (urlSelect.value !== DEFAULT_URL) openUrl(urlSelect.value);
      return;
    }

    setStatus("starting", "正在启动 DSH…");
    await invoke("start_dsh");
  } catch (err) {
    setStatus("error", "启动失败");
    appendLine("");
    appendLine("✘ 启动失败：" + String(err));
  }
}

boot();
