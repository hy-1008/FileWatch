'use strict';
// Pure memory: all IPC, Explorer, clipboard and monitor mutations are substituted.
const assert = require('node:assert/strict'), fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const requests = [], actions = [], copied = [], notices = [], elements = new Map(), handlers = {};
let info = { node: 44, is_dir: false, name: 'a & <b>.txt', path: 'C:\\测试\\a & <b>.txt', size: 20, alloc: 4096 };
let hold = null, rejectClipboard = false;
function el(id) {
  if (!elements.has(id)) {
    const active = new Set(['hidden']);
    const e = { id, children: [], style: {}, dataset: {}, isConnected: true, disabled: false,
      classList: { add: c => active.add(c), remove: c => active.delete(c), contains: c => active.has(c), toggle: (c, on) => on ? active.add(c) : active.delete(c) },
      appendChild(child) { this.children.push(child); }, replaceChildren() { this.children = []; }, setAttribute(k, v) { this[k] = v; },
      closest() { return null; }, contains(child) { return child === this || this.children.includes(child); },
      getBoundingClientRect() { return { left: 15, top: 15, width: 300, height: 260 }; },
      querySelectorAll() { return this.children.filter(c => c.role === 'menuitem' && !c.disabled); },
      querySelector() { return this.querySelectorAll()[0] || null; },
      focus() { scope.document.activeElement = this; }, showModal() { this.open = true; }, select() { this.selected = true; },
    };
    elements.set(id, e);
  }
  return elements.get(id);
}
const scope = { console, setTimeout, clearTimeout, actions, notices,
  navigator: { clipboard: { writeText: async s => { if (rejectClipboard) throw Error('clipboard denied'); copied.push(s); } } },
  document: { activeElement: null, body: { classList: { toggle() {} } }, getElementById: el, createElement: () => el(Symbol()),
    querySelector: () => el('wrap'), querySelectorAll: () => [], addEventListener: (name, fn, capture) => { handlers[name + (capture ? '-capture' : '')] = fn; } },
  window: { innerWidth: 960, innerHeight: 600, addEventListener: (name, fn) => { handlers[name] = fn; },
    __TAURI__: { core: { invoke: async (command, args) => {
      requests.push({ command, args });
      if (command === 'node_detail') return hold ? await hold : info;
      if (command === 'node_parent') return 10;
      if (command === 'reveal_path') return { path: 'C:\\测试', exact: false };
      if (command === 'explore_path') return { path: 'C:\\测试', exact: false };
      throw Error('Unexpected command: ' + command);
    } }, event: { listen: async () => () => {} } } },
};
vm.createContext(scope);
const src = path.join(__dirname, '../src');
for (const name of ['independent.js', 'context-menu.js']) vm.runInContext(fs.readFileSync(path.join(src, name), 'utf8'), scope);
const app = fs.readFileSync(path.join(src, 'app.js'), 'utf8');
vm.runInContext(app.slice(0, app.lastIndexOf('\nboot().catch')), scope);
vm.runInContext(`toast = s => notices.push(s); goto = async n => actions.push(['goto',n]); pickNode = async n => actions.push(['pick',n]);
  monShow = b => actions.push(['show',b]); setView = v => actions.push(['view',v]);
  startScan = async (drive,root) => actions.push(['scan',drive,root]);
  switchIndependentMode = async mode => actions.push(['mode',mode]); editIndependent = t => actions.push(['edit',t]);
  mutateIndependent = async (command,args) => actions.push([command,args]);
  renderExts = async () => {};`, scope);
