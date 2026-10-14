/* FileWatch GUI 主控：状态、IPC、各面板渲染。
 *
 * 数据流是单向的：
 *     invoke(...)  →  状态  →  render*()
 * 没有双向绑定，也没有框架 —— 一个 3000 行的工具用不上，反而更难读。
 *
 * 后端的真相只有一处：`state.focus`（当前视图根）。任何会改变它的动作
 * （点方块、点面包屑、滚轮）最后都收敛到 `goto(node)`，由它重新算一次布局。
 */

'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/* ── 状态 ──────────────────────────────────────────────────────────────── */

const state = {
  volumes: [],
  summary: null,
  /** 当前视图根节点 */
  focus: null,
  /** 扫描根节点（面包屑的起点） */
  rootNode: null,
  /** node → {name, path} 缓存，避免每次都回后端问 */
  meta: new Map(),
  crumbs: [],
  /** 被选中的节点 */
  picked: null,
  /** 高亮的扩展名 id（null = 不高亮） */
  highlight: null,
  mode: 'logical',
  shading: 'ext',
  detail: 1,
  scanning: false,
};

/** 「细节」滑块 → LOD 的 min_side 阈值。
 *  后端默认 80px；往左更精简（只展开大块），往右展开更深。 */
const DETAIL_MIN_SIDE = [240, 80, 32, 12];

/* ── 小工具 ────────────────────────────────────────────────────────────── */

function humanSize(bytes) {
  const v = Number(bytes) || 0;
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  let x = v;
  while (x >= 1024 && i < units.length - 1) { x /= 1024; i++; }
  return (i === 0 ? String(Math.round(x)) : x.toFixed(x >= 100 ? 0 : x >= 10 ? 1 : 2)) + ' ' + units[i];
}

function pct(v) { return (v * 100).toFixed(1) + '%'; }

function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function fmtTime(unixSec) {
  if (!unixSec) return '—';
  const d = new Date(unixSec * 1000);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

let toastTimer = null;
function toast(msg, isErr) {
  const el = document.getElementById('toast');
  el.textContent = msg;
  el.classList.toggle('err', !!isErr);
  el.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.add('hidden'), isErr ? 6000 : 2600);
}

/** 统一的命令调用包装：把后端的字符串错误变成 toast，而不是让界面静默卡住。 */
async function call(cmd, args) {
  try {
    return await invoke(cmd, args || {});
  } catch (e) {
    toast(typeof e === 'string' ? e : (e && e.message) || String(e), true);
    throw e;
  }
}

/* ── F1 盘符 ───────────────────────────────────────────────────────────── */

async function loadVolumes() {
  state.volumes = await call('list_volumes');
  const box = document.getElementById('volumes');
  box.innerHTML = '';
  for (const v of state.volumes) {
    const el = document.createElement('div');
    el.className = 'vol';
    el.title = `${v.root}　${v.fs_name}　${v.label || '(无卷标)'}`;
    const canMft = v.supports_mft && v.elevated;
    el.innerHTML =
      `<div class="top"><span class="drive">${esc(v.drive)}</span>` +
      `<span class="fs">${esc(v.fs_name)}</span></div>` +
      `<div class="label">${esc(v.label || '(无卷标)')}</div>` +
      `<div class="bar"><i style="width:${(v.used_ratio * 100).toFixed(1)}%"></i></div>` +
      `<div class="cap"><span>${humanSize(v.total)}</span>` +
      `<span>可用 ${humanSize(v.free)}</span></div>` +
      (canMft ? '<div class="fs">MFT 直读</div>'
              : `<div class="noadmin">${v.supports_mft ? '需管理员才能 MFT 直读，将降级' : '非 NTFS，走遍历'}</div>`);
    el.onclick = () => startScan(v.drive, null);
    box.appendChild(el);
  }
}

/* ── F2 扫描 ───────────────────────────────────────────────────────────── */

function setScanning(on) {
  state.scanning = on;
  document.getElementById('scan-overlay').classList.toggle('hidden', !on);
  document.getElementById('scan-root').disabled = on;
  document.querySelectorAll('.vol').forEach((e) => e.classList.toggle('disabled', on));
}

function progressUI(p) {
  document.getElementById('progress-text').textContent =
    `${p.phase}　已处理 ${p.done.toLocaleString()} 个节点` +
    (p.total ? ` / 共 ${p.total.toLocaleString()}` : '') +
    `　已用 ${(p.elapsed_ms / 1000).toFixed(1)} s`;
  // 目录遍历事先不知道总数，所以用「不确定进度」的观感：条走到 90% 封顶
  const bar = document.getElementById('progress-bar');
  bar.style.width = p.total ? `${Math.min(100, (p.done / p.total) * 100).toFixed(1)}%`
                            : `${Math.min(90, 4 + Math.log10(p.done + 10) * 22).toFixed(1)}%`;
}

