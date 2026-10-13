//! 手写 Win32 FFI 绑定。
//!
//! 为什么不用 `windows-sys` / `windows` crate：
//!   1. 目标构建机网络受限（crates.io 不可达），任何外部依赖都会让 `cargo build` 失败；
//!   2. 本项目只用到极少量的 kernel32 函数，手写声明的**链接面积最小**——
//!      在 MinGW(GNU) 工具链下只需要 `libkernel32.a` 一个导入库即可。
//!
//! 所有函数均为 `extern "system"`（x64 上是 Win64 ABI）。
//! 调用方必须保证指针参数的生命周期与对齐。
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use core::ffi::c_void;

// ── 基础类型 ────────────────────────────────────────────────────────────────
pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub type DWORD = u32;
pub type WORD = u16;
pub type BYTE = u8;
pub type WCHAR = u16;
pub type LARGE_INTEGER = i64;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

// ── CreateFileW ─────────────────────────────────────────────────────────────
pub const GENERIC_READ: DWORD = 0x8000_0000;
pub const GENERIC_WRITE: DWORD = 0x4000_0000;
pub const FILE_SHARE_READ: DWORD = 0x0000_0001;
pub const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
pub const FILE_SHARE_DELETE: DWORD = 0x0000_0004;
pub const OPEN_EXISTING: DWORD = 3;
pub const FILE_BEGIN: DWORD = 0;
/// 打开**目录**句柄必须带这个标志（否则 CreateFileW 对目录返回 ACCESS_DENIED）。
pub const FILE_FLAG_BACKUP_SEMANTICS: DWORD = 0x0200_0000;
/// 异步（OVERLAPPED）I/O。见 [`OVERLAPPED`] 的说明：目录监视必须用它，
/// 否则「挂上第一次读取」这件事只能在线程里做，而线程调度延迟期间发生的
/// 变更是**永久**丢失的。
pub const FILE_FLAG_OVERLAPPED: DWORD = 0x4000_0000;
/// 目录句柄的读取权限位（`FILE_LIST_DIRECTORY`）。注意它不叫 GENERIC_READ。
pub const FILE_LIST_DIRECTORY: DWORD = 0x0000_0001;

// ── ReadDirectoryChangesW（降级模式的增量来源，风险 R2 的解法） ──────────────
pub const FILE_NOTIFY_CHANGE_FILE_NAME: DWORD = 0x0000_0001;
pub const FILE_NOTIFY_CHANGE_DIR_NAME: DWORD = 0x0000_0002;
pub const FILE_NOTIFY_CHANGE_ATTRIBUTES: DWORD = 0x0000_0004;
pub const FILE_NOTIFY_CHANGE_SIZE: DWORD = 0x0000_0008;
pub const FILE_NOTIFY_CHANGE_LAST_WRITE: DWORD = 0x0000_0010;
pub const FILE_NOTIFY_CHANGE_LAST_ACCESS: DWORD = 0x0000_0020;
pub const FILE_NOTIFY_CHANGE_CREATION: DWORD = 0x0000_0040;
pub const FILE_NOTIFY_CHANGE_SECURITY: DWORD = 0x0000_0100;
/// 与资源管理器「全部勾选」等价的一组变更过滤位。
pub const FILE_NOTIFY_CHANGE_ALL: DWORD = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

/// `FILE_NOTIFY_INFORMATION.Action`
pub const FILE_ACTION_ADDED: DWORD = 0x0000_0001;
pub const FILE_ACTION_REMOVED: DWORD = 0x0000_0002;
pub const FILE_ACTION_MODIFIED: DWORD = 0x0000_0003;
pub const FILE_ACTION_RENAMED_OLD_NAME: DWORD = 0x0000_0004;
pub const FILE_ACTION_RENAMED_NEW_NAME: DWORD = 0x0000_0005;

/// `ReadDirectoryChangesW` 被 `CancelIoEx` / 关闭句柄中断时返回的错误码。
pub const ERROR_OPERATION_ABORTED: DWORD = 995;
/// 异步 I/O 已受理但尚未完成。**不是错误**。
pub const ERROR_IO_PENDING: DWORD = 997;

