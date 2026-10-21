//! 元数据口径：把两个平台的 `Metadata` 读成**同一组字段**。
//!
//! # 为什么需要
//!
//! 目录遍历、目录监视、重复文件查找都要问同一组问题：
//!
//! * 这是目录吗？是符号链接 / 重解析点吗？
//! * 修改时间是几秒（Unix 秒）？
//! * 它实际占了多少磁盘？
//! * 链接数是多少（硬链接去重）？
//!
//! Windows 上这些从 `std::os::windows::fs::MetadataExt` 取
//! （`file_attributes` / `last_write_time`），Linux 上从
//! `std::os::unix::fs::MetadataExt` 取（`mode` / `mtime` / `blocks` / `nlink`）。
//! 直接把 `use std::os::windows::fs::MetadataExt;` 留在业务代码里，
//! 移植时每一处都要改，而且**改漏了不会报错**（另一个平台编译不过会报，
//! 但同一份逻辑在两边逐渐分叉不会）。
//!
//! 所以这里给出唯一的映射，业务代码只调这几个函数。
//!
//! # 两个刻意的口径决定
//!
//! 1. **属性位是"合成"的。** Linux 没有 `FILE_ATTRIBUTE_*`，但索引树里存的就是
//!    这 32 位（快照格式、`Node::flags` 都依赖它）。这里按"目录 / 普通文件 /
//!    符号链接"三个事实合成同名位，取值与 Win32 一致 —— 于是上层的判断
//!    `attrs & FILE_ATTRIBUTE_DIRECTORY != 0` 两边通用。
//! 2. **"占用"优先用真实块数。** Linux 的 `st_blocks` 是内核给出的实际分配
//!    （稀疏文件会小于逻辑大小，这正是我们要如实呈现的）；拿不到（0）时
//!    才退回"按簇向上取整"的估算 —— 与 Windows 的算法一致。

use std::fs::Metadata;

use crate::util::align_up;

/// 合成 / 取真实属性位。返回值与 Win32 的 `dwFileAttributes` 同构。
pub fn attributes(md: &Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let ft = md.file_type();
        let mut a = 0u32;
        if ft.is_dir() {
            a |= crate::consts::FILE_ATTRIBUTE_DIRECTORY;
        }
        if ft.is_symlink() {
            a |= crate::consts::FILE_ATTRIBUTE_REPARSE_POINT;
        }
        // 普通文件在 Windows 上恒带 ARCHIVE（"需要备份"，新建文件默认置位）。
        // 这里补上它，是为了让「不是目录也不是链接」在两边都只表现为 ARCHIVE，
        // 而不是 Windows 有、Linux 零 —— 那样 `attrs & FILE_ATTRIBUTE_*`
        // 这类判据在跨平台时就不可移植了。
        if !ft.is_dir() && !ft.is_symlink() {
            a |= crate::consts::FILE_ATTRIBUTE_ARCHIVE;
        }
        a
    }
}

/// 是不是符号链接 / 重解析点（`skip_reparse` 用它防无限递归）。
pub fn is_reparse(md: &Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    attributes(md) & crate::consts::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// 修改时间（Unix 秒，UTC）。取不到时返回 0（与引擎的"无意义时间"约定一致）。
pub fn mtime_unix(md: &Metadata) -> i64 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        crate::util::filetime_to_unix(md.last_write_time() as i64)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        md.mtime()
    }
}

/// 创建时间（Unix 秒）。
///
/// Windows 有真正的"创建时间"；Linux 的 `st_ctime` 是 **inode 变更时间**
/// （改权限、改硬链接数都会刷新它），语义不同。所以这里返回 `None` 而不是
/// 硬凑一个值 —— 快照里的"创建时间"列在 Linux 上会显示为 `-`，
/// 这是**如实**的做法，把 ctime 当创建时间显示会静默撒谎。
pub fn ctime_unix(md: &Metadata) -> Option<i64> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let t = crate::util::filetime_to_unix(md.creation_time() as i64);
        if t > 0 {
            Some(t)
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        None
    }
}

/// 硬链接数。Windows 上取不到（`number_of_links` 仍是 unstable）→ 返回 0，
/// 表示"未知"，上层据此不做硬链接去重。
pub fn nlink(md: &Metadata) -> u64 {
    #[cfg(windows)]
    {
        let _ = md;
        0
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        md.nlink()
    }
}

/// 实际占用字节数。
///
/// * Linux：`st_blocks * 512`（内核口径，稀疏文件会小于逻辑大小）；
///   为 0（某些文件系统不填）时退回按簇对齐估算。
/// * Windows：没有等价字段（`$STANDARD_INFORMATION` 的 `AllocatedSize`
///   要 MFT 直读才有），退回按簇对齐估算。
pub fn allocated(md: &Metadata, logical: u64, cluster: u64) -> u64 {
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        let b = md.blocks().saturating_mul(512);
        if b > 0 {
            return b;
        }
    }
    #[cfg(windows)]
    {
        let _ = md;
    }
    align_up(logical, cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_flag_is_synthesized_consistently() {
        let d = crate::test_dir::TestDir::new("fw-fsmeta-dir").unwrap();
        let md = std::fs::metadata(d.path()).unwrap();
        assert!(md.is_dir());
        assert_ne!(
            attributes(&md) & crate::consts::FILE_ATTRIBUTE_DIRECTORY,
            0,
            "目录必须带 DIRECTORY 位"
        );
        assert_eq!(
            attributes(&md) & crate::consts::FILE_ATTRIBUTE_ARCHIVE,
            0,
            "目录不该带 ARCHIVE 位"
        );
    }

    #[test]
    fn regular_file_is_archive_not_directory() {
        let d = crate::test_dir::TestDir::new("fw-fsmeta-file").unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        assert_eq!(attributes(&md) & crate::consts::FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_ne!(attributes(&md) & crate::consts::FILE_ATTRIBUTE_ARCHIVE, 0);
        assert!(!is_reparse(&md));
        assert!(mtime_unix(&md) > 1_577_836_800, "修改时间看起来不像 2020 之后");
    }

    /// 稀疏 / 普通文件都必须满足「占用 ≥ 逻辑大小」以外的常识：
    /// 空文件为 0，小文件至少一个块。
    #[test]
    fn allocation_is_never_negative_and_covers_small_files() {
        let d = crate::test_dir::TestDir::new("fw-fsmeta-alloc").unwrap();
        let empty = d.path().join("e.bin");
        std::fs::write(&empty, b"").unwrap();
        let md = std::fs::metadata(&empty).unwrap();
        assert_eq!(allocated(&md, 0, 4096), 0, "空文件不该占块");

        let small = d.path().join("s.bin");
        std::fs::write(&small, vec![b'x'; 100]).unwrap();
        let md = std::fs::metadata(&small).unwrap();
        assert!(allocated(&md, 100, 4096) >= 100);
    }

    /// 符号链接必须被识别成 reparse —— 否则遍历会跟进链接，大盘上可能死循环。
    #[cfg(unix)]
    #[test]
    fn symlink_is_reparse_on_unix() {
        let d = crate::test_dir::TestDir::new("fw-fsmeta-link").unwrap();
        let target = d.path().join("dir");
        std::fs::create_dir(&target).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let md = std::fs::symlink_metadata(&link).unwrap();
        assert!(is_reparse(&md));
        assert_ne!(attributes(&md) & crate::consts::FILE_ATTRIBUTE_REPARSE_POINT, 0);
    }
}