async function startScan(drive, root) {
  if (state.scanning) { toast('已有一次扫描在进行中'); return; }
  setScanning(true);
  document.getElementById('progress-text').textContent = '准备中…';
  document.getElementById('progress-bar').style.width = '2%';
  document.querySelectorAll('.vol').forEach((e) => e.classList.remove('active'));

  // 记住用户选的是哪个盘，扫描完成后用于标亮
  state.pendingDrive = drive;
  await call('scan_start', {
    drive,
    root: root || null,
    watch: document.getElementById('watch').checked,
  });
}

async function afterScan(summary, errorMsg) {
  setScanning(false);
  if (errorMsg) {
    state.summary = null;
    setBadge('扫描失败', '');
    document.getElementById('st-paths').textContent = errorMsg;
    toast(errorMsg, true);
    return;
  }
  state.summary = summary;
  state.highlight = null;
  state.picked = null;
  Treemap.setHighlight(null);

  setBadge(summary.backend, summary.backend.includes('MFT') ? 'mft' : 'traverse');

  // 视图根 = 索引树的根节点。面板与面积图都从这里开始。
  state.rootNode = null;
  state.focus = null;
  state.meta.clear();

  // 从扫描根开始。**不要**硬编码节点号：虽然 NTFS 的根目录就是 MFT 记录 5，
  // 但那是引擎的实现细节，后端在摘要里已经把它带过来了。
  await goto(summary.root_node, true);
  renderStatus();
  renderExts();
  renderTopFiles();
  renderDetail(null);
  document.getElementById('empty-hint').classList.add('hidden');
  for (const w of summary.warnings || []) toast(w);
}

function setBadge(text, cls) {
  const b = document.getElementById('backend-badge');
  b.textContent = text;
  b.className = 'badge' + (cls ? ' ' + cls : '');
}

function renderStatus() {
  const s = state.summary;
  const st = (id, v) => { document.getElementById(id).textContent = v; };
  if (!s) {
    ['st-backend', 'st-counts', 'st-size', 'st-memory', 'st-time', 'st-layout'].forEach((i) => st(i, '—'));
    return;
  }
  st('st-backend', s.backend);
  st('st-counts', `${s.dirs.toLocaleString()} 目录 / ${s.files.toLocaleString()} 文件`);
  st('st-size', `逻辑 ${humanSize(s.logical)}　占用 ${humanSize(s.alloc)}`);
  st('st-memory', `索引约 ${humanSize(s.memory_bytes)}`);
  st('st-time', `${(s.wall_ms / 1000).toFixed(2)} s` + (s.errors ? `　错误 ${s.errors}` : ''));
  st('st-paths', s.root);
}

/* ── 布局与导航 ────────────────────────────────────────────────────────── */

/** 布局参数。集中在一处，免得 `layout` 与 `layout_info` 用上不同的口径 ——
 *  那会让「统计说展开 3000 个，画面只画了 800 个」这种对不上的问题极难查。 */
function layoutArgs(w, h) {
  return {
    width: Math.max(1, Math.round(w)),
    height: Math.max(1, Math.round(h)),
    root: state.focus,
    mode: state.mode,
    shading: state.shading,
    deep: true,
    min_side: DETAIL_MIN_SIDE[state.detail],
  };
}

async function refreshLayout() {
  if (state.focus === null) return;
  const { w, h } = Treemap.size();
  const args = layoutArgs(w, h);
  const buf = await call('layout', { args });
  const l = new Layout(buf instanceof ArrayBuffer ? buf : new Uint8Array(buf).buffer);
  Treemap.setLayout(l);

  // 布局统计（会重跑一次布局，所以只在这里按需调用，不进每帧路径）
  const info = await call('layout_info', { args: layoutArgs(w, h) });
  document.getElementById('canvas-status').textContent =
    `绘制 ${info.emitted.toLocaleString()} 个矩形　` +
    `太小折叠 ${info.collapsed_small.toLocaleString()}　` +
    `深度折叠 ${info.collapsed_depth.toLocaleString()}　` +
    `最大深度 ${info.max_depth_reached}` +
    (info.hit_budget ? '　⚠ 已触及节点预算上限' : '');
}

/** 跳转到某个节点：刷新面包屑 + 重新布局。`reset` 用于扫描完成后的首次进入。 */
async function goto(node, reset) {
  state.focus = node;
  if (reset) state.rootNode = node;
  state.crumbs = await call('breadcrumb', { node });
  renderBreadcrumb();
  await refreshLayout();
  await renderTree();
}

/** 需要显示名字时向后端要一次并缓存 —— 布局里只有 id。 */
async function metaOf(node) {
  if (state.meta.has(node)) return state.meta.get(node);
  const info = await call('node_detail', { node });
  const v = { name: info.name === '.' ? info.path : info.name, path: info.path, info };
  state.meta.set(node, v);
  return v;
}

function cachedName(node) {
  const m = state.meta.get(node);
  if (m) return m.name;
  // 还没有就异步补上并重画，避免为每个方块都串行等一次 IPC
  metaOf(node).then(() => Treemap.redraw()).catch(() => {});
  return '';
}

