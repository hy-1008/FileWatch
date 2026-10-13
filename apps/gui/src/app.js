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
}

boot().catch((e) => toast('界面初始化失败：' + (e && e.message ? e.message : e), true));
