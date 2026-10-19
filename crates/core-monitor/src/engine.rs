//! 监控引擎：把「USN 事件流」变成「索引更新 + 变更流水 + 阈值预警」。
//!
//! 这是 M2/M3 的收口模块，串起文档 §5.2 状态机的后半段：
//!
//! ```text
//! USN 事件 ──► 有界队列(背压) ──► debounce 合并 ──► 归一化事件
//!                                                      │
//!                        ┌─────────────────────────────┘
//!                        ▼
//!              增量更新索引树
//!              · FRN 查节点（数组索引，O(1)）
//!              · 回读 MFT 拿新大小（size_probe 回调注入）
//!              · delta 沿父链上传聚合（O(树深)）
//!              · 途经被监控目录时求值规则（O(1)）
//!                        │
//!                        ▼
//!              变更流水(JSONL) + 预警去重/合并 ──► 通知
//! ```
//!
//! **依赖注入**：回读 MFT 的动作由调用方通过 `size_probe` 闭包提供，
//! 因此本 crate 不需要依赖 `core-ntfs`，可以独立单测。

use crate::alert::{build_batch_notification, AlertSink, Notification};
use crate::changelog::ChangeLog;
use crate::debounce::{DebounceConfig, Debouncer, PendingEvent, RawEvent};
use crate::pathremap::PathRemap;
use crate::queue::RingQueue;
use crate::rules::{Alert, Rule, RuleConfig, RuleEngine};
use core_index::aggregate::{propagate_delta, propagate_from_parent, MAX_TREE_DEPTH};
use core_index::arena::{Node, INVALID_PARENT};
use core_index::{ChildIndex, Tree};
use core_usn::normalize::EventKind;

/// 文档 §9：增量事件延迟 ≤ 1.5 s（轮询 500 ms + 处理）。
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;
/// 文档 §5.5：环形缓冲 64 K 条
pub const DEFAULT_QUEUE_CAPACITY: usize = 64 * 1024;
pub const DEFAULT_MAX_EVENTS_PER_TICK: usize = 8192;
/// 文档 §5.4：相同内容 1 分钟内不重复弹
pub const DEFAULT_ALERT_DEDUP_MS: u64 = 60_000;

/// FRN → MFT 记录号（低 48 位）。
///
/// 刻意内联而不是依赖 `core-ntfs`，让 `core-monitor` 与磁盘解析彻底解耦。
#[inline]
fn recno_of(frn: u64) -> u32 {
    (frn & 0x0000_FFFF_FFFF_FFFF) as u32
}

