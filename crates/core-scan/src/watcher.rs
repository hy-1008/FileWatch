//! 降级模式的实时增量：把 `ReadDirectoryChangesW` 的**路径事件**
//! 翻译成监控引擎能吃的 **FRN 事件**。
//!
//! # 核心难题
//!
//! 引擎（[`core_monitor::MonitorEngine`]）全程按 `FRN → 节点下标` 做 O(1) 定位，
//! 因为 USN 记录天生带 FRN。但降级模式有两条限制：
//!
//! 1. 遍历出来的节点下标是**自增序号**，磁盘上没有任何东西对应它；
//! 2. `ReadDirectoryChangesW` 只给**相对路径字符串**。
//!
//! 所以这里必须维护一张 `小写绝对路径 → 节点下标` 的映射（[`WatcherSession::map`]），
//! 它是两个世界之间唯一的桥。映射在**遍历扫描时顺带建立**（零额外成本），
//! 之后由本模块在每次结构变更时同步维护。
//!
//! ```text
//!  ReadDirectoryChangesW                      MonitorEngine
//!   "sub\a.txt" ADDED        ┌──────────┐      frn=0x…0102 create
//!  ────────────────────────► │  路径映射  │ ───────────────────►
//!   "sub\a.txt" MODIFIED     │ path→idx │      frn=0x…0102 modify
//!  ────────────────────────► └──────────┘
//! ```
//!
//! # 两个必须补偿的语义差
//!
//! **① 父节点必须先存在。** `propagate_from_parent` 遇到 `!in_use` 的祖先会
//! **立即停止上传**。而内核会先投递「新建目录 `d`」再投递「新建 `d\f.bin`」，
//! 两条事件可能落在**同一个 debounce 窗口**里、在同一轮 tick 才被应用——
//! 那时 `d` 的节点还不存在，`f.bin` 的体积就永远传不到 `d` 和更上层。
//!
//! 解法：见到「新增目录」时**立刻**用 [`Tree::upsert_incremental`] 以 `size=0`
//! 物化一个占位节点。占位不贡献任何体积，随后引擎自己应用 Created 事件时
//! 会覆盖它，不会重复计数。
//!
//! **② 「创建 + 删除」在窗口内会互相抵消。** debounce 会把这一对合成
//! `Unknown` 并丢弃。对文件无所谓（节点从来没建过），但占位目录会残留成
//! `size=0` 的幽灵节点。因此要有一轮**轻量对账**：窗口过后回查磁盘，
//! 目录没了就把节点摘掉并做负向上传。

use crate::dirwatch::{DirWatcher, FsEvent};
use core_index::arena::INVALID_PARENT;
use core_index::aggregate::propagate_delta;
use core_index::Tree;
use core_io::winapi::*;
use core_monitor::{MonitorEngine, RawEvent, TickOutcome};
use core_ntfs::filename::make_frn;
use core_usn::normalize::{
    USN_REASON_DATA_EXTEND, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
};
use core_index::RECNO_MASK;
use std::collections::HashMap;
use std::path::Path;

/// 占位目录「多久之后回查磁盘」。取 2 倍默认 debounce 窗口（400 ms），
/// 保证窗口内的「创建 + 删除」已经互相抵消完。
const RECONCILE_AFTER_MS: u64 = 1_200;

#[derive(Debug, Clone, Default)]
pub struct WatchStats {
    /// 从内核收到的通知总数
    pub notifications: u64,
    pub created: u64,
    pub removed: u64,
    pub modified: u64,
    pub renamed: u64,
    /// 路径的父目录还不在映射里（通常是监视根之外，或映射已达上限）
    pub unresolved: u64,
    /// 解析出了节点下标但索引里查无此节点
    pub unknown_node: u64,
    /// 目录移动导致的子树键重写次数
    pub rekeyed: u64,
    /// 对账时清理掉的幽灵占位目录数
    pub reconciled: u64,
    /// 因目录移动等原因主动跳过的通知数
    pub skipped: u64,
}

impl WatchStats {
    pub fn summary_cn(&self) -> String {
        format!(
            "目录监视：通知 {}（新增 {} / 删除 {} / 修改 {} / 改名 {}）| 未解析路径 {} | 对账清理 {}",
            self.notifications, self.created, self.removed, self.modified, self.renamed,
            self.unresolved, self.reconciled
        )
    }
}

