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

interface BackendLogEntry extends LogLine {
  id: number
  timestampMs: number
}

interface LogPage {
  entries: BackendLogEntry[]
  hasMore: boolean
}

type PluginAction = 'install' | 'update' | 'reinstall' | 'remove'

interface PluginInfo {
  packageName: string
  displayName: string
  description: string
  homepage: string | null
  requestedVersion: string | null
  installedVersion: string | null
}

interface PluginCatalog {
  profile: string
  plugins: PluginInfo[]
}

interface PluginUpdate {
  packageName: string
  latestVersion: string | null
  updateAvailable: boolean
  error: string | null
}

interface ManagePluginResult {
  catalog: PluginCatalog
  serviceRestarted: boolean
  message: string
}

interface DisplayLog extends LogLine {
  backendId: number | null
  localId: number | null
  timestampMs: number
}

const $ = <T extends HTMLElement = HTMLElement>(sel: string): T => {
  const el = document.querySelector<T>(sel)
  if (el === null) throw new Error(`missing element ${sel}`)
  return el
}

let snap: Snapshot | null = null
let logStick = true
let toolchainSaving = false
let activeView: 'console' | 'app' = 'console'
let previousServiceStatus: Snapshot['service']['status'] | null = null
let pluginCatalog: PluginCatalog | null = null
let pluginUpdates = new Map<string, PluginUpdate>()
let pluginPanelOpen = false
let pluginLoading = false
let pluginChecking = false
let pluginOperation = false
let pluginUpdatesChecked = false
let pluginError: string | null = null
let keepConsoleDuringServiceTransition = false
const LOG_CHUNK_SIZE = 250
const LOG_PAGE_SIZE = 2000
let logEntries: DisplayLog[] = []
let backendLogIds = new Set<number>()
let clearedBackendThrough = 0
let localLogId = 0
let logHydrating = true
let currentLogChunk: HTMLPreElement | null = null
let currentLogChunkSize = 0
// Incoming lines are batched and flushed once per animation frame: appending
// and scrolling per line forces a synchronous layout of the whole pane on
// every line, which stutters badly while a subprocess streams output.
let pendingLogText = ''
let pendingLogCount = 0
let logFlushScheduled = false

function appendLog(entry: LogLine): void {
  const displayEntry: DisplayLog = {
    ...entry,
    backendId: null,
    localId: ++localLogId,
    timestampMs: Date.now(),
  }
  logEntries.push(displayEntry)
  if (!logHydrating) appendRenderedLog(displayEntry)
}

function acceptBackendLog(entry: BackendLogEntry): void {
  if (entry.id <= clearedBackendThrough || backendLogIds.has(entry.id)) return
  backendLogIds.add(entry.id)
  const displayEntry: DisplayLog = {
    source: entry.source,
    line: entry.line,
    backendId: entry.id,
    localId: null,
    timestampMs: entry.timestampMs,
  }
  logEntries.push(displayEntry)
  if (!logHydrating) appendRenderedLog(displayEntry)
}

function formatLog(entry: DisplayLog): string {
  const time = new Date(entry.timestampMs).toLocaleTimeString('zh-CN', { hour12: false })
  return `[${time}] [${entry.source}] ${entry.line}\n`
}

function appendRenderedLog(entry: DisplayLog): void {
  pendingLogText += formatLog(entry)
  pendingLogCount += 1
  if (!logFlushScheduled) {
    logFlushScheduled = true
    requestAnimationFrame(flushPendingLogs)
  }
}

function flushPendingLogs(): void {
  logFlushScheduled = false
  if (pendingLogText === '') return
  const pane = $('#log-pane')
  // Only trust the at-bottom measurement while the pane is laid out: when the
  // console view is hidden every dimension reads 0, which would fake "at
  // bottom" and leave the scroll wedged at the top when shown again.
  if (pane.clientHeight > 0) {
    logStick = pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 8
  }
  if (currentLogChunk === null || currentLogChunkSize >= LOG_CHUNK_SIZE) {
    currentLogChunk = document.createElement('pre')
    currentLogChunk.className = 'log-chunk'
    currentLogChunk.appendChild(document.createTextNode(''))
    pane.appendChild(currentLogChunk)
    currentLogChunkSize = 0
  }
  const chunkText = currentLogChunk.firstChild as Text
  chunkText.appendData(pendingLogText)
  currentLogChunkSize += pendingLogCount
  pendingLogText = ''
  pendingLogCount = 0
  if (logStick) pane.scrollTop = pane.scrollHeight
}

