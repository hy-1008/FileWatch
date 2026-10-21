//! # core-index —— 紧凑目录树、聚合与统计
//!
//! 分层（文档 §4.2）：**不依赖 UI**，可独立单测。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`arena`] | 40 字节紧凑 [`arena::Node`] 与节点数组（下标 = MFT 记录号） |
//! | [`string_pool`] | 文件名池化，节点只存 `u32` 偏移 |
//! | [`tree`] | 目录树构建、路径拼接、CSR 子节点索引 |
//! | [`aggregate`] | 大小聚合、**O(树深) 增量上传**、Top-N 大文件 |
//! | [`hardlink`] | 硬链接去重说明与「与资源管理器差异」解释 |
//! | [`ext_stats`] | 扩展名统计表（`ext_id` 与面积图共享） |
//! | [`sensitive`] | 按文件名/扩展名盘点敏感文件（零 IO，规则表可枚举） |
//!
//! 内存预算（文档 §5.5，200 万文件）：节点数组 80 MB + 字符串池 ≈48 MB
//! + CSR 子索引 16 MB ≈ **150 MB**，落在「常驻 ≤ 300 MB」的目标内。

pub mod aggregate;
pub mod arena;
pub mod ext_stats;
pub mod hardlink;
pub mod search;
pub mod sensitive;
pub mod string_pool;
pub mod tree;

pub use aggregate::{
    aggregate, propagate_delta, propagate_from_parent, sorted_children, top_files, SizeMode,
};
pub use arena::{recno_of, Arena, Node, INVALID_PARENT, RECNO_MASK};
pub use ext_stats::{ExtensionTable, ExtStat};
pub use hardlink::{build_report, DedupReport};
pub use search::{search, Hit, MatchMode, Outcome, Query, Scope, SimpleRegex};
// 敏感盘点的结果类型叫 `Finding`（不是 `Hit`）—— 与搜索的 `Hit` 区分开。
// 同名会让 check_api 的符号表互相覆盖，报出一堆假错。
pub use sensitive::{
    classify, scan as scan_sensitive, Finding, Options as SensitiveOptions,
    Report as SensitiveReport, Rule as SensitiveRule, Severity, RULES as SENSITIVE_RULES,
};
pub use string_pool::StringPool;
pub use tree::{ChildIndex, Tree};
