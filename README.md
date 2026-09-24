# ⚠️ 项目已存档（ARCHIVED）

**官方桌面版已推出，本项目停止维护。**

请直接下载官方版：

- **macOS（Apple Silicon）**：https://download.deepseek.com/dsh-desk/bin/mac-arm64/deepseek-harness-0.1.7-rc.1.20260924.1-mac-arm64.dmg
- **Windows（x64）**：https://download.deepseek.com/dsh-desk/bin/win-x64/deepseek-harness-0.1.7-rc.1.20260924.1-win-x64.exe

感谢一路使用与反馈。历史版本（v0.1.0 – v0.3.15）仍可在 [Releases](https://github.com/danbao/dsh-desktop/releases) 页面下载，应用内自动更新不再有新版本。代码与 issue 归档保留，仅供参考。

---

# DSH Desktop（存档说明）

以下为存档时的项目文档，描述的是 v0.3.15 的行为，仅供参考。

以 [Tauri 2](https://tauri.app/) 封装 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) 的 macOS 桌面端：托管内核源码、随时拉取上游最新代码并从源码构建，工作台在系统浏览器中运行 `dsh` Web 界面。

参考 [hairyf/deepseek-harness-desktop](https://github.com/hairyf/deepseek-harness-desktop)（其下载预编译发行版）；本项目改为直接管理上游 git 仓库，始终跟随最新源码。

## 功能

- **内核托管** — 首次启动自动浅克隆 `deepseek-ai/deepseek-harness` 到应用数据目录（可自定义目录）。
- **随时更新** — 「检查更新」只 fetch 对比落后数；「更新代码并构建」拉取最新代码、按需执行 `pnpm install` / `pnpm run build`，完成后恢复更新前的服务状态（在跑则自动重启）。上游 force-push 也能通过 `reset --hard FETCH_HEAD` 跟进；更新时自动清理上游删包遗留的孤儿目录。
- **插件管理** — 独立插件页管理 `web` profile 的 npm 插件，支持版本检查、升级、重装和卸载；服务运行中会自动停启并恢复。
- **服务生命周期** — 以独立进程组启动 `dsh --profile web`（loopback 绑定）；新版 harness 的 token 鉴权自动适配（从启动输出捕获带 token 的工作台地址，健康检查接受任意 HTTP 响应）。停止 / 重启、异常退出与健康检查连续失败都会反映到状态与日志。
- **菜单栏常驻** — 关闭窗口即隐藏到 macOS 菜单栏，应用与服务继续运行；托盘菜单支持启动/停止服务、打开工作台（默认浏览器直开带 token 地址）、检查应用更新与退出。
- **工作台走浏览器** — harness 使用 `SameSite=Strict` 会话 cookie，内嵌 iframe 无法携带；工作台以默认浏览器打开，30 天会话内书签直达。
- **日志可追溯** — 控制台日志窗格之外，全部日志落盘 `logs/app.log`（256 MiB × 4 代际滚动，总量约 1 GiB），重启不丢、事后可查。
- **退出清理** — 进程终止（SIGTERM/SIGINT）都会杀死整个服务进程组，不残留 node 进程；托盘「退出」先优雅停服。

## 环境要求

- macOS（开发与打包均在 macOS 完成）
- [Rust](https://rustup.rs/) 与 Xcode Command Line Tools
- Node.js `^22.19 || >=24` 与 pnpm（构建 harness 内核用；支持 NVM、fnm、Volta、asdf/mise 与 Homebrew 自动发现，也可在应用内手动指定路径）
- 首次克隆需要网络

## 数据目录

```
~/Library/Application Support/com.danbao.dsh-desktop/
├── config.json     # { "port": 3080, "autostart": true }
├── harness/        # 托管的 deepseek-harness 浅克隆（路径可自定义）
├── logs/           # app.log 滚动日志（256 MiB × 4）
└── state/          # 构建标记（记录产物对应的 commit）
```

## 开发

```sh
pnpm install
pnpm tauri dev        # 开发模式（Vite + cargo）
```

指向本地已有的 harness 工作区调试（跳过克隆；构建标记按路径区分存放）：

```sh
DSH_DESKTOP_HARNESS_PATH=/path/to/deepseek-harness pnpm tauri dev
```

## 打包

```sh
pnpm tauri build      # 产出 .app 与 .dmg（src-tauri/target/release/bundle）
```

## 发布

推送 `v*` tag（如 `v0.1.0`）触发 [GitHub Actions](.github/workflows/release.yml)，在 macOS runner 上交叉编译 aarch64 与 x86_64 两个架构，自动创建 GitHub Release 并上传 `.app` / `.dmg` 安装包；也可在 Actions 页面手动指定 tag 触发。

应用内置自动更新（Tauri updater）：顶栏「检查更新」从 GitHub Releases 拉取带签名的更新包校验后安装重启，产物签名密钥为 minisign 密钥对（私钥在 repo secrets `TAURI_SIGNING_PRIVATE_KEY`，本地备份 `~/.tauri/dsh-desktop.key`，丢失则历史版本无法升级到新版本）。

## 架构

```
前端 (vanilla TS + Vite)
  控制台：环境 / 内核 / 服务 / 插件四张状态卡 + 日志窗格
  插件页：独立整页管理 web profile 插件
  工作台：服务就绪后由默认浏览器打开（带 token）
        │ invoke 命令 / listen 事件
Rust 后端 (src-tauri/src)
  commands.rs   get_state / sync_harness / update_harness /
                start_service / stop_service / set_config / open_workbench
  gitops.rs     浅克隆、fetch、behind 对比、reset --hard、孤儿包目录清理
  logfile.rs    滚动日志文件（256 MiB × 4 代际）
  pipeline.rs   needs_build 判定、pnpm install / build、构建标记
  plugins.rs    web profile 插件目录、registry 版本检查与官方 CLI 操作
  service.rs    进程组 spawn、监督线程（健康检查/意外退出检测）、
                停止与信号清理（SIGTERM/SIGINT → kill 组）、
                token 化工作台地址捕获
  snapshot.rs   全量状态快照，state-changed 事件广播
  paths.rs      应用数据目录、config.json、日志目录、按路径哈希的构建标记
  toolchain.rs  Node/pnpm/git 发现与手动配置（NVM/fnm/Volta/asdf/mise…）
  tray.rs       macOS 菜单栏托盘（状态行、启停服务、工作台、退出）
  util.rs       登录 shell PATH 解析、流式子进程日志、loopback 探针
```

要点：

- GUI 不继承终端的 PATH。应用会合并继承环境、用户登录/交互 shell（zsh、bash、fish 等）和常见版本管理器目录，验证后让所有子进程共享同一工具链环境；自动检测失败时可在环境卡中分别指定 Node 与 pnpm。
- 服务以 `process_group(0)` 启动，停止与清理都作用于整个进程组。
- 构建标记存在应用数据目录而非 harness 树内，避免污染用户工作区。
- npm registry 支持自定义配置，自动读取 `~/.npmrc`。

## 安全说明

`dsh` 具备本地代码执行能力；本应用仅绑定 loopback，鉴权依赖 harness 自带的 token + SameSite=Strict 会话 cookie。请在可信环境中使用。

## License

MIT