// Re-pin the log pane to the newest line if we were following it. Needed when
// the pane becomes visible again: lines that arrived while hidden could not
// scroll the zero-size pane, so the position was left at the top.
function restickLog(): void {
  if (!logStick) return
  const pane = $('#log-pane')
  pane.scrollTop = pane.scrollHeight
}

function renderAllLogs(): void {
  // Queued lines are already in logEntries; the full re-render below covers
  // them, so drop the buffer to avoid appending them twice.
  pendingLogText = ''
  pendingLogCount = 0
  const pane = $('#log-pane')
  const fragment = document.createDocumentFragment()
  for (let start = 0; start < logEntries.length; start += LOG_CHUNK_SIZE) {
    const chunkEntries = logEntries.slice(start, start + LOG_CHUNK_SIZE)
    const chunk = document.createElement('pre')
    chunk.className = 'log-chunk'
    chunk.textContent = chunkEntries.map(formatLog).join('')
    fragment.appendChild(chunk)
  }
  pane.replaceChildren(fragment)
  currentLogChunk = pane.lastElementChild as HTMLPreElement | null
  currentLogChunkSize = logEntries.length % LOG_CHUNK_SIZE
  if (currentLogChunk !== null && currentLogChunkSize === 0) {
    currentLogChunkSize = LOG_CHUNK_SIZE
  }
  pane.scrollTop = pane.scrollHeight
}

async function hydrateLogs(): Promise<void> {
  let afterId: number | undefined
  try {
    while (true) {
      const page = await invoke<LogPage>('get_logs', { afterId, limit: LOG_PAGE_SIZE })
      for (const entry of page.entries) acceptBackendLog(entry)
      if (!page.hasMore || page.entries.length === 0) break
      afterId = page.entries[page.entries.length - 1]?.id
    }
  } finally {
    logEntries.sort((left, right) => {
      if (left.timestampMs !== right.timestampMs) return left.timestampMs - right.timestampMs
      return (left.backendId ?? Number.MAX_SAFE_INTEGER) - (right.backendId ?? Number.MAX_SAFE_INTEGER)
    })
    logHydrating = false
    renderAllLogs()
  }
}

/** Write text to the system clipboard, falling back to execCommand when the
 * async clipboard API is unavailable in the webview. */
async function writeClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    const area = document.createElement('textarea')
    area.value = text
    area.style.position = 'fixed'
    area.style.opacity = '0'
    document.body.appendChild(area)
    area.select()
    const ok = document.execCommand('copy')
    area.remove()
    return ok
  }
}

async function copyAllLogs(): Promise<void> {
  const text = logEntries.map(formatLog).join('')
  if (text === '') {
    toast('暂无日志可复制')
    return
  }
  const copied = await writeClipboard(text)
  if (copied) {
    toast(`已复制全部 ${logEntries.length} 行日志`)
  } else {
    toast('复制失败，请手动选择日志复制', true)
  }
}

async function clearLogs(): Promise<void> {
  const localCutoff = localLogId
  try {
    const backendCutoff = await invoke<number>('clear_logs')
    clearedBackendThrough = Math.max(clearedBackendThrough, backendCutoff)
    logEntries = logEntries.filter((entry) =>
      entry.backendId !== null ? entry.backendId > backendCutoff : (entry.localId ?? 0) > localCutoff,
    )
    backendLogIds = new Set(
      logEntries.flatMap((entry) => (entry.backendId === null ? [] : [entry.backendId])),
    )
    renderAllLogs()
  } catch (err) {
    toast(`清空日志失败：${String(err)}`, true)
    appendLog({ source: 'ui', line: `清空日志失败：${String(err)}` })
  }
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

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (char) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[char] ?? char)
}

function pluginActionsDisabled(): boolean {
  return pluginOperation
    || snap?.busy !== null
    || snap?.env.ready !== true
    || snap?.harness.present !== true
}

