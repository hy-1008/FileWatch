//! 重复文件去重：**(体积 → 部分哈希 → 全哈希)** 三级筛选 + 逐字节复核。
//!
//! 对应文档 §3.1 的 F12 与 §3.2 的去重规则。CLI 的 `dupes` 与界面的
//! 「重复文件」视图共用这一份实现 —— 同一个目录在两边给出的可回收空间
//! 必须一致，不一致就是 bug，不是「口径不同」。
//!
//! # 为什么要三级，而不是「直接全量哈希」
//!
//! 全盘哈希是**读整个磁盘**，在机械盘上要几个小时。三级筛选把绝大多数文件
//! 在**不读内容**的阶段就排除掉：
//!
//! | 级 | 判据 | 需要读的字节数 |
//! |---|---|---|
//! | ① 体积 | 体积唯一的文件不可能有重复 | **0** |
//! | ② 部分哈希 | 首尾各 4 KiB（不足 8 KiB 的文件直接整读） | O(8 KiB) |
//! | ③ 全哈希 | 整文件 | O(文件大小) |
//! | ④ 逐字节复核 | 只有真正撞上哈希的少数文件 | O(文件大小) |
//!
//! 现实里的分布是：一级就能筛掉 95% 以上（体积几乎总是唯一的），
//! 二级再筛掉大头。所以磁盘上真正被完整读一遍的，只有那些**真的重复**的文件。
//!
//! # 为什么最后一定要逐字节复核
//!
//! 前三级的判据都是「哈希相等」。任何 64 位哈希都有碰撞可能，而这里的后果不是
//! 「少报一对重复」，而是**让用户去删一个并不重复的文件**。所以第 ④ 级不是
//! 优化，是正确性要求：它让"这两个文件内容**确定**一样"这句话成立，
//! 也正因为有它，前面才能放心用非加密哈希（见 `core_io::hash` 的模块文档）。
//!
//! # 四个按文档约定排除的情况
//!
//! 1. **0 字节文件**：任何两个空文件内容都相同。真把它们全部列成一组，
//!    在有一堆 `.gitkeep` / 占位文件的目录里会得到一条几百项的"重复"记录，
//!    除了噪声没有任何价值。数量单独统计（`skipped_empty`）。
//! 2. **重解析点 / 符号链接**：不跟随（文档 §3.2）。跟随的话，两个指向同一
//!    目标的链接会被算成"重复"，而删掉其中任何一个都只是在删链接。
//! 3. **硬链接**：**不需要在这里处理**。索引里一条 FRN 只有一个节点，
//!    天然只出现一次 —— 硬链接的去重发生在建索引时（文档 §3.2）。
//!    这是「复用同一棵索引树」的直接好处。
//! 4. **读不到的文件**（权限 / 被独占 / 中途被删）：从成组中剔除并计数，
//!    **绝不**当成"内容未知所以算一样"。`unreadable` 会出现在报告里，
//!    否则用户会以为「可回收空间」是个精确值。
//!
//! # 稀疏文件的一个已知偏差
//!
//! 稀疏文件读出来是一大段 0。两个"体积相同、内容都是 0"的稀疏文件会被判为重复，
//! 但它们的**实际占用**可能都很小，删掉一个也回收不了多少。报告里的
//! `wasted` 按**逻辑大小**计算，所以这条偏大。要精确的话得按占用大小算，
//! 但那会把「逻辑相同、占用不同」的同一份数据排除掉 —— 两难，择其一并写明。

use core_index::arena::NODE_REPARSE;
use core_index::Tree;
use core_io::hash::PARTIAL_BLOCK;
use core_monitor::PathRemap;
use std::collections::HashMap;
use std::io;
use std::path::Path;

// ── 候选与诊断 ──────────────────────────────────────────────────────────────

/// 一个参与比较的文件。
///
/// 只存**去重真正需要**的三样东西：路径（拿去读）、体积（一级筛）、修改时间
/// （报告里区分"谁新谁旧"）。不存 node 下标 —— 候选集会被交给后台线程，
/// 而索引树随时可能被监控改掉，下标在那里就失效了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupCandidate {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