/// 异步 I/O 的控制块。
///
/// # 为什么手写而不是用 `windows-sys`
/// 全项目零第三方依赖，这个结构在 x64 / x86 上布局都一样（两个
/// `ULONG_PTR` + 一个 `{Offset, OffsetHigh}` 联合 + 一个 `HANDLE`），
/// 而下面这块联合里的字段我们**从不使用** —— 所以直接摊平成两个 `DWORD`，
/// 既省掉 `union`，也省掉 `Default` 不能派生在 union 上的麻烦。
/// 元素顺序不能改：内核按这个布局回填 `Internal` / `InternalHigh`。
///
/// # 生命周期约束（极其重要）
/// 一旦用它发起 I/O，**它和它指向的缓冲区都不能再移动**，直到 `GetOverlappedResult`
/// 报告完成 —— 内核保存的是这两个地址。所以 `dirwatch` 里把它和缓冲区一起放进
/// 一个 `Box`，分配好地址再发起第一次读取，之后才把 `Box` 交给线程。
#[repr(C)]
pub struct OVERLAPPED {
    pub Internal: usize,
    pub InternalHigh: usize,
    pub Offset: DWORD,
    pub OffsetHigh: DWORD,
    pub hEvent: HANDLE,
}

impl OVERLAPPED {
    /// 全零初始化（`hEvent = NULL` 表示不用事件对象，靠 `GetOverlappedResult` 等）。
    pub fn zeroed() -> Self {
        OVERLAPPED {
            Internal: 0,
            InternalHigh: 0,
            Offset: 0,
            OffsetHigh: 0,
            hEvent: std::ptr::null_mut(),
        }
    }
}

// ── 文件属性位 ──────────────────────────────────────────────────────────────
pub const FILE_ATTRIBUTE_READONLY: DWORD = 0x0000_0001;
pub const FILE_ATTRIBUTE_HIDDEN: DWORD = 0x0000_0002;
pub const FILE_ATTRIBUTE_SYSTEM: DWORD = 0x0000_0004;
pub const FILE_ATTRIBUTE_DIRECTORY: DWORD = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: DWORD = 0x0000_0020;
pub const FILE_ATTRIBUTE_REPARSE_POINT: DWORD = 0x0000_0400;
pub const INVALID_FILE_ATTRIBUTES: DWORD = 0xFFFF_FFFF;

// ── GetDriveTypeW ───────────────────────────────────────────────────────────
pub const DRIVE_UNKNOWN: u32 = 0;
pub const DRIVE_NO_ROOT_DIR: u32 = 1;
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;
pub const DRIVE_RAMDISK: u32 = 6;

// ── 错误码 ──────────────────────────────────────────────────────────────────
pub const ERROR_ACCESS_DENIED: DWORD = 5;
pub const ERROR_NO_MORE_FILES: DWORD = 18;
pub const ERROR_HANDLE_EOF: DWORD = 38;
pub const ERROR_MORE_DATA: DWORD = 234;
pub const ERROR_JOURNAL_NOT_ACTIVE: DWORD = 1179;
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: DWORD = 1178;
pub const ERROR_INVALID_PARAMETER: DWORD = 87;

// ── 结构体 ──────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct FILETIME {
    pub dwLowDateTime: DWORD,
    pub dwHighDateTime: DWORD,
}