/// 物化中的占位目录（等待对账）。
struct Placeholder {
    idx: u32,
    /// 小写绝对路径
    key: Box<str>,
    at_ms: u64,
}

/// 一条待回读大小的登记。
///
/// **为什么必须带时间戳而不是「每轮 tick 后清空」**：
/// debounce 窗口意味着在第 N 轮登记的事件要等到第 N+1 轮之后才会成熟
/// （成熟条件是 `now_ms - last_ms >= window_ms`），而大小回读只在事件
/// **真正 apply 的那一刻**才发生。若在 tick 后无条件 `clear()`，
/// 等到成熟那一轮 probe 表已经空了 → 引擎拿不到大小 → 新建/修改的文件
/// 一律被记成 0 字节。这是降级路径最致命的静默错误（不报错，只是数字全错）。
///
/// 改成「登记时间 + TTL，每次收到该文件的通知就刷新」：
/// 写入风暴会持续保鲜，成熟时必然还在表里；TTL 又保证表不会无限增长。
struct ProbeEntry {
    abs: String,
    at_ms: u64,
}

/// 降级模式下的实时增量会话。
pub struct WatcherSession {
    pub watcher: DirWatcher,
    /// **小写绝对路径 → 节点下标**。整个模块的桥。
    map: HashMap<Box<str>, u32>,
    /// 监视根（小写、已归一化）
    root_lower: String,
    /// 监视根的真实形态（保留原始大小写，用于访问文件系统）
    root_abs: String,
    /// 树里代表监视根的节点下标
    pub root_idx: u32,
    pub cluster: u64,
    next_idx: u32,
    placeholders: Vec<Placeholder>,
    /// 改名配对：`RENAMED_OLD_NAME` 存下标，等 `RENAMED_NEW_NAME` 来配
    pending_old: Option<(u32, Box<str>)>,
    /// 待回读大小的节点 → (真实路径, 登记时间)。见 [`ProbeEntry`] 的说明：
    /// **不能**在每轮 tick 后清空，否则事件成熟时拿不到大小。
    probe_paths: HashMap<u32, ProbeEntry>,
    pub stats: WatchStats,
}

impl WatcherSession {
    /// 用一次遍历扫描产出的路径索引建立会话。
    ///
    /// `map` 应当来自 [`crate::traverse::full_scan_traverse_indexed`]，
    /// `root` 与那次遍历的根必须一致。
    pub fn new(
        root: &Path,
        map: HashMap<Box<str>, u32>,
        tree: &Tree,
        buffer_bytes: usize,
        watch_subtree: bool,
    ) -> std::io::Result<WatcherSession> {
        let watcher = DirWatcher::start(root, buffer_bytes, watch_subtree)?;
        let root_abs = watcher.root.clone();
        let root_lower = root_abs.to_lowercase();
        let root_idx = map.get(root_lower.as_str()).copied().unwrap_or(5);
        let cluster = core_io::volumes::cluster_size(&root_abs).unwrap_or(4096);
        Ok(WatcherSession {
            watcher,
            map,
            root_lower,
            root_abs,
            root_idx,
            cluster,
            next_idx: tree.arena.nodes.len() as u32,
            placeholders: Vec::new(),
            pending_old: None,
            probe_paths: HashMap::new(),
            stats: WatchStats::default(),
        })
    }

    /// 路径索引里当前有多少条目。
    pub fn map_len(&self) -> usize {
        self.map.len()
    }

    /// 估算路径索引的常驻内存（用于「常驻 ≤ 300 MB」对账）。
    pub fn map_memory_bytes(&self) -> usize {
        self.map
            .iter()
            .map(|(k, _)| k.len() + 16 + core::mem::size_of::<u32>() + 8)
            .sum()
    }

    /// 当前登记了多少个「待回读大小」的节点。
    ///
    /// 这个数比它看起来重要：为 0 意味着事件成熟时引擎拿不到真实大小，
    /// 新建/修改的文件会被一律记成 0 字节 —— 一个**不报错**的静默错误。
    /// 出问题时先看它。
    pub fn pending_probe_count(&self) -> usize {
        self.probe_paths.len()
    }

