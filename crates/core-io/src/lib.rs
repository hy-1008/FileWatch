//! # core-io —— 卷句柄 / 大块预读 / DeviceIoControl 封装
//!
//! 设计约束（见文档 §4.2）：
//! * **不依赖 UI**，可独立单测；
//! * **零第三方 crate**：Win32 声明为手写 FFI（见 [`winapi`]），
//!   这样在受限网络环境下也能直接 `cargo build`，且链接面积最小。
//!
//! 分层边界：本 crate 只负责「怎么和卷设备说话」，**不解析任何 NTFS 结构**。

#![cfg_attr(not(windows), allow(unused))]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
compile_error!("FileWatch 仅支持 Windows：核心依赖 NTFS $MFT 直读与 USN Journal。");

pub mod buffered_reader;
pub mod ioctl;
pub mod jsonlite;
pub mod util;
pub mod volume;
pub mod volumes;
pub mod winapi;

pub use buffered_reader::ChunkReader;
pub use volume::{normalize_drive, Volume};
pub use volumes::{cluster_size, list_volumes, VolumeInfo};

/// MFT 顺序读的默认块大小：16 MB（文档 §5.1 性能要点 2）。
pub const MFT_READ_CHUNK: usize = 16 * 1024 * 1024;
