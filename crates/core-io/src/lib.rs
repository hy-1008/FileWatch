//! # core-io —— 卷句柄 / 大块预读 / 设备控制封装 / 平台口径
//!
//! 设计约束（见文档 §4.2）：
//! * **不依赖 UI**，可独立单测；
//! * **零第三方 crate**：Windows 的 Win32 声明、Linux 的 `inotify` / `statvfs`
//!   声明都是手写 FFI，这样在受限网络环境下也能直接 `cargo build`，
//!   且链接面积最小。
//!
//! 分层边界：本 crate 只负责「怎么和卷/文件系统说话」，**不解析任何 NTFS 结构**。
//!
//! ## 平台分派
//!
//! ```text
//! consts.rs       两个平台共用的数字（属性位/动作码/FSCTL/错误码）
//! paths.rs        分隔符与"根前缀"的唯一口径（`C:\` vs `/`）
//! fsmeta.rs       Metadata → (属性位, 修改时间, 占用, 链接数)
//! volume.rs   ─┬─ volume/win.rs     \\.\C: + DeviceIoControl
//!              └─ volume/linux.rs   /dev/xxx 或镜像文件 + pread
//! volumes.rs      卷枚举（GetLogicalDriveStringsW / /proc/self/mounts + statvfs）
//! ioctl.rs        DeviceIoControl 的 CTL_CODE 封装（仅 Windows）
//! winapi.rs       Win32 FFI（仅 Windows）/ 只含类型与常量的空壳（其它平台）
//! ```
//!
//! 为什么 `winapi` 在 Linux 上还留个壳：有一批"看起来像 Win32"的值其实是
//! **数据格式**（属性位、五个动作码、`FSCTL_*` 数字），上层引用的是这些值，
//! 留同名模块能让上层代码一行不改地跨平台编译。见 `winapi_linux.rs` 的说明。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod buffered_reader;
pub mod consts;
pub mod fsmeta;
pub mod hash;
pub mod jsonlite;
pub mod paths;
pub mod test_dir;
pub mod util;
pub mod volume;
pub mod volumes;

#[cfg(windows)]
pub mod ioctl;
#[cfg(windows)]
pub mod winapi;

#[cfg(not(windows))]
#[path = "winapi/linux.rs"]
pub mod winapi;

pub use buffered_reader::ChunkReader;
pub use volume::{normalize_drive, Volume};
pub use volumes::{cluster_size, list_volumes, VolumeInfo};

/// MFT 顺序读的默认块大小：16 MB（文档 §5.1 性能要点 2）。
pub const MFT_READ_CHUNK: usize = 16 * 1024 * 1024;

/// 本平台是否有"快路径"（`$MFT` 直读 + USN Journal 增量）。
///
/// 界面与 CLI 用它决定要不要在卷列表上写「极速」。Linux 恒为 `false`：
/// `$MFT` 能读，但 USN Journal 没有对应物，增量只能走 `inotify`。
pub const HAS_MFT_BACKEND: bool = cfg!(windows);

/// 增量事件源的显示名（CLI / 界面的提示文案共用一处）。
pub fn watch_backend_name() -> &'static str {
    if cfg!(windows) {
        "ReadDirectoryChangesW"
    } else {
        "inotify"
    }
}
