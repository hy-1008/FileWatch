/* Canvas 面积图渲染 + 命中测试 + 交互。
 *
 * # 交互模型（一个刻意的取舍，说清楚免得后来人以为是漏做了）
 *
 * 布局是**屏幕空间递归**的：每一层都用当前画布的像素重跑一次，所以标题栏恒定
 * 16 px、间距恒定 1~2 px，缩放时不会变形（文档 §5.6）。这带来一个后果 ——
 * 自由拖拽缩放与它天然冲突：如果把整张图当作一张大位图去缩放，那些「恒定像素」
 * 的装饰会跟着放大，屏幕空间递归就白做了。
 *
 * 所以这里的「缩放」表达为**层级导航**：
 *   滚轮向上 / 双击  → 聚焦光标下最深的那个方块（等价于放大）
 *   滚轮向下 / Esc   → 回到上一层（等价于缩小）
 * 连按滚轮就能一路钻到最深处，这正是 WizTree / SpaceSniffer 的模型，也是这类
 * 「每层铺满视口」的面积图唯一自洽的缩放方式。
 *
 * 「细节」滑块控制 LOD 的展开阈值（min_side）：往左更精简、往右展开更深。
 */

'use strict';

const Treemap = (() => {
  const canvas = document.getElementById('canvas');
  const ctx = canvas.getContext('2d', { alpha: false });
  const previous = document.getElementById('canvas-previous');
  const previousCtx = previous.getContext('2d', { alpha: false });
  const snapshot = document.createElement('canvas');
  const snapshotCtx = snapshot.getContext('2d', { alpha: false });
  const tip = document.getElementById('tip');
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
  let fade = null;
  let labelNames = new Map();

  let layout = null;      // 当前 Layout 实例
  let dpr = 1;
  let hoverIdx = -1;
  let selIdx = -1;
  /** 高亮的 ext_id 集合；null = 不高亮 */
  let highlightExt = null;
  /** 当前着色模式：`ext` / `depth` / `age`（年龄走热力图色带，见 wire.js 的 fwHeat） */
  let shadingMode = 'ext';
  let pendingShading = 'ext';
  /** node → 显示名，由 app 提供（布局里只有 id，没有名字） */
  let nameOf = () => '';
  let pathOf = () => '';
  let sizeText = (v) => String(v);
  let onPick = () => {};
  let onZoomIn = () => {};
  let onZoomOut = () => {};
  let onContext = () => {};

  /** 方块上能写字的最小屏幕尺寸。
   *
   *  **预取名字与画字必须用同一个判据** —— 两边各写一份的话，要么白取一批名字，
   *  要么出现「该画字却没有名字」的空方块。 */
  const LABEL_MIN_W = 40;
  const LABEL_MIN_H = 14;

  function measure() {
    const wrap = canvas.parentElement;
    return { w: Math.max(1, wrap.clientWidth), h: Math.max(1, wrap.clientHeight) };
  }

  function resize(size = measure()) {
    const { w, h } = size;
    dpr = Math.max(1, Math.min(2, window.devicePixelRatio || 1));
    // 给 width/height 赋同样的值也会清空位图，只在提交新帧时按需调整。
    const width = Math.round(w * dpr), height = Math.round(h * dpr);
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    return { w, h };
  }

  function cssSize() {
    return { w: canvas.width / dpr, h: canvas.height / dpr };
  }

  function stopFade() {
    if (fade) fade.cancel();
    fade = null;
    previous.style.opacity = '0';
  }

  function setLayout(l, size) {
    const oldSelection = layout && selIdx >= 0 && selIdx < layout.count ? layout.node(selIdx) : null;
    const oldHover = layout && hoverIdx >= 0 && hoverIdx < layout.count ? layout.node(hoverIdx) : null;
    const smooth = layout && layout.count > 0 && !reducedMotion.matches && typeof previous.animate === 'function';
    if (smooth) {
      snapshot.width = canvas.width;
      snapshot.height = canvas.height;
      snapshotCtx.globalAlpha = 1;
      snapshotCtx.drawImage(canvas, 0, 0);
      // 连续刷新从当前可见的合成画面开始，避免中途打断渐变时跳回上一帧。
      if (fade) {
        snapshotCtx.globalAlpha = Number(getComputedStyle(previous).opacity) || 0;
        snapshotCtx.drawImage(previous, 0, 0, snapshot.width, snapshot.height);
        snapshotCtx.globalAlpha = 1;
      }
    }
    stopFade();
    layout = l;
    shadingMode = pendingShading;
    labelNames = new Map();
    hoverIdx = -1;
    selIdx = -1;
    for (let i = 0; i < l.count; i++) {
      const node = l.node(i);
      if (node === oldSelection) selIdx = i;
      if (node === oldHover) hoverIdx = i;
      const r = l.rect(i);
      if (r[2] >= LABEL_MIN_W && r[3] >= LABEL_MIN_H) labelNames.set(node, nameOf(node));
    }
    tip.classList.add('hidden');
    resize(size);
    draw();
    if (smooth) {
      previous.width = canvas.width;
      previous.height = canvas.height;
      previousCtx.drawImage(snapshot, 0, 0, previous.width, previous.height);
      fade = previous.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 160, easing: 'ease-out' });
      const active = fade;
      active.onfinish = () => { if (fade === active) stopFade(); };
    }
  }

  function pick(x, y) {
    if (!layout) return -1;
    return layout.hit(x, y);
  }

  /* ── 绘制 ──────────────────────────────────────────────────────────── */

  function draw() {
    const { w, h } = cssSize();
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--bg-canvas').trim() || '#11171f';
    ctx.fillRect(0, 0, w, h);
    if (!layout || layout.count === 0) return;

    // ① 先铺色块。父节点先画、子节点覆盖其上 —— 后端输出顺序就是这个保证，
    //    所以我们只要正序画一遍即可，不需要自己排序。
    for (let i = 0; i < layout.count; i++) {
      const r = layout.rect(i);
      if (r[2] < 0.5 || r[3] < 0.5) continue;
      const dim = highlightExt !== null && layout.extId(i) !== highlightExt;
      ctx.globalAlpha = dim ? 0.22 : 1;
      // 年龄模式走热力图色带（同一个槽位、按 shade_t 换色相），其余模式是
      // 「槽位定色相 + shade_t 调亮度」。
      ctx.fillStyle = shadingMode === 'age'
        ? fwHeat(layout.shadeT(i))
        : fwColor(layout.slot(i), layout.shadeT(i));
      ctx.fillRect(r[0], r[1], r[2], r[3]);
    }
    ctx.globalAlpha = 1;

    // ② 再画分隔线与标题栏。都放在第二遍是为了不被相邻色块盖住。
    ctx.lineWidth = 1;
    for (let i = 0; i < layout.count; i++) {
      const r = layout.rect(i);
      if (r[2] < 1 || r[3] < 1) continue;
      const hd = layout.header(i);

      if (hd[3] > 0) {
        // 目录标题栏：固定 16px 高，画在块体顶部
        ctx.fillStyle = 'rgba(0,0,0,.42)';
        ctx.fillRect(hd[0], hd[1], hd[2], hd[3]);
      }
      ctx.strokeStyle = 'rgba(0,0,0,.55)';
      ctx.strokeRect(r[0] + 0.5, r[1] + 0.5, r[2] - 1, r[3] - 1);
    }

    // ③ 文字最后画。是否画**由屏幕尺寸现场判断**，而不是只看后端的
    //    `labeled` 位：后端是按「逻辑像素」判断的，而这里要的是「能不能读」。
    ctx.textBaseline = 'top';
    ctx.font = '11px "Cascadia Mono", Consolas, "Microsoft YaHei UI", monospace';
    for (let i = 0; i < layout.count; i++) {
      const r = layout.rect(i);
      if (r[2] < LABEL_MIN_W || r[3] < LABEL_MIN_H) continue;
      const name = labelNames.get(layout.node(i));
      if (!name) continue;
      const hd = layout.header(i);
      const ty = hd[3] > 0 ? hd[1] + 3 : r[1] + 2;
      const room = Math.floor((r[2] - 6) / 6.1);   // 11px 等宽字体约 6.1px/字符
      if (room < 5) continue;
      const text = name.length > room ? name.slice(0, Math.max(1, room - 1)) + '…' : name;
      ctx.fillStyle = 'rgba(255,255,255,.92)';
      ctx.fillText(text, r[0] + 3, ty);
      // 目录再补一行大小
      if (hd[3] > 0 && r[3] > 32) {
        ctx.fillStyle = 'rgba(255,255,255,.6)';
        ctx.fillText(sizeText(layout.size(i)), r[0] + 3, ty + 13);
      }
    }

    // ④ 选中/悬停描边
    const mark = (idx, color) => {
      if (idx < 0 || idx >= layout.count) return;
      const r = layout.rect(idx);
      ctx.strokeStyle = color;
      ctx.lineWidth = 2;
      ctx.strokeRect(r[0] + 1, r[1] + 1, Math.max(0, r[2] - 2), Math.max(0, r[3] - 2));
      ctx.lineWidth = 1;
    };
    mark(selIdx, '#ffffff');
    mark(hoverIdx, '#ffd166');
  }

  /* ── 交互 ──────────────────────────────────────────────────────────── */

  function localPos(ev) {
    const b = canvas.getBoundingClientRect();
    const size = cssSize();
    return { x: (ev.clientX - b.left) * size.w / Math.max(1, b.width),
             y: (ev.clientY - b.top) * size.h / Math.max(1, b.height) };
  }

  function showTip(ev, idx) {
    if (idx < 0) { tip.classList.add('hidden'); return; }
    const node = layout.node(idx);
    const kind = layout.isDir(idx) ? '目录' : '文件';
    const extra = layout.collapsed(idx) ? '　（因太小已折叠，双击可下钻）' : '';
    tip.innerHTML =
      `<div class="t-name">${esc(labelNames.get(node) || nameOf(node))}</div>` +
      `<div class="t-meta">${kind}　${sizeText(layout.size(idx))}${extra}</div>` +
      `<div class="t-path">${esc(pathOf(node))}</div>`;
    tip.classList.remove('hidden');
    const wrap = canvas.parentElement.getBoundingClientRect();
    let x = ev.clientX - wrap.left + 14;
    let y = ev.clientY - wrap.top + 14;
    // 贴边时翻到另一侧，别被裁掉
    if (x + tip.offsetWidth > wrap.width - 4) x = Math.max(4, x - tip.offsetWidth - 28);
    if (y + tip.offsetHeight > wrap.height - 4) y = Math.max(4, y - tip.offsetHeight - 28);
    tip.style.left = x + 'px';
    tip.style.top = y + 'px';
  }

  function esc(s) {
    return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  }

  canvas.addEventListener('mousemove', (ev) => {
    const { x, y } = localPos(ev);
    const idx = pick(x, y);
    if (idx !== hoverIdx) {
      hoverIdx = idx;
      draw();
    }
    showTip(ev, idx);
  });

  canvas.addEventListener('mouseleave', () => {
    hoverIdx = -1;
    tip.classList.add('hidden');
    draw();
  });

  canvas.addEventListener('click', (ev) => {
    const { x, y } = localPos(ev);
    const idx = pick(x, y);
    selIdx = idx;
    draw();
    if (idx >= 0) onPick(layout.node(idx));
  });

  // 双击 = 下钻到该方块
  canvas.addEventListener('contextmenu', (ev) => {
    const { x, y } = localPos(ev), idx = pick(x, y);
    if (idx >= 0) { selIdx = idx; draw(); }
    tip.classList.add('hidden');
    onContext(ev, idx >= 0 ? layout.node(idx) : null, canvas);
  });
  canvas.addEventListener('keydown', (ev) => {
    if (ev.key === 'ContextMenu' || (ev.shiftKey && ev.key === 'F10')) {
      onContext(ev, layout && selIdx >= 0 && selIdx < layout.count ? layout.node(selIdx) : null, canvas);
    }
  });

  // 双击 = 下钻到该方块
  canvas.addEventListener('dblclick', (ev) => {
    const { x, y } = localPos(ev);
    const idx = pick(x, y);
    if (idx >= 0) onZoomIn(layout.node(idx));
  });

  canvas.addEventListener('wheel', (ev) => {
    // 阻止 WebView 的默认滚动/缩放：这里全部由我们自己处理
    ev.preventDefault();
    const { x, y } = localPos(ev);
    if (ev.deltaY > 0) {
      onZoomOut();
    } else {
      const idx = pick(x, y);
      if (idx >= 0) {
        selIdx = idx;
        draw();
        onZoomIn(layout.node(idx));
      }
    }
  }, { passive: false });

  window.addEventListener('keydown', (ev) => {
    // 输入路径、调整下拉框或关闭弹窗时，不能误触面积图导航。
    if (ev.defaultPrevented || ev.ctrlKey || ev.altKey || ev.metaKey) return;
    const target = ev.target;
    if (target && (target.isContentEditable || target.closest?.('input, textarea, select, button, summary, .menu'))) return;
    if (document.querySelector('.overlay:not(.hidden)')) return;
    if (document.querySelector('dialog[open]')) return;
    if (document.getElementById('view-map').classList.contains('hidden')) return;
    if (ev.key === 'Escape') onZoomOut();
    else if (ev.key === 'Enter' && layout && selIdx >= 0 && selIdx < layout.count) onZoomIn(layout.node(selIdx));
  });

  // 调整窗口时先让旧帧随 CSS 尺寸缩放，应用拿到完整新布局后再替换位图。
  window.addEventListener('filewatch:appearance', draw);

  return {
    init(opts) {
      nameOf = opts.nameOf;
      pathOf = opts.pathOf;
      sizeText = opts.sizeText;
      onPick = opts.onPick;
      onZoomIn = opts.onZoomIn;
      onZoomOut = opts.onZoomOut;
      onContext = opts.onContext || (() => {});
      resize();
    },
    size: measure,
    setLayout,
    redraw: draw,
    clear() { stopFade(); layout = null; labelNames.clear(); hoverIdx = -1; selIdx = -1; tip.classList.add('hidden'); draw(); },
    setHighlight(extId) { highlightExt = extId; draw(); },
    setShading(mode) { pendingShading = mode; },
    /**
     * 指定布局里需要显示名字的节点（默认当前布局，去重、保持首次出现顺序）。
     *
     * 布局数据里只有 id，前端拿不到名字，所以得向后端要。这个方法的作用是
     * 让调用方**一次把要用的名字全取了** —— 逐个方块去要的话，一次布局就是
     * 几千次 IPC，而且每回来一个都会触发一次整图重画。
     */
    labelNodes(candidate = layout) {
      if (!candidate) return [];
      const seen = new Set();
      const out = [];
      for (let i = 0; i < candidate.count; i++) {
        const r = candidate.rect(i);
        if (r[2] < LABEL_MIN_W || r[3] < LABEL_MIN_H) continue;
        const n = candidate.node(i);
        if (seen.has(n)) continue;
        seen.add(n);
        out.push(n);
      }
      return out;
    },
    selectIdx: (i) => { selIdx = i; draw(); },
    get layout() { return layout; },
  };
})();