    /// 拉取通知 → 翻译 → 入引擎 → 跑一轮 tick。
    pub fn poll(
        &mut self,
        tree: &mut Tree,
        engine: &mut MonitorEngine,
        now_ms: u64,
    ) -> TickOutcome {
        let mut evs: Vec<FsEvent> = Vec::with_capacity(256);
        self.watcher.drain(4096, &mut evs);
        self.stats.notifications += evs.len() as u64;
        for ev in evs.iter() {
            self.handle(ev, tree, engine, now_ms);
        }

        // 回读真实大小的闭包：查登记过的路径，不借用 tree
        let paths = &self.probe_paths;
        let cluster = self.cluster;
        let out = engine.tick(tree, now_ms, |frn: u64| -> Option<(u64, u64, i64)> {
            let recno = (frn & RECNO_MASK) as u32;
            let p = paths.get(&recno)?;
            probe_path(&p.abs, cluster)
        });
        // 只按 TTL 过期淘汰，绝不无条件清空 —— 见 ProbeEntry 的说明
        let ttl = probe_ttl_ms(engine.cfg.debounce.window_ms);
        self.probe_paths
            .retain(|idx, e| {
                engine.debouncer.has_pending(make_frn(*idx as u64, 0))
                    || now_ms.saturating_sub(e.at_ms) < ttl
            });
        self.reconcile(tree, engine, now_ms);
        out
    }

    /// 收尾：把 debounce 池里剩余事件全部吐出。
    pub fn flush(&mut self, tree: &mut Tree, engine: &mut MonitorEngine, now_ms: u64) -> TickOutcome {
        let mut evs: Vec<FsEvent> = Vec::with_capacity(256);
        // Session 收尾先停止生产者，再按批吃完队列；限内存而不截断剩余通知。
        loop {
            evs.clear();
            if self.watcher.drain(4096, &mut evs) == 0 {
                break;
            }
            self.stats.notifications += evs.len() as u64;
            for ev in evs.iter() {
                self.handle(ev, tree, engine, now_ms);
            }
        }
        let paths = &self.probe_paths;
        let cluster = self.cluster;
        let out = engine.flush(tree, now_ms, |frn: u64| -> Option<(u64, u64, i64)> {
            let recno = (frn & RECNO_MASK) as u32;
            let p = paths.get(&recno)?;
            probe_path(&p.abs, cluster)
        });
        // flush 是收尾动作，此时把表清干净是安全的（后面不会再有成熟事件）
        self.probe_paths.clear();
        out
    }

    /// 登记/刷新一个待回读大小的路径。
    fn note_probe(&mut self, idx: u32, abs: String, now_ms: u64) {
        self.probe_paths
            .insert(idx, ProbeEntry { abs, at_ms: now_ms });
    }

    pub fn shutdown(&mut self) {
        self.watcher.stop();
    }

    // ── 事件翻译 ───────────────────────────────────────────────────────────

    fn handle(&mut self, ev: &FsEvent, tree: &mut Tree, engine: &mut MonitorEngine, now_ms: u64) {
        match ev.action {
            FILE_ACTION_ADDED => self.on_added(ev, tree, engine, now_ms),
            FILE_ACTION_REMOVED => self.on_removed(ev, tree, engine, now_ms),
            FILE_ACTION_MODIFIED => self.on_modified(ev, tree, engine, now_ms),
            FILE_ACTION_RENAMED_OLD_NAME => self.on_rename_old(ev, tree, engine, now_ms),
            FILE_ACTION_RENAMED_NEW_NAME => self.on_rename_new(ev, tree, engine, now_ms),
            _ => {
                self.stats.skipped += 1;
            }
        }
    }

    fn on_added(&mut self, ev: &FsEvent, tree: &mut Tree, engine: &mut MonitorEngine, now_ms: u64) {
        let key = self.key_of(&ev.rel);
        // 已经在索引里 → 不是「新增」，多半是覆盖写，按修改处理
        if self.map.contains_key(key.as_str()) {
            self.on_modified(ev, tree, engine, now_ms);
            return;
        }
        let Some(parent_idx) = self.parent_of(&key) else {
            self.stats.unresolved += 1;
            return;
        };

        let abs = self.abs_of(&ev.rel);
        let md = std::fs::symlink_metadata(&abs).ok();
        if md
            .as_ref()
            .map(|m| core_io::fsmeta::is_reparse(m))
            .unwrap_or(false)
        {
            self.stats.skipped += 1;
            return;
        }
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let name = last_component(&ev.rel);
        let idx = self.alloc_idx(tree);
        let mtime = md
            .as_ref()
            .map(core_io::fsmeta::mtime_unix)
            .unwrap_or(0);

        if is_dir {
            // ① 立刻物化占位节点：父链必须完整，否则子文件的 delta 传不上来
            tree.upsert_incremental(idx, parent_idx, 0, true, 0, 0, mtime, name);
            tree.dir_count = tree.dir_count.saturating_add(1);
            self.placeholders.push(Placeholder {
                idx,
                key: key.clone().into_boxed_str(),
                at_ms: now_ms,
            });
        } else {
            // 文件由引擎自己建（它会回读真实大小），这里只登记待回读路径
            self.note_probe(idx, abs, now_ms);
        }

        self.map.insert(key.into_boxed_str(), idx);
        self.stats.created += 1;

        engine.ingest(
            RawEvent {
                frn: make_frn(idx as u64, 0),
                parent_frn: make_frn(parent_idx as u64, 0),
                usn: 0,
                timestamp: unix_now(),
                reason: USN_REASON_FILE_CREATE,
                file_attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
                name: name.to_string(),
            },
            now_ms,
        );
    }

