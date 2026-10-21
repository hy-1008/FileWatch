//! 目录级历史（E6）：回答"这块盘这一周在长吗，今天长得正常不正常"。
//!
//! # 为什么不直接每天存一份快照
//!
//! 快照是**文件级**的（200 万行、上百 MB）。每天一份、留 30 天就是几个 GB ——
//! 而"这块盘在长吗"根本不需要文件级精度。所以历史单独一个小格式：只记
//! **目录聚合**（每个样本最多 [`MAX_DIRS`] 个目录行），一个月不到 1 MB，
//! 而且能直接 `cat` 出来看。
//!
//! # 分桶口径：按"文件的第 N 层祖先"
//!
//! 不是按"相对扫描根的层数"。命令行只扫 `C:\proj` 时，树里只有 `proj` 以下的
//! 节点，按相对层数算的话 `C:\proj\src` 会被记成"第 1 层"，与整卷扫描时的
//! "`C:\proj\src` 是第 3 层"对不上 —— 同一天里一次手动扫子目录、一次后台扫整卷，
//! 趋势图就会出现两条同名不同值的线。
//!
//! 从文件自身往上数 N 跳，两边就一致了：`C:\proj\src\x.rs` 的第 3 层祖先
//! 永远是 `C:\proj`，跟从哪儿开始扫无关。
//!
//! # 基线为什么按"天"而不是"同一小时"
//!
//! 采样间隔是可变的（用户会改周期、会关机、会手动点"记录一次"）。按小时比，
//! 会把"这次样本隔了 3 小时"误读成"这 3 小时涨得特别凶"。日粒度只要求
//! "一天里至少有一个样本"，而这正是周期任务能保证的。
//!
//! 于是：日增量 = 当天最后一个样本 − 上一个有样本的日子的大小，按**间隔天数**
//! 摊平成"每天涨多少"，再与过去 `lookback` 天的日增量比 z 值。
//!
//! 不足 [`MIN_BASELINE_DAYS`] 天时**不算 z**（`z: None`），界面显示"还需要 N 天
//! 基线"。拿一天的数据算 z 值，只是一个更精致的猜测 —— 与规则试算里
//! `judgeable=false` 是同一条原则。

use crate::snapshot::{escape, unescape};
use core_index::arena::INVALID_PARENT;
use core_index::Tree;
use core_io::util::local_days;
use core_monitor::PathRemap;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// 文件魔数。认不出就报错 —— 历史是增量写的，把别的文本当历史解析
/// 会得到一份"从没长过"的结论，比报错糟得多。
pub const MAGIC: &str = "#filewatch-history";
/// 格式版本。改字段就进位。
pub const VERSION: u32 = 1;

/// 每个样本最多记多少个目录（按大小降序取）。
///
/// 300 行的样本约 20 KB；留 400 个样本 ≈ 8 MB。再大就没意义了：
/// "哪个目录在长"看的是前几十名，排在 300 名之后的目录涨 1 MB 也轮不到它。
pub const MAX_DIRS: usize = 300;
/// 保留多少个样本（按日采样约等于一年）。
pub const MAX_SAMPLES: usize = 400;
/// 少于这么多天的基线就不给 z 值。
pub const MIN_BASELINE_DAYS: u32 = 3;
/// z 值到多少才算"异常"。
pub const Z_LIMIT: f64 = 2.0;
/// z 的显示上限。
///
/// σ 有下限（[`SIGMA_FLOOR`]），所以基线极稳时 z 会算出 850 这种数字 ——
/// 数学上没错，但"850 和 900 有什么区别"没有意义，只会让人以为程序在胡说。
/// 超过这个值统一显示成 99.9，意思是"远超基线"。
pub const Z_CAP: f64 = 99.9;
/// 默认回看多少天。
pub const DEFAULT_LOOKBACK: u32 = 7;
/// 标准差的下限（1 MiB）。日增量天然有噪声，σ 太小时 z 会变成天文数字
/// （0.1 MB 的 σ 配 10 MB 的日增量就是 z=100），那种数字没人敢信。
pub const SIGMA_FLOOR: f64 = 1024.0 * 1024.0;

