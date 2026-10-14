/* 校验 `src/admin-rule.js` 的权限弹窗触发规则。
 *
 * 为什么要单独一个脚本：界面点击没法自动化，而这段规则的三条分支里有两条
 * **只在特定机器上才走得到**——
 *   · `attempted`  只在「提权重启之后仍未提权」时出现（典型是系统关了 UAC）；
 *   · `privileged` 为真但令牌未提权，也只在 UAC 关闭的机器上出现。
 * 换句话说，真值表里大部分格子在这台开发机上永远不会被点击覆盖到。
 * 与其等用户在那类机器上遇到「每次启动都追问」或「无限重启」，不如在这里
 * 把 3×2 的组合全跑一遍。
 *
 * 用法：node apps/gui/tools/check-admin.js
 */

'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const RULE_JS = path.join(__dirname, '..', 'src', 'admin-rule.js');
const COMMANDS_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'commands.rs');
const INDEX_HTML = path.join(__dirname, '..', 'src', 'index.html');
const APP_JS = path.join(__dirname, '..', 'src', 'app.js');

/** 从 `commands.rs` 里抠出 `ElevationInfo` 的字段名。
 *
 * 这样"Rust 加了字段、前端夹具没跟着改"就成了一条会失败的断言，而不是
 * 到运行时才发现界面上的某个值一直是 `undefined`。 */
function rustInfoFields() {
  const src = fs.readFileSync(COMMANDS_RS, 'utf8');
  const start = src.indexOf('pub struct ElevationInfo {');
  if (start < 0) return null;
  const body = src.slice(start, src.indexOf('\n}', start));
  const names = [];
  for (const line of body.split('\n')) {
    const m = line.match(/^\s*pub (\w+):/);
    if (m) names.push(m[1]);
  }
  return names;
}

/** `admin-rule.js` 不是 CommonJS 也不是 ESM，用 vm 取它的顶层词法声明 —— 
 *  顶层 `function` 会挂到上下文对象上，但为保险起见统一在上下文里求值一次。 */
function loadRule() {
  const src = fs.readFileSync(RULE_JS, 'utf8');
  const sandbox = {};
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox, { filename: 'admin-rule.js' });
  return vm.runInContext(
    '({ fwAdminBootAction, fwAdminButtonClass, fwAdminButtonText })',
    sandbox,
    { filename: 'admin-rule-exports' });
}

let pass = 0;
const fails = [];
function ok(cond, what) {
  if (cond) { pass++; } else { fails.push(what); }
}
function eq(a, b, what) {
  ok(a === b, `${what}：得到 ${JSON.stringify(a)}，期望 ${JSON.stringify(b)}`);
}

/** 造一个 `elevation_info` 的返回。字段与 Rust 侧 `ElevationInfo` 一一对应
 *  —— 多一个少一个都说明两边脱钩了，所以这里刻意写全。 */
function info(over) {
  return Object.assign({
    privileged: false,
    token_elevated: false,
    volume_readable: false,
    can_request: true,
    kind: 'filtered',
    label: '降级模式',
    hint: '……',
    action_hint: '……',
    account: 'PC\\me',
    exe: 'C:\\x\\filewatch-gui.exe',
    command_line: 'filewatch-gui.exe --elevation-attempted',
    attempted: false,
  }, over || {});
}

/** Rust 侧 `ElevationInfo` 的字段名（与上面的夹具对照）。
 *  `src-tauri/src/commands.rs` 里加了字段却忘了在夹具里补，往往就是
 *  "界面拿到的字段是 undefined" 这类问题的起点 —— 所以拿它当一次对账。 */
const INFO_FIELDS = [
  'privileged', 'token_elevated', 'volume_readable', 'can_request', 'kind',
  'label', 'hint', 'action_hint', 'account', 'exe', 'command_line', 'attempted',
];

