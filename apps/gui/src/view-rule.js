/* 视图规则（纯函数）—— 不碰 DOM，可以脱离浏览器跑真值表。
 *
 * 为什么单独一个文件：界面的点击没法自动化，而"点亮哪一张盘符卡片"判错
 * 属于**看得见但看不出来**的那类 —— 卡片亮着，用户就以为自己在看那块盘，
 * 直到数字对不上才回头怀疑。抽成纯函数才能把边界全跑一遍（UNC、卷 GUID
 * 路径、盘符相对路径……这些在这台机器上都不会出现，但用户机器上会出现）。
 *
 * 校验：node apps/gui/tools/check-admin.js
 */

/** 扫描根 → 它落在哪一块本地卷上（`"C:"`）；不属于任何本地卷时返回 `null`。
 *
 * 后端给的 root 有五种形态，只有前三种能归到某个盘符：
 *
 * | 形态 | 例子 | 返回 |
 * |---|---|---|
 * | 裸盘符 | `C:` | `"C:"` |
 * | 盘根 | `C:\` | `"C:"` |
 * | 子目录 | `C:\Users` | `"C:"` |
 * | 卷 GUID 路径 | `\\?\Volume{…}\` | `null` |
 * | UNC 共享 | `\\server\share` | `null` |
 *
 * `C:foo` 这种「盘符相对路径」也返回 `null`：它在 Windows 上的意思是
 * "C 盘的当前目录"，和 `C:\` 不是一回事，点亮 C 盘会误导人。
 *
 * 返回 `null` 与返回 `"C:"` 的区别在界面上就是"一张卡片都不亮" ——
 * 所以宁可多返回 `null`，也不要猜。
 */
function fwDriveOfTarget(root) {
  if (typeof root !== 'string') return null;
  const s = root.trim();
  // 空串；以及 `\\` / `//` 开头的设备路径与 UNC —— 没有盘符可言。
  // 后端会把 `/` 归一成 `\`，但这里不依赖那个归一先发生。
  if (!s || s[0] === '\\' || s[0] === '/') return null;
  // 盘符后面必须正好是结尾或一个分隔符，否则就是 `C:foo`
  if (!/^[A-Za-z]:$/.test(s) && !/^[A-Za-z]:[\\/]/.test(s)) return null;
  return s.slice(0, 2).toUpperCase();
}

/** 在卷列表里找到"某个扫描根所在的那块盘"。
 *
 * 底部状态栏的「可用 / 总空间」靠它取值。写成纯函数是为了能跑真值表：
 * 后端给的 `drive` 有时带尾杠（`C:\`）、有时不带（`C:`），而扫描根是
 * `C:\Users\…` —— 直接 `===` 比字符串会在某一种组合上静默取不到，
 * 状态栏就一直显示 `—`，看起来像"这功能没做"。
 *
 * 找不到返回 `null`（UNC 路径、盘符被拔掉、卷列表还没拉到）。
 */
function fwVolumeOfTarget(volumes, target) {
  const want = fwDriveOfTarget(target);
  if (!want || !Array.isArray(volumes)) return null;
  return volumes.find((v) => fwDriveOfTarget(v && v.drive) === want) || null;
}

/** 文件名太长时**从中间省略**，并保证扩展名不被吃掉。
 *
 * 为什么不是尾部省略（`text-overflow: ellipsis`）：文件名的结尾恰恰是
 * 最有信息量的部分 —— `2026-10-16 季度汇总 v3 - 副本.xlsx` 与
 * `2026-10-16 季度汇总 v3 - 副本(1).xlsx` 只差结尾，尾部省略后两者
 * 看起来一模一样，用户会以为列表重复了。
 *
 * 规则：
 *   * 预算内原样返回（不画蛇添足）；
 *   * 超长时保留「头一段 + … + 扩展名」，扩展名连点算（`.tar.gz`）；
 *   * 没有扩展名（或以点开头的隐藏文件 `.gitignore`）就纯粹按头尾分；
 *   * 预算小到放不下扩展名时不强留 —— 留一个只有省略号的串更没用。
 *
 * @param {string} name 文件名（不含目录）
 * @param {number} max  最多显示多少个字符（含省略号）
 */
function fwMiddleEllipsis(name, max) {
  const s = String(name == null ? '' : name);
  const budget = Math.max(4, Math.floor(Number(max) || 0));
  if (s.length <= budget) return s;
  // 扩展名：最后一个点之后的部分，且点不在开头（`.gitignore` 是名字不是扩展名）
  const dot = s.lastIndexOf('.');
  let ext = '';
  if (dot > 0 && s.length - dot <= 12) ext = s.slice(dot);
  const room = budget - 1 - ext.length; // 1 是省略号自己
  if (room < 2) {
    // 扩展名太长/预算太小：退化成"头部 + …"
    return s.slice(0, budget - 1) + '…';
  }
  // 头多分一点（用户先看文件名开头），尾少分一点
  const head = Math.ceil(room * 0.6);
  const tail = room - head;
  return s.slice(0, head) + '…' + (tail > 0 ? s.slice(s.length - ext.length - tail, s.length - ext.length) : '') + ext;
}

/** 时间戳（Unix 秒）→ 本地 `YYYY-MM-DD HH:MM`。
 *
 * 用本地时间而不是 UTC：文件时间戳是"我这台机器上的什么时候"，
 * 用户在资源管理器里看到的也是本地时间，两边对不上就会被当成 bug。
 * 0 表示"不知道"（快照里没有 / 文件已删除），显示 `—`。
 */
function fwLocalTime(ts) {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  if (Number.isNaN(d.getTime())) return '—';
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
