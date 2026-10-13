//! # core-ntfs —— NTFS 结构解析
//!
//! 分层（文档 §4.2）：**不依赖 UI**，可用真实磁盘镜像做单测。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`boot`] | 引导扇区：簇大小、`$MFT` 起始 LCN、记录大小 |
//! | [`fixup`] | USA 更新序列校验（防撕裂写） |
//! | [`attribute`] | 属性链遍历、驻留/非驻留判定、Data Runs 定位 |
//! | [`runlist`] | Data Runs 变长整数解码 |
//! | [`filename`] | `$FILE_NAME` 解析 + 命名空间优选 + FRN 拆分 |
//! | [`record`] | 单条 MFT 记录 → [`record::MftRecord`] |
//! | [`mft`] | 挂载卷：拿到 `$MFT` 的物理段并顺序读整表 |
//!
//! 设计原则：**面对不可信数据，宁返回 `None` 不 panic**。所有解析函数都做了
//! 完整边界检查；损坏记录（撕裂写、越界长度、签名不符）一律被丢弃而不是崩溃。

pub mod attribute;
pub mod boot;
pub mod bytes;
pub mod filename;
pub mod fixup;
pub mod mft;
pub mod record;
pub mod runlist;

pub use boot::BootSector;
pub use mft::NtfsVolume;
pub use record::{parse_record, MftRecord};

/// 根目录的固定 MFT 记录号。
pub const ROOT_RECORD: u64 = 5;
/// `$MFT` 自身的记录号。
pub const MFT_RECORD: u64 = 0;
