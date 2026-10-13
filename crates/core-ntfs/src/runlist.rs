//! Data Runs（数据运行列表）解码。
//!
//! 非驻留属性的数据在磁盘上是**不连续**的，用一串「运行」描述：
//! 每项 = `长度(簇数)` + `起始 LCN 相对上一项的偏移`。
//! 这两段都是变长小端整数，头字节的低 4 位/高 4 位分别是两段的字节数。
//!
//! 这个解码器有两处关键用途：
//! 1. 全量扫描：拿到 `$MFT` 自身在磁盘上的物理段，从而顺序读整表；
//! 2. 增量阶段：如果需要，可以定位任意文件的数据位置。

use crate::bytes::{le_sint, le_uint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// 起始簇号（稀疏段无意义）
    pub lcn: u64,
    /// 连续簇数
    pub len_clusters: u64,
    /// 稀疏段：无对应物理位置（`off_size == 0`）
    pub sparse: bool,
}

impl DataRun {
    /// 本段覆盖的字节数
    #[inline]
    pub fn byte_len(&self, bytes_per_cluster: u64) -> u64 {
        self.len_clusters * bytes_per_cluster
    }

    /// 本段在磁盘上的字节起始偏移
    #[inline]
    pub fn byte_offset(&self, bytes_per_cluster: u64) -> u64 {
        self.lcn * bytes_per_cluster
    }
}

/// 解码属性里的 Data Runs 字节串。
///
/// 遇到 `0x00` 结束符、或任何非法/越界编码即停止（已解析出来的部分照样返回，
/// 这是一种「尽力而为」的容错策略：损坏的运行列表不该让整个扫描崩掉）。
pub fn decode_data_runs(buf: &[u8]) -> Vec<DataRun> {
    let mut runs = Vec::with_capacity(8);
    let mut i = 0usize;
    let mut lcn: i64 = 0;

    while i < buf.len() {
        let header = buf[i];
        if header == 0 {
            break; // 结束符
        }
        i += 1;

        let len_size = (header & 0x0F) as usize;
        let off_size = ((header >> 4) & 0x0F) as usize;

        if len_size == 0 || len_size > 8 || off_size > 8 {
            break; // 非法编码
        }
        if i + len_size + off_size > buf.len() {
            break; // 越界
        }

        let run_len = match le_uint(buf, i, len_size) {
            Some(v) => v,
            None => break,
        };
        i += len_size;

        if off_size == 0 {
            // 稀疏段：不占物理空间，LCN 不推进
            runs.push(DataRun {
                lcn: 0,
                len_clusters: run_len,
                sparse: true,
            });
            continue;
        }

        let delta = match le_sint(buf, i, off_size) {
            Some(v) => v,
            None => break,
        };
        i += off_size;

        lcn = match lcn.checked_add(delta) {
            Some(v) if v >= 0 => v,
            _ => break, // 溢出或负值：可判定为损坏
        };

        runs.push(DataRun {
            lcn: lcn as u64,
            len_clusters: run_len,
            sparse: false,
        });
    }

    runs
}

/// 把非稀疏段转成 `[(字节偏移, 字节长度)]`，供 `ChunkReader` 顺序读。
pub fn to_byte_ranges(runs: &[DataRun], bytes_per_cluster: u64) -> Vec<(u64, u64)> {
    runs.iter()
        .filter(|r| !r.sparse && r.len_clusters > 0)
        .map(|r| (r.byte_offset(bytes_per_cluster), r.byte_len(bytes_per_cluster)))
        .collect()
}

/// 所有段的总簇数。
pub fn total_clusters(runs: &[DataRun]) -> u64 {
    runs.iter().map(|r| r.len_clusters).sum()
}

/// 非稀疏段的总字节数（= 实际数据长度）。
pub fn total_bytes(runs: &[DataRun], bytes_per_cluster: u64) -> u64 {
    runs.iter()
        .filter(|r| !r.sparse)
        .map(|r| r.byte_len(bytes_per_cluster))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_run() {
        // header 0x21: len_size=1, off_size=2；len=0x18(24簇)，off=+0x0C40
        let buf = [0x21u8, 0x18, 0x40, 0x0C, 0x00];
        let runs = decode_data_runs(&buf);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len_clusters, 24);
        assert_eq!(runs[0].lcn, 0x0C40);
        assert!(!runs[0].sparse);
    }

    #[test]
    fn decodes_relative_offsets() {
        // 第一段 LCN=100（1 字节正偏移），第二段 +50（1 字节）
        let buf = [0x11u8, 0x10, 0x64, 0x11, 0x20, 0x32, 0x00];
        let runs = decode_data_runs(&buf);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].lcn, 100);
        assert_eq!(runs[1].lcn, 150); // 100 + 50
        assert_eq!(runs[0].len_clusters, 0x10);
    }

    #[test]
    fn handles_negative_delta() {
        // 第二段偏移为 0xF6 = -10 → LCN 回退
        let buf = [0x11u8, 0x10, 0x64, 0x11, 0x20, 0xF6, 0x00];
        let runs = decode_data_runs(&buf);
        assert_eq!(runs[0].lcn, 100);
        assert_eq!(runs[1].lcn, 90);
    }

    #[test]
    fn handles_sparse_run() {
        // 稀疏段：off_size = 0 → header 0x01
        let buf = [0x11u8, 0x10, 0x64, 0x01, 0x08, 0x11, 0x10, 0x0A, 0x00];
        let runs = decode_data_runs(&buf);
        assert_eq!(runs.len(), 3);
        assert!(runs[1].sparse);
        assert_eq!(runs[1].len_clusters, 8);
        // 稀疏段不推进 LCN
        assert_eq!(runs[2].lcn, 110); // 100 + 10
    }

    #[test]
    fn stops_at_terminator() {
        let buf = [0x11u8, 0x10, 0x64, 0x00, 0xFF, 0xFF];
        assert_eq!(decode_data_runs(&buf).len(), 1);
    }

    #[test]
    fn stops_on_truncated_run() {
        let buf = [0x11u8, 0x10]; // 声明需要 1+1 字节，但没了
        assert!(decode_data_runs(&buf).is_empty());
    }

    #[test]
    fn byte_ranges_skip_sparse() {
        let runs = vec![
            DataRun { lcn: 2, len_clusters: 3, sparse: false },
            DataRun { lcn: 0, len_clusters: 5, sparse: true },
            DataRun { lcn: 10, len_clusters: 2, sparse: false },
        ];
        let r = to_byte_ranges(&runs, 1024);
        assert_eq!(r, vec![(2048, 3072), (10240, 2048)]);
        assert_eq!(total_clusters(&runs), 10);
        assert_eq!(total_bytes(&runs, 1024), 5 * 1024);
    }
}