function renderPluginSummary(): void {
  const dot = $('#plugin-ready-dot')
  const info = $('#plugin-info')
  const toggle = $<HTMLButtonElement>('#btn-toggle-plugins')
  toggle.setAttribute('aria-expanded', String(pluginPanelOpen))
  toggle.textContent = pluginPanelOpen ? '收起插件' : '管理插件'
  toggle.disabled = pluginLoading

  if (pluginError !== null) {
    dot.className = 'env-dot problem'
    info.innerHTML = kv([
      ['Profile', 'web'],
      ['状态', pluginError],
    ])
    return
  }
  if (pluginCatalog === null) {
    dot.className = 'env-dot'
    info.innerHTML = kv([
      ['Profile', 'web'],
      ['状态', pluginLoading ? '读取中…' : '尚未读取'],
    ])
    return
  }
  const installed = pluginCatalog.plugins.filter((plugin) => plugin.installedVersion !== null).length
  const updates = [...pluginUpdates.values()].filter((update) => update.updateAvailable).length
  dot.className = 'env-dot ready'
  info.innerHTML = kv([
    ['Profile', pluginCatalog.profile],
    ['已安装', `${installed} 个`],
    ['可更新', pluginUpdatesChecked ? `${updates} 个` : '未检查'],
  ])
}

function renderPluginManager(): void {
  $('#plugin-manager').classList.toggle('hidden', !pluginPanelOpen)
  if (!pluginPanelOpen) return

  const status = $('#plugin-manager-status')
  const failedChecks = [...pluginUpdates.values()].filter((update) => update.error !== null).length
  if (pluginError !== null) {
    status.textContent = `读取插件失败：${pluginError}`
    status.className = 'plugin-manager-status problem'
  } else if (pluginOperation) {
    status.textContent = '正在变更插件；若服务已运行，桌面端会在完成后自动恢复。'
    status.className = 'plugin-manager-status working'
  } else if (pluginChecking) {
    status.textContent = '正在从 npm registry 检查版本…'
    status.className = 'plugin-manager-status working'
  } else if (failedChecks > 0) {
    status.textContent = `${failedChecks} 个插件暂时无法检查更新，本地管理仍可使用。`
    status.className = 'plugin-manager-status problem'
  } else {
    status.textContent = '插件保存在 ~/.dsh/profiles/web，更新 Harness 源码不会覆盖。'
    status.className = 'plugin-manager-status'
  }

  const disabled = pluginActionsDisabled()
  $<HTMLButtonElement>('#btn-check-plugin-updates').disabled = disabled || pluginChecking || pluginCatalog === null
  $<HTMLInputElement>('#custom-plugin-spec').disabled = disabled
  $<HTMLButtonElement>('#btn-install-custom-plugin').disabled = disabled

  const list = $('#plugin-list')
  if (pluginCatalog === null) {
    list.innerHTML = '<div class="plugin-empty">正在读取 web profile…</div>'
    return
  }
  if (pluginCatalog.plugins.length === 0) {
    list.innerHTML = '<div class="plugin-empty">尚未安装插件，可在下方输入 npm 包名安装。</div>'
    return
  }
  list.innerHTML = pluginCatalog.plugins.map((plugin) => {
    const update = pluginUpdates.get(plugin.packageName)
    const installed = plugin.installedVersion !== null
    const broken = plugin.requestedVersion !== null && !installed
    const stateClass = update?.updateAvailable === true ? 'update' : installed ? 'installed' : broken ? 'problem' : 'idle'
    const stateText = update?.updateAvailable === true
      ? `可升级至 ${update.latestVersion ?? '最新版'}`
      : installed
        ? `已安装 ${plugin.installedVersion}`
        : broken
          ? '安装不完整'
          : '未安装'
    const versionText = update?.latestVersion !== null && update?.latestVersion !== undefined
      ? `registry ${update.latestVersion}`
      : pluginUpdatesChecked
        ? 'registry 版本未知'
        : '尚未检查 registry'
    const homepage = plugin.homepage === null
      ? ''
      : `<a class="plugin-homepage" href="${escapeHtml(plugin.homepage)}" target="_blank" rel="noopener noreferrer">项目主页</a>`
    const actions = installed
      ? [
          update?.updateAvailable === true
            ? `<button class="btn btn-small btn-primary" aria-label="升级 ${escapeHtml(plugin.displayName)}" data-plugin-action="update" data-package="${escapeHtml(plugin.packageName)}">升级</button>`
            : '',
          `<button class="btn btn-small" aria-label="重装 ${escapeHtml(plugin.displayName)}" data-plugin-action="reinstall" data-package="${escapeHtml(plugin.packageName)}">重装</button>`,
          `<button class="btn btn-small btn-danger" aria-label="卸载 ${escapeHtml(plugin.displayName)}" data-plugin-action="remove" data-package="${escapeHtml(plugin.packageName)}">卸载</button>`,
        ].join('')
      : broken
        ? [
            `<button class="btn btn-small btn-primary" aria-label="修复安装 ${escapeHtml(plugin.displayName)}" data-plugin-action="install" data-package="${escapeHtml(plugin.packageName)}">修复安装</button>`,
            `<button class="btn btn-small btn-danger" aria-label="卸载 ${escapeHtml(plugin.displayName)}" data-plugin-action="remove" data-package="${escapeHtml(plugin.packageName)}">卸载</button>`,
          ].join('')
        : `<button class="btn btn-small btn-primary" aria-label="安装 ${escapeHtml(plugin.displayName)}" data-plugin-action="install" data-package="${escapeHtml(plugin.packageName)}">安装</button>`
    return `
      <article class="plugin-row">
        <span class="plugin-track ${stateClass}" aria-hidden="true"></span>
        <div class="plugin-identity">
          <div class="plugin-name-line">
            <strong>${escapeHtml(plugin.displayName)}</strong>
          </div>
          <code>${escapeHtml(plugin.packageName)}</code>
          <p>${escapeHtml(plugin.description)} ${homepage}</p>
        </div>
        <div class="plugin-version">
          <strong>${escapeHtml(stateText)}</strong>
          <span>${escapeHtml(versionText)}</span>
          ${plugin.requestedVersion === null ? '' : `<span>声明 ${escapeHtml(plugin.requestedVersion)}</span>`}
        </div>
        <div class="plugin-row-actions">${actions}</div>
      </article>`
  }).join('')
  list.querySelectorAll<HTMLButtonElement>('[data-plugin-action]').forEach((button) => {
    button.disabled = disabled
  })
}

