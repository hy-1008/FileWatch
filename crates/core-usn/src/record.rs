//! USN 记录解析（`USN_RECORD_V2` / `V3`）。
//!
//! `USN_RECORD_V2` 头（60 字节，之后紧跟 UTF-16LE 文件名）
//! ```text
//! 0x00 RecordLength(4)        0x04 MajorVersion(2)   0x06 MinorVersion(2)
//! 0x08 FileReferenceNumber(8) 0x10 ParentFileReferenceNumber(8)
//! 0x18 Usn(8)                 0x20 TimeStamp(8)      0x28 Reason(4)
//! 0x2C SourceInfo(4)          0x30 SecurityId(4)     0x34 FileAttributes(4)
//! 0x38 FileNameLength(2)      0x3A FileNameOffset(2) 0x3C FileName(UTF-16LE)
//! ```
//!
//! `USN_RECORD_V3` 把两个 FRN 扩到 **128 位**（ReFS 兼容），头变成 76 字节。
//! 文档 §5.2 明确要求「按 MajorVersion 分支解析」，这里严格照做。

use crate::bytes_u32;
use crate::bytes_u64;

pub const V2_HEADER_LEN: usize = 60;
pub const V3_HEADER_LEN: usize = 76;

/// 记录长度字段的上限保护，避免损坏数据导致越界/超大分配。
const MAX_RECORD_LEN: usize = 1 << 20;

#[derive(Debug, Clone, Copy)]
pub struct UsnRecord {
    /// 文件 FRN（V3 的 128 位会被截断到低 64 位并置 `is_v3`）
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    /// 时间戳（Unix 秒）
    pub timestamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    /// 文件名在**本记录缓冲**中的绝对偏移
    pub name_off: u32,
    /// 文件名字符数（UTF-16 码元）
    pub name_chars: u16,
    pub major_version: u16,
    /// V3 记录（128 位 FRN，ReFS）
    pub is_v3: bool,
}

