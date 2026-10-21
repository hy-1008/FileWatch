//! USN Journal 的查询 / 创建 / 枚举 / 读取。
//!
//! 结构体速查（文档 §5.2）：
//!
//! `USN_JOURNAL_DATA_V0`（56 字节，全 u64，小端）
//! ```text
//! 0x00 UsnJournalID   0x08 FirstUsn   0x10 NextUsn
//! 0x18 LowestValidUsn 0x20 MaxUsn     0x28 MaximumSize  0x30 AllocationDelta
//! ```
//!
//! `MFT_ENUM_DATA_V0`（24 字节）
//! ```text
//! 0x00 StartFileReferenceNumber   0x08 LowUsn   0x10 HighUsn
//! ```
//!
//! `READ_USN_JOURNAL_DATA_V0`（40 字节）
//! ```text
//! 0x00 StartUsn   0x08 ReasonMask   0x0C ReturnOnlyOnClose
//! 0x10 Timeout    0x18 BytesToWaitFor   0x20 UsnJournalID
//! ```

use crate::bytes_u64;
use core_io::consts::*;
use core_io::volume::Volume;
use core_io::winapi::{ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE};
use std::io;

pub const USN_JOURNAL_DATA_LEN: usize = 56;
pub const MFT_ENUM_DATA_LEN: usize = 24;
pub const READ_USN_JOURNAL_DATA_LEN: usize = 40;

/// 文档 §5.2：默认 32 MB 容量 / 4 MB 增量
pub const DEFAULT_MAXIMUM_SIZE: u64 = 32 * 1024 * 1024;
pub const DEFAULT_ALLOCATION_DELTA: u64 = 4 * 1024 * 1024;

/// 相邻两次 `FSCTL_READ_USN_JOURNAL` 之间的返回值头长度（NextUsn，8 字节）
pub const READ_RESULT_HEADER: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsnJournalData {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

impl UsnJournalData {
    pub fn parse(buf: &[u8]) -> Option<UsnJournalData> {
        if buf.len() < USN_JOURNAL_DATA_LEN {
            return None;
        }
        Some(UsnJournalData {
            id: bytes_u64(buf, 0x00)?,
            first_usn: bytes_u64(buf, 0x08)? as i64,
            next_usn: bytes_u64(buf, 0x10)? as i64,
            lowest_valid_usn: bytes_u64(buf, 0x18)? as i64,
            max_usn: bytes_u64(buf, 0x20)? as i64,
            maximum_size: bytes_u64(buf, 0x28)?,
            allocation_delta: bytes_u64(buf, 0x30)?,
        })
    }

    /// 剩余可用空间比例（用于提示用户日志快满了，文档 R4）。
    pub fn free_ratio(&self) -> f64 {
        if self.maximum_size == 0 {
            return 1.0;
        }
        let used = (self.next_usn - self.first_usn).max(0) as f64;
        (1.0 - used / self.maximum_size as f64).clamp(0.0, 1.0)
    }
}

/// `FSCTL_QUERY_USN_JOURNAL`
pub fn query(vol: &Volume) -> io::Result<UsnJournalData> {
    let mut out = vec![0u8; USN_JOURNAL_DATA_LEN];
    let n = vol.device_io(FSCTL_QUERY_USN_JOURNAL, &mut [], &mut out)?;
    if n < USN_JOURNAL_DATA_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FSCTL_QUERY_USN_JOURNAL 返回 {} 字节（期望 {}）", n, USN_JOURNAL_DATA_LEN),
        ));
    }
    UsnJournalData::parse(&out).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "USN_JOURNAL_DATA 解析失败")
    })
}

/// 日志是否已启用（不产生副作用）。
pub fn is_active(vol: &Volume) -> bool {
    query(vol).is_ok()
}

/// `FSCTL_CREATE_USN_JOURNAL`
///
/// **注意（文档 R3）**：给卷启用 USN Journal 是一项**系统级变更**，
/// 会带来持续的小量磁盘写入开销。调用方应在 UI/日志里明确提示用户。
pub fn create(vol: &Volume, maximum_size: u64, allocation_delta: u64) -> io::Result<()> {
    let mut input = vec![0u8; 16];
    input[0..8].copy_from_slice(&maximum_size.to_le_bytes());
    input[8..16].copy_from_slice(&allocation_delta.to_le_bytes());
    let mut out: [u8; 0] = [];
    vol.device_io(FSCTL_CREATE_USN_JOURNAL, &mut input, &mut out)?;
    Ok(())
}

