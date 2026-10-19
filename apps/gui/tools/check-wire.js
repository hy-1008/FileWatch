/* 校验前端解码端 `src/wire.js` 与线格式文档一致。
 *
 * 这条检查补的是自动化测试唯一的缺口：界面点击没法自动跑，所以
 * 「Rust 编码 / JS 解码」这一跨语言契约必须单独钉住。
 *
 * 做法是**三方对拍**：
 *   - Rust 侧 `wire.rs` 的 round_trip 测试：Rust 编码 → Rust 解码（钉在文档上）
 *   - 本脚本：按**文档里的偏移**手工造字节流 → `wire.js` 解码（也钉在文档上）
 * 两边都对着同一份文档，文档就成了唯一事实来源。任何一侧偷偷改了偏移，
 * 都会在这里或那里炸出来。
 *
 * 用法：node apps/gui/tools/check-wire.js
 */

'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

const HERE = __dirname;
const WIRE_JS = path.join(HERE, '..', 'src', 'wire.js');

/* ── 把 wire.js 加载进一个沙箱（它不是 CommonJS 模块，也不是 ESM）────── */
function loadWire() {
  const src = fs.readFileSync(WIRE_JS, 'utf8');
  const sandbox = { console };
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox, { filename: 'wire.js' });
  // 顶层 `const` / `class` 是**词法声明**，只存在于该上下文的词法环境里，
  // 不会变成 `sandbox` 的属性（只有 `var` 和函数声明会）。所以要在同一个
  // 上下文里再求值一次，才能把它们取出来。
  return vm.runInContext(
    '({ Layout, fwColor, fwHeat, FW_PALETTE, FW_HEAT, FW_VERSION, ITEM_BYTES, HEADER_BYTES, FW_MAGIC })',
    sandbox,
    { filename: 'wire-exports' });
}

/* ── 按文档造一个夹具 ──────────────────────────────────────────────────── */
const MAGIC = 0x594c5746;
const VERSION = 1;
const HEADER = 16;
const ITEM = 56;

function buildFixture(count, root) {
  const buf = Buffer.alloc(HEADER + count * ITEM);
  buf.writeUInt32LE(MAGIC, 0);
  buf.writeUInt32LE(VERSION, 4);
  buf.writeUInt32LE(count, 8);
  buf.writeUInt32LE(root, 12);

  for (let i = 0; i < count; i++) {
    const b = HEADER + i * ITEM;
    // 每一项都给一组「一眼能认出来」的值，错位就无法通过
    const node = 100 + i;
    const depth = i % 17;
    const isDir = i % 2 === 0;
    const collapsed = i % 3 === 0;
    const hasChildren = i % 5 !== 0;
    const labeled = i % 7 !== 0;
    let flags = 0;
    if (isDir) flags |= 1;
    if (collapsed) flags |= 2;
    if (hasChildren) flags |= 4;
    if (labeled) flags |= 8;

    buf.writeUInt32LE(node, b + 0);
    buf.writeUInt8(depth, b + 4);
    buf.writeUInt8(flags, b + 5);
    buf.writeUInt8(i % 16, b + 6);
    buf.writeUInt8(0, b + 7);                    // 保留
    buf.writeUInt32LE(i % 4 === 0 ? 0xffffffff : node, b + 8);
    buf.writeFloatLE(0.125 * (i % 8), b + 12);
    buf.writeFloatLE(1.5 + i, b + 16);           // rect.x
    buf.writeFloatLE(2.5 + i, b + 20);           // rect.y
    buf.writeFloatLE(3.5 + i, b + 24);           // rect.w
    buf.writeFloatLE(4.5 + i, b + 28);           // rect.h
    buf.writeFloatLE(isDir ? 0 : 9.5, b + 32);   // header.x（文件为 0）
    buf.writeFloatLE(isDir ? 0 : 9.5, b + 36);
    buf.writeFloatLE(isDir ? 100 : 0, b + 40);   // header.w
    buf.writeFloatLE(isDir ? 16 : 0, b + 44);    // header.h
    // u64：故意用一个超过 2^32 的值，确保没有被 32 位截断
    buf.writeBigUInt64LE(BigInt(5_000_000_000) + BigInt(i), b + 48);
  }
  return buf;
}

