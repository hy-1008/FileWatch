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

use core_index::{ChildIndex, ExtensionTable, Tree};
use core_scan::Session;
use serde::Serialize;
use std::sync::Mutex;

/// 一次已完成的扫描。
pub struct Loaded {
    pub session: Session,
    /// `小写路径 → 节点` 之外的**子节点索引**：`Expand` 一次就能拿到某个目录
    /// 的所有孩子，不用全表扫。
    pub children: ChildIndex,
    /// 扩展名聚合表。**按需构建**：它要为每个文件分配一次扩展名字符串，
    /// 200 万文件下是一笔实打实的开销（文档 §5.5 的内存预算），
    /// 而只看看面积图的用户根本用不到它 —— 所以拖到第一次真正需要时再建。
    pub exts: Option<ExtensionTable>,
}

impl Loaded {
    pub fn new(session: Session) -> Loaded {
        let children = ChildIndex::build(&session.tree);
        Loaded { session, children, exts: None }
    }

    /// 确保扩展名表已构建（幂等）。
    pub fn ensure_exts(&mut self) {
        if self.exts.is_none() {
            self.exts = Some(ExtensionTable::build(&self.session.tree));
        }
    }

    /// 取扩展名表。**调用前必须先 `ensure_exts()`**。
    ///
    /// 方法名不叫 `exts`：字段也叫 `exts`，`l.exts()` 与 `l.exts` 只差一个括号，
    /// 读代码的人会以为在读字段。
    pub fn ext_table(&self) -> &ExtensionTable {
        self.exts.as_ref().expect("调用前必须 ensure_exts()")
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
