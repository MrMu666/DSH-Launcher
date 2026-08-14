# DSH Launcher

> **中文** · [English](README.en.md)

一个用 [Tauri 2](https://tauri.app) 编写的 **DSH 桌面启动器**：一键拉起本机 `@deepseek-ai/dsh` Web 服务，并在常驻顶栏下方展示 DSH 网页；没有本机 DSH 时，也能直接连接其他机器的 `ip:3080`。

![icon](icon-source.svg)

## 功能特性

- 🚀 **一键启动**：启动时自动检查 `npx` 与 `@deepseek-ai/dsh` 包；缺失时在终端里提示手动安装，不静默失败
- 🖥️ **模拟终端**：实时滚动显示 DSH 子进程的 stdout / stderr，就绪后自动打开网页
- 🧭 **常驻顶栏**（无系统标题栏）：可按住拖动、双击最大化，右上角有最小化 / 最大化 / 关闭按钮
- 🔗 **地址下拉框**（顶栏中间）：默认 `127.0.0.1:3080`（不可删除），可添加并记住其他机器的 `ip:3080`，选择即切换打开，下次启动恢复上次使用的地址
- ⟳ **强制刷新**：顶栏右侧刷新按钮可随时重载网页
- 🛡️ **进程树管理**：Windows 下用 Job Object 保证启动器无论正常 / 异常退出（含强杀、崩溃），DSH 子进程都随之终止，不残留孤儿 node 进程
- ⚙️ **跨平台 CI**：内置 GitHub Actions workflow，push / 打 tag 自动构建 Windows、macOS、Linux 安装包

## 环境要求

- [Node.js](https://nodejs.org)（含 `npx`，位于 `PATH` 中）
- [Rust](https://rustup.rs)（含 `cargo`）
- Windows 下需要 [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)（Win10/11 通常已自带）

## 开发运行

```bash
npm install
npm run dev        # 等价于 tauri dev（debug 模式）
```

## 构建

```bash
npm run build       # 完整打包（Windows 生成 NSIS/MSI 安装包）
npm run build:exe   # 仅编译可执行文件，跳过安装包（tauri build --no-bundle）
```

构建产物位于 `src-tauri/target/release/`。

### 用 CI 出三平台安装包

项目内置 [GitHub Actions](.github/workflows/release.yml)：

```bash
git push origin main        # 触发构建，产物挂在 Actions run 上
git tag v0.1.0 && git push origin v0.1.0   # 打 tag 后自动生成三平台安装包 + 草稿 Release
```

## 使用说明

- **顶栏**：左侧标题，中间地址下拉框，右侧 ⟳ 刷新 + 最小化 / 最大化 / 关闭。
- **地址下拉框**：
  - 默认 `127.0.0.1:3080`，**不可删除**；
  - 点 **＋** 输入其他机器的地址（如 `192.168.1.50:3080` 或 `http://...`），回车确认即添加并打开，之后会记住该地址；
  - 点 **－** 删除当前选中的非默认地址；
  - 下次启动自动恢复上次使用的地址。
- **连接远端**：即使本机没装 `@deepseek-ai/dsh`，也能在地址栏添加并打开其他机器的 `ip:3080`。
- **强制刷新**：DSH 网页显示时点击 ⟳ 重新加载（未就绪时按钮禁用）。

## 工作原理

- **多 WebView 外壳**：主 WebView（顶栏 + 模拟终端）永不跳走；DSH 网页位于其下方的子 WebView，顶栏控件因此始终可用。
- **启动检查**：`check_deps` 用 `npx --version` 判断 npx；用离线文件系统检查 `@deepseek-ai/dsh`（npx 缓存 `_npx` 目录 / 全局安装目录），不依赖网络。
- **子进程**：`start_dsh` 用 `std::process::Command` 拉起 `npx @deepseek-ai/dsh web`（Windows 走 `cmd /C`），stdout/stderr 逐行通过 `dsh-output` 事件推送到终端。
- **就绪检测**：输出中出现 `127.0.0.1:3080` 时，后端发出一次 `dsh-ready`，短暂延迟后创建子 WebView 并加载目标地址。
- **地址切换**：`open_dsh_url` 把目标写入状态并**按需创建子 WebView 后导航**，地址列表与最近使用项持久化在 localStorage。
- **进程树**：Windows 用 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）保证子进程与启动器同生共死。

## 目录结构

```
dsh-start/
├── frontend/             # 前端（纯 HTML/JS/CSS，无打包器）
│   ├── index.html
│   └── src/
│       ├── main.js       # 事件监听、终端渲染、地址栏、窗口控制
│       └── styles.css
├── src-tauri/            # Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   └── src/
│       ├── main.rs
│       └── lib.rs        # 子进程、Job Object、check_deps、WebView 管理
├── .github/workflows/    # 跨平台 CI 构建
├── icon-source.svg       # 图标源文件（可用 tauri icon 重新生成）
└── package.json
```

## 说明

- 就绪检测基于终端输出中的 `127.0.0.1:3080` 字符串；若该端口已被其它进程占用，DSH 会报错退出，启动器会在终端显示错误。
- 首次运行时 `npx` 可能需要联网下载 `@deepseek-ai/dsh` 包，耗时视网络而定。
