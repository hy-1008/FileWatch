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
  /** node → 名字。**与 `meta` 分开**：面积图一次要几百上千个名字，
   *  批量取回来的只有名字（没有 path / 详情），混进 `meta` 会让
   *  `cachedPath` 读到 undefined，提示框里就出现 "undefined"。 */
  names: new Map(),
  crumbs: [],
  /** 被选中的节点 */
  picked: null,
  /** 高亮的扩展名 id（null = 不高亮） */
  highlight: null,
  mode: 'logical',
  shading: 'ext',
  detail: 1,
  scanning: false,
  /** 当前中间区的视图：map / dup / diff */
  view: 'map',
  /** 运行模式的键名（"lean" / "balanced" / "performance"） */
  profile: 'balanced',
  /** 运行模式派生出来的数字。**由后端下发**（`profile_budget`），
   *  前端不自己换算 —— 那正是"切了模式但什么都没变"的来路。 */
  budget: null,
  /** 开机自启动现状（后端读注册表） */
  autostart: null,
  /** 点右上角「×」时做什么：`'quit'` 退出所有进程 / `'tray'` 最小化到托盘。
   *  判定在后端（读落盘偏好），这里只是"上次选了什么"的回填值。 */
  closeBehavior: 'quit',
};

/** 「细节」滑块 → LOD 的 min_side 阈值。
 *  后端默认 80px；往左更精简（只展开大块），往右展开更深。
 *  最终值还要乘上模式的 `lod_scale`（见 `lodMinSide`）。 */
const DETAIL_MIN_SIDE = [240, 80, 32, 12];

/* ── 运行模式 ──────────────────────────────────────────────────────────────
 *
 * 「轻量」和「性能」差的是一串具体数字：面积图铺多细、一次取多少行、扫不扫
 * 后台。这些数字全部来自后端的 `profile_budget`。
 *
 * 这里只做一件事：把「细节」滑块给的阈值乘上模式的 `lod_scale`。
 * **行数不在前端换算** —— 列表命令收到的是基数，缩放由后端做
 * （`commands::row_limit`）。两边都算一遍的话，"切了模式但没变化"迟早发生。
 */

function lodMinSide() {
  const base = DETAIL_MIN_SIDE[state.detail] || DETAIL_MIN_SIDE[1];
  return base * (state.budget ? state.budget.lod_scale : 1);
}

/** 浏览器空闲时再做（轻量模式把侧栏拖到这时加载，先出面积图）。 */
function whenIdle(fn) {
  if (typeof window.requestIdleCallback === 'function') {
    window.requestIdleCallback(() => fn(), { timeout: 800 });
  } else {
    setTimeout(fn, 120);
  }
}

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

/* 时间只走 `fwLocalTime`（定义在 view-rule.js，带 Invalid Date 与范围保护）。
 *
 * 这里原来另有一份 `fmtTime`：同样的功能，但没有 Invalid Date 保护 ——
 * 遇到坏掉的时间戳（例如垃圾 FILETIME 读出来的巨大负数）会直接印出
 * `NaN-NaN-NaN NaN:NaN`，而它恰好被「详情」「重复文件」「增长归因」用着。
 * 两份实现迟早分叉，所以只留一份。 */

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
  renderVolumes();
}

/** 只负责按 `state.volumes` 画画。容量刷新（`volumes:tick`）与重新拉取
 *  （`loadVolumes`）都走这里 —— 两条路渲染出的卡片必须一模一样，否则
 *  "后台刷新一下"就会让界面悄悄变个样子。 */
function renderVolumes() {
  const box = document.getElementById('volumes');
  box.innerHTML = '';
  if (!state.volumes || !state.volumes.length) {
    box.innerHTML = '<div class="empty">（没有检测到卷）</div>';
    renderSpace();
    return;
  }
  for (const v of state.volumes) {
    const el = document.createElement('div');
    el.className = 'vol';
    // 折叠态把容量那几行都藏了，所以悬停提示要把它们带上 —— 那时候它是唯一出口
    el.title = `${v.root}　${v.fs_name}　${v.label || '(无卷标)'}\n` +
               `总共 ${humanSize(v.total)}　可用 ${humanSize(v.free)}`;
    // 选中态靠这个字段对上，而不是去比文本或 title —— 那两种都容易被改
    el.dataset.drive = v.drive;
    const canMft = v.supports_mft && v.elevated;
    // 已用比例交给 CSS 变量：展开态是横条的宽度，折叠态是竖条的水位高度
    el.style.setProperty('--used', `${(v.used_ratio * 100).toFixed(1)}%`);
    el.innerHTML =
      `<div class="top"><span class="drive">${esc(v.drive)}</span>` +
      `<span class="fs">${esc(v.fs_name)}</span></div>` +
      // 折叠态竖排用的单个字母（`C:` 竖起来那个冒号很碍眼）
      `<span class="glyph">${esc(v.drive.replace(/[^A-Za-z]/g, ''))}</span>` +
      `<div class="label">${esc(v.label || '(无卷标)')}</div>` +
      `<div class="bar"><i></i></div>` +
      `<div class="cap"><span class="k">总共</span><span>${humanSize(v.total)}</span></div>` +
      `<div class="cap"><span class="k">可用</span><span>${humanSize(v.free)}</span></div>` +
      (canMft ? '<div class="fs">MFT 直读</div>'
              : `<div class="noadmin">${v.supports_mft ? '需管理员才能 MFT 直读，将降级' : '非 NTFS，走遍历'}</div>`);
    el.onclick = () => startScan(v.drive, null);
    ContextMenu.bind(el, { kind: 'volume', volume: v });
    box.appendChild(el);
  }
  // 重列卷之后（比如插了个 U 盘）选中态不该丢
  markActiveDrive(state.summary ? state.summary.root : null);
  // 状态栏的「可用 / 总空间」跟着一起走：容量刷新的唯一出口
  renderSpace();
}

/** 底部状态栏的「可用 / 总空间」。
 *
 * 来源是**盘符容量**（`list_volumes`），不是索引里的字节数 —— 后者是"扫到的
 * 文件加起来多大"，前者才是"这块盘还剩多少"。两件事，别混着显示。
 * 没扫描时没有"当前盘"这个概念，就老实显示 `—`：拿第一块盘兜底会让用户
 * 以为那是他正在看的那块。 */
function renderSpace() {
  const el = document.getElementById('st-space');
  if (!el) return;
  const target = (state.summary && state.summary.root) || null;
  const v = fwVolumeOfTarget(state.volumes || [], target);
  if (!v) {
    el.textContent = '—';
    el.title = '当前盘符的可用空间 / 总容量';
    return;
  }
  el.textContent = `可用 ${humanSize(v.free)} / 共 ${humanSize(v.total)}（已用 ${pct(v.used_ratio)}）`;
  el.title = `${v.root}　${v.label || '(无卷标)'}　${v.fs_name}`;
}

/** 点亮"当前正在看的这块盘"的卡片；不属于任何卷时一张都不点亮。
 *
 * 调用点只有两处：开始扫描时用目标（立刻给反馈，不然点了卡片半天没动静），
 * 扫描结束后用后端回报的 root（以它为准 —— 粘贴的目录和点的卡片可能不是
 * 同一个）。判断交给 `view-rule.js`，这里只负责开关类名。 */
function markActiveDrive(target) {
  const want = fwDriveOfTarget(target);
  document.querySelectorAll('.vol').forEach((el) => {
    el.classList.toggle('active', want !== null && el.dataset.drive === want);
  });
}

/* ── 盘符栏折叠：把横向空间让给文件夹树 ──────────────────────────────────
 *
 * 这里只做两件事：给 `body` 挂一个类、把这个选择存进偏好。宽度怎么变、卡片
 * 怎么竖过来，全在 style.css 里 —— 中间区与右栏一条规则都不加，所以画布
 * 尺寸不变，**不需要** refreshLayout()（那会白算一遍布局）。
 */
function applyDrivesCollapsed(on) {
  document.body.classList.toggle('drives-collapsed', on);
  const btn = document.getElementById('drives-toggle');
  btn.setAttribute('aria-expanded', String(!on));
  btn.title = on ? '展开盘符栏' : '折叠盘符栏（把宽度让给文件夹树）';
}

async function toggleDrives() {
  const on = !document.body.classList.contains('drives-collapsed');
  applyDrivesCollapsed(on);
  await savePrefs({ drives_collapsed: on });
}

/* ── 预警区折叠：把纵向空间让给变更流水 ────────────────────────────────────
 *
 * 与盘符栏那套同样的分工：这里只挂一个类 + 存偏好，高度怎么分在 style.css。
 * 折叠后**条数仍然显示在标题栏**，所以预警不会因此被藏没 —— 折叠的是列表，
 * 不是这条信息。
 */
function applyAlertsCollapsed(on) {
  document.getElementById('monitor-alerts').classList.toggle('collapsed', on);
  const btn = document.getElementById('alerts-toggle');
  btn.setAttribute('aria-expanded', String(!on));
  btn.title = on ? '展开预警区' : '折叠预警区（把高度让给变更流水）';
}

async function toggleAlerts() {
  const on = !document.getElementById('monitor-alerts').classList.contains('collapsed');
  applyAlertsCollapsed(on);
  await savePrefs({ alerts_collapsed: on });
}

/* ── F2 扫描 ───────────────────────────────────────────────────────────── */

function setScanning(on) {
  if (on) ContextMenu.close();
  state.scanning = on;
  document.getElementById('scan-overlay').classList.toggle('hidden', !on);
  document.getElementById('menu-scan').disabled = on || mon.running || mon.starting;
  document.getElementById('select-folder').disabled = on || mon.running || mon.starting;
  document.querySelectorAll('.vol').forEach((e) => e.classList.toggle('disabled', on));
}

/** 扫「当前目录」= 现在正在看的那个根。没有扫描结果时给一句能照做的话。 */
function scanCurrentFolder() {
  if (state.scanning || mon.running || mon.starting) {
    toast('请先结束当前扫描或监控'); return;
  }
  const value = state.summary && state.summary.root;
  if (!value) { toast('先在左侧点一个盘符，或用「文件 → 选择文件夹…」挑一个目录'); return; }
  const drive = (value.match(/^([A-Za-z]):/) || [])[1];
  document.getElementById('file-menu').open = false;
  startScan(drive ? drive.toUpperCase() + ':' : value, drive ? value : null).catch(() => setScanning(false));
}

/** 选文件夹：**选完就扫**。
 *
 * 以前是"选完填进输入框、再点一次扫描"。那个输入框去掉之后，如果这里还停在
 * "填进去"那一步，用户点完选择器就什么都不会发生 —— 多一次点击却少了一个
 * 入口，是最坏的组合。取消选择器则什么都不做（连扫描也不许起）。 */
async function chooseFolder() {
  const button = document.getElementById('select-folder');
  button.disabled = true;
  document.getElementById('file-menu').open = false;
  try {
    const path = await call('choose_path', { kind: 'folder', title: '选择要扫描的文件夹' });
    if (!path) return;
    const drive = (path.match(/^([A-Za-z]):/) || [])[1];
    if (!drive) { toast('请选择一个本地磁盘上的文件夹'); return; }
    await startScan(drive.toUpperCase() + ':', path);
  } catch (e) { setScanning(false); }
  finally { button.disabled = state.scanning || mon.running || mon.starting; }
}

function wireMenus() {
  const menus = document.querySelectorAll('.menu');
  for (const menu of menus) {
    menu.addEventListener('toggle', () => {
      if (menu.open) for (const other of menus) if (other !== menu) other.open = false;
    });
    menu.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') {
        event.stopPropagation(); menu.open = false; menu.querySelector('summary').focus();
      }
    });
  }
  document.addEventListener('pointerdown', (event) => {
    for (const menu of menus) if (!menu.contains(event.target)) menu.open = false;
  });
  document.getElementById('select-folder').onclick = chooseFolder;
  document.getElementById('menu-scan').onclick = scanCurrentFolder;
  document.getElementById('menu-report').onclick = exportReport;
  document.getElementById('menu-refresh').onclick = () => {
    document.getElementById('file-menu').open = false; loadVolumes();
  };
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
  if (mon.running || mon.starting) { toast('请先停止实时监控再扫描'); return; }
  setScanning(true);
  document.getElementById('progress-text').textContent = '准备中…';
  document.getElementById('progress-bar').style.width = '2%';
  // 立刻点亮：等后端回报 root 是要等一次扫描的，期间界面得有反应
  markActiveDrive(root || drive);
  try {
    await call('scan_start', {
      drive,
      root: root || null,
      // 「同时建立实时监视」原本是左下角那个复选框，随那块一起去掉了。
      // 它现在是模式的一部分：性能模式默认建好（省一次重扫），其它模式不建 ——
      // 也就是 `profile_budget` 下发下来的 `default_watch`，前端不再有第二份口径。
      watch: !!(state.budget && state.budget.default_watch),
    });
  } catch (e) { setScanning(false); }
}

async function afterScan(summary, errorMsg) {
  setScanning(false);
  // E7：扫描结束（无论成败）都收起半成品视图 —— 它只是"扫的时候看一眼"，
  // 留着会挡住真正的面积图，而且数据已经过时了。
  hideScanPreview();
  if (errorMsg) {
    state.summary = null;
    setBadge('扫描失败', '');
    document.getElementById('st-paths').textContent = errorMsg;
    // 扫描没成功就没有"正在看的盘"，别让卡片一直亮着
    markActiveDrive(null);
    toast(errorMsg, true);
    return;
  }
  state.summary = summary;
  state.highlight = null;
  state.picked = null;
  Treemap.setHighlight(null);

  setBadge(summary.backend, summary.backend.includes('MFT') ? 'mft' : 'traverse');
  // 以后端回报的 root 为准：用户可能点的是卡片、但扫的是粘贴的目录
  markActiveDrive(summary.root);

  // 视图根 = 索引树的根节点。面板与面积图都从这里开始。
  state.rootNode = null;
  state.focus = null;
  state.meta.clear();

  // 从扫描根开始。**不要**硬编码节点号：虽然 NTFS 的根目录就是 MFT 记录 5，
  // 但那是引擎的实现细节，后端在摘要里已经把它带过来了。
  await goto(summary.root_node, true);
  renderStatus();
  // 轻量模式把侧栏拖到空闲时再加载：先出面积图，用户立刻有东西看。
  // 其余模式一起加载 —— 那两块本来就是右栏的主要内容，晚出来反而像卡了。
  if (state.budget && state.budget.lazy_panels) {
    whenIdle(() => { renderExts(); renderTopFiles(); });
  } else {
    renderExts();
    renderTopFiles();
  }
  renderDetail(null);
  // 扫完再读一次容量：扫了大半天之后，"还剩多少"这个数字早就变了，
  // 拿启动时的那份来显示就是过期值。
  loadVolumes().catch(() => {});
  document.getElementById('empty-hint').classList.add('hidden');
  for (const w of summary.warnings || []) toast(w);
}

