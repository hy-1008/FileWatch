'use strict';

// No filesystem writes/deletes: run the real controller in a VM with deferred IPC.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const requests = [];
const elements = new Map();
const classes = () => {
  const active = new Set();
  return { toggle(name, value) { if (value) active.add(name); else active.delete(name); }, contains: (name) => active.has(name) };
};
const element = (id) => {
  if (!elements.has(id)) elements.set(id, {
    textContent: '', innerHTML: '', className: '', title: '', disabled: false,
    classList: classes(), attributes: {},
    setAttribute(name, value) { this.attributes[name] = value; }, appendChild() {}, focus() {},
  });
  return elements.get(id);
};
const sandbox = {
  console, setTimeout, clearTimeout,
  window: { __TAURI__: { core: { invoke: (command) => new Promise((resolve) => requests.push({ command, resolve })) }, event: { listen() {} } } },
  document: { body: { classList: classes() }, getElementById: element, createElement: () => element(Symbol()) },
};
vm.createContext(sandbox);
vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/context-menu.js'), 'utf8'), sandbox);
vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/independent.js'), 'utf8'), sandbox);
const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
vm.runInContext(source.slice(0, source.lastIndexOf('\nboot().catch')), sandbox);
const api = vm.runInContext('({ mon, mergeMonitorRows, backfillMonitor, onMonitorTick, monShow, chooseFolder, setBadge })', sandbox);
const seqs = (rows) => Array.from(rows, (r) => r.seq);
const row = (seq, kind = 'created') => ({ seq, kind, path: 'C:\\test\\' + seq, size_text: '1 B', at_ms: 0 });

assert.deepEqual(seqs(api.mergeMonitorRows([row(5)], [row(3), row(6), row(5)], 10)), [6, 5, 3]);
assert.deepEqual(seqs(api.mergeMonitorRows([row(5)], [row(6, 'deleted'), row(7)], 1, 'created')), [7]);

function resolveBatch(batch, rows, snap = { running: true }) {
  for (const req of batch) req.resolve(req.command === 'monitor_snapshot' ? snap : req.command === 'monitor_changes' ? { rows } : []);
}

(async () => {
  const first = api.backfillMonitor();
  const batch1 = requests.splice(0);
  api.mon.filter = 'deleted';
  const second = api.backfillMonitor();
  const batch2 = requests.splice(0);
  resolveBatch(batch2, [row(8, 'deleted')]);
  await second;
  resolveBatch(batch1, [row(7)]);
  await first;
  assert.deepEqual(seqs(api.mon.rows), [8], 'old filter result must not replace the current filter');

  api.mon.filter = '';
  const pending = api.backfillMonitor();
  const batch3 = requests.splice(0);
  api.onMonitorTick({ rows: [row(12), row(11)], total_events: 12, elapsed_ms: 500, backpressure_total: 2, queue_dropped: 4 });
  resolveBatch(batch3, [row(10)], { running: false });
  await pending;
  assert.deepEqual(seqs(api.mon.rows), [12, 11, 10, 8], 'live rows arriving during IPC must survive backfill');
  assert.equal(api.mon.running, true, 'stale snapshot must not overwrite the live running state');
  assert.equal(api.mon.snap.queue_dropped, 4);
  api.onMonitorTick({ rows: [row(9)], total_events: 13, elapsed_ms: 1000 });
  assert.deepEqual(seqs(api.mon.rows), [12, 11, 10, 9, 8], 'late batches must be retained in descending order');
  api.onMonitorTick({ rows: [row(7)], total_events: 7, elapsed_ms: 100 });
  assert.equal(api.mon.snap.total_events, 13, 'an older tick must not roll totals backwards');
  assert.equal(api.mon.snap.elapsed_ms, 1000);
  api.monShow(true);
  resolveBatch(requests.splice(0), []);
  await new Promise(setImmediate);
  assert.equal(element('tree-pane').classList.contains('hidden'), true);
  assert.equal(element('monitor').classList.contains('hidden'), false);
  assert.equal(element('analysis-types').classList.contains('hidden'), true);
  assert.equal(element('monitor-changes').classList.contains('hidden'), false);
  assert.equal(element('monitor-alerts').classList.contains('hidden'), false);
  api.monShow(false);
  assert.equal(element('analysis-top').classList.contains('hidden'), false);
  assert.equal(element('monitor-changes').classList.contains('hidden'), true);
  assert.equal(element('nav-tree').attributes['aria-selected'], 'true');

  // 取消选择器：不扫描（"选完就扫"之后，取消必须是什么都不做，
  // 而不是拿着空路径去扫一次 —— 那会弹一句看不懂的错）
  const cancelled = api.chooseFolder();
  requests.splice(0)[0].resolve(null);
  await cancelled;
  assert.equal(requests.filter(r => r.command === 'scan_start').length, 0,
    'cancelling the picker must not start a scan');
  api.setBadge('MFT 直读', 'mft');
  assert.equal(element('backend-badge').textContent, 'MFT 直读（极速）');
  console.log('monitor controller: ordering, filtering, IPC races, sidebar switching and picker cancellation passed');
})().catch((error) => { console.error(error); process.exitCode = 1; });
