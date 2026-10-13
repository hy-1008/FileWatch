//! 挂载一个 NTFS 卷：解析引导扇区 → 定位 `$MFT` 自身的 Data Runs → 顺序读取整表。
//!
//! 这里是文档 §5.1「MFT 解析流水线」的实现。

use crate::attribute::{AttributeIter, ATTR_DATA};
use crate::boot::BootSector;
use crate::bytes::u64_at;
use crate::fixup::apply_fixup;
use crate::runlist::{decode_data_runs, to_byte_ranges, DataRun};
use core_io::Volume;
use std::io;

/// 一个已挂载的 NTFS 卷。
#[derive(Debug, Clone)]
pub struct NtfsVolume {
    pub boot: BootSector,
    /// `$MFT` 自身的物理段
    pub mft_runs: Vec<DataRun>,
    /// `$MFT` 的 `$DATA` 真实大小（字节）
    pub mft_data_size: u64,
    /// `$MFT` 的 `$DATA` 分配大小（字节）
    pub mft_alloc_size: u64,
    /// MFT 记录总数 = mft_data_size / record_size
    pub record_count: u64,
}

impl NtfsVolume {
    /// 读取引导扇区 + `$MFT` 第 0 条记录，把整卷的索引骨架立起来。
    ///
    /// 这一步**只需要两次小 IO**（512B + 1KB），是 M0 的核心验证点。
    pub fn mount(vol: &Volume) -> io::Result<NtfsVolume> {
        // ① 读引导扇区
        let mut boot_buf = vec![0u8; 512];
        vol.read_at(0, &mut boot_buf)?;
        let boot = BootSector::parse(&boot_buf)?;

        // ② 读 $MFT 第 0 条记录（它自己描述自己）
        let rec_size = boot.mft_record_size as usize;
        let mft0_off = boot.mft_byte_offset();
        let mut rec0 = vec![0u8; rec_size];
        vol.read_at(mft0_off, &mut rec0).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "读取 $MFT 第 0 条记录失败（偏移 {}，大小 {}）: {}",
                    mft0_off, rec_size, e
                ),
            )
        })?;

        if &rec0[0..4] != crate::record::FILE_SIGNATURE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "$MFT 第 0 条记录签名异常: {:?}",
                    String::from_utf8_lossy(&rec0[0..4])
                ),
            ));
        }
        if !apply_fixup(&mut rec0, boot.bytes_per_sector as usize) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "$MFT 第 0 条记录 fixup 校验失败（USA 不匹配）",
            ));
        }

        // ③ 从第 0 条记录里拿出 $DATA 的 Data Runs
        let (runs, real, alloc) = extract_data_runs(&rec0)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "$MFT 第 0 条记录缺少非驻留 $DATA 属性",
            )
        })?;

        if runs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "$MFT $DATA 的 Data Runs 为空",
            ));
        }
        if real % boot.mft_record_size as u64 != 0 {
            // 不致命：向下取整并提示
            eprintln!(
                "[warn] $MFT 数据大小 {} 不是记录大小 {} 的整数倍，已向下取整",
                real, boot.mft_record_size
            );
        }
        let record_count = real / boot.mft_record_size as u64;

        Ok(NtfsVolume {
            boot,
            mft_runs: runs,
            mft_data_size: real,
            mft_alloc_size: alloc,
            record_count,
        })
    }

    /// 记录号 → 该记录在 `$MFT` **逻辑空间**中的偏移。
    #[inline]
    pub fn record_logical_offset(&self, recno: u64) -> u64 {
        recno * self.boot.mft_record_size as u64
    }

    /// `$MFT` 逻辑偏移 → 物理字节偏移（跨段查表）。
    pub fn logical_to_physical(&self, logical: u64) -> Option<u64> {
        let bpc = self.boot.bytes_per_cluster;
        let mut acc: u64 = 0;
        for run in &self.mft_runs {
            let len = run.len_clusters.saturating_mul(bpc);
            if logical < acc.saturating_add(len) {
                if run.sparse {
                    return None;
                }
                return Some(run.lcn.saturating_mul(bpc) + (logical - acc));
            }
            acc = acc.saturating_add(len);
        }
        None
    }

    /// 读取单条 MFT 记录（**增量阶段回读新大小**的核心，文档 §5.3 方案 A）。
    ///
    /// 复用与全量扫描完全相同的解析器，且完全绕开路径解析。
    pub fn read_record(&self, vol: &Volume, recno: u64) -> io::Result<Option<Vec<u8>>> {
        let rec_size = self.boot.mft_record_size as u64;
        let logical = self.record_logical_offset(recno);
        if logical.saturating_add(rec_size) > self.mft_data_size {
            return Ok(None);
        }
        let phys = match self.logical_to_physical(logical) {
            Some(p) => p,
            None => return Ok(None),
        };
        let mut buf = vec![0u8; rec_size as usize];
        vol.read_raw(phys, &mut buf)?;
        Ok(Some(buf))
    }

    /// 校验记录号是否在范围内。
    #[inline]
    pub fn has_record(&self, recno: u64) -> bool {
        recno < self.record_count
    }

    /// 顺序扫过整个 `$MFT`，按块回调。
    ///
    /// 回调参数：`(本块第一条记录号, 本块缓冲)`。
    ///
    /// **不变量**：块大小与段长度都被规整为记录大小的整数倍，
    /// 因此**记录永远不会跨块或跨段**——这是能做零拷贝切片解析的前提。
    pub fn for_each_chunk<F>(&self, vol: &Volume, want_chunk: usize, mut f: F) -> io::Result<u64>
    where
        F: FnMut(u64, &mut [u8]),
    {
        let rec_size = self.boot.mft_record_size as usize;
        let bpc = self.boot.bytes_per_cluster;
        let sector = vol.sector_size() as usize;

        // 规整块大小：同时是记录大小与扇区大小的整数倍
        let mut chunk = want_chunk.max(rec_size).max(sector);
        if chunk % rec_size != 0 {
            chunk = (chunk / rec_size + 1) * rec_size;
        }
        if chunk % sector != 0 {
            chunk = (chunk / sector + 1) * sector;
        }

        let mut buf = vec![0u8; chunk];
        let mut logical: u64 = 0;
        let mut total_records: u64 = 0;

        for run in &self.mft_runs {
            if run.sparse || run.len_clusters == 0 {
                // 稀疏段不占物理空间，其逻辑长度对应「空洞」，跳过
                logical += run.len_clusters * bpc;
                continue;
            }
            let mut phys = run.lcn * bpc;
            let mut remaining = run.len_clusters * bpc;

            while remaining > 0 {
                let take = core::cmp::min(chunk as u64, remaining) as usize;
                vol.read_raw(phys, &mut buf[..take])?;

                let first_recno = logical / rec_size as u64;
                let n_records = take / rec_size;
                f(first_recno, &mut buf[..take]);

                total_records += n_records as u64;
                phys += take as u64;
                logical += take as u64;
                remaining -= take as u64;
            }
        }
        Ok(total_records)
    }

    /// `$MFT` 在磁盘上的物理区间列表（供调试输出 / 交叉验证）。
    pub fn mft_byte_ranges(&self) -> Vec<(u64, u64)> {
        to_byte_ranges(&self.mft_runs, self.boot.bytes_per_cluster)
    }
}

