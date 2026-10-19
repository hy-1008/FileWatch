/* 外观偏好仅保存在当前 WebView 的本地存储，不接触监控目标和磁盘文件。
 * 放在 head 中同步执行：先设置根元素，再加载样式，避免重启时闪一下错误主题。
 */
'use strict';

(() => {
  const key = 'filewatch.appearance.v1';
  const defaults = { theme: 'system', density: 'comfortable', layout: 'full' };
  const choices = {
    theme: ['system', 'light', 'dark'],
    density: ['comfortable', 'compact'],
    layout: ['full', 'focus'],
  };
  const root = document.documentElement;
  const systemTheme = window.matchMedia('(prefers-color-scheme: dark)');

  function readPrefs() {
    let saved = {};
    try { saved = JSON.parse(localStorage.getItem(key) || '{}') || {}; }
    catch (_) { /* 存储不可用或内容损坏时仍能正常启动。 */ }
    const prefs = { ...defaults };
    for (const name of Object.keys(choices)) {
      if (choices[name].includes(saved[name])) prefs[name] = saved[name];
    }
    return prefs;
  }

  let prefs = readPrefs();

  function apply() {
    root.dataset.theme = prefs.theme === 'system'
      ? (systemTheme.matches ? 'dark' : 'light') : prefs.theme;
    root.dataset.density = prefs.density;
    root.dataset.layout = prefs.layout;
    root.style.colorScheme = root.dataset.theme;
    for (const name of Object.keys(choices)) {
      const select = document.getElementById(name + '-select');
      if (select) select.value = prefs[name];
    }
    // Canvas 不受 CSS 主题自动驱动，需要显式重画；布局尺寸由应用观察器处理。
    window.dispatchEvent(new CustomEvent('filewatch:appearance', {
      detail: { ...prefs, resolvedTheme: root.dataset.theme },
    }));
  }

  apply();

  document.addEventListener('DOMContentLoaded', () => {
    apply();
    for (const name of Object.keys(choices)) {
      const select = document.getElementById(name + '-select');
      if (!select) continue;
      select.addEventListener('change', () => {
        if (!choices[name].includes(select.value)) return;
        prefs[name] = select.value;
        try { localStorage.setItem(key, JSON.stringify(prefs)); }
        catch (_) { /* 隐私策略禁用存储时，当前窗口的外观切换仍然有效。 */ }
        apply();
      });
    }
  }, { once: true });

  systemTheme.addEventListener('change', () => {
    if (prefs.theme === 'system') apply();
  });
  window.addEventListener('storage', (event) => {
    if (event.key === key || event.key === null) {
      prefs = readPrefs();
      apply();
    }
  });
})();
