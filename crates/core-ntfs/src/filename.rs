//! `$FILE_NAME`（属性 `0x30`）解析与命名空间优选。
//!
//! ## 命名空间优先级（文档 §3.2 去重规则）
//!
//! 一个文件在 NTFS 里可能有多个 `$FILE_NAME` 属性：
//! * `0` POSIX   —— 区分大小写，通常等于 Win32 名
//! * `1` Win32   —— 长文件名
//! * `2` DOS     —— 8.3 短名（如 `PROGRA~1`）
//! * `3` Win32&DOS —— 长短名相同
//!
//! 展示优先级：**Win32 / Win32&DOS > POSIX > DOS**，
//! 这样才不会把 `Program Files` 显示成 `PROGRA~1`。

use crate::bytes::{i64_at, u32_at, u64_at};

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

/// `$FILE_NAME` 段中文件名字符串的固定偏移（相对属性值起点）。
pub const NAME_OFFSET_IN_VALUE: usize = 0x42;

/// `$FILE_NAME` 值区的最小长度（不含名字本身）。
pub const FILE_NAME_MIN_LEN: usize = 0x42;

// $FILE_NAME 的 flags 位
pub const FN_FLAG_READONLY: u32 = 0x0000_0001;
pub const FN_FLAG_HIDDEN: u32 = 0x0000_0002;
pub const FN_FLAG_SYSTEM: u32 = 0x0000_0004;
pub const FN_FLAG_ARCHIVE: u32 = 0x0000_0020;
pub const FN_FLAG_SPARSE: u32 = 0x0000_0200;
pub const FN_FLAG_REPARSE: u32 = 0x0000_0400;
pub const FN_FLAG_COMPRESSED: u32 = 0x0000_0800;
pub const FN_FLAG_ENCRYPTED: u32 = 0x0000_4000;
pub const FN_FLAG_DIRECTORY: u32 = 0x1000_0000;

/// 命名空间的展示优先级得分，越大越优先。
#[inline]
pub fn namespace_rank(ns: u8) -> u8 {
    match ns {
        NS_WIN32 | NS_WIN32_DOS => 3,
        NS_POSIX => 2,
        NS_DOS => 1,
        _ => 0,
    }
}