function setBadge(text, cls) {
  const b = document.getElementById('backend-badge');
  b.textContent = String(text).includes('MFT') ? 'MFT 直读（极速）' : text;
  b.className = 'badge' + (cls ? ' ' + cls : '');
}

function renderStatus() {
  const s = state.summary;
  const st = (id, v) => { document.getElementById(id).textContent = v; };
  if (!s) {
    ['st-counts', 'st-size', 'st-memory', 'st-time', 'st-layout'].forEach((i) => st(i, '—'));
    return;
  }
  setBadge(s.backend, s.backend.includes('MFT') ? 'mft' : 'traverse');
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
    // 模式的倍率在这里生效：轻量铺得粗（矩形少 → 布局结果与 IPC 字节都少），
    // 性能铺得细。后端还会再夹一次范围，前端算错也不至于把界面卡死。
    min_side: lodMinSide(),
  };
}

let layoutRequest = 0;
async function refreshLayout() {
  const request = ++layoutRequest;
  if (state.focus === null) return;
  // 画布被别的视图挡住时它的 clientWidth/Height 是 0，这时候布局出来是一张
  // 1×1 的图。与其算一遍垃圾再算一遍，不如直接跳过 —— 切回面积图时
  // setView 会重新算一次。
  if (state.view !== 'map') return;
  const { w, h } = Treemap.size();
  const args = layoutArgs(w, h);
  const buf = await call('layout', { args });
  if (request !== layoutRequest || state.view !== 'map') return;
  const l = new Layout(buf instanceof ArrayBuffer ? buf : new Uint8Array(buf).buffer);
  // 等布局和文字都备好，再一次提交完整帧；IPC 等待期间保留旧画面。
  await primeNames(l, () => request === layoutRequest);
  if (request !== layoutRequest || state.view !== 'map') return;
  Treemap.setLayout(l, { w, h });
  document.getElementById('tree-hint').textContent = `${l.count.toLocaleString()} 个方块`;

  // 布局统计（会重跑一次布局，所以只在这里按需调用，不进每帧路径）
  const info = await call('layout_info', { args });
  if (request !== layoutRequest) return;
  document.getElementById('canvas-status').textContent =
    `绘制 ${info.emitted.toLocaleString()} 个矩形　` +
    `太小折叠 ${info.collapsed_small.toLocaleString()}　` +
    `深度折叠 ${info.collapsed_depth.toLocaleString()}　` +
    `最大深度 ${info.max_depth_reached}` +
    (info.hit_budget ? '　⚠ 已触及节点预算上限' : '');
}

/** 跳转到某个节点：刷新面包屑 + 重新布局。`reset` 用于扫描完成后的首次进入。 */
let navigationRequest = 0;
async function goto(node, reset) {
  const request = ++navigationRequest;
  state.focus = node;
  if (reset) state.rootNode = node;
  const [crumbs, info] = await Promise.all([call('breadcrumb', { node }), call('node_detail', { node })]);
  if (request !== navigationRequest) return;
  state.crumbs = crumbs;
  document.getElementById('tree-path').value = info.path;
  renderBreadcrumb();
  // **先画文件夹树，再算面积图。**

  // 用户点一个目录，最直接的反馈是左边那份列表。布局虽然后端只要几毫秒，
  // 但前后还有取名字、统计两次往返 —— 让列表排队等它就是白等：点完目录
  // 要顿一下才有反应。两边的数据来源互不相干，顺序反了纯属浪费。
  await renderTree();
  await refreshLayout();
}

async function goTreePath() {
  if (state.focus === null) { toast('请先扫描一个目录'); return; }
  const path = document.getElementById('tree-path').value.trim();
  if (!path) return;
  try {
    const node = await call('find_by_path', { path });
    if (node === null || node === undefined) { toast('扫描结果中没有这个文件夹，请检查路径或扫描范围', true); return; }
    setView('map');
    await goto(node);
  } catch (error) { toast(String(error), true); }
}

/** 需要显示名字时向后端要一次并缓存 —— 布局里只有 id。 */
async function metaOf(node) {
  if (state.meta.has(node)) return state.meta.get(node);
  const info = await call('node_detail', { node });
  const v = { name: info.name === '.' ? info.path : info.name, path: info.path, info };
  state.meta.set(node, v);
  state.names.set(node, v.name);
  return v;
}

/**
 * 在提交候选布局之前，一次取齐要显示的名字；过期结果不写入缓存。
 *
 * 以前是每个方块各发一次 `node_detail`，而且每回来一个名字就 `Treemap.redraw()`
 * 一次；一次布局有几千个方块时，这就成了几千次跨语言往返 + 几千次全量重画
 * （每次重画又要重新走一遍所有方块去问名字，重复请求还会互相触发）。
 * 后端其实只要 240 µs —— 慢的全是这一层。用户报的「文件夹树列表加载太慢」
 * 就是这个：`goto()` 里列表要等布局，而布局要等这一串名字风暴。
 *
 * 返回是否取回了新的名字。
 */
async function primeNames(candidate, isCurrent = () => true) {
  const todo = Treemap.labelNodes(candidate).filter(
    (n) => !state.names.has(n) && !state.meta.has(n));
  if (!todo.length) return false;
  const arr = await call('node_names', { nodes: todo });
  if (!isCurrent()) return false;
  for (let i = 0; i < todo.length; i++) {
    // 空串 = 后端找不到这个节点（已被删除），不要缓存成「有名字但为空」
    if (arr[i]) state.names.set(todo[i], arr[i]);
  }
  return true;
}

/** 面积图画字时只读缓存 —— **不要在这里发请求**：绘制会被高频调用
 *  （每帧/每次悬停），在里面做 IO 就是前面那个名字风暴的成因。
 *  名字由 `primeNames()` 在布局之后一次取齐。 */
function cachedName(node) {
  const m = state.meta.get(node);
  if (m) return m.name;
  return state.names.get(node) || '';
}

function cachedPath(node) {
  const m = state.meta.get(node);
  if (m) return m.path;
  metaOf(node).then(() => {}).catch(() => {});
  return `#${node}`;
}

/** 年龄模式的热力图图例。只有 `age` 模式需要解释颜色 —— 类型着色的色块
 *  在「文件类型」列表里本来就有一份，不需要再讲一遍。
 *
 *  色带取自 `FW_HEAT`（与画面同一份调色点），所以图例永远和画面对得上。 */
function renderHeatLegend() {
  const box = document.getElementById('heat-legend');
  const bar = document.getElementById('heat-bar');
  const on = state.shading === 'age';
  box.classList.toggle('hidden', !on);
  if (!on || bar.childElementCount === FW_HEAT.length) return;
  bar.innerHTML = '';
  for (const c of FW_HEAT) {
    const i = document.createElement('i');
    i.style.background = c;
    bar.appendChild(i);
  }
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
    ContextMenu.bind(el, { kind: 'node', node: c.node });
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
      const kids = await call('tree_rows', { parent: c.node, offset: 0, limit: 200, mode: state.mode, profile: state.profile });
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
  ContextMenu.bind(el, { kind: 'node', node });
  return el;
}

/* ── F5 类型列表 ───────────────────────────────────────────────────────── */

async function renderExts() {
  const rows = await call('ext_rows', { limit: 60, mode: state.mode, profile: state.profile });
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
    ContextMenu.bind(el, { kind: 'type', row: e });
    box.appendChild(el);
  }
  if (!rows.length) box.innerHTML = '<div class="dim">（无数据）</div>';
}

/* ── F12 大文件 ────────────────────────────────────────────────────────── */

async function renderTopFiles() {
  const rows = await call('top_files', { limit: 200, mode: state.mode, profile: state.profile });
  const box = document.getElementById('topfiles');
  box.innerHTML = '';
  for (const f of rows) {
    const el = document.createElement('div');
    el.className = 'file';
    el.title = `${f.path}\n双击前往所在文件夹`;
    el.innerHTML = `<span class="nm">${esc(f.name)}</span><span class="sz">${humanSize(f.size)}</span>`;
    el.onclick = () => pickNode(f.node);
    ContextMenu.bind(el, { kind: 'node', node: f.node });
    el.ondblclick = async () => {
      try {
        const parent = await call('node_parent', { node: f.node });
        monShow(false); setView('map');
        await goto(parent);
        await pickNode(f.node);
      } catch (error) { toast(String(error), true); }
    };
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
  box.oncontextmenu = null; box.onkeydown = null; box.tabIndex = -1;
  if (!info) {
    box.className = 'detail dim';
    box.textContent = '点选任意方块查看';
    return;
  }
  box.className = 'detail';
  ContextMenu.bind(box, { kind: 'node', node: info.node });

  // 索引里的 MFT 属性位与现场 stat 的属性位是**两套位**，不能混着解：
  // 前者来自扫描时的 $FILE_NAME / $STANDARD_INFORMATION，后者是此刻的
  // dwFileAttributes。分两行显示，就是为了不让人把"扫描时压缩"读成"现在压缩"。
  const mftTags = attrChips(info.flags, FW_NODE_BITS);
  const diskTags = info.disk ? attrChips(info.disk.attributes, FW_ATTR_BITS) : '';

  let diskBlock = '';
  if (!info.disk) {
    diskBlock = `<p class="detail-note">文件现在读不到（可能已被删除或没有权限）。`
      + `下面的数字来自扫描那一刻的索引。</p>`;
  } else {
    const d = info.disk;
    // 现场大小与索引不同时要说出来：这正是"扫描之后它又被写过"的证据。
    // **目录不比**：索引里目录的 size 是"子孙合计"，而 stat 拿到的是目录项
    // 自己的 4 KB —— 两者永远不等，标出来只会让每个目录都看起来"变过"。
    const drift = !info.is_dir && d.size !== info.size
      ? `<span class="drift">现已 ${humanSize(d.size)}</span>` : '';
    // 硬链接数：Windows 的 `std` 不提供这个字段（`fsmeta::nlink` 返回 0）。
    // 显示"0 个"是在撒谎，所以 0 就整行不出现。
    const linkRow = d.nlink > 0
      ? `<tr><td>硬链接</td><td>${d.nlink.toLocaleString()} 个${d.nlink > 1 ? '（删一个名字不会释放空间）' : ''}</td></tr>`
      : '';
    diskBlock =
      `<div class="detail-sec">系统查询（点击时）</div>` +
      `<table>` +
      `<tr><td>创建</td><td>${d.ctime ? esc(fwLocalTime(d.ctime)) : '—'}</td></tr>` +
      linkRow +
      `<tr><td>逻辑大小</td><td>${humanSize(d.size)} ${drift}</td></tr>` +
      `</table>` +
      (diskTags ? `<div class="chips">${diskTags}</div>` : '');
  }

  box.innerHTML =
    `<table>` +
    `<tr><td>名称</td><td>${esc(info.name === '.' ? info.path : info.name)}</td></tr>` +
    `<tr><td>类型</td><td>${info.is_dir ? '目录' : '文件'}</td></tr>` +
    `<tr><td>逻辑</td><td>${humanSize(info.size)}</td></tr>` +
    `<tr><td>占用</td><td>${humanSize(info.alloc)}</td></tr>` +
    (info.is_dir ? `<tr><td>子项</td><td>${info.children.toLocaleString()} 个，含 ${info.files.toLocaleString()} 个文件</td></tr>` : '') +
    `<tr><td>深度</td><td>${info.depth}</td></tr>` +
    `<tr><td>修改</td><td>${fwLocalTime(info.mtime)}</td></tr>` +
    // 引用号：改名不变、移动不变，只有删除重建才变 —— 用它判断"还是不是同一个文件"
    `<tr><td>引用号</td><td class="mono" title="序列号 ${info.seq} · 记录号 ${info.node}">${esc(info.frn)}</td></tr>` +
    `<tr><td>路径</td><td>${esc(info.path)}</td></tr>` +
    `</table>` +
    `<div class="detail-sec">MFT 属性（扫描时）</div>` +
    (mftTags ? `<div class="chips">${mftTags}</div>` : '<p class="detail-note">没有特殊属性。</p>') +
    diskBlock;
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
    // 与后端 `GuiPrefs::default()` 同口径（含 close_behavior），别只写一半：
    // 少一个字段就等于"用 undefined 走一遍"，而 undefined 不是任何一种合法值。
    return {
      ask_admin_on_start: true, drives_collapsed: false, alerts_collapsed: false,
      profile: 'balanced', periodic_scan: true, periodic_minutes: 60, close_behavior: 'quit',
    };
  }
}

/** 存偏好的**局部**改动。
 *
 * 后端 `set_gui_prefs` 收到的是一整个 `GuiPrefs`（整体替换），所以这里必须
 * 读-改-写：只把 patch 递过去的话，**其余**偏好会被默认值顶掉 —— 表现是
 * 「切换一下盘符栏折叠，用户之前勾的『不再自动询问权限』就没了，下次启动
 * 又弹窗」。这种错不抛任何异常，只能靠这条注释和它挡住。 */
async function savePrefs(patch) {
  try {
    const next = { ...(await loadPrefs()), ...patch };
    await invoke('set_gui_prefs', { value: next });
  } catch (e) {
    toast('偏好没能保存：' + (typeof e === 'string' ? e : (e && e.message) || e), true);
  }
}

/* ── 运行模式（轻量 / 标准 / 性能） ─────────────────────────────────────────
 *
 * 三个名字是界面上的，具体数字（`budget`）**全部由后端下发**。这里不做换算：
 * 一旦前端自己抄一份倍率，"切了模式但什么都没变"就迟早会发生，而且不报错。
 */

