//! Linux 卷句柄：**块设备 / 磁盘镜像文件的只读顺序读**。
//!
//! # 与 Windows 版的差别（先说清楚，免得读者以为是漏做）
//!
//! | 维度 | Windows | Linux |
//! |---|---|---|
//! | 卷的标识 | 盘符 `C:` | 挂载点 `/`、`/home` |
//! | 打开方式 | `CreateFileW("\\.\C:")` | `open("/dev/nvme0n1p2", O_RDONLY)` |
//! | 权限 | 管理员令牌 | `euid == 0`（块设备默认 0660 root:disk） |
//! | 任意偏移读 | 必须扇区对齐 | 不要求（内核自行处理） |
//! | `DeviceIoControl` | USN Journal 全套 | **没有对应物** |
//!
//! 最后一行是关键：**USN Journal 是 NTFS 的 Windows 实现特性**，Linux 的
//! `ntfs3` / `ntfs-3g` 都不暴露它。所以即使以 root 打开块设备、`$MFT` 直读
//! 能跑通，增量来源仍然只能是 `inotify`。这不是移植偷懒，是内核能力不同。
//!
//! # 为什么还保留这个类型
//!
//! 1. 让 `core-ntfs` / `core-usn` 的代码**一份源码两个平台都能编译** ——
//!    否则整个 MFT 解析层都得裹 `#[cfg(windows)]`，维护成本远高于这个壳；
//! 2. NTFS 盘挂在 Linux 上（`ntfs3`）时，root 仍可以直读 `$MFT` 做全量扫描
//!    —— 这条路是真实可用的，`Volume::open` 因此**不**直接拒绝；
//! 3. 单测可以拿一个 NTFS 镜像文件当"卷"（普通文件也接受），
//!    于是 MFT 解析在 Linux CI 上也能验证。

use crate::paths;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::{Path, PathBuf};

// ── libc 手写声明（零第三方依赖：不引 libc crate） ──────────────────────────

extern "C" {
    fn geteuid() -> u32;
    /// `struct statvfs` 的布局各 libc 版本略有差异（glibc 2.39 之后多了
    /// `f_type`），**所以这里不声明结构体**：传一块足够大的对齐缓冲，
    /// 再按已知偏移读前 5 个 `unsigned long`。这样既不会因结构体尺寸写错
    /// 而踩坏栈，也不必跟着发行版改布局。
    fn statvfs(path: *const std::os::raw::c_char, buf: *mut std::os::raw::c_void) -> i32;
}

/// `statvfs` 输出缓冲：256 字节远超任何 glibc/musl 的实际尺寸（112 / 96）。
#[repr(C, align(8))]
struct StatVfsBuf {
    raw: [u8; 256],
}

impl StatVfsBuf {
    fn new() -> StatVfsBuf {
        StatVfsBuf { raw: [0u8; 256] }
    }
    fn u64_at(&self, off: usize) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.raw[off..off + 8]);
        u64::from_le_bytes(a)
    }
}

/// 一个挂载点的容量信息：`(块大小, 总块数, 可用块数)`。
pub fn statvfs_of(path: &Path) -> Option<(u64, u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = StatVfsBuf::new();
    // SAFETY: `c` 以 NUL 结尾且存活到调用结束；`buf` 是 256 字节、
    // 对齐到 8 的可写缓冲，远大于任何 libc 的 `struct statvfs`。
    let rc = unsafe { statvfs(c.as_ptr(), &mut buf as *mut StatVfsBuf as *mut std::os::raw::c_void) };
    if rc != 0 {
        return None;
    }
    // 偏移（x86_64 / aarch64 的 LP64 布局一致）：
    //   0x00 f_bsize  0x08 f_frsize  0x10 f_blocks  0x18 f_bfree  0x20 f_bavail
    let frsize = buf.u64_at(0x08);
    let bsize = buf.u64_at(0x00);
    let block = if frsize > 0 { frsize } else { bsize };
    Some((block, buf.u64_at(0x10), buf.u64_at(0x20)))
}

/// `/proc/self/mounts` 的一行。
#[derive(Debug, Clone)]
pub struct MountEntry {
    /// 设备 / 虚拟源（`/dev/nvme0n1p2`、`tmpfs`、`overlay`）
    pub source: String,
    /// 挂载点（已反转义，恒为绝对路径且**不带**尾斜杠，根是 `/`）
    pub mount_point: String,
    /// 文件系统类型（`ext4` / `ntfs3` / `fuseblk` / `tmpfs` …）
    pub fs_type: String,
}

