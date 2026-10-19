//! 扩展名聚合（功能 F5「文件类型列表」）。
//!
//! 产出 `ExtensionTable`：类型统计 + `扩展名 → ext_id` 映射。
//! 面积图与类型列表**共享同一个 `ext_id` 空间**，这样点击类型就能
//! O(1) 地把对应的矩形重新上色（文档 §5.6「类型列表联动」）。

use crate::aggregate::SizeMode;
use crate::tree::{extension_of, Tree};
use std::borrow::Cow;
use std::collections::HashMap;

/// 扩展名 → 表键。**唯一**一处做大小写折叠的地方。
///
/// Windows 文件系统不区分大小写，`Report.TXT` 与 `notes.txt` 在用户眼里是同一个
/// 扩展名。折叠规则只能有一份：以前建表时折叠、按名字查时**不**折叠，于是
/// `A.TXT` 在类型列表里有统计数字、按名字却查不到，落到「其他」桶里。
fn key_of(ext: &str) -> Cow<'_, str> {
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(ext.to_ascii_lowercase())
    } else {
        Cow::Borrowed(ext)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtStat {
    /// 小写扩展名，无扩展名为 `""`（显示时用「(无扩展名)」）
    pub ext: String,
    pub count: u64,
    pub size: u64,
    pub alloc: u64,
}

impl ExtStat {
    pub fn display_ext(&self) -> &str {
        if self.ext.is_empty() {
            "(无扩展名)"
        } else {
            &self.ext
        }
    }

    #[inline]
    pub fn value(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Logical => self.size,
            SizeMode::Allocated => self.alloc,
        }
    }
}

/// 扩展名统计表，下标即 `ext_id`。
#[derive(Debug, Clone, Default)]
pub struct ExtensionTable {
    pub stats: Vec<ExtStat>,
    map: HashMap<String, u32>,
    pub total_size: u64,
    pub total_count: u64,
}

impl ExtensionTable {
    /// 遍历所有文件按扩展名聚合。O(n)。
    ///
    /// 排序：**按大小降序**（面积/列表默认口径），同大小按扩展名字典序，
    /// 保证结果稳定可复现（HashMap 迭代顺序是不确定的）。
    pub fn build(tree: &Tree) -> ExtensionTable {
        // 键必须是**小写**的扩展名：Windows 文件系统不区分大小写，
        // `Report.TXT` 与 `notes.txt` 在用户眼里是同一个扩展名，统计表里
        // 也该是同一行。`extension_of` 返回的是名字池里的原始切片（零分配），
        // 折叠大小写只能在这里做。
        //
        // 这里用 `String` 作键、每个文件一次分配 —— 可以接受：本表只在
        // 用户显式要 `--exts` 时构建一次，不在 200 万文件的扫描热路径上。
        let mut m: HashMap<String, (u64, u64, u64)> = HashMap::new();

        for node in tree.arena.nodes.iter() {
            if !node.is_file() {
                continue;
            }
            let name = tree.names.get(node.name);
            let key = key_of(extension_of(name)).into_owned();
            let e = m.entry(key).or_insert((0, 0, 0));
            e.0 += 1;
            e.1 = e.1.saturating_add(node.size);
            e.2 = e.2.saturating_add(node.alloc);
        }

        // 稳定排序：size desc → ext asc
        let mut entries: Vec<(String, (u64, u64, u64))> = m.into_iter().collect();
        entries.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then_with(|| a.0.cmp(&b.0)));

        let mut stats = Vec::with_capacity(entries.len());
        let mut map = HashMap::with_capacity(entries.len());
        let mut total_size: u64 = 0;
        let mut total_count: u64 = 0;

        for (i, (ext, (count, size, alloc))) in entries.into_iter().enumerate() {
            map.insert(ext.clone(), i as u32);
            total_size = total_size.saturating_add(size);
            total_count = total_count.saturating_add(count);
            stats.push(ExtStat { ext, count, size, alloc });
        }

