import './style.css'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { check } from '@tauri-apps/plugin-updater'
import { relaunch } from '@tauri-apps/plugin-process'
import { open } from '@tauri-apps/plugin-dialog'

/** Snapshot of everything the UI renders, produced by the Rust `get_state`. */
interface Snapshot {
  harness: {
    path: string
    present: boolean
    commit: string | null
    shortCommit: string | null
    subject: string | null
    commitDate: string | null
    behind: number | null
    buildNeeded: boolean
  }
  env: {
    nodeVersion: string | null
    pnpmVersion: string | null
    gitVersion: string | null
    nodeBin: string | null
    pnpmBin: string | null
    gitBin: string | null
    nodeSource: string | null
    pnpmSource: string | null
    gitSource: string | null
    shell: string | null
    discoveryNotes: string[]
    configuredNodePath: string | null
    configuredPnpmPath: string | null
    ready: boolean
    problems: string[]
  }
  service: {
    status: 'stopped' | 'starting' | 'running' | 'error'
    port: number
    url: string
    error: string | null
  }
  busy: string | null
}

interface LogLine {
  source: string
  line: string
}

const $ = <T extends HTMLElement = HTMLElement>(sel: string): T => {
  const el = document.querySelector<T>(sel)
  if (el === null) throw new Error(`missing element ${sel}`)
  return el
}

let snap: Snapshot | null = null
let logCount = 0
let toolchainSaving = false
const LOG_CAP = 4000

function appendLog(entry: LogLine): void {
  const pane = $('#log-pane')
  const atBottom = pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 8
  const time = new Date().toLocaleTimeString('zh-CN', { hour12: false })
  const row = document.createTextNode(`[${time}] [${entry.source}] ${entry.line}\n`)
  pane.appendChild(row)
  logCount += 1
  if (logCount > LOG_CAP) {
    // Trim from the front; text nodes hold one line each.
    for (let i = 0; i < 500; i += 1) {
      pane.firstChild?.remove()
    }
    logCount -= 500
  }
  if (atBottom) pane.scrollTop = pane.scrollHeight
}

function toast(message: string, isError = false): void {
  const el = $('#toast')
  el.textContent = message
  el.classList.toggle('toast-error', isError)
  el.classList.remove('hidden')
  window.setTimeout(() => el.classList.add('hidden'), isError ? 6000 : 3500)
}

function kv(pairs: Array<[string, string]>): string {
  return pairs.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join('')
}

const STATUS_TEXT: Record<Snapshot['service']['status'], string> = {
  stopped: '已停止',
  starting: '启动中…',
  running: '运行中',
  error: '出错',
}

function pillClass(status: Snapshot['service']['status']): string {
  switch (status) {
    case 'running':
      return 'pill pill-ok'
    case 'starting':
      return 'pill pill-warn'
    case 'error':
      return 'pill pill-err'
    default:
      return 'pill pill-idle'
  }
}