/* ── 断言小工具 ────────────────────────────────────────────────────────── */
let pass = 0;
const fails = [];
function ok(cond, what) {
  if (cond) { pass++; } else { fails.push(what); }
}
function eq(a, b, what) {
  ok(a === b, `${what}：得到 ${a}，期望 ${b}`);
}
function throws(fn, what) {
  try { fn(); fails.push(`${what}：本应抛错却没有`); }
  catch (e) { pass++; }
}

/** `rgb(r,g,b)` → `{r,g,b}`；不是这个格式返回 null。 */
function parseRgb(css) {
  const m = /^rgb\((\d+),(\d+),(\d+)\)$/.exec(String(css));
  return m ? { r: +m[1], g: +m[2], b: +m[3] } : null;
}

/* ── 跑 ────────────────────────────────────────────────────────────────── */
function main() {
  const sandbox = loadWire();
  const { Layout, fwColor, fwHeat, FW_PALETTE, FW_HEAT, FW_VERSION, ITEM_BYTES, HEADER_BYTES } = sandbox;

  // 常量本身也要对：前端按这些值算偏移
  eq(FW_VERSION, VERSION, '版本号常量');
  eq(ITEM_BYTES, ITEM, '每项字节数常量');
  eq(HEADER_BYTES, HEADER, '头部字节数常量');
  eq(FW_PALETTE.length, 16, '色板槽位数（应与后端 SHADE_SLOTS 一致）');

  const COUNT = 64;
  const ROOT = 5;
  const buf = buildFixture(COUNT, ROOT);
  const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
  const L = new Layout(ab);

  eq(L.count, COUNT, 'count');
  eq(L.root, ROOT, 'root');

  for (let i = 0; i < COUNT; i++) {
    const tag = `第 ${i} 项`;
    eq(L.node(i), 100 + i, tag + ' node');
    eq(L.depth(i), i % 17, tag + ' depth');
    eq(L.isDir(i), i % 2 === 0, tag + ' isDir');
    eq(L.collapsed(i), i % 3 === 0, tag + ' collapsed');
    eq(L.hasChildren(i), i % 5 !== 0, tag + ' hasChildren');
    eq(L.labeled(i), i % 7 !== 0, tag + ' labeled');
    eq(L.slot(i), i % 16, tag + ' shade_slot');
    eq(L.extId(i), i % 4 === 0 ? 0xffffffff : 100 + i, tag + ' extId');
    ok(Math.abs(L.shadeT(i) - 0.125 * (i % 8)) < 1e-6, tag + ' shade_t');

    const r = L.rect(i);
    ok(Math.abs(r[0] - (1.5 + i)) < 1e-4 && Math.abs(r[1] - (2.5 + i)) < 1e-4
       && Math.abs(r[2] - (3.5 + i)) < 1e-4 && Math.abs(r[3] - (4.5 + i)) < 1e-4,
       tag + ` rect 偏移不对：${r}`);
    const h = L.header(i);
    eq(h[3], i % 2 === 0 ? 16 : 0, tag + ' header.h');
    // u64 高位没被截断：5_000_000_000 > 2^32
    eq(L.size(i), 5_000_000_000 + i, tag + ' size（u64 高位）');
  }

  /* 命中测试：后端的输出顺序保证父在子前，所以倒序第一个命中的就是最深的 */
  {
    // 第 7 项的 rect 是 x=8.5..12, y=9.5..14
    const hit = L.hit(9, 10);
    ok(hit >= 0, '命中测试应能命中');
    const hr = L.rect(hit);
    // 命中项的矩形必须真的包含该点
    ok(9 >= hr[0] && 9 < hr[0] + hr[2] && 10 >= hr[1] && 10 < hr[1] + hr[3],
       `命中的矩形 ${hr} 并不包含点 (9,10)`);
    eq(L.hit(-999, -999), -1, '点在所有矩形之外应返回 -1');
  }

  /* 错误路径：协议不匹配时必须**抛错**，而不是按旧偏移读出一堆错数据 */
  {
    const badMagic = Buffer.from(buf);
    badMagic.writeUInt32LE(0xdeadbeef, 0);
    throws(() => new Layout(badMagic.buffer.slice(badMagic.byteOffset, badMagic.byteOffset + badMagic.byteLength)),
      'magic 不对');

    const badVer = Buffer.from(buf);
    badVer.writeUInt32LE(999, 4);
    throws(() => new Layout(badVer.buffer.slice(badVer.byteOffset, badVer.byteOffset + badVer.byteLength)),
      '版本不对');

    // 长度与项数不符：多给 4 字节
    const padded = Buffer.concat([buf, Buffer.alloc(4)]);
    throws(() => new Layout(padded.buffer.slice(padded.byteOffset, padded.byteOffset + padded.byteLength)),
      '长度与项数不符');
  }

  /* 空布局 */
  {
    const empty = new Layout(buildFixture(0, 7).buffer);
    eq(empty.count, 0, '空布局 count');
    eq(empty.hit(0, 0), -1, '空布局命中');
  }

  /* 颜色：必须产出合法的 rgb()，且不同槽位颜色不同 */
  {
    for (let s = 0; s < 16; s++) {
      const c = fwColor(s, 0.5);
      ok(/^rgb\(\d+,\d+,\d+\)$/.test(c), `槽位 ${s} 颜色格式：${c}`);
    }
    const a = fwColor(0, 0.5), b = fwColor(1, 0.5);
    ok(a !== b, '不同槽位应有不同颜色');
    // 强度为 0 与 1 必须真的不同（否则深度着色就是摆设）
    ok(fwColor(3, 0) !== fwColor(3, 1), '强度参数应影响亮度');
    // 越界槽位不能崩
    ok(/^rgb\(/.test(fwColor(999, 0.5)), '槽位越界应回绕而不是报错');
  }

  /* 年龄热力图：**必须换色相**，不能只调亮度。
   *
   * 这条是给一次真实返工留的：以前年龄模式把色槽固定成 0、只调亮度，
   * 于是整张图同一个颜色，用户反馈「按文件年龄的图也是同一颜色」。
   * 只断言「格式合法」是抓不到的 —— 得断言两端颜色差别够大。 */
  {
    const hot = parseRgb(fwHeat(0));    // 刚改过
    const cold = parseRgb(fwHeat(1));   // 三年以上
    ok(hot && cold, 'fwHeat 应产出合法 rgb()');
    const d = Math.abs(hot.r - cold.r) + Math.abs(hot.g - cold.g) + Math.abs(hot.b - cold.b);
    ok(d > 120, `热力图两端应明显不同（实际通道差之和 ${d}）`);
    ok(hot.r > hot.b, '最近改过的应偏暖（红分量大于蓝分量）');
    ok(cold.b > cold.r, '最旧的应偏冷（蓝分量大于红分量）');
    // 单调：t 越大越冷（红分量单调不增）
    let prevR = 256;
    for (let i = 0; i <= 20; i++) {
      const c = parseRgb(fwHeat(i / 20));
      ok(c.r <= prevR, `热力图红分量应随 t 单调不增（t=${i / 20}）`);
      prevR = c.r;
    }
    // 越界/非法输入不能崩
    ok(/^rgb\(/.test(fwHeat(-5)), 'fwHeat 负值应夹住');
    ok(/^rgb\(/.test(fwHeat(9)), 'fwHeat 超过 1 应夹住');
    ok(/^rgb\(/.test(fwHeat(null)), 'fwHeat 非数字应回落到 0');
    eq(fwHeat(0), fwHeat(0), '同一个 t 必须稳定（图例与画面取同一份调色点）');
  }

  /* 结果 */
  console.log('='.repeat(72));
  console.log('前端线格式解码端校验（src/wire.js vs 文档偏移）');
  console.log('='.repeat(72));
  console.log(`  ${COUNT} 项逐字段对拍 + 命中测试 + 3 条错误路径 + 色板 + 年龄热力图`);
  if (fails.length) {
    console.log(`\n  ❌ 失败 ${fails.length} 项：`);
    for (const f of fails.slice(0, 20)) console.log('     · ' + f);
    process.exit(1);
  }
  console.log(`  ✅ 通过 ${pass} 项断言`);
}

main();