/** 应用一个模式：取数字 → 存偏好 → 让已经画出来的东西按新数字重画。 */
async function applyProfile(key, opts) {
  const silent = opts && opts.silent;
  state.profile = key;
  const select = document.getElementById('profile');
  if (select) select.value = key;
  try {
    state.budget = await invoke('profile_budget', { profile: key });
  } catch (e) {
    // 取不到就按标准跑（不放大也不缩小），界面仍然可用 ——
    // 一个"取参数失败"不该让整扇窗废掉
    state.budget = null;
    if (!silent) toast('没能取到模式参数，已按标准模式继续', true);
    return;
  }
  if (!silent) {
    // 代价必须说出来：轻量不是免费的（看得粗），性能也不是（后台吃 IO）
    toast(state.budget.cost_cn);
  }
  refreshLayout().catch(() => {});
  if (state.summary) {
    renderTree().catch(() => {});
    renderExts().catch(() => {});
    renderTopFiles().catch(() => {});
  }
}

async function onProfileChange(e) {
  const key = e.target.value;
  await applyProfile(key);
  await savePrefs({ profile: key });
}

/* ── 开机自启动 · 关闭软件（都在「模式」菜单里） ─────────────────────────────
 *
 * 开机自启的真相在注册表里（HKCU 的 Run 键），**不进偏好文件**：用户在任务
 * 管理器里关掉之后，偏好文件里还写着"开"就成了骗人的显示。所以每次打开面板
 * 都重新问后端一次 —— 显示的是"系统现在是什么样"，不是"上次点成了什么样"。
 */

function renderAutostart(info) {
  state.autostart = info;
  const box = document.getElementById('autostart-toggle');
  if (!box) return;
  box.checked = !!info.enabled;
  box.disabled = false;
  // 状态说不清（比如注册表里的路径指向了别处）时把说明挂到整行上：
  // 只改一个勾，用户看不出"开了，但可能开的是另一个位置"。
  const row = document.getElementById('autostart-row');
  if (row) {
    row.title = info.enabled
      ? (info.stale ? info.note : '已开启：登录 Windows 时自动启动（写在 HKCU 的 Run 键里，不需要管理员）')
      : '勾选后登录 Windows 时自动启动（写在 HKCU 的 Run 键里，不需要管理员）';
  }
}

async function loadAutostart() {
  try {
    renderAutostart(await invoke('autostart_status'));
  } catch (e) {
    const box = document.getElementById('autostart-toggle');
    if (box) { box.checked = false; box.disabled = true; }
    const row = document.getElementById('autostart-row');
    if (row) row.title = `没能读到开机自启状态：${e}`;
  }
}

async function toggleAutostart() {
  const want = !(state.autostart && state.autostart.enabled);
  const box = document.getElementById('autostart-toggle');
  if (box) box.disabled = true;
  try {
    // 显示的是**操作之后的真实状态**，不是"用户刚才点的那个值"：
    // 写注册表可能被系统策略拦下，那种情况下如实说没开成，比假装成功好。
    const info = await invoke('set_autostart', { enable: want });
    renderAutostart(info);
    toast(info.enabled ? '已开启开机自启动' : '已关闭开机自启动');
    if (info.stale) toast(info.note, true);
  } catch (e) {
    toast('改开机自启动失败：' + (typeof e === 'string' ? e : (e && e.message) || e), true);
    await loadAutostart();
  } finally {
    if (box) box.disabled = false;
  }
}

/* ── 关闭软件：最小化到托盘 / 关闭所有进程 ───────────────────────────────────
 *
 * 判定在**后端**（`tray::handle_close_requested` 读落盘的偏好），前端只负责
 * 把选择存进偏好文件：关窗口那一刻界面可能正忙（扫描中、事件洪水），
 * 那种时候"点 × 之后去哪"不该依赖前端还能不能跑。
 *
 * 两条路的出口都留着：选了托盘之后，退出走托盘图标的右键菜单 —— 那条路
 * 不受这个选择影响，一直在，所以"藏到托盘"不会把程序变成关不掉的幽灵。 */

const CLOSE_BEHAVIORS = ['quit', 'tray'];

function applyCloseBehavior(key) {
  const value = CLOSE_BEHAVIORS.includes(key) ? key : 'quit';
  state.closeBehavior = value;
  for (const id of ['close-tray', 'close-quit']) {
    const radio = document.getElementById(id);
    if (radio) radio.checked = radio.value === value;
  }
  return value;
}

async function onCloseBehaviorChange(e) {
  const value = applyCloseBehavior(e.target.value);
  await savePrefs({ close_behavior: value });
  toast(value === 'tray'
    ? '以后点右上角「×」只收进托盘，监控继续跑（退出用托盘图标的右键菜单）'
    : '以后点右上角「×」直接退出所有进程');
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
  revision: 0,
  backfillRequest: 0,
  starting: false,
  stopping: false,
  lastTickElapsed: -1,
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
  document.body.classList.toggle('monitor-active', open);
  document.getElementById('monitor').classList.toggle('hidden', !open);
  document.getElementById('tree-pane').classList.toggle('hidden', open);
  document.getElementById('nav-tree').classList.toggle('on', !open);
  document.getElementById('nav-monitor').classList.toggle('on', open);
  document.getElementById('nav-tree').setAttribute('aria-selected', String(!open));
  document.getElementById('nav-monitor').setAttribute('aria-selected', String(open));
  document.getElementById('analysis-types').classList.toggle('hidden', open);
  document.getElementById('analysis-top').classList.toggle('hidden', open);
  document.getElementById('monitor-changes').classList.toggle('hidden', !open);
  document.getElementById('monitor-alerts').classList.toggle('hidden', !open);
  if (open) {
    backfillMonitor().catch(() => {});
    refreshIndependent().catch(() => {});
  }
}

function renderMonHead() {
  const s = mon.snap;
  const i = mon.info;
  const set = (id, v) => { document.getElementById(id).textContent = v; };

  const b = document.getElementById('mon-state');
  b.textContent = mon.starting ? '启动中' : mon.stopping ? '停止中' : mon.running ? '监控中' : (s && s.error ? '监控异常' : i ? '已停止' : '未启动');
  b.className = 'badge' + (mon.running ? ' mft' : (i ? '' : ''));

  set('mon-targets', i && i.paths.length ? i.paths.join('　') : '—');
  set('mon-source', i ? `${i.incremental}　${i.scope}` : '—');
  document.getElementById('mon-targets').classList.toggle('hidden', !i);
  document.getElementById('mon-source').classList.toggle('hidden', !i);
  set('mon-counts', s
    ? `已运行 ${monOffset(s.elapsed_ms || 0)}　变更 ${(s.total_events || 0).toLocaleString()} 条`
      + `　预警 ${(s.total_alerts || 0).toLocaleString()} 次`
      + (s.still_pending ? `　待定 ${s.still_pending}` : '')
      + (s.backpressure ? `　⚠ 背压 ${s.backpressure}` : '')
      + (s.queue_dropped ? `　丢弃 ${s.queue_dropped} 条，请重新扫描校准` : '')
      + (s.evicted ? `　历史已滚动淘汰 ${s.evicted} 条` : '')
    : '—');

  const err = document.getElementById('mon-error');
  const msg = (s && s.error) || '';
  err.textContent = msg;
  err.classList.toggle('hidden', !msg);
  err.title = msg;

  document.getElementById('mon-start').classList.toggle('hidden', mon.running);
  document.getElementById('mon-stop').classList.toggle('hidden', !mon.running);
  document.getElementById('mon-start').disabled = mon.starting || state.scanning;
  document.getElementById('mon-stop').disabled = mon.stopping;
  // 监控期间不能扫描（会换掉整棵树），按钮直接禁掉比让后端报错友好
  document.getElementById('menu-scan').disabled = mon.running || mon.starting || state.scanning;
  document.getElementById('select-folder').disabled = mon.running || mon.starting || state.scanning;
}

function renderMonList() {
  const rows = ind.mode === 'independent' ? ind.rows.filter(r => !mon.filter || r.kind === mon.filter) : mon.rows;
  const alerts = ind.mode === 'independent' ? ind.alerts : mon.alerts;
  const box = document.getElementById('mon-list');
  const hint = document.getElementById('mon-list-hint');
  box.innerHTML = '';

  const alertBox = document.getElementById('mon-alert-list');
  alertBox.innerHTML = '';
  document.getElementById('mon-alert-hint').textContent = `${alerts.length} 条`;
    if (!alerts.length) {
      alertBox.innerHTML = '<div class="mon-empty">还没有预警。在左侧设置阈值规则，例如 <code>grow=500M</code>。</div>';
    }
    for (const a of alerts.slice(0, 300)) {
      const observed = a.kind === 'absolute' ? a.current : (a.observed ?? a.current);
      const unit = a.kind === 'rate' ? ' / 秒' : '';
      const el = document.createElement('div');
      el.className = 'arow';
      el.title = `${a.message}\n双击：在资源管理器中定位`;
      el.innerHTML =
        `<span class="t">${eventLocalTime(a)}</span>` +
        `<span class="k ${a.kind}">${esc(a.kind_cn)}</span>` +
        `<span class="p">${esc(monKeepTail(a.path, 90))}</span>` +
        `<span class="s">${a.kind === 'growth' ? '净增长 ' : a.kind === 'rate' ? '速率 ' : '当前 '}${humanSize(observed)}${unit} / 阈值 ${humanSize(a.threshold)}${unit}</span>`;
      // 双击 = 在资源管理器里选中它。复用右键菜单那一份实现（`contextReveal`），
      // 不在这里手拼一条外壳命令：那要处理引号与参数的怪癖，而且本质上是
      // 把路径拼成字符串去执行
      el.ondblclick = () => contextReveal(a.path);
      alertBox.appendChild(el);
      ContextMenu.bind(el, { kind: 'record', row: a });
    }

  const shown = rows.slice(0, 300);
  hint.textContent = shown.length < rows.length
    ? `${shown.length} / ${rows.length} 条`
    : `${rows.length} 条`;
  if (!rows.length) {
    if (ind.mode === 'independent') {
      box.innerHTML = '<div class="mon-empty">尚无变更记录。新增并启用独立监控目标后，这里会显示该路径的后续变更。</div>';
      return;
    }
    box.innerHTML = mon.running
      ? '<div class="mon-empty">监控已开始，正在等待目标目录的文件变更…</div>'
      : '<div class="mon-empty">还没有变更记录。点「开始监控」后，这里会实时刷出新增 / 删除 / 改名 / 修改。</div>';
    return;
  }
  for (const r of shown) {
    const el = document.createElement('div');
    el.className = 'mrow';
    el.title = `${r.path}\n双击：在资源管理器中定位`;
    el.innerHTML =
      `<span class="t">${eventLocalTime(r)}</span>` +
      `<span class="k ${r.kind}">${esc(r.kind_cn)}</span>` +
      `<span class="s">${esc(r.size_text)}</span>` +
      `<span class="p${r.is_dir ? ' dir' : ''}">${esc(monKeepTail(r.path, 120))}</span>` +
      `<span class="x">${r.suffix ? esc(r.suffix) : ''}</span>`;
    el.ondblclick = () => contextReveal(r.path);
    ContextMenu.bind(el, { kind: 'record', row: r });
    box.appendChild(el);
  }
}

/** 序号去重后倒序排列，接受乱序到达的一批，避免丢掉较小但尚未见过的序号。 */
function mergeMonitorRows(existing, incoming, cap, filter = '') {
  const rows = new Map();
  for (const row of existing.concat(incoming || [])) {
    if (!filter || row.kind === filter) rows.set(row.seq, row);
  }
  return Array.from(rows.values()).sort((a, b) => b.seq - a.seq).slice(0, cap);
}

/** 从后端环形缓冲回填；过期的筛选请求不能覆盖较新的事件或筛选结果。 */
async function backfillMonitor() {
  const request = ++mon.backfillRequest;
  const revision = mon.revision;
  const filter = mon.filter;
  const [snap, page, alerts] = await Promise.all([
    call('monitor_snapshot'),
    call('monitor_changes', { limit: MON_ROW_CAP, kind: filter || null }),
    call('monitor_alerts', { limit: 300 }),
  ]);
  if (request !== mon.backfillRequest || filter !== mon.filter) return;
  const unchanged = revision === mon.revision;
  if (unchanged) {
    mon.snap = snap;
    mon.info = snap.info;
    mon.running = !!snap.running;
  }
  mon.rows = mergeMonitorRows(unchanged ? [] : mon.rows, page.rows, MON_ROW_CAP, filter);
  mon.maxSeq = mon.rows.reduce((m, r) => Math.max(m, r.seq), 0);
  mon.alerts = mergeMonitorRows(unchanged ? [] : mon.alerts, alerts, 300);
  renderMonHead();
  renderMonList();
}

