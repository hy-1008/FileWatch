//! Update Sequence Array（USA）fixup —— 防撕裂写校验。
//!
//! NTFS 会在每个扇区的最后 2 字节写入一个「更新序列号 USN」，
//! 真实数据被暂存到记录头部的 USA 数组里。这样做是为了保证
//! **多扇区记录的原子更新**：如果掉电导致只写了部分扇区，
//! 校验就会失败，从而识别出损坏记录（对应文档 R1）。
//!
//! 修复是**就地**进行的：把 USA 里的原始 2 字节写回各扇区末尾。

use crate::bytes::u16_at;

/// 对一条 MFT 记录就地做 fixup。
///
/// * `rec` 必须是**完整**的一条记录（长度 = 记录大小）
/// * `sector_size` 通常 512
///
/// 返回 `true` 表示校验通过并已完成修复；`false` 表示记录损坏，
/// **调用方必须丢弃这条记录**（不要使用其内容）。
pub fn apply_fixup(rec: &mut [u8], sector_size: usize) -> bool {
    if sector_size == 0 || rec.len() < sector_size {
        return false;
    }
    let usa_off = match u16_at(rec, 0x04) {
        Some(v) => v as usize,
        None => return false,
    };
    let usa_cnt = match u16_at(rec, 0x06) {
        Some(v) => v as usize,
        None => return false,
    };

    // 正常情形：usa_cnt = 记录中扇区数 + 1
    let expected = rec.len() / sector_size + 1;
    if usa_cnt != expected {
        // 极少数实现会不严格，这里放宽为「至少要有 USN 项且不越界」
        if usa_cnt == 0 || usa_cnt > expected {
            return false;
        }
    }
    // USA 数组整体必须落在记录内
    if usa_off + usa_cnt * 2 > rec.len() {
        return false;
    }

    let usn = match u16_at(rec, usa_off) {
        Some(v) => v,
        None => return false,
    };

    // 先校验全部扇区，再统一写回 —— 避免「改了一半才发现损坏」
    for i in 1..usa_cnt {
        let end = i * sector_size - 2; // 该扇区最后 2 字节
        match u16_at(rec, end) {
            Some(v) if v == usn => {}
            _ => return false, // 撕裂写 / 损坏
        }
    }
    for i in 1..usa_cnt {
        let end = i * sector_size - 2;
        let src = usa_off + i * 2;
        let b0 = rec[src];
        let b1 = rec[src + 1];
        rec[end] = b0;
        rec[end + 1] = b1;
    }
    true
}

/// 不做写回，只做校验（用于「这条记录是否可安全解析」的快速判断）。
pub fn verify_fixup(rec: &[u8], sector_size: usize) -> bool {
    if sector_size == 0 || rec.len() < sector_size {
        return false;
    }
    let (usa_off, usa_cnt) = match (u16_at(rec, 0x04), u16_at(rec, 0x06)) {
        (Some(a), Some(b)) => (a as usize, b as usize),
        _ => return false,
    };
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() {
        return false;
    }
    let usn = match u16_at(rec, usa_off) {
        Some(v) => v,
        None => return false,
    };
    for i in 1..usa_cnt {
        match u16_at(rec, i * sector_size - 2) {
            Some(v) if v == usn => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条 1024 字节记录：USA 在 0x30，2 个扇区 → usa_cnt = 3
    fn make_record(usn: u16, tail: [u16; 2]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        r[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        r[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
        // USA[1], USA[2] 存真实数据
        r[0x32..0x34].copy_from_slice(&tail[0].to_le_bytes());
        r[0x34..0x36].copy_from_slice(&tail[1].to_le_bytes());
        // 各扇区末尾放 USN
        r[510..512].copy_from_slice(&usn.to_le_bytes());
        r[1022..1024].copy_from_slice(&usn.to_le_bytes());
        r
    }

    #[test]
    fn fixup_restores_original_bytes() {
        let mut r = make_record(0xABCD, [0x1111, 0x2222]);
        assert!(apply_fixup(&mut r, 512));
        assert_eq!(u16_at(&r, 510), Some(0x1111));
        assert_eq!(u16_at(&r, 1022), Some(0x2222));
    }

    #[test]
    fn detects_torn_write() {
        let mut r = make_record(0xABCD, [0x1111, 0x2222]);
        // 第二个扇区末尾被写坏
        r[1022..1024].copy_from_slice(&0x9999u16.to_le_bytes());
        assert!(!apply_fixup(&mut r, 512));
        assert!(!verify_fixup(&r, 512));
    }

    #[test]
    fn rejects_out_of_bounds_usa() {
        let mut r = make_record(1, [1, 2]);
        r[0x04..0x06].copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert!(!apply_fixup(&mut r, 512));
    }
}
