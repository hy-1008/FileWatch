'use strict';

// Pure memory checks: no scanning, fixture creation or file deletion.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.join(__dirname, '../src');
const paints = [];
const animations = [];
let dimensionWrites = 0;
const wrap = { clientWidth: 640, clientHeight: 400 };
const context = (name) => new Proxy({}, {
  get(target, key) {
    if (key in target) return target[key];
    return (...args) => paints.push({ name, op: key, args });
  },
  set(target, key, value) { target[key] = value; return true; },
});
function canvas(name) {
  const ctx = context(name);
  let width = 300, height = 150;
  return {
    style: { opacity: '0' }, parentElement: wrap,
    get width() { return width; }, set width(value) { dimensionWrites++; width = value; },
    get height() { return height; }, set height(value) { dimensionWrites++; height = value; },
    getContext: () => ctx, addEventListener() {},
    animate(frames, options) {
      const animation = { cancel() { animation.cancelled = true; } };
      animations.push({ frames, options, animation }); return animation;
    },
  };
}
const main = canvas('main'), previous = canvas('previous');
const media = { matches: false };
const names = new Map([[5, 'root'], [8, 'stable-name.txt']]);
const elements = { canvas: main, 'canvas-previous': previous, tip: { classList: { add() {} } } };
const renderScope = {
  document: { getElementById: (id) => elements[id], createElement: () => canvas('scratch'), documentElement: {} },
  window: { devicePixelRatio: 1, matchMedia: () => media, addEventListener() {} },
  getComputedStyle: (el) => ({ opacity: el.style?.opacity || '0', getPropertyValue: () => '#101010' }),
  fwColor: () => '#888888', fwHeat: () => '#ff0000',
};
vm.createContext(renderScope);
vm.runInContext(fs.readFileSync(path.join(root, 'treemap.js'), 'utf8'), renderScope);
const renderer = vm.runInContext('Treemap', renderScope);
const layout = {
  count: 2, node: (i) => [5, 8][i], rect: () => [0, 0, 300, 200], header: () => [0, 0, 300, 16],
  extId: () => 0, shadeT: () => .4, slot: () => 0, size: () => 100,
};
renderer.init({ nameOf: (id) => names.get(id), pathOf: () => '', sizeText: String });
renderer.setLayout(layout, { w: 640, h: 400 });
const beforeMeasure = dimensionWrites;
renderer.size(); renderer.size();
assert.equal(dimensionWrites, beforeMeasure, 'measuring the next layout must not clear the visible canvas');
wrap.clientWidth = 720;
renderer.size();
assert.equal(main.width, 640, 'old frame must stay intact while resized layout is loading');
names.clear(); paints.length = 0;
renderer.redraw();
assert(paints.some((p) => p.op === 'fillText' && p.args[0] === 'stable-name.txt'), 'cache invalidation must not strip labels from the old visible frame');
names.set(5, 'root'); names.set(8, 'new-name.txt');
renderer.setLayout(layout, { w: 720, h: 400 });
assert.equal(main.width, 720);
assert.equal(animations.length, 1);
assert.equal(animations[0].options.duration, 160);
media.matches = true;
renderer.setLayout(layout, { w: 720, h: 400 });
assert.equal(animations.length, 1, 'reduced motion must disable fades');
assert.equal(animations[0].animation.cancelled, true);

// Exercise the real async controller with delayed layout/name IPC.
const calls = [], commits = [];
const candidate = { token: 'candidate', count: 2 };
const appScope = {
  console, setTimeout, clearTimeout,
  window: { __TAURI__: { core: { invoke: (command, args) => new Promise((resolve) => calls.push({ command, args, resolve })) }, event: { listen() {} } } },
  document: { getElementById: () => ({ textContent: '' }) },
  Treemap: { size: () => ({ w: 640, h: 400 }), labelNodes: () => [8], setLayout: (l) => commits.push(l) },
  Layout: class { constructor() { return candidate; } },
};
vm.createContext(appScope);
const appSource = fs.readFileSync(path.join(root, 'app.js'), 'utf8');
vm.runInContext(appSource.slice(0, appSource.lastIndexOf('\nboot().catch')), appScope);
const app = vm.runInContext('({ state, refreshLayout })', appScope);
const turn = () => new Promise(setImmediate);
function resolveLayout() { calls.shift().resolve(new ArrayBuffer(16)); }
function resolveInfo() { calls.shift().resolve({ emitted: 2, collapsed_small: 0, collapsed_depth: 0, max_depth_reached: 1 }); }

(async () => {
  app.state.focus = 5;
  const first = app.refreshLayout();
  resolveLayout(); await turn();
  assert.equal(calls[0].command, 'node_names');
  assert.equal(commits.length, 0, 'layout must not be presented before its labels arrive');
  calls.shift().resolve(['complete-name.txt']); await turn();
  assert.equal(commits.length, 1);
  assert.equal(app.state.names.get(8), 'complete-name.txt');
  resolveInfo(); await first;

  app.state.names.clear();
  const old = app.refreshLayout(); resolveLayout(); await turn();
  const oldNames = calls.shift();
  const latest = app.refreshLayout(); resolveLayout(); await turn();
  const newNames = calls.shift();
  newNames.resolve(['latest-name.txt']); await turn();
  resolveInfo(); await latest;
  oldNames.resolve(['obsolete-name.txt']); await old;
  assert.equal(commits.length, 2, 'superseded requests must never replace the current frame');
  assert.equal(app.state.names.get(8), 'latest-name.txt', 'late name results must not overwrite the new frame cache');
  console.log('rendering: retained frames, complete labels, stale IPC, resize and reduced-motion checks passed');
})().catch((error) => { console.error(error); process.exitCode = 1; });
