//! 扫描会话：把「选后端 → 全量扫描 → 建立增量会话 → 注册监控目标」串起来。
//!
//! 这是 CLI 与 GUI 共用的唯一入口（文档 §4.2 的 `core-scan::session`）。

use crate::full_scan::{self, ScanOptions, ScanStats};
use crate::incremental::IncrementalSession;
use crate::traverse::{self, TraverseOptions, TraverseStats};
use crate::watcher::WatcherSession;
use core_index::Tree;
use core_io::{Volume, VolumeInfo};
use core_monitor::{ChangeLog, EngineConfig, MonitorEngine, Rule, TickOutcome};
use core_ntfs::NtfsVolume;
use core_usn::CursorValidity;
use std::io;
use std::path::PathBuf;

/// 实际使用的扫描后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanBackend {
    /// MFT 直读（NTFS + 管理员）
    Mft,
    /// 目录递归遍历（降级）
    Traverse,
}

impl ScanBackend {
    pub fn label_cn(self) -> &'static str {
        match self {
            ScanBackend::Mft => "MFT 直读（极速）",
            ScanBackend::Traverse => "目录遍历（降级模式）",
        }
    }
}

/// 扫描阶段（进度回调用它区分来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhase {
    Mft,
    Traverse,
}

/// 扫描中的「半成品视图」回调（E7 渐进渲染）。
///
/// 引擎每扫到一定数量就回调一次，回调里拿到的是**已聚合过大小**的整棵树。
/// 为什么不让调用方自己决定什么时候聚合：扫描过程中目录节点的 `size` 一直是 0
/// （只有文件节点有大小），漏掉聚合的话画出来是一屏 0 面积的方块 ——
/// 看起来像"扫到的东西全是空的"，比不画还糟。所以聚合与回调必须绑在一起。
pub struct PartialSink(pub std::sync::Arc<dyn Fn(&core_index::Tree) + Send + Sync>);

impl Clone for PartialSink {
    fn clone(&self) -> Self {
        PartialSink(self.0.clone())
    }
}

impl std::fmt::Debug for PartialSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PartialSink")
    }
}

impl PartialSink {
    /// 到了步长就聚合并回调。返回是否真的回调了，并把 `next` 推进到下一个触发点。
    pub fn tick(
        &self,
        tree: &mut core_index::Tree,
        seen: u64,
        next: &mut u64,
        stride: u64,
    ) -> bool {
        if seen < *next {
            return false;
        }
        *next = seen.saturating_add(stride.max(1));
        core_index::aggregate::aggregate(tree);
        (self.0)(tree);
        true
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub drive: String,
    /// 允许自动创建 USN Journal（**系统级变更**，文档 R3，默认关）
    pub allow_create_journal: bool,
    /// 读卷失败时是否降级为目录遍历（文档 R2）
    pub allow_traverse_fallback: bool,
    /// **强制跳过 MFT 直读**，直接走目录遍历。
    ///
    /// 两个用途：① 在没有管理员权限的机器上验证降级链路；
    /// ② 确认某些场景下「完全不碰裸设备句柄」更安全（例如合规审计）。
    pub force_traverse: bool,
    /// 目录遍历模式的根（默认盘符根 `C:\`）
    pub scan_root: Option<PathBuf>,
    pub engine: EngineConfig,
    pub scan: ScanOptions,
    pub traverse: TraverseOptions,
    /// 变更流水落盘路径；`None` 表示不落盘
    pub changelog_path: Option<PathBuf>,
    /// 降级模式下是否启用 `ReadDirectoryChangesW` 实时增量（默认开）
    pub allow_traverse_watch: bool,
    /// 降级模式构建路径索引的节点数上限。
    ///
    /// 路径索引是 `小写绝对路径 → 节点下标` 的哈希表，约 110 字节/条；
    /// 超过上限就不建索引、只做一次性扫描（否则内存会突破 300 MB 预算）。
    pub watch_max_nodes: usize,
    /// 目录监视缓冲区大小。太小会在突发变更时丢事件（R5）
    pub watch_buffer_bytes: usize,
    /// 是否递归监视子目录
    pub watch_subtree: bool,
    /// 扫描过程中的半成品视图回调（`None` = 不回调）。
    ///
    /// 只在**全量扫描**阶段有效；增量回放不触发 —— 那一步量小，
    /// 而且此时画面上已经有完整的图了，再画一遍只会闪。
    pub partial: Option<PartialSink>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            drive: core_io::paths::DEFAULT_VOLUME.to_string(),
            allow_create_journal: false,
            allow_traverse_fallback: true,
            force_traverse: false,
            scan_root: None,
            engine: EngineConfig::default(),
            scan: ScanOptions::default(),
            traverse: TraverseOptions::default(),
            changelog_path: None,
            allow_traverse_watch: true,
            watch_max_nodes: 300_000,
            watch_buffer_bytes: 64 * 1024,
            watch_subtree: true,
            partial: None,
        }
    }
}

