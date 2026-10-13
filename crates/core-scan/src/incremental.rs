//! USN 增量会话：拉取变更 → 归一化 → 驱动监控引擎。
//!
//! 对应文档 §5.2 状态机的「增量模式」分支与 §5.3 的「回读 MFT 拿新大小」。

use core_index::Tree;
use core_io::Volume;
use core_monitor::{MonitorEngine, RawEvent, TickOutcome};
use core_ntfs::NtfsVolume;
use core_usn::journal::{self, DEFAULT_ALLOCATION_DELTA, DEFAULT_MAXIMUM_SIZE};
use core_usn::{evaluate, Cursor, CursorValidity, ReadRequest, UsnJournalData};
use std::io;

/// FRN → MFT 记录号
#[inline]
fn recno_of(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// 一次增量会话的全部状态。
pub struct IncrementalSession {
    pub vol: Volume,
    /// 有 NTFS 才能回读记录拿大小（文档 §5.3 方案 A）
    pub ntfs: Option<NtfsVolume>,
    pub journal: UsnJournalData,
    pub cursor: Cursor,
    buf: Vec<u8>,
    /// 累计读到的 USN 记录数（与 `engine.stats.raw_ingested` 对账，文档 §9）
    pub records_read: u64,
    pub pages_read: u64,
    /// 本次会话内检出的断层次数
    pub gaps_detected: u64,
}

impl IncrementalSession {
    /// 用已打开的卷建立会话。
    pub fn from_volume(
        vol: Volume,
        allow_create_journal: bool,
        ntfs: Option<NtfsVolume>,
    ) -> io::Result<(IncrementalSession, CursorValidity, bool)> {
        let (journal, created) = journal::ensure(
            &vol,
            DEFAULT_MAXIMUM_SIZE,
            DEFAULT_ALLOCATION_DELTA,
            allow_create_journal,
        )?;

        let stored = Cursor::load(vol.drive());
        let validity = evaluate(stored.as_ref(), &journal);
        let cursor = match (&stored, validity) {
            (Some(c), CursorValidity::Valid) => c.clone(),
            _ => Cursor::rebuild(vol.drive(), &journal),
        };

        Ok((
            IncrementalSession {
                vol,
                ntfs,
                journal,
                cursor,
                buf: vec![0u8; core_usn::DEFAULT_READ_BUFFER],
                records_read: 0,
                pages_read: 0,
                gaps_detected: 0,
            },
            validity,
            created,
        ))
    }

    /// 全量扫描完成后调用：把游标对齐到「扫描开始前」的日志位置。
    ///
    /// 这样扫描期间产生的变更会在随后的增量轮询里被重放，
    /// 不会因为「扫描期间有新写入」而漏事件。
    pub fn align_cursor_after_scan(&mut self, usn_snapshot: i64) {
        self.cursor = Cursor::new(self.vol.drive(), self.journal.id, usn_snapshot);
    }

    /// 重新查询日志头（游标可能被别的进程推动）。
    pub fn refresh_journal(&mut self) -> io::Result<()> {
        self.journal = journal::query(&self.vol)?;
        Ok(())
    }

    /// 拉一批变更并喂进引擎。返回本批读到的记录数。
    pub fn pull(&mut self, engine: &mut MonitorEngine, now_ms: u64) -> io::Result<usize> {
        let req = ReadRequest::new(self.journal.id, self.cursor.next_usn);
        let (next_usn, n) = journal::read_page(&self.vol, req, &mut self.buf)?;
        self.pages_read += 1;

        if n == 0 {
            return Ok(0);
        }

        let body = &self.buf[8..8 + n];
        let records = core_usn::parse_records(body);
        let count = records.len();

        for r in &records {
            let name = match r.name_bytes(body) {
                Some(b) => {
                    let units: Vec<u16> = b
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&units)
                }
                None => continue,
            };
            engine.ingest(
                RawEvent {
                    frn: r.frn,
                    parent_frn: r.parent_frn,
                    usn: r.usn,
                    timestamp: r.timestamp,
                    reason: r.reason,
                    file_attributes: r.file_attributes,
                    name,
                },
                now_ms,
            );
        }

        self.records_read += count as u64;

        // 游标只前进，不后退
        if next_usn > self.cursor.next_usn {
            self.cursor.next_usn = next_usn;
        }
        Ok(count)
    }

    /// 处理一轮：拉取 + 应用 + 预警。
    ///
    /// 内部把「回读 MFT 拿新大小」封装成 `size_probe` 闭包注入引擎
    /// （文档 §5.3 方案 A：按 FRN 直接定位读 1 KB，复用同一套记录解析器）。
    pub fn poll(&mut self, engine: &mut MonitorEngine, tree: &mut Tree, now_ms: u64) -> io::Result<TickOutcome> {
        self.pull(engine, now_ms)?;

        // 游标持久化：每轮都写，掉电最多丢一轮
        let _ = self.cursor.save();

        let vol = &self.vol;
        let ntfs = self.ntfs.as_ref();
        let sector = vol.sector_size() as usize;

        let probe = |frn: u64| -> Option<(u64, u64, i64)> {
            let ntfs = ntfs?;
            let recno = recno_of(frn);
            if !ntfs.has_record(recno) {
                return None;
            }
            let mut rec = ntfs.read_record(vol, recno).ok()??;
            let r = core_ntfs::parse_record(&mut rec, recno, sector)?;
            Some((r.real_size, r.alloc_size, r.mtime))
        };

        Ok(engine.tick(tree, now_ms, probe))
    }

    pub fn flush(&mut self, engine: &mut MonitorEngine, tree: &mut Tree, now_ms: u64) -> io::Result<TickOutcome> {
        self.pull(engine, now_ms)?;
        let vol = &self.vol;
        let ntfs = self.ntfs.as_ref();
        let sector = vol.sector_size() as usize;
        let probe = |frn: u64| -> Option<(u64, u64, i64)> {
            let ntfs = ntfs?;
            let recno = recno_of(frn);
            if !ntfs.has_record(recno) {
                return None;
            }
            let mut rec = ntfs.read_record(vol, recno).ok()??;
            let r = core_ntfs::parse_record(&mut rec, recno, sector)?;
            Some((r.real_size, r.alloc_size, r.mtime))
        };
        let out = engine.flush(tree, now_ms, probe);
        let _ = self.cursor.save();
        Ok(out)
    }

    /// 日志剩余空间比例；过低时应提示用户（文档 R4）。
    pub fn journal_free_ratio(&self) -> f64 {
        self.journal.free_ratio()
    }

    pub fn save_cursor(&self) -> io::Result<()> {
        self.cursor.save()
    }
}

impl std::fmt::Debug for IncrementalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalSession")
            .field("drive", &self.vol.drive())
            .field("journal_id", &self.journal.id)
            .field("next_usn", &self.cursor.next_usn)
            .field("records_read", &self.records_read)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recno_extraction() {
        assert_eq!(recno_of(0xFFFF_0000_0000_1234), 0x1234);
        assert_eq!(recno_of(0x1234), 0x1234);
    }
}