function cachedPath(node) {
  const m = state.meta.get(node);
  if (m) return m.path;
  metaOf(node).then(() => {}).catch(() => {});
  return `#${node}`;
}

function renderBreadcrumb() {
  const box = document.getElementById('breadcrumb');
  box.innerHTML = '';
  state.crumbs.forEach((c, i) => {
    if (i) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = '›';
      box.appendChild(sep);
    }
    const el = document.createElement('span');
    el.className = 'crumb' + (i === state.crumbs.length - 1 ? ' last' : '');
    el.textContent = i === 0 ? (c.name || '根') : c.name;
    el.onclick = () => goto(c.node);
    box.appendChild(el);
  });
}

/* ── F3 文件夹树 ───────────────────────────────────────────────────────── */

async function renderTree() {
  const box = document.getElementById('tree');
  box.innerHTML = '';
  if (state.focus === null) return;

  // 面包屑的每一级作为树的一层，当前焦点展开显示其子项
  const chain = state.crumbs.slice(-4);
  for (let i = 0; i < chain.length; i++) {
    const c = chain[i];
    const last = i === chain.length - 1;
    box.appendChild(treeRow(c.node, c.name, true, last ? '▾' : '▸', true));
    if (last) {
      const kids = await call('tree_rows', { parent: c.node, offset: 0, limit: 200, mode: state.mode });
      for (const k of kids) {
        box.appendChild(treeRow(k.node, k.name, k.is_dir, k.children ? '▸' : '', false, k));
      }
      if (!kids.length) {
        const d = document.createElement('div');
        d.className = 'dim';
        d.style.padding = '4px 8px';
        d.textContent = '（没有子项）';
        box.appendChild(d);
      }
    }
  }
  document.getElementById('tree-hint').textContent =
    `当前层级共 ${(() => { const l = Treemap.layout; return l ? l.count : 0; })()} 个方块`;
}

function treeRow(node, name, isDir, arrow, sel, row) {
  const el = document.createElement('div');
  el.className = 'tr' + (sel ? ' sel' : '');
  const size = row ? row.size : (state.meta.get(node)?.info?.size ?? 0);
  const cnt = row ? (row.is_dir ? `${row.files.toLocaleString()} 文件` : '') : '';
  el.innerHTML =
    `<span class="arrow${arrow === '▾' ? ' open' : ''}">${arrow || ''}</span>` +
    `<span class="nm${isDir ? ' dir' : ''}">${esc(name)}</span>` +
    `<span class="sz">${row ? humanSize(size) : ''}<span class="cnt">${cnt ? '　' + cnt : ''}</span></span>`;
  el.onclick = () => {
    if (isDir) goto(node);
    else pickNode(node);
  };
  return el;
}

/* ── F5 类型列表 ───────────────────────────────────────────────────────── */

async function renderExts() {
  const rows = await call('ext_rows', { limit: 60, mode: state.mode });
  const box = document.getElementById('exts');
  box.innerHTML = '';
  for (const e of rows) {
    const el = document.createElement('div');
    el.className = 'ext' + (state.highlight === e.ext_id ? ' on' : '');
    // 色块用的是布局里的槽位色，而槽位是 `ext_id % 15`（见后端 shade_of）。
    // 无扩展名的 ext_id 是 u32::MAX，后端把它归到最后一个槽位。
    const slot = (e.ext_id === 4294967295) ? 15 : (e.ext_id % 15);
    el.innerHTML =
      `<span class="nm"><span class="sw" style="background:${fwColor(slot, 0.5)}"></span>${esc(e.ext)}</span>` +
      `<span class="n">${e.count.toLocaleString()}</span>` +
      `<span class="s">${humanSize(e.size)}</span>`;
    el.onclick = () => {
      state.highlight = state.highlight === e.ext_id ? null : e.ext_id;
      Treemap.setHighlight(state.highlight);
      document.getElementById('clear-highlight').classList.toggle('hidden', state.highlight === null);
      renderExts();
    };
    box.appendChild(el);
  }
  if (!rows.length) box.innerHTML = '<div class="dim">（无数据）</div>';
}

/* ── F12 大文件 ────────────────────────────────────────────────────────── */

async function renderTopFiles() {
  const rows = await call('top_files', { limit: 200, mode: state.mode });
  const box = document.getElementById('topfiles');
  box.innerHTML = '';
  for (const f of rows) {
    const el = document.createElement('div');
    el.className = 'file';
    el.title = f.path;
    el.innerHTML = `<span class="nm">${esc(f.name)}</span><span class="sz">${humanSize(f.size)}</span>`;
    el.onclick = () => pickNode(f.node);
    box.appendChild(el);
  }
  if (!rows.length) box.innerHTML = '<div class="dim">（无数据）</div>';
}

/* ── 详情 ──────────────────────────────────────────────────────────────── */

async function pickNode(node) {
  const info = await call('node_detail', { node });
  state.picked = node;
  state.meta.set(node, {
    name: info.name === '.' ? info.path : info.name,
    path: info.path,
    info,
  });
  renderDetail(info);
}