/** 一轮 tick：攒行 + 需要时重画面积图。 */
function onMonitorTick(t) {
  // 同一运行中的旧心跳不能倒退运行时间或覆盖新的统计。
  const stale = Number(t.elapsed_ms) < mon.lastTickElapsed;
  mon.revision += 1;
  if (t.rows && t.rows.length) {
    mon.rows = mergeMonitorRows(mon.rows, t.rows, MON_ROW_CAP, mon.filter);
  }
  if (t.alerts && t.alerts.length) {
    mon.alerts = mergeMonitorRows(mon.alerts, t.alerts, 300);
  }
  if (stale) {
    if (mon.open) renderMonList();
    return;
  }
  mon.lastTickElapsed = Number(t.elapsed_ms) || 0;
  mon.snap = Object.assign({}, mon.snap || {}, {
    running: true,
    total_events: t.total_events,
    total_alerts: t.total_alerts,
    elapsed_ms: t.elapsed_ms,
    still_pending: t.still_pending,
    backpressure: t.backpressure_total || 0,
    queue_dropped: t.queue_dropped || 0,
    evicted: t.evicted || 0,
    error: t.error || null,
  });
  mon.running = true;

  // 状态栏跟着实时数字走（扫描摘要里的总量在监控期间会过期）
  if (t.totals) applyLiveTotals(t.totals);

  renderMonHead();
  if (mon.open && ((t.rows || []).length || (t.alerts || []).length)) renderMonList();

  // 预警的界面提示统一由 `tray:alert` 负责（后端发系统通知/闪托盘时同时推一条）：
  // 两处都 toast 的话，窗口在前台时同一条预警会弹两次。
  if (t.truncated) backfillMonitor().catch(() => {});
  if (t.backpressure) toast('事件过多：队列溢出，部分事件已丢弃（建议缩小监视范围）', true);

  // 代数变了 = 树被改过 → 面积图/文件夹树都过期了，重画（节流）
  if (t.generation && t.generation !== mon.lastGen) {
    mon.lastGen = t.generation;
    // 名字与详情缓存一起作废：改名、删除之后旧名字留在缓存里就是**错的信息**
    // （方块上写着已经不存在的路径，比不显示更糟）。
    state.names.clear();
    state.meta.clear();
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
  if (mon.running || mon.starting) return;
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
  if (!Number.isFinite(iv) || iv < 100 || iv > 60000) {
    toast('轮询间隔须在 100 到 60000 毫秒之间');
    return;
  }

  const changelog = document.getElementById('mon-changelog').value.trim();
  mon.starting = true;
  mon.revision += 1;
  mon.backfillRequest += 1;
  mon.rows = [];
  mon.alerts = [];
  mon.info = null;
  mon.snap = null;
  mon.maxSeq = 0;
  mon.lastGen = 0;
  mon.lastTickElapsed = -1;
  renderMonHead();
  renderMonList();
  try {
    await call('monitor_start', {
      paths,
      rules,
      intervalMs: Math.round(iv),
      changelog: changelog || null,
      createJournal: document.getElementById('mon-journal').checked,
    });
    await backfillMonitor();
  } catch (e) {
    /* call() 已经提示过 */
    await backfillMonitor().catch(() => {});
  } finally {
    mon.starting = mon.running && !mon.info;
    renderMonHead();
  }
}

async function stopMonitor() {
  if (mon.stopping) return;
  mon.stopping = true;
  renderMonHead();
  try {
    await call('monitor_stop');
    await backfillMonitor();
  } catch (e) { /* 已提示 */ }
  finally { mon.stopping = false; renderMonHead(); }
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
  document.getElementById('nav-tree').onclick = () => monShow(false);
  document.getElementById('nav-monitor').onclick = () => monShow(true);
  for (const id of ['nav-tree', 'nav-monitor']) {
    document.getElementById(id).onkeydown = (e) => {
      if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
        e.preventDefault();
        const next = id === 'nav-tree' ? 'nav-monitor' : 'nav-tree';
        monShow(next === 'nav-monitor');
        document.getElementById(next).focus();
      }
    };
  }
  document.getElementById('mon-start').onclick = startMonitor;
  document.getElementById('mon-stop').onclick = stopMonitor;
  document.getElementById('mon-export').onclick = exportCsv;
  document.getElementById('mon-simulate').onclick = runSimulate;
  document.getElementById('mon-use-focus').onclick = () => fillPaths('focus');
  document.getElementById('mon-use-vol').onclick = () => fillPaths('volume');

  document.getElementById('mon-filter').onchange = (e) => {
    mon.filter = e.target.value;
    if (ind.mode === 'independent') { renderMonList(); return; }
    backfillMonitor().catch(() => {});
  };
  renderMonHead();
  renderMonList();
}

/* ── 视图切换（面积图 / 重复文件 / 快照对比）─────────────────────────────
 *
 * 三个视图共用中间这一整块，只切显隐。两点值得写下来：
 *
 *   1. **切回面积图时必须重算布局。** 隐藏期间画布尺寸是 0，而"屏幕空间递归"
 *      的布局是拿像素算出来的 —— 用 1×1 的视口算出的东西必须丢掉重来。
 *   2. 左侧文件夹树**不跟着切**。它是导航，不是中间区的一部分；分析视图里
 *      点它只是改 `focus`，面积图下次显示时自然就是新的那一层。
 */

/* ── E7 扫描中的渐进渲染 ────────────────────────────────────────────────
 *
 * 后端每扫到一批（MFT 200k 槽位 / 遍历 400 个目录）就把「根的第一层」算好发过来，
 * 坐标是 0~1 的归一化值。这里只负责画到一个**独立画布**上：
 *
 * 不去碰 `Treemap` 的状态机 —— 扫描过程中布局每 200 ms 就换一次，
 * 让正式渲染器读半成品数据会把"当前焦点/视图根"搅乱。扫完隐藏这一层，
 * 真正的面积图照常接管，用户看到的过渡是"预览 → 真图"。
 */
function hideScanPreview() {
  const cv = document.getElementById('scan-preview');
  if (cv) cv.classList.add('hidden');
  // 遮罩恢复成常规样式（盖住全屏、居中卡片）
  document.body.classList.remove('previewing');
}

function onScanPartial(p) {
  if (!p || !p.rects || !p.rects.length) return;
  if (state.view !== 'map') return; // 在看别的页签，别为看不见的东西重绘
  const cv = document.getElementById('scan-preview');
  const host = document.getElementById('canvas');
  const w = host.clientWidth;
  const h = host.clientHeight;
  if (!w || !h) return;

  const dpr = window.devicePixelRatio || 1;
  const pxW = Math.round(w * dpr);
  const pxH = Math.round(h * dpr);
  if (cv.width !== pxW || cv.height !== pxH) {
    cv.width = pxW;
    cv.height = pxH;
  }
  cv.classList.remove('hidden');
  // 半成品期间把「正在扫描」的遮罩让开：72% 的黑色蒙层会把刚画出来的
  // 面积图压成一片灰，那这个功能就白做了。改成左下角的小卡片。
  document.body.classList.add('previewing');
  document.getElementById('empty-hint').classList.add('hidden');

  const g = cv.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, h);
  const styles = getComputedStyle(document.body);
  const accent = (styles.getPropertyValue('--accent') || '#378add').trim();
  const bg = (styles.getPropertyValue('--bg-panel') || '#1b1b1b').trim();
  const ink = (styles.getPropertyValue('--text') || '#ddd').trim();

  g.fillStyle = bg;
  g.fillRect(0, 0, w, h);

  g.font = '11px ui-sans-serif, system-ui, sans-serif';
  g.textBaseline = 'top';
  p.rects.forEach((r, i) => {
    const x = r.x * w;
    const y = r.y * h;
    const rw = r.w * w;
    const rh = r.h * h;
    if (rw < 1 || rh < 1) return;
    // 目录稍深、文件更淡：与面积图"块越大越显眼"的直觉一致，
    // 但这里不做类型着色 —— 半成品还谈不上"看类型"
    // 相邻方块要能区分：第一层常有一两个巨块（比如占 70%），统一透明度会
    // 糊成一整片纯色，看着像"页面还没画完"。按序号换三档，目录略深。
    g.globalAlpha = 0.24 + (i % 3) * 0.13 + (r.dir ? 0.05 : 0);
    g.fillStyle = accent;
    g.fillRect(x, y, rw, rh);
    g.globalAlpha = 0.75;
    g.strokeStyle = bg;
    g.lineWidth = 1;
    g.strokeRect(x + 0.5, y + 0.5, rw - 1, rh - 1);
    if (rw > 64 && rh > 20) {
      g.globalAlpha = 0.9;
      g.fillStyle = ink;
      const label = r.name.length > Math.floor(rw / 7) ? r.name.slice(0, Math.floor(rw / 7) - 1) + '…' : r.name;
      g.fillText(label, x + 4, y + 3);
    }
  });
  g.globalAlpha = 1;

  // 顶部状态条：让它一眼能看出"这是半成品，还在扫"
  const text = `扫描中 · 已扫 ${p.files.toLocaleString()} 个文件 · ${humanSize(p.total)}`;
  g.fillStyle = accent;
  g.globalAlpha = 0.9;
  g.fillRect(0, 0, w, 22);
  g.globalAlpha = 1;
  g.fillStyle = bg;
  g.fillText(text, 8, 5);
}

const VIEW_HINTS = {
  map: '滚轮下钻 · Esc 返回上一层',
  dup: '三级筛选：体积 → 首尾各 4 KiB 哈希 → 全文件哈希 → 逐字节复核',
  diff: '方向固定为「快照 → 现在的扫描结果」',
  search: '搜索在内存索引里做 · 不重新遍历磁盘',
  risk: '按文件名与扩展名匹配 · 不读文件内容 · 只读不修改',
  trend: '一天一个点 · 基线不足 3 天不给 z 值 · 只读不修改',
};

function setView(v) {
  state.view = v;
  document.getElementById('view-map').classList.toggle('hidden', v !== 'map');
  document.getElementById('view-dup').classList.toggle('hidden', v !== 'dup');
  document.getElementById('view-diff').classList.toggle('hidden', v !== 'diff');
  document.getElementById('view-search').classList.toggle('hidden', v !== 'search');
  document.getElementById('view-risk').classList.toggle('hidden', v !== 'risk');
  document.getElementById('view-trend').classList.toggle('hidden', v !== 'trend');
  document.getElementById('view-tab-map').classList.toggle('on', v === 'map');
  document.getElementById('view-tab-dup').classList.toggle('on', v === 'dup');
  document.getElementById('view-tab-diff').classList.toggle('on', v === 'diff');
  document.getElementById('view-tab-search').classList.toggle('on', v === 'search');
  document.getElementById('view-tab-risk').classList.toggle('on', v === 'risk');
  document.getElementById('view-tab-trend').classList.toggle('on', v === 'trend');
  document.getElementById('view-hint').textContent = VIEW_HINTS[v] || '';
  // 不在面积图页时不要留着"半成品"的遮罩样式（那是给地图让位的）
  if (v !== 'map') document.body.classList.remove('previewing');
  if (v === 'map') refreshLayout().catch(() => {});
  // 切到搜索页就把光标放进输入框：进这个页签的下一步一定是打字
  if (v === 'search') document.getElementById('search-text').focus();
  // 趋势页要现读一次历史：文件很小，但必须是最新的（别人可能刚在 CLI 里记过）
  if (v === 'trend') refreshTrend().catch(() => {});
}

/* ── F12 重复文件 ───────────────────────────────────────────────────────
 *
 * 查找在**后端线程**里跑（可能几分钟），所以这里是"发命令 + 收事件 + 画结果"。
 * 三个事件：dup:progress / dup:done / dup:error。
 */

const dup = {
  running: false,
  progress: null,
  result: null,
  error: null,
};

function dupSummaryText() {
  if (dup.error) return dup.error;
  if (dup.result) {
    const r = dup.result;
    return `${r.summary}　耗时 ${(r.elapsed_ms / 1000).toFixed(2)} s`
      + `　实际读取 ${humanSize(r.bytes_hashed)}`;
  }
  if (dup.running && dup.progress) {
    const p = dup.progress;
    return `${p.stage}　${p.done.toLocaleString()} / ${p.total.toLocaleString()}`
      + `　${pct(p.ratio)}　已用 ${(p.elapsed_ms / 1000).toFixed(1)} s`;
  }
  return '先在左侧扫描一个盘符或目录，再点「开始查找」。'
    + '默认只比较 ≥ 1 MiB 的文件：小文件数量太多，逐字节复核会把时间全花在它们身上。';
}

/** 候选清单那一行 —— 与 CLI 的「候选 N 个文件（排除：…）」对齐。 */
function dupCandidateText(r) {
  let s = `候选 ${r.candidates.toLocaleString()} 个文件`
    + `（排除：${r.skipped_small.toLocaleString()} 个小于下限、`
    + `${r.skipped_empty.toLocaleString()} 个空文件、`
    + `${r.skipped_reparse.toLocaleString()} 个符号链接）`;
  if (r.candidates_truncated > 0) {
    s += `　⚠ 候选数达到上限，还有 ${r.candidates_truncated.toLocaleString()} 个文件没被比较`;
  }
  if (r.unreadable > 0) {
    s += `　有 ${r.unreadable.toLocaleString()} 个文件读不到（权限/被占用/中途被删），没有计入任何一组`;
  }
  return s;
}

function renderDup() {
  const badge = document.getElementById('dup-state');
  badge.textContent = dup.running ? '查找中' : (dup.error ? '失败' : (dup.result ? '已完成' : '未开始'));
  badge.className = 'badge' + (dup.running ? ' traverse' : (dup.result && !dup.error ? ' mft' : ''));

  document.getElementById('dup-start').disabled = dup.running;
  // 没有结果时导出按钮点下去只会得到一句"请先查找"，不如直接禁掉
  document.getElementById('dup-export').disabled = !dup.result;

  const bar = document.getElementById('dup-bar');
  bar.style.width = dup.running && dup.progress
    ? `${Math.min(100, dup.progress.ratio * 100).toFixed(1)}%`
    : (dup.result ? '100%' : '0%');

  document.getElementById('dup-summary').textContent = dupSummaryText();
  document.getElementById('dup-summary').className =
    'hint' + (dup.error ? ' note-warn' : '');

  renderDupList();
}

function renderDupList() {
  const box = document.getElementById('dup-list');
  box.innerHTML = '';
  const r = dup.result;
  if (!r) {
    box.innerHTML = '<div class="mon-empty">还没有结果。</div>';
    return;
  }

  // 候选清单与截断提示放最上面：它们是"这份结果能不能信"的前提
  const head = document.createElement('div');
  head.className = 'mon-empty';
  const candText = dupCandidateText(r);
  head.innerHTML = esc(candText);
  if (r.candidates_truncated > 0) head.className = 'mon-empty note-warn';
  box.appendChild(head);

  if (!r.groups.length) {
    const d = document.createElement('div');
    d.className = 'mon-empty';
    d.textContent = '没有发现内容相同的重复文件。';
    box.appendChild(d);
    return;
  }

  for (let i = 0; i < r.groups.length; i++) {
    const g = r.groups[i];
    const el = document.createElement('div');
    el.className = 'dgroup';
    const gh = document.createElement('div');
    gh.className = 'dgh';
    gh.innerHTML =
      `<span class="idx">#${i + 1}</span>` +
      `<span class="sz">${humanSize(g.size)}</span>` +
      `<span class="n">${g.count.toLocaleString()} 个文件</span>` +
      `<span class="w">删到只剩一份可回收 ${humanSize(g.wasted)}</span>` +
      (g.truncated ? `<span class="note-dim">只列出前 ${g.files.length} 个</span>` : '');
    el.appendChild(gh);

    for (const f of g.files) {
      const row = document.createElement('div');
      row.className = 'dfile';
      row.title = f.path;
      row.innerHTML = `<span class="p">${esc(f.path)}</span>`
        + `<span class="d">${fwLocalTime(f.mtime)}</span>`;
      el.appendChild(row);
    }
    box.appendChild(el);
  }
}

