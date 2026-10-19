//! GUI 侧的共享状态。
//!
//! 一个进程里同一时刻只允许存在**一个**已加载的扫描结果（切盘符就丢掉重来）。
//! 这不是偷懒：一次全量扫描的索引本身就可能上百 MB，缓存多份会直接顶穿
//! 文档 §5.5 的内存预算。
//!
//! # 为什么要把 `ChildIndex` / `ExtensionTable` 一起缓存
//!
//! 它们都是 `O(节点数)` 的构建结果，而界面上每一次「展开文件夹」「切换排序」
//! 「重画面积图」都要用。丢掉就变成「每帧重建一次索引」—— 200 万节点下
//! 界面会卡到不能用。所以扫描结束时建一次，之后只读共享。
//!
//! # 监控会改树，所以缓存要认「代数」
//!
//! 实时监控（F6）会在原地改索引树：新增/删除/改名/体积变化。派生索引一旦
//! 落后于树，界面就会显示**过期但看起来正常**的数据 —— 这正是本项目最讨厌的
//! 那类错误。
//!
//! 处理办法是给树挂一个**代数计数器**：监控线程每应用一批事件就 +1，
//! 每份派生索引记住自己是从哪一代建出来的。谁要用派生索引，先
//! [`Loaded::refresh_derived`]，只在代数不匹配时才重建。
//!
//! 这样做的好处是**两边都不吃亏**：
//! * 用户不动界面时，一次重建都不发生（不监控也一样是零开销）；
//! * 用户一旦要看，拿到的必然是与当前树一致的索引，不存在"过期"这个状态。

use core_index::{ChildIndex, ExtensionTable, Tree};
use core_scan::Session;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 一次已完成的扫描。
pub struct Loaded {
    pub session: Session,
    /// 直接子节点索引：`Expand` 一次就能拿到某个目录的所有孩子，不用全表扫。
    /// **私有**：它可能落后于树，必须经 [`Loaded::refresh_derived`] 之后再取。
    children: ChildIndex,
    /// `children` 是从哪一代建出来的
    children_gen: u64,
    /// 扩展名聚合表。**按需构建**：它要为每个文件分配一次扩展名字符串，
    /// 200 万文件下是一笔实打实的开销（文档 §5.5 的内存预算），
    /// 而只看看面积图的用户根本用不到它 —— 所以拖到第一次真正需要时再建。
    exts: Option<ExtensionTable>,
    exts_gen: u64,
    /// 树的变更代数。监控线程持有它的一份 `Arc`，每应用一批事件就 +1。
    generation: Arc<AtomicU64>,
}

