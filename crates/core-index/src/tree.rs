//! 目录树构建与遍历（CSR 子节点索引）。

use crate::arena::{Arena, Node, INVALID_PARENT, NODE_ATTR_LIST, NODE_COMPRESSED, NODE_DIR,
                   NODE_HAS_ADS, NODE_HARDLINKED, NODE_IN_USE, NODE_ORPHAN, NODE_REPARSE,
                   NODE_RESIDENT, NODE_SPARSE};
use crate::string_pool::StringPool;
use core_ntfs::filename::{
    frn_to_record, FN_FLAG_COMPRESSED, FN_FLAG_REPARSE, FN_FLAG_SPARSE,
};
use core_ntfs::record::MftRecord;
use core_ntfs::ROOT_RECORD;

/// 从文件/目录名取扩展名（不含点，小写）。无扩展名返回 `""`。
/// 取扩展名（不含点）。返回的是 `name` 里的**原始切片，不做大小写折叠**。
///
/// 折叠大小写的责任在调用方（见 `ext_stats::ExtensionTable::build`）：
/// 这里保持零分配，因为它在扫描热路径上被逐文件调用。
pub fn extension_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(p) if p > 0 && p + 1 < name.len() => &name[p + 1..],
        _ => "",
    }
}

/// 目录树。
#[derive(Debug, Clone)]
pub struct Tree {
    pub arena: Arena,
    pub names: StringPool,
    /// 仅目录有意义：子孙文件数（数组与 `arena.nodes` 等长）
    pub dir_file_counts: Vec<u32>,
    pub root: u32,
    pub drive: String,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_size: u64,
    pub total_alloc: u64,
    /// 签名/fixup 校验失败的记录数（文档 R1 的可观测指标）
    pub corrupt_records: u64,
    /// 为空闲记录或缺少 `$FILE_NAME` 而跳过的记录数
    pub skipped_records: u64,
    /// 扫描过程中观察到的硬链接记录数
    pub hardlink_records: u64,
    /// 含 ADS 的文件数
    pub ads_records: u64,
    pub finalized: bool,
}

impl Tree {
    pub fn new(drive: &str, record_capacity: usize) -> Tree {
        Tree {
            arena: Arena::with_capacity(record_capacity),
            names: StringPool::new(),
            dir_file_counts: Vec::new(),
            root: ROOT_RECORD as u32,
            drive: drive.to_string(),
            file_count: 0,
            dir_count: 0,
            total_size: 0,
            total_alloc: 0,
            corrupt_records: 0,
            skipped_records: 0,
            hardlink_records: 0,
            ads_records: 0,
            finalized: false,
        }
    }

    /// 拿到记录总数后一次性预分配（避免 200 万次 realloc）。
    pub fn preallocate(&mut self, record_count: u64, name_bytes_estimate: usize) {
        self.arena.preallocate(record_count);
        self.dir_file_counts
            .resize(record_count as usize, 0);
        self.names = StringPool::with_capacity(name_bytes_estimate);
    }

    /// 插入一条已解析的 MFT 记录。
    ///
    /// `record_buf` 必须是该记录原始缓冲（`name_off` 相对它计算）。
    pub fn insert(&mut self, rec: &MftRecord, record_buf: &[u8]) -> Option<u32> {
        if rec.recno > u32::MAX as u64 {
            self.skipped_records += 1;
            return None;
        }
        let idx = rec.recno as u32;
        self.arena.ensure(idx);
        if idx as usize >= self.dir_file_counts.len() {
            self.dir_file_counts.resize(idx as usize + 1, 0);
        }

        // 名字入池（零拷贝：直接从记录缓冲解 UTF-16LE）
        let name_off = match rec.name_bytes(record_buf) {
            Some(b) => self.names.intern_utf16le(b),
            None => 0,
        };

        let mut flags = NODE_IN_USE;
        if rec.is_dir() {
            flags |= NODE_DIR;
        }
        if rec.is_hard_link() {
            flags |= NODE_HARDLINKED;
            self.hardlink_records += 1;
        }
        if rec.fn_flags & FN_FLAG_REPARSE != 0 {
            flags |= NODE_REPARSE;
        }
        if rec.fn_flags & FN_FLAG_COMPRESSED != 0 {
            flags |= NODE_COMPRESSED;
        }
        if rec.fn_flags & FN_FLAG_SPARSE != 0 {
            flags |= NODE_SPARSE;
        }
        if rec.has_named_stream {
            flags |= NODE_HAS_ADS;
            self.ads_records += 1;
        }
        if rec.resident_data {
            flags |= NODE_RESIDENT;
        }
        if rec.has_attr_list {
            flags |= NODE_ATTR_LIST;
        }

        let parent_rec = frn_to_record(rec.parent_frn);
        let parent = if parent_rec > u32::MAX as u64 {
            INVALID_PARENT
        } else {
            parent_rec as u32
        };

        self.arena.nodes[idx as usize] = Node {
            parent,
            seq: rec.seq,
            flags,
            // 目录自身不占数据簇，其大小由 aggregate 累加子孙得出（文档 §3.2）
            size: if rec.is_dir() { 0 } else { rec.real_size },
            alloc: if rec.is_dir() { 0 } else { rec.alloc_size },
            mtime: rec.mtime,
            name: name_off,
        };
        Some(idx)
    }