/// 从索引树里挑出的候选集，附带各项排除计数。
#[derive(Debug, Default, Clone)]
pub struct CandidateSet {
    pub items: Vec<DupCandidate>,
    /// 小于 `min_size` 而排除
    pub skipped_small: u64,
    /// 0 字节而排除
    pub skipped_empty: u64,
    /// 重解析点（符号链接等）而排除
    pub skipped_reparse: u64,
    /// 因为候选数超过上限而**截断**掉的文件数
    pub truncated: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupOptions {
    /// 小于这个体积的文件不参与比较
    pub min_size: u64,
    /// 二级筛「部分哈希」读首尾各多少字节
    pub block: usize,
    /// 单组最多列出多少个文件（真实数量仍如实计在 `count` 里）
    pub max_per_group: usize,
    /// 最多输出多少组（按可回收空间降序截断）
    pub max_groups: usize,
    /// 候选数上限。0 = 不限。
    ///
    /// 存在的理由：候选集是**字符串**的集合，200 万个路径就是 200 MB 上下。
    /// 界面默认 `min_size = 1 MiB` 时通常只有几千条，但把这个旋钮交给用户
    /// （`--min-size=0`）之后就要有个闸门。
    pub max_candidates: usize,
}

impl Default for DupOptions {
    fn default() -> Self {
        DupOptions {
            // 1 字节起：不擅自替用户决定「多小算无所谓」
            min_size: 1,
            block: PARTIAL_BLOCK,
            max_per_group: 50,
            max_groups: 500,
            max_candidates: 0,
        }
    }
}

/// 一组内容完全相同的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupGroup {
    /// 每个文件的体积（组内相同）
    pub size: u64,
    /// 组内文件**实际**数量（可能大于 `files.len()`）
    pub count: u64,
    /// 删到只剩一份能回收的字节数 = `size * (count - 1)`
    pub wasted: u64,
    /// 具体文件（最多 [`DupOptions::max_per_group`] 个，按路径排序）
    pub files: Vec<DupCandidate>,
    /// `files` 是否被截断过
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DupReport {
    /// 真正进入比较的文件数
    pub considered: u64,
    pub skipped_small: u64,
    pub skipped_empty: u64,
    pub skipped_reparse: u64,
    pub truncated: u64,
    /// 一级筛出的「体积相同」组数
    pub same_size_groups: u64,
    /// 一级筛后剩下的文件数
    pub same_size_files: u64,
    /// 二级筛后剩下的文件数
    pub partial_files: u64,
    /// 三级筛后剩下的文件数
    pub full_files: u64,
    /// 读取失败而剔除的文件数（权限 / 被占用 / 中途被删）
    pub unreadable: u64,
    /// 输出的组数（截断前）
    pub group_count: u64,
    pub groups: Vec<DupGroup>,
    /// 全部组一共能回收多少字节
    pub total_wasted: u64,
    /// 实际读了多少字节（用来解释"为什么慢了"）
    pub bytes_hashed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupStage {
    Partial,
    Full,
    Verify,
}

impl DupStage {
    pub fn label_cn(self) -> &'static str {
        match self {
            DupStage::Partial => "部分哈希",
            DupStage::Full => "完整哈希",
            DupStage::Verify => "逐字节复核",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DupProgress {
    pub stage: DupStage,
    pub done: u64,
    pub total: u64,
}

// ── 内容来源 ────────────────────────────────────────────────────────────────

/// 「怎么把一个文件的内容拿来做判据」。
///
/// 抽成 trait 而不是直接读盘，是为了**能在纯内存里单测三级筛选本身**：
/// 去重算法的分支（体积相同但内容不同、内容相同但体积不同、读失败怎么算……）
/// 用真实文件摆起来又慢又难构造，而用内存实现几行就能铺满。
/// 这与 `core-monitor` 把「回读大小」做成闭包注入是同一个理由。
pub trait ContentSource {
    /// 首尾块哈希。文件不大于 `2 * block` 时应退化为整文件哈希。
    fn partial_hash(&self, c: &DupCandidate, block: usize) -> io::Result<u64>;
    /// 整文件哈希。
    fn full_hash(&self, c: &DupCandidate) -> io::Result<u64>;
    /// 两个文件是否逐字节相同。
    fn equal(&self, a: &str, b: &str) -> io::Result<bool>;
}

/// 真实文件系统。
#[derive(Debug, Default, Clone, Copy)]
pub struct FsSource;

impl ContentSource for FsSource {
    fn partial_hash(&self, c: &DupCandidate, block: usize) -> io::Result<u64> {
        core_io::hash::hash_head_tail(Path::new(&c.path), block)
    }
    fn full_hash(&self, c: &DupCandidate) -> io::Result<u64> {
        core_io::hash::hash_file(Path::new(&c.path))
    }
    fn equal(&self, a: &str, b: &str) -> io::Result<bool> {
        core_io::hash::files_equal(Path::new(a), Path::new(b))
    }
}

// ── 从索引树收集候选 ────────────────────────────────────────────────────────

/// 遍历索引树，挑出可能重复的文件（**文件**，且有体积）。
///
/// 路径由 `Tree::path_of` 拼出（前缀恒为盘符），再过 [`PathRemap`] 换成用户
/// 真正扫描的那个目录 —— 这两步与变更流水、Top-N 用的是同一套规则，
/// 所以界面里「重复文件」列出的路径与「变更流水」里的同一条路径会完全一致。
pub fn collect_candidates(
    tree: &Tree,
    remap: Option<&PathRemap>,
    opts: &DupOptions,
) -> CandidateSet {
    let mut set = CandidateSet::default();
    for (i, node) in tree.arena.nodes.iter().enumerate() {
        if !node.in_use() || node.is_dir() {
            continue;
        }
        if node.flags & NODE_REPARSE != 0 {
            set.skipped_reparse += 1;
            continue;
        }
        if node.size == 0 {
            set.skipped_empty += 1;
            continue;
        }
        if node.size < opts.min_size {
            set.skipped_small += 1;
            continue;
        }
        if opts.max_candidates != 0 && set.items.len() >= opts.max_candidates {
            set.truncated += 1;
            continue;
        }
        let raw = tree.path_of(i as u32);
        let path = match remap {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        set.items.push(DupCandidate {
            path,
            size: node.size,
            mtime: node.mtime,
        });
    }
    set
}

// ── 三级筛选 ────────────────────────────────────────────────────────────────

/// 逐级筛选的进度状态。`done` 在每一级开始时归零，`total` 是**该级**的待办数。
struct NarrowState {
    done: u64,
    total: u64,
    unreadable: u64,
    bytes: u64,
}

/// 按「哈希」分桶，只留下桶内多于一个的组。
///
/// 三个判据（部分哈希 / 整文件哈希）共用这一份实现 —— 否则同一段
/// 「读失败就剔除」的逻辑会散在三处，将来改一处忘一处。
fn narrow<F>(
    items: &[DupCandidate],
    stage: DupStage,
    key: &mut F,
    state: &mut NarrowState,
    progress: &mut dyn FnMut(DupProgress),
) -> Vec<Vec<DupCandidate>>
where
    F: FnMut(&DupCandidate, &mut NarrowState) -> io::Result<u64>,
{
    let mut map: HashMap<u64, Vec<DupCandidate>> = HashMap::new();
    state.done = 0;
    state.total = items.len() as u64;
    for c in items {
        match key(c, state) {
            Ok(h) => map.entry(h).or_default().push(c.clone()),
            Err(_) => state.unreadable += 1,
        }
        state.done += 1;
        progress(DupProgress {
            stage,
            done: state.done,
            total: state.total,
        });
    }
    let mut out: Vec<Vec<DupCandidate>> = map.into_values().filter(|v| v.len() > 1).collect();
    // `HashMap` 的遍历顺序是随机的：不排序的话，同样的输入两次跑出来的
    // 组顺序都不一样（组内部还要靠 `[0]` 当代表做复核，顺序会影响复核次数）。
    out.sort_by(|a, b| a[0].path.cmp(&b[0].path));
    out
}

/// 逐字节复核：把「哈希相同」的一组拆成「内容确定相同」的若干簇。
fn cluster_by_content(
    group: &[DupCandidate],
    src: &dyn ContentSource,
    state: &mut NarrowState,
    progress: &mut dyn FnMut(DupProgress),
) -> Vec<Vec<DupCandidate>> {
    let mut clusters: Vec<Vec<DupCandidate>> = Vec::new();
    for c in group {
        let mut placed = false;
        for cl in clusters.iter_mut() {
            match src.equal(&cl[0].path, &c.path) {
                Ok(true) => {
                    cl.push(c.clone());
                    placed = true;
                    break;
                }
                Ok(false) => {}
                Err(_) => {
                    state.unreadable += 1;
                    placed = true;
                    break;
                }
            }
        }
        if !placed {
            clusters.push(vec![c.clone()]);
        }
        state.done += 1;
        progress(DupProgress {
            stage: DupStage::Verify,
            done: state.done,
            total: state.total,
        });
    }
    let mut out: Vec<Vec<DupCandidate>> = clusters.into_iter().filter(|c| c.len() > 1).collect();
    out.sort_by(|a, b| a[0].path.cmp(&b[0].path));
    out
}

/// 跑完三级筛选 + 复核，返回报告。
///
/// `progress` 在**每个文件**处理完时被调用一次。CLI/界面自己决定要不要节流
/// （界面是 60 ms 一条），这里不替它们节流 —— 否则 CLI 的进度条会一卡一卡的。
pub fn find_duplicates(
    entries: &[DupCandidate],
    opts: &DupOptions,
    src: &dyn ContentSource,
    progress: &mut dyn FnMut(DupProgress),
) -> DupReport {
    let mut rep = DupReport::default();
    let mut state = NarrowState {
        done: 0,
        total: 0,
        unreadable: 0,
        bytes: 0,
    };

    // ── ① 体积 ──
    let mut by_size: HashMap<u64, Vec<DupCandidate>> = HashMap::new();
    for e in entries {
        if e.size == 0 {
            rep.skipped_empty += 1;
            continue;
        }
        if e.size < opts.min_size {
            rep.skipped_small += 1;
            continue;
        }
        by_size.entry(e.size).or_default().push(e.clone());
    }
    let mut stage1: Vec<Vec<DupCandidate>> = by_size
        .into_values()
        .filter(|v| v.len() > 1)
        .collect();
    stage1.sort_by(|a, b| a[0].path.cmp(&b[0].path));
    rep.considered = stage1.iter().map(|g| g.len() as u64).sum();
    rep.same_size_groups = stage1.len() as u64;
    rep.same_size_files = rep.considered;

    // ── ② 部分哈希 ──
    let flat1: Vec<DupCandidate> = stage1.into_iter().flatten().collect();
    let stage2 = narrow(
        &flat1,
        DupStage::Partial,
        &mut |c, st| {
            let r = src.partial_hash(c, opts.block);
            if r.is_ok() {
                // 部分哈希最多读 2 个块（不足则整读）
                st.bytes += c.size.min(2 * opts.block as u64);
            }
            r
        },
        &mut state,
        progress,
    );
    rep.partial_files = stage2.iter().map(|g| g.len() as u64).sum();

    // ── ③ 整文件哈希 ──
    let flat2: Vec<DupCandidate> = stage2.into_iter().flatten().collect();
    let stage3 = narrow(
        &flat2,
        DupStage::Full,
        &mut |c, st| {
            let r = src.full_hash(c);
            if r.is_ok() {
                st.bytes += c.size;
            }
            r
        },
        &mut state,
        progress,
    );
    rep.full_files = stage3.iter().map(|g| g.len() as u64).sum();

    // ── ④ 逐字节复核 ──
    let mut verified: Vec<Vec<DupCandidate>> = Vec::new();
    for g in &stage3 {
        state.done = 0;
        state.total = g.len() as u64;
        verified.extend(cluster_by_content(g, src, &mut state, progress));
    }
    rep.unreadable = state.unreadable;
    rep.bytes_hashed = state.bytes;

    // ── 汇总成组 ──
    let mut groups: Vec<DupGroup> = verified
        .into_iter()
        .map(|mut files| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let count = files.len() as u64;
            let size = files[0].size;
            let truncated = count > opts.max_per_group as u64;
            if truncated {
                files.truncate(opts.max_per_group);
            }
            DupGroup {
                size,
                count,
                wasted: size.saturating_mul(count - 1),
                files,
                truncated,
            }
        })
        .collect();
    // 排序：可回收空间降序 → 单文件体积降序 → 首路径升序。
    // 第三项是为了**确定性**：前两项相同的组（比如一堆同样大小的重复小文件）
    // 必须有稳定的先后，否则同一份扫描两次导出的 CSV 行序不同，没法 diff。
    groups.sort_by(|a, b| {
        b.wasted
            .cmp(&a.wasted)
            .then(b.size.cmp(&a.size))
            .then(a.files[0].path.cmp(&b.files[0].path))
    });
    rep.group_count = groups.len() as u64;
    rep.total_wasted = groups.iter().map(|g| g.wasted).sum();

    // 总量按**截断前**的组算：用户看到的「可回收空间」不该因为只显示了
    // 前 500 组而变小 —— 那会让人以为多出来的组不存在。
    if groups.len() > opts.max_groups {
        groups.truncate(opts.max_groups);
    }
    rep.groups = groups;
    rep
}

/// 报告的中文一行摘要（CLI 与界面状态栏共用同一句话）。
///
/// 措辞刻意写成一条"漏斗"：每一级还剩多少、最后确认了多少。用户看到
/// 「读了几 GB 却只找出 3 组」时，这句话就是解释。
pub fn summary_cn(rep: &DupReport) -> String {
    format!(
        "体积相同的文件 {} 个（{} 组）→ 部分哈希后 {} → 完整哈希后 {}，\
         逐字节复核确认重复 {} 组 / 可回收 {}",
        rep.same_size_files,
        rep.same_size_groups,
        rep.partial_files,
        rep.full_files,
        rep.group_count,
        core_io::util::human_size(rep.total_wasted)
    )
}

/// 重复组的 CSV（CLI 与界面共用，避免两处各写一份列头）。
pub fn groups_to_csv(groups: &[DupGroup]) -> String {
    let mut out = String::from("group,size,count,wasted,path\n");
    for (i, g) in groups.iter().enumerate() {
        for f in &g.files {
            out.push_str(&format!(
                "{},{},{},{},{}\n",
                i + 1,
                g.size,
                g.count,
                g.wasted,
                csv_field(&f.path)
            ));
        }
    }
    out
}

/// CSV 字段转义：含 `,` `"` 或换行的路径要用双引号包起来，内部引号翻倍。
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
    use std::collections::HashMap as Map;

    /// 内存内容源：路径 → 内容。
    ///
    /// 关键能力是能**故意制造哈希碰撞**（见 [`CollidingSource`]）——
    /// 这在真实文件系统上几乎造不出来，而「碰撞时必须靠逐字节复核兜住」
    /// 恰恰是这段代码最要紧的一条不变式。
    struct MemSource {
        files: Map<String, Vec<u8>>,
        /// 哪些路径读取会失败
        broken: Vec<String>,
    }

    impl MemSource {
        fn new(pairs: &[(&str, &[u8])]) -> MemSource {
            MemSource {
                files: pairs
                    .iter()
                    .map(|(p, d)| (p.to_string(), d.to_vec()))
                    .collect(),
                broken: Vec::new(),
            }
        }
        fn with_broken(mut self, paths: &[&str]) -> MemSource {
            self.broken = paths.iter().map(|s| s.to_string()).collect();
            self
        }
        fn get(&self, p: &str) -> io::Result<&Vec<u8>> {
            if self.broken.iter().any(|b| b == p) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "模拟读取失败"));
            }
            self.files
                .get(p)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有这个文件"))
        }
    }