/// 读出当前挂载表。读不到时返回空表（调用方按"没有卷"处理，不 panic）。
pub fn mount_table() -> Vec<MountEntry> {
    let mut out = Vec::new();
    let raw = match std::fs::read_to_string("/proc/self/mounts") {
        Ok(s) => s,
        Err(_) => return out,
    };
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        let (Some(src), Some(mp), Some(fs)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        out.push(MountEntry {
            source: unescape_mount(src),
            mount_point: paths::normalize_root(&unescape_mount(mp)),
            fs_type: fs.to_string(),
        });
    }
    out
}

/// `/proc/mounts` 里的空格/tab/换行写成 `\040` `\011` `\012`（八进制）。
fn unescape_mount(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') as u32 * 64 + (oct[1] - b'0') as u32 * 8 + (oct[2] - b'0') as u32;
                out.push(v as u8);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把用户给的「卷」解析成 `(卷键, 设备路径)`。
///
/// 接受三种输入：
/// * `/dev/nvme0n1p2` —— 直接是设备；
/// * `/home` —— 挂载点，反查设备；
/// * 任意普通文件 —— 当作磁盘镜像（单测与离线分析用）。
///
/// 卷键是**归一后的挂载点**（或镜像文件路径），后续 `Tree::drive` 就存它。
pub fn normalize_drive(input: &str) -> io::Result<(String, String)> {
    let t = input.trim();
    if t.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "卷/挂载点不能为空（例如 / 或 /home 或 /dev/sda2）",
        ));
    }
    let p = PathBuf::from(t);
    let key = paths::normalize_root(&p.to_string_lossy());

    // ① 已经是设备（或镜像文件）→ 键取"它自己被挂载在哪"，找不到就用它自身
    if t.starts_with("/dev/") {
        let table = mount_table();
        let mp = table
            .iter()
            .find(|m| m.source == t)
            .map(|m| m.mount_point.clone())
            .unwrap_or_else(|| key.clone());
        return Ok((mp, t.to_string()));
    }

    // ② 挂载点 → 反查设备
    if let Some(m) = mount_table().into_iter().find(|m| m.mount_point == key) {
        return Ok((key, m.source));
    }

    // ③ 普通文件（镜像）→ 直接用；其余情况保持"键=输入"，让 open 去报真正的错
    Ok((key, t.to_string()))
}

/// 一个已打开的卷（块设备或镜像文件）。
///
/// 句柄只在构造时打开一次，`read_at` 用 `pread` 语义（不移动文件游标），
/// 因此**可以被多线程共享** —— 与 Windows 版 `unsafe impl Send/Sync` 的
/// 理由相同（每次读都带绝对偏移）。
pub struct Volume {
    file: File,
    drive: String,
    device_path: String,
    sector_size: u32,
}

impl Volume {
    /// 以只读方式打开。块设备需要 root；普通文件（镜像）不需要。
    pub fn open(drive_input: &str) -> io::Result<Volume> {
        let (drive, device_path) = normalize_drive(drive_input)?;
        let file = File::open(&device_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "打开 {} 失败: {}（Linux 下直读块设备需要 root；\
                     普通用户请用目录遍历模式：`scan --force-traverse`）",
                    device_path, e
                ),
            )
        })?;

        let md = file.metadata()?;
        let is_blk = md.file_type().is_block_device();
        let is_file = md.file_type().is_file();
        if !is_blk && !is_file {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} 不是块设备或镜像文件（它是目录？）。请给挂载点，或直接扫描该目录",
                    device_path
                ),
            ));
        }

        let mut vol = Volume {
            file,
            drive,
            device_path,
            sector_size: 512,
        };
        // 扇区大小：与 Windows 版**同一处**探测 —— 引导扇区 0x0B 处的
        // 每扇区字节数与文件系统无关（NTFS / FAT 都在这里），只做合法性校验。
        let mut boot = [0u8; 512];
        if vol.read_raw(0, &mut boot).is_ok() {
            let bps = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u32;
            if bps == 512 || bps == 1024 || bps == 2048 || bps == 4096 {
                vol.sector_size = bps;
            }
        }
        Ok(vol)
    }

    #[inline]
    pub fn drive(&self) -> &str {
        &self.drive
    }

    #[inline]
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    #[inline]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// 底层物理读：**不要求**扇区对齐（`pread` 允许任意偏移）。
    /// 内部循环直到读满，短读不当作结束。
    pub fn read_raw(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let n = self.file.read_at(&mut buf[done..], pos)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("读取 {} 在偏移 {} 处返回 0 字节（可能已到卷尾）", self.drive, pos),
                ));
            }
            done += n;
        }
        Ok(())
    }

    /// 安全读：任意偏移、任意长度。Linux 上没有对齐要求，直接等价于 `read_raw`。
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.read_raw(offset, buf)
    }

    /// 分配一个新缓冲并读满。
    pub fn read_exact_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }

    /// `DeviceIoControl` 在 Linux 上没有对应物。
    ///
    /// 明确返回 `Unsupported`，而不是"假装成功返回 0 字节" ——
    /// 后者会让 USN 会话把"没有日志"读成"日志是空的"，进而静默地
    /// 一条事件都不产生。
    pub fn device_io(&self, code: u32, _input: &mut [u8], _output: &mut [u8]) -> io::Result<usize> {
        Err(unsupported_ioctl(code))
    }

    /// 同上，允许调用方把某些错误码当正常结果 —— Linux 上仍然一律报错。
    pub fn device_io_allow(
        &self,
        code: u32,
        _input: &mut [u8],
        _output: &mut [u8],
        _allowed: &[u32],
    ) -> io::Result<Option<usize>> {
        Err(unsupported_ioctl(code))
    }
}

