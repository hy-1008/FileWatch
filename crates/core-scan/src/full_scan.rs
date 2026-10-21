//! MFT 全量扫描（M1 的核心）。
//!
//! 流程（文档 §5.1）：
//!
//! ```text
//! open_volume("\\.\C:")
//!   └─ read(boot_sector, 512B) → BootSector
//!   └─ read_mft_record(0)      → $MFT 自己的 Data Runs
//!   └─ for each run: 读 16 MB 块
//!        └─ 按 record_size 切片
//!             └─ looks_like_active_file()   ← 最便宜的过滤放最前
//!                  └─ parse_record()        ← fixup + 属性链
//!                       └─ tree.insert()
//! ```
//!
//! 性能要点（文档 §5.1）：
//! 1. 4 字节签名 + `flags` 位粗过滤，跳过空闲记录；
//! 2. 大块顺序读（默认 16 MB/次）；
//! 3. 只在需要时做完整属性解析。

use core_index::aggregate;
use core_index::Tree;
use core_io::{ChunkReader, Volume, MFT_READ_CHUNK};
use core_ntfs::record::{looks_like_active_file, parse_record};
use core_ntfs::NtfsVolume;
use std::io;
use std::time::Instant;

/// 平均文件名的 UTF-8 字节数估计——用于一次性给字符串池预留容量，避免反复 realloc。
/// 取 28 字节（含 NUL + 对齐余量）比较接近真实 NTFS 卷。
pub const AVG_NAME_BYTES: usize = 28;

#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// 单次顺序读的块大小
    pub chunk_bytes: usize,
    /// 是否预分配节点数组（拿到 record_count 后）
    pub preallocate: bool,
    /// 进度回调的节流间隔（每处理多少条记录回调一次）
    pub progress_every: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            chunk_bytes: MFT_READ_CHUNK,
            preallocate: true,
            progress_every: 50_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanStats {
    /// 遍历过的记录槽位数
    pub slots_seen: u64,
    /// 成功解析并入库的记录数
    pub records_parsed: u64,
    /// 因空闲/无 `$FILE_NAME` 跳过的记录数
    pub records_skipped: u64,
    /// fixup 校验失败的记录数（文档 R1 的可观测指标）
    pub records_corrupt: u64,
    /// 顺序读的块数
    pub chunks_read: u64,
    /// 从磁盘读取的字节数
    pub bytes_read: u64,
    pub elapsed_ms: u128,
    /// 记录总数（来自 `$MFT` 的 `$DATA` 大小）
    pub total_records: u64,
}

impl ScanStats {
    pub fn throughput_mb_s(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return 0.0;
        }
        (self.bytes_read as f64 / (1024.0 * 1024.0)) / (self.elapsed_ms as f64 / 1000.0)
    }

    pub fn records_per_sec(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return 0.0;
        }
        (self.slots_seen as f64) / (self.elapsed_ms as f64 / 1000.0)
    }

    pub fn summary_cn(&self) -> String {
        format!(
            "扫描完成：{} 条记录（跳过 {} / 损坏 {}），读 {} 个块共 {}，耗时 {} ms（{:.0} 条/秒，{:.0} MB/s）",
            self.records_parsed,
            self.records_skipped,
            self.records_corrupt,
            self.chunks_read,
            core_io::util::human_size(self.bytes_read),
            self.elapsed_ms,
            self.records_per_sec(),
            self.throughput_mb_s()
        )
    }
}

/// 用 MFT 直读做全量扫描。
///
/// `progress(done_slots, total_records)` 会被周期性调用，
/// 前端据此做「扫到多少画多少」的流式渲染（功能 F2）。
pub fn full_scan_mft<P>(
    vol: &Volume,
    drive: &str,
    ntfs: &NtfsVolume,
    opts: ScanOptions,
    progress: P,
) -> io::Result<(Tree, ScanStats)>
where
    P: FnMut(u64, u64),
{
    full_scan_mft_with(vol, drive, ntfs, opts, progress, None)
}

/// 每多少个 **MFT 槽位**回调一次半成品视图。
///
/// 200k 槽位在 SSD 上约 0.1~0.2 s，2M 条记录共 10 次回调 —— 界面看起来是
/// 「一帧一帧长出来」，而聚合的开销（每次 O(n)）摊到 10 次可以忽略。
/// 和进度条用同一个步长没有意义：进度要密，预览要省。
pub const PARTIAL_STRIDE: u64 = 200_000;

