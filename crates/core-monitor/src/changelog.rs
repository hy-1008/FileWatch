//! 变更流水落库（功能 F8，文档 §5.4「变更流水日志（可导出 CSV）」）。
//!
//! 采用 **JSONL**（每行一个 JSON 对象）而不是 SQLite：
//! * 零依赖（workspace 不引入 `rusqlite`，见 Cargo.toml 的说明）；
//! * 追加写，不需要事务，掉电最多丢最后一行；
//! * 天然可流式 tail，CSV 导出只是换个格式化函数。
//!
//! 超过 `max_bytes` 自动轮转：`changelog.jsonl` → `changelog.1.jsonl`。

use crate::debounce::PendingEvent;
use core_io::jsonlite;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub struct ChangeLog {
    path: PathBuf,
    writer: BufWriter<File>,
    pub written: u64,
    pub bytes_written: u64,
    pub max_bytes: u64,
}

impl ChangeLog {
    /// 默认路径：`%LOCALAPPDATA%\filewatch\changelog.jsonl`
    pub fn default_path() -> PathBuf {
        core_usn::cursor::data_dir().join("changelog.jsonl")
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<ChangeLog> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(ChangeLog {
            path,
            writer: BufWriter::with_capacity(64 * 1024, file),
            written: 0,
            bytes_written,
            max_bytes: 64 * 1024 * 1024, // 64 MB
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条变更记录。`full_path` 由调用方用索引树拼出。
    pub fn append(&mut self, ev: &PendingEvent, full_path: &str) -> std::io::Result<()> {
        let line = format_event_line(ev, full_path);
        self.writer.write_all(line.as_bytes())?;
        self.written += 1;
        self.bytes_written += line.len() as u64;
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    /// 达到容量上限时轮转。返回是否发生了轮转。
    pub fn maybe_rotate(&mut self) -> std::io::Result<bool> {
        if self.bytes_written < self.max_bytes {
            return Ok(false);
        }
        self.writer.flush()?;
        let rotated = self.path.with_extension("1.jsonl");
        // 覆盖式轮转：只保留一代历史
        let _ = fs::remove_file(&rotated);
        fs::rename(&self.path, &rotated)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.writer = BufWriter::with_capacity(64 * 1024, file);
        self.bytes_written = 0;
        Ok(true)
    }
}

/// 生成一行 JSONL（不写盘，便于测试与 CSV 复用）。
pub fn format_event_line(ev: &PendingEvent, full_path: &str) -> String {
    let mut s = String::with_capacity(256);
    s.push('{');
    s.push_str(&format!("\"kind\":{},", jsonlite::quote(ev.kind.as_str())));
    s.push_str(&format!("\"frn\":{},", ev.frn));
    s.push_str(&format!("\"parent\":{},", ev.parent_frn));
    s.push_str(&format!("\"name\":{},", jsonlite::quote(&ev.name)));
    s.push_str(&format!("\"path\":{},", jsonlite::quote(full_path)));
    s.push_str(&format!("\"is_dir\":{},", ev.is_dir));
    s.push_str(&format!("\"size\":{},", ev.new_size));
    s.push_str(&format!("\"alloc\":{},", ev.new_alloc));
    s.push_str(&format!("\"delta\":{},", ev.size_delta));
    s.push_str(&format!("\"merged\":{},", ev.merged));
    if let Some(old) = &ev.old_name {
        s.push_str(&format!("\"old_name\":{},", jsonlite::quote(old)));
    }
    s.push_str(&format!("\"usn\":{},", ev.usn));
    s.push_str(&format!("\"t_ms\":{}", ev.last_ms));
    s.push('}');
    s.push('\n');
    s
}

/// CSV 表头（功能 F8「可导出 CSV」）。
pub const CSV_HEADER: &str = "time_ms,kind,is_dir,size,alloc,delta,frn,parent,name,path,old_name";

/// 生成一行 CSV。
pub fn format_event_csv(ev: &PendingEvent, full_path: &str) -> String {
    fn esc(s: &str) -> String {
        if s.contains(',') || s.contains('"') || s.contains('\n') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }
    format!(
        "{},{},{},{},{},{},{},{},{},{},{}",
        ev.last_ms,
        ev.kind.as_str(),
        ev.is_dir,
        ev.new_size,
        ev.new_alloc,
        ev.size_delta,
        ev.frn,
        ev.parent_frn,
        esc(&ev.name),
        esc(full_path),
        esc(ev.old_name.as_deref().unwrap_or(""))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_usn::normalize::EventKind;

    fn ev(kind: EventKind) -> PendingEvent {
        PendingEvent {
            frn: 42,
            parent_frn: 5,
            kind,
            name: "a.txt".into(),
            old_name: None,
            is_dir: false,
            first_ms: 1000,
            last_ms: 1500,
            merged: 2,
            usn: 999,
            reason: 0x100,
            size_delta: 500,
            alloc_delta: 4096,
            new_size: 1500,
            new_alloc: 4096,
        }
    }

    #[test]
    fn jsonl_line_is_valid_flat_object() {
        let line = format_event_line(&ev(EventKind::Created), "C:\\a.txt");
        assert!(line.ends_with('\n'));
        let obj = jsonlite::parse_flat_object(&line);
        assert_eq!(jsonlite::get(&obj, "kind"), Some("created"));
        assert_eq!(jsonlite::get(&obj, "frn"), Some("42"));
        assert_eq!(jsonlite::get(&obj, "path"), Some("C:\\a.txt"));
        assert_eq!(jsonlite::get_u64(&obj, "delta"), Some(500));
        assert_eq!(jsonlite::get(&obj, "is_dir"), Some("false"));
    }

    #[test]
    fn jsonl_escapes_windows_paths() {
        let line = format_event_line(&ev(EventKind::Modified), "C:\\a\\\"weird\"\\b.txt");
        let obj = jsonlite::parse_flat_object(&line);
        assert_eq!(jsonlite::get(&obj, "path"), Some("C:\\a\\\"weird\"\\b.txt"));
    }

    #[test]
    fn rename_includes_old_name() {
        let mut e = ev(EventKind::Renamed);
        e.old_name = Some("old.txt".into());
        e.name = "new.txt".into();
        let line = format_event_line(&e, "C:\\new.txt");
        let obj = jsonlite::parse_flat_object(&line);
        assert_eq!(jsonlite::get(&obj, "old_name"), Some("old.txt"));
    }

    #[test]
    fn csv_escapes_commas_and_quotes() {
        let mut e = ev(EventKind::Created);
        e.name = "a,b\"c.txt".into();
        let line = format_event_csv(&e, "C:\\a,b.txt");
        // 含逗号 / 引号的字段必须**整段**加引号，内部引号翻倍。
        // 注意 path 列存的是整条路径，所以引号在 `C:\` 之前，而不是在 `a` 之前
        // —— 写成 `"a,b.txt"` 是错的（那要求 `a` 前面就是引号）。
        assert!(line.contains("\"C:\\a,b.txt\""), "line={}", line);
        assert!(line.contains("\"a,b\"\"c.txt\""), "line={}", line);
    }

    #[test]
    fn csv_header_matches_columns() {
        let cols = CSV_HEADER.split(',').count();
        let line = format_event_csv(&ev(EventKind::Deleted), "C:\\a.txt");
        // 简单计数：没有需要转义的字段时列数应一致
        assert_eq!(line.split(',').count(), cols);
    }

    #[test]
    fn writes_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("fw_changelog_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("changelog.jsonl");

        {
            let mut log = ChangeLog::open(&path).unwrap();
            log.append(&ev(EventKind::Created), "C:\\a.txt").unwrap();
            log.append(&ev(EventKind::Deleted), "C:\\b.txt").unwrap();
            log.flush().unwrap();
            assert_eq!(log.written, 2);
        }

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            jsonlite::get(&jsonlite::parse_flat_object(lines[0]), "kind"),
            Some("created")
        );
        assert_eq!(
            jsonlite::get(&jsonlite::parse_flat_object(lines[1]), "kind"),
            Some("deleted")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_one_generation() {
        let dir = std::env::temp_dir().join(format!("fw_rotate_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("changelog.jsonl");

        let mut log = ChangeLog::open(&path).unwrap();
        log.max_bytes = 10; // 极小，保证立刻轮转
        log.append(&ev(EventKind::Created), "C:\\a.txt").unwrap();
        log.flush().unwrap();
        assert!(log.maybe_rotate().unwrap());
        assert!(path.exists());
        assert!(path.with_extension("1.jsonl").exists());
        log.append(&ev(EventKind::Deleted), "C:\\b.txt").unwrap();
        log.flush().unwrap();

        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("deleted"));
        let rotated = fs::read_to_string(path.with_extension("1.jsonl")).unwrap();
        assert!(rotated.contains("created"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_creates_parent_dir() {
        let dir = std::env::temp_dir().join(format!("fw_mkdir_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("deep").join("log.jsonl");
        let mut log = ChangeLog::open(&path).unwrap();
        log.append(&ev(EventKind::Created), "p").unwrap();
        log.flush().unwrap();
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