pub struct Session {
    pub cfg: SessionConfig,
    pub info: Option<VolumeInfo>,
    pub backend: ScanBackend,
    pub tree: Tree,
    pub scan_stats: Option<ScanStats>,
    pub traverse_stats: Option<TraverseStats>,
    pub incremental: Option<IncrementalSession>,
    /// 降级模式下的目录监视会话（与 `incremental` 互斥，同时只有一个为 `Some`）
    pub watch: Option<WatcherSession>,
    pub engine: MonitorEngine,
    /// 启动时的游标有效性判定
    pub validity: CursorValidity,
    /// 是否本次新建了 USN Journal（退出时可提示用户恢复，文档 R3）
    pub journal_created: bool,
    /// 运行过程中累积的提示信息（UI / CLI 展示）
    pub warnings: Vec<String>,
}

impl Session {
    /// 打开会话：选后端 → 全量扫描 → 建立增量会话。
    pub fn open<P>(cfg: SessionConfig, mut progress: P) -> io::Result<Session>
    where
        P: FnMut(ScanPhase, u64, u64),
    {
        let mut warnings: Vec<String> = Vec::new();
        let info = core_io::list_volumes()
            .into_iter()
            .find(|v| v.drive.eq_ignore_ascii_case(&cfg.drive));

        // ── ⓪ 没有快速路径的平台（Linux）：直接走目录遍历 ────────────────
        //
        // `$MFT` 直读本身在 Linux 上做得到（root + 块设备），但**增量来源**
        // USN Journal 是 NTFS 的 Windows 实现特性，`ntfs3`/`ntfs-3g` 都不暴露。
        // 没有增量的"快速路径"只能做一次性扫描 —— 那还不如直接走遍历，
        // 功能完整（有 inotify 实时增量）、行为可预期。所以这里不做尝试，
        // 也不假装失败后降级，而是明确说明原因。
        //
        // 用**常量判断**而不是 `#[cfg]`：这样整个 `open` 只有一份代码，
        // Windows 的逻辑照旧编译、Linux 上只是永不进入 —— 比在每个分支上
        // 挂 `#[cfg]` 少一半出错机会（少写一个，另一个平台就悄悄走了错分支）。
        if !core_io::HAS_MFT_BACKEND {
            warnings.push(format!(
                "当前平台没有 USN Journal：$MFT 直读仅在 Windows 可用，本次使用目录遍历 + {}",
                core_io::watch_backend_name()
            ));
            return Self::open_traverse(cfg, info, warnings, progress);
        }

        // ── ⓪' 调用方要求强制降级 ────────────────────────────────────────
        if cfg.force_traverse {
            warnings.push(format!(
                "已按要求跳过 MFT 直读，直接使用目录遍历 + {}",
                core_io::watch_backend_name()
            ));
            return Self::open_traverse(cfg, info, warnings, progress);
        }

        // ── ① 尝试 MFT 直读 ──────────────────────────────────────────────
        match Volume::open(&cfg.drive) {
            Ok(vol) => match NtfsVolume::mount(&vol) {
                Ok(ntfs) => {
                    warnings.push(format!("扫描后端：{}", ScanBackend::Mft.label_cn()));

                    // 先记录日志位置，再扫描 —— 扫描期间产生的事件随后重放，不漏
                    let (mut inc, validity, created) = IncrementalSession::from_volume(
                        vol,
                        cfg.allow_create_journal,
                        Some(ntfs),
                    )
                    .map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!(
                                "USN Journal 不可用：{}。增量监控将不可用（可加 --create-journal 自动启用）",
                                e
                            ),
                        )
                    })?;
                    let usn_snapshot = inc.journal.next_usn;

                    if created {
                        warnings.push(
                            "已为卷启用 USN Journal（这是一项系统级变更，退出时可用 `journal --delete` 恢复）"
                                .to_string(),
                        );
                    }
                    if !validity.can_resume() {
                        warnings.push(format!("游标自愈：{}", validity.reason_cn()));
                    }

                    // 全量扫描
                    let t = std::time::Instant::now();
                    let (tree, stats) = full_scan::full_scan_mft_with(
                        &inc.vol,
                        &cfg.drive,
                        inc.ntfs.as_ref().unwrap(),
                        cfg.scan,
                        |done, total| progress(ScanPhase::Mft, done, total),
                        cfg.partial.as_ref(),
                    )?;
                    let _ = t;

                    inc.align_cursor_after_scan(usn_snapshot);
                    let _ = inc.save_cursor();

                    let engine = build_engine(&cfg);
                    Ok(Session {
                        cfg: cfg.clone(),
                        info,
                        backend: ScanBackend::Mft,
                        tree,
                        scan_stats: Some(stats),
                        traverse_stats: None,
                        incremental: Some(inc),
                        watch: None,
                        engine,
                        validity,
                        journal_created: created,
                        warnings,
                    })
                }
                Err(e) => {
                    if !cfg.allow_traverse_fallback {
                        return Err(e);
                    }
                    warnings.push(format!(
                        "NTFS 解析失败（{}），降级为目录遍历",
                        e
                    ));
                    Self::open_traverse(cfg, info, warnings, progress)
                }
            },
            Err(e) => {
                if !cfg.allow_traverse_fallback {
                    return Err(e);
                }
                warnings.push(format!(
                    "卷读取失败（{}），降级为目录遍历模式",
                    e
                ));
                Self::open_traverse(cfg, info, warnings, progress)
            }
        }
    }

    fn open_traverse<P>(
        cfg: SessionConfig,
        info: Option<VolumeInfo>,
        mut warnings: Vec<String>,
        mut progress: P,
    ) -> io::Result<Session>
    where
        P: FnMut(ScanPhase, u64, u64),
    {
        let root = cfg
            .scan_root
            .clone()
            .unwrap_or_else(|| PathBuf::from(core_io::paths::root_prefix(&cfg.drive)));
        warnings.push(format!("扫描后端：{}", ScanBackend::Traverse.label_cn()));
        warnings.push(format!(
            "降级模式说明：非 NTFS / 无管理员权限时无法直读 $MFT，速度慢 1~2 个数量级；\
             大小统计仍按「逻辑 / 占用」双值给出（占用按簇对齐估算）"
        ));

        // ① 全量遍历。若打算启用实时增量，就顺手收集「路径 → 节点」索引——
        //    事后反推是错的（Tree 里的路径前缀是盘符，不是真实扫描根）
        let want_watch = cfg.allow_traverse_watch;
        let (tree, tstats, paths) = traverse::full_scan_traverse_indexed_with(
            &root,
            &cfg.drive,
            cfg.traverse,
            want_watch,
            |done, _total| progress(ScanPhase::Traverse, done, 0),
            cfg.partial.as_ref(),
        );

        // ② 建立目录监视会话。三种情况会退化为「只扫描、不增量」：
        //    节点数超预算（路径索引内存会突破 300 MB）／
        //    打开目录句柄失败／调用方显式关闭
        let mut watch = None;
        if want_watch {
            let nodes = tree.arena.nodes.len();
            if nodes > cfg.watch_max_nodes {
                warnings.push(format!(
                    "节点数 {} 超过实时增量预算 {}（路径索引约需 {}），本次仅做一次性扫描；\
                     可放宽 watch_max_nodes，或把监视范围缩小到一个子目录",
                    nodes,
                    cfg.watch_max_nodes,
                    core_io::util::human_size((nodes as u64) * 110)
                ));
            } else {
                match WatcherSession::new(
                    &root,
                    paths,
                    &tree,
                    cfg.watch_buffer_bytes,
                    cfg.watch_subtree,
                ) {
                    Ok(w) => {
                        warnings.push(format!(
                            "实时增量已启用：{}（{}，缓冲 {} KB，路径索引 {} 条，约 {}）",
                            core_io::watch_backend_name(),
                            if cfg.watch_subtree { "递归子树" } else { "仅本层" },
                            cfg.watch_buffer_bytes / 1024,
                            w.map_len(),
                            core_io::util::human_size(w.map_memory_bytes() as u64)
                        ));
                        watch = Some(w);
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "目录监视不可用（{}）：本次仅做一次性扫描，后续变更不会被跟踪",
                            e
                        ));
                    }
                }
            }
        } else {
            warnings.push("实时增量已按配置关闭（只做一次性扫描）".to_string());
        }

        let engine = build_engine(&cfg);
        Ok(Session {
            cfg,
            info,
            backend: ScanBackend::Traverse,
            tree,
            scan_stats: None,
            traverse_stats: Some(tstats),
            incremental: None,
            watch,
            engine,
            validity: CursorValidity::Fresh,
            journal_created: false,
            warnings,
        })
    }

    /// 注册监控目标：`node` 必须存在于索引树中。
    pub fn add_target(
        &mut self,
        node: u32,
        path: impl Into<String>,
        rules: Vec<Rule>,
        now_ms: u64,
    ) -> usize {
        let current = self
            .tree
            .arena
            .get(node)
            .map(|n| n.size)
            .unwrap_or(0);
        self.engine.add_target(node, path, rules, current, now_ms)
    }

    /// 按路径注册监控目标（内部做一次线性查找）。
    pub fn add_target_by_path(
        &mut self,
        path: &str,
        rules: Vec<Rule>,
        now_ms: u64,
    ) -> Option<(u32, usize)> {
        let node = self.find_node_by_path(path)?;
        let size = self.tree.arena.get(node).map(|n| n.size).unwrap_or(0);
        let idx = self.engine.add_target(node, path, rules, size, now_ms);
        Some((node, idx))
    }

    /// 在索引树里按路径找节点。O(n)，仅用于用户点击/CLI 参数这类低频操作。
    ///
    /// 分两步，先准后全：
    /// 1. 降级模式下路径索引里存的是**真实绝对路径**，能精确命中就直接返回；
    /// 2. 否则线性扫树：先找完全相等，再退而求其次找**最深的**祖先目录。
    pub fn find_node_by_path(&self, path: &str) -> Option<u32> {
        let target = norm_lookup(path);

        // ① 路径索引精确命中。索引键与 `norm_lookup` 的输出同构
        //   （都是「小写、去尾部反斜杠」），直接查即可。
        if let Some(w) = self.watch.as_ref() {
            if let Some(idx) = w.lookup(&target) {
                return Some(idx);
            }
        }

        // ② 树内线性查找
        let mut best: Option<(usize, u32)> = None;
        for i in 0..self.tree.arena.nodes.len() {
            let idx = i as u32;
            if !self.tree.arena.get(idx).map(|n| n.in_use() && n.is_dir()).unwrap_or(false) {
                continue;
            }
            let p = norm_lookup(&self.tree.path_of(idx));
            if p == target {
                return Some(idx);
            }
            // 前缀回退必须落在**路径分量边界**上：否则 `c:\a` 会匹配到
            // `c:\ab`，用户选了一个目录却注册到了隔壁。同时取最深的那个
            // —— 匹配得越深越精确。
            let deeper = best.map(|(len, _)| p.len() > len).unwrap_or(true);
            if p.len() > 1
                && target.len() > p.len()
                && target.as_bytes()[p.len()] == core_io::paths::SEP as u8
                && target.starts_with(p.as_str())
                && deeper
            {
                best = Some((p.len(), idx));
            }
        }
        best.map(|(_, idx)| idx)
    }

    /// 一轮增量处理。
    ///
    /// 三条路径互斥：USN（快速）→ 目录监视（降级但有增量）→ 纯 tick（无增量）。
    pub fn poll(&mut self, now_ms: u64) -> io::Result<TickOutcome> {
        if let Some(inc) = self.incremental.as_mut() {
            return inc.poll(&mut self.engine, &mut self.tree, now_ms);
        }
        if let Some(w) = self.watch.as_mut() {
            let code = w.watcher.exit_error();
            if code != 0 {
                let msg = format!(
                    "目录监视已中断（错误码 {}），请重新扫描后再开始监控",
                    code
                );
                return Err(io::Error::new(io::ErrorKind::Other, msg));
            }
            return Ok(w.poll(&mut self.tree, &mut self.engine, now_ms));
        }
        Ok(self.engine.tick(&mut self.tree, now_ms, |_| None))
    }

    /// 中途落一次盘：保存 USN 游标。
    ///
    /// 与 [`Session::shutdown`] 的区别是**不拆会话** —— 监视还在继续。
    ///
    /// 为什么需要它：游标只在 `shutdown` 时保存，所以「监控三小时 → 进程被
    /// 强制结束/崩溃」会丢掉这三小时的进度。丢了不等于丢事件（下次 `open`
    /// 会从旧位置重放，而事件里带的是回读出来的**绝对**大小，重放是幂等的），
    /// 但重放要重新走一遍日志，代价实打实。界面按一个较慢的节奏（数十秒）
    /// 调一次这里，就能把最坏情况从"几小时"压到"几十秒"。
    ///
    /// 纯 tick 模式（没有增量会话）下是空操作。
    pub fn checkpoint(&mut self) -> io::Result<()> {
        if let Some(inc) = self.incremental.as_ref() {
            inc.save_cursor()?;
        }
        if let Some(log) = self.engine.changelog.as_mut() {
            log.flush()?;
        }
        Ok(())
    }

    /// 收尾：吐出剩余事件、保存游标、刷盘。
    pub fn shutdown(&mut self, now_ms: u64) -> io::Result<TickOutcome> {
        match self.incremental.as_mut() {
            Some(inc) => {
                let out = inc.flush(&mut self.engine, &mut self.tree, now_ms)?;
                let _ = inc.save_cursor();
                Ok(out)
            }
            None => match self.watch.as_mut() {
                Some(w) => {
                    w.shutdown();
                    let out = w.flush(&mut self.tree, &mut self.engine, now_ms);
                    Ok(out)
                }
                None => {
                    let mut out = self.engine.flush(&mut self.tree, now_ms, |_| None);
                    out.still_pending = 0;
                    Ok(out)
                }
            },
        }
    }

    /// 增量来源的中文标签。
    pub fn incremental_source_cn(&self) -> &'static str {
        if self.incremental.is_some() {
            "USN Journal"
        } else if self.watch.is_some() {
            core_io::watch_backend_name()
        } else {
            "无（仅一次性扫描）"
        }
    }

    /// 一行状态摘要。
    pub fn status_line(&self) -> String {
        let mut s = format!(
            "后端 {} | {} 个目录 / {} 个文件 | 逻辑 {} / 占用 {} | 内存约 {}",
            self.backend.label_cn(),
            self.tree.dir_count,
            self.tree.file_count,
            core_io::util::human_size(self.tree.total_size),
            core_io::util::human_size(self.tree.total_alloc),
            core_io::util::human_size(self.tree.memory_bytes() as u64)
        );
        if let Some(inc) = &self.incremental {
            s.push_str(&format!(
                " | USN next={} 剩余空间 {:.0}%",
                inc.cursor.next_usn,
                inc.journal_free_ratio() * 100.0
            ));
        } else if let Some(w) = &self.watch {
            s.push_str(&format!(
                " | 监视 {} 条路径（{}）",
                w.map_len(),
                core_io::util::human_size(w.map_memory_bytes() as u64)
            ));
        } else {
            s.push_str(" | 增量监控不可用（降级模式）");
        }
        s
    }
}

