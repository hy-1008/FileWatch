//! 卷句柄与磁盘读取（平台分派）。
//!
//! 这一层**完全不理解 NTFS**：它只知道「怎么打开一个卷」「怎么在某个字节偏移读
//! 一段数据」「怎么下发一次设备控制」。NTFS 语义在 `core-ntfs` 里。
//!
//! ```text
//! volume.rs
//!   ├─ volume/win.rs    \\.\C:  + CreateFileW/SetFilePointerEx/ReadFile + DeviceIoControl
//!   └─ volume/linux.rs  /dev/xxx 或镜像文件 + pread；DeviceIoControl 一律 Unsupported
//! ```
//!
//! 两边对外暴露的 API 完全一致（`Volume::open` / `read_at` / `device_io` …），
//! 所以 `core-ntfs`、`core-usn`、`core-scan` 的上层代码一份源码两个平台通用。

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{normalize_drive, probe_elevated, Volume};

#[cfg(not(windows))]
mod linux;

#[cfg(not(windows))]
pub use linux::{mount_table, normalize_drive, probe_elevated, statvfs_of, MountEntry, Volume};
