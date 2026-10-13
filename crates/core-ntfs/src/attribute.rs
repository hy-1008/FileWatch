//! MFT 属性链遍历。
//!
//! 属性类型速查（文档 §5.1）：
//! `0x10` 标准信息 / `0x20` 属性列表 / `0x30` 文件名 / `0x80` 数据 /
//! `0x90` 索引根 / `0xA0` 索引分配 / `0xB0` 位图 / `0xC0` 重解析点

use crate::bytes::*;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_OBJECT_ID: u32 = 0x40;
pub const ATTR_SECURITY_DESCRIPTOR: u32 = 0x50;
pub const ATTR_VOLUME_NAME: u32 = 0x60;
pub const ATTR_VOLUME_INFORMATION: u32 = 0x70;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_INDEX_ROOT: u32 = 0x90;
pub const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
pub const ATTR_BITMAP: u32 = 0xB0;
pub const ATTR_REPARSE_POINT: u32 = 0xC0;
pub const ATTR_EA_INFORMATION: u32 = 0xD0;
pub const ATTR_EA: u32 = 0xE0;
pub const ATTR_LOGGED_UTILITY_STREAM: u32 = 0x100;

pub const ATTR_END: u32 = 0xFFFF_FFFF;

/// 属性公共头字段（`0x10` 字节），后面跟驻留/非驻留专有部分。
pub const ATTR_HEADER_LEN: usize = 0x10;

#[derive(Debug, Clone, Copy)]
pub struct Attribute<'a> {
    /// 属性类型
    pub ty: u32,
    /// 整条属性的总长度（含头）
    pub len: u32,
    pub non_resident: bool,
    pub name_len: u8,
    pub name_off: u16,
    pub flags: u16,
    pub attr_id: u16,
    /// 已裁剪到 `len` 的原始字节
    pub raw: &'a [u8],
}

impl<'a> Attribute<'a> {
    /// 尝试把 `raw` 的开头当作一条属性解析。
    /// `raw` 可以比这条属性长（会自行裁剪）。
    pub fn parse(raw: &'a [u8]) -> Option<Attribute<'a>> {
        if raw.len() < ATTR_HEADER_LEN {
            return None;
        }
        let ty = u32_at(raw, 0)?;
        let len = u32_at(raw, 4)? as usize;
        if len < ATTR_HEADER_LEN || len > raw.len() {
            return None;
        }
        let raw = &raw[..len];
        Some(Attribute {
            ty,
            len: len as u32,
            non_resident: *raw.get(8)? != 0,
            name_len: *raw.get(9)?,
            name_off: u16_at(raw, 0x0A)?,
            flags: u16_at(raw, 0x0C)?,
            attr_id: u16_at(raw, 0x0E)?,
            raw,
        })
    }

    /// 是否为「命名属性」——即备选数据流（ADS）或命名索引。
    /// 主 `$DATA` 流是未命名的（文档 §3.2：只统计主流）。
    #[inline]
    pub fn is_named(&self) -> bool {
        self.name_len > 0
    }

    /// 属性名（UTF-16 → String）
    pub fn name(&self) -> Option<String> {
        if self.name_len == 0 {
            return None;
        }
        let start = self.name_off as usize;
        let end = start + self.name_len as usize * 2;
        let s = self.raw.get(start..end)?;
        let units: Vec<u16> = s
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    }

    /// 驻留属性的值区。
    pub fn resident_value(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let vlen = u32_at(self.raw, 0x10)? as usize;
        let voff = u16_at(self.raw, 0x14)? as usize;
        self.raw.get(voff..voff + vlen)
    }

    /// 驻留属性的值区在记录中的**绝对偏移**（配合 `raw` 的起点使用）。
    pub fn resident_value_offset(&self) -> Option<usize> {
        if self.non_resident {
            return None;
        }
        u16_at(self.raw, 0x14).map(|v| v as usize)
    }

    /// 非驻留属性的 `(分配大小, 真实大小, 已初始化大小)`
    pub fn non_resident_sizes(&self) -> Option<(u64, u64, u64)> {
        if !self.non_resident {
            return None;
        }
        Some((
            u64_at(self.raw, 0x28)?,
            u64_at(self.raw, 0x30)?,
            u64_at(self.raw, 0x38)?,
        ))
    }

    /// 非驻留属性的 Data Runs 字节串（从 run 偏移到属性尾）。
    pub fn run_list(&self) -> Option<&'a [u8]> {
        if !self.non_resident {
            return None;
        }
        let roff = u16_at(self.raw, 0x20)? as usize;
        if roff == 0 || roff >= self.raw.len() {
            return None;
        }
        self.raw.get(roff..)
    }