/// [`full_scan_mft`] + 半成品视图回调（E7）。
pub fn full_scan_mft_with<P>(
    vol: &Volume,
    drive: &str,
    ntfs: &NtfsVolume,
    opts: ScanOptions,
    mut progress: P,
    partial: Option<&crate::session::PartialSink>,
) -> io::Result<(Tree, ScanStats)>
where
    P: FnMut(u64, u64),
{
    let t0 = Instant::now();
    let mut stats = ScanStats {
        total_records: ntfs.record_count,
        ..Default::default()
    };

    let mut tree = Tree::new(drive, 0);
    if opts.preallocate {
        // 名字池按「记录数 × 平均名字长度」预留；一次分配好，之后不再 realloc
        let name_estimate = (ntfs.record_count as usize)
            .saturating_mul(AVG_NAME_BYTES)
            .min(512 * 1024 * 1024);
        tree.preallocate(ntfs.record_count, name_estimate);
    }

    let rec_size = ntfs.boot.mft_record_size as usize;
    let sector = vol.sector_size() as usize;
    let progress_every = opts.progress_every.max(1);
    let mut next_progress_at = progress_every;
    let mut next_partial_at = PARTIAL_STRIDE;

    // 逐段顺序读。`for_each_chunk` 保证记录不跨块/跨段，
    // 因此这里可以对每块做零拷贝的定长切片。
    ntfs.for_each_chunk(vol, opts.chunk_bytes, |first_recno, buf| {
        stats.chunks_read += 1;
        stats.bytes_read += buf.len() as u64;

        let n = buf.len() / rec_size;
        for i in 0..n {
            let start = i * rec_size;
            let rec = &mut buf[start..start + rec_size];
            stats.slots_seen += 1;

            // ① 最便宜的过滤
            if !looks_like_active_file(rec) {
                stats.records_skipped += 1;
                continue;
            }
            // ② 完整解析（就地 fixup）
            match parse_record(rec, first_recno + i as u64, sector) {
                Some(r) => {
                    if tree.insert(&r, rec).is_some() {
                        stats.records_parsed += 1;
                    } else {
                        stats.records_skipped += 1;
                    }
                }
                None => {
                    // 签名对但 fixup 失败 → 损坏记录
                    stats.records_corrupt += 1;
                }
            }
        }

        // 流式进度：前端据此做「扫到多少画多少」（功能 F2）
        if stats.slots_seen >= next_progress_at {
            progress(stats.slots_seen, stats.total_records);
            next_progress_at = stats.slots_seen + progress_every;
        }

        // E7：半成品视图。`tick` 内部先聚合再回调 —— 扫描中目录节点的 size
        // 还是 0，不聚合就画出来是一屏 0 面积的方块。
        if let Some(sink) = partial {
            sink.tick(&mut tree, stats.slots_seen, &mut next_partial_at, PARTIAL_STRIDE);
        }
    })?;

    // ③ 聚合：目录大小 / 文件数
    aggregate::aggregate(&mut tree);
    core_index::tree::mark_orphans(&mut tree);

    stats.elapsed_ms = t0.elapsed().as_millis();
    progress(stats.slots_seen, stats.total_records);
    Ok((tree, stats))
}

/// M0 技术验证：只读引导扇区 + `$MFT` 第 0 条记录，打印 Data Runs。
///
/// 对应文档 §8 的 M0 验收：打印出正确的簇大小、MFT 起始 LCN、Data Runs，
/// 并与 `fsutil fsinfo ntfsinfo C:` 交叉验证。
pub fn probe_volume(vol: &Volume) -> io::Result<ProbeReport> {
    let mut boot_buf = vec![0u8; 512];
    vol.read_at(0, &mut boot_buf)?;
    let boot = core_ntfs::BootSector::parse(&boot_buf)?;

    let mft0_off = boot.mft_byte_offset();
    let mut rec0 = vec![0u8; boot.mft_record_size as usize];
    vol.read_at(mft0_off, &mut rec0)?;

    let signature = String::from_utf8_lossy(&rec0[0..4]).to_string();
    let fixed_up = core_ntfs::fixup::apply_fixup(&mut rec0, boot.bytes_per_sector as usize);

    let mut runs = Vec::new();
    let mut real = 0u64;
    let mut alloc = 0u64;
    if fixed_up {
        // 首个属性的偏移必须从记录头 0x14 读，不能硬编码
        let first_attr = core_ntfs::bytes::u16_at(&rec0, 0x14).unwrap_or(0x38) as usize;
        for attr in core_ntfs::attribute::AttributeIter::new(&rec0, first_attr) {
            if attr.ty == core_ntfs::attribute::ATTR_DATA && !attr.is_named() && attr.non_resident {
                if let Some((a, r, _i)) = attr.non_resident_sizes() {
                    real = r;
                    alloc = a;
                }
                if let Some(rb) = attr.run_list() {
                    runs = core_ntfs::runlist::decode_data_runs(rb);
                }
                break;
            }
        }
    }

    Ok(ProbeReport {
        boot,
        mft_record_signature: signature,
        mft_fixup_ok: fixed_up,
        mft_runs: runs,
        mft_data_real_size: real,
        mft_data_alloc_size: alloc,
        sector_size_used: vol.sector_size(),
    })
}