impl Loaded {
    pub fn new(session: Session) -> Loaded {
        let children = ChildIndex::build(&session.tree);
        Loaded {
            session,
            children,
            children_gen: 0,
            exts: None,
            exts_gen: 0,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 当前代数。前端拿它决定「要不要重画面积图」。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 把代数计数器交给监控线程（它每应用一批事件就 +1）。
    pub fn generation_arc(&self) -> Arc<AtomicU64> {
        self.generation.clone()
    }

    /// 让派生索引与当前的树对齐。**只在代数变了时重建**。
    ///
    /// 每个要读派生索引的命令都必须先调它 —— 这是「不会显示过期数据」的
    /// 唯一保证点，所以宁可多写一行，也不要让字段可以被直接读到。
    pub fn refresh_derived(&mut self) {
        let g = self.generation();
        if self.children_gen != g {
            self.children = ChildIndex::build(&self.session.tree);
            self.children_gen = g;
        }
        // 扩展名表**已经建过**才跟着重建：没建过说明用户还没用到它，
        // 替它买单就违背了"按需构建"的初衷。
        if self.exts.is_some() && self.exts_gen != g {
            self.exts = Some(ExtensionTable::build(&self.session.tree));
            self.exts_gen = g;
        }
    }

    /// 确保扩展名表已构建（幂等），并顺带对齐两份派生索引。
    pub fn ensure_exts(&mut self) {
        if self.exts.is_none() {
            self.exts = Some(ExtensionTable::build(&self.session.tree));
            self.exts_gen = self.generation();
        }
        self.refresh_derived();
    }

    pub fn children(&self) -> &ChildIndex {
        &self.children
    }

    /// 取扩展名表。**调用前必须先 `ensure_exts()` 或 `refresh_derived()`**。
    ///
    /// 方法名不叫 `exts`：字段也叫 `exts`，`l.exts()` 与 `l.exts` 只差一个括号，
    /// 读代码的人会以为在读字段。
    pub fn ext_table(&self) -> &ExtensionTable {
        self.exts
            .as_ref()
            .expect("调用前必须 ensure_exts() 或 refresh_derived()")
    }

    pub fn tree(&self) -> &Tree {
        &self.session.tree
    }
}

/// 一次扫描的收尾摘要（给状态栏与错误提示用）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanSummary {
    pub ok: bool,
    /// 实际使用的后端（中文标签）
    pub backend: String,
    /// 扫描根**节点下标**。前端不要硬编码 5 —— 虽然 NTFS 的根目录
    /// 固定是 MFT 记录 5，但那是引擎的实现细节，写进界面就会在下一次改动时炸。
    pub root_node: u32,
    pub root: String,
    pub dirs: u64,
    pub files: u64,
    pub logical: u64,
    pub alloc: u64,
    pub memory_bytes: u64,
    /// 扫描耗时（毫秒）
    pub wall_ms: u64,
    /// 扫描过程中的错误条目数（权限/占用/竞态）
    pub errors: u64,
    /// 被跳过的重解析点
    pub skipped_reparse: u64,
    pub warnings: Vec<String>,
    /// 增量来源（USN / ReadDirectoryChangesW / 无）
    pub incremental: String,
    pub error: Option<String>,
}

impl ScanSummary {
    pub fn failed(msg: impl Into<String>) -> ScanSummary {
        ScanSummary {
            ok: false,
            backend: String::new(),
            root_node: 0,
            root: String::new(),
            dirs: 0,
            files: 0,
            logical: 0,
            alloc: 0,
            memory_bytes: 0,
            wall_ms: 0,
            errors: 0,
            skipped_reparse: 0,
            warnings: Vec::new(),
            incremental: String::new(),
            error: Some(msg.into()),
        }
    }
}

#[derive(Default)]
pub struct AppState {
    pub loaded: Option<Loaded>,
    /// 是否有扫描线程在跑（用来拒绝并发扫描）
    pub scanning: bool,
    pub last: Option<ScanSummary>,
    /// 最近一次扫描的盘符（"用户当前在看哪块盘"）。
    ///
    /// 后台周期任务靠它决定该刷谁：扫**别的**盘会把进程里唯一的扫描结果换掉，
    /// 等于偷偷把用户正在看的视图换掉（见 [`crate::scheduler`]）。
    pub last_drive: Option<String>,
}

impl AppState {
    /// 当前已加载的扫描结果，没有就报错。
    ///
    /// 每个命令都要用它，所以统一在这里给出可读的错误文案 ——
    /// 前端直接把它显示出来比「undefined 相关错误」有用得多。
    pub fn loaded(&self) -> Result<&Loaded, String> {
        self.loaded
            .as_ref()
            .ok_or_else(|| "还没有扫描结果，请先在左侧选择一个盘符或目录开始扫描".to_string())
    }

    pub fn loaded_mut(&mut self) -> Result<&mut Loaded, String> {
        self.loaded
            .as_mut()
            .ok_or_else(|| "还没有扫描结果，请先在左侧选择一个盘符或目录开始扫描".to_string())
    }
}

/// Tauri 管理状态用的别名。
pub type Shared = Mutex<AppState>;