/// 判断 `candidate` 是否应该替换掉当前已选中的 `current`。
#[inline]
pub fn is_better_namespace(candidate: u8, current: Option<u8>) -> bool {
    match current {
        None => true,
        Some(cur) => namespace_rank(candidate) > namespace_rank(cur),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileNameInfo {
    /// 父目录的完整 FRN（高 16 位是序列号）
    pub parent_frn: u64,
    pub namespace: u8,
    /// 文件名在**属性值**中的偏移（= `NAME_OFFSET_IN_VALUE`）
    pub name_offset_in_value: u16,
    /// 文件名字符数（UTF-16 码元数）
    pub name_chars: u16,
    /// `$FILE_NAME` 里冗余记录的大小。**注意：目录恒为 0**，仅供参考
    pub real_size: u64,
    pub alloc_size: u64,
    /// 修改时间（Unix 秒）
    pub mtime: i64,
    pub flags: u32,
    pub reparse_tag: u32,
}

impl FileNameInfo {
    #[inline]
    pub fn is_reparse(&self) -> bool {
        self.flags & FN_FLAG_REPARSE != 0
    }

    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags & FN_FLAG_COMPRESSED != 0
    }

    #[inline]
    pub fn is_sparse(&self) -> bool {
        self.flags & FN_FLAG_SPARSE != 0
    }

    #[inline]
    pub fn is_encrypted(&self) -> bool {
        self.flags & FN_FLAG_ENCRYPTED != 0
    }
}

/// 解析 `$FILE_NAME` 属性的**值区**（即 `Attribute::resident_value()` 的结果）。
pub fn parse_file_name_value(value: &[u8]) -> Option<FileNameInfo> {
    if value.len() < FILE_NAME_MIN_LEN {
        return None;
    }
    let parent_frn = u64_at(value, 0x00)?;
    let modified = i64_at(value, 0x10)?;
    let alloc_size = u64_at(value, 0x28)?;
    let real_size = u64_at(value, 0x30)?;
    let flags = u32_at(value, 0x38)?;
    let reparse_tag = u32_at(value, 0x3C)?;
    let name_chars = *value.get(0x40)? as u16;
    let namespace = *value.get(0x41)?;

    // 名字区必须完整落在值内
    let name_start = NAME_OFFSET_IN_VALUE;
    let name_end = name_start + name_chars as usize * 2;
    if name_end > value.len() {
        return None;
    }
    if name_chars == 0 {
        return None;
    }

    Some(FileNameInfo {
        parent_frn,
        namespace,
        name_offset_in_value: NAME_OFFSET_IN_VALUE as u16,
        name_chars,
        real_size,
        alloc_size,
        mtime: modified,
        flags,
        reparse_tag,
    })
}

/// FRN 低 48 位 = MFT 记录号；高 16 位 = 序列号。
///
/// 这是文档 §5.3 强调的**核心优化**：FRN 天然就是索引，
/// 不需要 `HashMap<u64, NodeId>`。
#[inline]
pub const fn frn_to_record(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// FRN 的序列号部分。
#[inline]
pub const fn frn_sequence(frn: u64) -> u16 {
    (frn >> 48) as u16
}

/// 由记录号 + 序列号拼出完整 FRN。
#[inline]
pub const fn make_frn(recno: u64, seq: u16) -> u64 {
    ((seq as u64) << 48) | (recno & 0x0000_FFFF_FFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_fn_value(parent: u64, ns: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        v[0x00..0x08].copy_from_slice(&parent.to_le_bytes());
        v[0x10..0x18].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        v[0x28..0x30].copy_from_slice(&4096u64.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&1234u64.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = ns;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_basic_file_name() {
        let v = build_fn_value(0x0005_0000_0000_0005, NS_WIN32, "Program Files");
        let f = parse_file_name_value(&v).unwrap();
        assert_eq!(f.namespace, NS_WIN32);
        assert_eq!(f.name_chars, 13);
        assert_eq!(f.parent_frn, 0x0005_0000_0000_0005);
        assert_eq!(f.real_size, 1234);
        assert_eq!(f.mtime, 1_700_000_000);
    }

    #[test]
    fn rejects_truncated_name() {
        let mut v = build_fn_value(5, NS_WIN32, "abcdef");
        v[0x40] = 200; // 声称 200 字符但缓冲不够
        assert!(parse_file_name_value(&v).is_none());
    }

    #[test]
    fn rejects_empty_name() {
        let mut v = build_fn_value(5, NS_WIN32, "a");
        v[0x40] = 0;
        assert!(parse_file_name_value(&v).is_none());
    }

    #[test]
    fn namespace_preference_order() {
        // Win32 应该压过 DOS
        assert!(is_better_namespace(NS_WIN32, Some(NS_DOS)));
        // DOS 不应该压过 POSIX
        assert!(!is_better_namespace(NS_DOS, Some(NS_POSIX)));
        // Win32&DOS 等价于 Win32
        assert_eq!(namespace_rank(NS_WIN32), namespace_rank(NS_WIN32_DOS));
        // 第一个总是被接受
        assert!(is_better_namespace(NS_DOS, None));
        // POSIX 应压过 DOS
        assert!(is_better_namespace(NS_POSIX, Some(NS_DOS)));
    }

    #[test]
    fn frn_split_and_join() {
        let frn = make_frn(12345, 7);
        assert_eq!(frn_to_record(frn), 12345);
        assert_eq!(frn_sequence(frn), 7);
        // 高位不应污染记录号
        assert_eq!(frn_to_record(0xFFFF_0000_0000_0001), 1);
    }
}
