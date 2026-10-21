//! 目录递归遍历 —— 降级路径（文档 §2.2 路径 C / 风险 R2、R12）。
//!
//! 何时走这条路：
//! * 非 NTFS 分区（exFAT / FAT32）；
//! * 网络盘 / 移动盘 / 光驱；
//! * **当前进程没有管理员权限**（读不了 `\\.\X:`）。
//!
//! 代价：比 MFT 直读慢 1~2 个数量级（1 TB 盘 5~30 分钟 vs 20~40 秒）。
//! 收益：**任何用户都能跑**，而且 `ReadDirectoryChangesW` 在同样权限下也能用，
//! 所以增量监控依然可用——只是慢一点。
//!
//! 这一路没有 FRN，因此节点下标改为「自增序号」；
//! 功能上等价，只是不能像 MFT 模式那样做 `FRN → 下标` 的 O(1) 定位。

use crate::dirwatch;
use core_index::aggregate;
use core_index::arena::{Node, NODE_DIR, NODE_IN_USE, NODE_REPARSE};
use core_index::Tree;
use core_io::volumes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct TraverseOptions {
    pub max_depth: u32,
    /// 不递归进入重解析点（符号链接 / 联接点），文档 §3.2
    pub skip_reparse: bool,
    pub progress_every: u64,
    /// 拿不到真实簇大小时的兜底值
    pub fallback_cluster_size: u64,
}

