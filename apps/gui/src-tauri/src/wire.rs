//! Rust → 前端的**二进制**布局通道。
//!
//! # 为什么不用 JSON
//!
//! 文档 §5.6 的传输要求：200 万节点序列化成 JSON 会把主线程卡死、体积也膨胀
//! 好几倍。而布局结果本来就是一串**定长数值**，没有任何嵌套结构 —— 那就没有
//! 理由为它付 JSON 的解析与字符串分配。直接按定长记录打包成 `ArrayBuffer`，
//! 前端用 `DataView` / 类型化数组视图零拷贝地读出来。
//!
//! # 线格式（**小端**）
//!
//! ```text
//! 头部 16 字节
//!   0  u32  magic   = 0x594C5746（ASCII "FWLY"）
//!   4  u32  version = 1
//!   8  u32  count   项数
//!  12  u32  root    视图根节点下标（前端做面包屑的起点）
//!
//! 每项 56 字节
//!   0  u32   node        节点下标（= MFT 记录号）
//!   4  u8    depth
//!   5  u8    flags       bit0 is_dir / bit1 collapsed / bit2 has_children / bit3 labeled
//!   6  u8    shade_slot
//!   7  u8    保留（只为让后面 8 字节处的 u32 落在 4 字节边界上）
//!   8  u32   ext_id      u32::MAX = 无扩展名/其他
//!  12  f32   shade_t     0~1
//!  16  4×f32 rect        x, y, w, h
//!  32  4×f32 header      x, y, w, h（文件为全 0）
//!  48  u64   size
//! ```
//!
//! 56 字节是**故意**留了 4 字节填充的：让 `size` 这个 u64 自然对齐，
// 前端可以用 `DataView.getBigUint64` 一次读出来。省掉那 4 字节会让每项
// 变成 52 字节、所有字段跨对齐边界 —— 前端就得手工拼高低位，不值得。

use core_viz::LayoutItem;

/// `"FWLY"` 的小端 u32。前端拿到 buffer 先验这个，防止协议错版本。
pub const MAGIC: u32 = 0x594C_5746;
/// 线格式版本。**改动字段布局时必须 +1**，否则前端会按旧偏移读出错乱数据。
pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 16;
pub const ITEM_BYTES: usize = 56;

pub const FLAG_IS_DIR: u8 = 1 << 0;
pub const FLAG_COLLAPSED: u8 = 1 << 1;
pub const FLAG_HAS_CHILDREN: u8 = 1 << 2;
pub const FLAG_LABELED: u8 = 1 << 3;