fn unsupported_ioctl(code: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "Linux 不支持 DeviceIoControl（控制码 0x{:08X}）：USN Journal 是 NTFS 的 \
             Windows 实现特性，Linux 上增量监控改用 inotify",
            code
        ),
    )
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("drive", &self.drive)
            .field("device_path", &self.device_path)
            .field("sector_size", &self.sector_size)
            .finish()
    }
}

/// 是否 root。块设备默认 `root:disk 0660`，非 root 打不开 —— 这就是
/// Linux 上"有没有管理员权限"的实际含义（没有 UAC 那种过滤令牌概念）。
pub fn probe_elevated() -> bool {
    // SAFETY: `geteuid` 无参数、无副作用、永远成功。
    unsafe { geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_always_the_root() {
        // 根目录归一后仍是 "/"，绝不能变成 "//"
        assert_eq!(paths::normalize_root("/"), "/");
        assert_eq!(paths::root_prefix("/"), "/");
    }

    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(unescape_mount("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount("/a\\011b"), "/a\tb");
        assert_eq!(unescape_mount("/plain"), "/plain");
        // 不完整的转义原样保留（不能吞字符）
        assert_eq!(unescape_mount("/a\\04"), "/a\\04");
    }

    #[test]
    fn mount_table_is_not_empty_on_linux() {
        let t = mount_table();
        assert!(!t.is_empty(), "读不到 /proc/self/mounts？");
        assert!(t.iter().any(|m| m.mount_point == "/"), "至少要有根挂载点");
        for m in &t {
            assert!(m.mount_point.starts_with('/'), "挂载点必须是绝对路径：{}", m.mount_point);
        }
    }

    /// 镜像文件也当作卷：这是 MFT 解析能在 Linux 上跑单测的前提。
    #[test]
    fn regular_file_is_accepted_as_image() {
        let d = crate::test_dir::TestDir::new("fw-vol-img").unwrap();
        let img = d.path().join("img.bin");
        std::fs::write(&img, vec![0u8; 8192]).unwrap();
        let vol = Volume::open(&img.to_string_lossy()).expect("镜像文件应当能打开");
        assert_eq!(vol.sector_size(), 512);
        let mut b = [0u8; 8];
        vol.read_raw(0, &mut b).unwrap();
        // 越界读要报错，不能返回全 0
        let mut big = vec![0u8; 16];
        assert!(vol.read_raw(8180, &mut big).is_err());
    }

    #[test]
    fn directory_is_rejected_with_a_helpful_message() {
        let d = crate::test_dir::TestDir::new("fw-vol-dir").unwrap();
        let e = Volume::open(&d.path().to_string_lossy()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("不是块设备") || msg.contains("失败"), "msg={msg}");
    }

    #[test]
    fn ioctl_reports_unsupported_not_silence() {
        let d = crate::test_dir::TestDir::new("fw-vol-ioctl").unwrap();
        let img = d.path().join("img.bin");
        std::fs::write(&img, vec![0u8; 4096]).unwrap();
        let vol = Volume::open(&img.to_string_lossy()).unwrap();
        let e = vol
            .device_io(crate::consts::FSCTL_QUERY_USN_JOURNAL, &mut [], &mut [])
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(e.to_string().contains("inotify"), "错误信息要给出替代方案");
    }
}