    #[inline]
    pub fn name(&self, idx: u32) -> &str {
        match self.arena.get(idx) {
            Some(n) => self.names.get(n.name),
            None => "",
        }
    }

    /// **增量路径**：用 USN 事件里已有的信息建立/更新一个节点。
    ///
    /// 与 [`Tree::insert`] 的区别在于数据来源：
    /// `insert` 走「MFT 记录 → 节点」，需要完整记录缓冲做零拷贝名字解析；
    /// 这里走「USN 事件（名字/父 FRN/属性）+ 回读得到的大小 → 节点」，
    /// 名字已经是 `String`，大小由 `core-scan` 回读 MFT 后传入。
    ///
    /// 返回节点下标。
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_incremental(
        &mut self,
        recno: u32,
        parent_recno: u32,
        seq: u16,
        is_dir: bool,
        size: u64,
        alloc: u64,
        mtime: i64,
        name: &str,
    ) -> u32 {
        self.arena.ensure(recno);
        if recno as usize >= self.dir_file_counts.len() {
            self.dir_file_counts.resize(recno as usize + 1, 0);
        }
        let name_off = self.names.intern_str(name);
        let mut flags = NODE_IN_USE;
        if is_dir {
            flags |= NODE_DIR;
        }
        self.arena.nodes[recno as usize] = Node {
            parent: parent_recno,
            seq,
            flags,
            // 目录的大小由 aggregate/propagate_delta 累积，自身置 0
            size: if is_dir { 0 } else { size },
            alloc: if is_dir { 0 } else { alloc },
            mtime,
            name: name_off,
        };
        recno
    }

    /// 只更新已存在节点的名字（重命名）。节点不存在时返回 `false`。
    pub fn rename_node(&mut self, recno: u32, new_name: &str, new_parent: u32) -> bool {
        if !self.arena.get(recno).map(|n| n.in_use()).unwrap_or(false) {
            return false;
        }
        let off = self.names.intern_str(new_name);
        if let Some(n) = self.arena.get_mut(recno) {
            n.name = off;
            if new_parent != u32::MAX {
                n.parent = new_parent;
            }
            true
        } else {
            false
        }
    }

    /// 把节点标记为「不再使用」，返回它被移除时的 `(size, alloc)` 供上层做负向 delta。
    ///
    /// 目录的 `size/alloc` 是聚合值，因此**必须返回**，否则父链上的聚合会失衡。
    ///
    /// 注意：**不清空 `parent`**。删除事件需要沿「这个节点原来的父链」做负向上传，
    /// 调用方正是靠 `parent` 字段找到起点；清空会导致删除的 delta 丢失。
    pub fn remove_node(&mut self, recno: u32) -> Option<(u64, u64)> {
        let node = self.arena.get(recno).copied()?;
        if !node.in_use() {
            return None;
        }
        if let Some(n) = self.arena.get_mut(recno) {
            n.flags &= !NODE_IN_USE;
        }
        Some((node.size, node.alloc))
    }

    /// 当前记录的 `(size, alloc)`（不存在或未使用返回 `None`）。
    #[inline]
    pub fn size_of(&self, recno: u32) -> Option<(u64, u64)> {
        let n = self.arena.get(recno)?;
        if !n.in_use() {
            return None;
        }
        Some((n.size, n.alloc))
    }

    #[inline]
    pub fn node(&self, idx: u32) -> Option<&Node> {
        self.arena.get(idx)
    }

    /// 目录的子孙文件数。
    #[inline]
    pub fn dir_file_count(&self, idx: u32) -> u32 {
        self.dir_file_counts.get(idx as usize).copied().unwrap_or(0)
    }

