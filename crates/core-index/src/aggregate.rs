//! 大小增量聚合与 Top-N 查询。
//!
//! ## 为什么要「增量」聚合（文档坑 #3）
//!
//! 200 万文件如果每次变更都重算整棵子树，就是 O(n)，UI 必然卡死。
//! 正确做法是**把 delta 沿父链上传**：文件大小变化时只做 O(树深) ≈ 10 次内存写。
//! `propagate_delta` 就是这个 O(depth) 原语，全量扫描的 `aggregate` 也是同一套逻辑。

use crate::arena::{Node, INVALID_PARENT};
use crate::tree::Tree;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// 树深上限保护。真实 NTFS 路径深度远小于此值；超过说明数据异常。
pub const MAX_TREE_DEPTH: u32 = 64;

/// 统计口径（文档 §3.2：两个值都要保留，UI 可切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeMode {
    /// 逻辑大小：文件实际字节数
    Logical,
    /// 占用大小：磁盘上实际分配的簇总和
    Allocated,
}

impl SizeMode {
    #[inline]
    pub fn of(self, n: &Node) -> u64 {
        match self {
            SizeMode::Logical => n.size,
            SizeMode::Allocated => n.alloc,
        }
    }

    pub fn label_cn(self) -> &'static str {
        match self {
            SizeMode::Logical => "逻辑大小",
            SizeMode::Allocated => "占用大小",
        }
    }
}

/// 把 `delta` 从 `start` 的父节点起沿父链上传。
///
/// 返回值：实际被更新的节点数（= 树深）。
///
/// **这是阈值预警引擎的核心**：调用方在遍历过程中可以顺带检查
/// 「当前祖先是不是被监控目录」，从而 O(1) 完成规则求值（文档 §5.4）。
pub fn propagate_delta(
    tree: &mut Tree,
    start: u32,
    delta_size: i64,
    delta_alloc: i64,
    on_ancestor: impl FnMut(&mut Tree, u32),
) -> u32 {
    let first = match tree.arena.get(start) {
        Some(n) => n.parent,
        None => return 0,
    };
    propagate_from_parent(tree, first, delta_size, delta_alloc, on_ancestor)
}

/// 从**显式指定的第一个祖先节点**开始上传 delta。
///
/// 删除与移动场景必须用这个版本：被删/被移动的节点其 `parent` 已被改写，
/// 从节点出发会走错链路。
pub fn propagate_from_parent(
    tree: &mut Tree,
    first_ancestor: u32,
    delta_size: i64,
    delta_alloc: i64,
    mut on_ancestor: impl FnMut(&mut Tree, u32),
) -> u32 {
    let mut cur = first_ancestor;
    // 计数语义：**实际被更新的节点数**（= 树深），不是循环轮次。
    let mut updated = 0u32;

    while cur != INVALID_PARENT && updated < MAX_TREE_DEPTH {
        let ci = cur as usize;
        if ci >= tree.arena.nodes.len() {
            break;
        }
        if !tree.arena.nodes[ci].in_use() {
            break; // 孤儿链，停止上传
        }
        {
            let p = &mut tree.arena.nodes[ci];
            p.size = add_signed(p.size, delta_size);
            p.alloc = add_signed(p.alloc, delta_alloc);
        }
        on_ancestor(tree, cur);
        updated += 1;

        let next = tree.arena.nodes[ci].parent;
        if next == cur {
            break; // 根节点自指
        }
        cur = next;
    }
    updated
}

/// 全量聚合：清零目录大小 → 把每个文件的 size/alloc 沿父链上传 → 统计总量。
///
/// 复杂度 O(n × 树深)。200 万文件、平均树深 10 时约 2000 万次内存写，
/// release 下在几十毫秒量级，只做一次（扫描结束时）。
pub fn aggregate(tree: &mut Tree) {
    let len = tree.arena.nodes.len();

    // ① 目录清零 + 统计文件/目录数与总量
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 0;
    let mut total_size: u64 = 0;
    let mut total_alloc: u64 = 0;

    for i in 0..len {
        let n = tree.arena.nodes[i];
        if !n.in_use() {
            continue;
        }
        if n.is_dir() {
            dir_count += 1;
            tree.arena.nodes[i].size = 0;
            tree.arena.nodes[i].alloc = 0;
        } else {
            file_count += 1;
            total_size = total_size.saturating_add(n.size);
            total_alloc = total_alloc.saturating_add(n.alloc);
        }
    }

    // ② 清零目录文件计数
    for c in tree.dir_file_counts.iter_mut() {
        *c = 0;
    }

    // ③ 逐个文件沿父链上传
    for i in 0..len {
        let n = tree.arena.nodes[i];
        if !n.in_use() || n.is_dir() {
            continue;
        }
        propagate_delta(tree, i as u32, n.size as i64, n.alloc as i64, |t, anc| {
            let ai = anc as usize;
            if ai < t.dir_file_counts.len() {
                t.dir_file_counts[ai] = t.dir_file_counts[ai].saturating_add(1);
            }
        });
    }

    tree.file_count = file_count;
    tree.dir_count = dir_count;
    tree.total_size = total_size;
    tree.total_alloc = total_alloc;
    tree.finalized = true;
}