        ExtensionTable {
            stats,
            map,
            total_size,
            total_count,
        }
    }

    /// 名字 → `ext_id`；未知扩展名返回 `u32::MAX`（「其他」桶）。
    ///
    /// 注意是查**扩展名**（`key_of` 折叠后）而不是整个文件名 —— 表的键是扩展名。
    pub fn id_of_name(&self, name: &str) -> u32 {
        let key = key_of(extension_of(name));
        self.map.get(key.as_ref()).copied().unwrap_or(u32::MAX)
    }

    /// 节点下标 → `ext_id`。
    ///
    /// 面积图拿它上色（`ext_id % (SHADE_SLOTS-1)`），点击类型列表高亮也靠它，
    /// 所以**查不到就等于整个面积图只有一个颜色** —— 见下面 `node_lookup_*` 的用例。
    pub fn id_of_node(&self, tree: &Tree, idx: u32) -> u32 {
        match tree.arena.get(idx) {
            Some(n) if n.is_file() => {
                let key = key_of(extension_of(tree.names.get(n.name)));
                self.map.get(key.as_ref()).copied().unwrap_or(u32::MAX)
            }
            _ => u32::MAX,
        }
    }

    /// 直接按小写扩展名取 id。
    pub fn id_of_ext(&self, ext: &str) -> Option<u32> {
        self.map.get(ext).copied()
    }

    pub fn len(&self) -> usize {
        self.stats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }

    /// 占比（0~1）。
    pub fn share(&self, ext_id: u32, mode: SizeMode) -> f64 {
        if self.total_size == 0 {
            return 0.0;
        }
        match self.stats.get(ext_id as usize) {
            Some(s) => s.value(mode) as f64 / self.total_size as f64,
            None => 0.0,
        }
    }

    /// 前 N 大类型 + 「其他」合计（列表页通常只显示前 20 项）。
    pub fn top(&self, n: usize, mode: SizeMode) -> (Vec<&ExtStat>, u64) {
        if self.stats.len() <= n {
            return (self.stats.iter().collect(), 0);
        }
        let head: Vec<&ExtStat> = self.stats.iter().take(n).collect();
        let rest: u64 = self.stats.iter().skip(n).map(|s| s.value(mode)).sum();
        (head, rest)
    }
}