    impl ContentSource for MemSource {
        fn partial_hash(&self, c: &DupCandidate, block: usize) -> io::Result<u64> {
            // 刻意调用**引擎里那个**判据，而不是在这里重写一遍首尾逻辑：
            // 重写的话，内存实现与磁盘实现一旦对"小文件算不算整读"理解不同，
            // 测试全绿而真机行为不同，症状只有"某些重复文件不成组"。
            Ok(core_io::hash::hash_head_tail_bytes(self.get(&c.path)?, block))
        }
        fn full_hash(&self, c: &DupCandidate) -> io::Result<u64> {
            let data = self.get(&c.path)?;
            core_io::hash::hash_reader(&mut data.as_slice())
        }
        fn equal(&self, a: &str, b: &str) -> io::Result<bool> {
            Ok(self.get(a)? == self.get(b)?)
        }
    }

    /// 所有文件都返回**同一个**哈希，但 `equal` 说实话。
    /// 用来证明「哈希撞车不会导致误报重复」。
    struct CollidingSource(MemSource);

    impl ContentSource for CollidingSource {
        fn partial_hash(&self, _c: &DupCandidate, _b: usize) -> io::Result<u64> {
            Ok(0xDEAD_BEEF)
        }
        fn full_hash(&self, _c: &DupCandidate) -> io::Result<u64> {
            Ok(0xDEAD_BEEF)
        }
        fn equal(&self, a: &str, b: &str) -> io::Result<bool> {
            self.0.equal(a, b)
        }
    }