function renderDetail(info) {
  const box = document.getElementById('detail-panel');
  if (!info) {
    box.className = 'detail dim';
    box.textContent = '点选任意方块查看';
    return;
  }
  box.className = 'detail';
  box.innerHTML =
    `<table>` +
    `<tr><td>名称</td><td>${esc(info.name === '.' ? info.path : info.name)}</td></tr>` +
    `<tr><td>类型</td><td>${info.is_dir ? '目录' : '文件'}</td></tr>` +
    `<tr><td>逻辑</td><td>${humanSize(info.size)}</td></tr>` +
    `<tr><td>占用</td><td>${humanSize(info.alloc)}</td></tr>` +
    (info.is_dir ? `<tr><td>子项</td><td>${info.children.toLocaleString()} 个，含 ${info.files.toLocaleString()} 个文件</td></tr>` : '') +
    `<tr><td>深度</td><td>${info.depth}</td></tr>` +
    `<tr><td>修改</td><td>${fmtTime(info.mtime)}</td></tr>` +
    `<tr><td>路径</td><td>${esc(info.path)}</td></tr>` +
    `</table>`;
}

/* ── F0 管理员权限（进入软件时申请）─────────────────────────────────────
 *
 * 清单保持 `asInvoker`（不靠 manifest 自动提权），所以"要不要管理员"这件事
 * 由这里在启动时问一次。三条规则：
 *
 *   1. 只在**真的没权限**时问。`privileged` 是后端算好的并集判据
 *      （令牌 + 卷设备能否打开），不要在界面上自己拼 —— UAC 被关掉的机器上
 *      令牌会说"未提权"，而卷其实是能读的。
 *   2. 带 `attempted` 时**不问**。那是"本次启动已经尝试过提权"的标记，
 *      防止在提权无效的机器上"重启→又弹→再重启"地转圈（见后端的 elevate 模块）。
 *   3. 用户勾了"不再自动询问"就落盘记住；但右下角的按钮永远能把窗调回来，
 *      所以这个选择是可逆的 —— 不会被自己的偏好锁死。
 */

let adminInfo = null;

/** 取权限现状并刷新状态栏按钮。 */
async function loadElevation() {
  adminInfo = await call('elevation_info');
  renderPermButton();
  return adminInfo;
}

function renderPermButton() {
  const b = document.getElementById('admin-btn');
  b.textContent = fwAdminButtonText(adminInfo);
  b.className = fwAdminButtonClass(adminInfo);
  if (!adminInfo) return;
  b.title = adminInfo.privileged
    ? `${adminInfo.hint}\n账户：${adminInfo.account}`
    : `${adminInfo.hint}\n点这里申请管理员权限`;
}

async function loadPrefs() {
  try {
    return await invoke('gui_prefs');
  } catch (e) {
    // 偏好文件的读取在后端是容错的；这里再兜一层，免得一条读不到偏好
    // 就让整个界面初始化失败
    return { ask_admin_on_start: true };
  }
}

async function savePrefs(next) {
  try {
    await invoke('set_gui_prefs', { value: next });
  } catch (e) {
    toast('偏好没能保存：' + (typeof e === 'string' ? e : (e && e.message) || e), true);
  }
}

function adminFacts(info) {
  const row = (k, v) => `<div class="kv"><span>${k}</span><span>${esc(v)}</span></div>`;
  return (
    row('账户', info.account) +
    row('当前状态', info.hint) +
    (info.action_hint ? row('点「重启」后', info.action_hint) : '') +
    row('将要执行的命令', info.command_line)
  );
}

/** 打开权限窗。`auto` 为真表示这是"进入软件时自动弹的"那一次。 */
async function openAdminModal(auto) {
  adminInfo = await call('elevation_info');
  if (adminInfo.privileged) {
    if (!auto) toast('当前已经是管理员权限：' + adminInfo.hint);
    renderPermButton();
    return;
  }
  const prefs = await loadPrefs();
  document.getElementById('admin-title').textContent =
    adminInfo.can_request ? '以管理员身份运行？' : '可以试着申请管理员权限';
  document.getElementById('admin-facts').innerHTML = adminFacts(adminInfo);
  document.getElementById('admin-remember').checked = !prefs.ask_admin_on_start;
  document.getElementById('admin-yes').disabled = false;
  const wait = document.getElementById('admin-wait');
  wait.classList.add('hidden');
  wait.textContent = '';
  document.getElementById('admin-modal').classList.remove('hidden');
}

function closeAdminModal() {
  document.getElementById('admin-modal').classList.add('hidden');
}

/** 弹窗里的两个按钮都先把"不再询问"落盘 —— 用户既然做了选择，就该被记住。 */
async function adminRemembered() {
  const remember = document.getElementById('admin-remember').checked;
  await savePrefs({ ask_admin_on_start: !remember });
}