async function startDup() {
  if (dup.running) { toast('已有一次查找在进行中'); return; }
  if (!state.summary) { toast('还没有扫描结果，先在左侧选一个盘符或目录'); return; }
  // **体积单位不在前端解析**：直接把文本交给后端，由它与 CLI 共用那一份语法
  const minSize = document.getElementById('dup-min-size').value;
  const top = Number(document.getElementById('dup-top').value.trim());
  const perGroup = Number(document.getElementById('dup-per-group').value.trim());
  for (const [v, name] of [[top, '最多组数'], [perGroup, '每组最多']]) {
    if (!Number.isInteger(v) || v <= 0) { toast(`${name}要是一个正整数`); return; }
  }

  dup.running = true;
  dup.progress = null;
  dup.error = null;
  dup.result = null;
  renderDup();
  try {
    await call('dup_start', { minSize, top, maxPerGroup: perGroup });
  } catch (e) {
    // 命令被拒（比如正在扫描）时后端不会推 dup:error，这里自己恢复状态
    dup.running = false;
    renderDup();
  }
}

async function dupExport() {
  if (!dup.result) { toast('还没有结果可导出'); return; }
  let p = document.getElementById('dup-csv').value.trim();
  if (!p) {
    // 默认名**由后端拼**：桌面的位置、时间戳的格式（UTC）都只有一处实现，
    // 前端自己 `new Date()` 拼就是本地时区，两个时间会在同一个名字上分叉
    p = await call('analyze_default_path', { kind: 'duplicates' });
    document.getElementById('dup-csv').value = p;
  }
  try {
    const n = await call('dup_export_csv', { path: p });
    toast(`已导出 ${n} 行到 ${p}`);
  } catch (e) { /* call() 已经提示过 */ }
}

/* ── F11 快照对比 ───────────────────────────────────────────────────────
 *
 * 两步：先把"现在"存成一份快照（下次当基准），或者挑一份已有的快照与
 * 现在的扫描结果比。方向固定，不提供"反着比"—— 反着比的结果就是数据反过来，
 * 用户很容易看错。
 */

const diff = {
  view: null,
  error: null,
  /** 正在跑的作业：`null` / `'compare'` / `'save'`。 */
  busy: null,
  stage: '',
  ratio: 0,
  t0: 0,
  timer: null,
};

/** 与引擎 `snapshot::signed_size` 同一个写法：真减号 U+2212，不是连字符。 */
function signedText(v) {
  return (v < 0 ? '\u2212' : '+') + humanSize(Math.abs(v));
}

/** 开工：显示进度行，并让"已用 x.x 秒"自己走起来。
 *
 * 秒数必须由前端自己跳：阶段名要等后端推事件（两帧最多一条），而"还活着"
 * 这件事在长跑里只能靠一个一直在动的数字证明 —— 卡死与"在跑"的差别全在这。 */
function diffStart(kind, stage) {
  diff.busy = kind;
  diff.stage = stage;
  diff.ratio = 0;
  diff.t0 = Date.now();
  if (diff.timer) clearInterval(diff.timer);
  diff.timer = setInterval(diffTick, 200);
  renderDiff();
}

function diffTick() {
  if (!diff.busy) return;
  document.getElementById('diff-stage').textContent =
    `${diff.stage}　已用 ${((Date.now() - diff.t0) / 1000).toFixed(1)} s`;
}

function diffStop() {
  diff.busy = null;
  if (diff.timer) { clearInterval(diff.timer); diff.timer = null; }
  renderDiff();
}

function renderDiff() {
  const badge = document.getElementById('diff-state');
  const v = diff.view;
  const busy = diff.busy;
  badge.textContent = busy === 'compare' ? '对比中'
    : busy === 'save' ? '保存中'
      : diff.error ? '失败' : (v ? '已对比' : '未对比');
  badge.className = 'badge' + (busy ? ' traverse' : (v && !diff.error ? ' mft' : ''));
  // 作业期间两个开工按钮都禁掉：后端会拒绝第二次，但让用户点了才知道没反应，
  // 不如一开始就不让它点（同时"按钮变灰"本身也是一条进度提示）
  document.getElementById('diff-compare').disabled = !!busy;
  document.getElementById('diff-save').disabled = !!busy;
  document.getElementById('diff-export').disabled = !v;

  const row = document.getElementById('diff-progress');
  row.classList.toggle('hidden', !busy);
  if (busy) {
    document.getElementById('diff-bar').style.width =
      `${Math.min(100, diff.ratio * 100).toFixed(1)}%`;
    document.getElementById('diff-stage').textContent =
      `${diff.stage}　已用 ${((Date.now() - diff.t0) / 1000).toFixed(1)} s`;
  }

  const sum = document.getElementById('diff-summary');
  if (diff.error) {
    sum.textContent = diff.error;
    sum.className = 'hint note-warn';
  } else if (v) {
    sum.textContent = v.summary;
    sum.className = 'hint' + (v.old_truncated || v.new_truncated ? ' note-warn' : '');
  } else {
    sum.textContent = '「保存快照」把现在的扫描结果存成基准；'
      + '之后改动了磁盘，再填上那份快照的路径点「对比」。';
    sum.className = 'hint';
  }
  renderDiffAttr(v);
  renderDiffList();
}

function renderDiffList() {
  const box = document.getElementById('diff-list');
  box.innerHTML = '';
  const v = diff.view;
  if (!v) {
    box.innerHTML = '<div class="mon-empty">还没有对比结果。</div>';
    return;
  }

  const head = document.createElement('div');
  head.className = 'mon-empty';
  let note = `基准快照 ${v.old_created}（${esc(v.old_root || '?')}，${v.old_total.toLocaleString()} 个文件）`
    + ` → 现在 ${v.new_created}（${esc(v.new_root || '?')}，${v.new_total.toLocaleString()} 个文件）`
    + `　未变化 ${v.same.toLocaleString()} 个`;
  if (v.root_mismatch) note += '\n⚠ 两份数据的扫描根不同，这份对比很可能没有意义';
  if (v.old_truncated || v.new_truncated) {
    note += `\n⚠ 快照被截断过（旧丢 ${v.old_truncated} / 新丢 ${v.new_truncated} 个文件），`
      + '没有被记进去的文件会被误判成「新增」，不要把这份清单当成删除清单';
  }
  if (!v.rows_total) note += '\n没有符合条件的变化。';
  else if (v.rows.length < v.rows_total) {
    note += `\n共 ${v.rows_total.toLocaleString()} 行变化，下面只列了 ${v.rows.length.toLocaleString()} 行（导出 CSV 可看全部）`;
  }
  head.innerHTML = note.split('\n').map(esc).join('<br>');
  if (v.root_mismatch || v.old_truncated || v.new_truncated) head.className = 'mon-empty note-warn';
  box.appendChild(head);

  // 列头。列表本身可横向滚动（窄窗口下六列放不下时，宁可横向滚，
  // 也不要把文件名和时间压成省略号 —— 那正是这张表要回答的问题）。
  const cols = document.createElement('div');
  cols.className = 'dhead';
  cols.innerHTML = '<span>类型</span><span class="d">变化</span><span>文件名</span>'
    + '<span>修改时间</span><span>创建时间</span><span>所在目录</span>';
  box.appendChild(cols);

  for (const r of v.rows) {
    const row = document.createElement('div');
    row.className = 'drow';
    const name = r.name || r.path.split(/[\\/]/).pop() || r.path;
    const dir = r.path.length > name.length
      ? r.path.slice(0, r.path.length - name.length).replace(/[\\/]+$/, '')
      : '';
    row.title = `${r.path}\n修改时间：${fwLocalTime(r.mtime)}\n创建时间：${fwLocalTime(r.ctime)}`;
    row.innerHTML =
      `<span class="k ${esc(r.kind)}">${esc(r.kind_cn)}</span>` +
      // 「变化」列两行：上面是 ±增量，下面是 0 B → 2.00 MB。
      // 单看增量不知道"原来多大"，单看两个尺寸又算不出增量。
      `<span class="d">${signedText(r.delta)}` +
        `<small>${humanSize(r.old_size)} → ${humanSize(r.new_size)}</small></span>` +
      `<span class="n">${esc(fwMiddleEllipsis(name, 48))}</span>` +
      `<span class="t">${esc(fwLocalTime(r.mtime))}</span>` +
      `<span class="t">${esc(fwLocalTime(r.ctime))}</span>` +
      `<span class="p">${esc(dir || '—')}</span>`;
    ContextMenu.bind(row, { kind: 'diffrow', row: r });
    box.appendChild(row);
  }
}

/* 存快照与对比都是「命令只开工、结果走事件」。
 *
 * 两件事在 200 万文件下都是好几秒（写一百多 MB / 读一百多 MB + 归并），
 * 在命令里同步做会把主线程堵死：窗口变成"未响应"，界面上却什么提示都没有
 * —— 用户只能猜是不是卡住了。所以这里发完命令就返回，进度条由
 * `diff:progress` 推着走，"已用 x.x 秒"由前端自己跳。 */
async function saveSnapshot() {
  if (!state.summary) { toast('还没有扫描结果，先在左侧选一个盘符或目录'); return; }
  if (diff.busy) { toast('还有一次快照作业没跑完'); return; }
  let p = document.getElementById('diff-save-path').value.trim();
  if (!p) {
    p = await call('analyze_default_path', { kind: 'snapshot' });
    document.getElementById('diff-save-path').value = p;
  }
  try {
    await call('snapshot_save', { path: p, maxEntries: 0 });
    diffStart('save', '准备中');
  } catch (e) { /* call() 已经提示过（路径类错误会当场返回） */ }
}

async function onSnapshotSaved(info) {
  toast(`已存下 ${info.entries.toLocaleString()} 个文件（${humanSize(info.bytes_on_disk)}）到 ${info.path}`);
  // 顺手把它填进基准框：用户存快照的目的多半就是"下次拿它比"
  document.getElementById('diff-path').value = info.path;
  if (info.truncated > 0) {
    toast(`注意：快照被截断，丢了 ${info.truncated.toLocaleString()} 个文件`, true);
  }
}

async function compareSnapshot() {
  const p = document.getElementById('diff-path').value.trim();
  if (!p) { toast('先填一份要对比的快照路径（.tsv）'); return; }
  if (!state.summary) { toast('还没有扫描结果，先在左侧选一个盘符或目录'); return; }
  if (diff.busy) { toast('还有一次快照作业没跑完'); return; }
  try {
    await call('snapshot_compare', {
      path: p,
      top: 300,
      minDelta: document.getElementById('diff-min-delta').value,
      order: document.getElementById('diff-order').value,
    });
    diffStart('compare', '准备中');
  } catch (e) { /* call() 已经提示过（路径空、体积看不懂都会当场返回） */ }
}

async function diffExport() {
  const v = diff.view;
  if (!v) { toast('还没有对比结果可导出'); return; }
  let p = document.getElementById('diff-csv').value.trim();
  if (!p) {
    p = await call('analyze_default_path', { kind: 'diff' });
    document.getElementById('diff-csv').value = p;
  }
  try {
    // CSV 正文由后端在对比时就算好了，这里只负责落盘 ——
    // 界面显示的行与导出的行必须来自同一份数据
    const n = await call('diff_export_csv', { path: p, body: v.csv });
    toast(`已导出 ${n} 行到 ${p}`);
  } catch (e) { /* call() 已经提示过 */ }
}

/* ── F13 文件搜索 ───────────────────────────────────────────────────────────
 *
 * 搜的是**内存里的索引树**（`search_nodes`），不是重新遍历磁盘 —— 200 万
 * 个名字全在字符串池里，一次搜索几十毫秒。这也意味着：先扫描，才有得搜。
 *
 * 三种模式的区别写在搜索框的 title 与 README 里：
 *   包含 —— 子串（大小写不敏感）
 *   模糊 —— 按顺序出现的字符（`rpt` 命中 `report`），连续命中排前面
 *   正则 —— 简化正则（`.` `*` `+` `?` `[abc]` `[a-z]` `[^…]` `^` `$` `\` 转义）
 */

const searchView = {
  view: null,
  error: null,
  running: false,
};

function renderSearch() {
  const badge = document.getElementById('search-state');
  const sum = document.getElementById('search-summary');
  const box = document.getElementById('search-list');
  const v = searchView.view;

  badge.textContent = searchView.running ? '搜索中'
    : searchView.error ? '失败' : (v ? `${v.matched.toLocaleString()} 个` : '未搜索');
  badge.className = 'badge' + (searchView.running ? ' traverse' : (v && !searchView.error ? ' mft' : ''));

  if (searchView.error) {
    sum.textContent = searchView.error;
    sum.className = 'hint note-warn';
  } else if (!v) {
    sum.textContent = '在已扫描的索引里搜，不重新遍历磁盘。'
      + '双击结果可前往它所在的目录，右键还有更多操作。';
    sum.className = 'hint';
  } else if (!v.matched) {
    sum.textContent = `没有命中（比过 ${v.scanned.toLocaleString()} 个节点，用时 ${v.elapsed_ms} ms）`;
    sum.className = 'hint';
  } else {
    sum.textContent = `${v.matched.toLocaleString()} 个命中`
      + (v.truncated ? `，只列前 ${v.hits.length.toLocaleString()} 个` : '')
      + `　比过 ${v.scanned.toLocaleString()} 个节点，用时 ${v.elapsed_ms} ms`;
    sum.className = 'hint';
  }

  // 只搜了内容（名称框空着）时，名称列表整块收起：留一个"还没有搜索。"
  // 的占位只会让人以为少了一块结果。
  const onlyContent = !v && !searchView.error && !!contentSearch.view;
  box.classList.toggle('hidden', onlyContent);

  box.innerHTML = '';
  if (!v || !v.hits.length) {
    box.innerHTML = `<div class="mon-empty">${searchView.error ? esc(searchView.error) : (v ? '没有命中。换个关键字，或把「方式」改成模糊试试。' : '还没有搜索。')}</div>`;
    return;
  }

  // 列头与快照对比同一套节奏：类型 / 文件名 / 大小 / 修改时间 / 所在目录
  const cols = document.createElement('div');
  cols.className = 'shead';
  cols.innerHTML = '<span>类型</span><span>文件名</span><span class="d">大小</span>'
    + '<span>修改时间</span><span>所在目录</span>';
  box.appendChild(cols);

  for (const h of v.hits) {
    const row = document.createElement('div');
    row.className = 'srow';
    const dir = h.path.length > h.name.length
      ? h.path.slice(0, h.path.length - h.name.length).replace(/[\\/]+$/, '')
      : '';
    row.title = h.path;
    row.innerHTML =
      `<span class="k${h.is_dir ? ' dir' : ''}">${h.is_dir ? '文件夹' : '文件'}</span>` +
      `<span class="n">${esc(fwMiddleEllipsis(h.name, 52))}</span>` +
      `<span class="d">${h.is_dir ? '—' : humanSize(h.size)}</span>` +
      `<span class="t">${esc(fwLocalTime(h.mtime))}</span>` +
      `<span class="p">${esc(dir || '—')}</span>`;
    // 右键复用"节点"菜单：定位、在资源管理器打开、复制路径、加入独立监控……
    // 搜索结果是索引里的真实节点，所以 node 菜单的每一项都成立。
    ContextMenu.bind(row, { kind: 'node', node: h.node });
    row.ondblclick = async () => {
      try {
        const parent = h.is_dir ? h.node : await call('node_parent', { node: h.node });
        monShow(false); setView('map'); await goto(parent);
        if (!h.is_dir) await pickNode(h.node);
      } catch (e) { /* call() 已提示 */ }
    };
    box.appendChild(row);
  }
}