impl UsnRecord {
    #[inline]
    pub fn name_bytes<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let s = self.name_off as usize;
        let e = s + self.name_chars as usize * 2;
        buf.get(s..e)
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.file_attributes & core_io::winapi::FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// 解析一段连续的 USN 记录（`buf` 起点必须是第一条记录的起点，
/// 即已跳过 `FSCTL_READ_USN_JOURNAL` / `FSCTL_ENUM_USN_DATA` 的 8 字节返回头）。
///
/// 采用「尽力而为」策略：遇到损坏记录立即停止，返回已成功解析的部分。
pub fn parse_records(buf: &[u8]) -> Vec<UsnRecord> {
    let mut out = Vec::new();
    let mut off = 0usize;

    while off + 8 <= buf.len() {
        let rec_len = match bytes_u32(buf, off) {
            Some(v) => v as usize,
            None => break,
        };
        if rec_len < V2_HEADER_LEN && rec_len < V3_HEADER_LEN {
            break; // 长度不合法（含 0）
        }
        if rec_len > MAX_RECORD_LEN || off + rec_len > buf.len() {
            break; // 越界
        }
        match parse_one(&buf[off..off + rec_len], off) {
            Some(r) => out.push(r),
            None => break,
        }
        off += rec_len;
    }
    out
}

fn parse_one(rec: &[u8], base: usize) -> Option<UsnRecord> {
    let major_version = crate::bytes_u16(rec, 0x04)?;
    let is_v3 = major_version >= 3;

    // FRN 与其父 FRN
    let (frn, parent_frn) = if is_v3 {
        let f = crate::bytes_u128(rec, 0x08)? as u64; // 低 64 位足够做索引
        let p = crate::bytes_u128(rec, 0x18)? as u64;
        (f, p)
    } else {
        (bytes_u64(rec, 0x08)?, bytes_u64(rec, 0x10)?)
    };

    let hdr = if is_v3 { V3_HEADER_LEN } else { V2_HEADER_LEN };
    if rec.len() < hdr {
        return None;
    }

    // V3 的后续字段整体后移 16 字节（两个 FRN 各多 8 字节）
    let shift = if is_v3 { 16 } else { 0 };
    let usn = bytes_u64(rec, 0x18 + shift)? as i64;
    let ts_raw = bytes_u64(rec, 0x20 + shift)? as i64;
    let reason = bytes_u32(rec, 0x28 + shift)?;
    let source_info = bytes_u32(rec, 0x2C + shift)?;
    let security_id = bytes_u32(rec, 0x30 + shift)?;
    let file_attributes = bytes_u32(rec, 0x34 + shift)?;
    let name_len_bytes = crate::bytes_u16(rec, 0x38 + shift)? as usize;
    let name_off_rel = crate::bytes_u16(rec, 0x3A + shift)? as usize;

    if name_len_bytes == 0 || name_len_bytes % 2 != 0 {
        return None;
    }
    if name_off_rel + name_len_bytes > rec.len() {
        return None;
    }

    // FILETIME（1601 起 100ns）→ Unix 秒
    let timestamp = filetime_to_unix(ts_raw);

    Some(UsnRecord {
        frn,
        parent_frn,
        usn,
        timestamp,
        reason,
        source_info,
        security_id,
        file_attributes,
        name_off: (base + name_off_rel) as u32,
        name_chars: (name_len_bytes / 2) as u16,
        major_version,
        is_v3,
    })
}

/// FILETIME 计数 → Unix 秒。
#[inline]
pub fn filetime_to_unix(ticks: i64) -> i64 {
    if ticks <= 0 {
        return 0;
    }
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    ticks / 10_000_000 - EPOCH_DIFF_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条 V2 记录
    fn build_v2(frn: u64, parent: u64, usn: i64, reason: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = units.len() * 2;
        let total = V2_HEADER_LEN + name_bytes;
        // 记录长度需 8 字节对齐（Windows 实践）
        let total_aligned = (total + 7) & !7;
        let mut r = vec![0u8; total_aligned];
        r[0x00..0x04].copy_from_slice(&(total_aligned as u32).to_le_bytes());
        r[0x04..0x06].copy_from_slice(&2u16.to_le_bytes());
        r[0x06..0x08].copy_from_slice(&0u16.to_le_bytes());
        r[0x08..0x10].copy_from_slice(&frn.to_le_bytes());
        r[0x10..0x18].copy_from_slice(&parent.to_le_bytes());
        r[0x18..0x20].copy_from_slice(&usn.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&(11_644_473_600i64 * 10_000_000 + 1_700_000_000 * 10_000_000).to_le_bytes());
        r[0x28..0x2C].copy_from_slice(&reason.to_le_bytes());
        r[0x34..0x38].copy_from_slice(&core_io::winapi::FILE_ATTRIBUTE_ARCHIVE.to_le_bytes());
        r[0x38..0x3A].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        r[0x3A..0x3C].copy_from_slice(&(V2_HEADER_LEN as u16).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[V2_HEADER_LEN + i * 2..V2_HEADER_LEN + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_v2_record() {
        let r = build_v2(0x000A_0000_0000_002A, 0x0005_0000_0000_0005, 12345, 0x100, "new.txt");
        let recs = parse_records(&r);
        assert_eq!(recs.len(), 1);
        let u = recs[0];
        assert_eq!(u.frn, 0x000A_0000_0000_002A);
        assert_eq!(u.parent_frn, 0x0005_0000_0000_0005);
        assert_eq!(u.usn, 12345);
        assert_eq!(u.reason, 0x100);
        assert_eq!(u.major_version, 2);
        assert!(!u.is_v3);
        assert_eq!(u.timestamp, 1_700_000_000);
        assert_eq!(u.name_chars, 7);
        let nb = u.name_bytes(&r).unwrap();
        let units: Vec<u16> = nb.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&units), "new.txt");
    }

    #[test]
    fn parses_multiple_records() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_v2(1, 5, 10, 0x100, "a"));
        buf.extend_from_slice(&build_v2(2, 5, 20, 0x200, "bb"));
        buf.extend_from_slice(&build_v2(3, 5, 30, 0x1000, "ccc"));
        let recs = parse_records(&buf);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].usn, 10);
        assert_eq!(recs[1].frn, 2);
        assert_eq!(recs[2].name_chars, 3);
    }

    #[test]
    fn parses_v3_record_with_128bit_frn() {
        let name = "x";
        let units: Vec<u16> = name.encode_utf16().collect();
        let total = V3_HEADER_LEN + units.len() * 2;
        let total = (total + 7) & !7;
        let mut r = vec![0u8; total];
        r[0x00..0x04].copy_from_slice(&(total as u32).to_le_bytes());
        r[0x04..0x06].copy_from_slice(&3u16.to_le_bytes());
        // FRN 128 位：低 64 位 = 77
        r[0x08..0x10].copy_from_slice(&77u64.to_le_bytes());
        r[0x18..0x20].copy_from_slice(&5u64.to_le_bytes());
        r[0x28..0x30].copy_from_slice(&999i64.to_le_bytes()); // usn
        r[0x38..0x3C].copy_from_slice(&0x200u32.to_le_bytes()); // reason @ 0x28+16
        r[0x48..0x4A].copy_from_slice(&(units.len() as u16 * 2).to_le_bytes()); // name len @0x38+16
        r[0x4A..0x4C].copy_from_slice(&(V3_HEADER_LEN as u16).to_le_bytes()); // name off @0x3A+16
        for (i, u) in units.iter().enumerate() {
            r[V3_HEADER_LEN + i * 2..V3_HEADER_LEN + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }

        let recs = parse_records(&r);
        assert_eq!(recs.len(), 1);
        assert!(recs[0].is_v3);
        assert_eq!(recs[0].major_version, 3);
        assert_eq!(recs[0].frn, 77);
        assert_eq!(recs[0].usn, 999);
        assert_eq!(recs[0].reason, 0x200);
    }

    #[test]
    fn stops_on_corrupt_record() {
        let mut buf = build_v2(1, 5, 10, 0x100, "a");
        buf.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00]); // 荒谬的长度
        assert_eq!(parse_records(&buf).len(), 1);
    }

    #[test]
    fn stops_on_truncated_tail() {
        let good = build_v2(1, 5, 10, 0x100, "a");
        let mut buf = good.clone();
        buf.extend_from_slice(&good[..10]); // 只有半条
        assert_eq!(parse_records(&buf).len(), 1);
    }

    #[test]
    fn rejects_zero_length_records() {
        let buf = vec![0u8; 200];
        assert!(parse_records(&buf).is_empty());
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix(0), 0);
        let t = 11_644_473_600i64 * 10_000_000;
        assert_eq!(filetime_to_unix(t), 0);
        assert_eq!(filetime_to_unix(t + 10_000_000), 1);
    }
}