/// 给 UI 用的十六色分类色板索引（按 `ext_id` 取模，保证同类型同色）。
pub fn color_slot(ext_id: u32, slots: usize) -> usize {
    if slots == 0 {
        return 0;
    }
    if ext_id == u32::MAX {
        return slots - 1; // 「其他」固定最后一色
    }
    (ext_id as usize) % slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{Node, NODE_DIR, NODE_IN_USE};
    use crate::tree::Tree;

    fn tree_with_files(files: &[(&str, u64, u64)]) -> Tree {
        let mut t = Tree::new("C:", files.len() + 2);
        t.arena.ensure(files.len() as u32 + 1);
        t.arena.nodes[0] = Node {
            parent: u32::MAX,
            seq: 1,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: 0,
        };
        for (i, (name, size, alloc)) in files.iter().enumerate() {
            let off = t.names.intern_str(name);
            t.arena.nodes[i + 1] = Node {
                parent: 0,
                seq: 1,
                flags: NODE_IN_USE,
                size: *size,
                alloc: *alloc,
                mtime: 0,
                name: off,
            };
        }
        t
    }

    #[test]
    fn aggregates_by_extension() {
        let t = tree_with_files(&[
            ("a.txt", 100, 4096),
            ("b.txt", 200, 4096),
            ("c.log", 50, 1024),
            ("d.bin", 9000, 12288),
        ]);
        let table = ExtensionTable::build(&t);
        assert_eq!(table.len(), 3);
        // 按大小降序：bin(9000) > txt(300) > log(50)
        assert_eq!(table.stats[0].ext, "bin");
        assert_eq!(table.stats[0].size, 9000);
        assert_eq!(table.stats[1].ext, "txt");
        assert_eq!(table.stats[1].count, 2);
        assert_eq!(table.stats[1].size, 300);
        assert_eq!(table.stats[2].ext, "log");
        assert_eq!(table.total_size, 9350);
        assert_eq!(table.total_count, 4);
    }

    #[test]
    fn extension_ids_are_stable_and_contiguous() {
        let t = tree_with_files(&[("a.txt", 1, 1), ("b.log", 2, 2)]);
        let table = ExtensionTable::build(&t);
        for (i, s) in table.stats.iter().enumerate() {
            assert_eq!(table.id_of_ext(&s.ext), Some(i as u32));
        }
    }

    #[test]
    fn unknown_extension_maps_to_sentinel() {
        let t = tree_with_files(&[("a.txt", 1, 1)]);
        let table = ExtensionTable::build(&t);
        assert_eq!(table.id_of_name("nope.xyz"), u32::MAX);
    }

    /// 按名字/节点查 id 必须与统计表**同一套算法**。
    ///
    /// 这组用例以前不存在，于是 `id_of_node` 拿**整个文件名**去查以扩展名为键的
    /// 表，永远查不到 → 每个文件都拿到 `u32::MAX` → 面积图整片同色
    /// （用户反馈：「着色选文件类型时没有按类型上色」）。这个 bug 不会让编译失败、
    /// 也不会让任何既有测试变红，只会让画面难看，所以必须有测试盯着。
    #[test]
    fn node_lookup_returns_the_same_id_as_the_table() {
        let t = tree_with_files(&[
            ("a.txt", 100, 100),
            ("b.LOG", 50, 50),
            ("Makefile", 5, 5),
            ("c.bin", 900, 900),
        ]);
        let table = ExtensionTable::build(&t);

        // 逐个文件对照：`id_of_node` == 按扩展名查
        for (i, ext) in [(1, "txt"), (2, "log"), (3, ""), (4, "bin")] {
            let want = table.id_of_ext(ext).expect("表里应该有这个扩展名");
            assert_eq!(
                table.id_of_node(&t, i),
                want,
                "节点 #{i}（.{ext}）拿到的 id 与统计表不一致"
            );
        }
        // 无扩展名是**独立的一类**，不是「其他」桶
        assert_ne!(table.id_of_node(&t, 3), u32::MAX);
        // 目录不参与类型着色（保持「其他」）
        assert_eq!(table.id_of_node(&t, 0), u32::MAX);
        // 表里没有的扩展名才算「其他」
        let t2 = tree_with_files(&[("d.xyz", 1, 1)]);
        let table2 = ExtensionTable::build(&t2);
        assert_eq!(table2.id_of_node(&t2, 1), table2.id_of_ext("xyz").unwrap());
        assert_eq!(table2.id_of_name("e.abc"), u32::MAX);
    }

    /// 不同扩展名必须拿到**不同的 id** —— 面积图的色槽是 `ext_id % (SHADE_SLOTS-1)`，
    /// 全都相等就等于全图一个颜色。
    #[test]
    fn distinct_extensions_get_distinct_ids() {
        let t = tree_with_files(&[
            ("a.txt", 10, 10),
            ("b.log", 20, 20),
            ("c.bin", 30, 30),
            ("d.png", 40, 40),
        ]);
        let table = ExtensionTable::build(&t);
        let ids: Vec<u32> = (1..=4).map(|i| table.id_of_node(&t, i)).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_ne!(*id, u32::MAX, "第 {i} 个文件不该落进「其他」桶");
        }
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "四个扩展名应得到四个不同的 id");
    }

    /// 大小写折叠：`A.TXT` 与 `b.txt` 归到同一行，按名字查也要命中同一个 id。
    #[test]
    fn lookup_is_case_insensitive() {
        let t = tree_with_files(&[("A.TXT", 10, 10), ("b.txt", 20, 20)]);
        let table = ExtensionTable::build(&t);
        assert_eq!(table.len(), 1);
        let id = table.id_of_ext("txt").unwrap();
        assert_eq!(table.id_of_node(&t, 1), id);
        assert_eq!(table.id_of_node(&t, 2), id);
        assert_eq!(table.id_of_name("c.TxT"), id);
    }

    #[test]
    fn case_insensitive_extension() {
        let t = tree_with_files(&[("A.TXT", 10, 10), ("b.txt", 20, 20)]);
        let table = ExtensionTable::build(&t);
        assert_eq!(table.len(), 1);
        assert_eq!(table.stats[0].ext, "txt");
        assert_eq!(table.stats[0].count, 2);
    }

    #[test]
    fn files_without_extension_bucket() {
        let t = tree_with_files(&[("Makefile", 5, 5), (".gitignore", 3, 3)]);
        let table = ExtensionTable::build(&t);
        assert_eq!(table.len(), 1);
        assert_eq!(table.stats[0].ext, "");
        assert_eq!(table.stats[0].display_ext(), "(无扩展名)");
        assert_eq!(table.stats[0].count, 2);
    }

    #[test]
    fn share_computation() {
        let t = tree_with_files(&[("a.txt", 75, 75), ("b.log", 25, 25)]);
        let table = ExtensionTable::build(&t);
        let txt = table.id_of_ext("txt").unwrap();
        let log = table.id_of_ext("log").unwrap();
        assert!((table.share(txt, SizeMode::Logical) - 0.75).abs() < 1e-9);
        assert!((table.share(log, SizeMode::Logical) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn top_with_rest() {
        let t = tree_with_files(&[
            ("a.a", 100, 100),
            ("b.b", 50, 50),
            ("c.c", 30, 30),
            ("d.d", 20, 20),
        ]);
        let table = ExtensionTable::build(&t);
        let (head, rest) = table.top(2, SizeMode::Logical);
        assert_eq!(head.len(), 2);
        assert_eq!(head[0].ext, "a");
        assert_eq!(rest, 50); // c + d
    }

    #[test]
    fn directories_are_excluded() {
        let mut t = tree_with_files(&[("a.txt", 100, 100)]);
        t.arena.ensure(9);
        let off = t.names.intern_str("somedir");
        t.arena.nodes[9] = Node {
            parent: 0,
            seq: 1,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: off,
        };
        let table = ExtensionTable::build(&t);
        assert_eq!(table.len(), 1);
        assert_eq!(table.total_count, 1);
    }

    #[test]
    fn color_slot_is_bounded() {
        assert_eq!(color_slot(u32::MAX, 16), 15);
        assert!(color_slot(37, 16) < 16);
        assert_eq!(color_slot(5, 0), 0);
    }
}