/// 带符号累加，下限钳到 0（防止浮点/异常 delta 把大小搞成负数）。
#[inline]
fn add_signed(v: u64, delta: i64) -> u64 {
    if delta >= 0 {
        v.saturating_add(delta as u64)
    } else {
        v.saturating_sub(delta.unsigned_abs())
    }
}

/// Top-N 大文件。用**有界最小堆**，内存与计算量与「文件总数」无关（文档 §5.1 性能要点 3）。
pub fn top_files(tree: &Tree, n: usize, mode: SizeMode) -> Vec<u32> {
    if n == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(n + 1);
    for (i, node) in tree.arena.nodes.iter().enumerate() {
        if !node.is_file() {
            continue;
        }
        let key = (mode.of(node), i as u32);
        if heap.len() < n {
            heap.push(Reverse(key));
        } else if let Some(&Reverse(min)) = heap.peek() {
            if key > min {
                heap.pop();
                heap.push(Reverse(key));
            }
        }
    }
    let mut v: Vec<u32> = heap.into_iter().map(|Reverse((_, i))| i).collect();
    v.sort_unstable_by(|&a, &b| {
        mode.of(&tree.arena.nodes[b as usize])
            .cmp(&mode.of(&tree.arena.nodes[a as usize]))
    });
    v
}

/// 某个目录下按大小降序的子节点（面积图与树形表都要用）。
pub fn sorted_children(tree: &Tree, children: &[u32], mode: SizeMode) -> Vec<u32> {
    let mut v: Vec<u32> = children.to_vec();
    v.sort_unstable_by(|&a, &b| {
        let sa = tree.arena.get(a).map(|n| mode.of(n)).unwrap_or(0);
        let sb = tree.arena.get(b).map(|n| mode.of(n)).unwrap_or(0);
        sb.cmp(&sa)
    });
    v
}

