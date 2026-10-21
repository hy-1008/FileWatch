//! 「分析」视图的后端：**F12 重复文件** 与 **F11 快照对比**。
//!
//! 与 `commands.rs` 一样，这里**只做编排**：算法全在 `core-scan` 里
//! （`core_scan::dedup` / `core_scan::snapshot`），CLI 用的是同一份实现，
//! 所以同一个目录在终端和界面里给出的「可回收空间」必须一致。
//!
//! # 为什么重复文件查找要单独一个后台线程 + 状态
//!
//! 它的耗时取决于磁盘上**真正重复的数据量**（三级筛选会把不重复的文件
//! 完全跳掉，但真重复的那部分总要读满一遍）。在机械盘上这可能是几分钟，
//! 所以：
//!
//! 1. **不在命令里同步做** —— 同步做会把整个界面冻住；
//! 2. **不持有扫描结果的锁** —— 只把「路径 + 体积」的候选表**复制**出来，
//!    然后松开锁。候选表是一份纯数据的快照，监控线程随后怎么改索引树都不影响它。
//!
//! # 一个刻意的取舍：不支持中途取消
//!
//! 取消需要在线程里到处插检查点、还要处理"取消后结果算不算数"。而这一步
//! 是只读的、不会留下半个文件，用户真要停可以直接关窗口。所以先不做。
//!
//! # F11 为什么也要走后台线程
//!
//! 存快照是「写一百多 MB 的文本」，对比是「读一百多 MB + 归并两百万条」——
//! 两个都是好几秒的事。放在命令里同步做，主线程会在这几秒里不响应任何消息，
//! 界面看起来就是「点了没反应」，用户只能猜是不是卡住了（Windows 还会把窗口
//! 标成"未响应"）。所以与 F12 一样：**命令只负责校验并开工，结果用事件推**。
//!
//! 进度条上的百分比是**整件活走到哪了**，不是某一阶段的百分比：三步的量纲
//! 不同（字节 / 条目 / 条目），只能各占一段权重。

use crate::state::Shared;
use core_scan::dedup::{
    collect_candidates, find_duplicates, groups_to_csv, summary_cn as dup_summary_cn, CandidateSet,
    DupCandidate, DupGroup, DupOptions, DupProgress, FsSource,
};
use core_scan::snapshot::{
    diff_with, rows_to_csv, summary_cn as diff_summary_cn, DiffOptions, DiffOrder, DiffReport,
    Progress, Snapshot, SnapshotOptions,
};
// 只有测试里要拿"不带进度的 diff"与"带进度的 diff_with"对拍，
// 所以单独引进来 —— 否则正式构建会报 unused import。
#[cfg(test)]
use core_scan::snapshot::diff;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// 进度事件最小间隔（与扫描一致）：60 ms ≈ 每帧最多一条。
///
/// 哈希一个文件就可能触发一次回调，200 万个候选下不节流会把前端渲染线程淹掉
/// —— 事件排队比绘制还慢。
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(60);

