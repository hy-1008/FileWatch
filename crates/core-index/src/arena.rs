//! 紧凑节点与节点数组（arena）。
//!
//! 目标：**单节点 ≤ 40 字节**（文档 §5.5）。
//!
//! ```text
//! parent u32 (4) + seq u16 (2) + flags u16 (2) + size u64 (8)
//! + alloc u64 (8) + mtime i64 (8) + name u32 (4) = 36 字节 → 对齐后 40 字节
//! ```
//!
//! 关键设计（文档 §5.3）：**FRN 的低 48 位就是 MFT 记录号，所以 FRN 天然是下标**，
//! `nodes[frn & 0xFFFF_FFFF_FFFF]` 直接定位，省掉整个 `HashMap<u64, NodeId>`
//! （200 万条目约浪费 30~50 MB 且 cache miss 严重）。

/// 无效父节点（未使用节点用）。
pub const INVALID_PARENT: u32 = u32::MAX;

// ── flags 位 ────────────────────────────────────────────────────────────────
pub const NODE_IN_USE: u16 = 0x0001;
pub const NODE_DIR: u16 = 0x0002;
/// 存在硬链接（`$STANDARD_INFORMATION.HardLinkCount > 1`）
pub const NODE_HARDLINKED: u16 = 0x0004;
pub const NODE_REPARSE: u16 = 0x0008;
pub const NODE_COMPRESSED: u16 = 0x0010;
pub const NODE_SPARSE: u16 = 0x0020;
/// 含备选数据流（ADS），本工具不计入统计（文档 §3.2）
pub const NODE_HAS_ADS: u16 = 0x0040;
/// `$DATA` 驻留（小文件）
pub const NODE_RESIDENT: u16 = 0x0080;
/// 属性溢出到 `$ATTRIBUTE_LIST`（大小可能不完整）
pub const NODE_ATTR_LIST: u16 = 0x0100;
/// 父目录不在索引内（孤儿节点，通常是已删除目录的残留）
pub const NODE_ORPHAN: u16 = 0x0200;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    /// 父节点下标（= 父目录 MFT 记录号）
    pub parent: u32,
    /// 序列号，用于识别记录复用（文档 §5.3）
    pub seq: u16,
    pub flags: u16,
    /// 逻辑大小：文件 = 自身；目录 = 子孙合计
    pub size: u64,
    /// 占用大小：文件 = 自身；目录 = 子孙合计
    pub alloc: u64,
    /// 修改时间（Unix 秒）
    pub mtime: i64,
    /// 字符串池偏移（替代 `String` 的 24 字节 + 堆分配）
    pub name: u32,
}

impl Node {
    pub const EMPTY: Node = Node {
        parent: INVALID_PARENT,
        seq: 0,
        flags: 0,
        size: 0,
        alloc: 0,
        mtime: 0,
        name: 0,
    };

    #[inline]
    pub fn in_use(&self) -> bool {
        self.flags & NODE_IN_USE != 0
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & NODE_DIR != 0
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        self.in_use() && self.flags & NODE_DIR == 0
    }
}

/// 节点数组：下标即 MFT 记录号。
#[derive(Debug, Clone)]
pub struct Arena {
    pub nodes: Vec<Node>,
}

impl Arena {
    pub fn with_capacity(n: usize) -> Arena {
        Arena {
            nodes: Vec::with_capacity(n),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 确保下标 `idx` 可寻址（中间空位填 `Node::EMPTY`）。
    #[inline]
    pub fn ensure(&mut self, idx: u32) {
        let need = idx as usize + 1;
        if need > self.nodes.len() {
            self.nodes.resize(need, Node::EMPTY);
        }
    }

    #[inline]
    pub fn get(&self, idx: u32) -> Option<&Node> {
        self.nodes.get(idx as usize)
    }

    #[inline]
    pub fn get_mut(&mut self, idx: u32) -> Option<&mut Node> {
        self.nodes.get_mut(idx as usize)
    }

    /// 已使用的节点数。
    pub fn used(&self) -> usize {
        self.nodes.iter().filter(|n| n.in_use()).count()
    }

    /// 预留全部槽位（拿到记录总数后一次性分配，避免反复 realloc）。
    pub fn preallocate(&mut self, count: u64) {
        let n = count as usize;
        if n > self.nodes.len() {
            self.nodes.resize(n, Node::EMPTY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_is_40_bytes() {
        // 文档 §5.5 的内存预算前提：单节点 40 字节
        assert_eq!(
            core::mem::size_of::<Node>(),
            40,
            "Node 必须是 40 字节，否则 200 万文件的 80 MB 预算不成立"
        );
    }

    #[test]
    fn node_alignment_is_8() {
        assert_eq!(core::mem::align_of::<Node>(), 8);
    }

    #[test]
    fn arena_resizes_with_empty() {
        let mut a = Arena::with_capacity(4);
        a.ensure(9);
        assert_eq!(a.len(), 10);
        assert!(!a.get(5).unwrap().in_use());
        assert_eq!(a.get(9).unwrap().parent, INVALID_PARENT);
    }

    #[test]
    fn flags_helpers() {
        let mut n = Node::EMPTY;
        assert!(!n.in_use());
        n.flags = NODE_IN_USE | NODE_DIR;
        assert!(n.in_use() && n.is_dir() && !n.is_file());
        n.flags = NODE_IN_USE;
        assert!(n.is_file());
    }

    #[test]
    fn preallocate_is_monotonic() {
        let mut a = Arena::with_capacity(0);
        a.preallocate(100);
        assert_eq!(a.len(), 100);
        a.preallocate(50);
        assert_eq!(a.len(), 100, "不应缩小");
    }
}
