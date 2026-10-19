/* 纯内存外观检查：不扫描、不创建、不修改、不删除任何磁盘文件。
 * node apps/gui/tools/check-appearance.js
 */
'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');
const assert = require('assert/strict');
const source = fs.readFileSync(path.join(__dirname, '..', 'src', 'theme.js'), 'utf8');

function boot(saved, locked = false) {
  const elements = {};
  const ready = {};
  const events = {};
  const media = { matches: false, addEventListener: (name, fn) => { media[name] = fn; } };
  const root = { dataset: {}, style: {} };
  const storage = {
    saved,
    getItem: () => {
      if (locked) throw Error('Storage disabled');
      return storage.saved;
    },
    setItem: (key, value) => {
      if (locked) throw Error('Storage disabled');
      assert.equal(key, 'filewatch.appearance.v1');
      storage.saved = value;
    },
  };
  for (const name of ['theme', 'density', 'layout']) {
    const id = name + '-select';
    elements[id] = { value: '', addEventListener: (event, fn) => { elements[id][event] = fn; } };
  }
  const sandbox = {
    document: {
      documentElement: root,
      getElementById: (id) => elements[id],
      addEventListener: (name, fn) => { ready[name] = fn; },
    },
    window: {
      matchMedia: () => media,
      dispatchEvent: () => {},
      addEventListener: (name, fn) => { events[name] = fn; },
    },
    localStorage: storage,
    CustomEvent: class {},
  };
  vm.runInNewContext(source, sandbox, { filename: 'theme.js' });
  // 首帧前就已应用主题，不依赖后端初始化成功。
  assert.ok(['light', 'dark'].includes(root.dataset.theme));
  ready.DOMContentLoaded();
  const select = (name, value) => {
    elements[name + '-select'].value = value;
    elements[name + '-select'].change();
  };
  return { root, elements, media, storage, events, select };
}

const fresh = boot(null);
assert.equal(fresh.root.dataset.theme, 'light');
fresh.media.matches = true;
fresh.media.change();
assert.equal(fresh.root.dataset.theme, 'dark', '系统主题变化应实时生效');

fresh.select('theme', 'light');
fresh.media.matches = true;
fresh.media.change();
assert.equal(fresh.root.dataset.theme, 'light', '手动主题不应被系统覆盖');
fresh.select('density', 'compact');
fresh.select('layout', 'focus');
const restored = boot(fresh.storage.saved);
assert.deepEqual({ ...restored.root.dataset }, { theme: 'light', density: 'compact', layout: 'focus' });
assert.equal(restored.elements['density-select'].value, 'compact');
assert.equal(restored.elements['layout-select'].value, 'focus');

for (const value of ['{broken', 'null', '42', '"text"', '{"theme":"bad","density":false,"layout":{}}']) {
  assert.deepEqual({ ...boot(value).root.dataset }, { theme: 'light', density: 'comfortable', layout: 'full' });
}
const unavailable = boot(null, true);
unavailable.select('theme', 'dark');
assert.equal(unavailable.root.dataset.theme, 'dark', '存储不可用时当前窗口仍应可切主题');

restored.storage.saved = '{"theme":"dark","layout":"full"}';
restored.events.storage({ key: 'filewatch.appearance.v1' });
assert.equal(restored.root.dataset.theme, 'dark', '其他窗口更新偏好后应同步');
restored.storage.saved = null;
restored.events.storage({ key: null });
assert.equal(restored.elements['theme-select'].value, 'system', '清空存储后回到默认偏好');

console.log('Appearance checks passed: system theme, manual override, persistence, invalid values, storage failures and cross-window synchronization.');