function wireAdminModal() {
  document.getElementById('admin-btn').onclick = () => {
    openAdminModal(false).catch(() => {});
  };

  // 「以管理员身份重启」：后端立刻返回，真正的结果走 admin:relaunch 事件 ——
  // UAC 确认框会阻塞好几秒，等在这里会把界面冻住
  document.getElementById('admin-yes').onclick = async () => {
    await adminRemembered();
    document.getElementById('admin-yes').disabled = true;
    const wait = document.getElementById('admin-wait');
    wait.textContent = '正在等待 Windows 的权限确认…';
    wait.classList.remove('hidden');
    try {
      await call('relaunch_elevated');
    } catch (e) {
      // 命令本身被拒（比如正在扫描）：恢复按钮，让用户能换个时机再试
      document.getElementById('admin-yes').disabled = false;
      wait.classList.add('hidden');
    }
  };

  document.getElementById('admin-no').onclick = async () => {
    await adminRemembered();
    closeAdminModal();
    // 说清楚"降级"意味着什么，避免用户以为功能被砍了
    if (!adminInfo.privileged) {
      toast('已按降级模式运行：功能不缺，扫描会慢一些');
    }
  };
}

/** 后端的提权结果。 */
function onRelaunchOutcome(r) {
  if (r.started) {
    const wait = document.getElementById('admin-wait');
    wait.textContent = '已启动管理员实例，本窗口即将关闭…';
    wait.classList.remove('hidden');
    return;
  }
  document.getElementById('admin-yes').disabled = false;
  document.getElementById('admin-wait').classList.add('hidden');
  toast(r.reason || '提权重启失败', true);
  // 顺手把真实状态刷一遍：万一刚才其实已经是管理员（比如 UAC 被关掉又开着），
  // 按钮的颜色要跟着变，不能一直挂着"降级模式"
  loadElevation()
    .then(() => {
      if (adminInfo.privileged) closeAdminModal();
    })
    .catch(() => {});
}

/* ── F6/F7/F8 实时监控 ─────────────────────────────────────────────────
 *
 * 后端的监控线程每应用一批事件就推一条 `monitor:tick`，里面带着渲染好的行
 * （体积说法与 CLI 同源）。这里只做三件事：攒行、画行、按代数重画面积图。
 *
 * **代数（generation）是这里的关键**：监控会就地改索引树，前端手上的布局
 * 随时可能过期。后端每改一次树就 +1，前端发现代数和上次不同就重画 ——
 * 这样"面积图永远滞后"这种问题不会出现，也不需要前端自己猜什么时候该刷。
 */

const mon = {
  open: false,
  running: false,
  info: null,
  snap: null,
  tab: 'changes',
  filter: '',
  /** 变更行，最新的在前 */
  rows: [],
  alerts: [],
  /** 已见过的最大序号，用来在做增量拼接时去重 */
  maxSeq: 0,
  lastGen: 0,
  /** 上次因代数变化重画的时间（节流用） */
  lastRedraw: 0,
  redrawTimer: null,
  statsTick: 0,
};

/** 界面上最多保留多少行。环形缓冲是 4000，但 DOM 不可能全画。 */
const MON_ROW_CAP = 800;

/** 保尾截断：路径最有价值的部分在末尾（文件名），从中间截等于没说。 */
function monKeepTail(s, n) {
  const str = String(s);
  if (str.length <= n) return str;
  return '…' + str.slice(str.length - n + 1);
}

/** 相对起点的 `mm:ss.mmm`。 */
function monOffset(ms) {
  const m = Math.floor(ms / 60000);
  const s = Math.floor((ms % 60000) / 1000);
  const msec = Math.floor(ms % 1000);
  const p = (v, w) => String(v).padStart(w, '0');
  return `${p(m, 2)}:${p(s, 2)}.${p(msec, 3)}`;
}

function monShow(open) {
  mon.open = open;
  document.getElementById('mon-body').classList.toggle('hidden', !open);
  document.getElementById('mon-toggle').textContent = (open ? '▾' : '▸') + ' 实时监控';
  if (open) {
    backfillMonitor().catch(() => {});
  }
}

function renderMonHead() {
  const s = mon.snap;
  const i = mon.info;
  const set = (id, v) => { document.getElementById(id).textContent = v; };

  const b = document.getElementById('mon-state');
  b.textContent = mon.running ? '监控中' : (i ? '已停止' : '未启动');
  b.className = 'badge' + (mon.running ? ' mft' : (i ? '' : ''));

  set('mon-targets', i && i.paths.length ? i.paths.join('　') : '—');
  set('mon-source', i ? `${i.incremental}　${i.scope}` : '—');
  set('mon-counts', s
    ? `已运行 ${monOffset(s.elapsed_ms || 0)}　变更 ${(s.total_events || 0).toLocaleString()} 条`
      + `　预警 ${(s.total_alerts || 0).toLocaleString()} 次`
      + (s.still_pending ? `　待定 ${s.still_pending}` : '')
      + (s.backpressure ? `　⚠ 背压 ${s.backpressure}` : '')
    : '—');

  const err = document.getElementById('mon-error');
  const msg = (s && s.error) || '';
  err.textContent = msg;
  err.classList.toggle('hidden', !msg);
  err.title = msg;

  document.getElementById('mon-start').classList.toggle('hidden', mon.running);
  document.getElementById('mon-stop').classList.toggle('hidden', !mon.running);
  // 监控期间不能扫描（会换掉整棵树），按钮直接禁掉比让后端报错友好
  document.getElementById('scan-root').disabled = mon.running || state.scanning;
}