    /// 拼接完整路径（Windows `C:\Windows\System32` / Linux `/home/me/docs`）。
    ///
    /// 深度上限 64，遇环立即停止——**绝不因损坏数据死循环**。
    ///
    /// 前缀由 [`core_io::paths::root_prefix`] 给出（卷键 → 带尾分隔符的根），
    /// 分隔符取 `MAIN_SEPARATOR`。把这条口径收在外面，是因为它必须与
    /// `core-scan::paths::join_path`、`dirwatch::normalize_root`、
    /// `PathRemap` 完全一致：任何一处用了不同的分隔符，
    /// 路径索引查表就会**静默失效**（表现为"事件未解析"，不报错）。
    pub fn path_of(&self, idx: u32) -> String {
        const MAX: usize = 64;
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = idx;
        let mut guard = 0;

        while guard < MAX {
            let n = match self.arena.get(cur) {
                Some(n) if n.in_use() => n,
                _ => break,
            };
            parts.push(self.names.get(n.name));
            if cur == self.root || n.parent == INVALID_PARENT || n.parent == cur {
                break;
            }
            cur = n.parent;
            guard += 1;
        }
        parts.reverse();
        let mut s = core_io::paths::root_prefix(&self.drive);
        // 根目录本身的 `$FILE_NAME` 就是 "."，它不该出现在路径里
        let joined = parts
            .iter()
            .filter(|p| !p.is_empty() && **p != ".")
            .cloned()
            .collect::<Vec<_>>()
            .join(core_io::paths::SEP_STR);
        if joined.is_empty() {
            return s; // 卷根
        }
        s.push_str(&joined);
        s
    }

    /// 路径深度（根 = 0）。
    pub fn depth_of(&self, idx: u32) -> u32 {
        const MAX: u32 = 64;
        let mut cur = idx;
        let mut d = 0;
        while d < MAX {
            let n = match self.arena.get(cur) {
                Some(n) if n.in_use() => n,
                _ => break,
            };
            if cur == self.root || n.parent == INVALID_PARENT || n.parent == cur {
                break;
            }
            cur = n.parent;
            d += 1;
        }
        d
    }

    /// 估算常驻内存占用（文档 §9 的「常驻内存 ≤ 300 MB」验收指标）。
    pub fn memory_bytes(&self) -> usize {
        self.arena.nodes.len() * core::mem::size_of::<Node>()
            + self.names.bytes_used()
            + self.dir_file_counts.len() * core::mem::size_of::<u32>()
    }
}

/// CSR 形式的子节点索引：`offsets[i]..offsets[i+1]` 是节点 `i` 的子节点区间。
///
/// 构建 O(n)、查询 O(1)。2,000,000 节点约 16 MB
/// （`offsets` 2M×4B + `items` 2M×4B），换取面积图/树形表的随机访问能力。
#[derive(Debug, Clone, Default)]
pub struct ChildIndex {
    offsets: Vec<u32>,
    items: Vec<u32>,
}

impl ChildIndex {
    pub fn build(tree: &Tree) -> ChildIndex {
        let n = tree.arena.nodes.len();
        let mut offsets = vec![0u32; n + 1];

        // ① 计数
        for (i, node) in tree.arena.nodes.iter().enumerate() {
            if !node.in_use() {
                continue;
            }
            let p = node.parent;
            if p == INVALID_PARENT || p as usize == i || p as usize >= n {
                continue;
            }
            if !tree.arena.nodes[p as usize].in_use() {
                continue;
            }
            offsets[p as usize + 1] += 1;
        }
        // ② 前缀和
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        // ③ 填充
        let mut cursor = offsets.clone();
        let mut items = vec![0u32; offsets[n] as usize];
        for (i, node) in tree.arena.nodes.iter().enumerate() {
            if !node.in_use() {
                continue;
            }
            let p = node.parent;
            if p == INVALID_PARENT || p as usize == i || p as usize >= n {
                continue;
            }
            if !tree.arena.nodes[p as usize].in_use() {
                continue;
            }
            let slot = cursor[p as usize];
            if (slot as usize) < items.len() {
                items[slot as usize] = i as u32;
            }
            cursor[p as usize] += 1;
        }

        ChildIndex { offsets, items }
    }

    /// 子节点下标切片。
    pub fn children(&self, dir: u32) -> &[u32] {
        let i = dir as usize;
        if i + 1 >= self.offsets.len() {
            return &[];
        }
        let a = self.offsets[i] as usize;
        let b = self.offsets[i + 1] as usize;
        self.items.get(a..b).unwrap_or(&[])
    }

    /// 子节点数。
    pub fn child_count(&self, dir: u32) -> usize {
        self.children(dir).len()
    }

    pub fn memory_bytes(&self) -> usize {
        (self.offsets.len() + self.items.len()) * core::mem::size_of::<u32>()
    }
}

