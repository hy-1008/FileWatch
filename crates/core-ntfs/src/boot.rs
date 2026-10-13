//! NTFS 引导扇区（$Boot）解析。
//!
//! 对应文档 §5.1 的字段偏移表。

use crate::bytes::*;
use std::io;

pub const NTFS_OEM_ID: &[u8; 8] = b"NTFS    ";

#[derive(Debug, Clone)]
pub struct BootSector {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub bytes_per_cluster: u64,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mftmirr_lcn: u64,
    pub clusters_per_mft_record_raw: i8,
    pub clusters_per_index_record_raw: i8,
    /// 单条 MFT 记录的字节数（通常 1024）
    pub mft_record_size: u32,
    pub index_record_size: u32,
    pub volume_serial: u64,
}

impl BootSector {
    /// 从 512 字节（或更大）的引导扇区缓冲解析。
    pub fn parse(buf: &[u8]) -> io::Result<BootSector> {
        if buf.len() < 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("引导扇区长度不足: {} < 512", buf.len()),
            ));
        }
        // 0x03 起 8 字节 OEM ID 必须是 "NTFS    "
        let oem = buf
            .get(0x03..0x0B)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "引导扇区越界"))?;
        if oem != NTFS_OEM_ID {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "非 NTFS 卷：OEM ID = {:?}（期望 \"NTFS    \"）",
                    String::from_utf8_lossy(oem)
                ),
            ));
        }

        let bytes_per_sector = u16_at(buf, 0x0B).unwrap_or(0) as u32;
        let sectors_per_cluster = u8_at(buf, 0x0D).unwrap_or(0) as u32;
        let total_sectors = u64_at(buf, 0x28).unwrap_or(0);
        let mft_lcn = u64_at(buf, 0x30).unwrap_or(0);
        let mftmirr_lcn = u64_at(buf, 0x38).unwrap_or(0);
        let clusters_per_mft_record_raw = i8_at(buf, 0x40).unwrap_or(0);
        let clusters_per_index_record_raw = i8_at(buf, 0x44).unwrap_or(0);
        let volume_serial = u64_at(buf, 0x48).unwrap_or(0);

        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("非法 bytes_per_sector = {}", bytes_per_sector),
            ));
        }
        if sectors_per_cluster == 0 || sectors_per_cluster > 128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("非法 sectors_per_cluster = {}", sectors_per_cluster),
            ));
        }

        let bytes_per_cluster = bytes_per_sector as u64 * sectors_per_cluster as u64;

        // 有符号值的语义：
        //   > 0 → 记录占这么多个簇
        //   < 0 → 记录大小 = 2^(-n) 字节
        let mft_record_size = decode_record_size(clusters_per_mft_record_raw, bytes_per_cluster)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "非法 clusters_per_mft_record = {}",
                        clusters_per_mft_record_raw
                    ),
                )
            })?;
        let index_record_size =
            decode_record_size(clusters_per_index_record_raw, bytes_per_cluster).unwrap_or(4096);

        if mft_record_size < 512 || mft_record_size > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("异常 MFT 记录大小 = {} 字节", mft_record_size),
            ));
        }

        Ok(BootSector {
            bytes_per_sector,
            sectors_per_cluster,
            bytes_per_cluster,
            total_sectors,
            mft_lcn,
            mftmirr_lcn,
            clusters_per_mft_record_raw,
            clusters_per_index_record_raw,
            mft_record_size,
            index_record_size,
            volume_serial,
        })
    }

    /// 卷总容量（字节）。
    pub fn total_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }

    /// $MFT 起始位置的字节偏移。
    pub fn mft_byte_offset(&self) -> u64 {
        self.mft_lcn * self.bytes_per_cluster
    }

    /// 簇号 → 字节偏移。
    #[inline]
    pub fn lcn_to_offset(&self, lcn: u64) -> u64 {
        lcn * self.bytes_per_cluster
    }
}

/// 把引导扇区里的「簇数 / 2 的负幂」字段还原为字节大小。
fn decode_record_size(raw: i8, bytes_per_cluster: u64) -> Option<u32> {
    if raw > 0 {
        Some((raw as u64 * bytes_per_cluster) as u32)
    } else if raw < 0 {
        let shift = (-(raw as i32)) as u32;
        if shift > 31 {
            return None;
        }
        Some(1u32 << shift)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_boot(bps: u16, spc: u8, mft_lcn: u64, cpmft: i8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0x03..0x0B].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&bps.to_le_bytes());
        b[0x0D] = spc;
        b[0x28..0x30].copy_from_slice(&(100_000u64).to_le_bytes());
        b[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&(mft_lcn + 4).to_le_bytes());
        b[0x40] = cpmft as u8;
        b[0x44] = 1;
        b
    }

    #[test]
    fn parses_typical_ntfs() {
        // 512B 扇区 / 8 扇区每簇(4KB) / MFT 记录 = -10 → 1024 字节
        let b = fake_boot(512, 8, 786432, -10);
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.bytes_per_sector, 512);
        assert_eq!(bs.sectors_per_cluster, 8);
        assert_eq!(bs.bytes_per_cluster, 4096);
        assert_eq!(bs.mft_record_size, 1024);
        assert_eq!(bs.mft_lcn, 786432);
        assert_eq!(bs.mft_byte_offset(), 786432 * 4096);
        assert_eq!(bs.index_record_size, 4096);
    }

    #[test]
    fn positive_cluster_count_form() {
        // cpmft = +2 且簇 4KB → 记录 8KB
        let b = fake_boot(512, 8, 100, 2);
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.mft_record_size, 8192);
    }

    #[test]
    fn rejects_non_ntfs() {
        let mut b = fake_boot(512, 8, 100, -10);
        b[0x03..0x0B].copy_from_slice(b"FAT32   ");
        assert!(BootSector::parse(&b).is_err());
    }

    #[test]
    fn rejects_bad_sector_size() {
        let b = fake_boot(123, 8, 100, -10);
        assert!(BootSector::parse(&b).is_err());
    }
}
