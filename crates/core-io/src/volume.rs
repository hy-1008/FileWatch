//! 卷句柄与磁盘读取。
//!
//! 这一层**完全不理解 NTFS**：它只知道「怎么打开 `\\.\X:`」「怎么在某个字节偏移读
//! 一段数据」「怎么下发一次 DeviceIoControl」。NTFS 语义在 `core-ntfs` 里。

use crate::ioctl;
use crate::util::to_wide;
use crate::winapi::{self, HANDLE, DWORD};
use std::io;

/// 归一化盘符：`"c"` / `"C:"` / `"C:\"` → `("C:", r"\\.\C:")`
pub fn normalize_drive(input: &str) -> io::Result<(String, String)> {
    let t = input.trim();
    let mut chars = t.chars();
    let letter = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("非法盘符: {:?}（应形如 C: 或 C:\\）", input),
            ))
        }
    };
    let drive = format!("{}:", letter);
    let device = format!(r"\\.\{}", drive);
    Ok((drive, device))
}

/// 一个已打开的卷。
pub struct Volume {
    handle: HANDLE,
    drive: String,
    device_path: String,
    sector_size: u32,
}

// 卷句柄是内核对象，跨线程读取是安全的（每次读前会 SetFilePointerEx 定位）。
unsafe impl Send for Volume {}
unsafe impl Sync for Volume {}

impl Volume {
    /// 以只读方式打开卷设备。**需要管理员权限**；非管理员会得到 `ERROR_ACCESS_DENIED(5)`。
    pub fn open(drive_input: &str) -> io::Result<Volume> {
        let (drive, device_path) = normalize_drive(drive_input)?;
        let wide = to_wide(&device_path);

        // SAFETY: wide 以 NUL 结尾且在调用期间存活。
        let h = unsafe {
            winapi::CreateFileW(
                wide.as_ptr(),
                winapi::GENERIC_READ,
                winapi::FILE_SHARE_READ | winapi::FILE_SHARE_WRITE,
                core::ptr::null_mut(),
                winapi::OPEN_EXISTING,
                0,
                core::ptr::null_mut(),
            )
        };
        if h == winapi::INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "打开 {} 失败: {}（读卷需要管理员权限，请以管理员身份运行；\
                     或改用 `scan --fallback` 目录遍历模式）",
                    device_path, e
                ),
            ));
        }

        let mut vol = Volume {
            handle: h,
            drive,
            device_path,
            sector_size: 512,
        };

        // 探测扇区大小（引导扇区 @0x0B 是一个与文件系统无关的卷几何字段）。
        let mut boot = [0u8; 512];
        if vol.read_raw(0, &mut boot).is_ok() {
            let bps = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u32;
            if bps == 512 || bps == 1024 || bps == 2048 || bps == 4096 {
                vol.sector_size = bps;
            }
        }
        Ok(vol)
    }

    #[inline]
    pub fn drive(&self) -> &str {
        &self.drive
    }

    #[inline]
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    #[inline]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    #[inline]
    pub fn raw_handle(&self) -> HANDLE {
        self.handle
    }

    /// 底层物理读：`offset` 与 `buf.len()` 都**必须**是扇区大小的整数倍。
    ///
    /// 内部会循环直到读满，避免短读。
    pub fn read_raw(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done: usize = 0;
        while done < buf.len() {
            let pos = offset + done as u64;

            // SAFETY: 句柄有效（Drop 前一直存活）；pos 为文件内绝对偏移。
            let ok = unsafe {
                winapi::SetFilePointerEx(
                    self.handle,
                    pos as i64,
                    core::ptr::null_mut(),
                    winapi::FILE_BEGIN,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            let mut got: DWORD = 0;
            let want = core::cmp::min(buf.len() - done, u32::MAX as usize) as DWORD;
            // SAFETY: buf[done..] 至少有 want 字节可写。
            let ok = unsafe {
                winapi::ReadFile(
                    self.handle,
                    buf[done..].as_mut_ptr() as *mut core::ffi::c_void,
                    want,
                    &mut got,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("读取卷 {} 在偏移 {} 处返回 0 字节（可能已到卷尾）", self.drive, pos),
                ));
            }
            done += got as usize;
        }
        Ok(())
    }

    /// 安全读：允许任意字节偏移与任意长度，内部做扇区对齐 + 临时缓冲。
    /// 适合引导扇区、单条 MFT 记录这类小读；批量顺序读请用 `read_raw` + `ChunkReader`。
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let ss = self.sector_size as u64;
        let start = offset & !(ss - 1);
        let end = offset + buf.len() as u64;
        let aligned_end = (end + ss - 1) & !(ss - 1);
        let total = (aligned_end - start) as usize;

        if start == offset && total == buf.len() {
            return self.read_raw(offset, buf);
        }

        let mut tmp = vec![0u8; total];
        self.read_raw(start, &mut tmp)?;
        let off = (offset - start) as usize;
        buf.copy_from_slice(&tmp[off..off + buf.len()]);
        Ok(())
    }

    /// 分配一个新缓冲并读满。用于不需要复用的场景。
    pub fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }

    /// 下发一次 DeviceIoControl。
    pub fn device_io(
        &self,
        code: DWORD,
        input: &mut [u8],
        output: &mut [u8],
    ) -> io::Result<usize> {
        ioctl::device_io_control(self.handle, code, input, output)
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        if self.handle != winapi::INVALID_HANDLE_VALUE && !self.handle.is_null() {
            // SAFETY: 句柄由本结构体独占持有，只在此处关闭一次。
            unsafe {
                winapi::CloseHandle(self.handle);
            }
            self.handle = winapi::INVALID_HANDLE_VALUE;
        }
    }
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("drive", &self.drive)
            .field("device_path", &self.device_path)
            .field("sector_size", &self.sector_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_common_forms() {
        assert_eq!(normalize_drive("C").unwrap(), ("C:".into(), r"\\.\C:".into()));
        assert_eq!(normalize_drive("c:").unwrap(), ("C:".into(), r"\\.\C:".into()));
        assert_eq!(normalize_drive(r" d:\ ").unwrap(), ("D:".into(), r"\\.\D:".into()));
        assert!(normalize_drive("1").is_err());
        assert!(normalize_drive("").is_err());
    }
}