function main() {
  const { fwAdminBootAction, fwAdminButtonClass, fwAdminButtonText } = loadRule();
  const prefsAsk = { ask_admin_on_start: true };
  const prefsSilent = { ask_admin_on_start: false };

  /* ── 真值表：3 种信息状态 × 2 种偏好 ─────────────────────────────── */
  // ① 没权限、没试过、用户要问 → 弹
  eq(fwAdminBootAction(info(), prefsAsk), 'ask', '降级 + 首次 + 要问 → 弹窗');
  // ② 没权限、没试过、用户关了自动问 → 不弹
  eq(fwAdminBootAction(info(), prefsSilent), 'none', '降级 + 关了自动问 → 不弹');
  // ③ 已有权限（提权令牌）→ 不弹，哪怕用户要求问
  eq(fwAdminBootAction(info({ privileged: true, kind: 'admin', label: '管理员' }), prefsAsk),
     'none', '已是管理员 → 不弹');
  // ④ UAC 关掉的机器：令牌未提权但卷能读 → privileged 为真 → 不弹
  eq(fwAdminBootAction(info({ privileged: true, token_elevated: false }), prefsAsk),
     'none', 'UAC 关闭（卷可直读）→ 不弹');
  // ⑤ 本次启动已经试过提权仍未生效 → 只提示，不弹（防无限重启）
  eq(fwAdminBootAction(info({ attempted: true }), prefsAsk), 'notify-failed',
     '试过提权仍失败 + 要问 → 只提示不弹');
  // ⑥ 同上但用户关了自动问 → 也不提示（他已经明确表示不想被打扰）
  eq(fwAdminBootAction(info({ attempted: true }), prefsSilent), 'notify-failed',
     '试过提权仍失败 → 仍提示一次（这是一条错误信息，不是询问）');

  /* ── 边界：拿不到信息 ─────────────────────────────────────────────── */
  eq(fwAdminBootAction(null, prefsAsk), 'none', 'info 为 null 时不弹');
  eq(fwAdminBootAction(info(), null), 'none', 'prefs 为 null 时不弹');
  eq(fwAdminBootAction(null, null), 'none', '两者都为 null 时不弹');
  // prefs 缺字段（老版本配置文件）→ 按"不问"处理，不做假设
  eq(fwAdminBootAction(info(), {}), 'none', 'prefs 缺字段 → 不弹');

  /* ── 按钮外观：颜色是唯一的"你在慢速通道"提示 ───────────────────── */
  eq(fwAdminButtonClass(info({ privileged: true })), 'mini perm ok', '有权限 → 绿色');
  eq(fwAdminButtonClass(info()), 'mini perm warn', '降级 → 橙色');
  eq(fwAdminButtonClass(null), 'mini perm warn', '未知 → 按降级显示（宁可提醒）');
  ok(/\bwarn\b/.test(fwAdminButtonClass(info({ attempted: true }))),
     '提权失败后按钮必须仍是橙色 —— 否则用户会以为提权成功了');

  eq(fwAdminButtonText(info({ label: '管理员', privileged: true })), '权限：管理员', '按钮文本');
  eq(fwAdminButtonText(info()), '权限：降级模式', '按钮文本（降级）');
  eq(fwAdminButtonText(null), '权限：—', '按钮文本（未知）');
  // label 缺失时不能渲染出 "权限：undefined"
  eq(fwAdminButtonText({}), '权限：未知', 'label 缺失时兜底');

  /* ── 与 Rust 侧对账 ─────────────────────────────────────────────── */
  const rust = rustInfoFields();
  ok(rust !== null, '应在 src-tauri/src/commands.rs 里找到 pub struct ElevationInfo');
  if (rust) {
    eq(
      rust.slice().sort().join(','),
      INFO_FIELDS.slice().sort().join(','),
      'ElevationInfo 的字段集合与前端夹具'
    );
  }

  /* ── DOM 接线 ────────────────────────────────────────────────────
   * 界面点击没法自动化，所以「id 打错 → 按钮点了没反应」这类错只能靠
   * 静态对账挡住：app.js 里 getElementById 的每一个 id，都必须在
   * index.html 里真的存在。 */
  const html = fs.readFileSync(INDEX_HTML, 'utf8');
  const appjs = fs.readFileSync(APP_JS, 'utf8');
  const htmlIds = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]));
  const usedIds = new Set([...appjs.matchAll(/getElementById\('([^']+)'\)/g)].map((m) => m[1]));
  const missing = [...usedIds].filter((id) => !htmlIds.has(id));
  ok(missing.length === 0, `app.js 引用了 index.html 里不存在的 id：${missing.join(', ')}`);

  // 权限窗的按钮与输入必须都在
  for (const id of ['admin-modal', 'admin-btn', 'admin-yes', 'admin-no',
                    'admin-remember', 'admin-facts', 'admin-title', 'admin-wait']) {
    ok(htmlIds.has(id), `index.html 缺少权限窗元素 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}（元素存在但没人管）`);
  }

  // 脚本顺序：admin-rule.js 必须在 app.js 之前，否则 boot() 走到
  // fwAdminBootAction 时那个函数还没定义（ReferenceError，且只在启动时炸一次）
  const iRule = html.indexOf('admin-rule.js');
  const iApp = html.indexOf('app.js');
  ok(iRule >= 0, 'index.html 应引入 admin-rule.js');
  ok(iRule >= 0 && iApp >= 0 && iRule < iApp, 'admin-rule.js 必须在 app.js 之前引入');

  // admin-rule.js 必须保持"纯"：一旦它开始摸 DOM，这个脚本就没法再跑真值表了
  const ruleSrc = fs.readFileSync(RULE_JS, 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')   // 去掉注释，注释里提到 document 不算
    .replace(/\/\/.*$/gm, '');
  ok(!/\bdocument\b/.test(ruleSrc), 'admin-rule.js 不应访问 document（否则无法脱离浏览器验证）');
  ok(!/\bwindow\b/.test(ruleSrc), 'admin-rule.js 不应访问 window');

  /* ── 结果 ────────────────────────────────────────────────────────── */
  console.log('='.repeat(72));
  console.log('权限弹窗触发规则校验（src/admin-rule.js）');
  console.log('='.repeat(72));
  console.log('  6 格真值表 + 4 条边界 + 6 项按钮外观 + 与 Rust 侧字段对账 + DOM 接线');
  if (fails.length) {
    console.log(`\n  ❌ 失败 ${fails.length} 项：`);
    for (const f of fails) console.log('     · ' + f);
    process.exit(1);
  }
  console.log(`  ✅ 通过 ${pass} 项断言`);
}

main();