/// 按大小降序挑选，最多 `n` 个，其余合并为「其他」的合计值。
/// 面积图的 LOD 剪枝会用到（文档 §2.4）。
pub fn top_children_with_rest(
    tree: &Tree,
    children: &[u32],
    n: usize,
    mode: SizeMode,
) -> (Vec<u32>, u64) {
    let sorted = sorted_children(tree, children, mode);
    if sorted.len() <= n {
        return (sorted, 0);
    }
    let (head, tail) = sorted.split_at(n);
    let rest: u64 = tail
        .iter()
        .map(|&i| tree.arena.get(i).map(|x| mode.of(x)).unwrap_or(0))
        .sum();
    (head.to_vec(), rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{ChildIndex, Tree};
    use core_ntfs::filename::make_frn;
    use core_ntfs::record::MftRecord;

    fn mk(recno: u64, parent_frn: u64, name: &str, is_dir: bool, size: u64, alloc: u64) -> (MftRecord, Vec<u8>) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut buf = vec![0u8; 0x30 + 0x42 + units.len() * 2];
        let off = 0x30 + 0x42;
        for (i, u) in units.iter().enumerate() {
            buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        (
            MftRecord {
                frn: make_frn(recno, 1),
                recno,
                seq: 1,
                flags: 1 | if is_dir { 2 } else { 0 },
                hard_link_count: 1,
                parent_frn,
                namespace: 1,
                name_off: off as u32,
                name_chars: units.len() as u16,
                real_size: size,
                alloc_size: alloc,
                mtime: 0,
                fn_flags: 0,
                has_attr_list: false,
                resident_data: false,
                has_named_stream: false,
            },
            buf,
        )
    }

    /// root(5) ─┬─ dir(10) ─┬─ f(11) 1000/4096
    ///          │           └─ f(12) 2000/4096
    ///          └─ f(20) 5000/8192
    fn sample() -> Tree {
        let mut t = Tree::new("C:", 64);
        let root = make_frn(5, 1);
        for (recno, parent, name, is_dir, size, alloc) in [
            (5u64, root, ".", true, 0, 0),
            (10, root, "Windows", true, 0, 0),
            (11, make_frn(10, 1), "a.log", false, 1000, 4096),
            (12, make_frn(10, 1), "b.log", false, 2000, 4096),
            (20, root, "pagefile.sys", false, 5000, 8192),
        ] {
            let (r, b) = mk(recno, parent, name, is_dir, size, alloc);
            t.insert(&r, &b);
        }
        t
    }

    #[test]
    fn aggregates_directory_sizes_bottom_up() {
        let mut t = sample();
        aggregate(&mut t);

        assert_eq!(t.arena.get(11).unwrap().size, 1000);
        assert_eq!(t.arena.get(11).unwrap().alloc, 4096);
        // Windows 目录 = 1000 + 2000
        assert_eq!(t.arena.get(10).unwrap().size, 3000);
        assert_eq!(t.arena.get(10).unwrap().alloc, 8192);
        // root = 全部之和
        assert_eq!(t.arena.get(5).unwrap().size, 8000);
        assert_eq!(t.arena.get(5).unwrap().alloc, 16384);
    }

    #[test]
    fn counts_files_per_directory() {
        let mut t = sample();
        aggregate(&mut t);
        assert_eq!(t.dir_file_count(10), 2);
        assert_eq!(t.dir_file_count(5), 3); // 2 个直接 + ?
        assert_eq!(t.file_count, 3);
        assert_eq!(t.dir_count, 2);
        assert_eq!(t.total_size, 8000);
        assert_eq!(t.total_alloc, 16384);
    }

    #[test]
    fn aggregate_is_idempotent() {
        let mut t = sample();
        aggregate(&mut t);
        let s1 = t.arena.get(5).unwrap().size;
        aggregate(&mut t);
        let s2 = t.arena.get(5).unwrap().size;
        assert_eq!(s1, s2, "重复 aggregate 不应把大小累加成两倍");
    }

    #[test]
    fn propagate_delta_is_o_depth() {
        let mut t = sample();
        aggregate(&mut t);
        let before = t.arena.get(5).unwrap().size;
        // 文件 11 增长 500。
        // 契约（见 `propagate_delta` 的文档注释）：它只更新**祖先** ——
        // 「把 delta 从 start 的父节点起沿父链上传」。start 自身的大小由
        // 调用方负责：引擎里是先 `upsert_incremental` / `set_size` 再 propagate。
        let steps = propagate_delta(&mut t, 11, 500, 0, |_, _| {});
        assert_eq!(
            t.arena.get(11).unwrap().size,
            1000,
            "节点自身不由 propagate_delta 修改"
        );
        assert_eq!(t.arena.get(10).unwrap().size, 3500);
        assert_eq!(t.arena.get(5).unwrap().size, before + 500);
        assert_eq!(steps, 2, "11 → 10 → 5");
    }

    #[test]
    fn propagate_delta_handles_shrink() {
        let mut t = sample();
        aggregate(&mut t);
        // 同样是「只动祖先」：10 从 3000 降到 2000，根从 8000 降到 7000
        propagate_delta(&mut t, 11, -1000, 0, |_, _| {});
        assert_eq!(t.arena.get(10).unwrap().size, 2000);
        assert_eq!(t.arena.get(5).unwrap().size, 7000);
        assert_eq!(t.arena.get(11).unwrap().size, 1000, "节点自身不变");
    }

    #[test]
    fn propagate_delta_clamps_at_zero() {
        let mut t = sample();
        aggregate(&mut t);
        // 过度收缩必须夹到 0，绝不 u64 回绕成天文数字
        propagate_delta(&mut t, 11, -999_999, 0, |_, _| {});
        assert_eq!(t.arena.get(10).unwrap().size, 0);
        assert_eq!(t.arena.get(5).unwrap().size, 0);
    }

    #[test]
    fn callback_visits_each_ancestor_once() {
        let mut t = sample();
        aggregate(&mut t);
        let mut visited = Vec::new();
        propagate_delta(&mut t, 11, 1, 1, |_, anc| visited.push(anc));
        assert_eq!(visited, vec![10, 5]);
    }

    #[test]
    fn top_files_returns_sorted_desc() {
        let mut t = sample();
        aggregate(&mut t);
        let top = top_files(&t, 2, SizeMode::Logical);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0], 20); // 5000
        assert_eq!(top[1], 12); // 2000
    }

    #[test]
    fn top_files_by_allocated_differs_from_logical() {
        let mut t = sample();
        aggregate(&mut t);
        let top = top_files(&t, 3, SizeMode::Allocated);
        // 20 → 8192, 11/12 → 各 4096
        assert_eq!(top[0], 20);
        assert_eq!(top.len(), 3);
    }

    #[test]
    fn top_files_limited_n() {
        let mut t = sample();
        aggregate(&mut t);
        assert_eq!(top_files(&t, 1, SizeMode::Logical).len(), 1);
        assert_eq!(top_files(&t, 0, SizeMode::Logical).len(), 0);
        assert_eq!(top_files(&t, 100, SizeMode::Logical).len(), 3); // 只有 3 个文件
    }

    #[test]
    fn sorted_children_desc() {
        let mut t = sample();
        aggregate(&mut t);
        let ci = ChildIndex::build(&t);
        let kids = sorted_children(&t, ci.children(5), SizeMode::Logical);
        // **降序**：`sorted_children` 按 sb.cmp(&sa) 排，最大的在前
        // （`top_files` / `top_children_with_rest` 都依赖这个方向）
        assert_eq!(kids, vec![20, 10]); // pagefile 5000 > Windows 3000
    }

    // 名字不能叫 `top_children_with_rest` —— 那会遮蔽 `use super::*` 导入的
    // 同名辅助函数，导致函数体里的调用解析到「这个测试自己」并因参数不符报 E0061。
    #[test]
    fn top_children_with_rest_keeps_tail_sum() {
        let mut t = sample();
        aggregate(&mut t);
        let ci = ChildIndex::build(&t);
        let (head, rest) = top_children_with_rest(&t, ci.children(5), 1, SizeMode::Logical);
        assert_eq!(head, vec![20]);
        assert_eq!(rest, 3000);
    }
}