// ── 传输对象 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DupProgressDto {
    /// 中文阶段名（部分哈希 / 完整哈希 / 逐字节复核）
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub elapsed_ms: u64,
    /// 0~1，`total == 0` 时为 1
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupFileDto {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupGroupDto {
    pub size: u64,
    pub count: u64,
    pub wasted: u64,
    pub truncated: bool,
    pub files: Vec<DupFileDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupResultDto {
    /// 引擎给出的漏斗摘要（与 CLI 一字不差）
    pub summary: String,
    // ── 候选清单阶段（对应 CLI 的「候选 N 个文件（排除：…）」那一行） ──
    /// 真正参与比较的文件数
    pub candidates: u64,
    pub skipped_small: u64,
    pub skipped_empty: u64,
    pub skipped_reparse: u64,
    /// **候选数达到上限被截断**掉的文件数。
    ///
    /// 前端必须显眼提示：截断意味着"还有没被比较过的文件"，
    /// 结论只能是"至少这些是重复的"，不能说"就这些"。
    pub candidates_truncated: u64,
    // ── 三级筛选 ──
    pub same_size_groups: u64,
    pub same_size_files: u64,
    pub partial_files: u64,
    pub full_files: u64,
    pub unreadable: u64,
    pub bytes_hashed: u64,
    pub group_count: u64,
    pub total_wasted: u64,
    pub groups: Vec<DupGroupDto>,
    pub elapsed_ms: u64,
}

/// 一次重复文件查找的完整可读状态（前端轮询它）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DupView {
    pub running: bool,
    pub progress: Option<DupProgressDto>,
    pub result: Option<DupResultDto>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DupState {
    running: bool,
    progress: Option<DupProgressDto>,
    result: Option<DupResultDto>,
    error: Option<String>,
}

/// 重复文件查找的共享状态。
///
/// 锁里只有「状态」这一小块数据，**不包含候选表**：候选表只在启动线程时
/// 短暂存在，不需要跨线程共享。
pub struct DupHub(Mutex<DupState>);

impl DupHub {
    pub fn new() -> Arc<DupHub> {
        Arc::new(DupHub(Mutex::new(DupState::default())))
    }

    /// 与 `AppState` 一样：锁被污染（上一个线程 panic 过）时明确报错，
    /// 而不是静默拿一份可能自相矛盾的状态继续跑。
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DupState>, String> {
        self.0.lock().map_err(|_| {
            "内部状态锁被污染（上一次查找线程 panic 过），请重启程序".to_string()
        })
    }

    pub fn is_running(&self) -> bool {
        self.lock().map(|g| g.running).unwrap_or(false)
    }

    fn view(&self) -> DupView {
        match self.lock() {
            Ok(g) => DupView {
                running: g.running,
                progress: g.progress.clone(),
                result: g.result.clone(),
                error: g.error.clone(),
            },
            Err(e) => DupView {
                running: false,
                progress: None,
                result: None,
                error: Some(e),
            },
        }
    }
}

// ── F12 重复文件 ────────────────────────────────────────────────────────────

/// 把界面上的「体积」文本框解析成字节数。
///
/// **不在前端解析。** 界面输入的 `1M` / `512K` 与 CLI 的 `--min-size=1M`
/// 必须走同一份语法（[`core_monitor::spec::parse_size`]）—— 前端自己写一个
/// 正则的话，用户在终端里学会的写法到界面上就不认了，而且这种分歧只在
/// 某个特定单位上才暴露。
///
/// 空串 = 不设下限（与 CLI 不给 `--min-size` 一致）。
fn parse_size_input(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(0);
    }
    core_monitor::spec::parse_size(t)
        .ok_or_else(|| format!("体积看不懂：「{t}」。写法示例：1M / 512K / 1073741824，留空 = 不设下限"))
}

/// 把候选集转成前端要的形状。
///
/// 候选集里可能有几十万条吗？不会 —— 界面默认「最小 1 MiB」，落到候选表里的
/// 通常只有几千条。`max_candidates` 也由界面传进来兜一层。
fn candidates_of(
    shared: &State<'_, Shared>,
    min_size: u64,
    max_candidates: usize,
) -> Result<CandidateSet, String> {
    let st = shared.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded()?;
    if st.scanning {
        return Err("正在扫描：扫描结束前索引树还会被整体替换，请稍后再试".into());
    }
    let opts = DupOptions {
        min_size,
        max_candidates,
        ..Default::default()
    };
    // 与 CLI、Top-N、变更流水共用同一套「合成路径 → 真实路径」规则
    let remap = l.session.path_remap();
    Ok(collect_candidates(l.tree(), remap.as_ref(), &opts))
}

/// 开始一次重复文件查找。立刻返回，结果通过事件推给前端：
/// `dup:progress` / `dup:done` / `dup:error`。
#[tauri::command]
pub fn dup_start(
    app: AppHandle,
    state: State<'_, Shared>,
    hub: State<'_, Arc<DupHub>>,
    min_size: String,
    top: Option<usize>,
    max_per_group: Option<usize>,
    max_candidates: Option<usize>,
) -> Result<(), String> {
    if hub.is_running() {
        return Err("已有一次查找在进行中".into());
    }
    let min_size = parse_size_input(&min_size)?;
    // 先把候选表取出来（这一步只在锁内做一次全表扫），随后立刻松开锁。
    let set = candidates_of(&state, min_size, max_candidates.unwrap_or(2_000_000))?;
    // 候选清单的几个计数要先拿出来：`set.items` 随后会被移进线程，
    // 而结果里要如实报出"排除了多少、有没有被截断"。
    let set_meta = (
        set.items.len() as u64,
        set.skipped_small,
        set.skipped_empty,
        set.skipped_reparse,
        set.truncated,
    );

    let opts = DupOptions {
        min_size,
        max_groups: top.unwrap_or(300),
        max_per_group: max_per_group.unwrap_or(50),
        max_candidates: max_candidates.unwrap_or(2_000_000),
        ..Default::default()
    };

    {
        let mut g = hub.lock()?;
        g.running = true;
        g.result = None;
        g.error = None;
        g.progress = None;
    }

    let hub_arc: Arc<DupHub> = Arc::clone(&*hub);
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut last = Instant::now();
        let app_p = app.clone();
        let mut progress = |p: DupProgress| {
            let now = Instant::now();
            if now.duration_since(last) < PROGRESS_MIN_GAP {
                return;
            }
            last = now;
            let dto = DupProgressDto {
                stage: p.stage.label_cn().to_string(),
                done: p.done,
                total: p.total,
                elapsed_ms: t0.elapsed().as_millis() as u64,
                ratio: if p.total == 0 {
                    1.0
                } else {
                    p.done as f64 / p.total as f64
                },
            };
            if let Ok(mut g) = hub_arc.lock() {
                g.progress = Some(dto.clone());
            }
            let _ = app_p.emit("dup:progress", dto);
        };

        let rep = find_duplicates(&set.items, &opts, &FsSource, &mut progress);
        let elapsed = t0.elapsed().as_millis() as u64;
        let dto = DupResultDto {
            summary: dup_summary_cn(&rep),
            candidates: set_meta.0,
            skipped_small: set_meta.1,
            skipped_empty: set_meta.2,
            skipped_reparse: set_meta.3,
            candidates_truncated: set_meta.4,
            same_size_groups: rep.same_size_groups,
            same_size_files: rep.same_size_files,
            partial_files: rep.partial_files,
            full_files: rep.full_files,
            unreadable: rep.unreadable,
            bytes_hashed: rep.bytes_hashed,
            group_count: rep.group_count,
            total_wasted: rep.total_wasted,
            groups: rep
                .groups
                .iter()
                .map(|g| DupGroupDto {
                    size: g.size,
                    count: g.count,
                    wasted: g.wasted,
                    truncated: g.truncated,
                    files: g
                        .files
                        .iter()
                        .map(|f| DupFileDto {
                            path: f.path.clone(),
                            size: f.size,
                            mtime: f.mtime,
                        })
                        .collect(),
                })
                .collect(),
            elapsed_ms: elapsed,
        };

        if let Ok(mut g) = hub_arc.lock() {
            g.running = false;
            g.result = Some(dto.clone());
            g.progress = None;
        }
        let _ = app.emit("dup:done", dto);
    });

    Ok(())
}

/// 当前进度 / 结果。前端在窗口打开时用它补一次状态（事件可能发生在监听之前）。
#[tauri::command]
pub fn dup_view(hub: State<'_, Arc<DupHub>>) -> DupView {
    hub.view()
}

/// 把结果导出成 CSV。
///
/// 表格由引擎的 [`groups_to_csv`] 生成 —— CLI 的 `--csv` 走的是同一个函数。
/// 在这里再写一份列头与转义，就是"界面导出的 CSV 和终端导出的 CSV 不一样"
/// 这种迟早会发生的事。
#[tauri::command]
pub fn dup_export_csv(hub: State<'_, Arc<DupHub>>, path: String) -> Result<usize, String> {
    let p = export_target(&path, "duplicates", "csv")?;
    let body = {
        let g = hub.lock()?;
        let r = g
            .result
            .as_ref()
            .ok_or_else(|| "还没有查找结果，请先点「开始查找」".to_string())?;
        // DTO → 引擎类型，只为了喂给那一个函数。多这一步换来的是"列头与转义
        // 只有一处实现"。
        let groups: Vec<DupGroup> = r
            .groups
            .iter()
            .map(|g| DupGroup {
                size: g.size,
                count: g.count,
                wasted: g.wasted,
                truncated: g.truncated,
                files: g
                    .files
                    .iter()
                    .map(|f| DupCandidate {
                        path: f.path.clone(),
                        size: f.size,
                        mtime: f.mtime,
                    })
                    .collect(),
            })
            .collect();
        let mut s = groups_to_csv(&groups);
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    };
    let n = body.lines().count().saturating_sub(1);
    write_text_file(&p, &body)?;
    Ok(n)
}

