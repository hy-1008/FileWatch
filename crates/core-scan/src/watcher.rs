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
use std::os::windows::fs::MetadataExt;
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

        self.reconcile(tree, now_ms);

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
            .retain(|_, e| now_ms.saturating_sub(e.at_ms) < ttl);
        out
    }

    /// 收尾：把 debounce 池里剩余事件全部吐出。
    pub fn flush(&mut self, tree: &mut Tree, engine: &mut MonitorEngine, now_ms: u64) -> TickOutcome {
        let mut evs: Vec<FsEvent> = Vec::with_capacity(256);
        self.watcher.drain(65_536, &mut evs);
        for ev in evs.iter() {
            self.handle(ev, tree, engine, now_ms);
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
        let md = std::fs::metadata(&abs).ok();
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let name = last_component(&ev.rel);
        let idx = self.alloc_idx(tree);
        let mtime = md
            .as_ref()
            .map(|m| core_usn::record::filetime_to_unix(m.last_write_time() as i64))
            .unwrap_or(0);

        if is_dir {
            // ① 立刻物化占位节点：父链必须完整，否则子文件的 delta 传不上来
            tree.upsert_incremental(idx, parent_idx, 0, true, 0, 0, mtime, name);
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
            let prefix = format!("{}\\", key);
            self.map.retain(|k, _| !k.starts_with(prefix.as_str()));
        }
        self.placeholders.retain(|p| p.idx != idx);

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

        // 目录整体搬家 → 子树的键全部要跟着改前缀
        if tree.node(idx).map(|n| n.is_dir()).unwrap_or(false) {
            self.rekey(&old_key, &new_key);
            for p in self.placeholders.iter_mut() {
                if let Some(tail) = p.key.strip_prefix(old_key.as_ref()) {
                    if tail.starts_with('\\') {
                        p.key = format!("{}{}", new_key, tail).into_boxed_str();
                    }
                }
            }
        } else {
            self.map.remove(old_key.as_ref());
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

    /// 把 `old_key` 及其所有子孙的键改写成 `new_key` 前缀。
    ///
    /// O(映射规模)，只在**目录搬家**时触发——这类操作本来就不频繁，
    /// 换来的是「移动一个大目录后立刻还能定位到里面的文件」。
    fn rekey(&mut self, old_key: &str, new_key: &str) {
        let prefix = format!("{}\\", old_key);
        let moved: Vec<(Box<str>, u32)> = self
            .map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix.as_str()))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (k, v) in moved {
            let tail = &k[old_key.len()..];
            self.map.remove(&k);
            self.map.insert(format!("{}{}", new_key, tail).into_boxed_str(), v);
            self.stats.rekeyed += 1;
        }
    }

    /// 对账：占位目录过了窗口期还在索引里，就回查磁盘确认它真的存在。
    ///
    /// 不存在的（"创建 + 删除"在同一窗口内互相抵消）要摘掉节点并做负向上传，
    /// 否则会留下一个体积为 0 的幽灵目录。
    fn reconcile(&mut self, tree: &mut Tree, now_ms: u64) {
        if self.placeholders.is_empty() {
            return;
        }
        let mut keep: Vec<Placeholder> = Vec::with_capacity(self.placeholders.len());
        for p in std::mem::take(&mut self.placeholders) {
            // 键已被改写/移除 → 说明节点本身已经被别的事件处理过
            if self.map.get(p.key.as_ref()) != Some(&p.idx) {
                continue;
            }
            if now_ms.saturating_sub(p.at_ms) < RECONCILE_AFTER_MS {
                keep.push(p);
                continue;
            }
            if Path::new(p.key.as_ref()).exists() {
                continue; // 真的存在，不再跟踪
            }
            if let Some((size, alloc)) = tree.remove_node(p.idx) {
                self.map.remove(p.key.as_ref());
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
    fn key_of(&self, rel: &str) -> String {
        let rel = rel.trim_start_matches('\\').replace('/', "\\");
        if self.root_lower.ends_with('\\') {
            format!("{}{}", self.root_lower, rel.to_lowercase())
        } else {
            format!("{}\\{}", self.root_lower, rel.to_lowercase())
        }
    }

    /// 小写键 → 真实可访问的绝对路径（大小写按监视根还原）。
    fn abs_of(&self, rel: &str) -> String {
        let rel = rel.trim_start_matches('\\').replace('/', "\\");
        if self.root_abs.ends_with('\\') {
            format!("{}{}", self.root_abs, rel)
        } else {
            format!("{}\\{}", self.root_abs, rel)
        }
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

/// `c:\a\b\c.txt` → `c:\a\b`
pub fn parent_key(key: &str) -> Option<&str> {
    // 盘根（`c:\`）没有父目录
    if key.len() <= 3 {
        return None;
    }
    let p = key.rfind('\\')?;
    // 最后一个分隔符正落在 `X:\` 的位置 → 父节点就是盘根本身，且必须**带上
    // 反斜杠**返回 `X:\`。因为索引里的盘根是 `normalize_root` 存进去的形态
    // （盘根保留尾部反斜杠），少了这个反斜杠，`c:\a` 的父会查成 `c:`，
    // 在索引里永远找不到 —— 表现为「以盘根为扫描根时，第一层子项全部
    // 变成未解析路径」。
    if p == 2 {
        return Some(&key[..3]);
    }
    if p < 2 {
        return None;
    }
    Some(&key[..p])
}

/// `sub\a.txt` → `a.txt`；`sub` → `sub`
pub fn last_component(rel: &str) -> &str {
    let rel = rel.trim_end_matches('\\');
    match rel.rfind('\\') {
        Some(p) if p + 1 < rel.len() => &rel[p + 1..],
        _ => rel,
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
    let md = std::fs::metadata(abs).ok()?;
    if md.is_dir() {
        return None;
    }
    let len = md.len();
    let alloc = if cluster == 0 {
        len
    } else {
        len.div_ceil(cluster) * cluster
    };
    let mtime = core_usn::record::filetime_to_unix(md.last_write_time() as i64);
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

    #[test]
    fn parent_key_handles_drive_root() {
        assert_eq!(parent_key("c:\\a\\b\\c.txt"), Some("c:\\a\\b"));
        assert_eq!(parent_key("c:\\a"), Some("c:\\"));
        assert_eq!(parent_key("c:\\"), None);
        assert_eq!(parent_key("c:"), None);
    }

    #[test]
    fn last_component_splits_correctly() {
        assert_eq!(last_component("a.txt"), "a.txt");
        assert_eq!(last_component(r"sub\a.txt"), "a.txt");
        assert_eq!(last_component(r"a\b\c"), "c");
        assert_eq!(last_component(r"sub\"), "sub");
    }

    #[test]
    fn key_and_abs_join_without_double_separator() {
        let build = |root: &str, rel: &str| {
            if root.ends_with('\\') {
                format!("{}{}", root, rel)
            } else {
                format!("{}\\{}", root, rel)
            }
        };
        assert_eq!(build("c:\\users\\x", r"sub\a.txt"), r"c:\users\x\sub\a.txt");
        assert_eq!(build("c:\\", r"sub\a.txt"), r"c:\sub\a.txt");
    }

    #[test]
    fn probe_path_reports_allocated_not_smaller_than_logical() {
        let dir = std::env::temp_dir().join(format!("fw_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.bin");
        std::fs::write(&f, vec![b'z'; 100]).unwrap();

        let (len, alloc, _m) = probe_path(&f.to_string_lossy(), 4096).unwrap();
        assert_eq!(len, 100);
        assert_eq!(alloc, 4096, "按簇对齐");

        // 目录必须被拒绝——它的体积是聚合值
        assert!(probe_path(&dir.to_string_lossy(), 4096).is_none());
        // 不存在的路径
        assert!(probe_path(r"C:\definitely\not\here_zzz", 4096).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_root_is_shared_contract() {
        // 键的拼接必须与 traverse 里收集路径时完全一致，
        // 否则降级模式的实时增量会一条都对不上
        assert_eq!(normalize_root("C:\\Users\\x\\"), "C:\\Users\\x");
        assert_eq!(normalize_root("C:\\"), "C:\\");
        assert_eq!(normalize_root("C:\\Users\\x").to_lowercase(), "c:\\users\\x");
    }
}