function renderMonList() {
  const box = document.getElementById('mon-list');
  const hint = document.getElementById('mon-list-hint');
  box.innerHTML = '';

  if (mon.tab === 'alerts') {
    hint.textContent = `${mon.alerts.length} 条`;
    if (!mon.alerts.length) {
      box.innerHTML = '<div class="mon-empty">还没有触发过预警。'
        + '在上面的「阈值规则」里写一条，例如 <code>grow=500M</code>。</div>';
      return;
    }
    for (const a of mon.alerts.slice(0, 300)) {
      const el = document.createElement('div');
      el.className = 'arow';
      el.title = a.message;
      el.innerHTML =
        `<span class="t">${monOffset(a.at_ms)}</span>` +
        `<span class="k ${a.kind}">${esc(a.kind_cn)}</span>` +
        `<span class="p">${esc(monKeepTail(a.path, 90))}</span>` +
        `<span class="s">${humanSize(a.current)} / 阈值 ${humanSize(a.threshold)}</span>`;
      box.appendChild(el);
    }
    return;
  }

  const shown = mon.rows.slice(0, 300);
  hint.textContent = shown.length < mon.rows.length
    ? `${shown.length} / ${mon.rows.length} 条（导出 CSV 可看全部）`
    : `${mon.rows.length} 条`;
  if (!mon.rows.length) {
    box.innerHTML = mon.running
      ? '<div class="mon-empty">监控已开始，正在等待变更…（在上面那个目录里新建/改动一个文件试试）</div>'
      : '<div class="mon-empty">还没有变更记录。点「开始监控」后，这里会实时刷出新增 / 删除 / 改名 / 修改。</div>';
    return;
  }
  for (const r of shown) {
    const el = document.createElement('div');
    el.className = 'mrow';
    el.title = r.path;
    el.innerHTML =
      `<span class="t">${monOffset(r.at_ms)}</span>` +
      `<span class="k ${r.kind}">${esc(r.kind_cn)}</span>` +
      `<span class="s">${esc(r.size_text)}</span>` +
      `<span class="p${r.is_dir ? ' dir' : ''}">${esc(monKeepTail(r.path, 120))}</span>` +
      `<span class="x">${r.suffix ? esc(r.suffix) : ''}</span>`;
    box.appendChild(el);
  }
}

/** 从后端环形缓冲回填（打开面板、切筛选、切页签时用）。 */
async function backfillMonitor() {
  mon.snap = await call('monitor_snapshot');
  mon.info = mon.snap.info || mon.info;
  mon.running = !!mon.snap.running;
  if (!mon.running && mon.info) {
    document.getElementById('mon-state').textContent = '已停止';
  }
  const page = await call('monitor_changes', {
    limit: 800,
    kind: mon.filter || null,
  });
  mon.rows = page.rows || [];
  mon.maxSeq = mon.rows.reduce((m, r) => Math.max(m, r.seq), 0);
  mon.alerts = await call('monitor_alerts', { limit: 300 });
  renderMonHead();
  renderMonList();
}

/** 一轮 tick：攒行 + 需要时重画面积图。 */
function onMonitorTick(t) {
  if (t.rows && t.rows.length) {
    // 增量拼接：只收比已见过更新的行（事件可能乱序/重复到达）
    const fresh = mon.filter ? t.rows.filter((r) => r.kind === mon.filter) : t.rows;
    const add = fresh.filter((r) => r.seq > mon.maxSeq);
    if (add.length) {
      mon.maxSeq = Math.max(mon.maxSeq, ...add.map((r) => r.seq));
      mon.rows = add.concat(mon.rows).slice(0, MON_ROW_CAP);
    }
  }
  if (t.alerts && t.alerts.length) {
    mon.alerts = t.alerts.concat(mon.alerts).slice(0, 300);
  }
  mon.snap = Object.assign({}, mon.snap || {}, {
    running: true,
    total_events: t.total_events,
    total_alerts: t.total_alerts,
    elapsed_ms: t.elapsed_ms,
    still_pending: t.still_pending,
    backpressure: (mon.snap && mon.snap.backpressure) || 0,
    error: (mon.snap && mon.snap.error) || null,
  });
  mon.running = true;

  // 状态栏跟着实时数字走（扫描摘要里的总量在监控期间会过期）
  if (t.totals) applyLiveTotals(t.totals);

  renderMonHead();
  if (mon.open) renderMonList();

  if (t.notification) toast(t.notification, true);
  if (t.truncated) toast(`一轮变更太多，列表只显示了最近 ${t.rows.length} 条`, true);
  if (t.backpressure) toast('事件过多：队列溢出，部分事件已丢弃（建议缩小监视范围）', true);

  // 代数变了 = 树被改过 → 面积图/文件夹树都过期了，重画（节流）
  if (t.generation && t.generation !== mon.lastGen) {
    mon.lastGen = t.generation;
    scheduleRedraw();
  }
}

