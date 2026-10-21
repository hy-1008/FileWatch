//! `DeviceIoControl` 安全封装与所需 CTL_CODE 常量。
//!
//! CTL_CODE 计算式（Windows 定义）：
//! `(DeviceType << 16) | (Access << 14) | (Function << 2) | Method`
//! 其中 `FILE_DEVICE_FILE_SYSTEM = 0x09`，`FILE_ANY_ACCESS = 0`，
//! `METHOD_BUFFERED = 0`、`METHOD_NEITHER = 3`。
//! 下方常量已按此式手工展开并标注，便于人工核对。

use crate::winapi::{self, DWORD, HANDLE};
use std::io;

// 控制码**定义**在 `crate::consts`（它们是固定数字，Linux 侧也要能用同一套
// 语义去判断"哪个功能不可用"）。这里重导出，保持 `core_io::ioctl::FSCTL_*`
// 这条既有路径不变。
pub use crate::consts::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_DELETE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_GET_NTFS_FILE_RECORD, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};

/// 一次 `DeviceIoControl` 调用。输入/输出缓冲均为可变切片。
///
/// 返回实际写入输出缓冲的字节数。
///
/// 注意：`FSCTL_ENUM_USN_DATA` / `FSCTL_READ_USN_JOURNAL` 是 **METHOD_NEITHER**，
/// 但对这两种控制码，I/O 管理器仍支持标准 in/out 缓冲用法，业界一致这么调。
pub fn device_io_control(
    handle: HANDLE,
    code: DWORD,
    input: &mut [u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned: DWORD = 0;
    let (in_ptr, in_len) = if input.is_empty() {
        (core::ptr::null_mut(), 0u32)
    } else {
        (input.as_mut_ptr() as *mut core::ffi::c_void, input.len() as DWORD)
    };
    let (out_ptr, out_len) = if output.is_empty() {
        (core::ptr::null_mut(), 0u32)
    } else {
        (output.as_mut_ptr() as *mut core::ffi::c_void, output.len() as DWORD)
    };

    // SAFETY: 指针与长度来自上方的切片，调用期间切片保持存活。
    let ok = unsafe {
        winapi::DeviceIoControl(
            handle,
            code,
            in_ptr,
            in_len,
            out_ptr,
            out_len,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// 与 `device_io_control` 相同，但允许调用方把「错误码」当作正常结果处理。
/// 用于 `FSCTL_ENUM_USN_DATA` 这类以 `ERROR_HANDLE_EOF` 表示「枚举完毕」的场景。
pub fn device_io_control_allow(
    handle: HANDLE,
    code: DWORD,
    input: &mut [u8],
    output: &mut [u8],
    allowed: &[DWORD],
) -> io::Result<Option<usize>> {
    let mut returned: DWORD = 0;
    let (in_ptr, in_len) = if input.is_empty() {
        (core::ptr::null_mut(), 0u32)
    } else {
        (input.as_mut_ptr() as *mut core::ffi::c_void, input.len() as DWORD)
    };
    let (out_ptr, out_len) = if output.is_empty() {
        (core::ptr::null_mut(), 0u32)
    } else {
        (output.as_mut_ptr() as *mut core::ffi::c_void, output.len() as DWORD)
    };

    // SAFETY: 同上。
    let ok = unsafe {
        winapi::DeviceIoControl(
            handle,
            code,
            in_ptr,
            in_len,
            out_ptr,
            out_len,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(Some(returned as usize));
    }
    let err = unsafe { winapi::GetLastError() };
    if allowed.contains(&err) {
        return Ok(None);
    }
    Err(io::Error::from_raw_os_error(err as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_codes_match_reference_values() {
        // 与 Windows SDK winioctl.h 中的定义逐条核对
        assert_eq!(FSCTL_ENUM_USN_DATA, 0x0009_00B3);
        assert_eq!(FSCTL_READ_USN_JOURNAL, 0x0009_00BB);
        assert_eq!(FSCTL_CREATE_USN_JOURNAL, 0x0009_00E7);
        assert_eq!(FSCTL_QUERY_USN_JOURNAL, 0x0009_00F4);
        assert_eq!(FSCTL_GET_NTFS_FILE_RECORD, 0x0009_0068);
    }
}