/* 内容搜索的状态。与名字搜索分开：一个是同步命令、一个是后台线程 + 事件，
 * 混在一个对象里只会让"哪个还在跑"变得说不清。 */
const contentSearch = { running: false, progress: null, view: null, error: null, hitCount: 0 };

/** 内容结果：文件名 / 命中行（最多三行）/ 大小 / 修改时间。 */
function renderContent() {
  const row = document.getElementById('content-progress');
  const box = document.getElementById('content-list');
  const v = contentSearch.view;
  const busy = contentSearch.running;

  row.classList.toggle('hidden', !busy);
  if (busy && contentSearch.progress) {
    const p = contentSearch.progress;
    document.getElementById('content-bar').style.width =
      `${p.total ? Math.min(100, (p.scanned / p.total) * 100).toFixed(1) : 0}%`;
    document.getElementById('content-stage').textContent =
      `${p.scanned.toLocaleString()} / ${p.total.toLocaleString()} 个文件`
      + `　命中 ${contentSearch.hitCount.toLocaleString()} 个文件`
      + `　已用 ${(p.elapsed_ms / 1000).toFixed(1)} s`;
  }

  const show = busy || !!contentSearch.error || !!(v && (v.hits.length || v.scanned));
  box.classList.toggle('hidden', !show);
  if (!show) { box.innerHTML = ''; return; }
  box.innerHTML = '';

  if (contentSearch.error) {
    box.innerHTML = `<div class="mon-empty note-warn">${esc(contentSearch.error)}</div>`;
    return;
  }
  if (!v) return;   // 还在跑，且还没有结果

  const head = document.createElement('div');
  head.className = 'mon-empty';
  head.textContent = v.hits.length
    ? `内容命中 ${v.hits.length.toLocaleString()} 个文件`
      + (v.truncated ? '（已达上限，先列这些）' : '')
      + `　读过 ${v.scanned.toLocaleString()} / ${v.candidates.toLocaleString()} 个纯文本文件`
      + `，用时 ${v.elapsed_ms} ms`
    : `内容没有命中（读过 ${v.scanned.toLocaleString()} 个纯文本文件，用时 ${v.elapsed_ms} ms）`;
  if (!v.hits.length) head.className = 'mon-empty';
  box.appendChild(head);
  if (!v.hits.length) return;

  const cols = document.createElement('div');
  cols.className = 'chead';
  cols.innerHTML = '<span>文件名</span><span>命中行</span><span class="d">大小</span><span>修改时间</span>';
  box.appendChild(cols);

  for (const h of v.hits) {
    const row = document.createElement('div');
    row.className = 'crow';
    row.title = h.path;
    const lines = h.lines
      .map((l) => `<span class="ln"><b>${l.line_no}</b>　${esc(l.text)}</span>`)
      .join('');
    row.innerHTML =
      `<span class="n">${esc(fwMiddleEllipsis(h.name, 34))}</span>` +
      `<span class="hits">${lines}</span>` +
      `<span class="d">${humanSize(h.size)}</span>` +
      `<span class="t">${esc(fwLocalTime(h.mtime))}</span>`;
    ContextMenu.bind(row, { kind: 'node', node: h.node });
    row.ondblclick = async () => {
      try {
        const parent = await call('node_parent', { node: h.node });
        monShow(false); setView('map'); await goto(parent);
        await pickNode(h.node);
      } catch (e) { /* call() 已提示 */ }
    };
    box.appendChild(row);
  }
}

/** 名称搜索（同步）+ 内容搜索（后台线程）由同一个按钮触发。
 *
 * 两个框可以只填一个：填名字就按名字找，填内容就按内容找，都填就两样都给。
 * "都填"不取交集 —— 用户心里的模型是"我要找到它"，不是"两个条件同时成立"；
 * 而且交集那套说法没法解释"名字命中但内容没命中"的行该不该出现。 */
async function runSearch() {
  const name = document.getElementById('search-text').value.trim();
  const contentText = document.getElementById('search-content').value.trim();
  if (!name && !contentText) { toast('先输入要查找的名称或内容'); return; }
  if (state.scanning) { toast('正在扫描：等扫描结束后再搜（否则结果和界面上的树对不上）'); return; }
  if (!state.summary) {
    toast('还没有扫描结果：先在左侧点一个盘符，或「文件 → 选择文件夹…」');
    return;
  }
  if (name) await runNameSearch(name);
  if (contentText) await runContentSearch(contentText);
  else { contentSearch.view = null; contentSearch.error = null; renderContent(); }
}

async function runNameSearch(text) {
  if (searchView.running) return;
  const button = document.getElementById('search-run');
  searchView.running = true;
  searchView.error = null;
  button.disabled = true;
  renderSearch();
  try {
    searchView.view = await call('search_nodes', {
      text,
      mode: document.getElementById('search-mode').value,
      scope: document.getElementById('search-scope').value,
      limit: 500,
    });
    searchView.error = null;
  } catch (e) {
    searchView.view = null;
    searchView.error = typeof e === 'string' ? e : (e && e.message) || String(e);
  } finally {
    searchView.running = false;
    if (!contentSearch.running) button.disabled = false;
    renderSearch();
  }
}

async function runContentSearch(text) {
  if (contentSearch.running) return;
  contentSearch.running = true;
  contentSearch.error = null;
  contentSearch.progress = null;
  contentSearch.hitCount = 0;
  document.getElementById('content-bar').style.width = '0%';
  document.getElementById('search-run').disabled = true;
  renderContent();
  try {
    // 开工即返回：真正的读文件在后台线程里，进度走 content:progress
    await call('content_search_start', {
      text,
      regex: document.getElementById('search-content-regex').checked,
    });
  } catch (e) {
    contentSearch.running = false;
    contentSearch.error = typeof e === 'string' ? e : (e && e.message) || String(e);
    document.getElementById('search-run').disabled = false;
    renderContent();
  }
}

/* 用文件/文件夹选择器填一个路径输入框。
 *
 * 选完**只填框、不自动执行**：立刻开始对比或导出，用户来不及看清自己选的是
 * 什么。「存快照到 / CSV」选的是**文件夹**，文件名留给后端补（前端 `new Date()`
 * 是本地时区，和快照正文里的 UTC 会对不上同一个文件名）；手填完整文件名照旧可用。 */
async function pickInto(inputId, kind, title, exts) {
  let path;
  try {
    path = await call('choose_path', { kind, title, exts: exts || null });
  } catch (e) {
    return;   // call() 已经提示过
  }
  if (path) document.getElementById(inputId).value = path;
}

/* 打开窗口时补一次快照作业状态：作业可能在监听注册之前就跑完了
 * （比如上次没关窗口），只靠事件会一直停在"未对比"。 */
async function backfillDiff() {
  const s = await call('diff_status');
  diff.view = s.result || null;
  diff.error = s.error || null;
  if (s.running) {
    // 作业还在跑：把进度接上（时间只能从现在算，所以秒数会偏小一点）
    diffStart(s.kind === 'save' ? 'save' : 'compare', s.stage || '进行中');
    diff.ratio = s.ratio || 0;
    renderDiff();
  } else {
    diffStop();
  }
}

/* 打开窗口时补一次重复文件状态：事件可能发生在监听注册之前（比如上次没关窗口）。 */
async function backfillDup() {
  const v = await call('dup_view');
  dup.running = !!v.running;
  dup.progress = v.progress || null;
  dup.result = v.result || null;
  dup.error = v.error || null;
  renderDup();
}

/* ── E1 增长归因 ─────────────────────────────────────────────────────────
 *
 * 三个榜的数据由**后端在 diff 里算好**（要全量变化行，前 300 行的明细表给不出
 * "哪个目录在长"）。这里只负责画，不重新聚合 —— 否则同一份快照在界面和
 * CSV / CLI 里会得出两套数。
 */

/** 带正负号的体积。`−` 用减号（U+2212）而不是连字符：和数字并排时才不会看成破折号。 */
function signedSize(n) {
  if (!n) return '0 B';
  return (n > 0 ? '+' : '−') + humanSize(Math.abs(n));
}

/** 路径保尾截断（后端的 `truncate_keep_tail` 同口径：信息在最后几段）。 */
function tailPath(s, n) {
  if (!s || s.length <= n) return s || '';
  return '…' + s.slice(s.length - n);
}

let attrCollapsed = false;

/** 造一行：名字 / 数值 / 归一化横条 / 副标题。 */
function attrLine(name, value, ratio, sub, mono) {
  const row = document.createElement('div');
  row.className = 'attr-line';
  row.title = name;

  const n = document.createElement('span');
  n.className = 'attr-name' + (mono ? ' mono' : '');
  n.textContent = name;

  const v = document.createElement('span');
  const down = value.startsWith('−');
  v.className = 'attr-val ' + (down ? 'down' : 'up');
  v.textContent = value;

  const track = document.createElement('span');
  track.className = 'attr-track';
  const bar = document.createElement('i');
  if (down) bar.className = 'down';
  // ratio = null 表示这一列不该画条（例如时段列里刚归零的桶）
  if (ratio !== null && Number.isFinite(ratio)) bar.style.width = `${(ratio * 100).toFixed(1)}%`;
  track.appendChild(bar);

  const s = document.createElement('span');
  s.className = 'attr-sub';
  s.textContent = sub;

  row.append(n, v, track, s);
  return row;
}

function attrCol(title, hintText) {
  const col = document.createElement('div');
  col.className = 'attr-col';
  const h = document.createElement('h3');
  h.textContent = title;
  if (hintText) {
    const s = document.createElement('span');
    s.className = 'hint';
    s.textContent = hintText;
    h.appendChild(s);
  }
  col.appendChild(h);
  return col;
}

function attrEmpty(text) {
  const p = document.createElement('p');
  p.className = 'attr-empty';
  p.textContent = text;
  return p;
}

/** 折叠/展开归因卡。状态只在本次会话里记 —— 这张卡只在"刚比过一次"之后存在，
 *  存进偏好会变成"下次打开时莫名其妙是收起的"。 */
function toggleDiffAttr() {
  attrCollapsed = !attrCollapsed;
  const btn = document.getElementById('diff-attr-toggle');
  btn.setAttribute('aria-expanded', String(!attrCollapsed));
  btn.classList.toggle('collapsed', attrCollapsed);
  const box = document.getElementById('diff-attr');
  // 折叠时连"有没有内容"都不用问：直接收掉，标题行留着
  box.classList.toggle('hidden', attrCollapsed);
}

function renderDiffAttr(v) {
  const box = document.getElementById('diff-attr');
  const head = document.getElementById('diff-attr-head');
  const sum = document.getElementById('diff-attr-sum');
  const a = v && v.attr;
  const hasUp = a && a.dirs_up.length;
  const hasDown = a && a.dirs_down.length;
  const hasExt = a && a.exts.some((e) => e.delta !== 0);
  if (!a || (!hasUp && !hasDown && !hasExt)) {
    box.classList.add('hidden');
    head.classList.add('hidden');
    box.replaceChildren();
    return;
  }
  head.classList.remove('hidden');
  box.classList.toggle('hidden', attrCollapsed);

  // 收起后这一行就是全部信息：谁在长、长了多少、总共动了多少目录
  const top = a.dirs_up[0];
  sum.textContent = top
    ? `增长最多：${tailPath(top.dir, 40)} ${signedSize(top.delta)}　共 ${a.dirs_total.toLocaleString()} 个目录有变化`
    : `共 ${a.dirs_total.toLocaleString()} 个目录有变化，没有变大的`;

  box.replaceChildren();

  // ── 目录 ────────────────────────────────────────────────────────────
  const colDir = attrCol('增长最多的目录', `聚合到 ${a.dir_depth} 层 · 共 ${a.dirs_total.toLocaleString()} 个目录有变化`);
  if (!hasUp) {
    colDir.appendChild(attrEmpty('没有变大的目录。'));
  } else {
    // 条形按**本列最大增量**归一：同一列里比才有意义
    const upMax = Math.max(1, ...a.dirs_up.map((d) => d.delta));
    for (const d of a.dirs_up.slice(0, 5)) {
      const row = attrLine(tailPath(d.dir, 46), signedSize(d.delta), d.delta / upMax,
        `${d.files.toLocaleString()} 个文件`, false);
      row.title = d.dir;
      // 右键与快照明细行同一套：目录可能已经不存在了，所以"定位/打开"交给后端兜底
      ContextMenu.bind(row, { kind: 'attrdir', row: d });
      colDir.appendChild(row);
    }
  }
  if (hasDown) {
    const sub = document.createElement('h3');
    sub.className = 'attr-h3-sub';
    sub.textContent = '减少最多';
    colDir.appendChild(sub);
    const downMax = Math.max(1, ...a.dirs_down.slice(0, 2).map((d) => Math.abs(d.delta)));
    for (const d of a.dirs_down.slice(0, 2)) {
      const row = attrLine(tailPath(d.dir, 46), signedSize(d.delta), Math.abs(d.delta) / downMax,
        `${d.files.toLocaleString()} 个文件`, false);
      row.title = d.dir;
      ContextMenu.bind(row, { kind: 'attrdir', row: d });
      colDir.appendChild(row);
    }
  }
  box.appendChild(colDir);

  // ── 类型 ────────────────────────────────────────────────────────────
  const colExt = attrCol('按类型', '变化幅度最大的五类');
  // 榜是按增量降序来的，直接取前几个会全是"新增"，把"减少得最厉害的"漏掉
  const extTop = a.exts
    .filter((e) => e.delta !== 0)
    .slice()
    .sort((x, y) => Math.abs(y.delta) - Math.abs(x.delta))
    .slice(0, 5);
  if (!extTop.length) {
    colExt.appendChild(attrEmpty('没有按类型的变化。'));
  } else {
    const extMax = Math.max(1, ...extTop.map((e) => Math.abs(e.delta)));
    for (const e of extTop) {
      const name = e.ext || '(无扩展名)';
      const text = `${name}\t${signedSize(e.delta)}\t${e.files.toLocaleString()} 个文件`;
      const row = attrLine(name, signedSize(e.delta), Math.abs(e.delta) / extMax,
        `${e.files.toLocaleString()} 个文件`, false);
      row.title = text;
      ContextMenu.bind(row, { kind: 'attrcopy', text });
      colExt.appendChild(row);
    }
  }
  box.appendChild(colExt);

  // ── 时段 ────────────────────────────────────────────────────────────
  const colTime = attrCol('变化最集中的时段', '按文件修改时间');
  const burstTop = a.bursts.slice(0, 3);
  if (!burstTop.length) {
    colTime.appendChild(attrEmpty('快照里没有可用的时间。'));
  } else {
    const burstMax = Math.max(1, ...burstTop.map((b) => Math.abs(b.delta)));
    for (const b of burstTop) {
      const t = fwLocalTime(b.hour_unix);
      const text = `${t}\t${signedSize(b.delta)}\t${b.files.toLocaleString()} 个文件`;
      const row = attrLine(t, signedSize(b.delta), Math.abs(b.delta) / burstMax,
        `${b.files.toLocaleString()} 个文件`, true);
      row.title = text;
      ContextMenu.bind(row, { kind: 'attrcopy', text });
      colTime.appendChild(row);
    }
  }
  const note = document.createElement('p');
  note.className = 'attr-note';
  note.textContent = '删除的行没有"删除时间"这个信息，它归到文件最后修改的那个小时。';
  colTime.appendChild(note);
  box.appendChild(colTime);
}