impl Default for TraverseOptions {
    fn default() -> Self {
        TraverseOptions {
            max_depth: 256,
            skip_reparse: true,
            progress_every: 20_000,
            fallback_cluster_size: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TraverseStats {
    pub dirs_visited: u64,
    pub files_visited: u64,
    /// 读目录/取元数据失败的次数（权限、占用、已删除等）
    pub errors: u64,
    pub skipped_reparse: u64,
    pub elapsed_ms: u128,
    pub total_size: u64,
    pub total_alloc: u64,
    pub max_depth_reached: u32,
    pub cluster_size: u64,
}

impl TraverseStats {
    pub fn summary_cn(&self) -> String {
        format!(
            "目录遍历完成：{} 个目录 / {} 个文件，跳过重解析点 {}，错误 {}，耗时 {} ms（{}）",
            self.dirs_visited,
            self.files_visited,
            self.skipped_reparse,
            self.errors,
            self.elapsed_ms,
            core_io::util::human_size(self.total_size)
        )
    }
}

/// 用目录递归遍历做全量扫描（降级模式）。
///
/// `progress(done_nodes, 0)` —— 这一路预先不知道总数，第二个参数恒为 0。
///
/// 不收集路径索引；需要配合 [`crate::watcher`] 做实时增量时请用
/// [`full_scan_traverse_indexed`]。
pub fn full_scan_traverse<P>(
    root: &Path,
    drive: &str,
    opts: TraverseOptions,
    progress: P,
) -> (Tree, TraverseStats)
where
    P: FnMut(u64, u64),
{
    let (tree, stats, _) = full_scan_traverse_indexed(root, drive, opts, false, progress);
    (tree, stats)
}

/// 与 [`full_scan_traverse`] 相同，但可选地顺手产出 **「小写绝对路径 → 节点下标」** 索引。
///
/// 为什么必须在这里产出：降级模式的节点下标是遍历时自增分配的，与磁盘上的
/// 任何标识都没有关系；`ReadDirectoryChangesW` 只给路径字符串。
/// 若事后用 [`Tree::path_of`] 反推，路径前缀是**盘符**（`C:\...`）而不是
/// 真实扫描根（可能是 `C:\Users\x\Downloads`），拼出来是错的。
/// 遍历时我们手里正好有 `entry.path()`，此时收集是零成本的。
pub fn full_scan_traverse_indexed<P>(
    root: &Path,
    drive: &str,
    opts: TraverseOptions,
    collect_paths: bool,
    progress: P,
) -> (Tree, TraverseStats, HashMap<Box<str>, u32>)
where
    P: FnMut(u64, u64),
{
    full_scan_traverse_indexed_with(root, drive, opts, collect_paths, progress, None)
}

/// 每访问多少个**目录**回调一次半成品视图（E7）。
///
/// 遍历比 MFT 慢一到两个数量级，按"目录数"而不是"条目数"计数：
/// 一个目录里的条目再多也是一次 `read_dir` 的产物，界面上看不出差别。
pub const PARTIAL_STRIDE_DIRS: u64 = 400;

/// [`full_scan_traverse_indexed`] + 半成品视图回调（E7）。
pub fn full_scan_traverse_indexed_with<P>(
    root: &Path,
    drive: &str,
    opts: TraverseOptions,
    collect_paths: bool,
    mut progress: P,
    partial: Option<&crate::session::PartialSink>,
) -> (Tree, TraverseStats, HashMap<Box<str>, u32>)
where
    P: FnMut(u64, u64),
{
    let t0 = Instant::now();
    let cluster = volumes::cluster_size(drive).unwrap_or(opts.fallback_cluster_size);
    let mut stats = TraverseStats {
        cluster_size: cluster,
        ..Default::default()
    };
    let mut paths: HashMap<Box<str>, u32> = HashMap::new();

    let mut tree = Tree::new(drive, 1 << 16);
    const ROOT_IDX: u32 = 5;

    tree.arena.ensure(ROOT_IDX);
    tree.dir_file_counts.resize(ROOT_IDX as usize + 1, 0);
    {
        let off = tree.names.intern_str(".");
        tree.arena.nodes[ROOT_IDX as usize] = Node {
            parent: ROOT_IDX,
            seq: 0,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: off,
        };
    }
    if collect_paths {
        let k = dirwatch::normalize_root(&root.to_string_lossy()).to_lowercase();
        paths.insert(k.into_boxed_str(), ROOT_IDX);
    }

    let mut next_idx: u32 = ROOT_IDX + 1;
    let mut stack: Vec<(PathBuf, u32, u32)> = vec![(root.to_path_buf(), ROOT_IDX, 0)];
    let every = opts.progress_every.max(1);
    let mut next_progress_at = ROOT_IDX as u64 + every;
    let mut next_partial_at = PARTIAL_STRIDE_DIRS;

    while let Some((dir, parent_idx, depth)) = stack.pop() {
        if depth > opts.max_depth {
            continue;
        }
        stats.dirs_visited += 1;
        stats.max_depth_reached = stats.max_depth_reached.max(depth);

        // E7：每访问若干个目录回调一次。`tick` 内部先聚合再回调 ——
        // 遍历过程中目录节点的 size 还是 0，不聚合就画出来是一屏空方块。
        if let Some(sink) = partial {
            sink.tick(
                &mut tree,
                stats.dirs_visited as u64,
                &mut next_partial_at,
                PARTIAL_STRIDE_DIRS,
            );
        }

        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };

            // 平台差异收在 `core_io::fsmeta` 里：Windows 读 `dwFileAttributes`，
            // Linux 由 `st_mode` 合成同名位（见该模块的说明）。
            let is_reparse = core_io::fsmeta::is_reparse(&md);
            let is_dir = md.is_dir();

            if is_dir && is_reparse && opts.skip_reparse {
                stats.skipped_reparse += 1;
                continue;
            }
            if next_idx == u32::MAX {
                break;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let idx = next_idx;
            next_idx += 1;

            tree.arena.ensure(idx);
            if idx as usize >= tree.dir_file_counts.len() {
                tree.dir_file_counts.resize(idx as usize + 1, 0);
            }

            let mut flags = NODE_IN_USE;
            if is_dir {
                flags |= NODE_DIR;
            }
            if is_reparse {
                flags |= NODE_REPARSE;
            }
            // 降级路径**不置 NODE_HARDLINKED**：
            //   `std::os::windows::fs::MetadataExt::number_of_links` 至今仍是
            //   unstable（feature `windows_by_handle`，见 rustc E0658 / issue #63010），
            //   而便宜的取链接数只有它这一条路 —— `GetFileInformationByHandle`
            //   需要给每个文件开一次句柄，200 万文件规模下不可接受。
            //   代价：同一份数据被多个硬链接指向时，降级模式会重复计入总大小
            //   （做硬链接去重需要 `--admin` 走 MFT 直读路径，那里能正确识别）。
            //   这是降级路径的已知精度取舍，README「已知限制」有列出。

            let logical = if is_dir { 0 } else { md.len() };
            // 占用大小：Linux 用内核给的 `st_blocks`（稀疏文件会小于逻辑大小，
            // 这正是要如实呈现的）；Windows 没有等价字段，按簇向上取整估算。
            let alloc = if is_dir {
                0
            } else {
                core_io::fsmeta::allocated(&md, logical, cluster)
            };
            let mtime = core_io::fsmeta::mtime_unix(&md);

            let noff = tree.names.intern_str(&name);
            tree.arena.nodes[idx as usize] = Node {
                parent: parent_idx,
                seq: 0,
                flags,
                size: logical,
                alloc,
                mtime,
                name: noff,
            };

            if collect_paths {
                // 键统一小写：Windows 路径比较不区分大小写，而通知里的
                // 大小写与用户创建时不一定一致
                let k = path.to_string_lossy().to_lowercase();
                paths.insert(k.into_boxed_str(), idx);
            }

            if is_dir {
                stack.push((path, idx, depth + 1));
            } else {
                stats.files_visited += 1;
                stats.total_size = stats.total_size.saturating_add(logical);
                stats.total_alloc = stats.total_alloc.saturating_add(alloc);
            }

            if next_idx as u64 >= next_progress_at {
                progress(next_idx as u64, 0);
                next_progress_at = next_idx as u64 + every;
            }
        }
    }

    aggregate::aggregate(&mut tree);
    core_index::tree::mark_orphans(&mut tree);

    stats.elapsed_ms = t0.elapsed().as_millis();
    progress(next_idx as u64, 0);
    (tree, stats, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对齐口径只有一份实现（`core_io::util::align_up`），这里只做存在性检查：
    /// 遍历的占用估算与快照/重复文件用的是同一个函数。
    #[test]
    fn cluster_alignment_uses_the_shared_helper() {
        assert_eq!(core_io::util::align_up(1, 4096), 4096);
        assert_eq!(core_io::util::align_up(4097, 4096), 8192);
    }

    #[test]
    fn traverse_real_temp_dir() {
        // 在临时目录里造一棵小树，验证遍历路径本身能跑通
        let owned = core_io::test_dir::TestDir::new("fw-traverse").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.txt"), vec![b'x'; 5000]).unwrap();
        std::fs::write(base.join("sub").join("b.bin"), vec![b'y'; 10]).unwrap();

        let (tree, stats) = full_scan_traverse(&base, "C:", TraverseOptions::default(), |_, _| {});

        assert!(stats.dirs_visited >= 2, "至少根 + sub");
        assert_eq!(stats.files_visited, 2);
        assert_eq!(stats.total_size, 5010);
        assert_eq!(tree.file_count, 2);
        assert!(tree.arena.get(5).unwrap().is_dir());
        // 根的大小应聚合到 5010
        assert_eq!(tree.arena.get(5).unwrap().size, 5010);

    }

    #[test]
    fn traverse_reports_allocated_larger_than_logical() {
        let owned = core_io::test_dir::TestDir::new("fw-traverse-alloc").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("small.txt"), b"hi").unwrap();

        let (tree, stats) = full_scan_traverse(&base, "C:", TraverseOptions::default(), |_, _| {});
        assert_eq!(stats.total_size, 2);
        assert!(stats.total_alloc >= 2, "占用大小不应小于逻辑大小");
        assert_eq!(stats.total_alloc % stats.cluster_size, 0);
        assert_eq!(tree.arena.get(5).unwrap().size, 2);

    }

    #[test]
    fn missing_root_does_not_panic() {
        let bogus = PathBuf::from(r"Z:\definitely\not\here\at\all");
        let (tree, stats) = full_scan_traverse(&bogus, "Z:", TraverseOptions::default(), |_, _| {});
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.files_visited, 0);
        // 根节点仍然存在
        assert!(tree.arena.get(5).unwrap().is_dir());
    }

    #[test]
    fn progress_callback_is_invoked() {
        let owned = core_io::test_dir::TestDir::new("fw-traverse-prog").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(&base).unwrap();
        for i in 0..30 {
            std::fs::write(base.join(format!("f{}.txt", i)), b"x").unwrap();
        }

        let mut calls = 0usize;
        let opts = TraverseOptions {
            progress_every: 5,
            ..Default::default()
        };
        full_scan_traverse(&base, "C:", opts, |_, _| calls += 1);
        assert!(calls >= 6, "至少每 5 个节点回调一次，实际 {}", calls);

    }

    #[test]
    fn depth_limit_stops_recursion() {
        let owned = core_io::test_dir::TestDir::new("fw-traverse-depth").unwrap();
        let base = owned.path().to_path_buf();
        let deep = base.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.txt"), b"z").unwrap();

        let opts = TraverseOptions {
            max_depth: 1,
            ..Default::default()
        };
        let (_, stats) = full_scan_traverse(&base, "C:", opts, |_, _| {});
        assert_eq!(stats.files_visited, 0, "超过深度限制的文件不应被统计");

    }
}