/// 把布局结果编成前端可直接消费的字节流。
pub fn encode_layout(root: u32, items: &[LayoutItem]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + items.len() * ITEM_BYTES);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    out.extend_from_slice(&root.to_le_bytes());

    for it in items {
        let mut flags = 0u8;
        if it.is_dir {
            flags |= FLAG_IS_DIR;
        }
        if it.collapsed {
            flags |= FLAG_COLLAPSED;
        }
        if it.has_children {
            flags |= FLAG_HAS_CHILDREN;
        }
        if it.labeled {
            flags |= FLAG_LABELED;
        }
        out.extend_from_slice(&it.node.to_le_bytes());
        out.push(it.depth.min(u8::MAX as u16) as u8);
        out.push(flags);
        out.push(it.shade_slot);
        out.push(0); // 填充
        out.extend_from_slice(&it.ext_id.to_le_bytes());
        out.extend_from_slice(&it.shade_t.to_le_bytes());
        for v in [
            it.rect.x,
            it.rect.y,
            it.rect.w,
            it.rect.h,
            it.header.x,
            it.header.y,
            it.header.w,
            it.header.h,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&it.size.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_viz::Rect;

    fn item(node: u32) -> LayoutItem {
        LayoutItem {
            node,
            rect: Rect { x: node as f32, y: node as f32 * 2.0, w: 10.0, h: 20.0 },
            header: Rect { x: 0.0, y: 0.0, w: 10.0, h: 16.0 },
            depth: (node % 17) as u16,
            is_dir: node % 2 == 0,
            ext_id: if node % 3 == 0 { u32::MAX } else { node },
            shade_slot: (node % 16) as u8,
            shade_t: 0.25,
            size: 1_000_000_000_000 + node as u64,
            collapsed: node % 5 == 0,
            has_children: node % 7 != 0,
            labeled: node % 11 != 0,
        }
    }

    /// 解码器 —— 前端读的是同一套偏移，这里把它在 Rust 里实现一遍用来对拍。
    /// 它同时也是「线格式文档写得对不对」的可执行校验。
    fn decode(bytes: &[u8]) -> (usize, u32, Vec<DecodedItem>) {
        let u32at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!(u32at(0), MAGIC, "magic 不对");
        assert_eq!(u32at(4), VERSION, "版本不对");
        let count = u32at(8) as usize;
        let root = u32at(12);
        assert_eq!(bytes.len(), HEADER_BYTES + count * ITEM_BYTES, "总长度与项数不符");

        let mut items = Vec::with_capacity(count);
        for i in 0..count {
            let b = HEADER_BYTES + i * ITEM_BYTES;
            let f32at = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
            let u64at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
            items.push(DecodedItem {
                node: u32at(b),
                depth: bytes[b + 4],
                flags: bytes[b + 5],
                shade_slot: bytes[b + 6],
                ext_id: u32at(b + 8),
                shade_t: f32at(b + 12),
                rect: [f32at(b + 16), f32at(b + 20), f32at(b + 24), f32at(b + 28)],
                header: [f32at(b + 32), f32at(b + 36), f32at(b + 40), f32at(b + 44)],
                size: u64at(b + 48),
            });
        }
        (count, root, items)
    }

    #[derive(Debug)]
    struct DecodedItem {
        node: u32,
        depth: u8,
        flags: u8,
        shade_slot: u8,
        ext_id: u32,
        shade_t: f32,
        rect: [f32; 4],
        header: [f32; 4],
        size: u64,
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let items: Vec<LayoutItem> = (0..500).map(item).collect();
        let bytes = encode_layout(5, &items);
        assert_eq!(bytes.len(), HEADER_BYTES + 500 * ITEM_BYTES);

        let (count, root, back) = decode(&bytes);
        assert_eq!(count, 500);
        assert_eq!(root, 5);

        for (src, dst) in items.iter().zip(back.iter()) {
            assert_eq!(src.node, dst.node);
            assert_eq!(
                src.depth.min(u8::MAX as u16) as u8,
                dst.depth,
                "depth 字段只有 1 字节（真实最大深度 16，够用）"
            );
            assert_eq!(src.is_dir, dst.flags & FLAG_IS_DIR != 0);
            assert_eq!(src.collapsed, dst.flags & FLAG_COLLAPSED != 0);
            assert_eq!(src.has_children, dst.flags & FLAG_HAS_CHILDREN != 0);
            assert_eq!(src.labeled, dst.flags & FLAG_LABELED != 0);
            assert_eq!(src.shade_slot, dst.shade_slot);
            assert_eq!(src.ext_id, dst.ext_id, "u32::MAX 要原样保留");
            assert_eq!(src.shade_t, dst.shade_t);
            assert_eq!(src.rect.x, dst.rect[0]);
            assert_eq!(src.rect.y, dst.rect[1]);
            assert_eq!(src.rect.w, dst.rect[2]);
            assert_eq!(src.rect.h, dst.rect[3]);
            assert_eq!(src.header.w, dst.header[2]);
            assert_eq!(src.header.h, dst.header[3]);
            assert_eq!(src.size, dst.size, "u64 不能在高位被截断");
        }
    }

    #[test]
    fn empty_layout_is_just_a_header() {
        let bytes = encode_layout(5, &[]);
        assert_eq!(bytes.len(), HEADER_BYTES);
        let (count, root, items) = decode(&bytes);
        assert_eq!((count, root), (0, 5));
        assert!(items.is_empty());
    }

    /// 尺寸钉死：前端是按硬编码偏移读的，改布局必须同步改前端并升版本号。
    #[test]
    fn layout_size_is_pinned() {
        assert_eq!(ITEM_BYTES, 56);
        assert_eq!(HEADER_BYTES, 16);
        assert_eq!(encode_layout(0, &[item(1)]).len(), 72);
    }

    /// `depth` 只占 1 字节，编码时会**夹住**而不是回绕。
    ///
    /// 回绕（`as u8` 截断）会让深度 256 变成 0 —— 一个「看起来是顶层」的
    /// 假象，比夹到 255 危险得多。真实深度上限是 16，所以只测边界行为。
    #[test]
    fn depth_is_clamped_not_wrapped() {
        let mut it = item(1);
        it.depth = 300;
        let bytes = encode_layout(5, &[it]);
        assert_eq!(bytes[HEADER_BYTES + 4], 255, "应夹到 u8 上限而不是回绕成 44");
        it.depth = 16;
        assert_eq!(encode_layout(5, &[it])[HEADER_BYTES + 4], 16);
    }
}