/* ── E6 增长趋势 ────────────────────────────────────────────────────────
 *
 * 数据全在后端算好（`history_view`），这里只画：
 * 一张按日的总量折线 + 一张"今天 vs 前 N 天基线"的表。
 *
 * 两条规矩：
 * 1. **没有 z 值就说没有**。基线不足 3 天时后端给 `z: null`，这里显示"差 N 天"，
 *    绝不能拿 0 顶上 —— 0 的意思是"和基线一样"，而实际情况是"还不知道"。
 * 2. 折线只有一天的数据时不画线，直接说"还成不了线"。一个孤零零的点看起来
 *    像是"持平"，比空着更误导。
 */

const trend = { view: null, error: null, busy: false };

async function refreshTrend() {
  try {
    trend.view = await call('history_view', {});
    trend.error = null;
  } catch (e) {
    trend.error = String(e);
  }
  renderTrend();
}

async function recordTrend() {
  if (trend.busy) return;
  trend.busy = true;
  const btn = document.getElementById('trend-record');
  btn.disabled = true;
  try {
    trend.view = await call('history_record');
    trend.error = null;
  } catch (e) {
    trend.error = String(e);
  }
  trend.busy = false;
  btn.disabled = false;
  renderTrend();
}

/** 折线：一行内联 SVG，零依赖。`days` 是 `[{date, total}]`。 */
function trendChart(v) {
  const pts = (v && v.days) || [];
  if (!pts.length) {
    return `<div class="trend-empty">还没有样本。
      <span class="hint">点「记录一次」开始记；性能模式下每次扫描后会自动记。</span></div>`;
  }
  if (pts.length === 1) {
    return `<div class="trend-empty">只有 1 天数据（${esc(pts[0].date)} · ${humanSize(pts[0].total)}），还成不了线。
      <span class="hint">再过一天就有了。</span></div>`;
  }
  // H 比绘图区多留一截给底部的日期标签：标签压在面积图上会看不清
  const W = 1000, H = 208, PX = 46, PY = 24;
  const vals = pts.map((p) => p.total);
  const lo = Math.min(...vals);
  const hi = Math.max(...vals);
  const span = Math.max(1, hi - lo);
  const x = (i) => PX + (W - PX * 2) * (i / (pts.length - 1));
  const y = (val) => H - PY - (H - PY * 2) * ((val - lo) / span);

  const line = pts.map((p, i) => `${x(i).toFixed(1)},${y(p.total).toFixed(1)}`).join(' ');
  const area = `${PX},${H - PY} ${line} ${W - PX},${H - PY}`;
  const dots = pts
    .map((p, i) => `<circle cx="${x(i).toFixed(1)}" cy="${y(p.total).toFixed(1)}" r="2.4"
      class="trend-dot"><title>${esc(p.date)} · ${humanSize(p.total)}</title></circle>`)
    .join('');
  // 只标首尾日期与最高/最低值：每天都标会糊成一团
  const label = (i, anchor) => `<text x="${x(i).toFixed(1)}" y="${H - 6}"
      class="trend-axis" text-anchor="${anchor}">${esc(pts[i].date.slice(5))}</text>`;
  // 不用 preserveAspectRatio="none"：那会把文字标签一起拉伸变形。
  // 让它按 viewBox 的比例缩放，高度随宽度走。
  return `<svg class="trend-svg" viewBox="0 0 ${W} ${H}" role="img"
      aria-label="总量趋势：${pts.length} 天">
    <line x1="${PX}" y1="${y(hi).toFixed(1)}" x2="${W - PX}" y2="${y(hi).toFixed(1)}" class="trend-grid"/>
    <line x1="${PX}" y1="${y(lo).toFixed(1)}" x2="${W - PX}" y2="${y(lo).toFixed(1)}" class="trend-grid"/>
    <text x="4" y="${(y(hi) + 4).toFixed(1)}" class="trend-axis">${humanSize(hi)}</text>
    <text x="4" y="${(y(lo) + 4).toFixed(1)}" class="trend-axis">${humanSize(lo)}</text>
    <polygon points="${area}" class="trend-area"/>
    <polyline points="${line}" class="trend-line"/>
    ${dots}
    ${label(0, 'start')}
    ${label(pts.length - 1, 'end')}
  </svg>`;
}

function renderTrend() {
  const v = trend.view;
  const chart = document.getElementById('trend-chart');
  const list = document.getElementById('trend-list');
  const state = document.getElementById('trend-state');
  const sum = document.getElementById('trend-summary');

  if (trend.error) {
    state.className = 'badge bad';
    state.textContent = '读不出来';
    sum.textContent = trend.error;
    chart.replaceChildren();
    list.replaceChildren();
    return;
  }
  if (!v) {
    state.className = 'badge';
    state.textContent = '—';
    sum.textContent = '';
    return;
  }

  state.className = 'badge ' + (v.samples ? 'ok' : '');
  state.textContent = v.samples ? `${v.samples} 个样本` : '还没有样本';
  sum.textContent = [
    v.span,
    v.latest,
    v.samples ? `记了 ${v.dirs_kept} / ${v.dirs_total} 个目录` : '',
    `${v.file_kb} KB`,
    v.auto ? '自动记录：开' : '自动记录：关（不在性能模式）',
    v.dropped ? `跳过 ${v.dropped} 个不完整样本` : '',
  ].filter(Boolean).join('　');

  chart.innerHTML = trendChart(v);

  // ── 异常表 ──────────────────────────────────────────────────────────
  list.replaceChildren();
  const head = document.createElement('div');
  head.className = 'xhead';
  head.innerHTML = '<span>目录</span><span class="d">今天</span><span class="d">基线均值</span>'
    + '<span class="d">σ</span><span class="d">z</span>';
  list.appendChild(head);

  if (!v.anomalies.length) {
    const p = document.createElement('div');
    p.className = 'mon-empty';
    p.textContent = v.samples < 2
      ? '至少要有两天的样本才能比较。'
      : '没有发现异常增长（今天没有明显超出前 7 天基线的目录）。';
    list.appendChild(p);
    return;
  }

  for (const a of v.anomalies) {
    const row = document.createElement('div');
    row.className = 'xrow';
    row.title = a.dir;
    const z = a.z === null || a.z === undefined
      ? `<span class="hint">差 ${a.days_needed} 天</span>`
      : `<b>${a.z.toFixed(1)}</b>`;
    row.innerHTML =
      `<span class="p">${esc(a.dir)}</span>` +
      `<span class="d up">${signedSize(a.delta)}</span>` +
      `<span class="d">${humanSize(Math.abs(a.base_mean))}</span>` +
      `<span class="d">${humanSize(Math.abs(a.base_sd))}</span>` +
      `<span class="d">${z}</span>`;
    // 右键与其它列表一致：目录还在就能定位；不在了交给后端兜底
    ContextMenu.bind(row, { kind: 'attrdir', row: { dir: a.dir, delta: a.delta, files: 0 } });
    list.appendChild(row);
  }
}

/* ── E5 风险清单 ─────────────────────────────────────────────────────────
 *
 * 纯内存遍历（零 IO），所以同步命令、不做进度条。
 * 页面上的口径必须写死：只看文件名与扩展名。"没找到" 不等于 "没有风险"。
 */
const risk = { view: null, error: null, running: false };

async function runRisk() {
  if (!state.summary) { risk.error = '先扫描一个盘或目录：风险清单是在索引里过规则。'; renderRisk(); return; }
  risk.running = true;
  risk.error = null;
  renderRisk();
  const min = document.getElementById('risk-min-size').value.trim();
  const limit = document.getElementById('risk-limit').value.trim();
  try {
    risk.view = await call('sensitive_scan', {
      min,
      limit: limit ? Number(limit) : null,
    });
  } catch (e) {
    risk.error = typeof e === 'string' ? e : (e && e.message) || String(e);
  } finally {
    risk.running = false;
    renderRisk();
  }
}

/** Windows 属性位 → 中文。只列"看得懂而且有意义"的那几个。 */
const FW_ATTR_BITS = [
  [0x0001, '只读'], [0x0002, '隐藏'], [0x0004, '系统'], [0x0020, '存档'],
  [0x0100, '临时'], [0x0200, '稀疏'], [0x0400, '重解析点'],
  [0x0800, '压缩'], [0x1000, '离线'], [0x2000, '未建索引'], [0x4000, '加密'],
];

/** 索引里的 `NODE_*` 位 → 中文（来自 MFT 解析，不是现场 stat）。 */
const FW_NODE_BITS = [
  [0x0004, '硬链接'], [0x0008, '重解析点'], [0x0010, '压缩'],
  [0x0020, '稀疏'], [0x0040, '含 ADS'], [0x0080, '数据驻留'],
  [0x0100, '属性列表溢出'], [0x0200, '孤儿节点'],
];

function attrChips(bits, table) {
  return table.filter(([bit]) => bits & bit).map(([, name]) => `<span class="tag">${name}</span>`).join('');
}

function renderRisk() {
  const badge = document.getElementById('risk-state');
  const sum = document.getElementById('risk-summary');
  const box = document.getElementById('risk-list');
  const v = risk.view;

  badge.textContent = risk.running ? '盘点中' : risk.error ? '失败' : v ? `${v.matched.toLocaleString()} 条` : '未盘点';
  badge.className = 'badge' + (v && !risk.error ? ' mft' : '');

  if (risk.error) {
    sum.textContent = risk.error;
    sum.className = 'hint note-warn';
  } else if (!v) {
    sum.textContent = '点「盘点」过一遍索引。';
    sum.className = 'hint';
  } else if (!v.matched) {
    sum.textContent = `按 ${v.rules} 条规则扫过 ${v.scanned.toLocaleString()} 个文件，没有命中（用时 ${v.elapsed_ms} ms）`;
    sum.className = 'hint';
  } else {
    sum.textContent = `${v.matched.toLocaleString()} 条命中`
      + (v.truncated ? `，只列前 ${v.rows.length.toLocaleString()} 条` : '')
      + `　按 ${v.rules} 条规则扫过 ${v.scanned.toLocaleString()} 个文件，用时 ${v.elapsed_ms} ms`;
    sum.className = 'hint';
  }

  box.innerHTML = '';
  if (!v || !v.rows.length) {
    box.innerHTML = `<div class="mon-empty">${v ? '没有命中。这不等于「没有风险」：只看文件名与扩展名，\n文件内容里有没有密钥它不知道。' : '还没有盘点。'}</div>`;
    return;
  }

  const head = document.createElement('div');
  head.className = 'rhead';
  head.innerHTML = '<span>级别</span><span>分类</span><span>文件名</span>'
    + '<span class="d">大小</span><span>修改时间</span><span>所在目录</span>';
  box.appendChild(head);

  for (const h of v.rows) {
    const row = document.createElement('div');
    row.className = 'rrow';
    const dir = h.path.length > h.name.length
      ? h.path.slice(0, h.path.length - h.name.length).replace(/[\\/]+$/, '')
      : '';
    row.title = `${h.path}\n${h.why}`;
    row.innerHTML =
      `<span class="k ${h.severity}">${esc(h.severity_cn)}</span>` +
      `<span class="cat">${esc(h.category)}</span>` +
      `<span class="n">${esc(fwMiddleEllipsis(h.name, 48))}</span>` +
      `<span class="d">${humanSize(h.size)}</span>` +
      `<span class="t">${esc(fwLocalTime(h.mtime))}</span>` +
      `<span class="p">${esc(dir || '—')}</span>`;
    ContextMenu.bind(row, { kind: 'node', node: h.node });
    row.ondblclick = async () => {
      try {
        const parent = await call('node_parent', { node: h.node });
        monShow(false); setView('map'); await goto(parent); await pickNode(h.node);
      } catch (e) { /* call() 已提示 */ }
    };
    box.appendChild(row);
  }
}

/* ── E4 规则试算 ─────────────────────────────────────────────────────────
 *
 * 只对当前索引负责：绝对阈值现在就能判，增量/速率必须有监控起点。
 * 所以后端把基线设成"现在"，judgeable=false 的那几条要**明确说不能判**，
 * 而不是显示"不会触发" —— 后者会被当成"阈值设得很安全"。
 */
