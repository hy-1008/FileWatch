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
use core_usn::normalize::EventKind;
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

/// 这条变更的「体积」该怎么说。
///
/// 说「说法」而不是「数据」：数字本身在 `PendingEvent` 里都有
/// （`size_delta` / `new_size`），这里定的是**怎么念给人听**。四条规则各有理由：
///
/// * **目录不念体积** —— 目录大小是子孙聚合值，逐条事件里的 `new_size`
///   对它没有意义，念出来只会误导；
/// * **删除念成负的原大小** —— 此时 `size_delta` 通常是 0（文件已经没了、回读
///   不到），唯一能反映"少了多少"的是事件里带上来的 `new_size`；
/// * **修改念增量**（带 `+`/`-`）—— 用户关心"变了多少"，不是"现在多大"；
/// * **其余念新大小**。
///
/// 放在引擎里而不是各前端各写一遍：CLI 要对齐成列、界面要拼 HTML，但
/// **「该显示哪个数」必须只有一个答案** —— 否则同一条变更在终端和界面上
/// 显示的数字不一样，用户没法判断该信哪个。
pub fn event_size_text(ev: &PendingEvent) -> String {
    if ev.is_dir {
        return String::new();
    }
    if ev.kind == EventKind::Deleted {
        let gone = if ev.size_delta < 0 {
            (-ev.size_delta) as u64
        } else {
            ev.new_size
        };
        return format!("-{}", core_io::util::human_size(gone));
    }
    if ev.size_delta > 0 {
        format!("+{}", core_io::util::human_size(ev.size_delta as u64))
    } else if ev.size_delta < 0 {
        format!("-{}", core_io::util::human_size((-ev.size_delta) as u64))
    } else {
        core_io::util::human_size(ev.new_size)
    }
}

/// 变更行右侧的补充说明：合并条数、重命名前的旧名。
///
/// 与 [`event_size_text`] 同属「说法」，所以也放这里 —— 两个前端各自排版，
/// 但内容一致。
pub fn event_suffix(ev: &PendingEvent) -> Option<String> {
    let mut s = String::new();
    if ev.merged > 1 {
        s.push_str(&format!("合并 {} 条", ev.merged));
    }
    if let (Some(old), EventKind::Renamed) = (ev.old_name.as_ref(), ev.kind) {
        if !s.is_empty() {
            s.push_str("　");
        }
        s.push_str(&format!("原名 {old}"));
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let owned = core_io::test_dir::TestDir::new("fw-changelog-test").unwrap();
        let dir = owned.path().to_path_buf();
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

    }

    #[test]
    fn rotation_keeps_one_generation() {
        let owned = core_io::test_dir::TestDir::new("fw-rotate-test").unwrap();
        let dir = owned.path().to_path_buf();
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

    }

    #[test]
    fn append_creates_parent_dir() {
        let owned = core_io::test_dir::TestDir::new("fw-mkdir-test").unwrap();
        let dir = owned.path().to_path_buf();
        let path = dir.join("nested").join("deep").join("log.jsonl");
        let mut log = ChangeLog::open(&path).unwrap();
        log.append(&ev(EventKind::Created), "p").unwrap();
        log.flush().unwrap();
        assert!(path.exists());
    }

    /// 体积说法四条规则。这些数字会**同时**出现在 CLI 的变更行和界面的
    /// 流水表里，所以它不该是"各自实现一遍"的东西。
    #[test]
    fn size_text_follows_the_four_rules() {
        // ① 目录不念体积
        let mut d = ev(EventKind::Created);
        d.is_dir = true;
        d.new_size = 12345;
        assert_eq!(event_size_text(&d), "");

        // ② 删除念负的原大小（此时 size_delta 是 0）
        let mut del = ev(EventKind::Deleted);
        del.new_size = 4096;
        del.size_delta = 0;
        assert_eq!(event_size_text(&del), "-4.00 KB");

        // 删除时若回读到了负的 delta，就用它（两个来源取得到的那个）
        let mut del2 = ev(EventKind::Deleted);
        del2.size_delta = -8192;
        assert_eq!(event_size_text(&del2), "-8.00 KB");

        // ③ 修改念增量（带符号）
        let mut grow = ev(EventKind::Modified);
        grow.size_delta = 2048;
        assert_eq!(event_size_text(&grow), "+2.00 KB");
        let mut shrink = ev(EventKind::Modified);
        shrink.size_delta = -1024;
        assert_eq!(event_size_text(&shrink), "-1.00 KB");

        // ④ 其余念新大小
        let mut fresh = ev(EventKind::Created);
        fresh.size_delta = 0;
        fresh.new_size = 1024 * 1024;
        assert_eq!(event_size_text(&fresh), "1.00 MB");
    }

    #[test]
    fn suffix_reports_merge_count_and_old_name() {
        // 默认夹具是 merged=2
        assert_eq!(event_suffix(&ev(EventKind::Created)), Some("合并 2 条".into()));

        let mut one = ev(EventKind::Created);
        one.merged = 1;
        assert_eq!(event_suffix(&one), None);

        let mut ren = ev(EventKind::Renamed);
        ren.merged = 1;
        ren.old_name = Some("old.txt".into());
        assert_eq!(event_suffix(&ren), Some("原名 old.txt".into()));

        // 两者都有时要并列，不能只显示一个
        ren.merged = 3;
        let s = event_suffix(&ren).unwrap();
        assert!(s.contains("合并 3 条") && s.contains("old.txt"), "实际 {s}");
    }
}