#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub boot: core_ntfs::BootSector,
    pub mft_record_signature: String,
    pub mft_fixup_ok: bool,
    pub mft_runs: Vec<core_ntfs::runlist::DataRun>,
    pub mft_data_real_size: u64,
    pub mft_data_alloc_size: u64,
    pub sector_size_used: u32,
}

impl ProbeReport {
    pub fn record_count(&self) -> u64 {
        if self.boot.mft_record_size == 0 {
            0
        } else {
            self.mft_data_real_size / self.boot.mft_record_size as u64
        }
    }

    /// 与 `fsutil fsinfo ntfsinfo X:` 逐项对照的文本。
    pub fn render_cn(&self, drive: &str) -> String {
        let mut s = String::new();
        s.push_str(&format!("盘符: {}\n", drive));
        s.push_str(&format!(
            "每扇区字节数: {}\n",
            self.boot.bytes_per_sector
        ));
        s.push_str(&format!(
            "每簇扇区数: {}\n",
            self.boot.sectors_per_cluster
        ));
        s.push_str(&format!(
            "每簇字节数: {}\n",
            self.boot.bytes_per_cluster
        ));
        s.push_str(&format!(
            "总扇区数: {}  总容量: {}\n",
            self.boot.total_sectors,
            core_io::util::human_size(self.boot.total_bytes())
        ));
        s.push_str(&format!("MFT 起始簇号 (LCN): {}\n", self.boot.mft_lcn));
        s.push_str(&format!(
            "MFT 起始字节偏移: {}\n",
            self.boot.mft_byte_offset()
        ));
        s.push_str(&format!("MFT 镜像 LCN: {}\n", self.boot.mftmirr_lcn));
        s.push_str(&format!(
            "每 MFT 记录字节数: {}\n",
            self.boot.mft_record_size
        ));
        s.push_str(&format!(
            "每索引记录字节数: {}\n",
            self.boot.index_record_size
        ));
        s.push_str(&format!(
            "卷序列号: 0x{:016X}\n",
            self.boot.volume_serial
        ));
        s.push_str(&format!(
            "$MFT 记录 0 签名: {}  fixup: {}\n",
            self.mft_record_signature,
            if self.mft_fixup_ok { "OK" } else { "失败" }
        ));
        s.push_str(&format!(
            "$MFT $DATA 真实大小: {}  分配大小: {}\n",
            self.mft_data_real_size, self.mft_data_alloc_size
        ));
        s.push_str(&format!("$MFT 记录总数: {}\n", self.record_count()));
        s.push_str(&format!(
            "$MFT 物理段数: {}  前 5 段:\n",
            self.mft_runs.len()
        ));
        for (i, r) in self.mft_runs.iter().take(5).enumerate() {
            s.push_str(&format!(
                "  [{}] LCN={} 簇数={} 字节偏移={} 长度={}\n",
                i,
                r.lcn,
                r.len_clusters,
                r.byte_offset(self.boot.bytes_per_cluster),
                core_io::util::human_size(r.byte_len(self.boot.bytes_per_cluster))
            ));
        }
        if self.mft_runs.len() > 5 {
            s.push_str(&format!("  ... 另有 {} 段\n", self.mft_runs.len() - 5));
        }
        s
    }
}