impl FILETIME {
    /// FILETIME 是「1601-01-01 起的 100 纳秒计数」，这里转成 Unix 时间戳（秒）。
    /// 返回 0 表示时间戳为 0（未设置）。
    #[inline]
    pub fn to_unix(self) -> i64 {
        let ticks = ((self.dwHighDateTime as u64) << 32) | (self.dwLowDateTime as u64);
        if ticks == 0 {
            return 0;
        }
        // 1601-01-01 → 1970-01-01 相差 11_644_473_600 秒
        const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
        (ticks / 10_000_000) as i64 - EPOCH_DIFF_SECS
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WIN32_FIND_DATAW {
    pub dwFileAttributes: DWORD,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: DWORD,
    pub nFileSizeLow: DWORD,
    pub dwReserved0: DWORD,
    pub dwReserved1: DWORD,
    pub cFileName: [WCHAR; 260],
    pub cAlternateFileName: [WCHAR; 14],
}

impl WIN32_FIND_DATAW {
    pub fn zeroed() -> Self {
        // SAFETY: 全 0 是本 POD 结构体的合法初值。
        unsafe { core::mem::zeroed() }
    }

    #[inline]
    pub fn file_size(&self) -> u64 {
        ((self.nFileSizeHigh as u64) << 32) | (self.nFileSizeLow as u64)
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    #[inline]
    pub fn is_reparse_point(&self) -> bool {
        self.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// cFileName 是定长 UTF-16 缓冲，按 NUL 截断。
    pub fn name(&self) -> String {
        let mut n = 0usize;
        while n < self.cFileName.len() && self.cFileName[n] != 0 {
            n += 1;
        }
        String::from_utf16_lossy(&self.cFileName[..n])
    }
}

/// `ReadDirectoryChangesW` 输出缓冲里的变长记录。
///
/// 布局（x64，`#[repr(C)]` 下 `DWORD` 后紧跟 4 字节对齐的 `FileName` 首字符）：
/// ```text
///  0  DWORD  NextEntryOffset   // 到下一记录头的字节偏移，0 = 最后一条
///  4  DWORD  Action
///  8  DWORD  FileNameLength    // **字节数**，不是字符数
/// 12  WCHAR  FileName[1...]    // 非 NUL 结尾，长度按上一个字段算
/// ```
/// 因此记录里的名字在缓冲区中的偏移恒定是 **12**。
#[repr(C)]
pub struct FILE_NOTIFY_INFORMATION {
    pub NextEntryOffset: DWORD,
    pub Action: DWORD,
    pub FileNameLength: DWORD,
    // FileName 紧跟在后面，用指针运算读取，这里不声明成定长数组
}

/// `FILE_NOTIFY_INFORMATION` 中 `FileName` 相对记录头的字节偏移。
pub const FILE_NOTIFY_NAME_OFFSET: usize = 12;

// ── kernel32 导入 ───────────────────────────────────────────────────────────
#[link(name = "kernel32")]
extern "system" {
    pub fn CreateFileW(
        lpFileName: *const WCHAR,
        dwDesiredAccess: DWORD,
        dwShareMode: DWORD,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: DWORD,
        dwFlagsAndAttributes: DWORD,
        hTemplateFile: HANDLE,
    ) -> HANDLE;

    pub fn CloseHandle(hObject: HANDLE) -> BOOL;

    pub fn ReadFile(
        hFile: HANDLE,
        lpBuffer: *mut c_void,
        nNumberOfBytesToRead: DWORD,
        lpNumberOfBytesRead: *mut DWORD,
        lpOverlapped: *mut c_void,
    ) -> BOOL;

    pub fn SetFilePointerEx(
        hFile: HANDLE,
        liDistanceToMove: LARGE_INTEGER,
        lpNewFilePointer: *mut LARGE_INTEGER,
        dwMoveMethod: DWORD,
    ) -> BOOL;

    pub fn DeviceIoControl(
        hDevice: HANDLE,
        dwIoControlCode: DWORD,
        lpInBuffer: *mut c_void,
        nInBufferSize: DWORD,
        lpOutBuffer: *mut c_void,
        nOutBufferSize: DWORD,
        lpBytesReturned: *mut DWORD,
        lpOverlapped: *mut c_void,
    ) -> BOOL;

    pub fn GetLastError() -> DWORD;

    pub fn GetLogicalDriveStringsW(nBufferLength: DWORD, lpBuffer: *mut WCHAR) -> DWORD;

    pub fn GetDriveTypeW(lpRootPathName: *const WCHAR) -> u32;

    pub fn GetVolumeInformationW(
        lpRootPathName: *const WCHAR,
        lpVolumeNameBuffer: *mut WCHAR,
        nVolumeNameSize: DWORD,
        lpVolumeSerialNumber: *mut DWORD,
        lpMaximumComponentLength: *mut DWORD,
        lpFileSystemFlags: *mut DWORD,
        lpFileSystemNameBuffer: *mut WCHAR,
        nFileSystemNameSize: DWORD,
    ) -> BOOL;

    pub fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const WCHAR,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> BOOL;

    pub fn GetDiskFreeSpaceW(
        lpRootPathName: *const WCHAR,
        lpSectorsPerCluster: *mut DWORD,
        lpBytesPerSector: *mut DWORD,
        lpNumberOfFreeClusters: *mut DWORD,
        lpTotalNumberOfClusters: *mut DWORD,
    ) -> BOOL;

    pub fn FindFirstFileW(
        lpFileName: *const WCHAR,
        lpFindFileData: *mut WIN32_FIND_DATAW,
    ) -> HANDLE;

    pub fn FindNextFileW(hFindFile: HANDLE, lpFindFileData: *mut WIN32_FIND_DATAW) -> BOOL;

    pub fn FindClose(hFindFile: HANDLE) -> BOOL;

    /// 监视目录变更。`bWatchSubtree != 0` 时递归监视整棵子树。
    ///
    /// 传 `lpOverlapped` 时是**异步**调用（只接受 `ERROR_IO_PENDING` 或立即成功）；
    /// 传 NULL 时是同步阻塞调用（不带 `FILE_FLAG_OVERLAPPED` 的句柄才允许）。
    ///
    /// `dirwatch` 用的是异步形态：只有这样才能在 `DirWatcher::start` 返回**之前**
    /// 就把第一次读取挂到内核上，从而消掉「start 返回后、线程被调度前」这段
    /// 变更永久丢失的窗口。
    pub fn ReadDirectoryChangesW(
        hDirectory: HANDLE,
        lpBuffer: *mut c_void,
        nBufferLength: DWORD,
        bWatchSubtree: BOOL,
        dwNotifyFilter: DWORD,
        lpBytesReturned: *mut DWORD,
        lpOverlapped: *mut c_void,
        lpCompletionRoutine: *mut c_void,
    ) -> BOOL;

    /// 等待一个异步 I/O 完成并取回实际字节数。
    ///
    /// `bWait = 1` 时阻塞到完成（含被 `CancelIoEx` 中止）；`bWait = 0` 时轮询。
    /// 返回 0 表示失败，此时 `GetLastError()` 区分「被取消」与「真错误」。
    pub fn GetOverlappedResult(
        hFile: HANDLE,
        lpOverlapped: *mut OVERLAPPED,
        lpNumberOfBytesTransferred: *mut DWORD,
        bWait: BOOL,
    ) -> BOOL;

    /// 中断句柄上挂起的所有 I/O —— 用来把阻塞中的 `ReadDirectoryChangesW`
    /// 唤醒（否则关线程时它会一直卡住）。
    pub fn CancelIoEx(hFile: HANDLE, lpOverlapped: *mut c_void) -> BOOL;

    /// 注册控制台事件处理函数（Ctrl+C / 关窗 / 注销 / 关机）。
    ///
    /// 处理函数返回非 0 表示「已处理」，系统不会再走默认的终止流程——
    /// 这样 CLI 才有机会把游标落盘、把变更流水刷完再退出。
    pub fn SetConsoleCtrlHandler(
        HandlerRoutine: Option<extern "system" fn(DWORD) -> BOOL>,
        Add: BOOL,
    ) -> BOOL;

    pub fn Sleep(dwMilliseconds: DWORD);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OVERLAPPED` 是**内核按固定布局回填**的结构。布局写错了编译器一句话
    /// 都不会说，只会在运行时表现为「偶发数据错乱」—— 所以把尺寸和关键
    /// 偏移在这里钉死。
    #[test]
    fn overlapped_layout_matches_win32() {
        // 两个 ULONG_PTR + {Offset, OffsetHigh} 联合（8 字节）+ HANDLE
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(
                core::mem::size_of::<OVERLAPPED>(),
                32,
                "x64 上 OVERLAPPED 必须是 32 字节"
            );
            assert_eq!(core::mem::offset_of!(OVERLAPPED, InternalHigh), 8);
            assert_eq!(core::mem::offset_of!(OVERLAPPED, hEvent), 24);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(
                core::mem::size_of::<OVERLAPPED>(),
                20,
                "x86 上 OVERLAPPED 必须是 20 字节"
            );
            assert_eq!(core::mem::offset_of!(OVERLAPPED, hEvent), 16);
        }
        assert_eq!(core::mem::size_of::<HANDLE>(), core::mem::size_of::<usize>());
    }

    /// `zeroed()` 必须真的把事件句柄清成 NULL —— 否则内核会把完成信号投递到
    /// 一个随机地址，是教科书级的 UB。
    #[test]
    fn overlapped_zeroed_has_no_event_handle() {
        let o = OVERLAPPED::zeroed();
        assert_eq!(o.Internal, 0);
        assert_eq!(o.InternalHigh, 0);
        assert_eq!(o.Offset, 0);
        assert_eq!(o.OffsetHigh, 0);
        assert!(o.hEvent.is_null());
    }

    /// 异步 I/O 的两个关键错误码不能记错（997 被当成失败会让监视器一启动就退出）。
    #[test]
    fn async_error_codes_are_distinct() {
        assert_eq!(ERROR_IO_PENDING, 997);
        assert_eq!(ERROR_OPERATION_ABORTED, 995);
        assert_ne!(ERROR_IO_PENDING, ERROR_OPERATION_ABORTED);
    }
}
