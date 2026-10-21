//! E6 的界面侧：读写目录历史，把引擎数据整理成前端要的形状。
//!
//! # 谁负责记录
//!
//! **只有性能模式**会自动记录（与周期任务是同一个开关 `Scheduler::should_run`）。
//! 理由和周期任务一样：在别人的盘上定期写文件是一个有副作用的动作，标准模式
//! 下不该悄悄发生。标准/轻量模式下这个页签仍然可用 —— 点「记录一次」才写。
//!
//! # 节流
//!
//! 同一个根 30 分钟内最多自动记一次。手动点「记录一次」不受节流限制
//! （那是用户的明确意图），但仍受引擎里的"同一分钟合并"约束。

use crate::prefs;
use crate::scheduler;
use crate::state::Shared;
use core_io::util::{local_date, local_time, unix_secs};
use core_scan::history::{self, History};
use core_scan::Session;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tauri::{Emitter, State};

/// 同一根两次**自动**记录的最小间隔（秒）。
pub const AUTO_GAP_SECS: i64 = 30 * 60;
/// 回看多少天算基线。
const LOOKBACK: u32 = history::DEFAULT_LOOKBACK;
/// 异常表最多几行。
const ANOMALY_TOP: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct DayPoint {
    /// 本地日期 `2026-10-21`
    pub date: String,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnomalyDto {
    pub dir: String,
    pub delta: i64,
    pub base_mean: i64,
    pub base_sd: i64,
    /// `null` = 基线天数不够，判不了（前端必须显示"还差 N 天"，不是 0）
    pub z: Option<f64>,
    pub days: u32,
    pub days_needed: u32,
    pub judgeable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryView {
    pub root: String,
    pub path: String,
    /// 自动记录是否开着（性能模式）
    pub auto: bool,
    pub samples: usize,
    pub dropped: u32,
    /// `2026-10-01 → 2026-10-21`，没有样本时为空
    pub span: String,
    pub latest: String,
    pub file_kb: u64,
    /// 按日聚合的总大小，给折线用
    pub days: Vec<DayPoint>,
    pub anomalies: Vec<AnomalyDto>,
    /// 最新样本里记了多少个目录 / 一共多少个桶
    pub dirs_kept: usize,
    pub dirs_total: usize,
}

/// 自动记录的节流表：根 → 上次记录时间。
fn last_records() -> &'static Mutex<HashMap<String, i64>> {
    static LAST: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 某个根的历史文件位置。CLI 与界面共用 `history::default_path` ——
/// 两处各写一份的话，"命令行记的样本界面看不见"这种问题永远不会有人发现。
pub fn path_for(root: &str) -> PathBuf {
    history::default_path(root)
}

/// 当前视图的根：优先扫描根，其次盘符。
pub fn current_root(s: &Session) -> String {
    let raw = s
        .cfg
        .scan_root
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| s.cfg.drive.clone());
    core_io::paths::to_native(&raw)
}

/// 扫描成功后挂在这里：性能模式 + 过了节流窗口才记一次。
///
/// **失败完全静默**：后台记录失败不该弹窗打断用户，但会写日志到 `history:error`
/// 事件，界面在趋势页里能看到（而不是"历史怎么一直不动"）。
pub fn auto_record(app: &tauri::AppHandle, s: &Session) {
    let p = prefs::load();
    if !scheduler::should_run(&p) {
        return;
    }
    let root = current_root(s);
    let now = unix_secs();
    {
        let mut map = match last_records().lock() {
            Ok(m) => m,
            Err(e) => e.into_inner(),
        };
        if let Some(prev) = map.get(&root) {
            if now - *prev < AUTO_GAP_SECS {
                return;
            }
        }
        map.insert(root.clone(), now);
    }
    match record(s, now, 3) {
        Ok(_) => {
            let _ = app.emit(
                "history:recorded",
                serde_json::json!({ "root": root, "at": local_time(now) }),
            );
        }
        Err(e) => {
            let _ = app.emit("history:error", e);
        }
    }
}

/// 真记一次。返回写入的目录数。
fn record(s: &Session, at: i64, depth: u32) -> Result<(usize, usize), String> {
    let root = current_root(s);
    let path = path_for(&root);
    let mut h = match History::load(&path) {
        Ok(h) => h,
        // 坏文件不覆盖：改名留证，再从头开始（与 CLI 同一处理）
        Err(e) => {
            let bad = path.with_extension(format!("tsv.bad-{}", core_io::util::local_stamp(at)));
            let _ = std::fs::rename(&path, &bad);
            eprintln!("历史文件读不出来（{e}），已改名为 {}", bad.display());
            History::default()
        }
    };
    if h.root.is_empty() {
        h.root = root.clone();
    }
    let remap = s.path_remap();
    let sample = history::capture(
        &s.tree,
        remap.as_ref(),
        at,
        depth,
        history::MAX_DIRS,
    );
    let kept = sample.dirs.len();
    let total = sample.dirs_total;
    h.push(sample);
    h.save(&path)?;
    Ok((kept, total))
}

fn view_of(root: &str) -> HistoryView {
    let path = path_for(root);
    let h = History::load(&path).unwrap_or_default();
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let auto = scheduler::should_run(&prefs::load());

    let span = match (h.samples.first(), h.samples.last()) {
        (Some(a), Some(b)) => format!(
            "{} → {}",
            local_date(a.at_unix),
            local_date(b.at_unix)
        ),
        _ => String::new(),
    };
    let latest = match h.samples.last() {
        Some(s) => format!(
            "{} · 总 {} · {} 个文件",
            local_time(s.at_unix),
            core_io::util::human_size(s.total),
            s.files
        ),
        None => String::new(),
    };
    let days: Vec<DayPoint> = h
        .daily_totals()
        .into_iter()
        .map(|(d, total)| {
            let (y, m, dd) = core_io::util::civil_from_days(d);
            DayPoint {
                date: format!("{y:04}-{m:02}-{dd:02}"),
                total,
            }
        })
        .collect();
    let anomalies = history::anomalies(&h, LOOKBACK, ANOMALY_TOP)
        .into_iter()
        .map(|a| AnomalyDto {
            dir: a.dir,
            delta: a.delta,
            base_mean: a.base_mean,
            base_sd: a.base_sd,
            z: a.z,
            days: a.days,
            days_needed: a.days_needed,
            judgeable: a.z.is_some(),
        })
        .collect();
    let (kept, total) = h
        .samples
        .last()
        .map(|s| (s.dirs.len(), s.dirs_total))
        .unwrap_or((0, 0));

    HistoryView {
        root: if h.root.is_empty() {
            root.to_string()
        } else {
            h.root.clone()
        },
        path: path.to_string_lossy().to_string(),
        auto,
        samples: h.samples.len(),
        dropped: h.dropped,
        span,
        latest,
        file_kb: bytes.div_ceil(1024),
        days,
        anomalies,
        dirs_kept: kept,
        dirs_total: total,
    }
}

/// 读历史并整理成前端要的形状。**不扫描**：只是读一个小文件。
#[tauri::command]
pub fn history_view(state: State<'_, Shared>, root: Option<String>) -> Result<HistoryView, String> {
    let root = match root {
        Some(r) => core_io::paths::to_native(&r),
        None => {
            let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
            match st.loaded.as_ref() {
                Some(l) => current_root(&l.session),
                None => st
                    .last_drive
                    .clone()
                    .unwrap_or_else(|| core_io::paths::DEFAULT_VOLUME.to_string()),
            }
        }
    };
    Ok(view_of(&root))
}

/// 立刻记一次（用当前已加载的树，不再扫一遍）。没有扫描结果时报错。
#[tauri::command]
pub fn history_record(state: State<'_, Shared>) -> Result<HistoryView, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st
        .loaded
        .as_ref()
        .ok_or_else(|| "还没有扫描结果：先扫一块盘，再记录".to_string())?;
    let now = unix_secs();
    if l.session.tree.file_count == 0 {
        return Err("索引是空的，记下来只会得到一条没有意义的历史".to_string());
    }
    let root = current_root(&l.session);
    record(&l.session, now, 3)?;
    // 手动记录不受节流限制，但要把节流表刷新，免得紧接着的自动记录又写一次
    if let Ok(mut m) = last_records().lock() {
        m.insert(root.clone(), now);
    }
    Ok(view_of(&root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_shared_with_the_engine() {
        assert_eq!(path_for("C:"), history::default_path("C:"));
        assert_eq!(path_for("c:\\"), path_for("C:"));
    }

    #[test]
    fn empty_history_renders_without_panicking() {
        let v = view_of("Q:\\definitely-not-a-volume");
        assert_eq!(v.samples, 0);
        assert!(v.days.is_empty());
        assert!(v.anomalies.is_empty());
        assert_eq!(v.root, "Q:\\definitely-not-a-volume");
    }

    /// 自动记录的最小间隔必须 > 0：0 会让每次扫描都写一遍盘，
    /// 一个上午就能攒出几百个样本把"每天一个点"的趋势图变成毛刺。
    #[test]
    fn auto_gap_is_sane() {
        assert!(AUTO_GAP_SECS >= 60);
        assert!(LOOKBACK >= 1);
    }
}