/// 用 MFT 找前 N 大文件（不构建整棵树，内存恒定，文档 §5.1 性能要点 3）。
///
/// 适合「只想看最大的几个文件」的场景，200 万文件下内存占用与 N 成正比。
pub fn top_n_files_via_mft(
    vol: &Volume,
    ntfs: &NtfsVolume,
    n: usize,
    chunk_bytes: usize,
) -> io::Result<Vec<(u64, u64, u64, String)>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let rec_size = ntfs.boot.mft_record_size as usize;
    let sector = vol.sector_size() as usize;
    let mut heap: BinaryHeap<Reverse<(u64, u64, u64, String)>> = BinaryHeap::with_capacity(n + 1);

    ntfs.for_each_chunk(vol, chunk_bytes, |first_recno, buf| {
        let nrec = buf.len() / rec_size;
        for i in 0..nrec {
            let start = i * rec_size;
            let rec = &mut buf[start..start + rec_size];
            if !looks_like_active_file(rec) {
                continue;
            }
            if let Some(r) = parse_record(rec, first_recno + i as u64, sector) {
                if r.is_dir() || r.real_size == 0 {
                    continue;
                }
                let name_bytes = match r.name_bytes(rec) {
                    Some(b) => b,
                    None => continue,
                };
                let units: Vec<u16> = name_bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&units);
                let item = (r.real_size, r.frn, r.parent_frn, name);
                if heap.len() < n {
                    heap.push(Reverse(item));
                } else if let Some(Reverse(min)) = heap.peek() {
                    // `min` 是 `&(u64, u64, u64, String)` —— 不能用 `&Reverse(min)`
                    // 解构，那会把整个元组（含 String）从共享引用里 move 出来。
                    if item.0 > min.0 {
                        heap.pop();
                        heap.push(Reverse(item));
                    }
                }
            }
        }
    })?;

    let mut v: Vec<(u64, u64, u64, String)> = heap
        .into_iter()
        .map(|Reverse((size, frn, parent, name))| (size, frn, parent, name))
        .collect();
    v.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    Ok(v)
}

/// 供上层复用：把 `ChunkReader` 直接包成「按记录遍历」的迭代器形态。
///
/// 目前 `full_scan_mft` 用的是 `NtfsVolume::for_each_chunk`，
/// 这个函数保留给「只知道物理区间、没有 NtfsVolume」的场景（如镜像取证）。
pub fn scan_physical_range<P>(
    vol: &Volume,
    start: u64,
    end: u64,
    record_size: u32,
    sector_size: usize,
    chunk_bytes: usize,
    mut on_record: P,
) -> io::Result<u64>
where
    P: FnMut(&mut [u8], u64),
{
    let mut reader = ChunkReader::new(vol, start, end, chunk_bytes);
    let rec = record_size as usize;
    let mut recno: u64 = 0;
    let mut count = 0u64;

    while let Some(chunk) = reader.next_chunk_mut()? {
        let n = chunk.len() / rec;
        for i in 0..n {
            let s = i * rec;
            on_record(&mut chunk[s..s + rec], recno);
            recno += 1;
            count += 1;
        }
    }
    let _ = sector_size;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_options_defaults_documented() {
        let o = ScanOptions::default();
        assert_eq!(o.chunk_bytes, 16 * 1024 * 1024, "文档 §5.1：16 MB/次");
        assert!(o.preallocate);
    }

    #[test]
    fn stats_throughput_math() {
        let s = ScanStats {
            bytes_read: 1024 * 1024 * 100,
            elapsed_ms: 1000,
            slots_seen: 200_000,
            ..Default::default()
        };
        assert!((s.throughput_mb_s() - 100.0).abs() < 0.01);
        assert!((s.records_per_sec() - 200_000.0).abs() < 1.0);
    }

    #[test]
    fn stats_throughput_zero_elapsed() {
        let s = ScanStats::default();
        assert_eq!(s.throughput_mb_s(), 0.0);
        assert_eq!(s.records_per_sec(), 0.0);
    }

    #[test]
    fn stats_summary_is_readable() {
        let s = ScanStats {
            records_parsed: 1_000_000,
            slots_seen: 1_200_000,
            records_skipped: 200_000,
            elapsed_ms: 3000,
            bytes_read: 1024 * 1024 * 1024,
            ..Default::default()
        };
        let t = s.summary_cn();
        assert!(t.contains("1000000") || t.contains("1,000,000") || t.contains("1000000"));
        assert!(t.contains("MB/s"));
    }
}
