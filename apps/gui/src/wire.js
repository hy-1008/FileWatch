/* 布局二进制通道的解码端 —— 与 `src-tauri/src/wire.rs` 是同一份协议。
 *
 * 为什么不用 JSON：200 万节点的布局结果序列化成 JSON 会卡死主线程，体积也要
 * 膨胀好几倍（文档 §5.6）。这里是定长记录，一个 DataView 就够。
 *
 * 字段偏移**必须**与 wire.rs 保持一致。任何一侧改了布局，都要同步改另一侧
 * 并把 VERSION 加一 —— 下面 decode 的第一件事就是校验版本，不匹配直接报错，
 * 而不是按旧偏移读出一堆看起来「差不多」的错数据。
 */

'use strict';

const FW_MAGIC = 0x594c5746; // "FWLY"
const FW_VERSION = 1;
const HEADER_BYTES = 16;
const ITEM_BYTES = 56;

const FLAG_IS_DIR = 1 << 0;
const FLAG_COLLAPSED = 1 << 1;
const FLAG_HAS_CHILDREN = 1 << 2;
const FLAG_LABELED = 1 << 3;

/** 类型化数组视图：不逐项分配对象，直接读同一块内存。 */
class Layout {
  constructor(buffer) {
    const dv = new DataView(buffer);
    if (buffer.byteLength < HEADER_BYTES) throw new Error('布局数据太短（连头都不完整）');
    const magic = dv.getUint32(0, true);
    const version = dv.getUint32(4, true);
    if (magic !== FW_MAGIC) {
      throw new Error(`布局数据 magic 不对：0x${magic.toString(16)}，期望 0x${FW_MAGIC.toString(16)}`);
    }
    if (version !== FW_VERSION) {
      throw new Error(`布局协议版本不匹配：后端 ${version} / 前端 ${FW_VERSION}（改了字段布局就要同时升两边）`);
    }
    this.count = dv.getUint32(8, true);
    this.root = dv.getUint32(12, true);
    if (buffer.byteLength !== HEADER_BYTES + this.count * ITEM_BYTES) {
      throw new Error(
        `布局长度与项数不符：${buffer.byteLength} != ${HEADER_BYTES} + ${this.count}×${ITEM_BYTES}` +
        `（多半是后端改了记录大小而前端没跟上）`);
    }
    this.dv = dv;
    this.base = HEADER_BYTES;
  }

  node(i)    { return this.dv.getUint32(this.base + i * ITEM_BYTES + 0, true); }
  depth(i)   { return this.dv.getUint8(this.base + i * ITEM_BYTES + 4); }
  flags(i)   { return this.dv.getUint8(this.base + i * ITEM_BYTES + 5); }
  slot(i)    { return this.dv.getUint8(this.base + i * ITEM_BYTES + 6); }
  extId(i)   { return this.dv.getUint32(this.base + i * ITEM_BYTES + 8, true); }
  shadeT(i)  { return this.dv.getFloat32(this.base + i * ITEM_BYTES + 12, true); }

  rect(i) {
    const o = this.base + i * ITEM_BYTES + 16;
    return [this.dv.getFloat32(o, true), this.dv.getFloat32(o + 4, true),
            this.dv.getFloat32(o + 8, true), this.dv.getFloat32(o + 12, true)];
  }

  header(i) {
    const o = this.base + i * ITEM_BYTES + 32;
    return [this.dv.getFloat32(o, true), this.dv.getFloat32(o + 4, true),
            this.dv.getFloat32(o + 8, true), this.dv.getFloat32(o + 12, true)];
  }

  /**
   * size 是 u64。用 `getBigUint64` 读出再降到 Number：Number 的整数精度
   * 到 2^53（≈ 8 PB），任何真实磁盘都不可能越过它，所以降级是安全的；
   * 但**不能**用 `getFloat64` 硬读那 8 个字节（浮点解释会得到完全不同的值）。
   */
  size(i) { return Number(this.dv.getBigUint64(this.base + i * ITEM_BYTES + 48, true)); }

  isDir(i)      { return (this.flags(i) & FLAG_IS_DIR) !== 0; }
  collapsed(i)  { return (this.flags(i) & FLAG_COLLAPSED) !== 0; }
  hasChildren(i){ return (this.flags(i) & FLAG_HAS_CHILDREN) !== 0; }
  labeled(i)    { return (this.flags(i) & FLAG_LABELED) !== 0; }

  /**
   * 命中测试：返回**最后绘制**（即最深）的那个节点的项下标。
   *
   * 后端的输出顺序保证父节点一定排在子节点前面，所以倒序扫描第一个命中的
   * 就是最深的那个 —— 和 LayoutResult::hit_test 是同一个约定。
   */
  hit(x, y) {
    for (let i = this.count - 1; i >= 0; i--) {
      const r = this.rect(i);
      if (x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]) return i;
    }
    return -1;
  }
}

/* ── 色板 ──────────────────────────────────────────────────────────────
 * 与后端 SHADE_SLOTS = 16 对齐：后端只给「槽位号 + 强度」，具体颜色由前端定，
 * 这样换主题不必动引擎。
 */
const FW_PALETTE = [
  '#4c8dff', '#57c98a', '#f2994a', '#eb5757', '#9b6bff', '#2fbdd6',
  '#d4c257', '#e06ba8', '#6ba8e0', '#3fae7f', '#c98a3f', '#a85555',
  '#7b6bd6', '#4fa3b8', '#b8a84a', '#c05f8e',
];

/** 槽位 + 强度 → CSS 颜色。强度用于同色系内区分（深度）。 */
function fwColor(slot, t) {
  const hex = FW_PALETTE[slot % FW_PALETTE.length];
  return fwMix(hex, hex, 0, 0.45 + clamp01(t) * 0.7);
}

/**
 * 年龄热力图：**刚改过的暖、越旧越冷**。
 *
 * 与后端 `ShadingMode::ByAge` 的 `shade_t` 配套：`t=0` 是「今天改过」，
 * `t=1` 是 `AGE_NORM_DAYS`（三年）以上。
 *
 * 之前这里走的是「同一个色相 + 调亮度」的老路（后端把槽位固定成 0），
 * 结果是整张图**同一个颜色**、只是深浅略有差别 —— 用户看不出新旧。
 * 热力图必须换色相：红→橙→黄→绿→青→蓝。
 */
const FW_HEAT = ['#eb5757', '#e0913f', '#d4c257', '#57c98a', '#2fbdd6', '#2f6fd6'];

/** `t` 在 0~1 → 热力图上的颜色（线性插值两端的调色点）。 */
function fwHeat(t) {
  const u = clamp01(t) * (FW_HEAT.length - 1);
  const i = Math.min(FW_HEAT.length - 2, Math.floor(u));
  return fwMix(FW_HEAT[i], FW_HEAT[i + 1], u - i, 1);
}

function clamp01(v) {
  return Math.max(0, Math.min(1, Number(v) || 0));
}

/** 两个十六进制色按 `f` 插值，再整体乘 `k`（亮度）。解析只写这一份。 */
function fwMix(hexA, hexB, f, k) {
  const a = fwRgb(hexA);
  const b = fwRgb(hexB);
  const ch = (i) => Math.max(0, Math.min(255, Math.round((a[i] + (b[i] - a[i]) * f) * k)));
  return `rgb(${ch(0)},${ch(1)},${ch(2)})`;
}

function fwRgb(hex) {
  return [
    parseInt(hex.slice(1, 3), 16),
    parseInt(hex.slice(3, 5), 16),
    parseInt(hex.slice(5, 7), 16),
  ];
}