/// 写文本文件（自动建父目录）。
///
/// 界面里所有"导出到某路径"的动作都走它：少了这一步，用户填一个还不存在的
/// 目录就会得到一个 `os error 3`，而错误文案里完全看不出该建目录。
/// `filewatch-<类型>-<UTC 时间戳>.<扩展名>`。文件名只能由后端拼：前端的
/// `new Date()` 是本地时区，而快照正文里的时间是 UTC，同一个文件会在两边分叉。
fn stamp_name(stem: &str, ext: &str) -> String {
    format!(
        "filewatch-{}-{}.{}",
        stem,
        core_io::util::local_stamp(core_io::util::unix_secs()),
        ext
    )
}

/// 导出目标：**给目录就补默认文件名，给文件就原样用**。
///
/// 界面上「存快照到 / CSV」可以点「浏览」选一个**文件夹**（让用户自己给快照
/// 起名字并不合理 —— 那个时间戳得跟正文里的 UTC 时间对上）。于是落盘前必须先
/// 分清拿到的是目录还是文件：把目录当文件写，得到的是一个 `os error 5`，
/// 从报错完全看不出「该选文件夹」和「该填文件名」的区别。
///
/// 判定：以分隔符结尾 → 目录；磁盘上确实是目录 → 目录；否则 → 文件
/// （还没创建的不存在的路径一律当文件，这样手填文件名照旧可用）。
pub(crate) fn export_target(path: &str, stem: &str, ext: &str) -> Result<String, String> {
    let t = path.trim();
    if t.is_empty() {
        return Err("请填写导出路径".into());
    }
    let p = t.replace('/', "\\");
    if p.ends_with('\\') || std::path::Path::new(&p).is_dir() {
        return Ok(format!("{}\\{}", p.trim_end_matches('\\'), stamp_name(stem, ext)));
    }
    Ok(p)
}

pub(crate) fn write_text_file(path: &str, body: &str) -> Result<(), String> {
    let path = std::path::PathBuf::from(path.trim().replace('/', "\\"));
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败：{e}"))?;
        }
    }
    std::fs::write(&path, body).map_err(|e| format!("写入失败：{e}"))
}

// ── F11 快照 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub path: String,
    pub root: String,
    pub created_unix: i64,
    /// 好看的时间（**本地**）
    pub created_text: String,
    pub entries: u64,
    pub truncated: u64,
    /// 索引里统计的总字节数
    pub total_size: u64,
    /// 写出来的文件有多少字节
    pub bytes_on_disk: u64,
}

/// 默认落盘路径：桌面 + `filewatch-<类型>-<本地时间戳>.<扩展名>`。
///
/// 三个导出（快照 / 重复文件 / 差异 CSV）共用它。**时间戳由后端给**
/// （[`core_io::util::local_stamp`]）：前端自己 `new Date()` 拼也能得到本地时间，
/// 但那样时间口径就分成了两份，改一处漏一处迟早会在同一个文件名上对不上。
///
/// 用**本地**时间而不是 UTC：这个文件名是给用户认的（"这是晚上九点那份"），
/// 而资源管理器、任务栏都是本地时间。
///
/// 未知的 `kind` 直接报错，不兜底成某个默认值 —— 兜底只会让前端拼错名字时
/// 静默导出到"另一个看起来也对"的文件。
#[tauri::command]
pub fn analyze_default_path(kind: String) -> Result<String, String> {
    let (stem, ext) = match kind.as_str() {
        "snapshot" => ("snapshot", "tsv"),
        "duplicates" => ("duplicates", "csv"),
        "diff" => ("diff", "csv"),
        // 批量动作导出的搜索结果（v0.1.8）
        "search" => ("search", "csv"),
        other => return Err(format!(
            "未知的导出类型「{other}」（只认 snapshot / duplicates / diff / search）"
        )),
    };
    let dir = crate::monitor::monitor_default_export_dir();
    Ok(format!(
        "{}\\{}",
        dir.trim_end_matches(['\\', '/']),
        stamp_name(stem, ext)
    ))
}

/// 三步（读盘 / 抓树 / 归并）各占多少进度条。
///
/// 权重只是经验分配，作用仅仅是让条**一直往前走** —— 它不承诺某一步真的占
/// 这么久。读盘几百 MB 通常是最慢的一段，所以给得最多。
/// 对比拆四段：读文件字节（快）→ 解析行（大头之一，200 万行要好几秒）
/// → 抓当前树 → 归并排序。解析不给进度的话，条会在"读取快照"上僵住，
/// 用户看到的正是"只有字、条不动"。
const W_READ_FILE: f64 = 0.15;
const W_PARSE: f64 = 0.30;
const W_CAPTURE: f64 = 0.40;
const W_DIFF: f64 = 0.15;
/// 存快照只有两步：抓树 + 写盘。写盘是大头（一百多 MB）。
const W_SAVE_WRITE: f64 = 0.55;

/// 一次快照作业的进度事件。
#[derive(Debug, Clone, Serialize)]
pub struct DiffProgressDto {
    /// `"compare"` / `"save"`
    pub kind: String,
    /// 中文阶段名（读取快照 / 建立当前快照 / 对比并排序 …）
    pub stage: String,
    /// 0~1，**整件活**走到哪了
    pub ratio: f64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default)]
struct DiffJob {
    running: bool,
    kind: String,
    stage: String,
    ratio: f64,
    result: Option<DiffView>,
    error: Option<String>,
}