fn build_engine(cfg: &SessionConfig) -> MonitorEngine {
    let mut engine = MonitorEngine::new(cfg.engine.clone());
    // 降级模式允许把扫描根设成**深目录**（`--root=D:\projects`），而
    // `Tree::path_of` 拼出来的前缀恒为盘符 —— 于是拿到的是 `C:\incoming\a.bin`
    // 这种磁盘上并不存在的路径。显示层会替换前缀，**变更流水落盘时也必须替换**，
    // 否则会出现「屏幕对、日志错」的不一致（用户拿日志回溯时才发现对不上）。
    if let Some(r) = cfg
        .scan_root
        .as_ref()
        .and_then(|p| core_monitor::PathRemap::new(&cfg.drive, &p.to_string_lossy()))
    {
        engine = engine.with_path_remap(r);
    }
    match &cfg.changelog_path {
        Some(p) if cfg.engine.write_changelog => match ChangeLog::open(p) {
            Ok(log) => engine.with_changelog(log),
            Err(_) => engine,
        },
        _ => engine,
    }
}

/// 路径查询归一化：**去尾部反斜杠 + 小写**（盘根不能削成空串）。
///
/// 两个来源必须归一到同一个键，否则「以盘根为扫描根时查 `C:\` 会返回 None」：
/// * 用户输入：CLI `--watch-path`、GUI 里点选，可能写成 `C:\`、`C:\Users\x\`、`C:`；
/// * [`Tree::path_of`]：盘根返回的是 `C:\`（带反斜杠）。
///
/// 这和 [`crate::watcher::parent_key`] 的盘根尾斜杠问题是**同一类**错误，
/// 只是一处发生在「定位父节点」、这里发生在「按路径查节点」。
/// 注意 `C:\` 要归一到 `C:` 而不是 `""` —— 削成空串会让所有路径都以它开头。
fn norm_lookup(s: &str) -> String {
    // 唯一实现：去尾分隔符 + 小写，卷根不能削成空串。
    // 必须与 `watcher::key_of`（写索引键）逐字节一致，否则查不到。
    core_io::paths::lookup_key(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手搭一棵最小的树：目录 5（根）→ 6（子目录）→ 三个文件。
    fn tiny_tree() -> core_index::Tree {
        use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
        let mut t = core_index::Tree::new("C:", 16);
        t.root = 5;
        t.arena.ensure(12);
        let set = |t: &mut core_index::Tree, idx: u32, parent: u32, dir: bool, size: u64| {
            if let Some(n) = t.arena.get_mut(idx) {
                *n = Node {
                    parent,
                    flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                    size,
                    alloc: size,
                    ..Node::EMPTY
                };
            }
        };
        set(&mut t, 5, 5, true, 0);
        set(&mut t, 6, 5, true, 0);
        set(&mut t, 7, 6, false, 100);
        set(&mut t, 8, 6, false, 50);
        set(&mut t, 9, 6, false, 7);
        t
    }

    /// 回调**必须看到已聚合的目录大小**。
    ///
    /// 扫描中目录节点的 size 一直是 0；漏掉聚合，界面画出来就是一屏
    /// 0 面积的方块 —— 看起来像"扫到的东西全是空的"，比不画还糟。
    #[test]
    fn partial_sink_aggregates_before_callback() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let s2 = seen.clone();
        let sink = PartialSink(std::sync::Arc::new(move |t: &core_index::Tree| {
            s2.lock().unwrap()
                .push(t.node(6).map(|n| n.size).unwrap_or(0));
        }));
        let mut tree = tiny_tree();
        let mut next = 10u64;
        // 没到步长：不回调
        assert!(!sink.tick(&mut tree, 9, &mut next, 10));
        assert!(seen.lock().unwrap().is_empty());
        // 到步长：回调一次，且目录 6 的大小已经算成 157
        assert!(sink.tick(&mut tree, 10, &mut next, 10));
        assert_eq!(seen.lock().unwrap().as_slice(), &[157]);
        assert_eq!(next, 20, "下一次触发点必须推进");
        // 再调一次同样的 seen 不重复触发
        assert!(!sink.tick(&mut tree, 10, &mut next, 10));
    }

    /// 回调是 `Send + Sync` 的：扫描在后台线程跑，闭包里要能带 AppHandle。
    #[test]
    fn partial_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PartialSink>();
    }

    #[test]
    fn backend_labels() {
        assert!(ScanBackend::Mft.label_cn().contains("MFT"));
        assert!(ScanBackend::Traverse.label_cn().contains("降级"));
    }

    /// Linux 版同一组契约：根恒为 `/`，去尾斜杠、小写，且**绝不**返回空串。
    #[cfg(not(windows))]
    #[test]
    fn norm_lookup_unifies_root_forms() {
        assert_eq!(norm_lookup("/"), "/");
        assert_eq!(norm_lookup("/home/me/"), "/home/me");
        assert_eq!(norm_lookup("/home/me"), "/home/me");
        assert_eq!(norm_lookup("/Home/Me"), "/home/me");
        // 退化输入不能归一成空串（空串是所有路径的前缀）
        assert_eq!(norm_lookup(""), "/");
    }

    #[cfg(windows)]
    #[test]
    fn norm_lookup_unifies_drive_root_forms() {
        // `C:\` 与 `C:` 必须归一到同一个键 —— 否则「以盘根为扫描根时查 `C:\`」
        // 会因为 `path_of` 给的是 `C:\` 而用户输入被削成 `C:` 而永远查不到
        assert_eq!(norm_lookup("C:\\"), "c:");
        assert_eq!(norm_lookup("C:"), "c:");
        assert_eq!(norm_lookup("C:\\Users\\x\\"), "c:\\users\\x");
        assert_eq!(norm_lookup("C:\\Users\\x"), "c:\\users\\x");
        // 退化输入不能归一成空串（空串是所有路径的前缀）
        assert_eq!(norm_lookup("\\"), "\\");
        assert_eq!(norm_lookup(""), "\\");
    }

    #[test]
    fn default_config_is_safe() {
        let c = SessionConfig::default();
        // 默认不主动创建 USN Journal —— 这是一项系统级变更（文档 R3）
        assert!(!c.allow_create_journal);
        // 默认允许降级，保证普通用户也能用
        assert!(c.allow_traverse_fallback);
        // 默认不强制降级：有权限就该走快速路径
        assert!(!c.force_traverse);
    }

    #[test]
    fn force_traverse_skips_mft_even_with_volume_handle() {
        let owned = core_io::test_dir::TestDir::new("fw-force-trav").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("only.txt"), b"abc").unwrap();

        let mut cfg = SessionConfig::default();
        cfg.scan_root = Some(base.clone());
        cfg.force_traverse = true;
        cfg.allow_traverse_watch = false;
        cfg.engine.write_changelog = false;

        let s = Session::open(cfg, |_, _, _| {}).unwrap();
        assert_eq!(s.backend, ScanBackend::Traverse);
        assert!(s.warnings.iter().any(|w| w.contains("跳过 MFT")));
        assert_eq!(s.tree.file_count, 1);

    }

    #[test]
    fn traverse_session_on_temp_dir() {
        let owned = core_io::test_dir::TestDir::new("fw-session").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(base.join("docs")).unwrap();
        std::fs::write(base.join("docs").join("readme.md"), b"hello world").unwrap();
        std::fs::write(base.join("data.bin"), vec![0u8; 2048]).unwrap();

        let mut cfg = SessionConfig::default();
        cfg.drive = "C:".into();
        cfg.scan_root = Some(base.clone());
        cfg.engine.write_changelog = false;

        let mut s = Session::open(cfg, |_, _, _| {}).unwrap();
        assert_eq!(s.backend, ScanBackend::Traverse);
        assert!(s.traverse_stats.is_some());
        assert_eq!(s.tree.file_count, 2);
        assert!(s.tree.total_size >= 2059);
        assert!(!s.warnings.is_empty());

        // 降级模式下 poll 不应报错
        let out = s.poll(1000).unwrap();
        assert_eq!(out.applied, 0);

        // 状态行可读
        let line = s.status_line();
        assert!(line.contains("降级"));

    }

    #[test]
    fn find_node_by_path_works() {
        let owned = core_io::test_dir::TestDir::new("fw-session-find").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(base.join("level1")).unwrap();
        std::fs::write(base.join("level1").join("f.txt"), b"x").unwrap();

        let mut cfg = SessionConfig::default();
        cfg.scan_root = Some(base.clone());
        cfg.engine.write_changelog = false;
        let mut s = Session::open(cfg, |_, _, _| {}).unwrap();

        // 根目录必须能定位到
        assert_eq!(s.find_node_by_path("C:\\"), Some(5));
        // 子目录也要能定位
        let sub = s.find_node_by_path("C:\\level1");
        assert!(sub.is_some(), "应能定位到 level1");

        // 按路径注册监控目标
        let r = s.add_target_by_path("C:\\level1", vec![Rule::absolute(1)], 0);
        assert!(r.is_some());
        assert_eq!(s.engine.rules.len(), 1);

    }

    /// 端到端：降级模式 + 实时增量（`ReadDirectoryChangesW`）。
    ///
    /// 这是**不需要管理员权限**就能验证整条链路的测试，也是本机唯一能跑的
    /// 「扫描 → 事件 → 增量更新索引 → 体积上传」全流程验证。
    #[test]
    fn degraded_mode_tracks_changes_live() {
        let owned = core_io::test_dir::TestDir::new("fw-live").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(base.join("inbox")).unwrap();
        std::fs::write(base.join("inbox").join("seed.txt"), vec![b'a'; 1000]).unwrap();

        let mut cfg = SessionConfig::default();
        cfg.scan_root = Some(base.clone());
        cfg.engine.write_changelog = false;
        cfg.engine.debounce.window_ms = 60; // 缩短窗口，测试才等得起
        cfg.watch_subtree = true;

        let mut s = Session::open(cfg, |_, _, _| {}).unwrap();
        assert_eq!(s.backend, ScanBackend::Traverse);
        if s.watch.is_none() {
            // 极端受限环境下连目录句柄都拿不到，跳过而不是误报
            eprintln!("跳过：目录监视不可用（{}）", s.warnings.join(" / "));
            return;
        }

        let root_idx = 5;
        let before = s.tree.arena.get(root_idx).unwrap().size;
        assert_eq!(before, 1000);

        // ① 新建一个子目录（验证「先物化占位目录」这条关键补偿）
        std::fs::create_dir_all(base.join("inbox").join("sub")).unwrap();
        // ② 在刚建的子目录里写文件——若占位机制失效，这 5000 字节传不到根
        std::fs::write(
            base.join("inbox").join("sub").join("deep.bin"),
            vec![b'b'; 5000],
        )
        .unwrap();

        // ③ 轮询直到内核把事件投递上来并成熟
        let mut applied = 0usize;
        for i in 0..60u64 {
            let now = 2_000 + i * 100;
            applied += s.poll(now).unwrap().applied;
            if applied >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            applied >= 2,
            "应至少应用「新建目录」与「新建文件」两条，实际 {}｜诊断：{}",
            applied,
            {
                let w = s.watch.as_ref().unwrap();
                format!(
                    "内核投递 {} 条 / 主线程读走 {} 条 / 丢弃 {} / 监视线程退出码 {} / 映射 {} 条 / 待回读 {} 条",
                    w.watcher.received(),
                    w.stats.notifications,
                    w.watcher.dropped(),
                    w.watcher.exit_error(),
                    w.map_len(),
                    w.pending_probe_count(),
                )
            }
        );

        // ④ 体积必须一路上传：root += 5000
        let after_root = s.tree.arena.get(root_idx).unwrap().size;
        assert_eq!(
            after_root,
            before + 5000,
            "新文件体积必须穿过新建目录上传到根（占位目录机制）"
        );

        // ⑤ 再改大它，验证「修改 → 增量 delta」
        std::fs::write(
            base.join("inbox").join("sub").join("deep.bin"),
            vec![b'c'; 9000],
        )
        .unwrap();
        let mut applied2 = 0usize;
        for i in 0..60u64 {
            let now = 10_000 + i * 100;
            applied2 += s.poll(now).unwrap().applied;
            if applied2 >= 1 && s.tree.arena.get(root_idx).unwrap().size == before + 9000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            s.tree.arena.get(root_idx).unwrap().size,
            before + 9000,
            "改写后根体积应更新为 1000 + 9000"
        );

        // ⑥ 删除它，验证负向 delta
        owned.remove_file("inbox/sub/deep.bin").unwrap();
        for i in 0..60u64 {
            let now = 20_000 + i * 100;
            let _ = s.poll(now).unwrap();
            if s.tree.arena.get(root_idx).unwrap().size == before {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            s.tree.arena.get(root_idx).unwrap().size,
            before,
            "删除后应回到初始体积"
        );

        // 标签由平台后端给出（Windows ReadDirectoryChangesW / Linux inotify）
        assert_eq!(s.incremental_source_cn(), core_io::watch_backend_name());
        assert!(s.status_line().contains("监视"));

        let _ = s.shutdown(30_000);
    }

    #[test]
    fn watch_can_be_disabled() {
        let owned = core_io::test_dir::TestDir::new("fw-nowatch").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("x.txt"), b"1").unwrap();

        let mut cfg = SessionConfig::default();
        cfg.scan_root = Some(base.clone());
        cfg.engine.write_changelog = false;
        cfg.allow_traverse_watch = false;

        let s = Session::open(cfg, |_, _, _| {}).unwrap();
        assert!(s.watch.is_none());
        assert_eq!(s.incremental_source_cn(), "无（仅一次性扫描）");
        assert!(s.warnings.iter().any(|w| w.contains("按配置关闭")));
        assert!(s.status_line().contains("不可用"));

    }
}