/// 一个目录在某次采样里的大小。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPoint {
    pub path: String,
    pub size: u64,
    pub files: u64,
}

/// 一次采样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub at_unix: i64,
    /// 整棵树的总大小（含没进 `dirs` 的那些目录）
    pub total: u64,
    pub files: u64,
    /// 有多少个桶（`dirs` 可能被 [`MAX_DIRS`] 截断）
    pub dirs_total: usize,
    /// 按大小降序
    pub dirs: Vec<DirPoint>,
}

/// 一份历史。
#[derive(Debug, Clone, Default)]
pub struct History {
    /// 扫描根（显示路径）
    pub root: String,
    /// 按时间升序
    pub samples: Vec<Sample>,
    /// 读取时因为计数对不上而丢掉的样本数（只可能有 0 或 1：
    /// 崩溃在写入途中只会留下最后一个不完整样本）
    pub dropped: u32,
}

/// 一条基线异常。
#[derive(Debug, Clone, PartialEq)]
pub struct Anomaly {
    pub dir: String,
    /// 最近一天的日增量（字节/天，正数）
    pub delta: i64,
    /// 基线的日均增量
    pub base_mean: i64,
    /// 基线的标准差
    pub base_sd: i64,
    /// z 值。基线天数不够时是 `None` —— 不是 0，也不是"不异常"。
    pub z: Option<f64>,
    /// 参与基线的天数
    pub days: u32,
    /// 还差几天才够基线
    pub days_needed: u32,
}

impl History {
    pub fn new(root: &str) -> History {
        History {
            root: core_io::paths::to_native(root),
            samples: Vec::new(),
            dropped: 0,
        }
    }