    /// 是否压缩（`flags & 0x0001`）——影响「占用大小」语义（文档 §3.2）
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags & 0x0001 != 0
    }

    /// 是否稀疏（`flags & 0x8000`）
    #[inline]
    pub fn is_sparse(&self) -> bool {
        self.flags & 0x8000 != 0
    }
}

/// 属性链迭代器：从 `first_attr_offset` 起，遇 `ATTR_END` 停止。
///
/// 迭代器**永不 panic**：任何越界/非法长度都直接终止迭代。
pub struct AttributeIter<'a> {
    buf: &'a [u8],
    off: usize,
    done: bool,
}

impl<'a> AttributeIter<'a> {
    pub fn new(buf: &'a [u8], first_attr_offset: usize) -> AttributeIter<'a> {
        AttributeIter {
            buf,
            off: first_attr_offset,
            done: false,
        }
    }
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Attribute<'a>> {
        if self.done {
            return None;
        }
        let rest = match self.buf.get(self.off..) {
            Some(r) if r.len() >= 4 => r,
            _ => {
                self.done = true;
                return None;
            }
        };
        // 终止符
        if rest[0] == 0xFF && rest[1] == 0xFF && rest[2] == 0xFF && rest[3] == 0xFF {
            self.done = true;
            return None;
        }
        match Attribute::parse(rest) {
            Some(a) => {
                let len = a.len as usize;
                self.off += len;
                if len < ATTR_HEADER_LEN {
                    self.done = true;
                }
                Some(a)
            }
            None => {
                self.done = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resident_attr(ty: u32, value: &[u8]) -> Vec<u8> {
        let vlen = value.len();
        let total = 0x18 + vlen;
        let mut a = vec![0u8; total];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        a[8] = 0; // resident
        a[9] = 0; // unnamed
        a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(vlen as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..].copy_from_slice(value);
        a
    }

    #[test]
    fn parses_resident_value() {
        let a_bytes = resident_attr(ATTR_FILE_NAME, b"hello");
        let a = Attribute::parse(&a_bytes).unwrap();
        assert_eq!(a.ty, ATTR_FILE_NAME);
        assert!(!a.non_resident);
        assert!(!a.is_named());
        assert_eq!(a.resident_value().unwrap(), b"hello");
        assert_eq!(a.resident_value_offset(), Some(0x18));
    }

    #[test]
    fn iterates_until_end_marker() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&resident_attr(ATTR_STANDARD_INFORMATION, b"\x00\x01"));
        buf.extend_from_slice(&resident_attr(ATTR_FILE_NAME, b"abc"));
        buf.extend_from_slice(&ATTR_END.to_le_bytes());

        let tys: Vec<u32> = AttributeIter::new(&buf, 0).map(|a| a.ty).collect();
        assert_eq!(tys, vec![ATTR_STANDARD_INFORMATION, ATTR_FILE_NAME]);
    }

    #[test]
    fn iterator_tolerates_garbage() {
        let buf = [0xFFu8, 0xFF, 0xFF, 0xFF];
        assert_eq!(AttributeIter::new(&buf, 0).count(), 0);
    }

    #[test]
    fn iterator_stops_on_bad_length() {
        let mut a = resident_attr(ATTR_DATA, b"x");
        a[4..8].copy_from_slice(&0xFFFFu32.to_le_bytes()); // 长度超界
        let count = AttributeIter::new(&a, 0).count();
        assert_eq!(count, 0);
    }

    #[test]
    fn parses_non_resident_sizes_and_runs() {
        // 构造一条非驻留 $DATA：run 偏移 0x40
        let mut a = vec![0u8; 0x48];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(0x48u32).to_le_bytes());
        a[8] = 1; // non resident
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x28..0x30].copy_from_slice(&(4096u64).to_le_bytes()); // allocated
        a[0x30..0x38].copy_from_slice(&(3000u64).to_le_bytes()); // real
        a[0x38..0x40].copy_from_slice(&(3000u64).to_le_bytes()); // init
        a[0x40] = 0x21;
        a[0x41] = 0x02;
        a[0x42] = 0x0A;
        a[0x43] = 0x00;
        a[0x44] = 0x00; // 结束符

        let attr = Attribute::parse(&a).unwrap();
        assert!(attr.non_resident);
        assert_eq!(attr.non_resident_sizes(), Some((4096, 3000, 3000)));
        let runs = crate::runlist::decode_data_runs(attr.run_list().unwrap());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len_clusters, 2);
        assert_eq!(runs[0].lcn, 10);
    }
}
