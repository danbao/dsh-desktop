# DSH Desktop

以 [Tauri 2](https://tauri.app/) 封装 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) 的 macOS 桌面端：托管内核源码、随时拉取上游最新代码并从源码构建，在应用窗口内运行 `dsh` Web 界面。

参考 [hairyf/deepseek-harness-desktop](https://github.com/hairyf/deepseek-harness-desktop)（其下载预编译发行版）；本项目改为直接管理上游 git 仓库，始终跟随最新源码。

## 功能

- **内核托管** — 首次启动自动浅克隆 `deepseek-ai/deepseek-harness` 到应用数据目录。
- **随时更新** — 一键检查上游更新（fetch + 提交数对比）；「更新代码并构建」拉取最新代码后按需执行 `pnpm install` / `pnpm run build`；「更新并重启服务」完成后自动重启服务。上游 force-push 也能通过 `reset --hard FETCH_HEAD` 跟进。
- **服务生命周期** — 以独立进程组启动 `dsh --profile web`（loopback 绑定），健康检查就绪后才切换到内嵌界面；停止 / 重启、异常退出与健康检查连续失败都会反映到状态栏与日志。
- **内嵌界面** — 服务就绪后通过 iframe 加载 `http://127.0.0.1:<port>`，顶栏可随时切回控制台查看日志与操作。
- **退出清理** — 关窗或进程终止（SIGTERM/SIGINT）都会杀死整个服务进程组，不残留 node 进程。

## 环境要求

- macOS（开发与打包均在 macOS 完成）
- [Rust](https://rustup.rs/) 与 Xcode Command Line Tools
- Node.js `>=24` 与 pnpm（构建 harness 内核用）
- 首次克隆需要网络

## 数据目录

```
~/Library/Application Support/com.danbao.dsh-desktop/
├── config.json     # { "port": 3080, "autostart": true }
├── harness/        # 托管的 deepseek-harness 浅克隆
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
  控制台：环境 / 内核 / 服务三张状态卡 + 日志窗格
  就绪后 iframe 加载 dsh Web UI，可切回控制台
        │ invoke 命令 / listen 事件
Rust 后端 (src-tauri/src)
  commands.rs   get_state / sync_harness / update_harness /
                start_service / stop_service / set_config
  gitops.rs     浅克隆、fetch、behind 对比、reset --hard FETCH_HEAD
  pipeline.rs   needs_build 判定、pnpm install / build、构建标记
  service.rs    进程组 spawn、监督线程（健康检查/意外退出检测）、
                停止与信号清理（SIGTERM/SIGINT → kill 组）
  snapshot.rs   全量状态快照，state-changed 事件广播
  paths.rs      应用数据目录、config.json、按路径哈希的构建标记
  util.rs       登录 shell PATH 解析、流式子进程日志、loopback 探针
```

要点：

- GUI 不继承 shell 的 PATH，启动时经用户登录 shell 解析一次 PATH，所有子进程使用该结果定位 node/pnpm/git。
- 服务以 `process_group(0)` 启动，停止与清理都作用于整个进程组。
- 构建标记存在应用数据目录而非 harness 树内，避免污染用户工作区。

## 安全说明

`dsh` 具备本地代码执行能力；本应用仅绑定 loopback，不提供鉴权层。请在可信环境中使用。

## License

MIT