    fn cand(path: &str, size: u64) -> DupCandidate {
        DupCandidate {
            path: path.to_string(),
            size,
            mtime: 0,
        }
    }

    fn opts(mut f: impl FnMut(&mut DupOptions)) -> DupOptions {
        let mut o = DupOptions::default();
        f(&mut o);
        o
    }

    fn run(entries: &[DupCandidate], src: &dyn ContentSource, o: &DupOptions) -> DupReport {
        find_duplicates(entries, o, src, &mut |_| {})
    }

    // ── 一级：体积 ──────────────────────────────────────────────────────────

    #[test]
    fn unique_sizes_are_never_read() {
        // 三个体积互不相同的文件：内容源里根本没有它们 —— 一旦去读就会报错，
        // 于是「没有 unreadable」等价于「一个字节都没读」。
        let src = MemSource::new(&[]);
        let e = [cand("a", 10), cand("b", 20), cand("c", 30)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.same_size_groups, 0);
        assert_eq!(rep.considered, 0);
        assert_eq!(rep.unreadable, 0, "体积唯一的文件不该被读");
        assert_eq!(rep.bytes_hashed, 0);
    }

    #[test]
    fn empty_files_are_skipped_on_purpose() {
        let src = MemSource::new(&[("a", b""), ("b", b"")]);
        let e = [cand("a", 0), cand("b", 0), cand("c", 0)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.skipped_empty, 3);
        assert!(rep.groups.is_empty(), "空文件不参与成组");
        assert_eq!(rep.unreadable, 0);
    }