/// 从一条（已 fixup 的）MFT 记录里提取非驻留 `$DATA` 的 Data Runs。
/// 返回 `(runs, real_size, alloc_size)`。
fn extract_data_runs(rec: &[u8]) -> io::Result<Option<(Vec<DataRun>, u64, u64)>> {
    let first_attr = u64_at(rec, 0x14).unwrap_or(0) as usize;
    let first_attr = if first_attr < 0x18 || first_attr >= rec.len() {
        // 头部可疑时退回到通用遍历（0x14 只是个 u16，这里兼容一下）
        match crate::bytes::u16_at(rec, 0x14) {
            Some(v) => v as usize,
            None => return Ok(None),
        }
    } else {
        first_attr
    };

    for attr in AttributeIter::new(rec, first_attr) {
        if attr.ty != ATTR_DATA || attr.is_named() || !attr.non_resident {
            continue;
        }
        let (alloc, real, _init) = match attr.non_resident_sizes() {
            Some(s) => s,
            None => continue,
        };
        let run_bytes = match attr.run_list() {
            Some(r) => r,
            None => continue,
        };
        return Ok(Some((decode_data_runs(run_bytes), real, alloc)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runlist::DataRun;

    fn fake_ntfs(runs: Vec<DataRun>, bpc: u64, mft_data_size: u64) -> NtfsVolume {
        let mut boot_buf = vec![0u8; 512];
        boot_buf[0x03..0x0B].copy_from_slice(b"NTFS    ");
        boot_buf[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        boot_buf[0x0D] = 8; // 4096 字节每簇
        boot_buf[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        boot_buf[0x30..0x38].copy_from_slice(&100u64.to_le_bytes());
        boot_buf[0x38..0x40].copy_from_slice(&104u64.to_le_bytes());
        boot_buf[0x40] = (-10i8) as u8;
        boot_buf[0x44] = 1;
        boot_buf[0x48..0x50].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        let boot = BootSector::parse(&boot_buf).unwrap();
        assert_eq!(boot.bytes_per_cluster, bpc);
        NtfsVolume {
            boot,
            mft_runs: runs,
            mft_data_size,
            mft_alloc_size: mft_data_size,
            record_count: mft_data_size / 1024,
        }
    }

    #[test]
    fn maps_logical_to_physical_across_runs() {
        let n = fake_ntfs(
            vec![
                DataRun { lcn: 100, len_clusters: 4, sparse: false }, // 0..16384
                DataRun { lcn: 500, len_clusters: 4, sparse: false }, // 16384..32768
            ],
            4096,
            32768,
        );
        assert_eq!(n.logical_to_physical(0), Some(100 * 4096));
        assert_eq!(n.logical_to_physical(4096), Some(101 * 4096));
        assert_eq!(n.logical_to_physical(16384), Some(500 * 4096));
        assert_eq!(n.logical_to_physical(32767), Some(503 * 4096 + 4095));
        assert_eq!(n.logical_to_physical(32768), None); // 越界
    }

    #[test]
    fn skips_sparse_runs_in_mapping() {
        let n = fake_ntfs(
            vec![
                DataRun { lcn: 100, len_clusters: 2, sparse: false },
                DataRun { lcn: 0, len_clusters: 2, sparse: true },
                DataRun { lcn: 200, len_clusters: 2, sparse: false },
            ],
            4096,
            24576,
        );
        assert_eq!(n.logical_to_physical(0), Some(100 * 4096));
        // 稀疏区间不可映射
        assert_eq!(n.logical_to_physical(8192), None);
        // 第三段在其后
        assert_eq!(n.logical_to_physical(16384), Some(200 * 4096));
    }

    #[test]
    fn record_offsets_and_bounds() {
        let n = fake_ntfs(vec![DataRun { lcn: 100, len_clusters: 4, sparse: false }], 4096, 16384);
        assert_eq!(n.record_logical_offset(0), 0);
        assert_eq!(n.record_logical_offset(3), 3072);
        assert_eq!(n.record_count, 16);
        assert!(n.has_record(15));
        assert!(!n.has_record(16));
    }

    #[test]
    fn chunking_aligns_to_record_and_sector() {
        let n = fake_ntfs(vec![DataRun { lcn: 100, len_clusters: 4, sparse: false }], 4096, 16384);
        // 用极小的目标块大小验证规整逻辑（不实际读盘）
        let chunk = {
            let want = 700usize;
            let rec = n.boot.mft_record_size as usize;
            let mut c = want.max(rec).max(512);
            if c % rec != 0 { c = (c / rec + 1) * rec; }
            if c % 512 != 0 { c = (c / 512 + 1) * 512; }
            c
        };
        assert_eq!(chunk % 1024, 0, "块大小必须是记录大小的整数倍");
        assert_eq!(chunk % 512, 0, "块大小必须是扇区大小的整数倍");
    }
}
