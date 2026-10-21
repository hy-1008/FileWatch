//! 增长归因：把「一堆变化行」压成「谁吃掉了空间」。
//!
//! `diff` 的产出是**文件级**的：几万条变化按增量排序，前几名往往是一堆
//! `pagefile.sys`、日志分片、浏览器缓存 —— 每个文件都对，但答不上人真正
//! 在问的那句话："这一周 50 GB 长在**哪儿**？"
//!
//! 所以这里把同一批变化行按三个维度重聚合：
//!
//! | 榜 | 回答的问题 |
//! |---|---|
//! | 目录（前 N 层） | 50 GB 长在哪个目录下 |
//! | 扩展名 | 是视频、日志还是数据库 |
//! | 时段（按小时） | 什么时候长的，是不是一次性的批量写入 |
//!
//! # 为什么必须在 `diff_with` 里面算
//!
//! 因为 [`DiffReport::rows`] 是**截断后**的（界面默认 300 行）。归因要的是全量，
//! 而全量只在归并过程中存在一次 —— 放到外面算就得再跑一遍 diff，或者要求调用方
//! 把 `top` 设成 0（两百万行会占几百 MB）。所以归因在 `all` 还没被截断、也还没
//! 重排的时候算完，挂在 [`DiffReport::attribution`] 上。
//!
//! # 两个如实呈现的口径
//!
//! 1. **被删除的文件没有"什么时候删的"这个信息**。快照只记 mtime，所以删除行
//!    进的是"文件最后修改时间"那个时段 —— 时段榜因此只能读成"这些变化对应的
//!    文件是什么时候写下的"，不能读成"删除发生在几点"。
//! 2. **目录表按 `dir_depth` 截断**（默认 3 层）。这不是为了省内存，是为了
//!    "看前十名"：不截断的话每个文件的直接父目录都各占一行，榜就没有信息量了。
//!
//! [`DiffReport::rows`]: crate::snapshot::DiffReport::rows
//! [`DiffReport::attribution`]: crate::snapshot::DiffReport::attribution

use crate::snapshot::{DiffKind, DiffRow};
use core_index::tree::extension_of;
use core_io::paths::{file_name, root_prefix, strip_root_prefix, to_native, SEP_STR};
use std::collections::HashMap;

/// 变化最集中的那几个小时，最多列这么多条。
pub const BURST_TOP: usize = 12;

/// 归因参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttrOptions {
    /// 目录聚合到第几层（相对快照根）。1 = 根的直接子目录。
    ///
    /// 默认 3：整卷扫描时，`C:\Users\HY\AppData` 这个粒度才有信息量；
    /// 1 层的话前几名永远是 `Windows` / `Users` / `Program Files`。
    pub dir_depth: u32,
    /// 每个榜最多多少条。
    pub top: usize,
}

impl Default for AttrOptions {
    fn default() -> Self {
        AttrOptions {
            dir_depth: 3,
            top: 20,
        }
    }
}

/// 一个目录的净变化。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirDelta {
    /// 聚合后的目录路径（显示路径，与快照里的写法一致）
    pub dir: String,
    /// 净变化（新增 + 变大 − 删除 − 变小）。负数表示这个目录整体变小了。
    pub delta: i64,
    pub added: u64,
    pub removed: u64,
    pub grew: u64,
    pub shrank: u64,
    /// 变化过的文件数（不是目录里的文件总数）
    pub files: u32,
}

/// 一个扩展名的净变化。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtDelta {
    /// 小写扩展名，不含点；没有扩展名的文件归到 `""`
    pub ext: String,
    pub delta: i64,
    pub added: u64,
    pub removed: u64,
    pub files: u32,
}

/// 一个小时的净变化。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Burst {
    /// 整点时间戳（Unix 秒，UTC 小时边界）
    pub hour_unix: i64,
    pub delta: i64,
    pub files: u32,
}

/// 归因结果。
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    /// 快照的扫描根（用来解释"这几个目录名是相对谁说的"）
    pub root: String,
    pub dir_depth: u32,
    /// 全部变化行的净增量之和（= `DiffReport::net_delta`，这里再算一份是为了
    /// 让归因结果单独拿出来也有意义，不需要调用方配对读）
    pub total_delta: i64,
    /// 变化过的文件数
    pub files: u64,
    /// 增长榜：`delta > 0`，降序，最多 `top` 条
    pub dirs_up: Vec<DirDelta>,
    /// 减少榜：`delta < 0`，最负的在前，最多 `top` 条
    pub dirs_down: Vec<DirDelta>,
    /// 变化过的目录总数（含没列出来的）—— 界面用它说"还有 N 个目录"
    pub dirs_total: u32,
    /// 扩展名榜：按 delta 降序，**不截断**（现实中通常几十个，本身就短，
    /// 截断反而会让"其他"这一类没法算）
    pub exts: Vec<ExtDelta>,
    /// 时段榜：按 |delta| 降序，最多 [`BURST_TOP`] 条
    pub bursts: Vec<Burst>,
}