/// `FSCTL_DELETE_USN_JOURNAL`（退出时恢复原状用）。
pub fn delete(vol: &Volume, journal_id: u64) -> io::Result<()> {
    let mut input = vec![0u8; 8];
    input[0..8].copy_from_slice(&journal_id.to_le_bytes());
    let mut out: [u8; 0] = [];
    vol.device_io(FSCTL_DELETE_USN_JOURNAL, &mut input, &mut out)?;
    Ok(())
}

/// 确保日志可用：已启用就直接查，未启用则按默认参数创建。
///
/// 返回 `(日志数据, 本次是否新建了日志)`。
pub fn ensure(
    vol: &Volume,
    maximum_size: u64,
    allocation_delta: u64,
    allow_create: bool,
) -> io::Result<(UsnJournalData, bool)> {
    match query(vol) {
        Ok(d) => Ok((d, false)),
        Err(e) => {
            if !allow_create {
                return Err(e);
            }
            match create(vol, maximum_size, allocation_delta) {
                Ok(()) => query(vol).map(|d| (d, true)),
                Err(ce) => {
                    // 竞态：别的进程可能刚好创建了
                    if let Ok(d) = query(vol) {
                        return Ok((d, false));
                    }
                    Err(io::Error::new(
                        ce.kind(),
                        format!("启用 USN Journal 失败: {}（原查询错误: {}）", ce, e),
                    ))
                }
            }
        }
    }
}

/// `FSCTL_ENUM_USN_DATA` 的一页。
///
/// * `buf` 由调用方提供且**会被复用**：`buf[0..8]` 放下一页的起始 FRN，
///   其后紧跟若干条 `USN_RECORD`。
/// * 返回 `(next_frn, 记录区长度)`；`next_frn == 0` 表示枚举完成。
///
/// 这一路只需要**一次全卷 IO**，但返回的记录里没有文件大小（文档坑 #1）。
pub fn enum_page(
    vol: &Volume,
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
    buf: &mut [u8],
) -> io::Result<(u64, usize)> {
    if buf.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "枚举缓冲至少需要 8 字节",
        ));
    }
    let mut input = vec![0u8; MFT_ENUM_DATA_LEN];
    input[0x00..0x08].copy_from_slice(&start_frn.to_le_bytes());
    input[0x08..0x10].copy_from_slice(&low_usn.to_le_bytes());
    input[0x10..0x18].copy_from_slice(&high_usn.to_le_bytes());

    // 输出缓冲的首 8 字节是 in/out，必须先清零
    for b in buf.iter_mut() {
        *b = 0;
    }

    let res = vol.device_io_allow(
        FSCTL_ENUM_USN_DATA,
        &mut input,
        buf,
        &[ERROR_HANDLE_EOF],
    )?;
    let n = match res {
        None => return Ok((0, 0)), // EOF：枚举结束
        Some(n) => n,
    };
    if n <= 8 {
        return Ok((0, 0));
    }
    let next_frn = bytes_u64(buf, 0).unwrap_or(0);
    // 只有当系统明确给出 0 时才认为结束；否则继续
    Ok((next_frn, n - 8))
}

/// `FSCTL_READ_USN_JOURNAL` 请求参数。
#[derive(Debug, Clone, Copy)]
pub struct ReadRequest {
    pub start_usn: i64,
    pub reason_mask: u32,
    /// 文档 §5.2：建议置 1，只在句柄关闭时收口，避免写入过程刷屏
    pub return_only_on_close: u32,
    pub timeout: u64,
    pub bytes_to_wait_for: u64,
    pub journal_id: u64,
}

