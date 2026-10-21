//! 跨平台常量：**只放「值必须一致」的数字**，不放任何平台专属的 FFI。
//!
//! # 为什么单独一个模块
//!
//! 引擎里有一批数字来自 Win32（`FILE_ATTRIBUTE_*` / `FILE_ACTION_*` /
//! `DRIVE_*` / `FSCTL_*`），但**用它们的地方并不都是 Windows 专属**：
//!
//! * `core-monitor::debounce` 只拿 `FILE_ATTRIBUTE_DIRECTORY` 判断「这条事件是目录」；
//! * `core-scan::watcher` 把目录事件翻译成 `FILE_ACTION_*`；
//! * `core-usn::record` 判 `FILE_ATTRIBUTE_*` 位。
//!
//! 这些是**数据格式**的一部分（`USN_RECORD` 里的 `FileAttributes` 就是 Win32 值），
//! 而不是系统调用。把它们搬到平台无关的模块里，Linux 版就能共用同一套语义，
//! 不必在 `core-monitor` 里到处写 `#[cfg(windows)]`。
//!
//! # 与 `winapi::*` 的关系
//!
//! `winapi` 在 Windows 上 `pub use` 的是**这里**的定义（不再自己写一份），
//! 所以「Windows 用 A、Linux 用 B」这种分叉从编译期就不存在。
//! 在 Linux 上 `winapi` 是一个只含类型别名与这些常量的空壳 —— 名字仍在，
//! 让人读代码时不必在两套命名间切换。
//!
//! # Linux 上的取值约定
//!
//! `FILE_ATTRIBUTE_*` 在 Linux 没有对应物，但**索引树与事件流仍然需要**
//! 「这是目录吗」「这是符号链接吗」这两条信息，所以沿用同一套位：
//!
//! | 位 | Linux 上的含义 |
//! |---|---|
//! | `FILE_ATTRIBUTE_DIRECTORY` | `st_mode` 是目录 |
//! | `FILE_ATTRIBUTE_ARCHIVE`   | 普通文件（Windows 上"尚未归档"= 普通文件） |
//! | `FILE_ATTRIBUTE_REPARSE_POINT` | 符号链接 / 挂载点二次跳转 |
//!
//! 取值由 [`crate::fsmeta`] 合成，规则只有那一处。

// ── 文件属性位（`WIN32_FILE_ATTRIBUTE_DATA::dwFileAttributes`） ──────────────

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

// ── 目录变更动作（`FILE_NOTIFY_INFORMATION::Action`） ───────────────────────
//
// 值就是 Win32 的值。Linux 的 inotify 掩码在 `core-scan::dirwatch` 里
// 映射成这五个值，于是上层（`watcher`）的事件翻译一份代码两处可用。

pub const FILE_ACTION_ADDED: u32 = 0x0000_0001;
pub const FILE_ACTION_REMOVED: u32 = 0x0000_0002;
pub const FILE_ACTION_MODIFIED: u32 = 0x0000_0003;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 0x0000_0004;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 0x0000_0005;

// ── 卷类型（`GetDriveTypeW` 返回值） ────────────────────────────────────────

pub const DRIVE_UNKNOWN: u32 = 0;
pub const DRIVE_NO_ROOT_DIR: u32 = 1;
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;
pub const DRIVE_RAMDISK: u32 = 6;

// ── 错误码（`GetLastError`） ────────────────────────────────────────────────
//
// 这些错误码会出现在**跨平台的数据**里：USN 会话把 `ERROR_HANDLE_EOF`
// 当作「枚举完毕」的正常信号，`cursor` 把 `ERROR_JOURNAL_NOT_ACTIVE`
// 当作「日志被删了」的判据。Linux 上这些分支走不到，但代码要能编译，
// 而且将来若用 `/dev` 直读 NTFS，错误语义要对得上。

pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_NO_MORE_FILES: u32 = 18;
pub const ERROR_HANDLE_EOF: u32 = 38;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_MORE_DATA: u32 = 234;
pub const ERROR_OPERATION_ABORTED: u32 = 995;
pub const ERROR_IO_PENDING: u32 = 997;
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
pub const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;

// ── 文件系统控制码（`CTL_CODE` 展开值，见 `winapi` 的推导注释） ─────────────

pub const FSCTL_GET_NTFS_FILE_RECORD: u32 = 0x0009_0068;
pub const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
pub const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
pub const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00E7;
pub const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
pub const FSCTL_DELETE_USN_JOURNAL: u32 = 0x0009_00B4;

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些数字是**磁盘/协议里的字节值**，改一个就会让 USN 解析静默错位。
    #[test]
    fn protocol_values_match_win32_header() {
        // winioctl.h：CTL_CODE(0x09, 26/44/45/46/57/61, ...)
        assert_eq!(FSCTL_GET_NTFS_FILE_RECORD, 0x0009_0068);
        assert_eq!(FSCTL_ENUM_USN_DATA, 0x0009_00B3);
        assert_eq!(FSCTL_DELETE_USN_JOURNAL, 0x0009_00B4);
        assert_eq!(FSCTL_READ_USN_JOURNAL, 0x0009_00BB);
        assert_eq!(FSCTL_CREATE_USN_JOURNAL, 0x0009_00E7);
        assert_eq!(FSCTL_QUERY_USN_JOURNAL, 0x0009_00F4);
        // winnt.h：FILE_ATTRIBUTE_*
        assert_eq!(FILE_ATTRIBUTE_DIRECTORY, 0x10);
        assert_eq!(FILE_ATTRIBUTE_ARCHIVE, 0x20);
        assert_eq!(FILE_ATTRIBUTE_REPARSE_POINT, 0x400);
        // WinBase.h：FILE_ACTION_*
        assert_eq!(FILE_ACTION_ADDED, 1);
        assert_eq!(FILE_ACTION_RENAMED_NEW_NAME, 5);
        // winerror.h
        assert_eq!(ERROR_HANDLE_EOF, 38);
        assert_eq!(ERROR_JOURNAL_NOT_ACTIVE, 1179);
    }

    /// 五个动作必须两两不同 —— 合并成同一值会让「新增」被当成「修改」。
    #[test]
    fn actions_are_distinct() {
        let v = [
            FILE_ACTION_ADDED,
            FILE_ACTION_REMOVED,
            FILE_ACTION_MODIFIED,
            FILE_ACTION_RENAMED_OLD_NAME,
            FILE_ACTION_RENAMED_NEW_NAME,
        ];
        for i in 0..v.len() {
            for j in i + 1..v.len() {
                assert_ne!(v[i], v[j], "动作常量重复");
            }
        }
    }
}