/** 节流的重画：监控每 500ms 就可能改树，而一次布局是 O(节点数)。 */
function scheduleRedraw() {
  const now = Date.now();
  const gap = 1500;
  const wait = Math.max(0, gap - (now - mon.lastRedraw));
  if (mon.redrawTimer) return;
  mon.redrawTimer = setTimeout(async () => {
    mon.redrawTimer = null;
    mon.lastRedraw = Date.now();
    if (state.focus === null) return;
    try {
      await refreshLayout();
      await renderTree();
      // 类型聚合与 Top-N 都是 O(n) 重建，降一个数量级的频率
      mon.statsTick += 1;
      if (mon.statsTick % 4 === 0) {
        renderExts();
        renderTopFiles();
      }
    } catch (e) {
      /* 重画失败不该打断监控：下一次 tick 还会再来一次 */
    }
  }, wait);
}

/** 用树里的实时总量刷新状态栏（其余字段仍来自扫描摘要）。 */
function applyLiveTotals(t) {
  if (!state.summary) return;
  state.summary.dirs = t.dirs;
  state.summary.files = t.files;
  state.summary.logical = t.logical;
  state.summary.alloc = t.alloc;
  renderStatus();
}

async function startMonitor() {
  const paths = document.getElementById('mon-paths').value
    .split('\n').map((s) => s.trim()).filter(Boolean);
  if (!paths.length) {
    toast('先填监控目标：点「用当前视图根」或「用当前盘符」，也可以手填一个目录');
    monShow(true);
    return;
  }
  const rules = document.getElementById('mon-rules').value
    .split('\n').map((s) => s.trim()).filter(Boolean);

  const iv = Number(document.getElementById('mon-interval').value.trim());
  if (!Number.isFinite(iv) || iv < 100) {
    toast('轮询间隔要是一个 ≥ 100 的数字（毫秒）');
    return;
  }

  const changelog = document.getElementById('mon-changelog').value.trim();
  try {
    await call('monitor_start', {
      paths,
      rules,
      intervalMs: Math.round(iv),
      changelog: changelog || null,
      createJournal: document.getElementById('mon-journal').checked,
    });
    mon.running = true;
    mon.tab = 'changes';
    syncTabs();
    renderMonHead();
  } catch (e) {
    /* call() 已经提示过 */
  }
}

async function stopMonitor() {
  try {
    await call('monitor_stop');
  } catch (e) { /* 已提示 */ }
}

async function exportCsv() {
  let p = document.getElementById('mon-csv').value.trim();
  if (!p) {
    const dir = await call('monitor_default_export_dir');
    const d = new Date();
    const z = (n) => String(n).padStart(2, '0');
    const stamp = `${d.getFullYear()}${z(d.getMonth() + 1)}${z(d.getDate())}`
      + `-${z(d.getHours())}${z(d.getMinutes())}${z(d.getSeconds())}`;
    p = `${dir}\\filewatch-changes-${stamp}.csv`;
    document.getElementById('mon-csv').value = p;
  }
  try {
    const n = await call('monitor_export_csv', { path: p, kind: mon.filter || null });
    document.getElementById('mon-export-hint').textContent = `已导出 ${n} 条`;
    toast(`已导出 ${n} 条到 ${p}`);
  } catch (e) { /* 已提示 */ }
}

function syncTabs() {
  document.getElementById('mon-tab-changes').classList.toggle('on', mon.tab === 'changes');
  document.getElementById('mon-tab-alerts').classList.toggle('on', mon.tab === 'alerts');
  // 预警没有类型筛选，切过去时把筛选器藏掉，免得让人以为筛选没生效
  document.getElementById('mon-filter').style.visibility =
    mon.tab === 'changes' ? 'visible' : 'hidden';
}

/** 把当前视图根的真实路径填进监控目标。 */
async function fillPaths(kind) {
  const t = document.getElementById('mon-paths');
  try {
    if (kind === 'focus') {
      if (state.focus === null) { toast('还没有扫描结果'); return; }
      const info = await call('node_detail', { node: state.focus });
      t.value = info.path;
    } else {
      const s = state.summary;
      if (!s) { toast('还没有扫描结果'); return; }
      // 扫描根是盘符（`C:` 或 `C:\`）时补上尾反斜杠：`C:` 在 Windows 上
      // 是「C 盘的当前目录」，与 `C:\` 不是一回事
      const r = s.root || '';
      t.value = /^[A-Za-z]:\\?$/.test(r) ? r[0].toUpperCase() + ':\\' : r;
    }
  } catch (e) { /* 已提示 */ }
}

