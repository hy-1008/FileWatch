//! USN 游标持久化与断层检测（文档坑 #2 / 风险 R4）。
//!
//! USN Journal 是**环形缓冲**：写满后从头覆盖，旧的 USN 会失效。
//! 因此必须持久化 `(JournalID, NextUsn)`，启动时用它判断能否续跑增量：
//!
//! | 判定 | 条件 | 处理 |
//! |---|---|---|
//! | `Fresh` | 没有游标文件 | 首次运行 → 全量扫描 |
//! | `Valid` | JournalID 一致且 `NextUsn ∈ [FirstUsn, NextUsn]` | 直接续跑增量 |
//! | `JournalRebuilt` | JournalID 变了 | 全量重扫 + 重建游标 |
//! | `GapDetected` | `cursor.NextUsn < FirstUsn` | 日志已被覆盖 → 全量重扫 |
//! | `CursorAhead` | `cursor.NextUsn > journal.NextUsn` | 卷被回滚/日志重建 → 全量重扫 |

use crate::journal::UsnJournalData;
use core_io::jsonlite;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub volume: String,
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorValidity {
    /// 没有游标（首次运行）
    Fresh,
    /// 可以安全续跑增量
    Valid,
    /// 日志被重建（JournalID 变化）
    JournalRebuilt,
    /// 游标落后于 FirstUsn，中间事件已丢失
    GapDetected,
    /// 游标比日志头还新（不可能是正常状态）
    CursorAhead,
}

impl CursorValidity {
    pub fn can_resume(self) -> bool {
        matches!(self, CursorValidity::Valid)
    }

    pub fn reason_cn(self) -> &'static str {
        match self {
            CursorValidity::Fresh => "首次运行，无历史游标",
            CursorValidity::Valid => "游标有效，续跑增量",
            CursorValidity::JournalRebuilt => "USN Journal 已重建（JournalID 变化），需全量重扫",
            CursorValidity::GapDetected => "日志环形缓冲已覆盖旧游标范围，需全量重扫",
            CursorValidity::CursorAhead => "游标超前于日志头（卷被回滚或日志被删），需全量重扫",
        }
    }
}

/// 持久化目录：`%LOCALAPPDATA%\filewatch`，取不到则退到当前目录。
pub fn data_dir() -> PathBuf {
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        if !base.is_empty() {
            return Path::new(&base).join("filewatch");
        }
    }
    PathBuf::from(".filewatch")
}

