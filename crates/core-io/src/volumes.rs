//! 逻辑卷枚举与容量信息（对应功能 F1「盘符选择」）。
//!
//! 两个平台的"卷"含义不同，但对外结构相同：
//!
//! | 字段 | Windows | Linux |
//! |---|---|---|
//! | `drive` | `"C:"` | 挂载点 `"/"` / `"/home"` |
//! | `root` | `"C:\"` | 挂载点带尾斜杠 `"/"` / `"/home/"` |
//! | `label` | 卷标 | 设备路径（`/dev/nvme0n1p2`） |
//! | `fs_name` | `"NTFS"` / `"exFAT"` | `"ext4"` / `"ntfs3"` / `"fuseblk"` |
//! | 容量 | `GetDiskFreeSpaceExW` | `statvfs` |
//!
//! `VolumeInfo` 本身与 `type_label` / `mode_hint` 是**共用的**：界面与 CLI
//! 拿到的字段形状一致，判断"能不能走快速路径"的口径也只有一处。

use crate::consts::*;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Windows `"C:"`；Linux 挂载点 `"/"` / `"/home"`
    pub drive: String,
    /// 带尾分隔符的根 `"C:\"` / `"/"`
    pub root: String,
    /// 卷标（Windows）/ 设备路径（Linux）
    pub label: String,
    /// `"NTFS"` / `"ext4"` / `"fuseblk"` ...
    pub fs_name: String,
    pub drive_type: u32,
    pub serial: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl VolumeInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    pub fn used_ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.used_bytes() as f64 / self.total_bytes as f64
        }
    }

    /// 只有本地固定盘的 NTFS 才支持 MFT 直读。
    ///
    /// **Linux 恒为假**：`$MFT` 直读本身在 Linux 上是可能的（root + 块设备），
    /// 但 `$MFT` 之后的增量来源 USN Journal 没有对应物，于是"快速路径"只能
    /// 做一次性扫描 —— 那还不如老老实实走目录遍历，功能完整、行为可预期。
    pub fn supports_mft(&self) -> bool {
        cfg!(windows) && self.drive_type == DRIVE_FIXED && self.fs_name.eq_ignore_ascii_case("NTFS")
    }

    pub fn type_label(&self) -> &'static str {
        match self.drive_type {
            DRIVE_REMOVABLE => "可移动",
            DRIVE_FIXED => "本地固定",
            DRIVE_REMOTE => "网络",
            DRIVE_CDROM => "光驱",
            DRIVE_RAMDISK => "内存盘",
            _ => "未知",
        }
    }

    /// 降级模式下给用户看的提示（对应文档 R2 / R12）。
    pub fn mode_hint(&self) -> &'static str {
        if self.supports_mft() {
            return "MFT 直读（极速）";
        }
        if self.drive_type == DRIVE_REMOTE {
            return "网络盘 → 目录遍历（降级）";
        }
        #[cfg(not(windows))]
        {
            // Linux 上不存在"没有管理员权限"这一维度（只有 root 能开块设备），
            // 所以提示直说后端，不要暗示"降级是因为权限不足"。
            "目录遍历 + inotify（Linux 后端）"
        }
        #[cfg(windows)]
        {
            if self.drive_type != DRIVE_FIXED {
                "非固定盘 → 目录遍历（降级）"
            } else {
                "非 NTFS → 目录遍历（降级）"
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Windows
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
pub fn list_volumes() -> Vec<VolumeInfo> {
    use crate::winapi;

    // 第一次调用传 0 拿所需缓冲长度
    let need = unsafe { winapi::GetLogicalDriveStringsW(0, core::ptr::null_mut()) };
    if need == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; need as usize + 1];
    let got = unsafe { winapi::GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) };
    if got == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    // 缓冲形如 "C:\\0D:\\0\0"
    let mut cur: Vec<u16> = Vec::new();
    for &c in buf.iter().take(got as usize + 1) {
        if c == 0 {
            if cur.is_empty() {
                break;
            }
            let root = String::from_utf16_lossy(&cur);
            cur.clear();
            if let Some(info) = query_one(&root) {
                out.push(info);
            }
        } else {
            cur.push(c);
        }
    }
    out
}