/// 标记父节点不可用的孤儿节点（通常是已删除目录的残留子项）。
pub fn mark_orphans(tree: &mut Tree) -> u64 {
    let n = tree.arena.nodes.len();
    let mut orphans = 0u64;
    for i in 0..n {
        let node = tree.arena.nodes[i];
        if !node.in_use() {
            continue;
        }
        let p = node.parent;
        let bad = p == INVALID_PARENT
            || p as usize >= n
            || p as usize == i
            || !tree.arena.nodes[p as usize].in_use();
        if bad && p as usize != i {
            tree.arena.nodes[i].flags |= NODE_ORPHAN;
            orphans += 1;
        }
    }
    orphans
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_ntfs::filename::make_frn;

    fn mk_record(recno: u64, parent_frn: u64, name: &str, is_dir: bool, size: u64, alloc: u64) -> (MftRecord, Vec<u8>) {
        // 构造 "rec 缓冲" 只填名字区，name_off 指向它
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut buf = vec![0u8; 0x30 + 0x42 + units.len() * 2];
        let name_off = 0x30 + 0x42;
        for (i, u) in units.iter().enumerate() {
            buf[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        let rec = MftRecord {
            frn: make_frn(recno, 1),
            recno,
            seq: 1,
            flags: 1 | if is_dir { 2 } else { 0 },
            hard_link_count: 1,
            parent_frn,
            namespace: 1,
            name_off: name_off as u32,
            name_chars: units.len() as u16,
            real_size: size,
            alloc_size: alloc,
            mtime: 1_700_000_000,
            fn_flags: 0,
            has_attr_list: false,
            resident_data: false,
            has_named_stream: false,
        };
        (rec, buf)
    }

    /// 构造一棵小树：root(5) → dir(10) → file(11), file(12);  root → file(20)
    fn build_sample_tree() -> Tree {
        let mut t = Tree::new("C:", 64);
        let root_frn = make_frn(5, 1);
        let items = vec![
            (5u64, root_frn, ".", true, 0, 0),
            (10, root_frn, "Windows", true, 0, 0),
            (11, make_frn(10, 1), "a.log", false, 1000, 4096),
            (12, make_frn(10, 1), "b.log", false, 2000, 4096),
            (20, root_frn, "pagefile.sys", false, 5000, 8192),
        ];
        for (recno, parent, name, is_dir, size, alloc) in items {
            let (rec, buf) = mk_record(recno, parent, name, is_dir, size, alloc);
            t.insert(&rec, &buf);
        }
        t
    }

    #[test]
    fn builds_names_and_parents() {
        let t = build_sample_tree();
        assert_eq!(t.name(10), "Windows");
        assert_eq!(t.name(11), "a.log");
        assert_eq!(t.arena.get(11).unwrap().parent, 10);
        assert!(t.arena.get(10).unwrap().is_dir());
        assert!(t.arena.get(11).unwrap().is_file());
    }

    #[test]
    fn path_assembly() {
        let t = build_sample_tree();
        assert_eq!(t.path_of(11), "C:\\Windows\\a.log");
        assert_eq!(t.path_of(20), "C:\\pagefile.sys");
        assert_eq!(t.path_of(5), "C:\\");
    }

    #[test]
    fn depth_calculation() {
        let t = build_sample_tree();
        assert_eq!(t.depth_of(5), 0);
        assert_eq!(t.depth_of(11), 2);
        assert_eq!(t.depth_of(20), 1);
    }

    #[test]
    fn child_index_groups_children() {
        let t = build_sample_tree();
        let ci = ChildIndex::build(&t);
        let mut root_kids = ci.children(5).to_vec();
        root_kids.sort_unstable();
        assert_eq!(root_kids, vec![10, 20]);
        assert_eq!(ci.children(10).len(), 2);
        assert_eq!(ci.child_count(11), 0);
    }

    #[test]
    fn extension_extraction() {
        assert_eq!(extension_of("a.log"), "log");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of(".hidden"), "");
        assert_eq!(extension_of("x.tar.gz"), "gz");
    }

    #[test]
    fn path_of_survives_cycles() {
        let mut t = Tree::new("C:", 16);
        let (rec10, buf10) = mk_record(10, make_frn(11, 1), "A", true, 0, 0);
        let (rec11, buf11) = mk_record(11, make_frn(10, 1), "B", true, 0, 0);
        t.insert(&rec10, &buf10);
        t.insert(&rec11, &buf11);
        // 10 与 11 互为父子：必须有环保护，不能挂起
        let p = t.path_of(10);
        assert!(!p.is_empty());
    }

    #[test]
    fn orphan_detection() {
        let mut t = Tree::new("C:", 16);
        // 父节点 99 不存在
        let (rec, buf) = mk_record(7, make_frn(99, 1), "orphan.txt", false, 1, 1);
        t.insert(&rec, &buf);
        let n = mark_orphans(&mut t);
        assert_eq!(n, 1);
        assert!(t.arena.get(7).unwrap().flags & NODE_ORPHAN != 0);
    }
}