async function runSimulate() {
  const hint = document.getElementById('mon-sim-hint');
  const rules = document.getElementById('mon-rules').value;
  const paths = document.getElementById('mon-paths').value
    .split('\n').map((s) => s.trim()).filter(Boolean);
  // 没填目标时用当前视图根：绝大多数场景就是"我想盯这个目录"
  if (!paths.length && state.focus != null) {
    const p = cachedPath(state.focus);
    if (p) paths.push(p);
  }
  if (!paths.length) { hint.textContent = '先填一个监控目标，或在左侧选一个目录'; return; }
  hint.textContent = '试算中…';
  try {
    const v = await call('rules_simulate', { paths, rules });
    renderSim(v);
  } catch (e) {
    hint.textContent = typeof e === 'string' ? e : '试算失败';
  }
}

function renderSim(v) {
  const box = document.getElementById('mon-sim');
  const hint = document.getElementById('mon-sim-hint');
  if (!v) { box.classList.add('hidden'); box.innerHTML = ''; return; }
  box.classList.remove('hidden');

  hint.textContent = v.fires
    ? `会立刻触发 ${v.fires} 条`
    : (v.verdicts.some((x) => !x.judgeable) ? '绝对阈值都没超（增量/速率要跑起来才算）' : '按当前数据都不会触发');

  const ruleText = (i) => (v.rules[i] ? v.rules[i].text : `#${i + 1}`);
  const rows = v.verdicts.map((x) => {
    let right;
    if (!x.judgeable) {
      right = '<span class="dim">要跑满一个窗口才能判断</span>';
    } else if (x.fires) {
      right = `<span class="bad">超出 ${humanSize(Math.abs(x.gap))}</span>`;
    } else {
      right = `<span class="ok">还差 ${humanSize(Math.abs(x.gap))}</span>`;
    }
    return `<div class="sim-row${x.fires ? ' fire' : ''}">`
      + `<span class="mono">${esc(ruleText(x.rule_index))}</span>`
      + `<span class="p" title="${esc(x.path)}">${esc(tailPath(x.path, 40))}</span>`
      + `<span class="${x.judgeable ? '' : 'dim'}">${x.judgeable ? humanSize(x.observed) : '—'}</span>`
      + right
      + '</div>';
  }).join('');

  const missing = v.missing.length
    ? `<p class="attr-note">这些路径不在索引里，没参与试算：${esc(v.missing.join('、'))}</p>`
    : '';
  box.innerHTML = rows + missing;
}

/* ── E3 导出单文件 HTML 报告 ─────────────────────────────────────────────
 *
 * 落盘位置由用户选**文件夹**、文件名由后端拼（与快照 / CSV 一套规矩：
 * 里面带 UTC 时间戳，和正文里的生成时间对得上）。
 */
async function exportReport() {
  if (!state.summary) { toast('先扫描一个盘或目录再导出报告', true); return; }
  let dir;
  try {
    dir = await call('choose_path', { kind: 'folder', title: '把 HTML 报告存到哪个文件夹' });
  } catch (e) { return; }
  if (!dir) return;
  try {
    const out = await call('export_report', { path: dir, top: 30 });
    toast(`报告已导出：${out}`);
  } catch (e) { /* call() 已提示 */ }
}

function wireViews() {
  document.getElementById('view-tab-map').onclick = () => setView('map');
  document.getElementById('view-tab-dup').onclick = () => setView('dup');
  document.getElementById('view-tab-diff').onclick = () => setView('diff');
  document.getElementById('view-tab-search').onclick = () => setView('search');
  document.getElementById('view-tab-risk').onclick = () => setView('risk');
  document.getElementById('view-tab-trend').onclick = () => setView('trend');
  document.getElementById('trend-record').onclick = recordTrend;

  document.getElementById('risk-run').onclick = runRisk;
  for (const id of ['risk-min-size', 'risk-limit']) {
    document.getElementById(id).onkeydown = (e) => {
      if (e.key === 'Enter') { e.preventDefault(); runRisk(); }
    };
  }

  document.getElementById('dup-start').onclick = startDup;
  document.getElementById('dup-export').onclick = dupExport;

  document.getElementById('diff-save').onclick = saveSnapshot;
  document.getElementById('diff-compare').onclick = compareSnapshot;
  document.getElementById('diff-export').onclick = diffExport;
  document.getElementById('diff-pick').onclick = () =>
    pickInto('diff-path', 'file', '选择基准快照（.tsv）', ['tsv']);
  document.getElementById('diff-save-pick').onclick = () =>
    pickInto('diff-save-path', 'folder', '把快照存到哪个文件夹');
  document.getElementById('diff-csv-pick').onclick = () =>
    pickInto('diff-csv', 'folder', '把差异 CSV 存到哪个文件夹');
  document.getElementById('diff-attr-toggle').onclick = toggleDiffAttr;
  document.getElementById('diff-order').onchange = () => {
    // 已经比过一次的话，换个排序就重排一遍（后端重跑 diff 很快，数据都在内存里）
    if (diff.view) compareSnapshot();
  };

  document.getElementById('search-run').onclick = runSearch;
  // 回车即搜：这个框的用途是"打完字就要结果"，再让用户去点一次没有道理
  for (const id of ['search-text', 'search-content']) {
    document.getElementById(id).onkeydown = (e) => {
      if (e.key === 'Enter') { e.preventDefault(); runSearch(); }
    };
  }
  // 换匹配方式/范围：已经搜过一次的话立刻重搜（索引在内存里，代价很小）
  for (const id of ['search-mode', 'search-scope']) {
    document.getElementById(id).onchange = () => { if (searchView.view) runSearch(); };
  }

  setView('map');
  renderDup();
  renderDiff();
  renderSearch();
  renderContent();
}

/* ── 启动 ──────────────────────────────────────────────────────────────── */

async function boot() {
  ContextMenu.wire();
  Treemap.init({
    nameOf: cachedName,
    pathOf: cachedPath,
    sizeText: humanSize,
    onPick: pickNode,
    onContext: (event, node, canvas) => ContextMenu.open(event, node === null ? { kind: 'map' } : { kind: 'node', node }, canvas),
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

  // 渲染层的着色模式与顶层选择保持一致（下拉框的 value 就是后端认识的字符串）
  Treemap.setShading(state.shading);
  renderHeatLegend();

  await loadVolumes();

  // 恢复上次的折叠状态。偏好文件坏掉/缺这个字段时是 `undefined !== true`，
  // 当没折叠处理。过渡在**首帧之后**才挂上（`drives-animated`），否则每次
  // 启动都会把「恢复成折叠态」补放成一段收拢动画。
  const startupPrefs = await loadPrefs();
  applyDrivesCollapsed(startupPrefs.drives_collapsed === true);
  applyAlertsCollapsed(startupPrefs.alerts_collapsed === true);
  // 恢复运行模式。启动时**不弹**代价说明（silent）：那句话是"切换时"才需要的，
  // 每次启动都弹一遍就变成噪音了。
  await applyProfile(startupPrefs.profile || 'balanced', { silent: true });
  // 关闭软件：非法值由 applyCloseBehavior 归一成 'quit'（与后端 `clamped` 同口径）
  applyCloseBehavior(startupPrefs.close_behavior);
  // 开机自启的真相在注册表里，每次启动都问一次后端
  await loadAutostart();
  requestAnimationFrame(() => document.body.classList.add('drives-animated'));

  await loadElevation();
  wireAdminModal();
  wireMonitor();
  wireIndependent();
  document.getElementById('tree-go').onclick = goTreePath;
  document.getElementById('tree-path').onkeydown = e => { if (e.key === 'Enter') goTreePath(); };
  wireMenus();
  wireViews();
  // 监控抽屉、布局选择、窗口缩放都会改变画布尺寸，需要重新计算矩形坐标。
  let resizeTimer;
  const canvasObserver = new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => refreshLayout().catch(() => {}), 120);
  });
  canvasObserver.observe(document.getElementById('canvas').parentElement);
  backfillMonitor().catch(() => {});
  backfillDup().catch(() => {});
  backfillDiff().catch(() => {});
  // 风险清单没有"后台作业"，开场画一次空状态就行 —— 但必须画：
  // 页签上的徽标与提示语由这个函数负责，不画就是"未盘点"三个字都没有。
  renderRisk();

  document.getElementById('refresh-vols').onclick = loadVolumes;
  document.getElementById('drives-toggle').onclick = toggleDrives;
  document.getElementById('alerts-toggle').onclick = toggleAlerts;
  document.getElementById('profile').onchange = onProfileChange;
  document.getElementById('autostart-toggle').onchange = toggleAutostart;
  // 两个 radio 共用 name，change 只在被选中的那个上触发
  for (const id of ['close-tray', 'close-quit']) {
    document.getElementById(id).onchange = onCloseBehaviorChange;
  }
  document.getElementById('mode').onchange = (e) => {
    state.mode = e.target.value;
    refreshLayout(); renderTree(); renderExts(); renderTopFiles();
  };
  document.getElementById('shading').onchange = (e) => {
    state.shading = e.target.value;
    // 告诉渲染层当前模式：年龄要走热力图色带，其余走「槽位定色相 + 强度调亮度」
    Treemap.setShading(state.shading);
    renderHeatLegend();
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
  // 后台周期任务刷新了容量（只有性能模式会跑）。**只更新数字**，不碰视图：
  // 用户正在看的树、面包屑、选中的方块都不该因为后台刷新一下而变。
  await listen('volumes:tick', (ev) => {
    state.volumes = ev.payload || [];
    renderVolumes();
  });
  await listen('scan:progress', (ev) => progressUI(ev.payload));
  await listen('scan:partial', (ev) => onScanPartial(ev.payload));
  await listen('scan:done', (ev) => afterScan(ev.payload, null));
  await listen('scan:error', (ev) => afterScan(null, String(ev.payload)));
  await listen('admin:relaunch', (ev) => onRelaunchOutcome(ev.payload || {}));

  // 监控
  await listen('monitor:started', (ev) => {
    mon.revision += 1;
    mon.backfillRequest += 1;
    mon.starting = false;
    mon.info = ev.payload;
    mon.running = true;
    monShow(true);
    if (ev.payload && ev.payload.reopened) {
      toast('为建立增量来源，已重新扫描一次（监控需要一个事件源）');
    }
    for (const w of (ev.payload && ev.payload.warnings) || []) toast(w);
    renderMonHead();
  });
  // 托盘右键 →「监控事件…」或某条预警：把窗口叫出来并切到监控页签。
  // 这是"程序收在托盘里时也能看到刚报了什么"的那条路 —— 用户不必先找窗口。
  await listen('tray:open-monitor', () => {
    document.getElementById('nav-monitor').click();
    renderMonHead();
    backfillMonitor().catch(() => {});
  });
  await listen('tray:alert', (ev) => {
    // 窗口在前台时，系统通知可能被系统折叠；界面里再提示一句（同一时刻只此一处）
    if (!document.hidden) {
      toast(String(ev.payload || '阈值预警'), true);
    }
    mon.revision += 1;
    backfillMonitor().catch(() => {});
  });

  await listen('monitor:tick', (ev) => onMonitorTick(ev.payload || {}));
  await listen('monitor:stopped', (ev) => {
    mon.revision += 1;
    mon.backfillRequest += 1;
    mon.stopping = false;
    mon.starting = false;
    mon.running = false;
    mon.snap = ev.payload || mon.snap;
    state.names.clear();
    state.meta.clear();
    // 收尾后索引稳定了，一次性把面板刷到最新
    renderMonHead();
    backfillMonitor().catch(() => {});
    refreshLayout().catch(() => {});
    renderTree().catch(() => {});
    renderExts().catch(() => {});
    renderTopFiles().catch(() => {});
  });
  await listen('monitor:error', (ev) => {
    mon.revision += 1;
    mon.backfillRequest += 1;
    mon.starting = false;
    mon.stopping = false;
    mon.running = false;
    mon.snap = Object.assign({}, mon.snap || {}, { error: String(ev.payload) });
    renderMonHead();
    renderMonList();
    toast('监控出错：' + String(ev.payload), true);
  });

  // F12 重复文件（后台线程推的三条事件）
  await listen('dup:progress', (ev) => {
    dup.progress = ev.payload;
    dup.running = true;
    renderDup();
  });
  await listen('dup:done', (ev) => {
    dup.running = false;
    dup.progress = null;
    dup.result = ev.payload;
    dup.error = null;
    renderDup();
    toast(ev.payload.total_wasted
      ? `找到 ${ev.payload.group_count} 组重复，可回收 ${humanSize(ev.payload.total_wasted)}`
      : '没有发现重复文件');
  });
  await listen('dup:error', (ev) => {
    dup.running = false;
    dup.progress = null;
    dup.error = String(ev.payload);
    renderDup();
    toast('查找失败：' + String(ev.payload), true);
  });

  // F11 快照作业（后台线程推的四条事件）
  await listen('diff:progress', (ev) => {
    const p = ev.payload;
    if (!diff.busy) diffStart(p.kind === 'save' ? 'save' : 'compare', p.stage);
    diff.stage = p.stage;
    diff.ratio = p.ratio;
    renderDiff();
  });
  await listen('diff:done', (ev) => {
    diff.view = ev.payload;
    diff.error = null;
    diffStop();
    if (diff.view.rows_total) {
      toast(`对比完成：共 ${diff.view.rows_total.toLocaleString()} 行变化`);
    }
  });
  await listen('diff:saved', (ev) => {
    diffStop();
    onSnapshotSaved(ev.payload);
  });
  await listen('diff:error', (ev) => {
    diff.view = null;
    diff.error = String(ev.payload);
    diffStop();
  });

  // F13 内容搜索（后台线程推的三条事件）
  await listen('content:progress', (ev) => {
    const p = ev.payload;
    contentSearch.running = true;
    contentSearch.progress = p;
    // 阶段文字与条由 renderContent 统一画（命中数要等 done 才知道）
    renderContent();
  });
  await listen('content:done', (ev) => {
    contentSearch.running = false;
    contentSearch.progress = null;
    contentSearch.view = ev.payload;
    contentSearch.error = null;
    contentSearch.hitCount = (ev.payload && ev.payload.hits.length) || 0;
    document.getElementById('search-run').disabled = searchView.running;
    renderSearch();
    renderContent();
    if (contentSearch.hitCount) {
      toast(`内容命中 ${contentSearch.hitCount.toLocaleString()} 个文件`);
    }
  });
  await listen('content:error', (ev) => {
    contentSearch.running = false;
    contentSearch.progress = null;
    contentSearch.view = null;
    contentSearch.error = String(ev.payload);
    document.getElementById('search-run').disabled = searchView.running;
    renderSearch();
    renderContent();
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