/// 作业的当前状态。前端打开视图时用它补一次（事件可能发生在监听注册之前）。
#[derive(Debug, Clone, Serialize)]
pub struct DiffStatusDto {
    pub running: bool,
    pub kind: String,
    pub stage: String,
    pub ratio: f64,
    pub result: Option<DiffView>,
    pub error: Option<String>,
}

pub struct DiffHub(Mutex<DiffJob>);

impl DiffHub {
    pub fn new() -> Arc<DiffHub> {
        Arc::new(DiffHub(Mutex::new(DiffJob::default())))
    }

    /// 与 `DupHub` 一样：锁被污染时明确报错，而不是拿一份自相矛盾的状态继续跑。
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DiffJob>, String> {
        self.0.lock().map_err(|_| {
            "内部状态锁被污染（上一次快照作业 panic 过），请重启程序".to_string()
        })
    }

    pub fn is_running(&self) -> bool {
        self.lock().map(|g| g.running).unwrap_or(false)
    }

    fn view(&self) -> DiffStatusDto {
        match self.lock() {
            Ok(g) => DiffStatusDto {
                running: g.running,
                kind: g.kind.clone(),
                stage: g.stage.clone(),
                ratio: g.ratio,
                result: g.result.clone(),
                error: g.error.clone(),
            },
            Err(e) => DiffStatusDto {
                running: false,
                kind: String::new(),
                stage: String::new(),
                ratio: 0.0,
                result: None,
                error: Some(e),
            },
        }
    }
}

/// 向前端报进度：写状态 + 推事件（与 F12 一样按 60 ms 节流）。
struct Reporter {
    hub: Arc<DiffHub>,
    app: AppHandle,
    t0: Instant,
    last: Instant,
    kind: String,
    stage: String,
    ratio: f64,
}

impl Reporter {
    fn new(hub: Arc<DiffHub>, app: AppHandle, kind: &str) -> Reporter {
        let now = Instant::now();
        Reporter {
            hub,
            app,
            t0: now,
            last: now,
            kind: kind.to_string(),
            stage: "准备中".to_string(),
            ratio: 0.0,
        }
    }

    /// 换阶段。**一定**推一条事件：长跑里阶段名是用户唯一能看出"它在动"的东西。
    fn stage(&mut self, stage: &str, ratio: f64) {
        self.stage = stage.to_string();
        self.ratio = ratio;
        self.push(true);
    }

    /// 同一阶段内的百分比推进（节流）。
    fn at(&mut self, ratio: f64) {
        // 进度条不许倒退：不同阶段量纲不同，算出来的比值偶尔会回退一点点
        if ratio <= self.ratio {
            return;
        }
        self.ratio = ratio;
        self.push(false);
    }

    fn push(&mut self, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(self.last) < PROGRESS_MIN_GAP {
            return;
        }
        self.last = now;
        if let Ok(mut g) = self.hub.lock() {
            g.stage = self.stage.clone();
            g.ratio = self.ratio;
        }
        let _ = self.app.emit(
            "diff:progress",
            DiffProgressDto {
                kind: self.kind.clone(),
                stage: self.stage.clone(),
                ratio: self.ratio,
                elapsed_ms: self.t0.elapsed().as_millis() as u64,
            },
        );
    }

    fn finish(&self) {
        if let Ok(mut g) = self.hub.lock() {
            g.running = false;
            g.ratio = 1.0;
            g.stage = "完成".to_string();
        }
    }

    fn fail(&self, msg: String) {
        if let Ok(mut g) = self.hub.lock() {
            g.running = false;
            g.error = Some(msg.clone());
        }
        let _ = self.app.emit("diff:error", msg);
    }
}

/// 抓一份"现在"的快照（对比与存快照共用）。
///
/// 路径换算与扫描根的取法只有这一处：两处各写一遍的话，界面里的快照与
/// 终端里的快照会在"根"这个字段上分叉，而 root 不一致正是 diff 要报警的事。
///
/// 抓树期间**持有状态锁**（树在里面，没法复制一份再算）。监控线程会在这
/// 段时间里等一下 —— 它不会丢事件（USN 是内核队列），只是晚一点应用。
fn capture_now(
    app: &AppHandle,
    opts: &SnapshotOptions,
    on: &mut Progress<'_>,
) -> Result<(Snapshot, u64), String> {
    let shared = app.state::<Shared>();
    let st = shared.lock().map_err(|_| "状态锁被污染".to_string())?;
    if st.scanning {
        return Err("正在扫描：扫描结束后索引树才是稳定的，请稍后再试".into());
    }
    let l = st.loaded()?;
    let remap = l.session.path_remap();
    let root = l
        .session
        .cfg
        .scan_root
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| l.session.cfg.drive.clone());
    let total_size = l.tree().total_size;
    Ok((
        Snapshot::capture_with(
            l.tree(),
            remap.as_ref(),
            &root,
            core_io::util::unix_secs(),
            opts,
            on,
        ),
        total_size,
    ))
}