function render(): void {
  if (snap === null) return
  const { env, harness, service, busy } = snap

  $('#status-pill').className = pillClass(service.status)
  $('#status-pill').textContent =
    STATUS_TEXT[service.status] + (busy !== null ? `（${busy}中…）` : '')

  $('#env-info').innerHTML = kv([
    ['Node', env.nodeVersion ?? '未找到'],
    ...(env.nodeBin !== null ? ([['Node 来源', `${env.nodeSource ?? '自动'} → ${env.nodeBin}`]] as Array<[string, string]>) : []),
    ['pnpm', env.pnpmVersion ?? '未找到'],
    ...(env.pnpmBin !== null ? ([['pnpm 来源', `${env.pnpmSource ?? '自动'} → ${env.pnpmBin}`]] as Array<[string, string]>) : []),
    ['git', env.gitVersion ?? '未找到'],
    ['登录 shell', env.shell ?? '未知'],
    ...(env.discoveryNotes.length > 0 ? ([['检测说明', env.discoveryNotes.join('；')]] as Array<[string, string]>) : []),
    ...(env.problems.length > 0 ? ([['问题', env.problems.join('；')]] as Array<[string, string]>) : []),
  ])
  $('#env-ready-dot').className = `env-dot ${env.ready ? 'ready' : 'problem'}`

  $('#harness-info').innerHTML = harness.present
    ? kv([
        ['路径', harness.path],
        ['当前提交', `${harness.shortCommit ?? '?'} ${harness.commitDate ?? ''}`],
        ['说明', harness.subject ?? ''],
        [
          '上游更新',
          harness.behind === null ? '未检查' : harness.behind > 0 ? `落后 ${harness.behind} 个提交` : '已是最新',
        ],
        ['构建状态', harness.buildNeeded ? '需要构建（代码有更新或产物缺失）' : '产物与代码一致'],
      ])
    : kv([['路径', harness.path], ['状态', '尚未克隆，点击「更新代码并构建」获取最新代码']])

  $('#service-info').innerHTML = kv([
    ['状态', STATUS_TEXT[service.status]],
    ['地址', service.url],
    ...(service.error !== null ? ([['错误', service.error]] as Array<[string, string]>) : []),
  ])

  const startBtn = $<HTMLButtonElement>('#btn-start')
  const stopBtn = $<HTMLButtonElement>('#btn-stop')
  const syncBtn = $<HTMLButtonElement>('#btn-sync')
  const updateBtn = $<HTMLButtonElement>('#btn-update')
  const restartBtn = $<HTMLButtonElement>('#btn-update-restart')
  const savePortBtn = $<HTMLButtonElement>('#btn-save-port')
  const refreshToolchainBtn = $<HTMLButtonElement>('#btn-refresh-toolchain')
  const saveToolchainBtn = $<HTMLButtonElement>('#btn-save-toolchain')
  const pickNodeBtn = $<HTMLButtonElement>('#btn-pick-node')
  const pickPnpmBtn = $<HTMLButtonElement>('#btn-pick-pnpm')
  const running = service.status === 'running'
  startBtn.disabled = !env.ready || !harness.present || busy !== null || running
  stopBtn.disabled = busy !== null || service.status === 'stopped'
  syncBtn.disabled = busy !== null || !harness.present || !env.ready
  updateBtn.disabled = busy !== null || !env.ready
  restartBtn.disabled = busy !== null || !env.ready || !harness.present
  savePortBtn.disabled = busy !== null || service.status !== 'stopped'
  refreshToolchainBtn.disabled = busy !== null || toolchainSaving
  saveToolchainBtn.disabled = busy !== null || running || service.status === 'starting' || toolchainSaving
  pickNodeBtn.disabled = running || service.status === 'starting' || toolchainSaving
  pickPnpmBtn.disabled = running || service.status === 'starting' || toolchainSaving
  $('#toolchain-running-hint').classList.toggle('hidden', service.status === 'stopped')

  const showApp = running && !$('#app-view').classList.contains('hidden')
  $('#console-view').classList.toggle('hidden', showApp)
  $('#app-view').classList.toggle('hidden', !running)
  $('#btn-console').classList.toggle('hidden', !running)
  $('#btn-app').classList.toggle('hidden', !(running && $('#console-view').classList.contains('hidden')))
  if (running) {
    const frame = $<HTMLIFrameElement>('#frame')
    if (!frame.src.startsWith('http://127.0.0.1') || !frame.src.includes(`:${service.port}`)) {
      frame.src = service.url
    }
  }
}

async function run(name: string, args?: Record<string, unknown>): Promise<void> {
  try {
    await invoke(name, args)
  } catch (err) {
    toast(`${name} 失败：${String(err)}`, true)
    appendLog({ source: 'ui', line: `${name} 失败：${String(err)}` })
  }
  await refresh()
}

async function refresh(): Promise<void> {
  try {
    snap = await invoke<Snapshot>('get_state')
  } catch (err) {
    toast(`读取状态失败：${String(err)}`, true)
    return
  }
  render()
}

function showToolchainSettings(show: boolean): void {
  const panel = $('#toolchain-settings')
  panel.classList.toggle('hidden', !show)
  const toggle = $<HTMLButtonElement>('#btn-toggle-toolchain')
  toggle.setAttribute('aria-expanded', String(show))
  toggle.textContent = show ? '收起设置' : '工具链设置'
  if (show && snap !== null) {
    $<HTMLInputElement>('#node-path-input').value = snap.env.configuredNodePath ?? ''
    $<HTMLInputElement>('#pnpm-path-input').value = snap.env.configuredPnpmPath ?? ''
  }
}

async function pickExecutable(inputSelector: string, title: string): Promise<void> {
  try {
    const selected = await open({ multiple: false, directory: false, title })
    if (typeof selected === 'string') {
      $<HTMLInputElement>(inputSelector).value = selected
    }
  } catch (err) {
    toast(`选择文件失败：${String(err)}`, true)
  }
}

async function saveToolchain(): Promise<void> {
  if (toolchainSaving) return
  toolchainSaving = true
  render()
  try {
    const nodePath = $<HTMLInputElement>('#node-path-input').value.trim() || null
    const pnpmPath = $<HTMLInputElement>('#pnpm-path-input').value.trim() || null
    await invoke('set_toolchain_config', { nodePath, pnpmPath })
    await refresh()
    showToolchainSettings(false)
    toast('工具链设置已保存并重新检测')
  } catch (err) {
    toast(`保存工具链失败：${String(err)}`, true)
    appendLog({ source: 'ui', line: `保存工具链失败：${String(err)}` })
  } finally {
    toolchainSaving = false
    render()
  }
}