    fn on_removed(
        &mut self,
        ev: &FsEvent,
        tree: &mut Tree,
        engine: &mut MonitorEngine,
        now_ms: u64,
    ) {
        let key = self.key_of(&ev.rel);
        let Some(idx) = self.map.get(key.as_str()).copied() else {
            self.stats.unresolved += 1;
            return;
        };
        // 必须在把键从索引里摘掉**之前**解析父 —— 回退路径要靠索引查
        let parent = self.parent_of_idx(tree, idx, &key);
        self.map.remove(key.as_str());

        // 目录被删 → 整棵子树的键同时失效，一并摘掉
        let is_dir = tree.node(idx).map(|n| n.is_dir()).unwrap_or(false);
        if is_dir {
            let prefix = format!("{}{}", key, core_io::paths::SEP);
            self.map.retain(|k, _| !k.starts_with(prefix.as_str()));
        }
        // 占位目录仍交给对账：创建+删除可能被防抖抵消，树节点需另行清理。

        self.stats.removed += 1;
        engine.ingest(
            RawEvent {
                frn: make_frn(idx as u64, 0),
                parent_frn: make_frn(parent.unwrap_or(INVALID_PARENT) as u64, 0),
                usn: 0,
                timestamp: unix_now(),
                reason: USN_REASON_FILE_DELETE,
                file_attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
                name: last_component(&ev.rel).to_string(),
            },
            now_ms,
        );
    }

    fn on_modified(
        &mut self,
        ev: &FsEvent,
        tree: &mut Tree,
        engine: &mut MonitorEngine,
        now_ms: u64,
    ) {
        let key = self.key_of(&ev.rel);
        let Some(&idx) = self.map.get(key.as_str()) else {
            self.stats.unresolved += 1;
            return;
        };
        // 目录的体积是聚合值，绝不能被单条通知覆写
        if tree.node(idx).map(|n| n.is_dir()).unwrap_or(false) {
            self.stats.skipped += 1;
            return;
        }
        let abs = self.abs_of(&ev.rel);
        self.note_probe(idx, abs, now_ms);
        self.stats.modified += 1;

        // 见 `parent_of_idx`：不能直接读 tree 里的父，节点可能还没物化
        let parent = self.parent_of_idx(tree, idx, &key).unwrap_or(INVALID_PARENT);
        engine.ingest(
            RawEvent {
                frn: make_frn(idx as u64, 0),
                parent_frn: make_frn(parent as u64, 0),
                usn: 0,
                timestamp: unix_now(),
                reason: USN_REASON_DATA_EXTEND,
                file_attributes: 0,
                name: last_component(&ev.rel).to_string(),
            },
            now_ms,
        );
    }

    fn on_rename_old(
        &mut self,
        ev: &FsEvent,
        tree: &mut Tree,
        engine: &mut MonitorEngine,
        now_ms: u64,
    ) {
        let key = self.key_of(&ev.rel);
        let Some(&idx) = self.map.get(key.as_str()) else {
            self.stats.unknown_node += 1;
            return;
        };
        // 改名事件里树上的父仍是**旧位置**，用 `parent_of_idx` 回退到索引
        // 也没有意义（索引里此刻还是旧键），统一按「树优先」即可。
        let parent = self.parent_of_idx(tree, idx, &key).unwrap_or(INVALID_PARENT);
        self.stats.renamed += 1;
        engine.ingest(
            RawEvent {
                frn: make_frn(idx as u64, 0),
                parent_frn: make_frn(parent as u64, 0),
                usn: 0,
                timestamp: unix_now(),
                reason: USN_REASON_RENAME_OLD_NAME,
                file_attributes: 0,
                name: last_component(&ev.rel).to_string(),
            },
            now_ms,
        );
        // 留着等 NEW_NAME 配对；内核保证两者相邻投递
        self.pending_old = Some((idx, key.into_boxed_str()));
    }