function renderPlugins(): void {
  renderPluginSummary()
  renderPluginManager()
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
  renderPlugins()

  const consoleWasHidden = $('#console-view').classList.contains('hidden')

  if (running && previousServiceStatus !== 'running' && !keepConsoleDuringServiceTransition) {
    activeView = 'app'
  } else if (!running) {
    activeView = 'console'
  }
  previousServiceStatus = service.status

  const showApp = running && activeView === 'app'
  $('#console-view').classList.toggle('hidden', showApp)
  $('#app-view').classList.toggle('hidden', !showApp)
  const consoleBtn = $<HTMLButtonElement>('#btn-console')
  const appBtn = $<HTMLButtonElement>('#btn-app')
  const refreshAppBtn = $<HTMLButtonElement>('#btn-refresh-app')
  consoleBtn.setAttribute('aria-pressed', String(!showApp))
  appBtn.setAttribute('aria-pressed', String(showApp))
  appBtn.disabled = !running
  refreshAppBtn.disabled = !running
  if (consoleWasHidden && !showApp) restickLog()
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

function refreshWorkbench(): void {
  if (snap?.service.status !== 'running') return
  const url = new URL(snap.service.url)
  url.searchParams.set('_dsh_refresh', String(Date.now()))
  $<HTMLIFrameElement>('#frame').src = url.toString()
  activeView = 'app'
  render()
  toast('工作台已刷新')
}

let refreshInFlight: Promise<void> | null = null

async function refreshOnce(): Promise<void> {
  try {
    snap = await invoke<Snapshot>('get_state')
  } catch (err) {
    toast(`读取状态失败：${String(err)}`, true)
    return
  }
  render()
}

function refresh(): Promise<void> {
  if (refreshInFlight !== null) return refreshInFlight
  const request = refreshOnce().finally(() => {
    if (refreshInFlight === request) refreshInFlight = null
  })
  refreshInFlight = request
  return request
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

async function loadPlugins(checkUpdates: boolean): Promise<void> {
  if (pluginLoading) return
  pluginLoading = true
  pluginError = null
  renderPlugins()
  let shouldCheck = false
  try {
    pluginCatalog = await invoke<PluginCatalog>('get_plugins')
    shouldCheck = checkUpdates
  } catch (err) {
    pluginError = String(err)
  } finally {
    pluginLoading = false
    renderPlugins()
  }
  if (shouldCheck && snap?.env.ready === true && snap.harness.present) {
    await checkPluginUpdates()
  }
}

async function checkPluginUpdates(): Promise<void> {
  if (pluginChecking || pluginOperation || pluginCatalog === null) return
  pluginChecking = true
  renderPlugins()
  try {
    const updates = await invoke<PluginUpdate[]>('check_plugin_updates')
    pluginUpdates = new Map(updates.map((update) => [update.packageName, update]))
    pluginUpdatesChecked = true
  } catch (err) {
    toast(`检查插件更新失败：${String(err)}`, true)
  } finally {
    pluginChecking = false
    renderPlugins()
  }
}

async function setPluginPanel(show: boolean): Promise<void> {
  pluginPanelOpen = show
  renderPlugins()
  if (show) await loadPlugins(true)
}

async function managePlugin(action: PluginAction, packageSpec: string): Promise<void> {
  if (pluginOperation || pluginActionsDisabled()) return
  pluginOperation = true
  keepConsoleDuringServiceTransition = true
  activeView = 'console'
  render()
  try {
    const result = await invoke<ManagePluginResult>('manage_plugin', {
      request: { action, packageSpec },
    })
    pluginCatalog = result.catalog
    pluginUpdates.clear()
    pluginUpdatesChecked = false
    const customInput = $<HTMLInputElement>('#custom-plugin-spec')
    if (action === 'install' && customInput.value.trim() === packageSpec) customInput.value = ''
    toast(`${result.message}${result.serviceRestarted ? '，服务已恢复' : ''}`)
    if (action === 'install') {
      appendLog({ source: 'plugin', line: '请到 工作台 → 设置 → 插件 → 插件配置 完成配置' })
    }
  } catch (err) {
    toast(`插件操作失败：${String(err)}`, true)
    appendLog({ source: 'plugin', line: `插件操作失败：${String(err)}` })
    await loadPlugins(false)
  } finally {
    await refresh()
    pluginOperation = false
    keepConsoleDuringServiceTransition = false
    activeView = 'console'
    render()
  }
  void checkPluginUpdates()
}

function confirmPluginAction(action: PluginAction, packageSpec: string): boolean {
  const restarts = snap?.service.status === 'running' || snap?.service.status === 'starting'
  const restartHint = restarts ? '\n\nHarness 服务会短暂停止并自动恢复。' : ''
  if (action === 'remove') {
    return window.confirm(`确认卸载 ${packageSpec}？${restartHint}`)
  }
  if (action === 'install' && $<HTMLInputElement>('#custom-plugin-spec').value.trim() === packageSpec) {
    return window.confirm(`确认从 npm registry 安装 ${packageSpec}？插件代码将在本机运行。${restartHint}`)
  }
  return true
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
  $('#btn-toggle-plugins').addEventListener('click', () => void setPluginPanel(!pluginPanelOpen))
  $('#btn-close-plugins').addEventListener('click', () => void setPluginPanel(false))
  $('#btn-check-plugin-updates').addEventListener('click', () => void checkPluginUpdates())
  $('#plugin-list').addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLButtonElement>('[data-plugin-action]')
    if (button === null) return
    const action = button.dataset.pluginAction as PluginAction | undefined
    const packageSpec = button.dataset.package
    if (action === undefined || packageSpec === undefined || !confirmPluginAction(action, packageSpec)) return
    void managePlugin(action, packageSpec)
  })
  $('#custom-plugin-form').addEventListener('submit', (event) => {
    event.preventDefault()
    const input = $<HTMLInputElement>('#custom-plugin-spec')
    const packageSpec = input.value.trim()
    if (packageSpec === '') {
      toast('请输入 npm 插件包名', true)
      input.focus()
      return
    }
    if (!confirmPluginAction('install', packageSpec)) return
    void managePlugin('install', packageSpec)
  })
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
  $('#btn-copy-log').addEventListener('click', () => void copyAllLogs())
  $('#btn-clear-log').addEventListener('click', () => void clearLogs())
  $('#btn-console').addEventListener('click', () => {
    activeView = 'console'
    render()
  })
  $('#btn-app').addEventListener('click', () => {
    if (snap?.service.status !== 'running') return
    activeView = 'app'
    render()
  })
  $('#btn-refresh-app').addEventListener('click', refreshWorkbench)
}

async function subscribe(): Promise<void> {
  await listen<BackendLogEntry>('log', (event) => acceptBackendLog(event.payload))
  await listen<Snapshot>('state-changed', (event) => {
    snap = event.payload
    render()
  })
}

async function initialize(): Promise<void> {
  bind()
  await subscribe()
  await hydrateLogs()
  await refresh()
  await loadPlugins(false)
}

void initialize().catch((err) => {
  logHydrating = false
  appendLog({ source: 'ui', line: `初始化失败：${String(err)}` })
  toast(`初始化失败：${String(err)}`, true)
})

// Events provide immediate updates; polling repairs a missed busy/start event.
window.setInterval(() => {
  if (snap !== null && (snap.busy !== null || snap.service.status === 'starting')) void refresh()
}, 1000)
