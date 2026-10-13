//! 单条 MFT 记录解析（对应文档附录 A 的伪代码，但做了完整边界检查）。
//!
//! 解析顺序刻意按「从便宜到贵」排列，先把明显无效的记录挡掉：
//! 1. 4 字节签名 `"FILE"`（`"BAAD"` 也在这里被挡）
//! 2. `flags & 0x0001`（未使用记录直接跳过）
//! 3. USA fixup（防撕裂写）
//! 4. 属性链遍历

use crate::attribute::{AttributeIter, ATTR_ATTRIBUTE_LIST, ATTR_DATA, ATTR_FILE_NAME};
use crate::bytes::u16_at;
use crate::filename::{is_better_namespace, make_frn, parse_file_name_value};
use crate::fixup::apply_fixup;

pub const FILE_SIGNATURE: &[u8; 4] = b"FILE";
pub const BAAD_SIGNATURE: &[u8; 4] = b"BAAD";

pub const RECORD_IN_USE: u16 = 0x0001;
pub const RECORD_IS_DIR: u16 = 0x0002;

/// 一条已解析的 MFT 记录。
///
/// 为了让「零拷贝」成立，`name` 不存字符串，而是存**记录缓冲内的偏移 + 字符数**；
/// 调用方在缓冲被覆盖前把它解码进字符串池（见 `core-index::string_pool`）。
#[derive(Debug, Clone, Copy)]
pub struct MftRecord {
    /// 完整 FRN（序列号 << 48 | 记录号）
    pub frn: u64,
    /// 记录号（= FRN 低 48 位）
    pub recno: u64,
    pub seq: u16,
    pub flags: u16,
    pub hard_link_count: u16,
    pub parent_frn: u64,
    pub namespace: u8,
    /// 文件名在**记录缓冲**中的绝对偏移
    pub name_off: u32,
    /// 文件名字符数（UTF-16 码元）
    pub name_chars: u16,
    /// 逻辑大小（真实字节数）
    pub real_size: u64,
    /// 占用大小（磁盘分配字节数）
    pub alloc_size: u64,
    /// 修改时间（Unix 秒）
    pub mtime: i64,
    /// `$FILE_NAME` 的属性位（稀疏/压缩/重解析）
    pub fn_flags: u32,
    /// 存在 `$ATTRIBUTE_LIST`（属性溢出到扩展记录）
    pub has_attr_list: bool,
    /// `$DATA` 是驻留的（小文件）
    pub resident_data: bool,
    /// 是否含命名 `$DATA`（即存在 ADS，不计入主流统计）
    pub has_named_stream: bool,
}

impl MftRecord {
    #[inline]
    pub fn in_use(&self) -> bool {
        self.flags & RECORD_IN_USE != 0
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & RECORD_IS_DIR != 0
    }

    #[inline]
    pub fn is_hard_link(&self) -> bool {
        self.hard_link_count > 1
    }

    /// 取名字的原始 UTF-16LE 字节切片。
    /// **必须**在对应记录缓冲被覆盖之前调用。
    #[inline]
    pub fn name_bytes<'a>(&self, record_buf: &'a [u8]) -> Option<&'a [u8]> {
        let s = self.name_off as usize;
        let e = s + self.name_chars as usize * 2;
        record_buf.get(s..e)
    }
}

/// 极便宜的预过滤：只查签名与 in-use 位。
///
/// 文档 §5.1 性能要点 1：先用这个把空闲记录、目录记录挡掉，
/// 避免对无效记录做 fixup 校验和属性链遍历。
#[inline]
pub fn looks_like_active_file(rec: &[u8]) -> bool {
    if rec.len() < 0x18 {
        return false;
    }
    if &rec[0..4] != FILE_SIGNATURE {
        return false;
    }
    let flags = u16::from_le_bytes([rec[0x16], rec[0x17]]);
    flags & RECORD_IN_USE != 0
}

