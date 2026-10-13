//! 硬链接去重与「与资源管理器对不上」的解释面板（文档 §3.2 / 风险 R8）。
//!
//! ## 为什么天然就去重了
//!
//! NTFS 上一个硬链接**不是两个文件**：多个目录项指向**同一个 MFT 记录**。
//! 本工具以「MFT 记录号 = 节点下标」建模（文档 §5.3），所以
//! 一条记录只会产生一个节点，**大小天然只计一次**——不需要额外的哈希去重。
//!
//! 但资源管理器按「目录项」遍历，会把硬链接重复计入，
//! 于是用户会看到两边数字不一致。本模块产出一份**可解释的差异说明**。

use crate::arena::{NODE_COMPRESSED, NODE_HARDLINKED, NODE_HAS_ADS, NODE_REPARSE, NODE_SPARSE};
use crate::tree::Tree;

#[derive(Debug, Clone, Default)]
pub struct DedupReport {
    /// 被去重的硬链接记录数（每个都代表「多个目录项指向同一份数据」）
    pub hardlinked_records: u64,
    /// 重解析点（符号链接 / 联接点）数量——不跟随，不递归（文档 §3.2）
    pub reparse_points: u64,
    /// 稀疏文件数量（逻辑大小 ≫ 占用大小）
    pub sparse_files: u64,
    /// 压缩文件数量（占用大小取压缩后实际分配量）
    pub compressed_files: u64,
    /// 含 ADS 的文件数量（只统计主 `$DATA` 流）
    pub ads_files: u64,
    /// 稀疏/压缩导致的「逻辑 − 占用」差额合计
    pub sparse_compressed_slack: u64,
}

impl DedupReport {
    /// 生成「与资源管理器差异」的人话解释。
    pub fn explain_lines(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.hardlinked_records > 0 {
            v.push(format!(
                "硬链接：{} 条记录被多个目录项引用，本工具按 MFT 记录号去重，只计一次大小",
                self.hardlinked_records
            ));
        }
        if self.reparse_points > 0 {
            v.push(format!(
                "重解析点/符号链接：{} 个（不跟随、不递归，本身只计很小的体积）",
                self.reparse_points
            ));
        }
        if self.sparse_files > 0 || self.compressed_files > 0 {
            v.push(format!(
                "稀疏 {} / 压缩 {} 个文件：逻辑大小与占用大小差异合计 {}",
                self.sparse_files,
                self.compressed_files,
                core_io::util::human_size(self.sparse_compressed_slack)
            ));
        }
        if self.ads_files > 0 {
            v.push(format!(
                "备选数据流(ADS)：{} 个文件含命名数据流，本工具只统计主 $DATA 流",
                self.ads_files
            ));
        }
        if v.is_empty() {
            v.push("未发现硬链接/稀疏/压缩/ADS 等特殊情形，与资源管理器的差异应仅来自系统保留区域。".to_string());
        }
        v
    }
}

/// 遍历索引树生成去重报告。O(n)。
pub fn build_report(tree: &Tree) -> DedupReport {
    let mut r = DedupReport::default();
    for node in tree.arena.nodes.iter() {
        if !node.in_use() || node.is_dir() {
            continue;
        }
        if node.flags & NODE_HARDLINKED != 0 {
            r.hardlinked_records += 1;
        }
        if node.flags & NODE_REPARSE != 0 {
            r.reparse_points += 1;
        }
        if node.flags & NODE_SPARSE != 0 {
            r.sparse_files += 1;
        }
        if node.flags & NODE_COMPRESSED != 0 {
            r.compressed_files += 1;
        }
        if node.flags & NODE_HAS_ADS != 0 {
            r.ads_files += 1;
        }
        // 压缩/稀疏时分配量通常小于逻辑量
        if (node.flags & (NODE_SPARSE | NODE_COMPRESSED)) != 0 && node.size > node.alloc {
            r.sparse_compressed_slack = r
                .sparse_compressed_slack
                .saturating_add(node.size - node.alloc);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{NODE_COMPRESSED, NODE_DIR, NODE_HARDLINKED, NODE_IN_USE, NODE_SPARSE};
    use crate::arena::Node;

    fn tree_with(nodes: Vec<(u16, u64, u64)>) -> Tree {
        let mut t = Tree::new("C:", nodes.len() + 1);
        t.arena.ensure(nodes.len() as u32);
        // 根节点
        t.arena.nodes[0] = Node {
            parent: u32::MAX,
            seq: 1,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: 0,
        };
        for (i, (flags, size, alloc)) in nodes.iter().enumerate() {
            t.arena.nodes[i + 1] = Node {
                parent: 0,
                seq: 1,
                flags: NODE_IN_USE | *flags,
                size: *size,
                alloc: *alloc,
                mtime: 0,
                name: 0,
            };
        }
        t
    }

    #[test]
    fn counts_special_files() {
        let t = tree_with(vec![
            (NODE_HARDLINKED, 100, 100),
            (NODE_HARDLINKED, 200, 200),
            (NODE_REPARSE, 10, 10),
            (NODE_SPARSE, 1_000_000, 4096),
            (NODE_COMPRESSED, 5000, 1000),
            (NODE_HAS_ADS, 100, 100),
            (0, 42, 42),
        ]);
        let r = build_report(&t);
        assert_eq!(r.hardlinked_records, 2);
        assert_eq!(r.reparse_points, 1);
        assert_eq!(r.sparse_files, 1);
        assert_eq!(r.compressed_files, 1);
        assert_eq!(r.ads_files, 1);
        // (1_000_000 - 4096) + (5000 - 1000)
        //   = 995_904 + 4_000 = 999_904
        // 注意别再写成 `999_904 + 4000`：1_000_000 - 4096 是 995_904 而不是
        // 999_904（那相当于只减了 96），会把 compressed 那项重复加一遍。
        assert_eq!(r.sparse_compressed_slack, 995_904 + 4_000);
    }

    #[test]
    fn empty_case_explains_no_diff() {
        let t = tree_with(vec![]);
        let r = build_report(&t);
        let lines = r.explain_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("未发现"));
    }

    #[test]
    fn explanation_mentions_hardlinks() {
        let t = tree_with(vec![(NODE_HARDLINKED, 1, 1)]);
        let lines = build_report(&t).explain_lines();
        assert!(lines.iter().any(|l| l.contains("硬链接")));
    }
}