const api = vm.runInContext('({ state, mon, ind, ContextMenu, contextItems, contextCopy, contextRecord, contextMonitor, contextParent })', scope);
const action = (model, id) => model.items.find(i => i.id === id);
function event(target, overrides = {}) {
  return { type: 'contextmenu', target, currentTarget: target, clientX: 955, clientY: 595,
    preventDefault() { this.prevented = true; }, stopPropagation() { this.stopped = true; }, ...overrides };
}
(async () => {
  api.state.summary = { root: 'C:\\测试' }; api.state.focus = 5;
  const node = await api.contextItems({ kind: 'node', node: 44 });
  assert.equal(node.title, info.path);
  assert(!node.items.some(i => ['delete', 'execute', 'rename'].includes(i.id)));
  await action(node, 'locate').action();
  assert(actions.some(a => a[0] === 'goto' && a[1] === 10));
  await action(node, 'copy-path').action(); assert.equal(copied.pop(), info.path);
  const fileInfo = info;
  info = { ...info, name: '.', path: 'C:\\deep\\root', is_dir: true };
  await action(await api.contextItems({ kind: 'node', node: 44 }), 'copy-name').action();
  assert.equal(copied.pop(), 'root', 'synthetic root name must be replaced by the real folder name');
  info = fileInfo;
  await action(node, 'reveal').action(); assert.match(notices.pop(), /仍存在的上级目录/);
  assert.deepEqual(requests.filter(r => r.command === 'reveal_path').map(r => r.args.path), [info.path]);
  const session = api.state.summary; api.state.summary = {};
  assert.throws(action(node, 'detail').action, /扫描结果已切换/); api.state.summary = session;
  // 「打开」与「定位」是两件事：打开进到这个文件夹，定位则选中它
  await action(node, 'open').action();
  assert.equal(requests.filter(r => r.command === 'explore_path').at(-1).args.path, info.path);
  assert.match(notices.pop(), /已打开仍存在的上级目录/);
  assert.equal(action(node, 'open').label, '在文件资源管理器打开');

  const row = { seq: 2, time_ms: 1700000000123, kind: 'modified', kind_cn: '修改', path: 'C:\\' + '长路径'.repeat(90), name: 'test', size_text: '+2 KB', suffix: '合并 3 次' };
  const record = await api.contextItems({ kind: 'record', row });
  await action(record, 'copy-record').action();
  assert(copied.pop().includes(row.path), 'copy must preserve the full path, never the truncated DOM text');
  assert.match(api.contextRecord(row), /合并 3 次/);

  // 快照对比行：定位要发原始路径（不是 DOM 里被截断的那段），复制要带变化值
  const drow = { kind: 'grew', kind_cn: '变大', delta: 2048, old_size: 10, new_size: 2058, path: 'C:\\测试\\报告 v2.pdf' };
  const diffMenu = await api.contextItems({ kind: 'diffrow', row: drow });
  assert.equal(diffMenu.title, drow.path);
  assert(!diffMenu.items.some(i => i.id === 'monitor'), '对比行是历史差异，不提供「加入独立监控」');
  await action(diffMenu, 'reveal').action();
  assert.equal(requests.filter(r => r.command === 'reveal_path').at(-1).args.path, drow.path);
  await action(diffMenu, 'copy-diff').action();
  const diffText = copied.pop();
  assert(diffText.includes(drow.path) && diffText.includes('变大') && /\+2(\.\d+)? KB/.test(diffText), diffText);
  await action(diffMenu, 'copy-name').action();
  assert.equal(copied.pop(), '报告 v2.pdf');
  api.ind.targets = [{ id: 7, path: info.path, rules: ['grow=1G'], enabled: true }];
  const target = await api.contextItems({ kind: 'target', id: 7 });
  await action(target, 'toggle').action();
  const toggle = actions.pop(); assert.equal(toggle[0], 'independent_upsert'); assert.equal(toggle[1].enabled, false);
  await action(target, 'remove').action(); assert.equal(actions.pop()[0], 'independent_remove');
  assert(!target.items.some(i => i.id === 'monitor'), 'existing target must not offer duplicate creation');
  await api.contextMonitor(info.path);
  assert.equal(actions.findLast(a => a[0] === 'edit')[1].id, 7, 'matching path edits the existing monitor');
  await api.contextMonitor('D:\\new.txt');
  assert.equal(el('ind-path').value, 'D:\\new.txt');

  api.mon.running = true;
  const volume = await api.contextItems({ kind: 'volume', volume: { drive: 'C:', root: 'C:', label: '系统', total: 100, free: 30, fs_name: 'NTFS' } });
  assert(action(volume, 'scan').disabled, 'linked monitor scan lock must be respected');
  api.mon.running = false;
  const type = await api.contextItems({ kind: 'type', row: { ext: '.log', ext_id: 3, size: 100, count: 4 } });
  assert.equal(action(type, 'highlight').label, '高亮此文件类型');
  assert.equal(api.contextParent('C:\\a.txt'), 'C:\\');
  assert.equal(api.contextParent('\\\\server\\share\\a.txt'), '\\\\server\\share');
  assert.equal(api.contextParent('\\\\server\\share'), null);
  rejectClipboard = true; await api.contextCopy(info.path);
  assert.equal(el('copy-text').value, info.path); assert(el('copy-text').selected); assert(el('copy-dialog').open);
  rejectClipboard = false;

  api.ContextMenu.wire();
  const origin = el('origin'); api.ContextMenu.bind(origin, { kind: 'type', row: { ext: '.log', ext_id: 3, size: 100, count: 4 } });
  const right = event(origin); await origin.oncontextmenu(right); assert(right.prevented);
  const menu = el('context-menu'); assert(!menu.classList.contains('hidden'));
  assert(parseInt(menu.style.left) <= 652 && parseInt(menu.style.top) <= 332, 'menu must fit the viewport');
  const buttons = menu.querySelectorAll(); assert.equal(scope.document.activeElement, buttons[0]);
  menu.onkeydown(event(menu, { type: 'keydown', key: 'End' })); assert.equal(scope.document.activeElement, buttons.at(-1));
  menu.onkeydown(event(menu, { type: 'keydown', key: 'Home' })); assert.equal(scope.document.activeElement, buttons[0]);
  const escape = event(menu, { type: 'keydown', key: 'Escape' }); handlers['keydown-capture'](escape);
  assert(escape.prevented && escape.stopped); assert(menu.classList.contains('hidden')); assert.equal(scope.document.activeElement, origin);
  const nativeInput = event({ closest: () => ({}) }); await api.ContextMenu.open(nativeInput, { kind: 'map' }, origin);
  assert(!nativeInput.prevented, 'input fields retain their native editing menu');
  await origin.onkeydown(event(origin, { type: 'keydown', key: 'F10', shiftKey: true })); assert(!menu.classList.contains('hidden'));
  handlers.resize(); assert(menu.classList.contains('hidden'));

  let release; hold = new Promise(resolve => { release = resolve; });
  const stale = api.ContextMenu.open(event(origin), { kind: 'node', node: 44 }, origin);
  await origin.oncontextmenu(event(origin));
  release(info); await stale; hold = null;
  assert.equal(menu.children[0].textContent, '.log · 4 个文件', 'slow node lookup must not replace a newer right click');
  assert(!menu.children.some(c => c.innerHTML), 'untrusted filenames must never become menu HTML');
  console.log('context menus: per-object actions, exact target paths, stale scans, clipboard fallback, viewport bounds, keyboard, input preservation and async races passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
