//! # core-scan —— 扫描调度与会话编排
//!
//! 分层（文档 §4.2）：**不依赖 UI**，可独立单测。
//!
//! 这是「把零件装成整机」的一层：它同时依赖 `core-io` / `core-ntfs` /
//! `core-usn` / `core-index` / `core-monitor`，反过来**没有任何 crate 依赖它**，
//! 因此 CLI 与 GUI 都从这里取同一条装配路径。
//!
//! | 模块 | 职责 | 文档对应 |
//! |---|---|---|
//! | [`full_scan`] | MFT 全量扫描、`probe` 自检、恒定内存 Top-N | M0 / M1 |
//! | [`traverse`] | 无管理员 / 非 NTFS 时的目录遍历降级 | 风险 R2 |
//! | [`dirwatch`] | `ReadDirectoryChangesW` 封装（降级模式的实时事件源） | 风险 R2 |
//! | [`watcher`] | 路径事件 → FRN 事件的翻译层（含路径索引与对账） | 风险 R2 |
//! | [`paths`] | 合成路径拼接 / 事件 → 可显示路径（CLI 与界面共用） | 显示与落盘一致性 |
//! | [`incremental`] | USN 增量会话：拉页 → 归一化 → 回读大小 | M2 |
//! | [`session`] | **统一入口**：选后端 → 全量 → 建增量 → 注册监控 | M1~M3 |
//!
//! ## 两条路径一致对外
//!
//! ```text
//!           ┌───────────────┐
//!  Volume::open ├─ 失败 → 目录遍历 + ReadDirectoryChangesW（慢，但普通用户可用） ─┐
//!           └─ 成功           │
//!                ↓             │
//!         NtfsVolume::mount    │
//!           ├─ 失败 ───────────┤
//!           └─ 成功 → MFT 直读 ─┴→ Tree + ScanStats
//! ```
//!
//! 两条路径产出**同一个** [`core_index::Tree`]，后续增量、预警、面积图
//! 完全共用，无需分支；`Session::poll` 对调用方也只暴露一个入口。
//! 这是保证「降级模式功能不打折」的关键。
//!
//! ## 一个容易踩的顺序问题
//!
//! [`session::Session::open`] 里刻意**先**记录 USN 位置、**后**全量扫描，
//! 扫描完成再把游标对齐回「扫描开始前」。反过来的话，扫描过程中产生的
//! 变更（尤其是扫描过该目录之后的写入）会被永久漏掉——见模块内注释。

pub mod dirwatch;
pub mod full_scan;
pub mod incremental;
pub mod paths;
pub mod session;
pub mod traverse;
pub mod watcher;

pub use dirwatch::{parse_notifications, DirWatcher, FsEvent};
pub use full_scan::{
    full_scan_mft, probe_volume, scan_physical_range, top_n_files_via_mft, ProbeReport, ScanOptions,
    ScanStats, AVG_NAME_BYTES,
};
pub use incremental::IncrementalSession;
pub use paths::{event_path, join_path, raw_event_path};
pub use session::{ScanBackend, ScanPhase, Session, SessionConfig};
pub use traverse::{
    full_scan_traverse, full_scan_traverse_indexed, TraverseOptions, TraverseStats,
};
pub use watcher::{probe_path, WatchStats, WatcherSession};

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约测试：本 crate 必须同时暴露「两条后端」与「统一会话」，
    /// 否则 CLI 无法在无管理员环境下完成装配。
    #[test]
    fn public_surface_is_complete() {
        // 两条全量扫描路径
        let _ = full_scan_mft::<fn(u64, u64)>;
        let _ = full_scan_traverse::<fn(u64, u64)>;
        let _ = full_scan_traverse_indexed::<fn(u64, u64)>;
        // 自检与增量
        let _ = probe_volume;
        let _: Option<IncrementalSession> = None;
        // 降级模式的实时增量（路径事件 → FRN 事件）
        // `parse_notifications` 的第二参是 `impl FnMut(FsEvent)`（APIT，不是普通泛型参数），
        // 所以既不能裸取函数项（E0283 无法推断），也不能写 `::<T>`（E0107）。
        // 正确做法是给一个具体函数指针类型，让 APIT 由目标类型推导出来。
        let _: fn(&[u8], fn(FsEvent)) -> usize = parse_notifications;
        let _ = DirWatcher::start;
        let _: Option<WatcherSession> = None;
        let _ = probe_path;
        // 会话配置默认值必须是「安全默认」（不擅自改系统状态）
        let c = SessionConfig::default();
        assert!(!c.allow_create_journal);
        assert!(c.allow_traverse_fallback);
        assert!(!c.force_traverse);
        assert_eq!(c.drive, "C:");
    }

    #[test]
    fn scan_options_defaults_match_doc() {
        let o = ScanOptions::default();
        assert_eq!(o.chunk_bytes, core_io::MFT_READ_CHUNK);
        assert_eq!(core_io::MFT_READ_CHUNK, 16 * 1024 * 1024);
        assert!(o.preallocate);
    }

    #[test]
    fn traverse_defaults_are_conservative() {
        let t = TraverseOptions::default();
        // 不跟进重解析点，避免符号链接导致的无限递归（文档 §3.2）
        assert!(t.skip_reparse);
        assert!(t.max_depth >= 64);
        assert_eq!(t.fallback_cluster_size, 4096);
    }
}