#[cfg(windows)]
fn query_one(root: &str) -> Option<VolumeInfo> {
    use crate::util::{from_utf16_until_nul, to_wide};
    use crate::winapi;

    let wide = to_wide(root);
    let drive_type = unsafe { winapi::GetDriveTypeW(wide.as_ptr()) };

    // 卷标 + 文件系统名
    let mut label = vec![0u16; 261];
    let mut fs = vec![0u16; 261];
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let ok = unsafe {
        winapi::GetVolumeInformationW(
            wide.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    let (label_s, fs_s) = if ok != 0 {
        (from_utf16_until_nul(&label), from_utf16_until_nul(&fs))
    } else {
        // 光驱空载 / 无权限时拿不到，不影响枚举
        (String::new(), String::new())
    };

    // 容量
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok_cap = unsafe {
        winapi::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_to_caller, &mut total, &mut free)
    };
    if ok_cap == 0 {
        total = 0;
        free = 0;
    }

    let drive = root.trim_end_matches('\\').to_string();
    Some(VolumeInfo {
        drive,
        root: root.to_string(),
        label: label_s,
        fs_name: fs_s,
        drive_type,
        serial,
        total_bytes: total,
        free_bytes: free,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Linux
// ─────────────────────────────────────────────────────────────────────────────

/// 不当作"卷"展示的伪文件系统。
///
/// 判据是"用户**不会**去这里面找自己的文件"：`proc`/`sysfs`/`cgroup` 之类
/// 是内核的镜面，`devpts`/`mqueue` 是 IPC，`squashfs` 是 snap 只读镜像。
/// 把它们列出来只会让卷列表变成一屏没人看的噪音。
#[cfg(not(windows))]
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "securityfs",
    "debugfs",
    "tracefs",
    "pstore",
    "bpf",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "nsfs",
    "efivarfs",
    "rpc_pipefs",
    "binfmt_misc",
    "selinuxfs",
    "ramfs",
    "squashfs",
    "fuse.portal",
];

#[cfg(not(windows))]
pub fn list_volumes() -> Vec<VolumeInfo> {
    use std::collections::HashSet;

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for m in crate::volume::mount_table() {
        if PSEUDO_FS.contains(&m.fs_type.as_str()) {
            continue;
        }
        // 同一个挂载点可能被 bind-mount 多次
        if !seen.insert(m.mount_point.clone()) {
            continue;
        }
        let (total, free) = match crate::volume::statvfs_of(std::path::Path::new(&m.mount_point)) {
            Some((block, blocks, avail)) => {
                (block.saturating_mul(blocks), block.saturating_mul(avail))
            }
            None => (0, 0),
        };
        let drive_type = linux_drive_type(&m.source);
        out.push(VolumeInfo {
            root: crate::paths::root_prefix(&m.mount_point),
            drive: m.mount_point,
            label: m.source,
            fs_name: m.fs_type,
            drive_type,
            serial: 0,
            total_bytes: total,
            free_bytes: free,
        });
    }
    // 稳定的展示顺序：根打头，其余按挂载点字典序
    out.sort_by(|a, b| {
        let ka = (a.drive != "/", a.drive.clone());
        let kb = (b.drive != "/", b.drive.clone());
        ka.cmp(&kb)
    });
    out
}

#[cfg(not(windows))]
fn linux_drive_type(source: &str) -> u32 {
    if source.starts_with("//") || source.contains(":/") {
        return DRIVE_REMOTE; // cifs / nfs 写成 //host/share 或 host:/export
    }
    if source.contains("/dev/sr") || source.contains("/dev/cdrom") {
        return DRIVE_CDROM;
    }
    if source.contains("/dev/loop") {
        return DRIVE_RAMDISK;
    }
    if source.starts_with("/dev/") {
        // 可移动介质的判据是 /sys/block/<disk>/removable，需要多读一次 sysfs。
        // 这里不做：卷列表只用于展示与选择，把 U 盘说成"本地固定"不会让
        // 任何逻辑出错（真正影响行为的 `supports_mft` 在 Linux 上恒假）。
        return DRIVE_FIXED;
    }
    DRIVE_UNKNOWN // overlay / tmpfs 等虚拟文件系统
}

// ─────────────────────────────────────────────────────────────────────────────
// 共用：簇大小与可执行文件目录
// ─────────────────────────────────────────────────────────────────────────────

/// 卷的簇大小（字节）。
///
/// 目录遍历降级模式需要它来把「逻辑大小」换算成「占用大小」
/// （文档 §3.2 要求两个值都要显示）。
///
/// * Windows：`GetDiskFreeSpaceW` 的「每簇扇区数 × 每扇区字节数」；
/// * Linux：`statvfs` 的 `f_frsize`（文件系统的基本分配单元，相当于簇）。
#[cfg(windows)]
pub fn cluster_size(root: &str) -> Option<u64> {
    use crate::util::to_wide;
    use crate::winapi;

    let mut root = root.to_string();
    if !root.ends_with('\\') {
        root.push('\\');
    }
    let wide = to_wide(&root);
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    let ok = unsafe {
        winapi::GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 || sectors_per_cluster == 0 || bytes_per_sector == 0 {
        return None;
    }
    Some(sectors_per_cluster as u64 * bytes_per_sector as u64)
}

#[cfg(not(windows))]
pub fn cluster_size(root: &str) -> Option<u64> {
    let p = std::path::Path::new(root);
    let (block, _, _) = crate::volume::statvfs_of(p)?;
    if block == 0 {
        None
    } else {
        Some(block)
    }
}

/// 当前可执行文件所在目录（用于定位持久化目录的兜底）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(drive: &str, fs: &str, dtype: u32) -> VolumeInfo {
        VolumeInfo {
            drive: drive.to_string(),
            root: crate::paths::root_prefix(drive),
            label: String::new(),
            fs_name: fs.to_string(),
            drive_type: dtype,
            serial: 0,
            total_bytes: 1000,
            free_bytes: 400,
        }
    }

    #[test]
    fn ratios_and_hints() {
        let v = info(crate::paths::DEFAULT_VOLUME, "NTFS", DRIVE_FIXED);
        assert_eq!(v.used_bytes(), 600);
        assert!((v.used_ratio() - 0.6).abs() < 1e-9);
        assert!(!v.mode_hint().is_empty());
        assert_eq!(info("Z:", "NTFS", DRIVE_FIXED).total_bytes, 1000);
    }

    /// Linux 上 `supports_mft` 必须恒假：否则界面会提示"极速"，用户点进去
    /// 却因为 USN Journal 不存在而拿到一个空增量。
    #[cfg(not(windows))]
    #[test]
    fn mft_is_never_advertised_on_linux() {
        let v = info("/", "ntfs3", DRIVE_FIXED);
        assert!(!v.supports_mft());
        assert!(v.mode_hint().contains("inotify"), "{}", v.mode_hint());
    }

    #[test]
    fn root_is_consistent_with_volume_key() {
        let v = info(crate::paths::DEFAULT_VOLUME, "any", DRIVE_FIXED);
        assert!(v.root.ends_with(crate::paths::SEP));
    }

    #[test]
    fn listing_is_stable() {
        // 同一个调用结果里的键必须唯一（否则界面会画出两个"同一个盘"）
        let vols = list_volumes();
        let mut keys: Vec<&str> = vols.iter().map(|v| v.drive.as_str()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "卷键重复");
        if cfg!(not(windows)) {
            assert!(!vols.is_empty(), "Linux 上至少要有根挂载点");
        }
    }
}
