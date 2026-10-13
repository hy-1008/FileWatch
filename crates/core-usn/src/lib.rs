//! # core-usn —— USN Journal 读取与事件归一化
//!
//! 分层（文档 §4.2）：**不依赖 UI**，可独立单测。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`journal`] | `QueryUSNJournal` / `CreateUSNJournal` / `EnumUSNData` / `ReadUSNJournal` |
//! | [`record`] | `USN_RECORD_V2` / `V3` 解析（按 MajorVersion 分支） |
//! | [`normalize`] | Reason 位掩码 → 语义事件；回收站/临时文件规则 |
//! | [`cursor`] | `(JournalID, NextUsn)` 持久化与断层自愈判定 |
//!
//! 文档坑 #1 提醒：**USN 记录不带文件大小**。本 crate 只负责「谁变了」，
//! 「变成多大」由 `core-scan` 回读 MFT 记录（复用 `core-ntfs` 的同一个解析器）。

pub mod cursor;
pub mod journal;
pub mod normalize;
pub mod record;

pub use cursor::{evaluate, Cursor, CursorValidity};
pub use journal::{ReadRequest, UsnJournalData};
pub use normalize::{classify, EventKind};
pub use record::{parse_records, UsnRecord};

// ── 小端字节读取（供 USN 结构体解析用） ─────────────────────────────────────

#[inline]
pub fn bytes_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
pub fn bytes_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
pub fn bytes_u64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

#[inline]
pub fn bytes_u128(b: &[u8], off: usize) -> Option<u128> {
    let s = b.get(off..off + 16)?;
    let mut a = [0u8; 16];
    a.copy_from_slice(s);
    Some(u128::from_le_bytes(a))
}

/// 默认读取缓冲：64 KB（一页通常能返回几百条记录）
pub const DEFAULT_READ_BUFFER: usize = 64 * 1024;
/// 全量枚举缓冲：1 MB
pub const DEFAULT_ENUM_BUFFER: usize = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_readers_check_bounds() {
        let b = [0x11u8, 0x22, 0x33, 0x44];
        assert_eq!(bytes_u16(&b, 0), Some(0x2211));
        assert_eq!(bytes_u32(&b, 0), Some(0x4433_2211));
        assert_eq!(bytes_u64(&b, 0), None);
        assert_eq!(bytes_u128(&b, 0), None);
    }
}
