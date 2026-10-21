/* 校验 `src/admin-rule.js` 的权限弹窗触发规则 + `src/view-rule.js` 的视图规则，
 * 外加整个界面的 DOM 接线与版面结构。
 *
 * 为什么要单独一个脚本：界面点击没法自动化，而这几段规则里有相当一部分
 * **只在特定机器/特定窗口尺寸下才走得到**——
 *   · `attempted`  只在「提权重启之后仍未提权」时出现（典型是系统关了 UAC）；
 *   · `privileged` 为真但令牌未提权，也只在 UAC 关闭的机器上出现；
 *   · `fwDriveOfTarget` 的 UNC / 卷 GUID 分支，开发机上根本没有这类卷。
 * 换句话说，大部分格子在这台开发机上永远不会被点击覆盖到。与其等用户遇到
 * 「每次启动都追问」或「点亮了一块不相干的盘」，不如在这里全跑一遍。
 *
 * 用法：node apps/gui/tools/check-admin.js
 */

'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const RULE_JS = path.join(__dirname, '..', 'src', 'admin-rule.js');
const VIEW_RULE_JS = path.join(__dirname, '..', 'src', 'view-rule.js');
const COMMANDS_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'commands.rs');
const INDEX_HTML = path.join(__dirname, '..', 'src', 'index.html');
const STYLE_CSS = path.join(__dirname, '..', 'src', 'style.css');
const APP_JS = path.join(__dirname, '..', 'src', 'app.js');
const TREEMAP_JS = path.join(__dirname, '..', 'src', 'treemap.js');
const PREFS_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'prefs.rs');
const PROFILE_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'profile.rs');
const SCHED_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'scheduler.rs');
const MAIN_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'main.rs');
const TRAY_RS = path.join(__dirname, '..', 'src-tauri', 'src', 'tray.rs');
const CARGO_TOML = path.join(__dirname, '..', 'src-tauri', 'Cargo.toml');
const CAPABILITIES_JSON = path.join(__dirname, '..', 'src-tauri', 'capabilities', 'default.json');

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
function evalIn(file, expr) {
  const src = fs.readFileSync(file, 'utf8');
  const sandbox = {};
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox, { filename: path.basename(file) });
  return vm.runInContext(expr, sandbox, { filename: path.basename(file) + '-exports' });
}

function loadRule() {
  return evalIn(RULE_JS, '({ fwAdminBootAction, fwAdminButtonClass, fwAdminButtonText })');
}

function loadViewRule() {
  return evalIn(VIEW_RULE_JS, '({ fwDriveOfTarget, fwVolumeOfTarget, fwMiddleEllipsis, fwLocalTime })');
}

/** 源码里有没有真的去摸 DOM（注释里提到不算）。
 *
 * 这些 `*-rule.js` 之所以能被这个脚本跑真值表，前提就是它们**纯**。
 * 一旦有人图省事在里面查了一次 `document.getElementById`，这个脚本会
 * 以 `document is not defined` 直接崩 —— 崩了还看得见，比静默失效好，
 * 但不如在这里明说"不行"。 */
