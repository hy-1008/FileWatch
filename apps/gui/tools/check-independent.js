'use strict';
// In-memory controller regression: no disk watching, writes or deletes.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const requests = [], elements = new Map(), notifications = [], navigations = [];
function element(id) {
  if (!elements.has(id)) {
    const classes = new Set();
    const e = { value: '', checked: false, disabled: false, children: [], textContent: '',
      classList: { add: c => classes.add(c), remove: c => classes.delete(c), toggle: (c, on) => on ? classes.add(c) : classes.delete(c), contains: c => classes.has(c) },
      setAttribute() {}, appendChild(child) { this.children.push(child); },
    };
    Object.defineProperty(e, 'innerHTML', { set(v) { this.html = v; this.children = []; }, get() { return this.html; } });
    elements.set(id, e);
  }
  return elements.get(id);
}
const scope = { console, setTimeout, clearTimeout, navigations, notifications,
  window: { __TAURI__: { core: { invoke: (command, args) => new Promise((resolve, reject) => requests.push({ command, args, resolve, reject })) }, event: { listen: async () => () => {} } } },
  document: { body: { classList: { toggle() {} } }, getElementById: element, createElement: () => element(Symbol()), querySelectorAll: () => [] },
};
vm.createContext(scope);
const dir = path.join(__dirname, '../src');
vm.runInContext(fs.readFileSync(path.join(dir, 'context-menu.js'), 'utf8'), scope);
vm.runInContext(fs.readFileSync(path.join(dir, 'independent.js'), 'utf8'), scope);
const app = fs.readFileSync(path.join(dir, 'app.js'), 'utf8');
vm.runInContext(app.slice(0, app.lastIndexOf('\nboot().catch')), scope);
vm.runInContext(`toast = (s) => notifications.push(s); goto = async n => navigations.push(n); pickNode = async n => {}; setView = () => {};`, scope);
const api = vm.runInContext('({ ind, mon, state, eventLocalTime, refreshIndependent, mutateIndependent, editIndependent, goTreePath, renderTopFiles, renderMonList })', scope);
const turn = () => new Promise(setImmediate);
const snapshot = (name) => ({ targets: [{ id: 1, path: name, is_dir: true, rules: [], enabled: false, status: { state: 'paused', size: 0, baseline: 0, events: 0 } }], rows: [], alerts: [] });
(async () => {
  const epoch = Date.UTC(2026, 9, 15, 4, 5, 6, 789), local = new Date(epoch);
  const p = (n, w = 2) => String(n).padStart(w, '0');
  const expected = `${local.getFullYear()}-${p(local.getMonth()+1)}-${p(local.getDate())} ${p(local.getHours())}:${p(local.getMinutes())}:${p(local.getSeconds())}.789`;
  assert.equal(api.eventLocalTime({ time_ms: epoch }), expected, 'must follow the process local timezone');
  api.mon.info = { started_ms: epoch - 500 };
  assert.equal(api.eventLocalTime({ at_ms: 500 }), expected, 'legacy rows use start epoch plus elapsed time');
  api.ind.mode = 'independent';
  api.renderMonList();
  assert.match(element('mon-list').innerHTML, /新增并启用独立监控目标/);
  api.ind.alerts = [{ kind: 'rate', observed: 2048, current: 1073741824, threshold: 1024, time_ms: epoch, path: 'C:\\test', kind_cn: '增长速率' }];
  api.renderMonList();
  assert.match(element('mon-alert-list').children[0].innerHTML, /速率 2.00 KB \/ 秒 \/ 阈值 1.00 KB \/ 秒/);
  api.ind.mode = 'linked'; api.ind.alerts = [];

  const old = api.refreshIndependent(), latest = api.refreshIndependent();
  const olderRequest = requests.shift(), newerRequest = requests.shift();
  newerRequest.resolve(snapshot('new')); await latest;
  olderRequest.resolve(snapshot('old')); await old;
  assert.equal(api.ind.targets[0].path, 'new', 'a late snapshot cannot undo recent edits');
  api.editIndependent(api.ind.targets[0]);
  assert.equal(element('ind-path').value, 'new');
  assert.equal(element('ind-enabled').checked, false);

  const save = api.mutateIndependent('independent_upsert', { id: 1, path: 'new', rules: [], enabled: true });
  assert.equal(api.mon.running, false);
  assert.equal(element('menu-scan').disabled, false, 'independent changes must not disable scans');
  assert.equal(requests[0].command, 'independent_upsert');
  requests.shift().resolve(1); await turn(); requests.shift().resolve(snapshot('saved'));
  assert.equal(await save, true);
  const fail = api.mutateIndependent('independent_remove', { id: 1 });
  requests.shift().reject(new Error('save failed')); assert.equal(await fail, false);
  assert.equal(api.ind.targets[0].path, 'saved', 'failed persistence must preserve the visible definition');

  api.state.focus = 5; element('tree-path').value = 'C:\\unknown';
  const missing = api.goTreePath(); requests.shift().resolve(null); await missing;
  assert.equal(navigations.length, 0, 'unknown paths must never navigate to a fallback ancestor');
  const valid = api.goTreePath(); requests.shift().resolve(12); await valid;
  assert.deepEqual(navigations, [12]);

  const list = api.renderTopFiles(); requests.shift().resolve([{ node: 33, name: 'large.bin', path: 'C:\\sub\\large.bin', size: 20 }]); await list;
  const double = element('topfiles').children[0].ondblclick();
  assert.equal(requests[0].command, 'node_parent', 'double click resolves parent identity without opening/executing a file');
  assert.equal(requests[0].args.node, 33);
  requests.shift().resolve(22); await double;
  assert.deepEqual(navigations, [12, 22]);
  console.log('independent: local timestamps, stale snapshots, CRUD errors, scan isolation, exact paths and file-parent navigation passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