fn sanitize(volume: &str) -> String {
    volume
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

impl Cursor {
    pub fn new(volume: &str, journal_id: u64, next_usn: i64) -> Cursor {
        Cursor {
            volume: volume.to_string(),
            journal_id,
            next_usn,
        }
    }

    pub fn path_for(volume: &str) -> PathBuf {
        data_dir().join(format!("cursor-{}.json", sanitize(volume)))
    }

    /// 读取游标。文件不存在 / 损坏一律返回 `None`（等价于首次运行）。
    pub fn load(volume: &str) -> Option<Cursor> {
        let p = Cursor::path_for(volume);
        let text = fs::read_to_string(&p).ok()?;
        let obj = jsonlite::parse_flat_object(&text);
        let journal_id = jsonlite::get_u64(&obj, "journal_id")?;
        let next_usn = jsonlite::get_i64(&obj, "next_usn")?;
        Some(Cursor {
            volume: jsonlite::get(&obj, "volume").unwrap_or(volume).to_string(),
            journal_id,
            next_usn,
        })
    }

    /// 原子写：先写 `.tmp` 再 rename，避免掉电留下半个文件。
    pub fn save(&self) -> io::Result<()> {
        let dir = data_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("cursor-{}.json", sanitize(&self.volume)));

        let mut s = String::with_capacity(128);
        s.push_str("{\n");
        s.push_str(&format!("  \"volume\": {},\n", jsonlite::quote(&self.volume)));
        s.push_str(&format!("  \"journal_id\": {},\n", self.journal_id));
        s.push_str(&format!("  \"next_usn\": {}\n", self.next_usn));
        s.push_str("}\n");

        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, s.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn clear(volume: &str) -> io::Result<()> {
        let p = Cursor::path_for(volume);
        if p.exists() {
            fs::remove_file(p)?;
        }
        Ok(())
    }

    /// 用当前日志状态重建游标（全量扫描完成后调用）。
    pub fn rebuild(volume: &str, journal: &UsnJournalData) -> Cursor {
        Cursor::new(volume, journal.id, journal.next_usn)
    }
}

/// 判断持久化游标能否用于续跑增量。
pub fn evaluate(cursor: Option<&Cursor>, current: &UsnJournalData) -> CursorValidity {
    let c = match cursor {
        None => return CursorValidity::Fresh,
        Some(c) => c,
    };
    if c.journal_id != current.id {
        return CursorValidity::JournalRebuilt;
    }
    if c.next_usn < current.first_usn {
        return CursorValidity::GapDetected;
    }
    if c.next_usn > current.next_usn {
        return CursorValidity::CursorAhead;
    }
    CursorValidity::Valid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(id: u64, first: i64, next: i64) -> UsnJournalData {
        UsnJournalData {
            id,
            first_usn: first,
            next_usn: next,
            lowest_valid_usn: first,
            max_usn: next,
            maximum_size: 32 * 1024 * 1024,
            allocation_delta: 4 * 1024 * 1024,
        }
    }

    #[test]
    fn fresh_when_no_cursor() {
        assert_eq!(evaluate(None, &journal(7, 0, 100)), CursorValidity::Fresh);
    }

    #[test]
    fn valid_within_range() {
        let c = Cursor::new("C:", 7, 50);
        assert_eq!(evaluate(Some(&c), &journal(7, 10, 100)), CursorValidity::Valid);
        // 边界：等于 FirstUsn 与 NextUsn 都应视为有效
        let c2 = Cursor::new("C:", 7, 10);
        assert_eq!(evaluate(Some(&c2), &journal(7, 10, 100)), CursorValidity::Valid);
        let c3 = Cursor::new("C:", 7, 100);
        assert_eq!(evaluate(Some(&c3), &journal(7, 10, 100)), CursorValidity::Valid);
    }

    #[test]
    fn detects_journal_rebuild() {
        let c = Cursor::new("C:", 7, 50);
        assert_eq!(
            evaluate(Some(&c), &journal(8, 0, 100)),
            CursorValidity::JournalRebuilt
        );
    }

    #[test]
    fn detects_gap() {
        let c = Cursor::new("C:", 7, 5);
        assert_eq!(
            evaluate(Some(&c), &journal(7, 10, 100)),
            CursorValidity::GapDetected
        );
    }

    #[test]
    fn detects_cursor_ahead() {
        let c = Cursor::new("C:", 7, 500);
        assert_eq!(
            evaluate(Some(&c), &journal(7, 10, 100)),
            CursorValidity::CursorAhead
        );
    }

    #[test]
    fn resume_flag_only_for_valid() {
        assert!(CursorValidity::Valid.can_resume());
        assert!(!CursorValidity::GapDetected.can_resume());
        assert!(!CursorValidity::Fresh.can_resume());
    }

    #[test]
    fn path_is_per_volume() {
        let a = Cursor::path_for("C:");
        let b = Cursor::path_for("D:");
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("cursor-C_"));
        assert!(b.to_string_lossy().contains("cursor-D_"));
    }

    #[test]
    fn json_roundtrip_via_parser() {
        // 模拟 save 写完后的内容能被 load 的解析路径读回
        let text = "{\n  \"volume\": \"C:\",\n  \"journal_id\": 123,\n  \"next_usn\": 456\n}\n";
        let obj = jsonlite::parse_flat_object(text);
        assert_eq!(jsonlite::get(&obj, "volume"), Some("C:"));
        assert_eq!(jsonlite::get_u64(&obj, "journal_id"), Some(123));
        assert_eq!(jsonlite::get_i64(&obj, "next_usn"), Some(456));
    }
}