    fn on_rename_new(
        &mut self,
        ev: &FsEvent,
        tree: &mut Tree,
        engine: &mut MonitorEngine,
        now_ms: u64,
    ) {
        let Some((idx, old_key)) = self.pending_old.take() else {
            // 没配到旧名（例如旧名在监视范围外）→ 当新增处理
            self.on_added(ev, tree, engine, now_ms);
            return;
        };
        let new_key = self.key_of(&ev.rel);
        // 这里**故意**按路径索引取父（`parent_of`），不用 `parent_of_idx`：
        // 树上的 `parent` 此刻还是**旧位置**，而改名/移动事件要的是新位置。
        let parent_idx = self
            .parent_of(&new_key)
            .unwrap_or(INVALID_PARENT);
        let new_abs = self.abs_of(&ev.rel);

        // 目录整体搬家 → 子树的键全部要跟着改前缀
        if tree.node(idx).map(|n| n.is_dir()).unwrap_or(false) {
            self.stats.rekeyed += remap_directory_paths(
                &mut self.map,
                &mut self.placeholders,
                &mut self.probe_paths,
                &old_key,
                &new_key,
                &new_abs,
                now_ms,
            );
        } else {
            self.map.remove(old_key.as_ref());
            self.note_probe(idx, new_abs, now_ms);
        }
        self.map.insert(new_key.into_boxed_str(), idx);

        engine.ingest(
            RawEvent {
                frn: make_frn(idx as u64, 0),
                parent_frn: make_frn(parent_idx as u64, 0),
                usn: 0,
                timestamp: unix_now(),
                reason: USN_REASON_RENAME_NEW_NAME,
                file_attributes: 0,
                name: last_component(&ev.rel).to_string(),
            },
            now_ms,
        );
    }

    /// 对账：占位目录过了窗口期还在索引里，就回查磁盘确认它真的存在。
    ///
    /// 不存在的（"创建 + 删除"在同一窗口内互相抵消）要摘掉节点并做负向上传，
    /// 否则会留下一个体积为 0 的幽灵目录。
    fn reconcile(&mut self, tree: &mut Tree, engine: &MonitorEngine, now_ms: u64) {
        if self.placeholders.is_empty() {
            return;
        }
        let mut keep: Vec<Placeholder> = Vec::with_capacity(self.placeholders.len());
        for p in std::mem::take(&mut self.placeholders) {
            if now_ms.saturating_sub(p.at_ms) < RECONCILE_AFTER_MS
                || engine.debouncer.has_pending(make_frn(p.idx as u64, 0))
            {
                keep.push(p);
                continue;
            }
            let still_mapped = self.map.get(p.key.as_ref()) == Some(&p.idx);
            if still_mapped {
                match std::fs::symlink_metadata(p.key.as_ref()) {
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {
                        // 权限不足或短暂共享冲突不能被当作删除。
                        keep.push(p);
                        continue;
                    }
                }
            }
            if let Some((size, alloc)) = tree.remove_node(p.idx) {
                tree.dir_count = tree.dir_count.saturating_sub(1);
                if still_mapped {
                    self.map.remove(p.key.as_ref());
                }
                if size != 0 || alloc != 0 {
                    propagate_delta(tree, p.idx, -(size as i64), -(alloc as i64), |_, _| {});
                }
                self.stats.reconciled += 1;
            }
        }
        self.placeholders = keep;
    }

    // ── 路径工具 ───────────────────────────────────────────────────────────

    /// 相对路径 → 小写绝对路径键。
    ///
    /// 拼接口径交给 [`core_io::paths::join`]：它保证卷根不会被拼出双分隔符
    /// （`C:////a` / `//a`），而这份键必须与遍历时收集的键**逐字节一致**，
    /// 否则查表失败会表现为"事件未解析"（不报错）。
    fn key_of(&self, rel: &str) -> String {
        let rel = clean_rel(rel);
        core_io::paths::join(&self.root_lower, &rel.to_lowercase())
    }