function wireMonitor() {
  document.getElementById('mon-toggle').onclick = () => monShow(!mon.open);
  document.getElementById('mon-start').onclick = startMonitor;
  document.getElementById('mon-stop').onclick = stopMonitor;
  document.getElementById('mon-export').onclick = exportCsv;
  document.getElementById('mon-use-focus').onclick = () => fillPaths('focus');
  document.getElementById('mon-use-vol').onclick = () => fillPaths('volume');

  document.getElementById('mon-tab-changes').onclick = () => {
    mon.tab = 'changes'; syncTabs(); renderMonList();
  };
  document.getElementById('mon-tab-alerts').onclick = () => {
    mon.tab = 'alerts'; syncTabs(); renderMonList();
  };
  document.getElementById('mon-filter').onchange = (e) => {
    mon.filter = e.target.value;
    backfillMonitor().catch(() => {});
  };
  syncTabs();
  renderMonHead();
}

/* ── 启动 ──────────────────────────────────────────────────────────────── */

async function boot() {
  Treemap.init({
    nameOf: cachedName,
    pathOf: cachedPath,
    sizeText: humanSize,
    onPick: pickNode,
    onZoomIn: async (node) => {
      // 只在是目录且有子项时下钻；文件方块双击等于选中
      const info = await call('node_detail', { node });
      if (info.is_dir && info.children > 0) await goto(node);
      else pickNode(node);
    },
    onZoomOut: async () => {
      if (state.crumbs.length > 1) {
        await goto(state.crumbs[state.crumbs.length - 2].node);
      } else {
        toast('已经在最外层了');
      }
    },
  });

  await loadVolumes();
  await loadElevation();
  wireAdminModal();
  wireMonitor();
  backfillMonitor().catch(() => {});

  document.getElementById('refresh-vols').onclick = loadVolumes;
  document.getElementById('scan-root').onclick = () => {
    const v = document.getElementById('custom-root').value.trim();
    if (!v) { toast('先填一个目录路径'); return; }
    const drive = (v.match(/^([A-Za-z]):/) || [])[1];
    startScan(drive ? drive.toUpperCase() + ':' : v, drive ? v : null);
  };
  document.getElementById('mode').onchange = (e) => {
    state.mode = e.target.value;
    refreshLayout(); renderTree(); renderExts(); renderTopFiles();
  };
  document.getElementById('shading').onchange = (e) => {
    state.shading = e.target.value;
    refreshLayout();
  };
  document.getElementById('detail').oninput = (e) => {
    state.detail = Number(e.target.value);
    refreshLayout();   // 细节只影响布局，不需要重新扫描
  };
  document.getElementById('clear-highlight').onclick = () => {
    state.highlight = null;
    Treemap.setHighlight(null);
    document.getElementById('clear-highlight').classList.add('hidden');
    renderExts();
  };

  // 后端事件
  await listen('scan:progress', (ev) => progressUI(ev.payload));
  await listen('scan:done', (ev) => afterScan(ev.payload, null));
  await listen('scan:error', (ev) => afterScan(null, String(ev.payload)));
  await listen('admin:relaunch', (ev) => onRelaunchOutcome(ev.payload || {}));

  // 监控
  await listen('monitor:started', (ev) => {
    mon.info = ev.payload;
    mon.running = true;
    monShow(true);
    if (ev.payload && ev.payload.reopened) {
      toast('为建立增量来源，已重新扫描一次（监控需要一个事件源）');
    }
    for (const w of (ev.payload && ev.payload.warnings) || []) toast(w);
    renderMonHead();
  });
  await listen('monitor:tick', (ev) => onMonitorTick(ev.payload || {}));
  await listen('monitor:stopped', (ev) => {
    mon.running = false;
    mon.snap = ev.payload || mon.snap;
    // 收尾后索引稳定了，一次性把面板刷到最新
    renderMonHead();
    backfillMonitor().catch(() => {});
    refreshLayout().catch(() => {});
    renderTree().catch(() => {});
    renderExts().catch(() => {});
    renderTopFiles().catch(() => {});
  });
  await listen('monitor:error', (ev) => {
    mon.running = false;
    mon.snap = Object.assign({}, mon.snap || {}, { error: String(ev.payload) });
    renderMonHead();
    renderMonList();
    toast('监控出错：' + String(ev.payload), true);
  });

  // 进入软件时申请一次管理员权限。
  // 判断本身在 admin-rule.js 里（那是个能被 tools/check-admin.js 跑真值表的
  // 纯函数）—— 这里只做分派，别再往 if 里加条件。
  const prefs = await loadPrefs();
  const action = fwAdminBootAction(adminInfo, prefs);
  if (action === 'ask') {
    await openAdminModal(true);
  } else if (action === 'notify-failed') {
    toast('提权未生效，已按降级模式启动（可点右下角「权限」再试一次）');
  }
}

boot().catch((e) => toast('界面初始化失败：' + (e && e.message ? e.message : e), true));
