//! `winapi` 模块在非 Windows 平台的**空壳**：只有类型别名与常量，没有任何 FFI。
//!
//! # 为什么留着这个名字
//!
//! 有相当一部分"看起来像 Win32"的东西其实是**数据格式**，不是系统调用：
//! `USN_RECORD::FileAttributes` 是 Win32 属性位、`FILE_NOTIFY_INFORMATION::Action`
//! 是那五个动作码、`FSCTL_*` 是 `CTL_CODE` 展开的固定数字。上层代码
//! （`core-monitor::debounce`、`core-usn::record`、`core-scan::watcher`）
//! 引用的是这些**值**，而不是函数。
//!
//! 若把这些引用全部改成 `core_io::consts::*`，Windows 侧那 5 个文件也要跟着
//! 改一遍 —— 改动面越大，回归风险越大。留一个同名壳，上层一行都不用动：
//! 表达式 `core_io::winapi::FILE_ATTRIBUTE_DIRECTORY` 在两个平台都成立。
//!
//! 真正的系统调用（`CreateFileW` / `ReadDirectoryChangesW` / `DeviceIoControl`）
//! 在 Linux 上**不存在**，所以这个模块里一个函数都没有 —— 谁要是写了
//! `winapi::ReadFile`，编译期就会报"找不到"，而不是链接期或运行期才发现。

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use core::ffi::c_void;

// ── 基础类型（与 Win32 同宽，保证结构体解析代码两个平台一致） ───────────────
pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub type DWORD = u32;
pub type WORD = u16;
pub type BYTE = u8;
pub type WCHAR = u16;
pub type LARGE_INTEGER = i64;
pub type ULONG_PTR = usize;

/// Linux 上没有句柄。保留这个符号是为了让"某个值表示无效"的判断能编译。
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

// ── 常量：与 Windows 完全同源 ───────────────────────────────────────────────
pub use crate::consts::*;

/// `FILE_NOTIFY_INFORMATION` 里 `FileName` 的固定偏移（`NextEntryOffset` 4 +
/// `Action` 4 + `FileNameLength` 4）。这个偏移是**结构定义**，与平台无关。
pub const FILE_NOTIFY_NAME_OFFSET: usize = 12;
