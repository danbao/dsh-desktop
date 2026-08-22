import './style.css'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

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
    ['pnpm', env.pnpmVersion ?? '未找到'],
    ['git', env.gitVersion ?? '未找到'],
    ...(env.problems.length > 0 ? ([['问题', env.problems.join('；')]] as Array<[string, string]>) : []),
  ])

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
  const running = service.status === 'running'
  startBtn.disabled = !env.ready || !harness.present || busy !== null || running
  stopBtn.disabled = busy !== null || service.status === 'stopped'
  syncBtn.disabled = busy !== null || !harness.present || !env.ready
  updateBtn.disabled = busy !== null || !env.ready
  restartBtn.disabled = busy !== null || !env.ready || !harness.present
  savePortBtn.disabled = busy !== null || service.status !== 'stopped'

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

function bind(): void {
  $('#btn-sync').addEventListener('click', () => void syncOnly())
  $('#btn-update').addEventListener('click', () => void update(false))
  $('#btn-update-restart').addEventListener('click', () => void update(true))
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