impl Attribution {
    /// 增长榜里最大的那一项（界面用来写一句人话总结）。
    pub fn top_dir(&self) -> Option<&DirDelta> {
        self.dirs_up.first()
    }
}

/// 把一条变化行累加进某个目录。三个维度共用这一处，避免三份各自漏掉某种 `kind`。
fn add_dir(e: &mut DirDelta, r: &DiffRow) {
    e.delta += r.delta;
    e.files += 1;
    match r.kind {
        DiffKind::Added => e.added += r.new_size,
        DiffKind::Removed => e.removed += r.old_size,
        DiffKind::Grew => e.grew += r.delta.unsigned_abs(),
        DiffKind::Shrank => e.shrank += r.delta.unsigned_abs(),
    }
}

fn add_ext(e: &mut ExtDelta, r: &DiffRow) {
    e.delta += r.delta;
    e.files += 1;
    match r.kind {
        DiffKind::Added => e.added += r.new_size,
        DiffKind::Removed => e.removed += r.old_size,
        DiffKind::Grew | DiffKind::Shrank => {}
    }
}

/// 一条变化行属于哪个目录（写到 `buf` 里，返回是否命中）。
///
/// **不分配**：这个函数在归因里每条变化行都要调一次，百万行级别时
/// "每行 new 一个 String" 本身就是主要开销。命中判断用调用方给的缓冲。
pub fn dir_of_into(buf: &mut String, path: &str, root: &str, depth: u32) -> bool {
    buf.clear();
    if depth == 0 {
        return false;
    }
    let rel = match strip_root_prefix(path, root) {
        Some(r) => r,
        None => return false,
    };
    // 先数段数再取前 n 段：n 取决于总段数（最后一段是文件名，不算目录），
    // 所以没法一遍拿到。split 是零分配的，多走一遍比先 collect 成 Vec 便宜。
    let total = rel.split(['\\', '/']).filter(|s| !s.is_empty()).count();
    if total == 0 {
        return false; // path 就是根本身
    }
    let n = (total - 1).min(depth as usize);
    buf.push_str(&root_prefix(root));
    for (i, seg) in rel
        .split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .take(n)
        .enumerate()
    {
        if i > 0 {
            buf.push_str(SEP_STR);
        }
        buf.push_str(seg);
    }
    true
}

/// 便利版：[`dir_of_into`] 的分配版，给测试与非热路径用。
pub fn dir_of(path: &str, root: &str, depth: u32) -> Option<String> {
    let mut buf = String::new();
    if dir_of_into(&mut buf, path, root, depth) {
        Some(buf)
    } else {
        None
    }
}