    /// 小写键 → 真实可访问的绝对路径（大小写按监视根还原）。
    fn abs_of(&self, rel: &str) -> String {
        let rel = clean_rel(rel);
        core_io::paths::join(&self.root_abs, &rel)
    }

    /// 路径索引精确查找：**小写绝对路径** → 节点下标。
    ///
    /// 给 [`crate::session::Session::find_node_by_path`] 用的「第一步」：
    /// 索引里的键是遍历时从 `entry.path()` 直接收上来的**真实绝对路径**，
    /// 比事后用 [`Tree::path_of`] 反推可靠 —— 反推只能拼出盘符前缀
    /// （扫描根是 `C:\Users\x\Downloads` 时也只会拼成 `C:\...`），
    /// 详见 [`crate::traverse::full_scan_traverse_indexed`] 的说明。
    ///
    /// 调用方负责把输入归一到同样的形态（小写、去尾部反斜杠）。
    pub fn lookup(&self, lower_abs: &str) -> Option<u32> {
        self.map.get(lower_abs).copied()
    }

    /// 取父目录的节点下标。
    fn parent_of(&self, key: &str) -> Option<u32> {
        let p = parent_key(key)?;
        self.map.get(p).copied()
    }

    /// 解析某节点的父下标，**优先用树、失效则回退路径索引**。
    ///
    /// 为什么不能只用 `tree.node(idx).parent`：降级模式下节点是「懒物化」的 ——
    /// watcher 先分配下标并登记进路径索引，真正写进 arena 要等引擎把 debounce
    /// 窗口里的事件 apply 出来。而「同一窗口内先新增、再修改」正是编辑器保存
    /// 文件的常态：第二次事件读到的还是一枚空节点（`parent = INVALID_PARENT`）。
    /// 一旦让这个无效父指针进了引擎，合并后事件的 delta 就再也爬不上父链
    /// ——症状是「文件大小是对的，但所在目录与根的总量纹丝不动」这种**静默**错误，
    /// 比直接报错难查得多。所以：树里的父有效就用它，否则回退到路径索引。
    fn parent_of_idx(&self, tree: &Tree, idx: u32, key: &str) -> Option<u32> {
        if let Some(n) = tree.node(idx) {
            if n.in_use() && n.parent != INVALID_PARENT {
                return Some(n.parent);
            }
        }
        self.parent_of(key)
    }

    /// 分配一个未被占用的节点下标。
    fn alloc_idx(&mut self, tree: &Tree) -> u32 {
        let mut i = self.next_idx.max(tree.arena.nodes.len() as u32);
        while tree.arena.get(i).map(|n| n.in_use()).unwrap_or(false) {
            i += 1;
        }
        self.next_idx = i.saturating_add(1);
        i
    }
}

impl std::fmt::Debug for WatcherSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatcherSession")
            .field("root", &self.watcher.root)
            .field("map_entries", &self.map.len())
            .field("placeholders", &self.placeholders.len())
            .field("stats", &self.stats)
            .finish()
    }
}

// ── 纯函数工具（可单测） ────────────────────────────────────────────────────

/// 目录改名时同步更新目录自身、后代、占位目录与待回读路径。
/// 前缀必须位于路径组件边界，避免误改同名前缀的相邻目录。
fn remap_directory_paths(
    map: &mut HashMap<Box<str>, u32>,
    placeholders: &mut [Placeholder],
    probes: &mut HashMap<u32, ProbeEntry>,
    old_key: &str,
    new_key: &str,
    new_abs: &str,
    now_ms: u64,
) -> u64 {
    let prefix = format!("{}{}", old_key, core_io::paths::SEP);
    let moved: Vec<(Box<str>, u32)> = map
        .iter()
        .filter(|(key, _)| key.as_ref() == old_key || key.starts_with(&prefix))
        .map(|(key, idx)| (key.clone(), *idx))
        .collect();
    for (key, idx) in &moved {
        let tail = &key[old_key.len()..];
        map.remove(key.as_ref());
        map.insert(format!("{}{}", new_key, tail).into_boxed_str(), *idx);
        if let Some(probe) = probes.get_mut(idx) {
            probe.abs = format!("{}{}", new_abs, tail);
            probe.at_ms = now_ms;
        }
    }
    for placeholder in placeholders {
        if placeholder.key.as_ref() == old_key || placeholder.key.starts_with(&prefix) {
            placeholder.key = format!("{}{}", new_key, &placeholder.key[old_key.len()..])
                .into_boxed_str();
        }
    }
    moved.len() as u64
}