    /// 追加一个样本。
    ///
    /// 同一分钟内的两次采样只留最后一个：用户连点"记录一次"不该在图上戳出一串
    /// 几乎重合的点（那会让折线看起来像抖动）。
    pub fn push(&mut self, s: Sample) {
        if let Some(last) = self.samples.last_mut() {
            if (s.at_unix - last.at_unix).abs() < 60 {
                *last = s;
                return;
            }
        }
        self.samples.push(s);
        self.samples.sort_by_key(|s| s.at_unix);
        if self.samples.len() > MAX_SAMPLES {
            let extra = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..extra);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 最近一个样本落在哪一天（本地日序号）。没有样本返回 `None`。
    pub fn latest_day(&self) -> Option<i64> {
        self.samples.last().map(|s| local_days(s.at_unix))
    }

    /// 总大小的时间序列 `(at_unix, 总大小)`。
    pub fn total_series(&self) -> Vec<(i64, u64)> {
        self.samples.iter().map(|s| (s.at_unix, s.total)).collect()
    }

    /// 每天最后一个样本的总大小 `(本地日序号, 总大小)`，按日期升序。
    ///
    /// 趋势图与"最近几天涨了多少"都用它：同一天里记了好几个样本时，
    /// 只留当天最后一个 —— 否则一天会在图上占好几个点，斜率被采样密度带偏。
    pub fn daily_totals(&self) -> Vec<(i64, u64)> {
        let mut m: BTreeMap<i64, u64> = BTreeMap::new();
        for s in &self.samples {
            m.insert(local_days(s.at_unix), s.total);
        }
        m.into_iter().collect()
    }

    /// 某个目录的样本序列。该样本没记这个目录（被截断或那天它还不存在）就跳过。
    pub fn dir_series(&self, dir: &str) -> Vec<(i64, u64)> {
        self.samples
            .iter()
            .filter_map(|s| {
                s.dirs
                    .iter()
                    .find(|d| d.path == dir)
                    .map(|d| (s.at_unix, d.size))
            })
            .collect()
    }

    /// 最近一个样本里的目录（按大小降序，已截断）。
    pub fn latest_dirs(&self) -> &[DirPoint] {
        self.samples.last().map(|s| s.dirs.as_slice()).unwrap_or(&[])
    }

    /// 读。文件不存在 → 空历史（不是错误：第一次运行本来就没有）。
    pub fn load(path: &Path) -> Result<History, String> {
        match std::fs::File::open(path) {
            Ok(f) => History::read(BufReader::new(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(History::default()),
            Err(e) => Err(format!("打开历史文件失败：{e}")),
        }
    }

    fn read<R: BufRead>(rd: R) -> Result<History, String> {
        let mut lines = rd.lines();
        let head = match lines.next() {
            Some(Ok(l)) => l,
            _ => return Err("历史文件是空的".to_string()),
        };
        let mut hp = head.split('\t');
        if hp.next() != Some(MAGIC) {
            return Err(format!("不是 FileWatch 的历史文件（首行是「{head}」）"));
        }
        let ver: u32 = hp
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| "历史文件的版本号读不出来".to_string())?;
        if ver > VERSION {
            return Err(format!(
                "历史文件的版本是 {ver}，这个版本的程序只认到 {VERSION}（请升级）"
            ));
        }

        let mut h = History::default();
        let mut pending: Option<Sample> = None;
        let mut want = 0usize;
        for line in lines {
            let line = line.map_err(|e| format!("读历史文件失败：{e}"))?;
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("#root\t") {
                h.root = unescape(rest);
                continue;
            }
            if let Some(rest) = line.strip_prefix("#at\t") {
                // 上一个样本没能凑满行数 → 丢掉（只可能是崩溃留下的尾巴）
                if let Some(p) = pending.take() {
                    if p.dirs.len() != want {
                        h.dropped += 1;
                        continue;
                    }
                    h.samples.push(p);
                }
                let f: Vec<&str> = rest.split('\t').collect();
                if f.len() < 4 {
                    return Err(format!("历史文件的样本头不完整：{line}"));
                }
                let at: i64 = f[0].parse().map_err(|_| format!("样本时间不是整数：{line}"))?;
                let total: u64 = f[1].parse().unwrap_or(0);
                let files: u64 = f[2].parse().unwrap_or(0);
                let total_dirs: usize = f[3].parse().unwrap_or(0);
                want = f.get(4).and_then(|v| v.parse().ok()).unwrap_or(0);
                pending = Some(Sample {
                    at_unix: at,
                    total,
                    files,
                    dirs_total: total_dirs.max(want),
                    dirs: Vec::with_capacity(want),
                });
                continue;
            }
            // 目录行：路径 \t 大小 \t 文件数
            if let Some(s) = pending.as_mut() {
                let mut f = line.split('\t');
                let p = unescape(f.next().unwrap_or(""));
                let size: u64 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let files: u64 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                s.dirs.push(DirPoint { path: p, size, files });
            }
        }
        if let Some(p) = pending.take() {
            if p.dirs.len() == want {
                h.samples.push(p);
            } else {
                h.dropped += 1;
            }
        }
        h.samples.sort_by_key(|s| s.at_unix);
        Ok(h)
    }

    /// 写。先落 `.part` 再改名：中途被杀不会留下一份"看起来正常、少了一半"的
    /// 历史（少了的那一半会让趋势图凭空掉下去）。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
        }
        let tmp = path.with_extension("tsv.part");
        {
            let f = std::fs::File::create(&tmp).map_err(|e| format!("写历史文件失败：{e}"))?;
            let mut w = BufWriter::new(f);
            self.write(&mut w).map_err(|e| format!("写历史文件失败：{e}"))?;
            w.flush().map_err(|e| format!("写历史文件失败：{e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("改名失败：{e}"))
    }

    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        writeln!(w, "{MAGIC}\t{VERSION}")?;
        writeln!(w, "#root\t{}", escape(&self.root))?;
        for s in &self.samples {
            writeln!(
                w,
                "#at\t{}\t{}\t{}\t{}\t{}",
                s.at_unix,
                s.total,
                s.files,
                s.dirs_total,
                s.dirs.len()
            )?;
            for d in &s.dirs {
                writeln!(w, "{}\t{}\t{}", escape(&d.path), d.size, d.files)?;
            }
        }
        Ok(())
    }
}

/// 文件的第 `depth` 层祖先（不足就取最上面那个）。
///
/// 走到卷根就停：`C:\a.txt` 的桶是 `C:\`，而不是"再往上是 5 号记录"。
fn ancestor_at(tree: &Tree, node: u32, depth: u32) -> u32 {
    let mut cur = node;
    let mut hops = 0;
    loop {
        if hops >= depth {
            return cur;
        }
        let n = match tree.arena.get(cur) {
            Some(n) if n.in_use() => n,
            _ => return cur,
        };
        let p = n.parent;
        if p == INVALID_PARENT || p == cur {
            return cur;
        }
        cur = p;
        hops += 1;
        if cur == tree.root {
            return cur;
        }
    }
}

/// 把当前树压成一个样本。
pub fn capture(
    tree: &Tree,
    remap: Option<&PathRemap>,
    at_unix: i64,
    depth: u32,
    max_dirs: usize,
) -> Sample {
    let depth = depth.max(1);
    let max_dirs = max_dirs.max(1);
    let mut agg: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for (idx, n) in tree.arena.nodes.iter().enumerate() {
        if !n.in_use() || n.is_dir() {
            continue;
        }
        let b = ancestor_at(tree, idx as u32, depth);
        let e = agg.entry(b).or_insert((0, 0));
        e.0 = e.0.saturating_add(n.size);
        e.1 += 1;
    }
    let dirs_total = agg.len();
    let mut dirs: Vec<DirPoint> = agg
        .into_iter()
        .map(|(idx, (size, files))| {
            let p = tree.path_of(idx);
            DirPoint {
                path: match remap {
                    Some(r) => r.apply(&p),
                    None => p,
                },
                size,
                files,
            }
        })
        .collect();
    // 大小降序；同大小按路径，保证同一棵树两次采样给出**完全一样**的顺序
    // （否则"前 300 名"会随机抖动，趋势图跟着抖）
    dirs.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    dirs.truncate(max_dirs);
    Sample {
        at_unix,
        total: tree.total_size,
        files: tree.file_count,
        dirs_total,
        dirs,
    }
}

/// 每个目录"每天最后一个样本的大小"。`BTreeMap<日序号, 大小>`。
fn daily_sizes(h: &History) -> HashMap<&str, BTreeMap<i64, u64>> {
    let mut per: HashMap<&str, BTreeMap<i64, u64>> = HashMap::new();
    for s in &h.samples {
        let d = local_days(s.at_unix);
        for dp in &s.dirs {
            per.entry(dp.path.as_str()).or_default().insert(d, dp.size);
        }
    }
    per
}

/// 基线与异常。
///
/// 口径：最近一个有样本的日子 vs 上一个有样本的日子，差值按间隔天数摊平
/// （"每天涨多少"）；基线取那之前的 `lookback` 天里同样的日增量。
pub fn anomalies(h: &History, lookback: u32, top: usize) -> Vec<Anomaly> {
    let latest = match h.samples.last() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let today = local_days(latest.at_unix);
    let lookback = lookback.max(1) as i64;
    let per = daily_sizes(h);
    let mut out: Vec<Anomaly> = Vec::new();

    for (dir, days) in per {
        // 今天没有这个目录 → 它只是"今天没被记进前 300 名"，不是"没增长"
        let cur = match days.get(&today) {
            Some(v) => *v as i64,
            None => continue,
        };
        // 上一个有样本的日子
        let (&prev_day, &prev_size) = match days.range(..today).next_back() {
            Some(kv) => kv,
            None => continue,
        };
        let gap = (today - prev_day).max(1) as f64;
        let today_delta = ((cur - prev_size as i64) as f64 / gap).round() as i64;
        if today_delta <= 0 {
            continue;
        }

        // 基线：更早的那些相邻日期的日增量
        let mut base: Vec<f64> = Vec::new();
        let mut keys: Vec<i64> = days.keys().copied().filter(|d| *d < prev_day).collect();
        keys.sort_unstable();
        for w in keys.windows(2) {
            let (d0, d1) = (w[0], w[1]);
            if d1 >= today {
                continue;
            }
            if today - d1 > lookback {
                continue;
            }
            let a = days[&d0] as i64;
            let b = days[&d1] as i64;
            let g = (d1 - d0).max(1) as f64;
            base.push((b - a) as f64 / g);
        }

        let n = base.len() as u32;
        let mean = if base.is_empty() {
            0.0
        } else {
            base.iter().sum::<f64>() / base.len() as f64
        };
        let sd = if base.len() < 2 {
            0.0
        } else {
            let var = base.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (base.len() - 1) as f64;
            var.sqrt()
        };
        let judgeable = n >= MIN_BASELINE_DAYS;
        let z = if judgeable {
            Some(((today_delta as f64 - mean) / sd.max(SIGMA_FLOOR)).min(Z_CAP))
        } else {
            None
        };
        out.push(Anomaly {
            dir: dir.to_string(),
            delta: today_delta,
            base_mean: mean.round() as i64,
            base_sd: sd.round() as i64,
            z,
            days: n,
            days_needed: MIN_BASELINE_DAYS.saturating_sub(n),
        });
    }

    // 能下结论的排前面（z 降序），不能下结论的按增量降序跟在后面 ——
    // 把"还需要 2 天基线"的条目混在 z 榜里，等于用噪声挤掉真信号。
    out.sort_by(|a, b| match (a.z, b.z) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.delta.cmp(&a.delta),
    });
    // 只保留"够得上异常"的（能下结论）与"明显在涨"的（还不能下结论）
    out.retain(|a| match a.z {
        Some(z) => z >= Z_LIMIT,
        None => a.delta > 0,
    });
    out.truncate(top.max(1));
    out
}

/// 历史文件名里的根标识：盘符优先，其余用净化后的路径 + 短哈希。
///
/// 同一个根必须**永远**映射到同一个文件名（否则趋势图会分裂），不同根不能撞
/// （否则两条盘的历史会互相覆盖）。
pub fn file_key(root: &str) -> String {
    let root = core_io::paths::to_native(root);
    let b = root.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        if core_io::paths::trim_sep(&root[2..]).is_empty() {
            return (b[0] as char).to_ascii_uppercase().to_string();
        }
    }
    let mut clean = String::new();
    for c in root.chars() {
        if c.is_ascii_alphanumeric() {
            clean.push(c.to_ascii_lowercase());
        } else if !clean.ends_with('-') {
            clean.push('-');
        }
        if clean.len() >= 32 {
            break;
        }
    }
    let clean = clean.trim_matches('-').to_string();
    // FNV-1a：只要稳定、不要求抗碰撞（同一个根反复算要一致）
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{clean}-{:08x}", (h >> 32) as u32)
}

