//! 宽字符与字符串工具（不引入任何外部 crate）。

/// UTF-8 `&str` → 以 NUL 结尾的 UTF-16 缓冲（供 `*W` API 使用）。
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 定长 / 不定长 UTF-16 切片 → `String`（非法代理对替换为 U+FFFD）。
pub fn from_utf16_lossy(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf)
}

/// UTF-16 切片 → `String`（遇到 NUL 停止）。
pub fn from_utf16_until_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 把 UTF-16LE 字节切片解码为 UTF-8 字节，直接追加进 `out`，返回写入的起始偏移与长度。
///
/// 这是 MFT `$FILE_NAME` 与 USN `FileName` 的共用路径：
/// 关键在于**不分配中间 `String`**，直接扩写字符串池。
pub fn utf16le_bytes_to_utf8(out: &mut Vec<u8>, raw: &[u8]) -> (u32, u32) {
    let start = out.len() as u32;
    let n = raw.len() / 2;

    // 常见情形：纯 ASCII 文件名（无代理对），走零分配快速路径。
    let mut all_ascii = true;
    for i in 0..n {
        let c = raw[i * 2];
        let hi = raw[i * 2 + 1];
        if hi != 0 || c >= 0x80 {
            all_ascii = false;
            break;
        }
    }
    if all_ascii {
        for i in 0..n {
            out.push(raw[i * 2]);
        }
        return (start, (out.len() as u32) - start);
    }

    // 慢路径：完整 UTF-16 解码（含代理对）。
    let units: Vec<u16> = (0..n)
        .map(|i| u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]))
        .collect();
    let s = String::from_utf16_lossy(&units);
    out.extend_from_slice(s.as_bytes());
    (start, (out.len() as u32) - start)
}

/// 人类可读的字节数（与资源管理器口径一致的 1024 进制）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

/// 取小写扩展名（不含点）。无扩展名返回空串。
pub fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(p) if p > 0 && p + 1 < name.len() => name[p + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 冒泡出当前进程是否具备管理员权限的最可靠判据：
/// 直接尝试打开卷设备。成功 = 有权限。
pub fn is_elevated_hint() -> bool {
    crate::volume::Volume::open("C:").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_works() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(1024 * 1024), "1.00 MB");
    }

    #[test]
    fn extension_works() {
        assert_eq!(extension_of("a.txt"), "txt");
        assert_eq!(extension_of("A.TXT"), "txt");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
    }

    #[test]
    fn utf16_fast_path() {
        let mut out = Vec::new();
        let raw: Vec<u8> = "hello.txt"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let (off, len) = utf16le_bytes_to_utf8(&mut out, &raw);
        assert_eq!(off, 0);
        assert_eq!(len, 9);
        assert_eq!(&out[..], b"hello.txt");
    }

    #[test]
    fn utf16_slow_path_cjk() {
        let mut out = Vec::new();
        let raw: Vec<u8> = "测试文件"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let (_, _) = utf16le_bytes_to_utf8(&mut out, &raw);
        assert_eq!(String::from_utf8(out).unwrap(), "测试文件");
    }
}