/// 解析一条记录。`rec` 会被**就地** fixup 修改。
///
/// * `recno` —— 这条记录的记录号（用于拼 FRN）
/// * `sector_size` —— 通常 512
///
/// 返回 `None` 表示：空闲记录 / 损坏记录 / 缺少 `$FILE_NAME`。
pub fn parse_record(rec: &mut [u8], recno: u64, sector_size: usize) -> Option<MftRecord> {
    // 1. 签名
    if rec.len() < 0x30 || &rec[0..4] != FILE_SIGNATURE {
        return None;
    }

    // 2. flags 粗过滤（放在 fixup 之前，省掉一次全记录扫描）
    let flags = u16_at(rec, 0x16)?;
    if flags & RECORD_IN_USE == 0 {
        return None;
    }

    // 3. fixup
    if !apply_fixup(rec, sector_size) {
        return None;
    }

    // 4. 头部字段
    let seq = u16_at(rec, 0x10)?;
    let hard_link_count = u16_at(rec, 0x12)?;
    let first_attr = u16_at(rec, 0x14)? as usize;
    if first_attr < 0x18 || first_attr >= rec.len() {
        return None;
    }

    // 5. 属性链
    let mut best_ns: Option<u8> = None;
    let mut name_off: u32 = 0;
    let mut name_chars: u16 = 0;
    let mut parent_frn: u64 = 0;
    let mut mtime: i64 = 0;
    let mut fn_flags: u32 = 0;
    let mut real_size: u64 = 0;
    let mut alloc_size: u64 = 0;
    let mut has_attr_list = false;
    let mut resident_data = false;
    let mut has_named_stream = false;
    let mut got_data = false;

    let base = rec.as_ptr() as usize;

    for attr in AttributeIter::new(rec, first_attr) {
        match attr.ty {
            ATTR_ATTRIBUTE_LIST => {
                has_attr_list = true;
            }
            ATTR_FILE_NAME => {
                let value = match attr.resident_value() {
                    Some(v) => v,
                    None => continue,
                };
                let f = match parse_file_name_value(value) {
                    Some(f) => f,
                    None => continue,
                };
                if !is_better_namespace(f.namespace, best_ns) {
                    continue;
                }
                // 名字在整条记录里的绝对偏移：
                //   记录起点 → 属性起点 → 值起点 → 名字起点
                let value_ptr = value.as_ptr() as usize;
                let abs = value_ptr
                    .wrapping_sub(base)
                    .wrapping_add(f.name_offset_in_value as usize);
                if abs + f.name_chars as usize * 2 > rec.len() {
                    continue;
                }
                best_ns = Some(f.namespace);
                name_off = abs as u32;
                name_chars = f.name_chars;
                parent_frn = f.parent_frn;
                mtime = f.mtime;
                fn_flags = f.flags;
            }
            ATTR_DATA => {
                // 只统计**未命名**的主流（ADS 单独标记）
                if attr.is_named() {
                    has_named_stream = true;
                    continue;
                }
                if got_data {
                    continue; // 已有主流
                }
                if attr.non_resident {
                    if let Some((alloc, real, _init)) = attr.non_resident_sizes() {
                        alloc_size = alloc;
                        real_size = real;
                        resident_data = false;
                        got_data = true;
                    }
                } else if let Some(v) = attr.resident_value() {
                    // 驻留：逻辑大小 = 值长度；占用大小 = 0 簇
                    real_size = v.len() as u64;
                    alloc_size = 0;
                    resident_data = true;
                    got_data = true;
                }
            }
            _ => {}
        }
    }

    // 没有 $FILE_NAME 就无法构建目录树
    if best_ns.is_none() || name_chars == 0 {
        return None;
    }

    Some(MftRecord {
        frn: make_frn(recno, seq),
        recno,
        seq,
        flags,
        hard_link_count,
        parent_frn,
        namespace: best_ns.unwrap_or(0),
        name_off,
        name_chars,
        real_size,
        alloc_size,
        mtime,
        fn_flags,
        has_attr_list,
        resident_data,
        has_named_stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribute::{ATTR_END, ATTR_STANDARD_INFORMATION};

    /// 造一条最小可用 MFT 记录：$FILE_NAME(Win32) + $DATA(非驻留)
    fn build_record(recno_seq: u16, flags: u16, parent: u64, name: &str, real: u64, alloc: u64) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();

        // $FILE_NAME 属性（驻留）
        let fn_val_len = 0x42 + units.len() * 2;
        let fn_attr_len = 0x18 + fn_val_len;
        let mut fn_attr = vec![0u8; fn_attr_len];

        // $DATA 属性（非驻留，run 在 0x40）
        let data_attr_len = 0x48;
        let mut data_attr = vec![0u8; data_attr_len];

        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // USA count (1024/512+1)
        rec[0x10..0x12].copy_from_slice(&recno_seq.to_le_bytes()); // seq
        rec[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // hard link count
        rec[0x16..0x18].copy_from_slice(&flags.to_le_bytes());

        let mut off = 0x38usize;
        rec[0x14..0x16].copy_from_slice(&(off as u16).to_le_bytes());

        // ── $STANDARD_INFORMATION（占位）
        let si_len = 0x48;
        let mut si = vec![0u8; si_len];
        si[0..4].copy_from_slice(&ATTR_STANDARD_INFORMATION.to_le_bytes());
        si[4..8].copy_from_slice(&(si_len as u32).to_le_bytes());
        si[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        si[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        rec[off..off + si_len].copy_from_slice(&si);
        off += si_len;

        // ── $FILE_NAME
        fn_attr[0..4].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
        fn_attr[4..8].copy_from_slice(&(fn_attr_len as u32).to_le_bytes());
        fn_attr[8] = 0;
        fn_attr[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        fn_attr[0x10..0x14].copy_from_slice(&(fn_val_len as u32).to_le_bytes());
        fn_attr[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let v = &mut fn_attr[0x18..];
        v[0x00..0x08].copy_from_slice(&parent.to_le_bytes());
        v[0x10..0x18].copy_from_slice(&1_700_000_100i64.to_le_bytes());
        v[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = crate::filename::NS_WIN32;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        rec[off..off + fn_attr_len].copy_from_slice(&fn_attr);
        off += fn_attr_len;

        // ── $DATA（非驻留）
        data_attr[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        data_attr[4..8].copy_from_slice(&(data_attr_len as u32).to_le_bytes());
        data_attr[8] = 1;
        data_attr[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        data_attr[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        data_attr[0x30..0x38].copy_from_slice(&real.to_le_bytes());
        data_attr[0x38..0x40].copy_from_slice(&real.to_le_bytes());
        data_attr[0x40] = 0x21;
        data_attr[0x41] = 0x08;
        data_attr[0x42] = 0x05;
        data_attr[0x43] = 0x00;
        data_attr[0x44] = 0x00;
        rec[off..off + data_attr_len].copy_from_slice(&data_attr);
        off += data_attr_len;

        // ── 终止符
        rec[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());

        // USA：把两个扇区末尾换成 USN，并把真实值存进 USA 数组
        let usn: u16 = 0xBEEF;
        let s1 = u16::from_le_bytes([rec[510], rec[511]]);
        let s2 = u16::from_le_bytes([rec[1022], rec[1023]]);
        rec[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
        rec[0x32..0x34].copy_from_slice(&s1.to_le_bytes());
        rec[0x34..0x36].copy_from_slice(&s2.to_le_bytes());
        rec[510..512].copy_from_slice(&usn.to_le_bytes());
        rec[1022..1024].copy_from_slice(&usn.to_le_bytes());
        rec
    }

    #[test]
    fn quick_filter_rejects_free_records() {
        let mut r = build_record(1, RECORD_IN_USE, 5, "a.txt", 10, 4096);
        assert!(looks_like_active_file(&r));
        r[0x16..0x18].copy_from_slice(&0u16.to_le_bytes());
        assert!(!looks_like_active_file(&r));
    }

    #[test]
    fn quick_filter_rejects_baad_and_short() {
        let mut r = build_record(1, RECORD_IN_USE, 5, "a.txt", 10, 4096);
        r[0..4].copy_from_slice(b"BAAD");
        assert!(!looks_like_active_file(&r));
        assert!(!looks_like_active_file(&[0u8; 4]));
    }

    #[test]
    fn parses_full_record() {
        let mut r = build_record(42, RECORD_IN_USE, 0x0005_0000_0000_0005, "report.pdf", 12345, 16384);
        let rec = parse_record(&mut r, 42, 512).expect("should parse");
        assert_eq!(rec.recno, 42);
        assert_eq!(rec.seq, 42);
        assert_eq!(rec.frn, make_frn(42, 42));
        assert!(!rec.is_dir());
        assert_eq!(rec.parent_frn, 0x0005_0000_0000_0005);
        assert_eq!(rec.real_size, 12345);
        assert_eq!(rec.alloc_size, 16384);
        assert_eq!(rec.mtime, 1_700_000_100);
        assert_eq!(rec.namespace, crate::filename::NS_WIN32);
        assert_eq!(rec.name_chars, 10);
        let nb = rec.name_bytes(&r).unwrap();
        let units: Vec<u16> = nb.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&units), "report.pdf");
    }

    #[test]
    fn detects_directory_flag() {
        let mut r = build_record(5, RECORD_IN_USE | RECORD_IS_DIR, 5, ".", 0, 0);
        let rec = parse_record(&mut r, 5, 512).unwrap();
        assert!(rec.is_dir());
    }

    #[test]
    fn rejects_corrupted_fixup() {
        let mut r = build_record(42, RECORD_IN_USE, 5, "x.txt", 1, 1);
        r[1022] ^= 0xFF; // 撕裂写
        assert!(parse_record(&mut r, 42, 512).is_none());
    }
}