/// 对一批变化行做归因。
///
/// `rows` 必须是**全量**（未经 `top` 截断），否则归因只反映前几百行。
pub fn build(root: &str, rows: &[DiffRow], opts: &AttrOptions) -> Attribution {
    // 老快照（0.1.3 之前用命令行建的）里根可能是 `C:/a/b` 这种写法，
    // 而归因拿到的每行路径是 `C://a//b` —— 这里再归一一次，读旧文件也正确。
    let root = &to_native(root);
    let mut rep = Attribution {
        root: root.to_string(),
        dir_depth: opts.dir_depth,
        files: rows.len() as u64,
        ..Default::default()
    };

    let mut dirs: HashMap<String, DirDelta> = HashMap::new();
    let mut exts: HashMap<String, ExtDelta> = HashMap::new();
    let mut bursts: HashMap<i64, Burst> = HashMap::new();

    // 两个复用缓冲：见 `dir_of_into` 的注释。
    let mut key = String::new();
    let mut low = String::new();

    for row in rows {
        rep.total_delta = rep.total_delta.saturating_add(row.delta);

        if dir_of_into(&mut key, &row.path, root, opts.dir_depth) {
            match dirs.get_mut(key.as_str()) {
                Some(e) => add_dir(e, row),
                None => {
                    let mut e = DirDelta::default();
                    e.dir.push_str(&key);
                    add_dir(&mut e, row);
                    dirs.insert(key.clone(), e);
                }
            }
        }

        // 扩展名一律折叠成小写：`JPG` 与 `jpg` 是同一类，否则榜会被大小写劈成两半。
        let ext = extension_of(file_name(&row.path));
        low.clear();
        low.push_str(ext);
        low.make_ascii_lowercase();
        match exts.get_mut(low.as_str()) {
            Some(e) => add_ext(e, row),
            None => {
                let mut e = ExtDelta::default();
                e.ext.push_str(&low);
                add_ext(&mut e, row);
                exts.insert(low.clone(), e);
            }
        }

        // 时段：新快照有 mtime 就用新的；删除行只有旧的（见模块注释的口径 1）。
        let t = if row.new_mtime != 0 {
            row.new_mtime
        } else {
            row.old_mtime
        };
        if t > 0 {
            let hour = t - t.rem_euclid(3600);
            let e = bursts.entry(hour).or_insert(Burst {
                hour_unix: hour,
                ..Default::default()
            });
            e.delta += row.delta;
            e.files += 1;
        }
    }

    let mut all_dirs: Vec<DirDelta> = dirs.into_values().collect();
    rep.dirs_total = all_dirs.len() as u32;
    // 降序：先按净增量，再按路径 —— 同分时顺序稳定，测试才能断言。
    all_dirs.sort_by(|a, b| b.delta.cmp(&a.delta).then_with(|| a.dir.cmp(&b.dir)));
    rep.dirs_up = all_dirs
        .iter()
        .filter(|d| d.delta > 0)
        .take(opts.top)
        .cloned()
        .collect();
    // 减少榜：最负的在前。不复用上一次排序 —— 那个是降序，取尾部还得再翻一次，
    // 不如直接按升序重排（目录数量在十万级以内，这一遍是毫秒级）。
    let mut down: Vec<DirDelta> = all_dirs.into_iter().filter(|d| d.delta < 0).collect();
    down.sort_by(|a, b| a.delta.cmp(&b.delta).then_with(|| a.dir.cmp(&b.dir)));
    down.truncate(opts.top);
    rep.dirs_down = down;

    let mut all_exts: Vec<ExtDelta> = exts.into_values().collect();
    all_exts.sort_by(|a, b| b.delta.cmp(&a.delta).then_with(|| a.ext.cmp(&b.ext)));
    rep.exts = all_exts;

    let mut all_bursts: Vec<Burst> = bursts.into_values().collect();
    all_bursts.sort_by(|a, b| {
        b.delta
            .unsigned_abs()
            .cmp(&a.delta.unsigned_abs())
            .then_with(|| a.hour_unix.cmp(&b.hour_unix))
    });
    all_bursts.truncate(BURST_TOP);
    rep.bursts = all_bursts;

    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_io::paths::{join, DEFAULT_VOLUME};

    fn row(path: &str, kind: DiffKind, old_size: u64, new_size: u64, mtime: i64) -> DiffRow {
        DiffRow {
            path: path.to_string(),
            kind,
            old_size,
            new_size,
            delta: new_size as i64 - old_size as i64,
            old_mtime: mtime,
            new_mtime: mtime,
        }
    }

    #[test]
    fn dir_of_takes_depth_components_below_root() {
        let root = root_prefix(DEFAULT_VOLUME);
        let p = join(&join(&join(&join(&root, "Users"), "HY"), "Documents"), "a.txt");
        // 3 层 = 根下三个分量（文件名不算）
        assert_eq!(dir_of(&p, &root, 3), Some(join(&join(&join(&root, "Users"), "HY"), "Documents")));
        assert_eq!(dir_of(&p, &root, 1), Some(join(&root, "Users")));
        // 文件直接在根下：目录就是根本身（带尾分隔符）
        let top = join(&root, "boot.ini");
        assert_eq!(dir_of(&top, &root, 3), Some(root.clone()));
        // 深度 0 / 根外路径：不归因，而不是塞进一个空名字的桶
        assert_eq!(dir_of(&p, &root, 0), None);
        assert_eq!(dir_of(&p, &join(&root, "Nowhere"), 3), None);
    }

    #[test]
    fn build_separates_growth_from_shrink() {
        let root = root_prefix(DEFAULT_VOLUME);
        let big = join(&root, "big");
        let small = join(&root, "small");
        let rows = vec![
            row(&join(&big, "a.mp4"), DiffKind::Added, 0, 3_000, 7_200),
            row(&join(&big, "b.mp4"), DiffKind::Grew, 1_000, 2_000, 7_300),
            row(&join(&small, "c.log"), DiffKind::Removed, 500, 0, 7_400),
            row(&join(&small, "d.log"), DiffKind::Shrank, 900, 400, 7_500),
        ];
        let a = build(&root, &rows, &AttrOptions::default());

        assert_eq!(a.files, 4);
        assert_eq!(a.total_delta, 3_000 + 1_000 - 500 - 500);
        // 增长榜只有 big，净 +4000
        assert_eq!(a.dirs_up.len(), 1);
        assert_eq!(a.dirs_up[0].dir, big);
        assert_eq!(a.dirs_up[0].delta, 4_000);
        assert_eq!(a.dirs_up[0].added, 3_000);
        assert_eq!(a.dirs_up[0].grew, 1_000);
        assert_eq!(a.dirs_up[0].files, 2);
        // 减少榜只有 small，净 −1000
        assert_eq!(a.dirs_down.len(), 1);
        assert_eq!(a.dirs_down[0].dir, small);
        assert_eq!(a.dirs_down[0].delta, -1_000);
        assert_eq!(a.dirs_down[0].removed, 500);
        assert_eq!(a.dirs_down[0].shrank, 500);
        // 目录总数包含两个（不是"列出来的行数"）
        assert_eq!(a.dirs_total, 2);
    }

    #[test]
    fn build_folds_extension_case_and_keeps_no_ext() {
        let root = root_prefix(DEFAULT_VOLUME);
        let rows = vec![
            row(&join(&root, "A.MP4"), DiffKind::Added, 0, 100, 3_600),
            row(&join(&root, "b.mp4"), DiffKind::Added, 0, 50, 3_600),
            row(&join(&root, "Makefile"), DiffKind::Added, 0, 10, 3_600),
        ];
        let a = build(&root, &rows, &AttrOptions::default());
        let mp4 = a.exts.iter().find(|e| e.ext == "mp4").expect("大小写要合并成一个桶");
        assert_eq!(mp4.files, 2);
        assert_eq!(mp4.delta, 150);
        assert!(a.exts.iter().any(|e| e.ext.is_empty() && e.files == 1), "无扩展名要留一个空串桶");
    }

    #[test]
    fn build_truncates_each_board_but_keeps_total_count() {
        let root = root_prefix(DEFAULT_VOLUME);
        let mut rows = Vec::new();
        for i in 0..30u32 {
            let d = join(&root, &format!("d{i:02}"));
            rows.push(row(&join(&d, "x.bin"), DiffKind::Added, 0, 100 + i as u64, 3_600));
        }
        let opts = AttrOptions {
            dir_depth: 3,
            top: 5,
        };
        let a = build(&root, &rows, &opts);
        assert_eq!(a.dirs_up.len(), 5, "只列前 5 个");
        assert_eq!(a.dirs_total, 30, "但总数要如实报");
        // 第一名必须是增量最大的那个
        assert!(a.dirs_up[0].dir.ends_with("d29"), "{}", a.dirs_up[0].dir);
        // 扩展名榜不截断
        assert_eq!(a.exts.len(), 1);
    }

    /// 扫描根写成 `C:/a/b`（命令行常见）而归因拿到的路径是 `C:\a\b\x.bin`：
    /// 两者必须能对上，否则目录榜整列空掉且不报错。
    #[cfg(windows)]
    #[test]
    fn build_accepts_a_root_with_foreign_separators() {
        let rows = vec![row("C:\\a\\b\\x.bin", DiffKind::Added, 0, 100, 3_600)];
        let a = build("C:/a/b", &rows, &AttrOptions::default());
        assert_eq!(a.dirs_up.len(), 1, "根与路径的分隔符不同也要能切出目录");
        // 文件直接落在扫描根下 → 桶名就是根本身，按项目口径带尾分隔符
        // （与盘根 `C:\\` 的写法一致）
        assert_eq!(a.dirs_up[0].dir, "C:\\a\\b\\");
        assert_eq!(a.files, 1);
    }

    #[test]
    fn bursts_bucket_by_hour() {
        let root = root_prefix(DEFAULT_VOLUME);
        let p = join(&root, "a.log");
        let rows = vec![
            row(&p, DiffKind::Grew, 0, 100, 3_600),
            row(&p, DiffKind::Grew, 0, 200, 3_601),
            row(&p, DiffKind::Grew, 0, 300, 7_200),
        ];
        let a = build(&root, &rows, &AttrOptions::default());
        assert_eq!(a.bursts.len(), 2);
        // 3_601 与 3_600 同属 3_600 那个小时 → 合并成 300
        let first = a.bursts.iter().find(|b| b.hour_unix == 3_600).unwrap();
        assert_eq!(first.delta, 300);
        assert_eq!(first.files, 2);
    }

    #[test]
    fn rows_outside_the_root_are_skipped_not_bucketed_under_a_fake_dir() {
        let root = root_prefix(DEFAULT_VOLUME);
        // 扫描根取一个子目录：这样"根外"在 Windows（盘根）与 Linux（/）上都成立。
        // 直接拿卷根做根的话，Linux 上任何路径都在 `/` 底下，这个断言就永远为真了。
        let scope = join(&root, "scope");
        let outside = join(&join(&root, "elsewhere"), "x.bin");
        let rows = vec![row(&outside, DiffKind::Added, 0, 100, 3_600)];
        let a = build(&scope, &rows, &AttrOptions::default());
        assert_eq!(a.files, 1, "行数照样计入总数");
        assert!(a.dirs_up.is_empty() && a.dirs_down.is_empty(), "但不该凭空造出一个目录");
        assert_eq!(a.total_delta, 100);
    }
}
