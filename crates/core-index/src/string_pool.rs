//! 字符串池：所有文件名拼进一个大 `Vec<u8>`，节点只存 `u32` 偏移。
//!
//! 文档 §5.5 关键优化点 1。以 NUL 分隔（文件名本身不可能含 NUL）。
//! 2,000,000 × 平均 24 B ≈ 48 MB，相比每节点一个 `String`
//! （24 B 结构 + 堆分配 + 分配器元数据）省下一半以上，且**无 200 万次小分配**。
//!
//! 偏移 0 恒为空串，这样 `Node::EMPTY` 的 `name = 0` 天然指向 `""`。

use core_io::util::utf16le_bytes_to_utf8;

#[derive(Debug, Clone, Default)]
pub struct StringPool {
    buf: Vec<u8>,
}

impl StringPool {
    pub fn new() -> StringPool {
        let mut buf = Vec::with_capacity(1024);
        buf.push(0); // 保留空串，占据偏移 0
        StringPool { buf }
    }

    /// 预分配，避免大扫描时反复 realloc。
    pub fn with_capacity(bytes: usize) -> StringPool {
        let mut buf = Vec::with_capacity(bytes.max(16));
        buf.push(0);
        StringPool { buf }
    }

    /// 把一段 UTF-16LE 字节解析成 UTF-8 并写入池，返回偏移。
    pub fn intern_utf16le(&mut self, raw_utf16le: &[u8]) -> u32 {
        if raw_utf16le.is_empty() {
            return 0;
        }
        let (off, _len) = utf16le_bytes_to_utf8(&mut self.buf, raw_utf16le);
        self.buf.push(0);
        off
    }

    /// 直接写入一个 UTF-8 字符串（测试与人工构造用）。
    pub fn intern_str(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        off
    }

    /// 取回字符串。偏移越界或非法 UTF-8 时返回 `""`（绝不 panic）。
    #[inline]
    pub fn get(&self, off: u32) -> &str {
        let start = off as usize;
        if start == 0 || start >= self.buf.len() {
            return "";
        }
        let rest = &self.buf[start..];
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        core::str::from_utf8(&rest[..end]).unwrap_or("")
    }

    /// 池占用字节数。
    #[inline]
    pub fn bytes_used(&self) -> usize {
        self.buf.len()
    }

    /// 条目数（用于统计）。
    pub fn count(&self) -> usize {
        self.buf.iter().filter(|&&b| b == 0).count().saturating_sub(1)
    }

    pub fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
    }

    #[test]
    fn empty_pool_returns_empty_string() {
        let p = StringPool::new();
        assert_eq!(p.get(0), "");
        assert_eq!(p.get(999), "");
    }

    #[test]
    fn interns_and_reads_back() {
        let mut p = StringPool::new();
        let a = p.intern_utf16le(&utf16("hello"));
        let b = p.intern_utf16le(&utf16("world.txt"));
        let c = p.intern_utf16le(&utf16("测试目录"));
        assert_eq!(p.get(a), "hello");
        assert_eq!(p.get(b), "world.txt");
        assert_eq!(p.get(c), "测试目录");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_input_maps_to_offset_zero() {
        let mut p = StringPool::new();
        assert_eq!(p.intern_utf16le(&[]), 0);
        assert_eq!(p.get(0), "");
    }

    #[test]
    fn handles_duplicate_names_without_corruption() {
        let mut p = StringPool::new();
        for _ in 0..100 {
            p.intern_utf16le(&utf16("same"));
        }
        assert_eq!(p.count(), 100);
        // 每个偏移都要能正确读回
        let mut off = 1u32;
        for _ in 0..100 {
            assert_eq!(p.get(off), "same");
            off += 5; // 4 字节 + NUL
        }
    }

    #[test]
    fn long_name_and_special_chars() {
        let mut p = StringPool::new();
        let name = "a-very-long-file-name-with-中文-and-emoji-😀.dat";
        let off = p.intern_utf16le(&utf16(name));
        assert_eq!(p.get(off), name);
    }

    #[test]
    fn offset_never_points_into_middle_of_string() {
        let mut p = StringPool::new();
        let off = p.intern_utf16le(&utf16("abcdef"));
        // 从前一个偏移 + 1 读取会读到一个较短的后缀，但不会 panic
        let _ = p.get(off + 1);
        assert_eq!(p.get(off), "abcdef");
    }
}