impl ReadRequest {
    /// 默认：收口模式 + 匹配全部 Reason。
    pub fn new(journal_id: u64, start_usn: i64) -> ReadRequest {
        ReadRequest {
            start_usn,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 1,
            timeout: 0,
            bytes_to_wait_for: 0,
            journal_id,
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        let mut v = vec![0u8; READ_USN_JOURNAL_DATA_LEN];
        v[0x00..0x08].copy_from_slice(&self.start_usn.to_le_bytes());
        v[0x08..0x0C].copy_from_slice(&self.reason_mask.to_le_bytes());
        v[0x0C..0x10].copy_from_slice(&self.return_only_on_close.to_le_bytes());
        v[0x10..0x18].copy_from_slice(&self.timeout.to_le_bytes());
        v[0x18..0x20].copy_from_slice(&self.bytes_to_wait_for.to_le_bytes());
        v[0x20..0x28].copy_from_slice(&self.journal_id.to_le_bytes());
        v
    }
}

/// 从 `start_usn` 起拉一批变更。返回 `(next_usn, 记录区长度)`。
pub fn read_page(vol: &Volume, req: ReadRequest, buf: &mut [u8]) -> io::Result<(i64, usize)> {
    if buf.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "读取缓冲至少需要 8 字节",
        ));
    }
    // FSCTL_READ_USN_JOURNAL 的输出头也是 in/out 语义，先写回 StartUsn
    for b in buf.iter_mut() {
        *b = 0;
    }
    buf[0..8].copy_from_slice(&req.start_usn.to_le_bytes());

    let mut input = req.to_bytes();
    let res = vol.device_io_allow(
        FSCTL_READ_USN_JOURNAL,
        &mut input,
        buf,
        &[ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE],
    )?;
    let n = match res {
        None => return Ok((req.start_usn, 0)),
        Some(n) => n,
    };
    if n < 8 {
        return Ok((req.start_usn, 0));
    }
    let next_usn = bytes_u64(buf, 0).unwrap_or(0) as i64;
    Ok((next_usn, n - 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_journal_data() {
        let mut b = vec![0u8; 56];
        b[0x00..0x08].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        b[0x08..0x10].copy_from_slice(&100i64.to_le_bytes());
        b[0x10..0x18].copy_from_slice(&900i64.to_le_bytes());
        b[0x18..0x20].copy_from_slice(&50i64.to_le_bytes());
        b[0x20..0x28].copy_from_slice(&1000i64.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&(32u64 * 1024 * 1024).to_le_bytes());
        b[0x30..0x38].copy_from_slice(&(4u64 * 1024 * 1024).to_le_bytes());

        let d = UsnJournalData::parse(&b).unwrap();
        assert_eq!(d.id, 0x1122_3344_5566_7788);
        assert_eq!(d.first_usn, 100);
        assert_eq!(d.next_usn, 900);
        assert_eq!(d.maximum_size, 32 * 1024 * 1024);
        assert_eq!(d.allocation_delta, 4 * 1024 * 1024);
    }

    #[test]
    fn journal_parse_rejects_short_buffer() {
        assert!(UsnJournalData::parse(&[0u8; 55]).is_none());
    }

    #[test]
    fn free_ratio_sane() {
        let d = UsnJournalData {
            maximum_size: 1000,
            first_usn: 0,
            next_usn: 250,
            ..Default::default()
        };
        assert!((d.free_ratio() - 0.75).abs() < 1e-9);
        let empty = UsnJournalData { maximum_size: 0, ..Default::default() };
        assert_eq!(empty.free_ratio(), 1.0);
    }

    #[test]
    fn read_request_serialization_offsets() {
        let r = ReadRequest {
            start_usn: 0x0102_0304_0506_0708,
            reason_mask: 0xAABB_CCDD,
            return_only_on_close: 1,
            timeout: 7,
            bytes_to_wait_for: 9,
            journal_id: 0xDEAD,
        };
        let v = r.to_bytes();
        assert_eq!(v.len(), READ_USN_JOURNAL_DATA_LEN);
        assert_eq!(bytes_u64(&v, 0x00), Some(0x0102_0304_0506_0708));
        assert_eq!(bytes_u64(&v, 0x08).map(|x| x as u32), Some(0xAABB_CCDD));
        assert_eq!(bytes_u64(&v, 0x0C).map(|x| x as u32), Some(1));
        assert_eq!(bytes_u64(&v, 0x10), Some(7));
        assert_eq!(bytes_u64(&v, 0x18), Some(9));
        assert_eq!(bytes_u64(&v, 0x20), Some(0xDEAD));
    }
}
