//! 小端字节读取工具。所有函数都做边界检查，越界返回 `None`。
//!
//! MFT/USN 解析面对的是**不可信数据**（撕裂写、损坏记录、恶意构造的镜像），
//! 因此这里坚持「宁返回 None，不 panic」的原则。

#[inline]
pub fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

#[inline]
pub fn i8_at(b: &[u8], off: usize) -> Option<i8> {
    b.get(off).map(|&v| v as i8)
}

#[inline]
pub fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
pub fn i16_at(b: &[u8], off: usize) -> Option<i16> {
    u16_at(b, off).map(|v| v as i16)
}

#[inline]
pub fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
pub fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

#[inline]
pub fn i64_at(b: &[u8], off: usize) -> Option<i64> {
    u64_at(b, off).map(|v| v as i64)
}

/// u128（USN_RECORD_V3 的 128 位 FRN 用）。
#[inline]
pub fn u128_at(b: &[u8], off: usize) -> Option<u128> {
    let s = b.get(off..off + 16)?;
    let mut a = [0u8; 16];
    a.copy_from_slice(s);
    Some(u128::from_le_bytes(a))
}

/// 从 `[off, off+size)` 取一个小端无符号整数（size ≤ 8）。
/// Data Runs 的长度/偏移字段是变长的，需要这个。
#[inline]
pub fn le_uint(b: &[u8], off: usize, size: usize) -> Option<u64> {
    if size == 0 || size > 8 {
        return None;
    }
    let s = b.get(off..off + size)?;
    let mut v: u64 = 0;
    for (i, &byte) in s.iter().enumerate() {
        v |= (byte as u64) << (8 * i);
    }
    Some(v)
}

/// 从 `[off, off+size)` 取一个**带符号**小端整数（Data Runs 的 LCN 偏移是相对值）。
#[inline]
pub fn le_sint(b: &[u8], off: usize, size: usize) -> Option<i64> {
    if size == 0 || size > 8 {
        return None;
    }
    let raw = le_uint(b, off, size)?;
    let bits = size * 8;
    if bits < 64 {
        let sign_bit = 1u64 << (bits - 1);
        if raw & sign_bit != 0 {
            // 符号扩展
            return Some((raw | !((1u64 << bits) - 1)) as i64);
        }
    }
    Some(raw as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_checked() {
        let b = [1u8, 2, 3];
        assert_eq!(u16_at(&b, 0), Some(0x0201));
        assert_eq!(u16_at(&b, 2), None);
        assert_eq!(u32_at(&b, 0), None);
        assert_eq!(u8_at(&b, 3), None);
    }

    #[test]
    fn sign_extension_works() {
        // 0xFE 单字节 → -2
        assert_eq!(le_sint(&[0xFE], 0, 1), Some(-2));
        // 0x0A → +10
        assert_eq!(le_sint(&[0x0A], 0, 1), Some(10));
        // 两字节 0xFF 0x7F → 0x7FFF 正数
        assert_eq!(le_sint(&[0xFF, 0x7F], 0, 2), Some(0x7FFF));
        // 两字节 0x00 0x80 → -32768
        assert_eq!(le_sint(&[0x00, 0x80], 0, 2), Some(-32768));
    }
}