/// 把**当前**扫描结果存成一份快照。
///
/// 立刻返回，结果通过 `diff:saved` 推给前端（进度走 `diff:progress`）。
/// 写盘 + 拼文本在 200 万文件下要好几秒，同步做会让窗口变成"未响应"。
#[tauri::command]
pub fn snapshot_save(
    app: AppHandle,
    hub: State<'_, Arc<DiffHub>>,
    path: String,
    max_entries: Option<usize>,
) -> Result<(), String> {
    // 路径先校验完再开工：这类错误必须当场弹提示，而不是几秒后从事件里冒出来
    // —— 那时用户已经不知道自己在等什么了。
    let p = export_target(&path, "snapshot", "tsv")?;
    if hub.is_running() {
        return Err("已有一次快照作业在进行中".into());
    }
    {
        let mut g = hub.lock()?;
        g.running = true;
        g.kind = "save".to_string();
        g.stage = "准备中".to_string();
        g.ratio = 0.0;
        g.error = None;
    }
    let opts = SnapshotOptions {
        max_entries: max_entries.unwrap_or(0),
    };
    let hub_arc: Arc<DiffHub> = Arc::clone(&*hub);
    std::thread::spawn(move || {
        let mut rep = Reporter::new(hub_arc, app.clone(), "save");

        rep.stage("建立当前快照", 0.0);
        let (snap, total_size) = match capture_now(&app, &opts, &mut |d, t| {
            let r = ratio_of(d, t);
            rep.at(r * (1.0 - W_SAVE_WRITE));
        }) {
            Ok(v) => v,
            Err(e) => {
                rep.fail(e);
                return;
            }
        };

        rep.stage("写入快照文件", 1.0 - W_SAVE_WRITE);
        let out = std::path::PathBuf::from(p.replace('/', "\\"));
        if let Some(dir) = out.parent() {
            if !dir.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    rep.fail(format!("创建目录失败：{e}"));
                    return;
                }
            }
        }
        // `save_with` 自己会先写 `.part` 再改名（避免留下半份快照被当成基准），
        // 父目录由上面保证存在。
        if let Err(e) = snap.save_with(&out, &mut |d, t| {
            rep.at(1.0 - W_SAVE_WRITE + ratio_of(d, t) * W_SAVE_WRITE);
        }) {
            rep.fail(format!("写入失败：{e}"));
            return;
        }
        let bytes_on_disk = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);

        rep.finish();
        let _ = app.emit(
            "diff:saved",
            SnapshotInfo {
                path: out.to_string_lossy().to_string(),
                root: snap.root.clone(),
                created_unix: snap.created_unix,
                created_text: core_io::util::local_time(snap.created_unix),
                entries: snap.len() as u64,
                truncated: snap.truncated,
                total_size,
                bytes_on_disk,
            },
        );
    });
    Ok(())
}