    #[test]
    fn min_size_filters_small_files() {
        let src = MemSource::new(&[("small1", b"xx"), ("small2", b"xx")]);
        let e = [cand("small1", 2), cand("small2", 2)];
        let o = opts(|o| o.min_size = 1024);
        let rep = run(&e, &src, &o);
        assert_eq!(rep.skipped_small, 2);
        assert!(rep.groups.is_empty());
    }

    // ── 二级 / 三级 ─────────────────────────────────────────────────────────

    #[test]
    fn same_size_but_different_content_is_not_a_duplicate() {
        let src = MemSource::new(&[("a", b"AAAAAAAA"), ("b", b"BBBBBBBB")]);
        let e = [cand("a", 8), cand("b", 8)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.same_size_groups, 1);
        assert_eq!(rep.partial_files, 0, "内容不同，第二级就该被筛掉");
        assert!(rep.groups.is_empty());
    }

    /// 中段不同、首尾相同 → 二级筛看不出来，**三级必须看出来**。
    /// 这条是「为什么不能只有两级」的直接证据。
    #[test]
    fn middle_only_difference_is_caught_by_the_full_hash() {
        let n = PARTIAL_BLOCK * 4;
        let mut d1 = vec![0u8; n];
        let mut d2 = vec![0u8; n];
        d2[n / 2] = 9;
        d1[0] = 1;
        d2[0] = 1;
        let src = MemSource::new(&[("a", &d1), ("b", &d2)]);
        let e = [cand("a", d1.len() as u64), cand("b", d2.len() as u64)];
        let rep = run(&e, &src, &DupOptions::default());
        // 二级把两个都留下了（首尾一样）
        assert!(rep.partial_files >= 2, "首尾相同，二级不应筛掉它们");
        // 三级把它们分开了
        assert!(rep.groups.is_empty(), "中段不同 → 不是重复");
    }

