//! # core-monitor —— 监控引擎与阈值预警
//!
//! 分层（文档 §4.2）：**不依赖 UI**，可独立单测。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`queue`] | 有界环形队列 + 背压计数（风险 R5） |
//! | [`debounce`] | 事件归一化与合并（编辑器保存风暴 / 回收站 / 临时文件） |
//! | [`rules`] | 三种阈值规则 + 滞回 + 冷却（M3 核心） |
//! | [`alert`] | 内容去重 + 同批合并 + 严重级别 |
//! | [`changelog`] | 变更流水 JSONL 落库与轮转（功能 F8） |
//! | [`pathremap`] | 合成路径（盘符前缀）→ 真实扫描根，显示与落盘共用 |
//! | [`engine`] | 串起上面全部：USN 事件 → 索引更新 → 流水 → 预警 |
//!
//! 关键设计：**回读 MFT 的动作通过 `size_probe` 闭包注入**，
//! 因此本 crate 不依赖 `core-ntfs`，磁盘无关、可单测。

pub mod alert;
pub mod changelog;
pub mod debounce;
pub mod engine;
pub mod pathremap;
pub mod queue;
pub mod rules;

pub use alert::{AlertSink, Notification, Severity};
pub use changelog::ChangeLog;
pub use debounce::{DebounceConfig, Debouncer, PendingEvent, RawEvent};
pub use engine::{
    EngineConfig, EngineStats, MonitorEngine, TickOutcome, DEFAULT_ALERT_DEDUP_MS,
    DEFAULT_MAX_EVENTS_PER_TICK, DEFAULT_POLL_INTERVAL_MS, DEFAULT_QUEUE_CAPACITY,
};
pub use pathremap::PathRemap;
pub use queue::RingQueue;
pub use rules::{Alert, Rule, RuleConfig, RuleEngine, RuleKind, Target};