function readsDom(file) {
  const src = fs.readFileSync(file, 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/\/\/.*$/gm, '');
  return /\bdocument\b/.test(src) || /\bwindow\b/.test(src);
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
  // 着色下拉框的取值要与 `shading_of()` 的分支对齐，所以要读一眼 Rust 源
  const commandsSrc = fs.readFileSync(COMMANDS_RS, 'utf8');
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

  /* ── 盘符归属：扫描根 → 该点亮哪一张卡片 ────────────────────────────
   * 判错的表现是"看得见但看不出来"：卡片亮着，用户就以为自己在看那块盘。
   * 后三种形态在这台机器上不会出现（没有挂载点卷、没有网络盘），所以
   * 只能在这里钉住。 */
  const { fwDriveOfTarget, fwVolumeOfTarget } = loadViewRule();
  eq(fwDriveOfTarget('C:'), 'C:', '裸盘符 → C:');
  eq(fwDriveOfTarget('c:'), 'C:', '小写盘符统一成大写');
  eq(fwDriveOfTarget('C:\\'), 'C:', '盘根（带尾反斜杠）');
  eq(fwDriveOfTarget('C:\\Users\\HY'), 'C:', '子目录归到所在盘');
  eq(fwDriveOfTarget('D:/projects'), 'D:', '正斜杠形态也要认（shell 传参会换分隔符）');
  eq(fwDriveOfTarget('  E:\\  '), 'E:', '两侧空白先吃掉');
  eq(fwDriveOfTarget('C:foo'), null, '盘符相对路径不是盘根 → 一张都不点亮');
  eq(fwDriveOfTarget('\\\\?\\Volume{9f8a-1c2b}\\'), null, '卷 GUID 路径没有盘符');
  eq(fwDriveOfTarget('\\\\server\\share'), null, 'UNC 共享不是本地卷');
  eq(fwDriveOfTarget('/home/user'), null, 'POSIX 路径不点亮');
  eq(fwDriveOfTarget(''), null, '空串');
  eq(fwDriveOfTarget('   '), null, '只有空白');
  eq(fwDriveOfTarget(null), null, 'null');
  eq(fwDriveOfTarget(undefined), null, 'undefined');
  eq(fwDriveOfTarget(42), null, '数字（后端字段万一改名就是这种输入）');
  eq(fwDriveOfTarget('CC:\\x'), null, '两个字母不是盘符');

  /* ── 扫描根 → 那块盘本身（底部「可用 / 总空间」取值用）────────────────
   * 后端给的 `drive` 有时带尾杠、有时不带，而扫描根是 `C:\Users\…`。
   * 直接比字符串会在某一种组合上静默取不到，状态栏就一直显示 `—`，
   * 看起来像"这功能根本没做"。 */
  const vols = [{ drive: 'C:' }, { drive: 'D:\\' }];
  eq(fwVolumeOfTarget(vols, 'C:\\Users\\HY'), vols[0], '子目录 → 找到它所在的那块盘');
  eq(fwVolumeOfTarget(vols, 'c:'), vols[0], '大小写与尾杠都不影响匹配');
  eq(fwVolumeOfTarget(vols, 'd:\\projects'), vols[1], '盘符带尾杠的卷也能配上');
  eq(fwVolumeOfTarget(vols, 'F:\\x'), null, '列表里没有这块盘 → null');
  eq(fwVolumeOfTarget(vols, '\\\\server\\share'), null, 'UNC 没有盘符 → null');
  eq(fwVolumeOfTarget(vols, null), null, '没有扫描根 → null');
  eq(fwVolumeOfTarget([], 'C:'), null, '卷列表还没拉到 → null');
  eq(fwVolumeOfTarget(null, 'C:'), null, '卷列表为 null 时不炸');

  /* ── 文件名中间省略（列表里"过长隐藏中间、保留扩展名"） ────────────
   * 为什么不是尾部省略：文件名的结尾往往才是区别所在（`- 副本.xlsx` 与
   * `- 副本(1).xlsx` 尾部省略后长得一模一样）。
   * 这里把边界全跑一遍，包括"预算比扩展名还小"这种不能崩的情况。 */
  const { fwMiddleEllipsis, fwLocalTime } = loadViewRule();
  eq(fwMiddleEllipsis('a.xlsx', 20), 'a.xlsx', '放得下就原样返回');
  eq(fwMiddleEllipsis('x'.repeat(50) + '.pdf', 20).endsWith('.pdf'), true,
     '超长时扩展名必须留着');
  eq(fwMiddleEllipsis('x'.repeat(50) + '.pdf', 20).length <= 20, true, '结果不能超过预算');
  const long1 = '2026-10-16 季度汇总 v3 - 副本.xlsx';
  const long2 = '2026-10-16 季度汇总 v3 - 副本(1).xlsx';
  eq(fwMiddleEllipsis(long1, 24) !== fwMiddleEllipsis(long2, 24), true,
     '只差结尾的两个文件名，中间省略后必须还能区分（尾部省略就区分不了）');
  eq(fwMiddleEllipsis('archive.tar.gz', 12).endsWith('.gz'), true, '多点扩展名保留最后一段');
  eq(fwMiddleEllipsis('.gitignore', 8).includes('…'), true, '以点开头的隐藏文件按普通名字处理');
  eq(fwMiddleEllipsis('abc', 0), 'abc', '预算为 0 也按 4 兜底，不能返回空串或抛错');
  eq(fwMiddleEllipsis(null, 10), '', 'null 不炸');
  eq(fwLocalTime(0), '—', '没有时间戳显示占位符，不显示 1970');
  eq(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/.test(fwLocalTime(1_700_000_000)), true,
     '时间格式统一为 YYYY-MM-DD HH:MM（本地时间）');

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
  const appjs = fs.readFileSync(APP_JS, 'utf8') + '\n' + ['independent.js', 'context-menu.js'].map(name => fs.readFileSync(path.join(path.dirname(APP_JS), name), 'utf8')).join('\n');
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

  /* ── 分析视图（F12 重复文件 / F11 快照对比）的 DOM 接线 ─────────────
   * 同样的双向对账。这两个视图尤其需要它：面积图的错误当场就能看见，
   * 而"导出按钮没接线"只会表现成"点了没反应"，很容易被当成没找到重复文件。 */
  const ANALYSIS_IDS = [
    // 视图切换
    'view-tab-map', 'view-tab-dup', 'view-tab-diff', 'view-hint',
    'view-map', 'view-dup', 'view-diff',
    // F12 重复文件
    'dup-min-size', 'dup-top', 'dup-per-group', 'dup-start', 'dup-state',
    'dup-bar', 'dup-summary', 'dup-list', 'dup-csv', 'dup-export',
    // F11 快照对比
    'diff-path', 'diff-order', 'diff-min-delta', 'diff-compare', 'diff-state',
    'diff-save-path', 'diff-save', 'diff-csv', 'diff-export', 'diff-summary', 'diff-list',
    'diff-progress', 'diff-bar', 'diff-stage',
  ];
  for (const id of ANALYSIS_IDS) {
    ok(htmlIds.has(id), `index.html 缺少分析视图元素 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}（元素存在但没人管）`);
  }
  // 三个视图必须初始互斥：至少有一个带 hidden，否则打开窗口时三块内容叠在一起
  const hiddenViews = ['view-map', 'view-dup', 'view-diff']
    .filter((id) => new RegExp(`id="${id}"[^>]*class="[^"]*hidden`).test(html));
  ok(hiddenViews.length === 2,
     `初始应恰有 2 个视图带 hidden（实际 ${hiddenViews.length}：${hiddenViews.join(', ')}）`);

  /* ── F11 的两个观感要求：按键要小、开工要有动静 ──────────────────────
   * 用户实际报的是「浏览按键太大」和「按下后没有进度条/弹窗，分不清是不是
   * 卡住了」。前者是宽度问题（一行三个文字按钮会把整行挤换行），后者是
   * 反馈问题（几秒的活不能没有提示）。两条都写成断言，免得下次改回去。 */
  const buttonById = (id) =>
    new RegExp(`<button id="${id}"[^>]*>([\\s\\S]*?)</button>`).exec(html);
  for (const id of ['diff-pick', 'diff-save-pick', 'diff-csv-pick']) {
    const btn = buttonById(id);
    ok(!!btn, `index.html 缺少 #${id}`);
    if (!btn) continue;
    ok(/class="[^"]*\bpick\b/.test(btn[0]), `#${id} 应当是图标按钮（带 .pick 类）`);
    ok(!/浏览/.test(btn[1]), `#${id} 不应再写"浏览…"文字 —— 三个文字按钮会把这一行挤换行`);
    ok(/<svg/.test(btn[1]), `#${id} 应有一个图标（svg）`);
    ok(/title="/.test(btn[0]), `#${id} 必须带 title：图标按钮没有文字，作用只能靠悬停告诉用户`);
  }
  // 「与当前扫描对比 / 保存当前快照」：短标签 + title 解释
  for (const id of ['diff-compare', 'diff-save']) {
    const btn = buttonById(id);
    ok(!!btn, `index.html 缺少 #${id}`);
    if (!btn) continue;
    const text = btn[1].replace(/<[^>]*>/g, '').trim();
    ok(text.length <= 5, `#${id} 的按钮文字应当短（实际「${text}」）—— 长解释放 title 里`);
    ok(/title="[^"]{8,}"/.test(btn[0]), `#${id} 必须带 title 说明作用（悬停解释）`);
  }
  // 进度行：空闲时必须收起（不白占一行），作业时显示
  const progRow = /id="diff-progress"[^>]*/.exec(html);
  ok(!!progRow && /hidden/.test(progRow[0]), '进度行 #diff-progress 空闲时应带 hidden');
  // 进度条的填充样式必须点名 #diff-bar。选择器只列 #progress-bar / #dup-bar 的话，
  // 新加的条会"宽度在动但看不见"—— 这条真出过，靠无头截图才抓到。
  const fillCss = fs.readFileSync(STYLE_CSS, 'utf8');
  ok(/#diff-bar, #content-bar\s*\{/.test(fillCss) || /#progress-bar,[\s\S]{0,60}?#diff-bar/.test(fillCss),
     'style.css 的进度条填充选择器必须包含 #diff-bar（漏了就是一条隐形的进度条）');

  const analyzeRs = fs.readFileSync(
    path.join(__dirname, '..', 'src-tauri', 'src', 'analyze.rs'), 'utf8');
  for (const [fn, cmd] of [['saveSnapshot', 'snapshot_save'], ['compareSnapshot', 'snapshot_compare']]) {
    const body = new RegExp(`async function ${fn}\\(\\)\\s*\\{([\\s\\S]*?)\\n\\}`).exec(appjs);
    ok(!!body, `app.js 里应有 ${fn}()`);
    if (!body) continue;
    ok(body[1].includes(`'${cmd}'`), `${fn} 应调用 ${cmd}`);
    ok(/diffStart\(/.test(body[1]),
       `${fn} 开工后必须显示进度（diffStart）—— 否则点下去界面毫无动静，用户只能猜是不是卡住了`);
  }
  ok(/setInterval\(diffTick/.test(appjs),
     '"已用 x.x 秒"必须由前端自己跳：进度事件是节流的，长跑里靠它证明还活着');
  // 四条事件前后端对账（少一条的表现就是"进度条走到一半不动了"）
  for (const ev of ['diff:progress', 'diff:done', 'diff:saved', 'diff:error']) {
    ok(analyzeRs.includes(`"${ev}"`), `analyze.rs 应推送 ${ev}`);
    ok(appjs.includes(`'${ev}'`), `app.js 应监听 ${ev}`);
  }
  // 两个命令必须是"开工即返回"：同步做几秒会把窗口堵成"未响应"
  for (const cmd of ['snapshot_save', 'snapshot_compare']) {
    ok(new RegExp(`pub fn ${cmd}\\([\\s\\S]*?\\) -> Result<\\(\\), String>`).test(analyzeRs),
       `${cmd} 必须是开工即返回（Result<(), String>）—— 同步做会把主线程堵死`);
  }
  ok(/std::thread::spawn/.test(analyzeRs), 'F11 的活必须在后台线程里跑');
  // 解析必须有独立进度段：read+parse 合在一个回调里的话，大快照解析的几秒
  // 里条是僵的 —— 用户看到的正是"只有字、条不动"。
  ok(/rep\.stage\("读取快照文件"/.test(analyzeRs) && /rep\.stage\("解析快照"/.test(analyzeRs),
     '对比必须拆出「读取快照文件」与「解析快照」两个阶段（解析没有进度 = 条僵住）');
  ok(/Snapshot::read_to_string_with/.test(analyzeRs) && /Snapshot::from_tsv_with/.test(analyzeRs),
     'compare 必须用 read_to_string_with + from_tsv_with（带解析进度的引擎接口）');
  // 对比行的右键菜单：绑定必须存在，且菜单分支里要有"复制本行变化"
  ok(/ContextMenu\.bind\(row, \{ kind: 'diffrow'/.test(appjs),
     '对比列表的行必须绑右键菜单（diffrow）');
  const ctxjs = fs.readFileSync(path.join(path.dirname(APP_JS), 'context-menu.js'), 'utf8');
  ok(/kind === 'diffrow'/.test(ctxjs) && /copy-diff/.test(ctxjs),
     'context-menu.js 必须有 diffrow 分支（含复制本行变化）');

  /* ── 快照对比列表：列头 + 中间省略 + 时间列 ──────────────────────────
   * 这一版的列表是"一张表"，不是若干行拼起来的字符串：少了列头，用户只能
   * 记住六列的顺序；少了时间列，就只能回资源管理器一个个看。 */
  ok(/className = 'dhead'/.test(appjs), '对比列表必须有列头（dhead）');
  for (const col of ['类型', '变化', '文件名', '修改时间', '创建时间']) {
    ok(appjs.includes(col), `对比列表的列头缺少「${col}」`);
  }
  ok(/fwMiddleEllipsis\(name,/.test(appjs),
     '文件名要用中间省略（尾部省略会让"只差结尾"的文件名看起来一模一样）');
  ok(/fwLocalTime\(r\.mtime\)/.test(appjs) && /fwLocalTime\(r\.ctime\)/.test(appjs),
     '修改时间与创建时间都要显示（0 由 fwLocalTime 变成 —）');
  ok(/pub name: String/.test(analyzeRs) && /pub ctime: i64/.test(analyzeRs),
     'DiffRowDto 必须带 name 与 ctime（前端不猜路径分隔符、创建时间由后端 stat）');
  ok(/fn fill_ctimes/.test(analyzeRs) && /fill_ctimes\(&mut view\.rows\)/.test(analyzeRs),
     '创建时间必须在后台线程里补齐（索引与快照都不存 ctime）');

  /* ── 文件搜索（与面积图/重复文件/快照对比同级的第四个页签） ─────────── */
  for (const id of ['view-tab-search', 'view-search', 'search-text', 'search-mode',
                    'search-scope', 'search-run', 'search-state', 'search-summary', 'search-list']) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}（元素存在但没人管）`);
  }
  ok(/call\('search_nodes'/.test(appjs), '搜索必须调后端 search_nodes（在内存索引里搜）');
  const commandsRs = fs.readFileSync(
    path.join(__dirname, '..', 'src-tauri', 'src', 'commands.rs'), 'utf8');
  ok(/core_index::search\(/.test(commandsRs),
     'search_nodes 必须走引擎的 core_index::search（不能在前端或命令里另写一套匹配）');
  const engineSearchRs = fs.readFileSync(
    path.join(__dirname, '..', '..', '..', 'crates', 'core-index', 'src', 'search.rs'), 'utf8');
  ok(/pub fn parse\(s: &str\) -> MatchMode/.test(engineSearchRs)
     && /struct SimpleRegex/.test(engineSearchRs),
     '三种匹配方式（含简化正则）必须由引擎实现，前端只传字符串');
  ok(/value="regex"/.test(html) && /value="fuzzy"/.test(html),
     '搜索方式下拉框必须提供 模糊 / 正则');
  ok(/value="dirs"/.test(html), '搜索范围必须能选文件夹');
  ok(/view-tab-search/.test(appjs) && /setView\('search'\)/.test(appjs),
     '文件搜索必须能被切到（页签 → setView）');
  ok(/\['search-text', 'search-content'\]/.test(appjs)
     && /\.onkeydown = \(e\) => \{\s*if \(e\.key === 'Enter'\)/.test(appjs),
     '两个搜索框（名称 / 内容）都必须支持回车即搜');

  /* ── 内容搜索（F13 后半段：纯文本类） ──────────────────────────────
   * 它是唯一需要真读文件的搜索，所以必须走"后台线程 + 三条事件"；
   * 进度条还得进填充选择器（漏了就是一条隐形的条，这个坑踩过一次）。 */
  for (const id of ['search-content', 'search-content-regex', 'content-progress',
                    'content-bar', 'content-stage', 'content-list']) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}`);
  }
  const guiSearchRs = fs.readFileSync(
    path.join(__dirname, '..', 'src-tauri', 'src', 'search.rs'), 'utf8');
  for (const ev of ['content:progress', 'content:done', 'content:error']) {
    ok(guiSearchRs.includes(`"${ev}"`), `后端应推送 ${ev}`);
    ok(appjs.includes(`'${ev}'`), `app.js 应监听 ${ev}`);
  }
  ok(/content_search_start/.test(appjs) && /content_search_start/.test(guiSearchRs),
     '内容搜索命令必须前后端对上（开工即返回，结果走事件）');
  ok(/std::thread::spawn/.test(guiSearchRs), '读文件必须在后台线程里跑（同步会冻住窗口）');
  ok(/PROGRESS_MIN_GAP/.test(guiSearchRs), '内容搜索的进度事件必须节流（别把事件队列灌满）');
  ok(/is_text_name/.test(guiSearchRs) && /MAX_FILE_BYTES/.test(guiSearchRs),
     '内容搜索必须按白名单扩展名 + 单文件体积上限筛候选');
  const fillCss2 = fs.readFileSync(STYLE_CSS, 'utf8');
  ok(/#diff-bar, #content-bar\s*\{/.test(fillCss2) || /#diff-bar[\s\S]{0,40}#content-bar\s*\{/.test(fillCss2),
     'style.css 的进度条填充选择器必须包含 #content-bar（漏了就是一条隐形的进度条）');

  /* ── 第 1/2 栏（盘符 · 文件夹树）的 DOM 接线 ───────────────────────
   * 特别注意：`#volumes` 与 `#tree` 的**父栏**是这轮改动的主角。 */
  // （`custom-root` / `scan-root` / `watch` 已随左下角的扫描目录模块删除，
  //   别再把它们加回来 —— 扫描入口现在只有"点盘符"和"文件 → 选择文件夹…"两处。）
  const SHELL_IDS = ['volumes', 'refresh-vols', 'tree', 'tree-hint'];
  for (const id of SHELL_IDS) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}（元素存在但没人管）`);
  }

  /* ── 着色：下拉框的取值必须与后端「认得的字符串」完全一致 ────────────
   * `shading_of()` 对不认识的值**静默**回落到 `ByExtension`。于是前端多写
   * 一个拼错的 option，表现是「选了没反应」——不报错、不崩，只是画出来还是
   * 按类型着色。用户报的「按文件年龄的图也是同一颜色」就有这一层味道，
   * 所以把两侧的取值表对齐成断言。 */
  // 只取 #shading 这一段里的 option，别把「口径」下拉框的取值也算进来
  const shadeSelect = /<select id="shading">([\s\S]*?)<\/select>/.exec(html);
  ok(!!shadeSelect, 'index.html 应有一个 #shading 下拉框');
  const shadeValues = shadeSelect
    ? [...shadeSelect[1].matchAll(/<option value="([^"]+)"/g)].map((m) => m[1])
    : [];
  ok(shadeValues.length >= 3, `#shading 至少该有 3 个取值（实际 ${shadeValues.join(',') || '无'}）`);
  ok(shadeValues.includes('ext') && shadeValues.includes('age'),
     `#shading 必须含 ext 与 age（实际 ${shadeValues.join(',')}）`);
  for (const v of shadeValues) {
    const known = v === 'ext' || commandsSrc.includes(`Some("${v}") => ShadingMode`);
    ok(known,
       `#shading 的取值 "${v}" 后端不认（commands.rs 的 shading_of 里没有对应分支，` +
       `会静默按文件类型着色）`);
  }
  // 图例：id 必须存在且真的被接线（热力图没有图例就只能靠猜哪头是旧）
  ok(htmlIds.has('heat-legend') && htmlIds.has('heat-bar'), 'index.html 缺少热力图图例');
  ok(usedIds.has('heat-legend') && usedIds.has('heat-bar'), 'app.js 没有接线热力图图例');
  // 色带必须由 wire.js 的调色点生成 —— 图例与画面各写一份颜色迟早会分叉
  ok(/FW_HEAT/.test(appjs), '图例的色带应取自 wire.js 的 FW_HEAT，而不是自己写一串颜色');

  /* ── 渲染层不许做 IO ────────────────────────────────────────────────
   * 这条是给「文件夹树列表加载太慢」留的回归守卫。原因不是后端慢
   * （`tree_rows` 最坏 240 µs），而是面积图每个方块各发一次取名字的 IPC，
   * 且每回来一个就整图重画一次 —— 几千个方块就是几千次往返 + O(N²) 遍历。
   * 名字现在由 `primeNames()` 在布局之后**一次**取齐，绘制路径必须保持纯。 */
  const treemapJs = fs.readFileSync(TREEMAP_JS, 'utf8');
  const noComment = (t) => t.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '');
  ok(!/\binvoke\(|call\(/.test(noComment(treemapJs)),
     'treemap.js 不得发 IPC（绘制会被高频调用，在里面做 IO 就是名字风暴）');
  const cachedNameBody = /function cachedName\(node\)\s*\{([\s\S]*?)\n\}/.exec(appjs);
  ok(!!cachedNameBody, 'app.js 里应有 cachedName()');
  if (cachedNameBody) {
    ok(!/await|call\(/.test(cachedNameBody[1]),
       'cachedName 必须只读缓存：它每个方块都会调一次，里面做 IO 就是 N 次 IPC');
  }
  // 预取名字与画字必须用同一套尺寸判据（两边各写一份 → 会出现"要画却没名字"）
  const labelConsts = (treemapJs.match(/LABEL_MIN_[WH]/g) || []).length;
  ok(labelConsts >= 5,
     `LABEL_MIN_W/H 至少该出现 5 次（定义 + 预取 + 绘制；实际 ${labelConsts}）` +
     '—— 少了说明有人把其中一处改回了字面量');

  /* ── 版面结构：盘符列与文件夹树列必须分开 ──────────────────────────
   * 对一次真实 bug 的回归守卫：两者曾经同处一个 aside，两块都是"可被压扁"的
   * flex 项，窗口一矮一起被压，盘符卡片溢出自己的块、盖到树上 —— 用户报的
   * 就是「左侧盘符和文件列表有重叠」。把「不能同栏」写成断言，比下次再靠
   * 肉眼发现便宜得多。 */
  const panels = [...html.matchAll(/<(aside|section) class="panel ([^"]+)">([\s\S]*?)<\/\1>/g)]
    .map((m) => ({ cls: m[2].trim().split(/\s+/), body: m[3] }));
  ok(panels.length >= 4, `index.html 里应至少有 4 个 panel（实际 ${panels.length}）`);
  const find = (name) => panels.filter((p) => p.cls.includes(name));
  eq(find('drives').length, 1, '恰有 1 个 panel drives');
  eq(find('left').length, 1, '恰有 1 个 panel left');
  const pd = find('drives')[0];
  const pl = find('left')[0];
  if (pd && pl) {
    ok(/id="volumes"/.test(pd.body), '盘符栏里应有 #volumes');
    ok(!/id="tree"/.test(pd.body), '盘符栏里**不能**有文件夹树 —— 这正是重叠的根因');
    ok(/id="tree"/.test(pl.body), '文件夹树栏里应有 #tree');
    ok(!/id="volumes"/.test(pl.body), '文件夹树栏里不能有盘符列表');
    // 盘符栏现在只剩一个可滚动的盘符列表：扫描入口是"点卡片"和
    // "文件 → 选择文件夹…"，不再有钉在底部的输入框。
    ok(!/id="custom-root"/.test(pd.body), '盘符栏里不应再有扫描目录输入框');
    for (const id of ['nav-tree', 'nav-monitor', 'tree-pane', 'monitor', 'mon-paths']) {
      ok(pl.body.includes(`id="${id}"`), `导航栏缺少 #${id}`);
    }
  }
  const pr = find('right')[0];
  for (const id of ['analysis-types', 'analysis-top', 'monitor-changes', 'monitor-alerts', 'mon-list', 'mon-alert-list']) {
    ok(!!pr && pr.body.includes(`id="${id}"`), `右栏缺少 #${id}`);
  }
  const interfaceMenu = /id="interface-menu"[\s\S]*?<\/details>/.exec(html)?.[0] || '';
  for (const id of ['theme-select', 'density-select', 'layout-select', 'shading', 'mode', 'detail']) {
    ok(interfaceMenu.includes(`id="${id}"`), `界面菜单缺少 #${id}`);
  }
  const footer = /<footer[\s\S]*?<\/footer>/.exec(html)?.[0] || '';
  ok(footer.includes('id="backend-badge"'), '读取后端状态必须在底部状态栏');
  ok(!html.includes('id="mon-toggle"'), '底部旧监控抽屉应移除');
  // panel 的类名必须在样式表里有对应规则：写错一个字母（panel drive）
  // 的后果是那一栏宽度变成 auto，表面上像"栏没了"，很难看出是拼写问题
  const css = fs.readFileSync(STYLE_CSS, 'utf8');
  for (const p of panels) {
    for (const c of p.cls) {
      ok(css.includes(`.panel.${c}`), `style.css 里没有 .panel.${c} 的规则（类名拼错了？）`);
    }
  }

  // 滚动条必须走自绘（「滚动条」一段）。它非常容易被"好心"改坏：
  //   · 加了标准属性 scrollbar-width / scrollbar-color → Chromium 会整段
  //     忽略 ::-webkit-scrollbar，界面悄悄退回 16px 的系统滚动条，不报错；
  //   · 用 background 简写改滑块颜色 → 连 background-clip 一起重置，滑块变胖。
  // 这里守住三条底线：规则存在、变量存在、互斥的标准属性没被加进来。
  ok(css.includes('::-webkit-scrollbar-thumb'), 'style.css 缺少自绘滚动条规则');
  for (const v of ['--sb-size', '--sb-thumb', '--sb-thumb-hover']) {
    ok(css.includes(v + ':'), `style.css 缺少滚动条变量 ${v}`);
  }
  // 剔除注释再判：说明文字里当然可以**提到**这两个属性，规则里不能**出现**它们
  const cssCode = css.replace(/\/\*[\s\S]*?\*\//g, '');
  ok(!/scrollbar-width|scrollbar-color/.test(cssCode),
     'style.css 不得设置 scrollbar-width/scrollbar-color（会令 ::-webkit-scrollbar 整段失效）');

  /* ── 盘符栏折叠：让出的宽度必须**原样**转给文件夹树 ────────────────────
   * 折叠的实现全在 CSS 里，所以守的是三处最容易静默写坏的地方：
   *   · `body` 上的类名两侧写得不一样（JS 挂 `drives-collapsed`、CSS 里写
   *     `collapse-drives`）→ 点了没反应，且不报任何错；
   *   · 文件夹树的折叠宽度**写死一个数字**、没用 calc 加加减减 → 上面任何
   *     一个栏宽一改就错位，肉眼看不出来；
   *   · 顺手也给中间区/右栏加了折叠规则 → 违背"其余空间列不变"。 */
  const cssFlat = cssCode.replace(/\s+/g, ' ');
  ok(/body\.drives-collapsed \.panel\.drives /.test(cssFlat),
     'style.css 缺少 `body.drives-collapsed .panel.drives` 规则');
  const collapsedLeft = /body\.drives-collapsed \.panel\.left \{([^}]*)\}/.exec(cssFlat);
  ok(!!collapsedLeft, 'style.css 缺少 `body.drives-collapsed .panel.left` 规则');
  if (collapsedLeft) {
    const body = collapsedLeft[1];
    ok(/calc\(/.test(body),
       '文件夹树的折叠宽度要用 calc(...) 吃掉盘符栏让出的宽度，不能另写一个数字');
    for (const v of ['--left-w', '--drives-w', '--drives-w-collapsed']) {
      ok(body.includes(v), `折叠宽度应引用 ${v}（少了它，改一次栏宽就会错位）`);
    }
  }
  ok(!/body\.drives-collapsed \.panel\.(center|right)/.test(cssFlat),
     '折叠不得给中间区/右栏加规则 —— 「其余空间列不变」是这条功能的前提');

  // 类名两侧必须一致：app.js 挂的那个字符串，style.css 里得真的有
  const drivesCls = [...appjs.matchAll(/classList\.(?:toggle|add|remove)\('(drives-[\w-]+)'/g)]
    .map((m) => m[1]);
  ok(drivesCls.length >= 2,
     `app.js 里应有两个 drives-* 类名开关（折叠态 + 过渡开关），实际 ${drivesCls.length}`);
  for (const c of new Set(drivesCls)) {
    ok(css.includes(c), `app.js 挂的类名 \`${c}\` 在 style.css 里没有出现（点了会没反应）`);
  }
  // 折叠必须**真的更窄**（两处写反了就是"折叠反而更宽"）
  const pxVar = (name) => {
    const m = new RegExp(`${name}:\\s*(\\d+)px`).exec(css);
    return m ? Number(m[1]) : NaN;
  };
  const wOpen = pxVar('--drives-w');
  const wMin = pxVar('--drives-w-collapsed');
  ok(Number.isFinite(wOpen) && Number.isFinite(wMin),
     `style.css 应有 --drives-w / --drives-w-collapsed 的 px 定义（取到 ${wOpen} / ${wMin}）`);
  ok(wMin < wOpen, `--drives-w-collapsed (${wMin}px) 必须小于 --drives-w (${wOpen}px)`);

  /* ── 预警区折叠 ────────────────────────────────────────────────────────
   * 与盘符折叠同一套分工（JS 只挂类、高度归 CSS），同一个坑：改一处忘另一处
   * 就是"点了没反应"，不报任何错。多一条只属于这里的坑 —— 顺手给流水块也写
   * 一条折叠规则，两块的高度就变成两处各说一半，改一次必然错位。 */
  ok(htmlIds.has('alerts-toggle') && usedIds.has('alerts-toggle'),
     '#alerts-toggle 存在且必须接线（预警折叠按钮）');
  const alertsRule = /#monitor-alerts\.collapsed \{([^}]*)\}/.exec(cssFlat);
  ok(!!alertsRule, 'style.css 缺少 `#monitor-alerts.collapsed` 规则');
  if (alertsRule) {
    ok(/flex:\s*0 0 auto/.test(alertsRule[1]),
       '折叠态要退出高度分配（flex: 0 0 auto），否则省下的空间不会回到流水');
  }
  ok(/#monitor-alerts\.collapsed \.mon-list \{[^}]*display:\s*none/.test(cssFlat),
     '折叠后必须把列表本身收起来，否则标题栏下面拖着一块空白');
  ok(!/#monitor-changes\.collapsed/.test(cssFlat),
     '不该给流水块写折叠规则 —— 高度是「预警退出分配」让出来的，写两处迟早对不上');
  // 类名两侧一致（app.js 挂的字符串，CSS 里得真的有）
  const collapsedCls = [...appjs.matchAll(/classList\.toggle\('(collapsed)'/g)].map((m) => m[1]);
  ok(collapsedCls.length >= 1, 'app.js 应给预警区挂 `collapsed` 类');
  ok(/#monitor-alerts\.collapsed/.test(css), 'style.css 里应有对应的 `.collapsed` 规则');
  // 盘符那条旋转规则必须点名：写宽了会把预警的箭头一起转过去
  ok(!/body\.drives-collapsed button\.icon/.test(cssFlat),
     '盘符的旋转规则要点名 #drives-toggle，写宽了会误转别的折叠按钮');

  /* ── 双击定位 ──────────────────────────────────────────────────────────
   * 变更流水与阈值预警都要能双击定位到所在目录。判据是「复用右键菜单那份
   * 实现」而不是「能不能打开」—— 后者测不到"两处各写一份、只有一份对"。
   * 更不能自己拼 `explorer /select,`：那是把用户输入拼进一条命令行去执行。 */
  const dblReveals = [...appjs.matchAll(/ondblclick\s*=\s*\(\)\s*=>\s*contextReveal\(/g)];
  ok(dblReveals.length >= 2,
     `流水与预警两处都要能双击定位（实际 ${dblReveals.length} 处）`);
  // 只查**被引号包起来的**字符串字面量：注释里提到这个词不算拼命令
  ok(!/['"`]\s*explorer|\/select,\s*['"`]|['"`]\s*cmd(\.exe)?\s/i.test(appjs),
     'app.js 不得自己拼资源管理器命令（一律走后端的 reveal_path）');

  /* ── 路径选择器 ────────────────────────────────────────────────────────
   * 三条只在这种静默失败里暴露的错：
   *   1. `kind` 拼错 → 运行时弹出来的是另一种框（想要文件夹却给了文件框）；
   *   2. 还在用旧的 `choose_file` / `choose_folder` → 命令已注销，必失败；
   *   3. 新命令没注册 → 同上。 */
  const pickKinds = new Set(
    [...appjs.matchAll(/pickInto\('[\w-]+',\s*'(\w+)'/g)].map((m) => m[1])
  );
  ok(pickKinds.size >= 2, `没能从 app.js 抠出 pickInto 的 kind（实际 ${pickKinds.size} 个）`);
  for (const k of pickKinds) {
    ok(k === 'file' || k === 'folder',
       `pickInto 的 kind \`${k}\` 后端不认（只认 file / folder）`);
  }
  for (const jsFile of [APP_JS, path.join(__dirname, '..', 'src', 'independent.js')]) {
    const src = fs.readFileSync(jsFile, 'utf8');
    ok(!/'(choose_file|choose_folder)'/.test(src),
       `${path.basename(jsFile)} 还在用旧的选择器命令（已合并成 choose_path）`);
  }
  ok(/picker::choose_path/.test(fs.readFileSync(MAIN_RS, 'utf8')),
     'main.rs 必须注册 picker::choose_path（漏注册 = 运行时 command not found）');

  /* ── 偏好字段：app.js 用的键名必须真的是 prefs.rs 里的字段 ──────────────
   * 写错一个大小写（`drivesCollapsed` vs `drives_collapsed`）不会报任何错：
   * 存下去的是后端不认识的键、读出来永远是 `undefined` —— 表现是
   * 「折叠了，重启又展开了」，从现象反查不到源头。 */
  const prefsRs = fs.readFileSync(PREFS_RS, 'utf8');
  const rustPrefs = new Set([...prefsRs.matchAll(/pub (\w+):/g)].map((m) => m[1]));
  ok(rustPrefs.size >= 2, `没能从 prefs.rs 的 GuiPrefs 里抠出字段（实际 ${rustPrefs.size} 个）`);
  const usedPrefs = new Set([
    ...[...appjs.matchAll(/savePrefs\(\{\s*(\w+):/g)].map((m) => m[1]),
    ...[...appjs.matchAll(/\w*[Pp]refs\w*\.(\w+)/g)].map((m) => m[1]),
  ]);
  ok(usedPrefs.size >= 2, `没能从 app.js 里抠出用到的偏好键（实际 ${usedPrefs.size} 个）`);
  for (const k of usedPrefs) {
    ok(rustPrefs.has(k), `app.js 用的偏好键 \`${k}\` 在 prefs.rs 的 GuiPrefs 里不存在（存了也读不出来）`);
  }

  /* ── 运行模式（轻量 / 标准 / 性能）────────────────────────────────────
   * 三个最容易静默写坏的地方：
   *   1. 下拉框的 `value` 与后端 `Profile::parse` 认得的键名不一致 → 选了没反应
   *      （和着色那个 bug 同一类：`parse` 对不认识的值静默回落到标准）；
   *   2. 前端自己抄一份倍率去算行数 → 两边各算一次，"切了模式但没变化"；
   *   3. 命令没注册进 `generate_handler` → 运行时才报 `command not found`。 */
  for (const id of ['profile', 'autostart-toggle', 'st-space']) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}（元素存在但没人管）`);
  }
  // 「可用 / 总空间」属于底部状态栏：它是"这块盘还剩多少"，和 st-size
  // （"扫到的文件加起来多大"）不是一回事，放错地方就会被当成重复信息
  ok(/<footer[\s\S]*?id="st-space"[\s\S]*?<\/footer>/.test(html), '#st-space 应在底部状态栏里');

  const modeSelect = /<select id="profile"[^>]*>([\s\S]*?)<\/select>/.exec(html);
  ok(!!modeSelect, 'index.html 应有一个 #profile 下拉框');
  const modeValues = modeSelect
    ? [...modeSelect[1].matchAll(/<option value="([^"]+)"/g)].map((m) => m[1])
    : [];
  const profileRs = fs.readFileSync(PROFILE_RS, 'utf8');
  // 键名以 `Profile::key()` 为准（`parse` 里还有 "light"/"轻量" 之类的别名，
  // 那些不该出现在下拉框里 —— 写了就等于同时有两个值指向同一个模式）
  const rustProfileKeys = new Set(
    [...profileRs.matchAll(/Profile::\w+\s*=>\s*"([\w-]+)"/g)].map((m) => m[1])
  );
  ok(rustProfileKeys.size >= 3, `没能从 profile.rs 的 key() 里抠出模式键（实际 ${rustProfileKeys.size} 个）`);
  for (const v of modeValues) {
    ok(rustProfileKeys.has(v), `#profile 的取值 "${v}" 后端不认（会静默按标准模式跑）`);
  }
  for (const k of rustProfileKeys) {
    ok(modeValues.includes(k), `后端有模式 "${k}"，但下拉框里没有它（用户选不到）`);
  }
  eq(modeValues[0], 'balanced', '默认（第一项）必须是标准 —— 轻量看得粗、性能跑后台，都不该替用户选');

  // 倍率只能由后端下发：前端自己乘一份，就是"切了模式但什么都没变"的来路
  ok(!/function rowsFor/.test(appjs),
     '列表行数必须由后端按模式缩放（commands::row_limit），前端不再自己乘倍率');
  ok((appjs.match(/profile: state\.profile/g) || []).length >= 3,
     '三个列表命令都要把当前模式传给后端（不传就变成标准模式的行数）');
  ok(/state\.budget \? state\.budget\.lod_scale : 1/.test(appjs),
     'LOD 阈值必须乘后端下发的 lod_scale（否则"轻量模式"对面积图毫无影响）');
  ok(/invoke\('profile_budget'/.test(appjs),
     '切换模式后必须向后端取一次派生参数（不能把倍率写死在前端）');

  /* ── 开机自启动 ───────────────────────────────────────────────────── */
  ok(/invoke\('autostart_status'\)/.test(appjs), '启动时应读一次开机自启的**真实**状态');
  ok(/invoke\('set_autostart'/.test(appjs), '应能切换开机自启');
  const toggleAuto = /async function toggleAutostart\(\)\s*\{([\s\S]*?)\n\}/.exec(appjs);
  ok(!!toggleAuto && /renderAutostart\(info\)/.test(toggleAuto[1]),
     '切换后必须用**后端返回的状态**重画按钮（写注册表可能被系统策略拦下，不能假装成功）');
  // 自启的真相在注册表里，不进偏好文件 —— 存两份就会出现"用户在任务管理器里
  // 关掉了，界面还显示已开启"
  ok(!/savePrefs\(\{\s*[^}]*autostart/.test(appjs),
     '开机自启不该存进偏好文件（真相在注册表，存两份就会显示错的）');

  /* ── 「模式」菜单：开机自启与关闭软件都在这里 ─────────────────────────
   * 它们都是"这个程序该怎么跑"的选择，不是随手点的按钮。塞在顶栏时：
   *   开机自启 —— 要么看不见，要么被当成"立即启动"；
   *   关闭软件 —— 顶栏放不下两个单选，也没地方写清各自的后果。 */
  const modeMenu = /<details class="menu" id="mode-menu">([\s\S]*?)<\/details>/.exec(html);
  ok(!!modeMenu, 'index.html 应有 #mode-menu（模式弹出面板）');
  ok(!!modeMenu && /id="profile"/.test(modeMenu[1]), '运行模式下拉框应在「模式」菜单里');
  ok(!!modeMenu && /<input id="autostart-toggle" type="checkbox">/.test(modeMenu[1]),
     '开机自启必须是**复选框**（不是按钮）：它是个状态，不是一个动作');
  ok(!htmlIds.has('autostart-btn'), '顶栏那个「开机自启」按钮应已移除（它现在在模式菜单里）');
  const closeRadios = [...html.matchAll(/<input type="radio" name="close-behavior" id="([\w-]+)" value="(\w+)"/g)]
    .map((m) => ({ id: m[1], value: m[2] }));
  eq(closeRadios.length, 2, '「关闭软件」应正好两个单选项');
  ok(closeRadios.some((r) => r.value === 'tray') && closeRadios.some((r) => r.value === 'quit'),
     '「关闭软件」的取值必须是 tray / quit（与 prefs.rs 的 CLOSE_* 常量同名）');
  ok(!!modeMenu && /class="menu-label"/.test(modeMenu[1]),
     '两个单选项上面要有「关闭软件」这个分组标题（否则看起来像两个无关选项）');
  ok(/pub const CLOSE_QUIT/.test(prefsRs) && /pub const CLOSE_TRAY/.test(prefsRs),
     'prefs.rs 应一处定义关闭行为的两个取值');
  ok(/pub fn close_to_tray/.test(prefsRs) && /close_behavior != CLOSE_TRAY/.test(prefsRs),
     '未知的关闭行为必须归一成"退出"（宁可退出，也不要藏起来当成幽灵进程）');
  ok(/savePrefs\(\{ close_behavior/.test(appjs),
     '关闭软件的选择要存进偏好（后端关窗口时读的是落盘的那份）');
  ok(/applyCloseBehavior\(startupPrefs\.close_behavior\)/.test(appjs),
     '启动时要回填上次的关闭行为');
  const trayRs = fs.readFileSync(TRAY_RS, 'utf8');
  ok(/prefs::load\(\)\.close_to_tray\(\)/.test(trayRs),
     '关闭行为的判定必须在后端（读落盘偏好），不能依赖前端那一刻还活着');
  ok(/"quit"/.test(trayRs) && /app\.exit\(0\)/.test(trayRs),
     '托盘菜单必须有「退出」出口 —— 少了它，"最小化到托盘"就变成关不掉的幽灵');
  const mainRsText = fs.readFileSync(MAIN_RS, 'utf8');
  ok(/on_window_event/.test(mainRsText) && /tray::handle_close_requested/.test(mainRsText),
     'main.rs 必须拦下窗口关闭请求并交给 tray 判定');
  ok(/if let Err\(e\) = tray::build/.test(mainRsText),
     '托盘建不起来不该让程序起不来（它只是个便利，不是启动条件）');

  /* ── 预警提醒：系统通知 + 托盘闪动 + 托盘菜单里的监控入口 ────────────
   * 监控的意义是"我不盯着时它替我盯"，所以阈值命中必须能穿透"窗口不在前台"
   * 这件事。三件套缺任何一件都会退化成"只有开窗才看得见"。 */
  ok(/tauri-plugin-notification/.test(fs.readFileSync(CARGO_TOML, 'utf8')),
     'GUI 层应引入 tauri-plugin-notification（窗口收起时的系统通知）');
  ok(/"notification:default"/.test(fs.readFileSync(CAPABILITIES_JSON, 'utf8')),
     'capabilities 里必须允许 notification:default（否则通知静默失败）');
  ok(/tauri_plugin_notification::init\(\)/.test(mainRsText),
     'main.rs 必须注册通知插件');
  ok(/pub fn notify_alert/.test(trayRs) && /notification\(\)\.builder\(\)/.test(trayRs),
     'tray.rs 的 notify_alert 必须真的发系统通知');
  ok(/pub fn flash/.test(trayRs) && /set_icon/.test(trayRs),
     '托盘图标必须能闪动（交替 set_icon 告警图/正常图）');
  ok(/fn alarm_variant/.test(trayRs) && /0xE5/.test(trayRs),
     '告警图标应是在原图右上角画红点，而不是换一张完全不同的图');
  ok(/AtomicBool/.test(trayRs) && /flashing/.test(trayRs),
     '闪动要防重入：两条预警连着来不能互抢图标、最后停在告警态');
  ok(/"monitor"[\s\S]{0,120}"alert0"/.test(trayRs),
     '托盘菜单必须有「监控事件」与最近预警条目');
  ok(/emit\("tray:open-monitor"/.test(trayRs) && /tray:open-monitor/.test(appjs),
     '托盘菜单点击后要通知前端切到监控页（tray:open-monitor）');
  ok(/fn menu_text/.test(trayRs),
     '预警文本进菜单前必须压成单行并截断（长路径会把菜单撑出屏幕）');
  // 两条监控路径都要接上：联动（monitor:tick）与独立（independent record）
  ok((fs.readFileSync(path.join(__dirname, '..', 'src-tauri', 'src', 'monitor.rs'), 'utf8')
       .match(/tray::notify_alert/g) || []).length >= 2,
     '联动监控的轮询与收尾两条路都要发提醒（收尾也可能吐出最后几条预警）');
  const indepRs = fs.readFileSync(path.join(__dirname, '..', 'src-tauri', 'src', 'independent.rs'), 'utf8');
  ok(/pub fn attach/.test(indepRs) && /fn notify_alert/.test(indepRs),
     '独立监控也要有通知出口（Hub 在窗口建好之后 attach 一次）');
  ok(/hub\.notify_alert\(&text\)/.test(indepRs),
     '独立监控的预警必须真的发出去（record 返回触发文本，锁放掉之后再发）');
  ok(/independent\.attach\(/.test(mainRsText),
     'main.rs 要在恢复独立监控之前 attach，否则启动即触发的那条预警发不出去');

  /* ── 左下角那块「扫描目录」应当已经移除 ───────────────────────────────
   * 盘符列表本身就是"选扫描范围"的地方，再放一个输入框，两个入口会互相抢 ——
   * 用户填了框却点的是盘符，或者反过来。 */
  for (const gone of ['custom-root', 'scan-root']) {
    ok(!htmlIds.has(gone), `#${gone} 应已随左下角的扫描目录模块一起删除`);
  }
  ok(!/getElementById\('watch'\)/.test(appjs),
     '「同时建立实时监视」复选框已删除：它现在是模式的一部分（budget.default_watch）');
  ok(/watch: !!\(\s*state\.budget && state\.budget\.default_watch\s*\)/.test(appjs),
     '扫描时的 watch 必须来自后端下发的 default_watch（口径只剩一处）');

  /* ── 右键「在文件资源管理器打开」 ─────────────────────────────────────
   * 与「定位」是两件事：打开 = 进到这个文件夹；定位 = 打开上级并选中它。 */
  ok(/async function contextExplore/.test(ctxjs) && /'explore_path'/.test(ctxjs),
     'context-menu.js 应有「在文件资源管理器打开」（contextExplore → explore_path）');
  ok(/item\('open', '在文件资源管理器打开'/.test(ctxjs),
     '「打开」要排在「定位」之前（两者是相邻的一对动作）');
  ok(/pub async fn explore_path/.test(fs.readFileSync(
       path.join(__dirname, '..', 'src-tauri', 'src', 'explorer.rs'), 'utf8')),
     'explorer.rs 应有 explore_path 命令');
  ok(/explore_target/.test(fs.readFileSync(
       path.join(__dirname, '..', 'src-tauri', 'src', 'explorer.rs'), 'utf8')),
     '打开动作必须经 explore_target 决定目标（文件 → 所在目录，绝不执行文件）');

  /* ── v0.1.1 右键「打开方式…」/「用默认程序打开」 ────────────────────────
   * 这两个动作**会执行目标文件**，所以边界必须写死：
   *   · 程序清单来自系统对话框，前端不许自己读注册表/拼命令行；
   *   · 可执行扩展名只在「打开方式…」里出现，不给一键执行。 */
  const explorerRs = fs.readFileSync(path.join(__dirname, '..', 'src-tauri', 'src', 'explorer.rs'), 'utf8');
  ok(/item\('open-with', '打开方式…'/.test(ctxjs),
     '右键菜单要有「打开方式…」');
  ok(/'open_with_dialog_path'/.test(ctxjs) && /pub async fn open_with_dialog_path/.test(explorerRs),
     '「打开方式…」必须走 open_with_dialog_path（前端只管递路径）');
  ok(/SHOpenWithDialog/.test(explorerRs) && /OPENASINFO/.test(explorerRs),
     '「打开方式」要用系统原生的 SHOpenWithDialog，不要自己猜关联程序');
  ok(/fn file_target/.test(explorerRs) && /不是文件/.test(explorerRs),
     '两个打开命令共用 file_target：目录与已删除的目标都要拒绝');
  ok(/CONTEXT_EXECUTABLE/.test(ctxjs) && /open-default/.test(ctxjs),
     '「用默认程序打开」必须经 CONTEXT_EXECUTABLE 挡掉可执行文件（点一下就跑起来）');
  ok(/contextIsExecutable\(path\) \? \[\] : \[item\('open-default'/.test(ctxjs),
     '可执行文件只能留「打开方式…」，不能同时给出「用默认程序打开」');

  /* ── 后台周期任务：不许换掉用户正在看的那块盘 ──────────────────────────
   * 一次扫描会替换进程里那份**唯一**的扫描结果，所以"后台扫 D 盘"的代价是
   * 用户正在看的 C 盘视图被换掉 —— 比"数据旧一点"糟得多。这条守卫挡的是
   * 有人图省事把轮转写成"逐个盘符扫一遍"。 */
  const schedRs = fs.readFileSync(SCHED_RS, 'utf8');
  ok(/next_scan_target/.test(schedRs) && /spawn_scan/.test(schedRs),
     '周期任务必须经 next_scan_target 挑目标再扫描');
  ok(/last_drive/.test(schedRs),
     '周期任务必须知道"用户当前在看哪块盘"（AppState::last_drive）');
  ok(!/for\s+\w+\s+in\s+&?drives[\s\S]{0,300}?spawn_scan/.test(schedRs),
     '周期任务不得逐个盘符无脑扫描 —— 那会把用户当前视图换掉');
  ok(/should_run/.test(schedRs) && !/background\(\)\s*\{\s*true/.test(profileRs),
     '后台任务只在性能模式跑（轻量模式的全部意义就是不跑）');

  /* ── 命令注册：app.js 调的每个命令都必须在 main.rs 里注册过 ───────────
   * 漏注册的后果是运行时报 `command not found`，而且只在点到那一处时才暴露。 */
  const mainRs = fs.readFileSync(MAIN_RS, 'utf8');
  const handlerBlock = /generate_handler!\[([\s\S]*?)\]/.exec(mainRs);
  ok(!!handlerBlock, 'main.rs 里应能找到 generate_handler! 列表');
  const registered = new Set(
    handlerBlock ? [...handlerBlock[1].matchAll(/(\w+)::(\w+)\s*,/g)].map((m) => m[2]) : []
  );
  const invokedCmds = new Set([...appjs.matchAll(/(?:invoke|call)\('(\w+)'/g)].map((m) => m[1]));
  const unregistered = [...invokedCmds].filter((c) => !registered.has(c));
  ok(unregistered.length === 0,
     `app.js 调了没注册的命令：${unregistered.join(', ')}（main.rs 的 generate_handler! 里没有）`);
  ok(registered.size >= 20, `main.rs 应注册一批命令（实际 ${registered.size} 个）`);

  // 分析视图的接线全在 app.js 里，所以它也必须在最后加载
  ok(html.indexOf('treemap.js') < html.indexOf('app.js'), 'treemap.js 必须在 app.js 之前引入');

  // 脚本顺序：两条规则都在 app.js 之前，否则 boot() 走到它们时函数还没定义
  // （ReferenceError，且只在启动时炸一次）
  const iRule = html.indexOf('admin-rule.js');
  const iView = html.indexOf('view-rule.js');
  const iApp = html.indexOf('app.js');
  ok(iRule >= 0, 'index.html 应引入 admin-rule.js');
  ok(iView >= 0, 'index.html 应引入 view-rule.js');
  ok(iRule >= 0 && iApp >= 0 && iRule < iApp, 'admin-rule.js 必须在 app.js 之前引入');
  ok(iView >= 0 && iApp >= 0 && iView < iApp, 'view-rule.js 必须在 app.js 之前引入');

  // 两个 `*-rule.js` 都必须保持"纯"：一旦开始摸 DOM，这个脚本就没法再跑真值表
  ok(!readsDom(RULE_JS), 'admin-rule.js 不应访问 document / window');
  ok(!readsDom(VIEW_RULE_JS), 'view-rule.js 不应访问 document / window');

  /* ── E1 增长归因 ────────────────────────────────────────────────────
   * 归因必须由后端在 diff 里算（要全量行，明细表只有前 300 行）。
   * 前端只画不聚合 —— 一旦在这里二次聚合，界面与 CSV/CLI 就会给出两套数。 */
  const ENGINE = path.join(__dirname, '..', '..', '..', 'crates');
  const snapshotRs = fs.readFileSync(path.join(ENGINE, 'core-scan', 'src', 'snapshot.rs'), 'utf8');
  const reportRs = fs.readFileSync(path.join(ENGINE, 'core-scan', 'src', 'report.rs'), 'utf8');
  const rulesRs = fs.readFileSync(path.join(ENGINE, 'core-monitor', 'src', 'rules.rs'), 'utf8');
  const sensitiveRs = fs.readFileSync(path.join(ENGINE, 'core-index', 'src', 'sensitive.rs'), 'utf8');
  const ntfsFilenameRs = fs.readFileSync(path.join(ENGINE, 'core-ntfs', 'src', 'filename.rs'), 'utf8');
  const cliRs = fs.readFileSync(path.join(__dirname, '..', '..', 'cli', 'src', 'main.rs'), 'utf8');

  ok(htmlIds.has('diff-attr') && usedIds.has('diff-attr'), '#diff-attr 必须在且被接线');
  ok(/renderDiffAttr\(v\)/.test(appjs), 'renderDiff 必须调 renderDiffAttr（否则归因卡永远不出现）');
  ok(/pub attr: Option<crate::attribution::AttrOptions>/.test(snapshotRs)
     && /pub attribution: Option<crate::attribution::Attribution>/.test(snapshotRs),
     'DiffOptions.attr / DiffReport.attribution 必须由引擎提供');
  ok(/attr: Some\(core_scan::AttrOptions::default\(\)\)/.test(analyzeRs),
     '界面的对比必须打开归因（否则后端不算，卡片永远空）');
  ok(/pub struct AttributionDto/.test(analyzeRs) && /pub attr: Option<AttributionDto>/.test(analyzeRs),
     'DiffView 必须带 attr 字段供前端渲染');

  /* ── E2 详情面板的 MFT 字段 ───────────────────────────────────────────
   * FRN 是 64 位，过 JSON 到 JS 会掉精度（超过 2^53）。
   * 所以它必须是字符串 —— 用数字会显示成一个"看起来合理但错误"的值。 */
  ok(/pub frn: String/.test(commandsRs),
     'NodeInfo.frn 必须是十六进制字符串（u64 过 JSON 到 JS 会掉精度）');
  ok(/pub (?:const )?fn make_frn/.test(ntfsFilenameRs),
     'FRN 必须由 core-ntfs::filename::make_frn 拼（位布局只一处）');
  ok(/FW_NODE_BITS/.test(appjs) && /FW_ATTR_BITS/.test(appjs),
     '索引位与系统属性位是两套，前端要分两张表解（不能混）');
  ok(/detail-sec/.test(appjs) && /MFT 属性（扫描时）/.test(appjs),
     '详情面板必须说清"哪个数字来自扫描、哪个来自点击时"');

  /* ── E3 单文件 HTML 报告 ────────────────────────────────────────────── */
  ok(htmlIds.has('menu-report'), 'index.html 缺少 #menu-report');
  ok(usedIds.has('menu-report'), 'app.js 没有接线 #menu-report（菜单项点了没反应）');
  ok(/exportReport/.test(appjs) && /call\('export_report'/.test(appjs),
     '菜单项必须调后端 export_report');
  ok(/core_scan::report::build_html/.test(analyzeRs),
     '界面的报告必须走引擎模板（与 CLI 同一份，否则两边版面对不上）');
  ok(/core_scan::build_html/.test(cliRs), 'CLI 的 report 也必须走同一份模板');
  // 只看模板本身：`#[cfg(test)]` 里的断言里当然会提到 http://
  const reportTpl = reportRs.split('#[cfg(test)]')[0];
  ok(!/https?:\/\//.test(reportTpl),
     '报告模板里不许出现外链（"断网可看"是这个功能的全部意义）');
  ok(/pub fn escape_html/.test(reportRs) && /escape_html\(/.test(reportRs),
     '报告必须转义文件名（名字里有 < 会把整页解析断掉）');

  /* ── E4 规则试算 ────────────────────────────────────────────────────── */
  ok(htmlIds.has('mon-simulate') && usedIds.has('mon-simulate'), '#mon-simulate 必须在且被接线');
  ok(htmlIds.has('mon-sim') && usedIds.has('mon-sim'), '#mon-sim 必须在且被接线');
  ok(/call\('rules_simulate'/.test(appjs), '试算必须调后端 rules_simulate');
  ok(/pub fn simulate/.test(rulesRs) && /pub struct Verdict/.test(rulesRs),
     '规则试算必须在引擎里（judgeable / gap 的口径与 evaluate 一致）');
  ok(/judgeable/.test(appjs), '前端必须区分"不会触发"与"判断不了"');
  ok(/Rate => \{[\s\S]{0,200}window_ms == 0/.test(rulesRs) || /elapsed_ms == 0/.test(rulesRs),
     '速率规则必须看时间跨度（没有窗口时判不了，而不是判成"不触发"）');

  /* ── 时间口径：只许一份实现，且必须挡住坏时间戳 ────────────────────
   * 曾经有两份（`fmtTime` / `fwLocalTime`），前者没有 Invalid Date 保护，
   * 于是一批时间显示成 `NaN-NaN-NaN`。这类"两条路各自演化"的问题只能靠
   * 静态断言钉住。 */
  ok(!/function fmtTime\s*\(/.test(appjs),
     '不许再有第二份时间格式化（fmtTime）：时间只走 fwLocalTime');
  const viewJs = fs.readFileSync(VIEW_RULE_JS, 'utf8');
  ok(/Number\.isNaN\(d\.getTime\(\)\)/.test(viewJs) && /y < 1601 \|\| y > 2200/.test(viewJs),
     'fwLocalTime 必须挡住 Invalid Date 与超出文件系统范围的时间戳');
  // 后端的时间格式化也只剩一份本地实现
  const coreUtilRs = fs.readFileSync(path.join(ENGINE, 'core-io', 'src', 'util.rs'), 'utf8');
  ok(/pub fn local_time/.test(coreUtilRs) && /pub fn local_stamp/.test(coreUtilRs),
     'core-io 必须提供 local_time / local_stamp（本地时区口径的唯一实现）');
  ok(/SystemTimeToTzSpecificLocalTime/.test(coreUtilRs),
     'Windows 上本地时间必须走 SystemTimeToTzSpecificLocalTime（含夏令时）');

  /* ── 归因卡的折叠与右键 ────────────────────────────────────────────── */
  for (const id of ['diff-attr-head', 'diff-attr-toggle', 'diff-attr-sum']) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}`);
  }
  ok(/function toggleDiffAttr/.test(appjs) && /attrCollapsed/.test(appjs),
     '归因卡必须能折叠（标题行留着，否则整块消失像是功能没了）');
  ok(/diff-attr-toggle'\)\.onclick = toggleDiffAttr/.test(appjs),
     '折叠按钮必须接上 toggleDiffAttr');
  ok(/'attrdir'/.test(appjs) && /'attrdir'/.test(ctxjs),
     '归因的目录行必须能右键（菜单类型要在 context-menu.js 里实现）');
  ok(/'attrcopy'/.test(appjs) && /'attrcopy'/.test(ctxjs),
     '类型 / 时段行右键要能复制这一行');

  /* ── E5 风险清单 ────────────────────────────────────────────────────── */
  for (const id of ['view-tab-risk', 'view-risk', 'risk-min-size', 'risk-limit',
                    'risk-run', 'risk-state', 'risk-summary', 'risk-list']) {
    ok(htmlIds.has(id), `index.html 缺少 #${id}`);
    ok(usedIds.has(id), `app.js 没有接线 #${id}`);
  }
  ok(/view-tab-risk/.test(appjs) && /setView\('risk'\)/.test(appjs),
     '风险清单必须能被切到（页签 → setView）');
  ok(/call\('sensitive_scan'/.test(appjs), '盘点必须调后端 sensitive_scan');
  ok(/core_index::sensitive::scan\(/.test(commandsRs), 'sensitive_scan 必须走引擎（规则表只有一份）');
  ok(/pub const RULES/.test(sensitiveRs) && /pub fn classify/.test(sensitiveRs),
     '规则表与匹配口径必须在 core-index::sensitive');
  ok(/不读文件内容/.test(html),
     '页面上必须写明"不读文件内容"（否则"没找到"会被读成"没有风险"）');
  ok(/Severity::High/.test(sensitiveRs), '命中必须分级（全平铺的清单没人看得完）');

  /* ── 结果 ────────────────────────────────────────────────────────── */
  console.log('='.repeat(72));
  console.log('界面规则校验（admin-rule.js / view-rule.js + DOM 接线 + 版面结构）');
  console.log('='.repeat(72));
  console.log('  6 格真值表 + 4 条边界 + 6 项按钮外观 + 16 格盘符归属真值表');
  console.log('  与 Rust 侧字段对账 + DOM 接线双向对账 + 栏位结构守卫');
  console.log('  着色取值与后端对齐 + 渲染层无 IO + 滚动条自绘 + 折叠栏宽度守恒');
  console.log('  运行模式取值与后端对账 + 开机自启接线 + 周期任务不得换掉当前视图');
  console.log(`  index.html ${htmlIds.size} 个 id，app.js 引用其中 ${usedIds.size} 个`);
  if (fails.length) {
    console.log(`\n  ❌ 失败 ${fails.length} 项：`);
    for (const f of fails) console.log('     · ' + f);
    process.exit(1);
  }
  console.log(`  ✅ 通过 ${pass} 项断言`);
}

main();