/// `(已完成, 总量)` → 0~1。总量为 0（这一步算不出总量）时按 1 处理：
/// 与其让条停在 0 让人以为卡住了，不如走满。
fn ratio_of(done: u64, total: u64) -> f64 {
    if total == 0 {
        1.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffRowDto {
    /// 稳定标识：added / removed / grew / shrank
    pub kind: String,
    pub kind_cn: String,
    pub delta: i64,
    pub old_size: u64,
    pub new_size: u64,
    pub path: String,
    /// 文件名（不含目录）。中间省略要保留扩展名，所以判断"哪部分是扩展名"
    /// 不能靠前端去猜路径分隔符 —— 这里给出后端算好的名字。
    pub name: String,
    /// 修改时间（Unix 秒，取新快照；已删除的行取旧快照）。快照里本来就有。
    pub mtime: i64,
    /// 创建时间（Unix 秒）。**索引里不采集 ctime**（每节点 +8 B，200 万节点
    /// 就是 16 MB，换一个"列表里显示"的字段不划算），所以这里在后台线程里
    /// 对**已经在列出的那几百行**逐行 stat 一次补上。取不到（已删除/无权限）为 0。
    pub ctime: i64,
}

/// 目录归因的一行。
#[derive(Debug, Clone, Serialize)]
pub struct DirDeltaDto {
    pub dir: String,
    pub delta: i64,
    pub added: u64,
    pub removed: u64,
    pub grew: u64,
    pub shrank: u64,
    pub files: u32,
}

/// 类型归因的一行。
#[derive(Debug, Clone, Serialize)]
pub struct ExtDeltaDto {
    pub ext: String,
    pub delta: i64,
    pub added: u64,
    pub removed: u64,
    pub files: u32,
}

/// 时段归因的一行。
#[derive(Debug, Clone, Serialize)]
pub struct BurstDto {
    pub hour_unix: i64,
    pub delta: i64,
    pub files: u32,
}

/// 增长归因（E1）的界面版。字段名与引擎一一对应，不做重命名 ——
/// 一旦两边叫法不同，改引擎的人就不知道界面在等什么。
#[derive(Debug, Clone, Serialize)]
pub struct AttributionDto {
    pub dir_depth: u32,
    pub total_delta: i64,
    pub files: u64,
    pub dirs_total: u32,
    /// 增长榜（降序）
    pub dirs_up: Vec<DirDeltaDto>,
    /// 减少榜（最负的在前）
    pub dirs_down: Vec<DirDeltaDto>,
    /// 全部扩展名（按 delta 降序，不截断）
    pub exts: Vec<ExtDeltaDto>,
    pub bursts: Vec<BurstDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffView {
    pub summary: String,
    pub old_created: String,
    pub new_created: String,
    pub old_root: String,
    pub new_root: String,
    pub old_total: u64,
    pub new_total: u64,
    pub added: u64,
    pub removed: u64,
    pub grew: u64,
    pub shrank: u64,
    pub same: u64,
    pub added_bytes: u64,
    pub removed_bytes: u64,
    pub grew_bytes: u64,
    pub shrank_bytes: u64,
    pub net_delta: i64,
    pub rows_total: u64,
    pub rows: Vec<DiffRowDto>,
    /// 增长归因（E1）。`None` = 这次对比没算（不会出现在界面的对比流程里，
    /// 留 `Option` 是为了 CSV / 老调用方不必被迫消费它）。
    pub attr: Option<AttributionDto>,
    /// 两份快照的扫描根不同 —— 结果很可能没有意义，界面要显眼提示
    pub root_mismatch: bool,
    /// 旧快照存的时候就被截断过，丢了多少条。
    ///
    /// 这个必须显示：截断的快照里"没有"的文件，在对比时会被判成**新增**，
    /// 用户很可能照着这份清单去删实际上一直存在的文件。
    pub old_truncated: u64,
    pub new_truncated: u64,
    pub csv: String,
}

/// 用某份快照与**当前**扫描结果对比。方向固定为「快照 → 现在」，
/// 所以「新增/变大」都是相对现在说的，界面上不必让用户选方向。
///
/// 立刻返回，结果通过 `diff:done` 推给前端（进度走 `diff:progress`）。
/// 读几百 MB 的快照 + 归并两百万条要好几秒，同步做等于让窗口"未响应"。
#[tauri::command]
pub fn snapshot_compare(
    app: AppHandle,
    hub: State<'_, Arc<DiffHub>>,
    path: String,
    top: Option<usize>,
    min_delta: Option<String>,
    order: Option<String>,
) -> Result<(), String> {
    let p = path.trim();
    if p.is_empty() {
        return Err("请填写要对比的快照路径".into());
    }
    // 参数类错误在开工**之前**返回：前端会立刻弹提示，
    // 而不是先等几秒再从事件里收到一句"体积看不懂"。
    let opts = DiffOptions {
        min_delta: parse_size_input(min_delta.as_deref().unwrap_or(""))?,
        top: top.unwrap_or(300),
        order: match order.as_deref() {
            Some("abs") => DiffOrder::ByAbsChange,
            Some("path") => DiffOrder::ByPath,
            _ => DiffOrder::ByGrowth,
        },
        // 归因与明细是互补的两份答案，界面上总是两个都显示 ——
        // 所以这里无条件开（开销是归并过程中的一次哈希累加，可以忽略）。
        attr: Some(core_scan::AttrOptions::default()),
    };
    if hub.is_running() {
        return Err("已有一次快照作业在进行中".into());
    }
    {
        // 有没有扫描结果也要先查：没有的话后面几秒全白等
        let shared = app.state::<Shared>();
        let st = shared.lock().map_err(|_| "状态锁被污染".to_string())?;
        if st.scanning {
            return Err("正在扫描：扫描结束后索引树才是稳定的，请稍后再试".into());
        }
        let _ = st.loaded()?;
    }
    {
        let mut g = hub.lock()?;
        g.running = true;
        g.kind = "compare".to_string();
        g.stage = "准备中".to_string();
        g.ratio = 0.0;
        g.error = None;
    }

    let hub_arc: Arc<DiffHub> = Arc::clone(&*hub);
    let path_owned = p.replace('/', "\\");
    std::thread::spawn(move || {
        let mut rep = Reporter::new(hub_arc, app.clone(), "compare");

        rep.stage("读取快照文件", 0.0);
        let text = match Snapshot::read_to_string_with(std::path::Path::new(&path_owned), &mut |d, t| {
            rep.at(ratio_of(d, t) * W_READ_FILE);
        }) {
            Ok(s) => s,
            Err(e) => {
                rep.fail(e);
                return;
            }
        };

        rep.stage("解析快照", W_READ_FILE);
        let old = match Snapshot::from_tsv_with(&text, &mut |d, t| {
            rep.at(W_READ_FILE + ratio_of(d, t) * W_PARSE);
        }) {
            Ok(s) => s,
            Err(e) => {
                rep.fail(e);
                return;
            }
        };

        rep.stage("建立当前快照", W_READ_FILE + W_PARSE);
        let (new_snap, _total_size) =
            match capture_now(&app, &SnapshotOptions::default(), &mut |d, t| {
                rep.at(W_READ_FILE + W_PARSE + ratio_of(d, t) * W_CAPTURE);
            }) {
                Ok(v) => v,
                Err(e) => {
                    rep.fail(e);
                    return;
                }
            };

        rep.stage("对比并排序", W_READ_FILE + W_PARSE + W_CAPTURE);
        let report = diff_with(&old, &new_snap, &opts, &mut |d, t| {
            rep.at(W_READ_FILE + W_PARSE + W_CAPTURE + ratio_of(d, t) * W_DIFF);
        });
        let mut view = diff_view(&old, &new_snap, &report);
        // 创建时间要碰磁盘，放在"对比并排序"之后、收尾之前：此时结果已经是
        // 最终的那几百行，stat 几百次不影响整体耗时（相对读几十 MB 快照而言）。
        fill_ctimes(&mut view.rows);

        rep.finish();
        if let Ok(mut g) = rep.hub.lock() {
            g.result = Some(view.clone());
        }
        let _ = app.emit("diff:done", view);
    });
    Ok(())
}

/// 当前作业状态（打开视图时补一次，之后靠事件）。
///
/// 名字叫 `diff_status` 而不是 `diff_view`：后者是本文件里"报告 → DTO"那个
/// 私有转换函数，两个同名会让读代码的人分不清哪个是命令。
#[tauri::command]
pub fn diff_status(hub: State<'_, Arc<DiffHub>>) -> DiffStatusDto {
    hub.view()
}

/// 从完整路径里取文件名。路径分隔符两种都认（快照里的路径可能是 `/`，
/// 也可能是 `\\` —— 有的用户会把快照手工编辑成另一种写法）。
fn file_name_of(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// 给**已经列出来的**那几百行补创建时间。
///
/// 索引里不存 ctime，快照里也没有它（加一列要升格式版本，而格式是给别人
/// 读的），所以这里按需 stat 一次。行数由 `top` 决定（默认 300），
/// 每次 stat 是纯元数据读，几百次在毫秒级 —— 放在后台线程里做，
/// 用户看到进度条的同时这些数字就已经填好了。
///
/// **失败一律写 0**：文件可能已被删除（这正是差异行说的）、也可能没权限。
/// 创建时间取不到不该让整份对比失败。
fn fill_ctimes(rows: &mut [DiffRowDto]) {
    for row in rows {
        let created = std::fs::metadata(row.path.replace('/', "\\"))
            .and_then(|m| m.created())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        row.ctime = created;
    }
}

fn diff_view(old: &Snapshot, new: &Snapshot, rep: &DiffReport) -> DiffView {
    let root_mismatch = !old.root.is_empty() && !new.root.is_empty() && old.root != new.root;
    DiffView {
        summary: diff_summary_cn(rep),
        old_created: core_io::util::local_time(old.created_unix),
        new_created: core_io::util::local_time(new.created_unix),
        old_root: old.root.clone(),
        new_root: new.root.clone(),
        old_total: rep.old_total,
        new_total: rep.new_total,
        added: rep.added,
        removed: rep.removed,
        grew: rep.grew,
        shrank: rep.shrank,
        same: rep.same,
        added_bytes: rep.added_bytes,
        removed_bytes: rep.removed_bytes,
        grew_bytes: rep.grew_bytes,
        shrank_bytes: rep.shrank_bytes,
        net_delta: rep.net_delta,
        rows_total: rep.rows_total,
        rows: rep
            .rows
            .iter()
            .map(|r| DiffRowDto {
                kind: r.kind.key().to_string(),
                kind_cn: r.kind.label_cn().to_string(),
                delta: r.delta,
                old_size: r.old_size,
                new_size: r.new_size,
                path: r.path.clone(),
                name: file_name_of(&r.path),
                // 取"现在"的那一侧：新增/变大/变小看新快照，删除只能看旧快照
                mtime: if r.new_mtime != 0 { r.new_mtime } else { r.old_mtime },
                ctime: 0,
            })
            .collect(),
        // CSV 由引擎生成：界面「导出」按钮与 CLI 的 `--csv` 必须一模一样
        csv: rows_to_csv(&rep.rows),
        attr: rep.attribution.as_ref().map(|a| AttributionDto {
            dir_depth: a.dir_depth,
            total_delta: a.total_delta,
            files: a.files,
            dirs_total: a.dirs_total,
            dirs_up: a
                .dirs_up
                .iter()
                .map(|d| DirDeltaDto {
                    dir: d.dir.clone(),
                    delta: d.delta,
                    added: d.added,
                    removed: d.removed,
                    grew: d.grew,
                    shrank: d.shrank,
                    files: d.files,
                })
                .collect(),
            dirs_down: a
                .dirs_down
                .iter()
                .map(|d| DirDeltaDto {
                    dir: d.dir.clone(),
                    delta: d.delta,
                    added: d.added,
                    removed: d.removed,
                    grew: d.grew,
                    shrank: d.shrank,
                    files: d.files,
                })
                .collect(),
            exts: a
                .exts
                .iter()
                .map(|e| ExtDeltaDto {
                    ext: e.ext.clone(),
                    delta: e.delta,
                    added: e.added,
                    removed: e.removed,
                    files: e.files,
                })
                .collect(),
            bursts: a
                .bursts
                .iter()
                .map(|b| BurstDto {
                    hour_unix: b.hour_unix,
                    delta: b.delta,
                    files: b.files,
                })
                .collect(),
        }),
        root_mismatch,
        old_truncated: rep.old_truncated,
        new_truncated: rep.new_truncated,
    }
}

/// 保存差异行 CSV。内容由 [`snapshot_compare`] 返回的 `csv` 字段给，
/// 所以**不在这里重新生成** —— 否则界面显示的行和导出的行可能对不上。
#[tauri::command]
pub fn diff_export_csv(path: String, body: String) -> Result<usize, String> {
    let p = export_target(&path, "diff", "csv")?;
    let n = body.lines().count().saturating_sub(1);
    write_text_file(&p, &body)?;
    Ok(n)
}

/// 导出单文件 HTML 报告（E3）。
///
/// 排版在引擎里（`core_scan::report`），这里只负责取当前索引 + 落盘 ——
/// 与 CLI 的 `filewatch report` 是同一份模板，所以两边的数字与版面必须一致。
///
/// 同步执行：它只是一次内存遍历 + 一次写文件。真机上百万级的卷大约 1 秒，
/// 而挂进度条要动事件的时序（用户点完立刻看到"已导出"更符合这条操作的性质）。
#[tauri::command]
pub fn export_report(
    state: State<'_, Shared>,
    path: String,
    top: Option<usize>,
) -> Result<String, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    if st.scanning {
        return Err("正在扫描：报告要基于稳定的索引树，扫完再导".into());
    }
    let l = st.loaded()?;
    let tree = l.tree();
    let root = l
        .session
        .cfg
        .scan_root
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| tree.path_of(tree.root));
    let ext = core_index::ExtensionTable::build(tree);
    let opts = core_scan::report::ReportOptions {
        root,
        created_unix: core_io::util::unix_secs(),
        top: top.unwrap_or(30).max(1),
        ..Default::default()
    };
    let html = core_scan::report::build_html(tree, &ext, l.session.path_remap().as_ref(), &opts);
    let target = export_target(&path, "report", "html")?;
    write_text_file(&target, &html)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_handles_both_separators() {
        assert_eq!(file_name_of(r"C:\a\b\报表 v2.pdf"), "报表 v2.pdf");
        assert_eq!(file_name_of("C:/a/b/report.pdf"), "report.pdf");
        assert_eq!(file_name_of("report.pdf"), "report.pdf");
        assert_eq!(file_name_of(""), "");
    }

    /// 创建时间按需 stat：存在的文件要有值，已删除的行必须是 0（不是乱码值），
    /// 而且不能因为某一行失败就中断后面的行。
    #[test]
    fn fill_ctimes_reads_existing_files_and_zeroes_missing_ones() {
        let owned = core_io::test_dir::TestDir::new("ctime").unwrap();
        let live = owned.path().join("live.txt");
        std::fs::write(&live, b"x").unwrap();
        let mut rows = vec![
            DiffRowDto { kind: "grew".into(), kind_cn: "变大".into(), delta: 1,
                old_size: 0, new_size: 1, path: live.to_string_lossy().into(),
                name: "live.txt".into(), mtime: 0, ctime: 0 },
            DiffRowDto { kind: "removed".into(), kind_cn: "删除".into(), delta: -1,
                old_size: 1, new_size: 0,
                path: owned.path().join("gone-1234.txt").to_string_lossy().into(),
                name: "gone-1234.txt".into(), mtime: 0, ctime: 0 },
            DiffRowDto { kind: "added".into(), kind_cn: "新增".into(), delta: 1,
                old_size: 0, new_size: 1, path: live.to_string_lossy().into(),
                name: "live.txt".into(), mtime: 0, ctime: 0 },
        ];
        fill_ctimes(&mut rows);
        assert!(rows[0].ctime > 1_600_000_000, "存在的文件应读到创建时间：{}", rows[0].ctime);
        assert_eq!(rows[1].ctime, 0, "已删除的文件只能是 0");
        assert!(rows[2].ctime > 0, "一行失败不能影响后面的行");
    }

    /// 浏览按钮选的是**文件夹**，手填的是**文件名**，两种都得能用。
    #[test]
    fn export_target_accepts_both_a_folder_and_a_file_name() {
        let owned = core_io::test_dir::TestDir::new("export-target").unwrap();
        let dir = owned.path().to_string_lossy().to_string();

        // 目录的三种写法（选出来的、手填带尾杠的、用斜杠的）都补默认文件名
        for given in [dir.clone(), format!("{dir}\\"), dir.replace('\\', "/")] {
            let got = export_target(&given, "snapshot", "tsv").unwrap();
            assert!(got.starts_with(&dir), "{given:?} -> {got:?}");
            assert!(got.contains("filewatch-snapshot-"), "{got:?} 该用统一的文件名");
            assert!(got.ends_with(".tsv"), "{got:?}");
        }
        // 文件名原样保留 —— 包括那个还没被创建出来的新文件
        for name in ["mine.tsv", "not-created-yet.tsv"] {
            let file = format!("{dir}\\{name}");
            assert_eq!(export_target(&file, "snapshot", "tsv").unwrap(), file);
        }
        // 空串仍然是错误，不能兜底成"导出到当前目录"
        assert!(export_target("  ", "snapshot", "tsv").is_err());
    }

    /// 差异行的 CSV 必须原样来自引擎（这里只验证转换没丢字段）。
    #[test]
    fn diff_rows_keep_every_field() {
        let old = Snapshot::from_tsv(&format!(
            "#filewatch-snapshot\t1\n#root\tC:\\\n#created\t1\n#count\t1\nC:\\a.txt\t100\t1\n"
        ))
        .unwrap();
        let new = Snapshot::from_tsv(&format!(
            "#filewatch-snapshot\t1\n#root\tC:\\\n#created\t2\n#count\t1\nC:\\a.txt\t300\t2\n"
        ))
        .unwrap();
        let rep = diff(&old, &new, &DiffOptions { top: 0, ..Default::default() });
        let v = diff_view(&old, &new, &rep);
        assert_eq!(v.rows.len(), 1);
        assert_eq!(v.rows[0].kind, "grew");
        assert_eq!(v.rows[0].kind_cn, "变大");
        assert_eq!(v.rows[0].delta, 200);
        assert_eq!(v.rows[0].new_size, 300);
        assert!(!v.root_mismatch);
        assert!(v.csv.starts_with("kind,delta,old_size,new_size,path"));
    }

    #[test]
    fn root_mismatch_is_flagged() {
        let a = Snapshot::from_tsv("#filewatch-snapshot\t1\n#root\tC:\\\n#count\t0\n").unwrap();
        let b = Snapshot::from_tsv("#filewatch-snapshot\t1\n#root\tD:\\\n#count\t0\n").unwrap();
        let rep = diff(&a, &b, &DiffOptions::default());
        assert!(diff_view(&a, &b, &rep).root_mismatch);
    }

    /// 截断过的快照必须把"丢了多少条"带到界面上。
    ///
    /// 少了这个字段，用户会看到一份"新增 3000 个文件"的清单 —— 那 3000 个
    /// 其实一直都在，只是没被记进快照。界面如果照它提示用户去清理，就是在
    /// 让用户删掉自己的文件。
    #[test]
    fn truncated_snapshots_are_reported() {
        let tsv = "#filewatch-snapshot\t1\n#root\tC:\\\n#count\t1\nC:\\a.txt\t100\t1\n";
        let old = Snapshot::from_tsv(tsv).unwrap();
        let mut new = Snapshot::from_tsv(tsv).unwrap();
        new.truncated = 7;

        let rep = diff(&old, &new, &DiffOptions::default());
        let v = diff_view(&old, &new, &rep);
        assert_eq!(v.old_truncated, 0);
        assert_eq!(v.new_truncated, 7);
        assert!(v.summary.contains("截断"), "摘要里必须写明截断：{}", v.summary);
    }

    /// 界面输入框里的体积写法必须与 CLI 的 `--min-size` 同源。
    #[test]
    fn size_input_uses_the_shared_syntax() {
        assert_eq!(parse_size_input("").unwrap(), 0, "留空 = 不设下限");
        assert_eq!(parse_size_input("   ").unwrap(), 0);
        assert_eq!(parse_size_input(" 1M ").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_input("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_size_input("1GiB").unwrap(), 1024 * 1024 * 1024, "大小写不敏感");
        assert_eq!(parse_size_input("1048576").unwrap(), 1024 * 1024, "纯数字 = 字节");
        // 看不懂必须报错，不能静默退回 0：那等于"写错单位 → 悄悄变成不设下限"，
        // 而用户以为自己设了 1 MB 的下限
        assert!(parse_size_input("1MB_x").is_err());
        assert!(parse_size_input("abc").is_err());
    }

    /// 三种导出的默认名必须是同一套拼法（同目录、同时间戳格式、不同后缀）。
    /// 前端不再自己拼时间戳 —— 那条路会在本地时区与 UTC 之间悄悄分叉。
    #[test]
    fn default_export_paths_share_one_timestamp() {
        let snap = analyze_default_path("snapshot".into()).unwrap();
        let dups = analyze_default_path("duplicates".into()).unwrap();
        let df = analyze_default_path("diff".into()).unwrap();

        assert!(snap.ends_with(".tsv"), "{snap}");
        assert!(dups.ends_with(".csv") && df.ends_with(".csv"));
        assert!(snap.contains("\\filewatch-snapshot-"), "{snap}");
        assert!(dups.contains("\\filewatch-duplicates-"), "{dups}");
        assert!(df.contains("\\filewatch-diff-"), "{df}");
        // 同一次运行里三者应落在同一个目录（取桌面那一段）
        let dir_of = |p: &str| p.rsplit_once('\\').unwrap().0.to_string();
        assert_eq!(dir_of(&snap), dir_of(&dups));
        assert_eq!(dir_of(&dups), dir_of(&df));

        // 未知类型必须报错，不能悄悄给一个"看起来也对"的名字
        assert!(analyze_default_path("whatever".into()).is_err());
    }

    /// `DupHub::view` 在锁被污染时要给出明确错误，而不是 panic。
    #[test]
    fn poisoned_hub_reports_instead_of_panicking() {
        let hub = DupHub::new();
        {
            let c = Arc::clone(&hub);
            let _ = std::thread::spawn(move || {
                let _g = c.0.lock().unwrap();
                panic!("故意污染锁");
            })
            .join();
        }
        let v = hub.view();
        assert!(!v.running);
        assert!(v.error.is_some(), "锁被污染时必须给出错误文案");
        assert!(!hub.is_running());
    }
}