/// FRN → 序列号（高 16 位），用于识别记录复用。
#[inline]
fn seq_of(frn: u64) -> u16 {
    (frn >> 48) as u16
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// USN 轮询间隔
    pub poll_interval_ms: u64,
    pub debounce: DebounceConfig,
    pub rules: RuleConfig,
    pub queue_capacity: usize,
    /// 单次 tick 最多处理多少条事件（防止一个 tick 卡住 UI）
    pub max_events_per_tick: usize,
    pub alert_dedup_ms: u64,
    /// 是否写变更流水
    pub write_changelog: bool,
    /// 是否把本轮应用的事件**复制**进 [`TickOutcome::events`]。
    ///
    /// 默认关：UI 要渲染实时事件列表时打开。开着会为每条事件分配
    /// 一到两个 `String`，高频写入场景下是一笔真实开销，所以不做默认。
    pub collect_events: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            debounce: DebounceConfig::default(),
            rules: RuleConfig::default(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_events_per_tick: DEFAULT_MAX_EVENTS_PER_TICK,
            alert_dedup_ms: DEFAULT_ALERT_DEDUP_MS,
            write_changelog: true,
            collect_events: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EngineStats {
    pub ticks: u64,
    pub raw_ingested: u64,
    pub events_applied: u64,
    /// 因队列溢出被丢弃的事件数（文档 §9「事件零丢失」对账依据）
    pub queue_dropped: u64,
    /// 因背压降级为「标记子树脏，稍后局部重扫」的次数（文档 R5）
    pub backpressure_events: u64,
    pub size_rereads: u64,
    pub ancestors_touched: u64,
}

#[derive(Debug, Default)]
pub struct TickOutcome {
    /// 本 tick 处理的事件数
    pub applied: usize,
    /// 通过去重、真正上报的预警
    pub alerts: Vec<Alert>,
    /// 合并后的通知（无预警时为 `None`）
    pub notification: Option<Notification>,
    /// 是否发生了背压（队列溢出）
    pub backpressure: bool,
    /// debounce 池里还剩多少未成熟事件
    pub still_pending: usize,
    /// 本轮**实际应用**的事件（仅当 [`EngineConfig::collect_events`] 打开时非空）。
    ///
    /// 元素此时已带上回读得到的 `new_size/new_alloc/size_delta/alloc_delta`，
    /// 可以直接渲染成一行变更日志。
    pub events: Vec<PendingEvent>,
}

pub struct MonitorEngine {
    pub cfg: EngineConfig,
    pub queue: RingQueue<RawEvent>,
    pub debouncer: Debouncer,
    pub rules: RuleEngine,
    pub alerts: AlertSink,
    pub changelog: Option<ChangeLog>,
    /// 合成路径 → 真实路径。降级模式指定深目录做扫描根时必须设置，
    /// 否则**变更流水里写的是并不存在的 `C:\...` 路径**（见 [`PathRemap`]）。
    pub path_remap: Option<PathRemap>,
    pub stats: EngineStats,
}

impl MonitorEngine {
    pub fn new(cfg: EngineConfig) -> MonitorEngine {
        let debouncer = Debouncer::new(cfg.debounce);
        let queue = RingQueue::new(cfg.queue_capacity);
        let rules = RuleEngine::new(cfg.rules);
        let alerts = AlertSink::new(cfg.alert_dedup_ms);
        MonitorEngine {
            cfg,
            queue,
            debouncer,
            rules,
            alerts,
            changelog: None,
            path_remap: None,
            stats: EngineStats::default(),
        }
    }

    pub fn with_changelog(mut self, log: ChangeLog) -> MonitorEngine {
        self.changelog = Some(log);
        self
    }

    /// 设置合成路径 → 真实路径的重映射（例如 `C:\` → `D:\projects`）。
    pub fn with_path_remap(mut self, remap: PathRemap) -> MonitorEngine {
        self.path_remap = Some(remap);
        self
    }

    /// 注册监控目标。
    pub fn add_target(
        &mut self,
        node: u32,
        path: impl Into<String>,
        rules: Vec<Rule>,
        current_size: u64,
        now_ms: u64,
    ) -> usize {
        self.rules.add_target(node, path, rules, current_size, now_ms)
    }

    /// 直接投递（同一线程内使用）。跨线程请用 [`MonitorEngine::enqueue`]。
    pub fn ingest(&mut self, ev: RawEvent, now_ms: u64) {
        self.stats.raw_ingested += 1;
        self.debouncer.offer(ev, now_ms);
    }

    /// 入有界队列（生产者线程调用）。返回 `false` 表示发生背压。
    pub fn enqueue(&mut self, ev: RawEvent) -> bool {
        self.stats.raw_ingested += 1;
        let ok = self.queue.push(ev);
        if !ok {
            self.stats.queue_dropped += 1;
        }
        ok
    }

    pub fn pending(&self) -> usize {
        self.debouncer.pending_len()
    }

    /// 常规 tick。
    pub fn tick<S>(&mut self, tree: &mut Tree, now_ms: u64, probe: S) -> TickOutcome
    where
        S: FnMut(u64) -> Option<(u64, u64, i64)>,
    {
        self.run(tree, now_ms, false, probe)
    }

    /// 强制吐出全部待处理事件（退出前收尾）。
    pub fn flush<S>(&mut self, tree: &mut Tree, now_ms: u64, probe: S) -> TickOutcome
    where
        S: FnMut(u64) -> Option<(u64, u64, i64)>,
    {
        self.run(tree, now_ms, true, probe)
    }

    fn run<S>(&mut self, tree: &mut Tree, now_ms: u64, force: bool, mut probe: S) -> TickOutcome
    where
        S: FnMut(u64) -> Option<(u64, u64, i64)>,
    {
        self.stats.ticks += 1;

        // ① 有界队列 → debouncer
        if !self.queue.is_empty() {
            let mut tmp: Vec<RawEvent> = Vec::with_capacity(1024);
            self.queue.drain_into(&mut tmp, if force { usize::MAX } else { 65_536 });
            for e in tmp {
                self.debouncer.offer(e, now_ms);
            }
        }
        let dropped = self.queue.dropped();
        if dropped > self.stats.queue_dropped {
            self.stats.backpressure_events += 1;
        }

        // ② debounce 合并
        let mut events: Vec<PendingEvent> = Vec::with_capacity(256);
        if force {
            self.debouncer.flush_all(&mut events);
        } else {
            self.debouncer.drain_matured_limited(
                now_ms,
                self.cfg.max_events_per_tick.max(1),
                &mut events,
            );
        }

        let mut outcome = TickOutcome {
            backpressure: dropped > 0,
            ..Default::default()
        };
        let mut candidates: Vec<Alert> = Vec::new();
        // 同批目录删除共享一次子索引；创建/移动后才失效，避免删除风暴 O(n²)。
        let mut deletion_children: Option<ChildIndex> = None;

        // ③ 逐条应用
        for ev in events.iter_mut() {
            let recno = recno_of(ev.frn);
            // 删除后节点即失效，路径必须**在应用之前**取；
            // 其余情况（新增/改名）应用之后才拿得到正确路径。
            let pre_path = tree.path_of(recno);

            Self::apply_one(
                tree,
                &mut self.rules,
                &mut self.stats,
                ev,
                now_ms,
                &mut probe,
                &mut candidates,
                &mut deletion_children,
            );
            if matches!(ev.kind, EventKind::Created | EventKind::Renamed) {
                deletion_children = None;
            }

            if self.cfg.write_changelog {
                if let Some(log) = self.changelog.as_mut() {
                    let path = if ev.kind == EventKind::Deleted {
                        pre_path
                    } else {
                        tree.path_of(recno)
                    };
                    // `tree.path_of` 给的是**合成路径**（前缀恒为盘符）。
                    // 落盘前必须换回真实扫描根，否则日志里写的是磁盘上
                    // 并不存在的 `C:\...` —— 屏幕对、日志错，最难查的那种不一致。
                    let path = match self.path_remap.as_ref() {
                        Some(r) => r.apply(&path),
                        None => path,
                    };
                    let _ = log.append(ev, &path);
                }
            }
            outcome.applied += 1;
            self.stats.events_applied += 1;
            if self.cfg.collect_events {
                outcome.events.push(ev.clone());
            }
        }

        // ④ 预警去重 + 同批合并
        let mut passed: Vec<Alert> = Vec::with_capacity(candidates.len());
        for a in candidates {
            if self.alerts.should_emit(&a, now_ms) {
                passed.push(a);
            }
        }
        outcome.notification = build_batch_notification(&passed);
        outcome.alerts = passed;
        outcome.still_pending = self.debouncer.pending_len();

        if self.cfg.write_changelog {
            if let Some(log) = self.changelog.as_mut() {
                let _ = log.flush();
                let _ = log.maybe_rotate();
            }
        }
        self.alerts.gc(now_ms);
        outcome
    }

    /// 单条事件 → 索引更新 + 规则求值。
    ///
    /// 所有参数都显式传入（而不是 `&mut self`），这样 `tree` 与 `self.rules`
    /// 才能同时可变借用——`propagate_delta` 的回调里两者都要用到。
    #[allow(clippy::too_many_arguments)]
    fn apply_one<S>(
        tree: &mut Tree,
        rules: &mut RuleEngine,
        stats: &mut EngineStats,
        ev: &mut PendingEvent,
        now_ms: u64,
        probe: &mut S,
        candidates: &mut Vec<Alert>,
        deletion_children: &mut Option<ChildIndex>,
    ) where
        S: FnMut(u64) -> Option<(u64, u64, i64)>,
    {
        let recno = recno_of(ev.frn);
        let parent_recno = recno_of(ev.parent_frn);
        let seq = seq_of(ev.frn);
        let before = tree.node(recno).copied().filter(|n| n.in_use());
        // 过期的 USN 删除/修改不能作用于已经复用该记录号的新文件。
        if ev.kind != EventKind::Created && before.map(|n| n.seq != seq).unwrap_or(false) {
            return;
        }
        let before_files = before.map(|n| {
            if n.is_dir() { tree.dir_file_count(recno) as i64 } else { 1 }
        }).unwrap_or(0);

        match ev.kind {
            // ── 新增 ────────────────────────────────────────────────────────
            EventKind::Created => {
                let (mut size, mut alloc, mut mtime) = match if ev.is_dir { None } else { probe(ev.frn) } {
                    Some(v) => {
                        stats.size_rereads += 1;
                        v
                    }
                    None => (0, 0, 0),
                };
                // 同一记录号上已有节点：可能是 FRN 复用（序列号变了）
                if let Some(old) = tree.arena.get(recno).copied() {
                    if old.in_use() {
                        // 降级监视会提前物化父目录；子文件可能先成熟并已计入它。
                        // 创建事件随后到达时必须保留这些聚合值，不能重置为零。
                        if ev.is_dir && old.is_dir() && old.seq == seq {
                            size = old.size;
                            alloc = old.alloc;
                            mtime = old.mtime;
                        } else if old.is_dir() {
                            // 目录记录被复用时，旧目录的后代也不再属于新记录。
                            Self::remove_descendants(tree, recno, deletion_children);
                            if let Some(count) = tree.dir_file_counts.get_mut(recno as usize) {
                                *count = 0;
                            }
                        }
                        let old_parent = old.parent;
                        let (os, oa) = (old.size, old.alloc);
                        // 先把旧内容从父链上扣掉，避免重复计入
                        propagate_from_parent(tree, old_parent, -(os as i64), -(oa as i64), |t, anc| {
                            stats.ancestors_touched += 1;
                            let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                            if let Some(a) = rules.evaluate(anc, total, now_ms) {
                                candidates.push(a);
                            }
                        });
                    }
                }
                ev.new_size = size;
                ev.new_alloc = alloc;

                tree.upsert_incremental(
                    recno,
                    parent_recno,
                    seq,
                    ev.is_dir,
                    size,
                    alloc,
                    mtime,
                    &ev.name,
                );
                // upsert 对目录默认清零；恢复的是此前聚合值，不是目录自身元数据大小。
                if ev.is_dir {
                    if let Some(node) = tree.arena.get_mut(recno) {
                        node.size = size;
                        node.alloc = alloc;
                    }
                }

                if size > 0 || alloc > 0 {
                    ev.size_delta = size as i64;
                    ev.alloc_delta = alloc as i64;
                    propagate_delta(tree, recno, size as i64, alloc as i64, |t, anc| {
                        stats.ancestors_touched += 1;
                        let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                        if let Some(a) = rules.evaluate(anc, total, now_ms) {
                            candidates.push(a);
                        }
                    });
                }
            }

            // ── 删除 ────────────────────────────────────────────────────────
            EventKind::Deleted => {
                if before.map(|n| n.is_dir()).unwrap_or(false) {
                    // 目录整体移出监视范围可能只有目录通知。同步摘除后代，
                    // 否则 Top-N 和全局文件数会保留无法再定位的幽灵文件。
                    Self::remove_descendants(tree, recno, deletion_children);
                }
                if let Some((os, oa)) = tree.remove_node(recno) {
                    ev.new_size = 0;
                    ev.new_alloc = 0;
                    ev.size_delta = -(os as i64);
                    ev.alloc_delta = -(oa as i64);
                    // `remove_node` 保留 parent，所以这里能沿着原链路做负向上传
                    propagate_delta(tree, recno, -(os as i64), -(oa as i64), |t, anc| {
                        stats.ancestors_touched += 1;
                        let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                        if let Some(a) = rules.evaluate(anc, total, now_ms) {
                            candidates.push(a);
                        }
                    });
                }
            }

            // ── 重命名 / 移动 ───────────────────────────────────────────────
            EventKind::Renamed => {
                let old_parent = tree
                    .arena
                    .get(recno)
                    .map(|n| n.parent)
                    .unwrap_or(INVALID_PARENT);
                let moved = old_parent != INVALID_PARENT && old_parent != parent_recno;

                if tree.rename_node(recno, &ev.name, parent_recno) && moved {
                    if let Some((s, a)) = tree.size_of(recno) {
                        // 从旧父链扣除
                        propagate_from_parent(tree, old_parent, -(s as i64), -(a as i64), |t, anc| {
                            stats.ancestors_touched += 1;
                            let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                            if let Some(a) = rules.evaluate(anc, total, now_ms) {
                                candidates.push(a);
                            }
                        });
                        // 加到新父链
                        propagate_delta(tree, recno, s as i64, a as i64, |t, anc| {
                            stats.ancestors_touched += 1;
                            let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                            if let Some(a) = rules.evaluate(anc, total, now_ms) {
                                candidates.push(a);
                            }
                        });
                    }
                }
                // 同一窗口的修改会合并进重命名；迁移旧大小之后再回读最终大小。
                if let Some((ds, da)) = Self::refresh_file_size(tree, stats, ev, probe) {
                    propagate_delta(tree, recno, ds, da, |t, anc| {
                        stats.ancestors_touched += 1;
                        let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                        if let Some(a) = rules.evaluate(anc, total, now_ms) {
                            candidates.push(a);
                        }
                    });
                }
            }

            // ── 修改 ────────────────────────────────────────────────────────
            EventKind::Modified | EventKind::SizeSemanticsChanged => {
                if let Some((ds, da)) = Self::refresh_file_size(tree, stats, ev, probe) {
                    propagate_delta(tree, recno, ds, da, |t, anc| {
                        stats.ancestors_touched += 1;
                        let total = t.arena.get(anc).map(|n| n.size).unwrap_or(0);
                        if let Some(a) = rules.evaluate(anc, total, now_ms) {
                            candidates.push(a);
                        }
                    });
                }
            }

            // 属性/硬链接变更不影响大小聚合（文档 §5.2）
            EventKind::AttrChanged | EventKind::HardLinkChanged | EventKind::Unknown => {}
        }
        let after = tree.node(recno).copied().filter(|n| n.in_use());
        Self::account_globals(tree, before, after);
        let after_files = after.map(|n| {
            if n.is_dir() { tree.dir_file_count(recno) as i64 } else { 1 }
        }).unwrap_or(0);
        if before.map(|n| n.parent) != after.map(|n| n.parent) || before_files != after_files {
            if let Some(old) = before {
                Self::adjust_file_counts(tree, old.parent, -before_files);
            }
            if let Some(new) = after {
                Self::adjust_file_counts(tree, new.parent, after_files);
            }
        }
        if after.is_none() {
            if let Some(count) = tree.dir_file_counts.get_mut(recno as usize) {
                *count = 0;
            }
        }
    }

    /// 与全量 aggregate 的口径一致：总大小只计文件，不能再加目录聚合值。
    fn account_globals(tree: &mut Tree, before: Option<Node>, after: Option<Node>) {
        if let Some(old) = before {
            if old.is_dir() {
                tree.dir_count = tree.dir_count.saturating_sub(1);
            } else {
                tree.file_count = tree.file_count.saturating_sub(1);
                tree.total_size = tree.total_size.saturating_sub(old.size);
                tree.total_alloc = tree.total_alloc.saturating_sub(old.alloc);
            }
        }
        if let Some(new) = after {
            if new.is_dir() {
                tree.dir_count = tree.dir_count.saturating_add(1);
            } else {
                tree.file_count = tree.file_count.saturating_add(1);
                tree.total_size = tree.total_size.saturating_add(new.size);
                tree.total_alloc = tree.total_alloc.saturating_add(new.alloc);
            }
        }
    }

    fn adjust_file_counts(tree: &mut Tree, mut parent: u32, delta: i64) {
        if delta == 0 { return; }
        for _ in 0..MAX_TREE_DEPTH {
            let Some(node) = tree.node(parent).copied().filter(|n| n.in_use()) else { break; };
            if let Some(count) = tree.dir_file_counts.get_mut(parent as usize) {
                *count = if delta > 0 {
                    count.saturating_add(delta as u32)
                } else {
                    count.saturating_sub(delta.unsigned_abs() as u32)
                };
            }
            if node.parent == parent { break; }
            parent = node.parent;
        }
    }

    /// 仅修改内存索引。普通文件事件仍是 O(树深)，目录整体删除时才构建子索引。
    fn remove_descendants(tree: &mut Tree, root: u32, cache: &mut Option<ChildIndex>) {
        let children = cache.get_or_insert_with(|| ChildIndex::build(tree));
        let mut pending = children.children(root).to_vec();
        while let Some(idx) = pending.pop() {
            if idx == root { continue; }
            let Some(node) = tree.node(idx).copied().filter(|n| n.in_use()) else { continue; };
            pending.extend_from_slice(children.children(idx));
            tree.remove_node(idx);
            Self::account_globals(tree, Some(node), None);
            if let Some(count) = tree.dir_file_counts.get_mut(idx as usize) {
                *count = 0;
            }
        }
    }

    fn refresh_file_size<S>(
        tree: &mut Tree,
        stats: &mut EngineStats,
        ev: &mut PendingEvent,
        probe: &mut S,
    ) -> Option<(i64, i64)>
    where
        S: FnMut(u64) -> Option<(u64, u64, i64)>,
    {
        let node = tree.arena.get_mut(recno_of(ev.frn))?;
        if !node.in_use() || node.is_dir() {
            return None;
        }
        // 即使回读失败，纯重命名也应报告原来的大小。
        ev.new_size = node.size;
        ev.new_alloc = node.alloc;
        let (size, alloc, mtime) = probe(ev.frn)?;
        stats.size_rereads += 1;
        let ds = size as i64 - node.size as i64;
        let da = alloc as i64 - node.alloc as i64;
        ev.new_size = size;
        ev.new_alloc = alloc;
        ev.size_delta = ds;
        ev.alloc_delta = da;
        node.size = size;
        node.alloc = alloc;
        if mtime > 0 {
            node.mtime = mtime;
        }
        (ds != 0 || da != 0).then_some((ds, da))
    }
}

impl std::fmt::Debug for MonitorEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorEngine")
            .field("targets", &self.rules.len())
            .field("pending", &self.debouncer.pending_len())
            .field("queued", &self.queue.len())
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
    use core_index::aggregate::aggregate;
    use crate::rules::Rule;
    use core_usn::normalize::*;

    const MB: u64 = 1024 * 1024;

    /// 建一棵 root(5) → logs(10) 的树
    fn sample_tree() -> Tree {
        let mut t = Tree::new("C:", 32);
        t.arena.ensure(20);
        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size: if dir { 0 } else { size },
                alloc: if dir { 0 } else { size },
                mtime: 1_700_000_000,
                name: off,
            };
        };
        set(&mut t, 5, 5, ".", true, 0);
        set(&mut t, 10, 5, "logs", true, 0);
        set(&mut t, 11, 10, "existing.log", false, 5 * MB);
        t.dir_file_counts.resize(t.arena.nodes.len(), 0);
        aggregate(&mut t);
        t
    }

    fn raw(frn: u64, parent: u64, reason: u32, name: &str, attrs: u32) -> RawEvent {
        RawEvent {
            frn,
            parent_frn: parent,
            usn: 100,
            timestamp: 1_700_000_000,
            reason,
            file_attributes: attrs,
            name: name.to_string(),
        }
    }

    fn frn(recno: u64, seq: u16) -> u64 {
        core_ntfs_frn(recno, seq)
    }

    /// 本地复制 FRN 拼装（避免测试依赖 core-ntfs）
    fn core_ntfs_frn(recno: u64, seq: u16) -> u64 {
        ((seq as u64) << 48) | (recno & 0x0000_FFFF_FFFF_FFFF)
    }

    const ATTR_FILE: u32 = 0x20;
    const ATTR_DIR: u32 = 0x10;

    fn engine(rules: Vec<Rule>) -> MonitorEngine {
        let mut cfg = EngineConfig::default();
        cfg.write_changelog = false; // 单测不落盘
        cfg.debounce.window_ms = 100;
        let mut e = MonitorEngine::new(cfg);
        e.add_target(10, r"C:\logs", rules, 5 * MB, 0);
        e
    }

    #[test]
    fn created_file_updates_aggregate() {
        let mut t = sample_tree();
        let mut e = engine(vec![Rule::absolute(1_000 * MB)]);

        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "new.log", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((2 * MB, 2 * MB, 1_700_000_100)));

        assert_eq!(out.applied, 1);
        assert_eq!(t.arena.get(20).unwrap().size, 2 * MB);
        // logs 目录 = 5MB + 2MB
        assert_eq!(t.arena.get(10).unwrap().size, 7 * MB);
        assert_eq!(t.arena.get(5).unwrap().size, 7 * MB);
        assert!(out.alerts.is_empty(), "远未到阈值");
    }

    #[test]
    fn regression_tick_budget_never_discards_mature_events() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.cfg.max_events_per_tick = 2;
        for id in 20..25 {
            e.ingest(raw(frn(id, 1), frn(10, 1), USN_REASON_FILE_CREATE, "new.log", ATTR_FILE), 1000);
        }
        let first = e.tick(&mut t, 2000, |_| Some((MB, MB, 0)));
        assert_eq!((first.applied, first.still_pending), (2, 3));
        assert!(!first.backpressure, "保留的积压不代表事件丢失");
        let second = e.tick(&mut t, 2100, |_| Some((MB, MB, 0)));
        let third = e.tick(&mut t, 2200, |_| Some((MB, MB, 0)));
        assert_eq!((second.applied, third.applied, third.still_pending), (2, 1, 0));
        assert_eq!(t.size_of(10).unwrap().0, 10 * MB);
    }

    #[test]
    fn regression_flush_bypasses_tick_budget() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.cfg.max_events_per_tick = 1;
        for id in 20..24 {
            e.ingest(raw(frn(id, 1), frn(10, 1), USN_REASON_FILE_CREATE, "new.log", ATTR_FILE), 1000);
        }
        let out = e.flush(&mut t, 1001, |_| Some((MB, MB, 0)));
        assert_eq!((out.applied, out.still_pending), (4, 0));
        assert_eq!(t.size_of(10).unwrap().0, 9 * MB);
    }

    #[test]
    fn regression_created_then_renamed_file_is_materialized() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "draft.log", ATTR_FILE), 1000);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_RENAME_OLD_NAME, "draft.log", ATTR_FILE), 1010);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_RENAME_NEW_NAME, "saved.log", ATTR_FILE), 1020);
        e.tick(&mut t, 2000, |_| Some((MB, MB, 0)));
        assert_eq!(t.name(20), "saved.log");
        assert_eq!(t.size_of(10).unwrap().0, 6 * MB);
    }

    #[test]
    fn regression_rename_and_modify_updates_size_in_new_parent() {
        let mut t = sample_tree();
        t.upsert_incremental(12, 5, 1, true, 0, 0, 0, "archive");
        let mut e = engine(vec![]);
        e.ingest(raw(frn(11, 1), frn(10, 1), USN_REASON_RENAME_OLD_NAME, "existing.log", ATTR_FILE), 1000);
        e.ingest(raw(frn(11, 1), frn(12, 1), USN_REASON_RENAME_NEW_NAME, "new.log", ATTR_FILE), 1010);
        e.ingest(raw(frn(11, 1), frn(12, 1), USN_REASON_DATA_EXTEND, "new.log", ATTR_FILE), 1020);
        e.tick(&mut t, 2000, |_| Some((7 * MB, 7 * MB, 0)));
        assert_eq!(t.size_of(10).unwrap().0, 0);
        assert_eq!(t.size_of(12).unwrap().0, 7 * MB);
        assert_eq!(t.size_of(5).unwrap().0, 7 * MB);
    }

    #[test]
    fn regression_directory_create_preserves_placeholder_children() {
        let mut t = sample_tree();
        t.upsert_incremental(12, 5, 0, true, 0, 0, 0, "pending");
        t.upsert_incremental(20, 12, 0, false, MB, MB, 0, "child.log");
        aggregate(&mut t);
        let mut e = engine(vec![]);
        e.ingest(raw(frn(12, 0), frn(5, 1), USN_REASON_FILE_CREATE, "pending", ATTR_DIR), 1000);
        e.tick(&mut t, 2000, |_| None);
        assert_eq!(t.size_of(12).unwrap().0, MB);
        assert_eq!(t.size_of(5).unwrap().0, 6 * MB);
    }

    /// 全量聚合作为独立对照；此处只在测试里扫描整棵树。
    fn assert_live_totals(tree: &Tree) {
        let mut rebuilt = tree.clone();
        aggregate(&mut rebuilt);
        assert_eq!((tree.file_count, tree.dir_count, tree.total_size, tree.total_alloc),
            (rebuilt.file_count, rebuilt.dir_count, rebuilt.total_size, rebuilt.total_alloc));
        for (idx, node) in tree.arena.nodes.iter().enumerate() {
            if node.in_use() && node.is_dir() {
                assert_eq!(tree.dir_file_count(idx as u32), rebuilt.dir_file_count(idx as u32),
                    "directory {idx} file count");
                assert_eq!((node.size, node.alloc),
                    (rebuilt.arena.nodes[idx].size, rebuilt.arena.nodes[idx].alloc),
                    "directory {idx} aggregate");
            }
        }
    }

    #[test]
    fn regression_global_totals_follow_create_resize_reuse_and_delete() {
        let mut tree = sample_tree();
        let mut engine = engine(vec![]);
        for (seq, reason, size, alloc) in [
            (1, USN_REASON_FILE_CREATE, 2 * MB, 3 * MB),
            (1, USN_REASON_DATA_EXTEND, 4 * MB, 6 * MB),
            (1, USN_REASON_DATA_TRUNCATION, MB, 2 * MB),
            (2, USN_REASON_FILE_CREATE, 3 * MB, 4 * MB),
        ] {
            engine.ingest(raw(frn(20, seq), frn(10, 1), reason, "live.bin", ATTR_FILE), 1000);
            engine.flush(&mut tree, 1100, |_| Some((size, alloc, 0)));
            assert_live_totals(&tree);
            assert_eq!(tree.file_count, 2);
            assert_eq!(tree.total_size, 5 * MB + size);
        }
        // 旧序列号的迟到删除不允许删掉复用后的文件。
        engine.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_DELETE, "old.bin", ATTR_FILE), 2000);
        engine.flush(&mut tree, 2100, |_| None);
        assert_eq!(tree.file_count, 2);
        assert_eq!(tree.total_size, 8 * MB);
        assert_live_totals(&tree);
        for _ in 0..2 {
            engine.ingest(raw(frn(20, 2), frn(10, 1), USN_REASON_FILE_DELETE, "live.bin", ATTR_FILE), 3000);
            engine.flush(&mut tree, 3100, |_| None);
            assert_eq!(tree.file_count, 1);
            assert_live_totals(&tree);
        }
    }

    #[test]
    fn regression_directory_move_and_delete_update_all_totals() {
        let mut tree = sample_tree();
        tree.upsert_incremental(12, 5, 1, true, 0, 0, 0, "archive");
        tree.upsert_incremental(13, 10, 1, true, 0, 0, 0, "nested");
        tree.upsert_incremental(20, 13, 1, false, MB, 2 * MB, 0, "nested.bin");
        aggregate(&mut tree);
        let mut engine = engine(vec![]);
        engine.ingest(raw(frn(10, 1), frn(12, 1), USN_REASON_RENAME_NEW_NAME, "moved", ATTR_DIR), 1000);
        engine.flush(&mut tree, 1100, |_| None);
        assert_live_totals(&tree);
        assert_eq!((tree.file_count, tree.dir_count), (2, 4));
        assert_eq!(tree.dir_file_count(12), 2);
        // 单条目录通知必须移除内存子树；后到的逐文件通知不能再减一次。
        engine.ingest(raw(frn(10, 1), frn(12, 1), USN_REASON_FILE_DELETE, "moved", ATTR_DIR), 2000);
        engine.flush(&mut tree, 2100, |_| None);
        assert_live_totals(&tree);
        assert_eq!((tree.file_count, tree.dir_count, tree.total_size, tree.total_alloc), (0, 2, 0, 0));
        for id in [11, 13, 20] { assert!(!tree.node(id).unwrap().in_use()); }
        engine.ingest(raw(frn(20, 1), frn(13, 1), USN_REASON_FILE_DELETE, "nested.bin", ATTR_FILE), 3000);
        engine.flush(&mut tree, 3100, |_| None);
        assert_live_totals(&tree);
    }

    #[test]
    fn regression_placeholder_is_counted_once_when_create_matures() {
        let mut tree = sample_tree();
        // 与 WatcherSession 提前物化目录采用同一记账方式。
        tree.upsert_incremental(12, 5, 0, true, 0, 0, 0, "pending");
        tree.dir_count += 1;
        let mut engine = engine(vec![]);
        engine.ingest(raw(frn(20, 0), frn(12, 0), USN_REASON_FILE_CREATE, "child.bin", ATTR_FILE), 1000);
        engine.flush(&mut tree, 1100, |_| Some((MB, 2 * MB, 0)));
        engine.ingest(raw(frn(12, 0), frn(5, 1), USN_REASON_FILE_CREATE, "pending", ATTR_DIR), 1200);
        engine.flush(&mut tree, 1300, |_| None);
        assert_live_totals(&tree);
        assert_eq!((tree.file_count, tree.dir_count), (2, 3));
        engine.ingest(raw(frn(12, 0), frn(5, 1), USN_REASON_FILE_DELETE, "pending", ATTR_DIR), 1400);
        engine.flush(&mut tree, 1500, |_| None);
        assert_live_totals(&tree);
        assert_eq!((tree.file_count, tree.dir_count), (1, 2));
    }

    #[test]
    fn deleted_file_updates_aggregate() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        // 先"创建"一个节点再删
        t.upsert_incremental(20, 10, 1, false, 3 * MB, 3 * MB, 0, "tmp.log");
        core_index::aggregate::aggregate(&mut t);
        assert_eq!(t.arena.get(10).unwrap().size, 8 * MB);

        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_DELETE, "tmp.log", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| None);
        assert_eq!(out.applied, 1);
        assert_eq!(t.arena.get(10).unwrap().size, 5 * MB);
        assert!(!t.arena.get(20).unwrap().in_use());
    }

    #[test]
    fn modified_file_applies_delta() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        // 文件 11 从 5MB 涨到 9MB
        e.ingest(raw(frn(11, 1), frn(10, 1), USN_REASON_DATA_EXTEND, "existing.log", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((9 * MB, 9 * MB, 1_700_000_200)));
        assert_eq!(out.applied, 1);
        assert_eq!(t.arena.get(11).unwrap().size, 9 * MB);
        assert_eq!(t.arena.get(10).unwrap().size, 9 * MB);
    }

    #[test]
    fn shrinking_file_applies_negative_delta() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(11, 1), frn(10, 1), USN_REASON_DATA_TRUNCATION, "existing.log", ATTR_FILE), 1000);
        e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));
        assert_eq!(t.arena.get(11).unwrap().size, 1 * MB);
        assert_eq!(t.arena.get(10).unwrap().size, 1 * MB);
    }

    #[test]
    fn directory_size_is_never_overwritten() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        let before = t.arena.get(10).unwrap().size;
        // 目录本身的 $DATA 变更
        e.ingest(raw(frn(10, 1), frn(5, 1), USN_REASON_DATA_EXTEND, "logs", ATTR_DIR), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((999 * MB, 999 * MB, 0)));
        assert_eq!(out.applied, 1);
        assert_eq!(
            t.arena.get(10).unwrap().size,
            before,
            "目录聚合值不应被单条事件覆写"
        );
    }

    #[test]
    fn rename_updates_name() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(11, 1), frn(10, 1), USN_REASON_RENAME_OLD_NAME, "existing.log", ATTR_FILE), 1000);
        e.ingest(raw(frn(11, 1), frn(10, 1), USN_REASON_RENAME_NEW_NAME, "renamed.log", ATTR_FILE), 1010);
        let out = e.tick(&mut t, 2000, |_| None);
        assert_eq!(out.applied, 1);
        assert_eq!(t.name(11), "renamed.log");
        // 大小不变
        assert_eq!(t.arena.get(10).unwrap().size, 5 * MB);
    }

    #[test]
    fn move_between_directories_rebalances() {
        let mut t = sample_tree();
        // 再加一个目标目录 12
        t.upsert_incremental(12, 5, 1, true, 0, 0, 0, "archive");
        core_index::aggregate::aggregate(&mut t);
        let mut e = engine(vec![]);

        // 把 11 从 logs(10) 移到 archive(12)
        e.ingest(raw(frn(11, 1), frn(12, 1), USN_REASON_RENAME_NEW_NAME, "existing.log", ATTR_FILE), 1000);
        e.tick(&mut t, 2000, |_| None);

        assert_eq!(t.arena.get(11).unwrap().parent, 12);
        assert_eq!(t.arena.get(10).unwrap().size, 0, "旧目录应被扣空");
        assert_eq!(t.arena.get(12).unwrap().size, 5 * MB, "新目录应收到");
    }

    #[test]
    fn frn_reuse_handled_as_delete_plus_create() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        // 记录 11 原本 seq=1；现在来一个 seq=2 的 create（记录被复用）
        e.ingest(raw(frn(11, 2), frn(10, 1), USN_REASON_FILE_CREATE, "brand-new.bin", ATTR_FILE), 1000);
        e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));
        assert_eq!(t.arena.get(11).unwrap().seq, 2);
        assert_eq!(t.name(11), "brand-new.bin");
        assert_eq!(t.arena.get(11).unwrap().size, 1 * MB);
        assert_eq!(t.arena.get(10).unwrap().size, 1 * MB, "旧 5MB 应被扣掉");
    }

    #[test]
    fn threshold_alert_fires_once_and_only_once() {
        let mut t = sample_tree();
        let mut e = engine(vec![Rule::absolute(6 * MB)]);

        // 一次灌入 5MB 新文件，使 logs 从 5MB 涨到 10MB
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "big.log", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((5 * MB, 5 * MB, 0)));
        assert_eq!(out.alerts.len(), 1, "应触发一次预警");
        assert_eq!(out.alerts[0].kind, crate::rules::RuleKind::Absolute);
        assert!(out.notification.is_some());

        // 继续增长：滞回不应重复触发
        e.ingest(raw(frn(21, 1), frn(10, 1), USN_REASON_FILE_CREATE, "big2.log", ATTR_FILE), 3000);
        let out2 = e.tick(&mut t, 4000, |_| Some((2 * MB, 2 * MB, 0)));
        assert!(out2.alerts.is_empty(), "滞回期内不应重复预警");
    }

    #[test]
    fn cooldown_blocks_repeat_after_reset() {
        let mut t = sample_tree();
        let mut e = engine(vec![Rule::absolute(6 * MB)]);

        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "big.log", ATTR_FILE), 1000);
        assert_eq!(e.tick(&mut t, 2000, |_| Some((5 * MB, 5 * MB, 0))).alerts.len(), 1);

        // 删掉 → 大小回落，滞回复位
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_DELETE, "big.log", ATTR_FILE), 3000);
        assert!(e.tick(&mut t, 4000, |_| None).alerts.is_empty());

        // 冷却期内再灌 → 被抑制
        e.ingest(raw(frn(21, 1), frn(10, 1), USN_REASON_FILE_CREATE, "again.log", ATTR_FILE), 5000);
        assert!(e.tick(&mut t, 6000, |_| Some((5 * MB, 5 * MB, 0))).alerts.is_empty());
        assert!(e.rules.stats.suppressed_by_cooldown > 0);
    }

    #[test]
    fn debounce_merges_edit_storm() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        // 模拟编辑器保存：6 个事件
        for (i, reason) in [
            USN_REASON_FILE_CREATE,
            USN_REASON_DATA_EXTEND,
            USN_REASON_DATA_OVERWRITE,
            USN_REASON_BASIC_INFO_CHANGE,
            USN_REASON_CLOSE,
            USN_REASON_CLOSE,
        ]
        .iter()
        .enumerate()
        {
            e.ingest(raw(frn(20, 1), frn(10, 1), *reason, "doc.txt", ATTR_FILE), 1000 + i as u64 * 10);
        }
        let out = e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));
        assert_eq!(out.applied, 1, "6 个事件应合并为 1 条");
    }

    #[test]
    fn create_then_delete_in_window_is_cancelled() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "temp.bin", ATTR_FILE), 1000);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_DELETE, "temp.bin", ATTR_FILE), 1050);
        let out = e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));
        assert_eq!(out.applied, 0, "创建+删除应完全抵消");
        assert_eq!(t.arena.get(10).unwrap().size, 5 * MB);
    }

    #[test]
    fn backpressure_is_reported() {
        let mut cfg = EngineConfig::default();
        cfg.write_changelog = false;
        cfg.queue_capacity = 2;
        let mut e = MonitorEngine::new(cfg);
        for i in 0..10u64 {
            e.enqueue(raw(frn(100 + i, 1), frn(5, 1), USN_REASON_FILE_CREATE, "x", ATTR_FILE));
        }
        assert!(e.stats.queue_dropped >= 8);
        let mut t = sample_tree();
        let out = e.tick(&mut t, 1000, |_| Some((1, 1, 0)));
        assert!(out.backpressure);
    }

    #[test]
    fn size_probe_missing_does_not_crash() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "ghost.bin", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| None);
        assert_eq!(out.applied, 1);
        assert_eq!(t.arena.get(20).unwrap().size, 0);
    }

    #[test]
    fn modified_on_unknown_node_is_ignored() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        let before = t.arena.get(5).unwrap().size;
        // 节点 19 从没出现过
        e.ingest(raw(frn(19, 1), frn(5, 1), USN_REASON_DATA_EXTEND, "unknown", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((7 * MB, 7 * MB, 0)));
        assert_eq!(out.applied, 1);
        assert_eq!(t.arena.get(5).unwrap().size, before, "不应凭空注入大小");
    }

    #[test]
    fn flush_emits_pending_events() {
        let mut t = sample_tree();
        let mut e = engine(vec![]);
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "a.log", ATTR_FILE), 1000);
        // 窗口没到
        assert_eq!(e.tick(&mut t, 1050, |_| Some((1, 1, 0))).applied, 0);
        // flush 强制吐出
        assert_eq!(e.flush(&mut t, 1060, |_| Some((1, 1, 0))).applied, 1);
    }

    #[test]
    fn changelog_records_are_written() {
        use crate::changelog::ChangeLog;
        let owned = core_io::test_dir::TestDir::new("engine_log").unwrap();
        let dir = owned.path().to_path_buf();
        let path = dir.join("log.jsonl");

        let mut t = sample_tree();
        let mut cfg = EngineConfig::default();
        cfg.debounce.window_ms = 50;
        let mut e = MonitorEngine::new(cfg).with_changelog(ChangeLog::open(&path).unwrap());
        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "logged.bin", ATTR_FILE), 1000);
        e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("logged.bin"));
        assert!(text.contains("\"kind\":\"created\""));
        // JSONL 里的反斜杠是被转义的（写成 `C:\\logs`），所以直接在原始行里
        // 找 `C:\logs` 会永远失败 —— 那是把它当成纯文本而不是 JSON 了。
        // 解析出来再比对解码后的值才是可靠写法。
        let line = text
            .lines()
            .find(|l| l.contains("logged.bin"))
            .expect("changelog 里应有一行 logged.bin");
        let obj = core_io::jsonlite::parse_flat_object(line);
        let p = core_io::jsonlite::get(&obj, "path").unwrap_or_default();
        assert!(p.contains(r"C:\logs"), "解码后的 path 不对：{}", p);
    }

    /// 回归：降级模式指定深目录做扫描根时，**变更流水落盘的也必须是真实路径**。
    ///
    /// `Tree::path_of` 的前缀恒为盘符，所以 `--root=D:\projects` 时引擎拼出的是
    /// `C:\...` 这种磁盘上并不存在的路径。显示层一直都在替换前缀，而落盘这条路
    /// 一开始漏了 —— 结果是「屏幕上的路径是对的、日志里是错的」，
    /// 用户拿日志回溯问题时才会发现对不上，是最难查的那种不一致。
    #[test]
    fn changelog_paths_are_remapped_to_real_scan_root() {
        use crate::changelog::ChangeLog;
        let owned = core_io::test_dir::TestDir::new("engine_remap").unwrap();
        let dir = owned.path().to_path_buf();
        let path = dir.join("log.jsonl");

        let mut t = sample_tree();
        let synthetic = t.path_of(20);
        assert!(
            synthetic.starts_with(r"C:\"),
            "前提：合成路径以盘符开头，实际 {}",
            synthetic
        );

        let mut cfg = EngineConfig::default();
        cfg.debounce.window_ms = 50;
        let mut e = MonitorEngine::new(cfg)
            .with_changelog(ChangeLog::open(&path).unwrap())
            .with_path_remap(PathRemap::new("C:", r"D:\projects").expect("应建立重映射"));
        e.ingest(
            raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "remapped.bin", ATTR_FILE),
            1000,
        );
        e.tick(&mut t, 2000, |_| Some((1 * MB, 1 * MB, 0)));

        let text = std::fs::read_to_string(&path).unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("remapped.bin"))
            .expect("changelog 里应有一行 remapped.bin");
        let obj = core_io::jsonlite::parse_flat_object(line);
        let p = core_io::jsonlite::get(&obj, "path").unwrap_or_default();
        assert!(
            p.starts_with(r"D:\projects\"),
            "日志路径必须换成真实扫描根，实际 {}",
            p
        );
        assert!(
            !p.starts_with(r"C:\"),
            "日志里不该再出现合成路径，实际 {}",
            p
        );
    }

    #[test]
    fn notification_merges_multiple_alerts() {
        let mut t = sample_tree();
        t.upsert_incremental(12, 5, 1, true, 0, 0, 0, "other");
        core_index::aggregate::aggregate(&mut t);

        let mut cfg = EngineConfig::default();
        cfg.write_changelog = false;
        cfg.debounce.window_ms = 50;
        let mut e = MonitorEngine::new(cfg);
        e.add_target(10, r"C:\logs", vec![Rule::absolute(1 * MB)], 5 * MB, 0);
        e.add_target(12, r"C:\other", vec![Rule::absolute(1 * MB)], 0, 0);

        e.ingest(raw(frn(20, 1), frn(10, 1), USN_REASON_FILE_CREATE, "a", ATTR_FILE), 1000);
        e.ingest(raw(frn(21, 1), frn(12, 1), USN_REASON_FILE_CREATE, "b", ATTR_FILE), 1000);
        let out = e.tick(&mut t, 2000, |_| Some((3 * MB, 3 * MB, 0)));

        assert_eq!(out.alerts.len(), 2);
        let n = out.notification.expect("应有合并通知");
        assert_eq!(n.count, 2);
        assert!(n.title.contains("2 条"));
    }

    #[test]
    fn frn_helpers() {
        assert_eq!(recno_of(frn(1234, 7)), 1234);
        assert_eq!(seq_of(frn(1234, 7)), 7);
    }
}