    #[test]
    fn identical_files_are_grouped() {
        let src = MemSource::new(&[("a", b"same bytes"), ("b", b"same bytes"), ("c", b"other!")]);
        let e = [cand("a", 10), cand("b", 10), cand("c", 6)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.groups.len(), 1);
        let g = &rep.groups[0];
        assert_eq!(g.count, 2);
        assert_eq!(g.size, 10);
        assert_eq!(g.wasted, 10, "删到只剩一份回收 10 字节");
        assert_eq!(
            g.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    /// 三份相同 → 可回收 = 2 × size，不是 3 × size（不能把要保留的那份也算成垃圾）。
    #[test]
    fn wasted_counts_all_but_one_copy() {
        let src = MemSource::new(&[("a", b"xyz"), ("b", b"xyz"), ("c", b"xyz")]);
        let e = [cand("a", 3), cand("b", 3), cand("c", 3)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.groups[0].count, 3);
        assert_eq!(rep.groups[0].wasted, 6);
        assert_eq!(rep.total_wasted, 6);
    }

    // ── 复核 ────────────────────────────────────────────────────────────────

    /// **本模块最要紧的一条**：哈希全撞上时，只有逐字节复核能挡住误报。
    #[test]
    fn hash_collision_is_stopped_by_byte_verification() {
        let mem = MemSource::new(&[("a", b"AAAA"), ("b", b"BBBB"), ("c", b"CCCC")]);
        let src = CollidingSource(mem);
        let e = [cand("a", 4), cand("b", 4), cand("c", 4)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.full_files, 3, "哈希阶段三个文件确实「一样」");
        assert!(
            rep.groups.is_empty(),
            "复核阶段必须发现它们内容不同 → 0 组，而不是报出 1 组 3 个"
        );
    }

    /// 撞哈希但**其中两个真的相同**时：只有那两个成组。
    #[test]
    fn collision_with_a_real_pair_keeps_only_the_real_pair() {
        let mem = MemSource::new(&[("a", b"AAAA"), ("b", b"AAAA"), ("c", b"CCCC")]);
        let src = CollidingSource(mem);
        let e = [cand("a", 4), cand("b", 4), cand("c", 4)];
        let rep = run(&e, &src, &DupOptions::default());
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].count, 2);
        assert_eq!(
            rep.groups[0]
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    // ── 读失败 ──────────────────────────────────────────────────────────────

    /// 读不到的文件必须被剔除并计数，且**不能**因为"都读不到"而互相成组。
    #[test]
    fn unreadable_files_are_excluded_and_counted() {
        let src = MemSource::new(&[("a", b"payload")])
            .with_broken(&["b", "c"]);
        let e = [cand("a", 7), cand("b", 7), cand("c", 7)];
        let rep = run(&e, &src, &DupOptions::default());
        assert!(rep.groups.is_empty(), "读不到 ≠ 内容相同");
        assert_eq!(rep.unreadable, 2);
    }

    // ── 截断与确定性 ────────────────────────────────────────────────────────

    #[test]
    fn max_per_group_truncates_the_list_but_not_the_count() {
        let pairs: Vec<(String, Vec<u8>)> = (0..10)
            .map(|i| (format!("f{i}"), b"same".to_vec()))
            .collect();
        let refs: Vec<(&str, &[u8])> = pairs
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let src = MemSource::new(&refs);
        let e: Vec<DupCandidate> = (0..10).map(|i| cand(&format!("f{i}"), 4)).collect();
        let o = opts(|o| o.max_per_group = 3);
        let rep = run(&e, &src, &o);
        let g = &rep.groups[0];
        assert_eq!(g.count, 10, "真实数量必须如实");
        assert_eq!(g.files.len(), 3, "列出的文件受上限约束");
        assert!(g.truncated);
        assert_eq!(g.wasted, 36, "可回收空间按真实数量算：4 × 9");
        assert_eq!(rep.total_wasted, 36);
    }

    #[test]
    fn max_groups_truncates_output_but_total_stays_complete() {
        // 造 6 个互不相同的"重复对"
        let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
        let mut ents: Vec<DupCandidate> = Vec::new();
        for i in 0..6u64 {
            let size = (i + 1) * 10;
            let data = vec![b'x' + i as u8; size as usize];
            pairs.push((format!("a{i}"), data.clone()));
            pairs.push((format!("b{i}"), data));
            ents.push(cand(&format!("a{i}"), size));
            ents.push(cand(&format!("b{i}"), size));
        }
        let refs: Vec<(&str, &[u8])> = pairs
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let src = MemSource::new(&refs);
        let o = opts(|o| o.max_groups = 2);
        let rep = run(&ents, &src, &o);
        assert_eq!(rep.groups.len(), 2, "输出被截断");
        assert_eq!(rep.group_count, 6, "但组数如实");
        let expect: u64 = (1..=6).sum::<u64>() * 10;
        assert_eq!(rep.total_wasted, expect, "可回收空间不被截断影响");
        // 排序：可回收大的在前 → 最后两组（size 60、50）被留下
        assert_eq!(rep.groups[0].size, 60);
        assert_eq!(rep.groups[1].size, 50);
    }

    /// 同样输入跑两遍，组顺序与组内顺序都必须一样 —— 否则导出的 CSV 没法 diff。
    #[test]
    fn output_order_is_deterministic() {
        let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
        let mut ents: Vec<DupCandidate> = Vec::new();
        // 三组同样体积（都是 8）的重复对 → 前两个排序键全部相同，只剩路径能分先后
        for i in 0..3 {
            for j in 0..2 {
                let data = vec![b'a' + i as u8; 8];
                pairs.push((format!("d{i}_f{j}"), data));
                ents.push(cand(&format!("d{i}_f{j}"), 8));
            }
        }
        let refs: Vec<(&str, &[u8])> = pairs
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let src = MemSource::new(&refs);
        let a = run(&ents, &src, &DupOptions::default());
        let b = run(&ents, &src, &DupOptions::default());
        let key = |r: &DupReport| {
            r.groups
                .iter()
                .map(|g| g.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&a), key(&b));
        assert_eq!(key(&a)[0], vec!["d0_f0".to_string(), "d0_f1".to_string()]);
    }

    // ── 进度 ────────────────────────────────────────────────────────────────

    #[test]
    fn progress_covers_every_stage_and_ends_at_total() {
        let src = MemSource::new(&[("a", b"12345678"), ("b", b"12345678")]);
        let e = [cand("a", 8), cand("b", 8)];
        let mut seen: Vec<(DupStage, u64, u64)> = Vec::new();
        let rep = find_duplicates(&e, &DupOptions::default(), &src, &mut |p| {
            seen.push((p.stage, p.done, p.total))
        });
        assert_eq!(rep.groups.len(), 1);
        let last = |s: DupStage| {
            seen.iter()
                .filter(|x| x.0 == s)
                .next_back()
                .cloned()
                .unwrap_or_else(|| panic!("{s:?} 没有进度"))
        };
        let (_, d1, t1) = last(DupStage::Partial);
        let (_, d2, t2) = last(DupStage::Full);
        let (_, d3, t3) = last(DupStage::Verify);
        assert_eq!(d1, t1, "二级进度必须走满");
        assert_eq!(d2, t2, "三级进度必须走满");
        assert_eq!(d3, t3, "复核进度必须走满");
    }

    // ── 从索引树收集 ────────────────────────────────────────────────────────

    fn tree_with_files() -> Tree {
        let mut t = Tree::new("C:", 32);
        t.arena.ensure(20);
        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, flags: u16, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 } | flags,
                size,
                alloc: size,
                mtime: idx as i64,
                name: off,
            };
        };
        set(&mut t, 5, 5, ".", true, 0, 0);
        set(&mut t, 10, 5, "docs", true, 0, 0);
        set(&mut t, 11, 10, "a.bin", false, 0, 5000);
        set(&mut t, 12, 10, "b.bin", false, 0, 5000);
        set(&mut t, 13, 10, "tiny.txt", false, 0, 10);
        set(&mut t, 14, 10, "empty", false, 0, 0);
        set(&mut t, 15, 10, "link", false, NODE_REPARSE, 5000);
        t.root = 5;
        t
    }

    #[test]
    fn collect_skips_dirs_empty_and_reparse() {
        let t = tree_with_files();
        let set = collect_candidates(&t, None, &DupOptions::default());
        let paths: Vec<&str> = set.items.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&r"C:\docs\a.bin"));
        assert!(paths.contains(&r"C:\docs\b.bin"));
        assert!(paths.contains(&r"C:\docs\tiny.txt"));
        assert!(!paths.contains(&r"C:\docs\empty"), "0 字节不参与");
        assert!(!paths.contains(&r"C:\docs\link"), "重解析点不跟随");
        assert!(!paths.iter().any(|p| p.ends_with("docs")), "目录不是候选");
        assert_eq!(set.skipped_empty, 1);
        assert_eq!(set.skipped_reparse, 1);
    }

    #[test]
    fn collect_applies_min_size_and_remap() {
        let t = tree_with_files();
        let o = opts(|o| o.min_size = 1000);
        let set = collect_candidates(&t, None, &o);
        assert_eq!(set.items.len(), 2);
        assert_eq!(set.skipped_small, 1);

        // 重映射：合成前缀 `C:\` 换成真实的扫描根
        let r = PathRemap::new("C:", r"D:\projects").unwrap();
        let set = collect_candidates(&t, Some(&r), &DupOptions::default());
        assert!(set
            .items
            .iter()
            .any(|c| c.path == r"D:\projects\docs\a.bin"));
    }

    #[test]
    fn collect_honours_candidate_cap() {
        let t = tree_with_files();
        let o = opts(|o| {
            o.max_candidates = 1;
            o.min_size = 1;
        });
        let set = collect_candidates(&t, None, &o);
        assert_eq!(set.items.len(), 1);
        assert!(set.truncated >= 2, "被上限挡掉的文件要计数");
    }

    // ── 输出格式 ────────────────────────────────────────────────────────────

    #[test]
    fn csv_escapes_commas_and_quotes() {
        // 注意：不能用 `r"..."` 原串 —— 路径里真的要带引号，原串遇到 `"` 就结束了。
        let messy = "C:\\a,b\\say \"hi\".txt";
        let g = DupGroup {
            size: 3,
            count: 2,
            wasted: 3,
            truncated: false,
            files: vec![cand(messy, 3), cand(r"C:\plain.txt", 3)],
        };
        let csv = groups_to_csv(&[g]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "group,size,count,wasted,path");
        assert_eq!(
            lines[1],
            "1,3,2,3,\"C:\\a,b\\say \"\"hi\"\".txt\"",
            "含逗号与引号的路径必须整体加引号、内部引号翻倍"
        );
        assert_eq!(lines[2], r"1,3,2,3,C:\plain.txt", "普通路径不加引号");
    }

    #[test]
    fn summary_mentions_the_funnel() {
        let src = MemSource::new(&[("a", b"12345678"), ("b", b"12345678")]);
        let e = [cand("a", 8), cand("b", 8)];
        let rep = run(&e, &src, &DupOptions::default());
        let s = summary_cn(&rep);
        assert!(s.contains("确认重复 1 组"), "{s}");
        assert!(s.contains("可回收"), "{s}");
    }
}
