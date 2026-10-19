/* Context-sensitive actions. Target text is always assigned with textContent. */
'use strict';
function contextPathKey(path) { return String(path).replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase(); }
function contextParent(path) {
  const value = String(path).replace(/\//g, '\\').replace(/\\+$/, '');
  const index = value.lastIndexOf('\\');
  if (index < 0) return null;
  if (index === 2 && /^[a-z]:/i.test(value)) return value.slice(0, 3);
  if (value.startsWith('\\\\') && value.slice(2).split('\\').length <= 2) return null;
  return value.slice(0, index) || null;
}
async function contextCopy(text) {
  try { await navigator.clipboard.writeText(String(text)); toast('已复制'); }
  catch (_) {
    const dialog = document.getElementById('copy-dialog');
    const input = document.getElementById('copy-text'); input.value = String(text);
    dialog.showModal(); input.focus(); input.select();
  }
}
async function contextReveal(path) {
  const result = await call('reveal_path', { path });
  if (!result.exact) toast(`目标已不存在，已定位到仍存在的上级目录：${result.path}`);
}
/** 在资源管理器里**打开**目标：文件夹就进这个文件夹，文件则打开它所在的目录
 *  （返回值是那个目录）。为什么不"打开文件"：那等于用默认程序执行它。 */
async function contextExplore(path) {
  const result = await call('explore_path', { path });
  if (!result.exact) toast(`目标已不存在，已打开仍存在的上级目录：${result.path}`);
}
async function contextLocate(path) {
  let node = await call('find_by_path', { path });
  if (node === null || node === undefined) {
    const parent = contextParent(path);
    if (parent) node = await call('find_by_path', { path: parent });
  }
  if (node === null || node === undefined) { toast('当前扫描结果不包含此目录，请先扫描对应范围', true); return; }
  monShow(false); setView('map'); await goto(node);
}
async function contextMonitor(path) {
  monShow(true);
  document.getElementById('monitor-mode').value = 'independent';
  await switchIndependentMode('independent');
  const existing = ind.targets.find(t => contextPathKey(t.path) === contextPathKey(path));
  editIndependent(existing || null);
  if (!existing) document.getElementById('ind-path').value = path;
  document.getElementById('ind-path').focus();
}
function contextHighlight(extId) {
  state.highlight = extId; Treemap.setHighlight(extId);
  document.getElementById('clear-highlight').classList.toggle('hidden', extId === null);
  return renderExts();
}
function contextRecord(row) {
  return [eventLocalTime(row), row.kind_cn || row.kind, row.path,
    row.old_name ? `原名称：${row.old_name}` : '', row.size_text || '', row.suffix || '', row.message || '',
    row.target_path ? `监控目标：${row.target_path}` : ''].filter(Boolean).join('\n');
}

/** Shared menu model; separate from DOM positioning for regression tests. */
async function contextItems(descriptor) {
  const item = (id, label, action, disabled = false) => ({ id, label, action, disabled });
  const separator = () => ({ separator: true });
  const common = (path, name) => [
    // 两个"打开资源管理器"的动作不是一回事：
    //   打开 —— 进到这个文件夹（是文件时进它所在的目录）
    //   定位 —— 打开上级目录并**选中**它（看清它在哪、周围有什么）
    item('open', '在文件资源管理器打开', () => contextExplore(path)),
    item('reveal', '在资源管理器中定位', () => contextReveal(path)),
    separator(), item('copy-path', '复制完整路径', () => contextCopy(path)),
    item('copy-name', '复制名称', () => contextCopy((name && name !== '.' ? name : '') || path.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || path)),
    item('monitor', '加入独立监控…', () => contextMonitor(path)),
  ];
  const busy = state.scanning || mon.running || mon.starting;
  if (descriptor.kind === 'node') {
    const session = state.summary;
    const info = await call('node_detail', { node: descriptor.node });
    if (session !== state.summary) throw new Error('扫描结果已切换，请重新右键选择目标');
    const guard = () => { if (session !== state.summary) throw new Error('扫描结果已切换，请重新选择目标'); };
    return { title: info.path, items: [
      item('detail', '查看详情', () => { guard(); return pickNode(info.node); }),
      item('locate', info.is_dir ? '进入此文件夹' : '前往所在目录', async () => {
        guard(); const node = info.is_dir ? info.node : await call('node_parent', { node: info.node });
        guard(); monShow(false); setView('map'); await goto(node);
        if (!info.is_dir) await pickNode(info.node);
      }), ...common(info.path, info.name), separator(),
      item('copy-info', '复制大小与路径', () => contextCopy(`${info.path}\n逻辑大小：${humanSize(info.size)}\n占用大小：${humanSize(info.alloc)}`)),
      separator(),
      item('parent', '当前视图返回上一级', () => { guard(); return goto(state.crumbs[state.crumbs.length - 2].node); }, state.crumbs.length < 2),
      item('refresh', '刷新当前面积图', () => { guard(); return refreshLayout(); }, state.scanning),
    ] };
  }
  if (descriptor.kind === 'volume') {
    const v = descriptor.volume, path = /^[a-z]:\\?$/i.test(v.root) ? v.root.slice(0, 2) + '\\' : v.root;
    return { title: `${v.drive} ${v.label || ''}`, items: [
      item('scan', busy ? '扫描此盘符（请先结束联动监控或扫描）' : '扫描此盘符', () => startScan(v.drive, null), busy),
      ...common(path, v.label || v.drive),
      item('copy-info', '复制容量信息', () => contextCopy(`${path}\n${v.label || ''} ${v.fs_name}\n总容量：${humanSize(v.total)}\n可用：${humanSize(v.free)}`)),
    ] };
  }
  if (descriptor.kind === 'type') {
    const row = descriptor.row;
    return { title: `${row.ext} · ${row.count.toLocaleString()} 个文件`, items: [
      item('highlight', '高亮此文件类型', () => contextHighlight(row.ext_id)),
      item('clear-highlight', '取消类型高亮', () => contextHighlight(null), state.highlight === null),
      separator(), item('copy-info', '复制类型统计', () => contextCopy(`${row.ext}\n文件数：${row.count}\n大小：${humanSize(row.size)}`)),
    ] };
  }
  if (descriptor.kind === 'target') {
    const target = ind.targets.find(t => t.id === descriptor.id);
    if (!target) throw new Error('此监控已被移除');
    return { title: target.path, items: [
      item('edit', '修改路径与阈值…', () => editIndependent(target), ind.busy),
      item('toggle', target.enabled ? '暂停监控' : '启用监控', () => mutateIndependent('independent_upsert', { id: target.id, path: target.path, rules: target.rules, enabled: !target.enabled }), ind.busy),
      separator(), item('locate', '在面积图中定位', () => contextLocate(target.path), state.focus === null),
      ...common(target.path), separator(),
      item('copy-rules', '复制阈值规则', () => contextCopy(target.rules.join('\n')), !target.rules.length),
      item('remove', '移除监控（保留文件）', () => mutateIndependent('independent_remove', { id: target.id }), ind.busy),
    ].filter(i => i.id !== 'monitor') };
  }
  if (descriptor.kind === 'record') {
    const row = descriptor.row;
    return { title: row.path, items: [
      item('copy-record', '复制完整记录', () => contextCopy(contextRecord(row))),
      item('locate', '在面积图中定位所在目录', () => contextLocate(row.path), state.focus === null),
      ...common(row.path, row.name),
      ...(row.target_path ? [item('target', '查看所属监控…', () => contextMonitor(row.target_path))] : []),
    ] };
  }
  if (descriptor.kind === 'diffrow') {
    const row = descriptor.row;
    return { title: row.path, items: [
      item('locate', '在面积图中定位所在目录', () => contextLocate(row.path), state.focus === null),
      item('reveal', '在资源管理器中定位', () => contextReveal(row.path)),
      separator(), item('copy-path', '复制完整路径', () => contextCopy(row.path)),
      item('copy-name', '复制名称', () => contextCopy(row.path.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || row.path)),
      item('copy-diff', '复制本行变化', () => contextCopy(
        `${row.kind_cn}\t${signedText(row.delta)}\t` +
        `${humanSize(row.old_size)} → ${humanSize(row.new_size)}\n${row.path}`)),
    ] };
  }
  return { title: '面积图', items: [
    item('parent', '返回上一级', () => goto(state.crumbs[state.crumbs.length - 2].node), state.crumbs.length < 2),
    item('root', '返回扫描根目录', () => goto(state.rootNode), state.rootNode === null || state.focus === state.rootNode),
    item('refresh', '刷新当前面积图', refreshLayout, state.focus === null || state.scanning),
    separator(), item('clear-highlight', '取消类型高亮', () => contextHighlight(null), state.highlight === null),
  ] };
}

const ContextMenu = (() => {
  let token = 0, anchor = null, menu = null;
  function close(restore = false) {
    token++;
    if (menu) { menu.classList.add('hidden'); menu.replaceChildren(); }
    anchor?.classList.remove('context-anchor');
    if (restore && anchor?.isConnected) anchor.focus();
    anchor = null;
  }
  function position(x, y) {
    const rect = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - rect.width - 8))}px`;
    menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - rect.height - 8))}px`;
  }
  function render(model) {
    menu.replaceChildren();
    const title = document.createElement('div'); title.className = 'context-title';
    title.textContent = model.title; title.title = model.title; menu.appendChild(title);
    for (const entry of model.items) {
      if (entry.separator) { const line = document.createElement('div'); line.className = 'context-separator'; line.setAttribute('role', 'separator'); menu.appendChild(line); continue; }
      const button = document.createElement('button'); button.type = 'button'; button.setAttribute('role', 'menuitem');
      button.tabIndex = -1; button.textContent = entry.label; button.disabled = !!entry.disabled;
      button.dataset.action = entry.id;
      button.onclick = async () => { close(true); try { await entry.action(); } catch (error) { toast(String(error), true); } };
      menu.appendChild(button);
    }
  }
  async function open(event, descriptor, element = event.currentTarget) {
    if (event.target?.closest?.('input,textarea,select,[contenteditable="true"]')) return;
    event.preventDefault(); event.stopPropagation(); close();
    for (const top of document.querySelectorAll('.menu')) top.open = false;
    anchor = element; menu = document.getElementById('context-menu');
    anchor.classList.add('context-anchor');
    const current = token, keyboard = event.type === 'keydown';
    const rect = element.getBoundingClientRect();
    const x = keyboard ? rect.left + 12 : event.clientX, y = keyboard ? rect.top + 12 : event.clientY;
    render({ title: '读取目标…', items: [] }); menu.classList.remove('hidden'); position(x, y);
    try {
      const model = await contextItems(descriptor);
      if (current !== token) return;
      render(model); position(x, y);
      menu.querySelector('button:not(:disabled)')?.focus();
    } catch (error) { if (current === token) { close(true); toast(String(error), true); } }
  }
  function bind(element, descriptor) {
    element.tabIndex = 0;
    element.oncontextmenu = event => open(event, descriptor, element);
    element.onkeydown = event => {
      if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) open(event, descriptor, element);
    };
  }
  function wire() {
    menu = document.getElementById('context-menu');
    const wrap = document.querySelector('.canvas-wrap');
    if (wrap) wrap.oncontextmenu = event => open(event, { kind: 'map' }, wrap);
    menu.oncontextmenu = e => e.preventDefault();
    menu.onkeydown = event => {
      const buttons = [...menu.querySelectorAll('button:not(:disabled)')];
      const index = buttons.indexOf(document.activeElement);
      if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
        event.preventDefault(); event.stopPropagation();
        const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
        buttons[next]?.focus();
      } else if (event.key === 'Escape' || event.key === 'Tab') { event.preventDefault(); event.stopPropagation(); close(true); }
    };
    document.addEventListener('pointerdown', e => { if (!menu.contains(e.target)) close(); });
    document.addEventListener('scroll', e => { if (!menu.contains(e.target)) close(); }, true);
    window.addEventListener('resize', () => close()); window.addEventListener('blur', () => close());
    document.addEventListener('keydown', e => {
      if (e.key === 'Escape' && !menu.classList.contains('hidden')) { e.preventDefault(); e.stopPropagation(); close(true); }
    }, true);
  }
  return { bind, open, close, wire };
})();
