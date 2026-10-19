/* 固定路径监控的管理界面；不改变面积图会话，也不阻止扫描。 */
const ind = { mode: 'linked', targets: [], rows: [], alerts: [], error: null, editing: null, request: 0, busy: false };
function eventLocalTime(row) {
  const epoch = Number(row.time_ms || ((mon.info?.started_ms || 0) + Number(row.at_ms || 0)));
  if (!epoch) return '时间未知';
  const d = new Date(epoch), p = (n, w = 2) => String(n).padStart(w, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}
async function refreshIndependent() {
  const request = ++ind.request;
  const data = await call('independent_snapshot');
  if (request !== ind.request) return;
  Object.assign(ind, { targets: data.targets || [], rows: data.rows || [], alerts: data.alerts || [], error: data.error });
  renderIndependent();
  if (ind.mode === 'independent') renderMonList();
}
function editIndependent(target) {
  ind.editing = target ? target.id : null;
  document.getElementById('ind-editor-title').textContent = target ? '修改监控' : '新增监控';
  document.getElementById('ind-path').value = target?.path || '';
  document.getElementById('ind-rules').value = (target?.rules || []).join('\n');
  document.getElementById('ind-enabled').checked = target ? target.enabled : true;
  document.getElementById('ind-editor').classList.remove('hidden');
}
function renderIndependent() {
  const box = document.getElementById('ind-targets');
  box.innerHTML = '';
  document.getElementById('ind-count').textContent = `${ind.targets.length} / 32 项`;
  const error = document.getElementById('ind-error');
  error.textContent = ind.error || ''; error.classList.toggle('hidden', !ind.error);
  const names = { starting: '建立索引中', running: '监控中', paused: '已暂停', waiting: '等待恢复', error: '异常' };
  for (const target of ind.targets) {
    const card = document.createElement('div'); card.className = 'ind-target';
    const s = target.status;
    card.innerHTML = `<div class="ind-title">${esc(target.is_dir ? '文件夹' : '文件')} · ${esc(names[s.state] || s.state)}</div>` +
      `<div class="ind-path" title="${esc(target.path)}">${esc(target.path)}</div>` +
      `<div class="hint">当前 ${humanSize(s.size)} · 基线 ${humanSize(s.baseline)} · 变更 ${s.events}</div>` +
      `<div class="hint">${esc(target.rules.join(' 或 ') || '仅记录变更')}</div>` +
      (s.error ? `<div class="mon-err">${esc(s.error)}</div>` : '');
    const actions = document.createElement('div'); actions.className = 'row';
    for (const [label, action] of [
      ['修改', () => editIndependent(target)],
      [target.enabled ? '暂停' : '启用', () => mutateIndependent('independent_upsert', { id: target.id, path: target.path, rules: target.rules, enabled: !target.enabled })],
      ['移除', () => mutateIndependent('independent_remove', { id: target.id })],
    ]) {
      const button = document.createElement('button'); button.type = 'button'; button.className = 'mini';
      button.textContent = label; button.disabled = ind.busy; button.onclick = action;
      if (label === '移除') button.title = '只移除监控配置，不删除文件';
      actions.appendChild(button);
    }
    card.appendChild(actions); box.appendChild(card);
    ContextMenu.bind(card, { kind: 'target', id: target.id });
  }
  if (!ind.targets.length) box.innerHTML = '<p class="hint">尚无独立监控。添加文件或文件夹即可开始，无需先扫描盘符。</p>';
}
async function mutateIndependent(command, args) {
  if (ind.busy) return false;
  ContextMenu.close();
  ind.busy = true; renderIndependent();
  document.getElementById('ind-save').disabled = true;
  try { await call(command, args); await refreshIndependent(); return true; }
  catch (error) { toast(String(error), true); return false; }
  finally { ind.busy = false; renderIndependent(); document.getElementById('ind-save').disabled = false; }
}
async function switchIndependentMode(mode) {
    ind.mode = mode;
    document.getElementById('linked-monitor').classList.toggle('hidden', ind.mode !== 'linked');
    document.getElementById('independent-pane').classList.toggle('hidden', ind.mode !== 'independent');
    renderMonList();
    if (ind.mode === 'independent') await refreshIndependent();
}
function wireIndependent() {
  document.getElementById('monitor-mode').onchange = event => switchIndependentMode(event.target.value).catch(e => toast(String(e), true));
  document.getElementById('ind-new').onclick = () => editIndependent(null);
  document.getElementById('ind-cancel').onclick = () => document.getElementById('ind-editor').classList.add('hidden');
  // 标题跟着用途走：选择器弹出的窗口上写着"选择要监控的文件"才对得上这里在干什么
  for (const [id, kind, title] of [
    ['ind-file', 'file', '选择要监控的文件'],
    ['ind-folder', 'folder', '选择要监控的文件夹'],
  ]) {
    document.getElementById(id).onclick = async () => {
      try {
        const path = await call('choose_path', { kind, title });
        if (path) document.getElementById('ind-path').value = path;
      } catch (e) { toast(String(e), true); }
    };
  }
  document.getElementById('ind-save').onclick = async () => {
    const saved = await mutateIndependent('independent_upsert', { id: ind.editing,
      path: document.getElementById('ind-path').value.trim(),
      rules: document.getElementById('ind-rules').value.split(/\r?\n/).map(s => s.trim()).filter(Boolean),
      enabled: document.getElementById('ind-enabled').checked });
    if (saved) document.getElementById('ind-editor').classList.add('hidden');
  };
  for (const button of document.querySelectorAll('.rule-help')) button.onclick = (e) => {
    e.preventDefault(); document.getElementById('rule-help-dialog').showModal();
  };
  let timer;
  listen('independent:changed', () => {
    if (!timer) timer = setTimeout(() => { timer = null; refreshIndependent().catch(() => {}); }, 200);
  }).catch(() => {});
  refreshIndependent().catch(e => toast(String(e), true));
}