/// `C:\a\b\c.txt` → `C:\a\b`（Linux `"/a/b/c.txt"` → `"/a/b"`）
///
/// 卷根**没有父目录**（返回 `None`）。子项的父必须返回**带尾分隔符的卷根**
/// （`C:\` / `/`），因为索引里的卷根是 `normalize_root` 存进去的形态；
/// 少了那个分隔符，`C:\a` 的父会查成 `C:`，在索引里永远找不到 ——
/// 表现为「以卷根为扫描根时，第一层子项全部变成未解析路径」。
pub fn parent_key(key: &str) -> Option<&str> {
    let sep = core_io::paths::SEP;
    // 先去尾分隔符再找：`C:\a\` 与 `C:\a` 的父应当一致
    let trimmed = key.trim_end_matches(sep);
    let p = trimmed.rfind(sep)?;
    // 父本身就是卷根时，返回**带尾分隔符**的那一段
    if core_io::paths::is_volume_root(&trimmed[..p]) {
        return Some(&key[..p + sep.len_utf8()]);
    }
    Some(&key[..p])
}

/// `sub\a.txt` → `a.txt`；`sub` → `sub`
pub fn last_component(rel: &str) -> &str {
    let sep = core_io::paths::SEP;
    let rel = rel.trim_end_matches(sep);
    match rel.rfind(sep) {
        Some(p) if p + sep.len_utf8() < rel.len() => &rel[p + sep.len_utf8()..],
        _ => rel,
    }
}

/// 通知里的相对路径 → 干净的相对路径（去头部分隔符，统一成本平台分隔符）。
///
/// Windows 的 `ReadDirectoryChangesW` 恒给 `\`，Linux 的 inotify 恒给 `/`；
/// 而这里的入参可能来自旧索引、手工构造的测试或跨平台日志，所以两边都认。
fn clean_rel(rel: &str) -> String {
    let t = rel.trim_start_matches(['\\', '/']);
    if cfg!(windows) {
        t.replace('/', "\\")
    } else {
        t.replace('\\', "/")
    }
}

/// 待回读路径登记的有效期（毫秒）。
///
/// 取值必须**明显大于 debounce 窗口**：登记发生在通知到达时，
/// 而事件成熟发生在「最后一次通知 + 窗口」之后，中间还要经过若干次 tick。
/// 8 倍窗口 + 2 秒余量足够覆盖「窗口 + 轮询间隔 + 调度抖动」的最坏叠加，
/// 同时把表的大小限制在「最近这段时间内被写过的文件数」量级。
fn probe_ttl_ms(window_ms: u64) -> u64 {
    window_ms.saturating_mul(8).saturating_add(2_000).max(10_000)
}

/// 读文件当前真实大小 → `(逻辑大小, 占用大小, mtime)`。
///
/// 目录返回 `None`：目录的体积是聚合值，绝不能由外向内覆写。
pub fn probe_path(abs: &str, cluster: u64) -> Option<(u64, u64, i64)> {
    let md = std::fs::symlink_metadata(abs).ok()?;
    if md.is_dir() || core_io::fsmeta::is_reparse(&md) {
        return None;
    }
    let len = md.len();
    let alloc = core_io::fsmeta::allocated(&md, len, cluster);
    let mtime = core_io::fsmeta::mtime_unix(&md);
    Some((len, alloc, mtime))
}

