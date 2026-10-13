//! 逻辑卷枚举与容量信息（对应功能 F1「盘符选择」）。

use crate::util::{from_utf16_until_nul, to_wide};
use crate::winapi;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// `"C:"`
    pub drive: String,
    /// `"C:\"`
    pub root: String,
    pub label: String,
    /// `"NTFS"` / `"exFAT"` / `"FAT32"` / ...
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
    pub fn supports_mft(&self) -> bool {
        self.drive_type == winapi::DRIVE_FIXED && self.fs_name.eq_ignore_ascii_case("NTFS")
    }

    pub fn type_label(&self) -> &'static str {
        match self.drive_type {
            winapi::DRIVE_REMOVABLE => "可移动",
            winapi::DRIVE_FIXED => "本地固定",
            winapi::DRIVE_REMOTE => "网络",
            winapi::DRIVE_CDROM => "光驱",
            winapi::DRIVE_RAMDISK => "内存盘",
            _ => "未知",
        }
    }

    /// 降级模式下给用户看的提示（对应文档 R2 / R12）。
    pub fn mode_hint(&self) -> &'static str {
        if self.supports_mft() {
            "MFT 直读（极速）"
        } else if self.drive_type == winapi::DRIVE_REMOTE {
            "网络盘 → 目录遍历（降级）"
        } else if self.drive_type != winapi::DRIVE_FIXED {
            "非固定盘 → 目录遍历（降级）"
        } else {
            "非 NTFS → 目录遍历（降级）"
        }
    }
}

/// 枚举本机全部逻辑卷（含卷标、文件系统、容量）。
pub fn list_volumes() -> Vec<VolumeInfo> {
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

fn query_one(root: &str) -> Option<VolumeInfo> {
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
        winapi::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut free,
        )
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

/// 卷的簇大小（字节）。
///
/// 目录遍历降级模式需要它来把「逻辑大小」换算成「占用大小」
/// （文档 §3.2 要求两个值都要显示）。
pub fn cluster_size(root: &str) -> Option<u64> {
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

/// 当前可执行文件所在目录（用于定位持久化目录的兜底）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}