async function refreshToolchain(): Promise<void> {
  try {
    await invoke('refresh_toolchain')
    await refresh()
    toast(snap?.env.ready === true ? '工具链检测完成' : '工具链检测完成，请查看环境问题', snap?.env.ready !== true)
  } catch (err) {
    toast(`重新检测失败：${String(err)}`, true)
    appendLog({ source: 'ui', line: `重新检测失败：${String(err)}` })
  }
}

async function syncOnly(): Promise<void> {
  await run('sync_harness')
  if (snap?.harness.behind != null) {
    toast(snap.harness.behind > 0 ? `上游有 ${snap.harness.behind} 个新提交` : '已经是最新代码')
  }
}

async function update(restart: boolean): Promise<void> {
  await run('update_harness', { restart })
  toast(restart ? '更新完成，服务已按需重启' : '更新并构建完成')
}

let appUpdateBusy = false

/** 应用本体更新：检查 GitHub Releases → 下载签名包 → 安装并重启。 */
async function checkAppUpdate(): Promise<void> {
  if (appUpdateBusy) return
  appUpdateBusy = true
  const btn = $<HTMLButtonElement>('#btn-check-update')
  btn.disabled = true
  try {
    const update = await check()
    if (update === null) {
      toast('应用已是最新版本')
      return
    }
    toast(`发现新版本 ${update.version}，正在下载…`)
    appendLog({ source: 'desktop', line: `应用更新：v${update.version}（${update.currentVersion} → ${update.version}）` })
    let received = 0
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case 'Started':
          received = 0
          break
        case 'Progress':
          received += event.data.chunkLength
          appendLog({ source: 'desktop', line: `下载中… ${Math.round(received / 1024)} KB` })
          break
        case 'Finished':
          appendLog({ source: 'desktop', line: '下载完成，安装并重启' })
          break
      }
    })
    await relaunch()
  } catch (err) {
    toast(`检查更新失败：${String(err)}`, true)
    appendLog({ source: 'desktop', line: `检查更新失败：${String(err)}` })
  } finally {
    appUpdateBusy = false
    btn.disabled = false
  }
}

function bind(): void {
  $('#btn-sync').addEventListener('click', () => void syncOnly())
  $('#btn-update').addEventListener('click', () => void update(false))
  $('#btn-update-restart').addEventListener('click', () => void update(true))
  $('#btn-check-update').addEventListener('click', () => void checkAppUpdate())
  $('#btn-refresh-toolchain').addEventListener('click', () => void refreshToolchain())
  $('#btn-toggle-toolchain').addEventListener('click', () => {
    showToolchainSettings($('#toolchain-settings').classList.contains('hidden'))
  })
  $('#btn-cancel-toolchain').addEventListener('click', () => showToolchainSettings(false))
  $('#btn-save-toolchain').addEventListener('click', () => void saveToolchain())
  $('#btn-pick-node').addEventListener('click', () => void pickExecutable('#node-path-input', '选择 Node 可执行文件'))
  $('#btn-pick-pnpm').addEventListener('click', () => void pickExecutable('#pnpm-path-input', '选择 pnpm 可执行文件'))
  $('#btn-start').addEventListener('click', () => void run('start_service'))
  $('#btn-stop').addEventListener('click', () => void run('stop_service'))
  $('#btn-save-port').addEventListener('click', async () => {
    const port = Number($<HTMLInputElement>('#port-input').value)
    if (!Number.isInteger(port) || port < 1024 || port > 65535) {
      toast('端口需在 1024–65535 之间', true)
      return
    }
    await run('set_config', { port })
    toast('端口已保存')
  })
  $('#btn-clear-log').addEventListener('click', () => {
    $('#log-pane').textContent = ''
    logCount = 0
  })
  $('#btn-console').addEventListener('click', () => {
    $('#app-view').classList.add('hidden')
    $('#console-view').classList.remove('hidden')
    $('#btn-app').classList.remove('hidden')
    $('#btn-console').classList.add('hidden')
  })
  $('#btn-app').addEventListener('click', () => {
    $('#console-view').classList.add('hidden')
    $('#app-view').classList.remove('hidden')
    $('#btn-console').classList.remove('hidden')
    $('#btn-app').classList.add('hidden')
  })
}

async function subscribe(): Promise<void> {
  await listen<LogLine>('log', (event) => appendLog(event.payload))
  await listen<Snapshot>('state-changed', (event) => {
    snap = event.payload
    render()
  })
}

void subscribe()
bind()
void refresh()

// Poll while a service is starting so the UI flips to the app view promptly.
window.setInterval(() => {
  if (snap?.service.status === 'starting') void refresh()
}, 1000)