/// 秒级 Unix 时间戳。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    // `normalize_root` 只在测试里用（用来锁定与 dirwatch 的路径规范化契约）。
    // 放进 `mod tests` 而不是文件顶部，是因为 `#[cfg(test)]` 模块在 `cargo build`
    // 下根本不参与编译，顶部导入会变成 dead import 并触发 unused_imports 警告。
    use crate::dirwatch::normalize_root;

    /// 用本平台分隔符拼路径 —— 同一组断言两个平台都能跑。
    fn p(parts: &[&str]) -> String {
        parts.join(core_io::paths::SEP_STR)
    }

    #[test]
    fn directory_rename_updates_all_path_registries_on_component_boundaries() {
        let root = p(&["c:", "root"]);
        let old = p(&["c:", "root", "old"]);
        let new = p(&["c:", "root", "new"]);
        let old_child = p(&["c:", "root", "old", "child.bin"]);
        let older = p(&["c:", "root", "older", "keep.bin"]);

        let mut map: HashMap<Box<str>, u32> = [
            (old.clone().into_boxed_str(), 10),
            (old_child.clone().into_boxed_str(), 11),
            (older.clone().into_boxed_str(), 12),
        ]
        .into_iter()
        .collect();
        let mut placeholders = vec![Placeholder {
            idx: 10,
            key: old.clone().into_boxed_str(),
            at_ms: 1000,
        }];
        let mut probes = HashMap::from([
            (
                11,
                ProbeEntry {
                    abs: old_child.clone(),
                    at_ms: 1000,
                },
            ),
            (
                12,
                ProbeEntry {
                    abs: older.clone(),
                    at_ms: 1000,
                },
            ),
        ]);
        let count = remap_directory_paths(
            &mut map,
            &mut placeholders,
            &mut probes,
            &old,
            &new,
            &p(&["c:", "root", "New"]),
            2000,
        );
        assert_eq!(count, 2);
        assert!(!map.contains_key(old.as_str()));
        assert_eq!(map.get(new.as_str()), Some(&10));
        assert_eq!(map.get(p(&[&new, "child.bin"]).as_str()), Some(&11));
        // 「older」不能被当成「old」的子项 —— 分量边界
        assert_eq!(map.get(older.as_str()), Some(&12));
        assert_eq!(placeholders[0].key.as_ref(), new);
        assert_eq!(probes[&11].abs, p(&["c:", "root", "New", "child.bin"]));
        assert_eq!(probes[&11].at_ms, 2000);
        assert_eq!(probes[&12].abs, older);
        let _ = root;
    }

    /// 父节点必须是**带尾分隔符的卷根**，否则第一层子项全部查不到。
    #[test]
    fn parent_key_handles_volume_root() {
        let r = core_io::paths::root_prefix("c:");
        let a = core_io::paths::join(&r, "a");
        let b = core_io::paths::join(&a, "b");
        let c = core_io::paths::join(&b, "c.txt");
        assert_eq!(parent_key(&c), Some(b.as_str()));
        assert_eq!(parent_key(&a), Some(r.as_str()));
        // 卷根没有父
        assert_eq!(parent_key(&r), None);
        assert_eq!(parent_key("c:"), None);
        // 尾分隔符不影响
        assert_eq!(parent_key(&format!("{a}{}", core_io::paths::SEP)), Some(r.as_str()));
    }

    #[test]
    fn last_component_splits_correctly() {
        assert_eq!(last_component("a.txt"), "a.txt");
        assert_eq!(last_component(&p(&["sub", "a.txt"])), "a.txt");
        assert_eq!(last_component(&p(&["a", "b", "c"])), "c");
        assert_eq!(last_component(&format!("sub{}", core_io::paths::SEP)), "sub");
    }

    #[test]
    fn probe_path_reports_allocated_not_smaller_than_logical() {
        let owned = core_io::test_dir::TestDir::new("probe").unwrap();
        let dir = owned.path().to_path_buf();
        let f = dir.join("x.bin");
        std::fs::write(&f, vec![b'z'; 100]).unwrap();

        let (len, alloc, _m) = probe_path(&f.to_string_lossy(), 4096).unwrap();
        assert_eq!(len, 100);
        assert!(alloc >= 4096, "占用至少一个簇，实际 {alloc}");
        assert_eq!(alloc % 4096, 0, "占用按簇对齐");

        // 目录必须被拒绝——它的体积是聚合值
        assert!(probe_path(&dir.to_string_lossy(), 4096).is_none());
        // 不存在的路径
        let bogus = core_io::paths::join(
            &core_io::paths::root_prefix(core_io::paths::DEFAULT_VOLUME),
            "definitely/not/here_zzz",
        );
        assert!(probe_path(&bogus, 4096).is_none());
    }

    #[test]
    fn normalize_root_is_shared_contract() {
        // 键的拼接必须与 traverse 里收集路径时完全一致，
        // 否则降级模式的实时增量会一条都对不上
        let r = core_io::paths::root_prefix("C:");
        let sub = core_io::paths::join(&r, "Users");
        assert_eq!(normalize_root(&sub), sub);
        assert_eq!(normalize_root(&r), r);
        // 相对路径里混入正斜杠时也要统一（git-bash 会把 C:/x 传成 C:/x）
        assert!(clean_rel("a/b").contains(core_io::paths::SEP) || !cfg!(windows));
    }
}