/// 默认的历史文件位置：`%APPDATA%\FileWatch\history\<key>.tsv`。
///
/// CLI 与界面**必须**用同一个位置，否则命令行记的样本界面看不见，
/// 用户会以为"历史没在记"。取不到 `APPDATA` 时退到 `%USERPROFILE%\.filewatch`，
/// 再不行退到系统临时目录 —— 宁可写偏一点，也不要因为一个环境变量缺失就
/// 把功能变成"静默不记录"。
pub fn default_path(root: &str) -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| std::path::PathBuf::from(p).join(".filewatch")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("FileWatch")
        .join("history")
        .join(format!("{}.tsv", file_key(root)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};

    /// 手工搭一棵树：目录 1 → 2 → 3，文件挂在 3 下面。
    /// 名字留空 —— 这里验的是"分到哪个桶"，不是路径长什么样。
    fn tiny_tree() -> Tree {
        let mut t = Tree::new("C:", 16);
        t.root = 1;
        t.arena.ensure(10);
        let mut set = |idx: u32, parent: u32, flag: u16, size: u64| {
            if let Some(n) = t.arena.get_mut(idx) {
                *n = Node {
                    parent,
                    flags: NODE_IN_USE | flag,
                    size,
                    alloc: size,
                    ..Node::EMPTY
                };
            }
        };
        set(1, INVALID_PARENT, NODE_DIR, 0);
        set(2, 1, NODE_DIR, 0);
        set(3, 2, NODE_DIR, 0);
        set(4, 3, 0, 100);
        set(5, 3, 0, 50);
        set(6, 3, 0, 7);
        t
    }

    #[test]
    fn buckets_are_counted_from_the_file_itself() {
        let t = tiny_tree();
        let s = capture(&t, None, 1_000, 3, 100);
        // 文件 4/5/6 的第 1 层祖先都是 3（同一个桶）
        assert_eq!(s.dirs.len(), 1, "三个文件应落进同一个桶");
        assert_eq!(s.dirs[0].size, 157);
        assert_eq!(s.dirs[0].files, 3);
        // 第 2 层 → 目录 2；第 5 层（超过树深）→ 卷根 1
        let s2 = capture(&t, None, 1_000, 2, 100);
        assert_eq!(s2.dirs[0].size, 157);
        let s5 = capture(&t, None, 1_000, 5, 100);
        assert_eq!(s5.dirs.len(), 1, "超过树深时全部落到卷根这一个桶");
    }

    #[test]
    fn capture_truncates_but_reports_the_real_bucket_count() {
        let t = tiny_tree();
        let s = capture(&t, None, 1_000, 1, 1);
        assert_eq!(s.dirs.len(), 1);
        assert_eq!(s.dirs_total, 1);
    }

    fn sample(at: i64, pairs: &[(&str, u64)]) -> Sample {
        Sample {
            at_unix: at,
            total: pairs.iter().map(|p| p.1).sum(),
            files: 0,
            dirs_total: pairs.len(),
            dirs: pairs
                .iter()
                .map(|(p, s)| DirPoint {
                    path: p.to_string(),
                    size: *s,
                    files: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn roundtrip_keeps_paths_with_tabs_and_newlines() {
        let mut h = History::new(r"C:\a");
        h.push(sample(1_760_000_000, &[("C:\\a\\tab\there", 10)]));
        h.push(sample(1_760_100_000, &[("C:\\a\\line\nbreak", 20)]));
        let dir = std::env::temp_dir().join("fw-history-test");
        let p = dir.join("rt.tsv");
        h.save(&p).unwrap();
        let back = History::load(&p).unwrap();
        assert_eq!(back.samples.len(), 2);
        assert_eq!(back.samples[0].dirs[0].path, "C:\\a\\tab\there");
        assert_eq!(back.samples[1].dirs[0].path, "C:\\a\\line\nbreak");
        assert_eq!(back.root, r"C:\a");
        assert!(!p.with_extension("tsv.part").exists(), "临时文件必须已被改名");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_minute_samples_are_merged() {
        let mut h = History::default();
        h.push(sample(1_000, &[("C:\\a", 1)]));
        h.push(sample(1_030, &[("C:\\a", 2)]));
        assert_eq!(h.samples.len(), 1, "一分钟内连点两次不该产生两个点");
        assert_eq!(h.samples[0].total, 2);
        h.push(sample(2_000, &[("C:\\a", 3)]));
        assert_eq!(h.samples.len(), 2);
    }

    #[test]
    fn truncated_tail_is_dropped_not_trusted() {
        let dir = std::env::temp_dir().join("fw-history-trunc");
        let p = dir.join("t.tsv");
        let mut h = History::default();
        h.push(sample(1_000, &[("C:\\a", 1), ("C:\\b", 2)]));
        h.save(&p).unwrap();
        // 手工砍掉最后一行**目录**（不是末尾的换行）→ 最后一个样本的计数
        // 对不上，整条丢弃并计数
        let txt = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<&str> = txt.lines().collect();
        lines.pop();
        std::fs::write(&p, lines.join("\n")).unwrap();
        let back = History::load(&p).unwrap();
        assert_eq!(back.samples.len(), 0);
        assert_eq!(back.dropped, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_an_empty_history() {
        let h = History::load(Path::new("C:\\definitely\\not\\here\\x.tsv")).unwrap();
        assert!(h.is_empty());
    }

    #[test]
    fn wrong_magic_is_an_error() {
        let dir = std::env::temp_dir().join("fw-history-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.tsv");
        std::fs::write(&p, "id\tname\n1\tx\n").unwrap();
        assert!(History::load(&p).is_err(), "随便一个 TSV 不能被当成历史");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每天涨 10 MB，今天忽然涨 100 MB —— 基线 5 天，z 应该很高。
    #[test]
    fn anomaly_flags_a_burst_against_a_stable_baseline() {
        let day = 86_400;
        let base = 1_760_000_000 - (1_760_000_000 % day);
        let mut h = History::default();
        let mut size = 1_000_000u64;
        for i in 0..6 {
            h.push(sample(base + i * day, &[("C:\\data", size)]));
            size += 10_000_000;
        }
        // 第 7 天暴涨 100 MB
        h.push(sample(base + 6 * day, &[("C:\\data", size + 100_000_000)]));
        let a = anomalies(&h, 7, 10);
        let hit = a.iter().find(|x| x.dir == "C:\\data").expect("应当报出来");
        assert!(hit.z.unwrap() > Z_LIMIT, "z={:?}", hit.z);
        assert!(hit.delta > 90_000_000);
        assert_eq!(hit.days_needed, 0);
    }

    /// 只有两天数据 → 不给 z，但把"还差几天"说清楚。
    #[test]
    fn insufficient_baseline_is_marked_not_judged() {
        let day = 86_400;
        let base = 1_760_000_000 - (1_760_000_000 % day);
        let mut h = History::default();
        h.push(sample(base, &[("C:\\data", 1_000_000)]));
        h.push(sample(base + day, &[("C:\\data", 5_000_000)]));
        let a = anomalies(&h, 7, 10);
        assert_eq!(a.len(), 1);
        assert!(a[0].z.is_none(), "两天数据不该给出 z 值");
        assert!(a[0].days_needed > 0);
    }

    /// 平稳增长不该被报成异常。
    #[test]
    fn steady_growth_is_not_an_anomaly() {
        let day = 86_400;
        let base = 1_760_000_000 - (1_760_000_000 % day);
        let mut h = History::default();
        let mut size = 1_000_000u64;
        for i in 0..9 {
            h.push(sample(base + i * day, &[("C:\\data", size)]));
            size += 50_000_000;
        }
        let a = anomalies(&h, 7, 10);
        assert!(a.is_empty(), "每天稳定涨 50 MB 不是异常：{a:?}");
    }

    /// 目录消失（今天没扫到 / 被删了）不该被算成"负增长异常"。
    #[test]
    fn missing_dir_today_is_skipped() {
        let day = 86_400;
        let base = 1_760_000_000 - (1_760_000_000 % day);
        let mut h = History::default();
        h.push(sample(base, &[("C:\\gone", 9_000_000)]));
        h.push(sample(base + day, &[("C:\\other", 9_000_000)]));
        let a = anomalies(&h, 7, 10);
        assert!(a.iter().all(|x| x.dir != "C:\\gone"), "没数据的目录不该出现");
    }

    #[test]
    fn file_key_is_stable_and_distinct() {
        assert_eq!(file_key("C:"), "C");
        assert_eq!(file_key("c:\\"), "C");
        assert_eq!(file_key("C:/Users/me/proj"), file_key("C:\\Users\\me\\proj"));
        assert_ne!(file_key("C:\\a"), file_key("C:\\b"));
        assert!(!file_key("C:\\Users\\me\\proj").contains('\\'));
    }

    /// 路径转义必须与快照共用一份（否则同一个路径在两份文件里的写法不同，
    /// 出问题时没法互相印证）。
    #[test]
    fn uses_the_snapshot_escape() {
        for p in ["a\tb", "a\\b", "x\r\ny"] {
            assert_eq!(unescape(&escape(p)), p);
        }
    }
}
