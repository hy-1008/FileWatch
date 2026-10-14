//! F6/F7/F8：实时增量监控、阈值预警、变更流水。
//!
//! # 这一层做什么、不做什么
//!
//! 事件从哪来、怎么合并、什么时候该报警，全部在 `core-monitor` / `core-scan`
//! 里（CLI 用的是同一套）。这里只负责三件事：
//!
//! 1. **驱动轮询**：一个后台线程按固定间隔调 `Session::poll`；
//! 2. **保存最近若干条**（环形缓冲）：给界面回放，并作为 CSV 导出的数据源；
//! 3. **把变更与预警推给前端**（`monitor:tick` 事件）。
//!
//! # 为什么用后台线程 + 事件，而不是让前端轮询
//!
//! 轮询是 `Session::poll` 的节奏（默认 500 ms），前端再轮一次就成了两套节拍，
//! 中间还会积压。事件推送是单向的，前端只负责显示。
//!
//! # 为什么监控必须占用「同一个 Session」
//!
//! 监控会**改索引树**（新增/删除/改名/体积变化），而界面的面积图、文件夹树
//! 都读这棵树。若给监控另开一个会话，就变成两棵树各说各话 —— 面积图永远
//! 看不到变化。所以：**同一棵树**，界面靠 [`crate::state::Loaded::generation`]
//! 的代数变化知道自己该重画。
//!
//! 代价是「扫描」与「监控」不能同时进行（扫描会换掉整棵树），所以
//! [`start`] 与 `commands::scan_start` 互相拒绝。

use crate::state::Shared;
use core_monitor::{Alert, PendingEvent, Rule};
use core_scan::{ScanPhase, Session, SessionConfig};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

/// 内存里保留多少条变更（导出 CSV 的数据源也是它）。
///
/// 4000 条 × 约 300 字节 ≈ 1.2 MB，可以忽略。再多的意义不大：界面一屏
/// 几十行，用户真要回溯应该开落盘/导出。
pub const RING_CAPACITY: usize = 4000;

/// 预警最多留多少条（预警比变更稀有得多）。
pub const ALERT_CAPACITY: usize = 500;

/// 一条 tick 事件最多带多少行。事件风暴（解压、`git checkout`）时一轮
/// 可能有上万条，全塞进 IPC 会把前端渲染线程淹掉（文档 R5）。
const MAX_ROWS_PER_EVENT: usize = 100;

/// 游标落盘间隔（见 `core_scan::Session::checkpoint`）。
const CHECKPOINT_MS: u64 = 30_000;

/// 轮询的下限。比这更密只是白烧 CPU：变更感应本身有几百毫秒的合并窗口。
const MIN_INTERVAL_MS: u64 = 100;

// ── 传给前端的传输对象 ──────────────────────────────────────────────────────

/// 一条规则的可读形态。`text` 是用户原样输入的字符串，用来回显。
#[derive(Debug, Clone, Serialize)]
pub struct RuleInfo {
    /// `"absolute"` / `"growth"` / `"rate"`
    pub kind: String,
    /// 中文名（取自 `RuleKind::label_cn`，与 CLI 同源）
    pub kind_cn: String,
    pub threshold: u64,
    /// 仅速率规则使用
    pub window_ms: u64,
    pub text: String,
}

impl RuleInfo {
    fn of(r: &Rule, text: &str) -> RuleInfo {
        RuleInfo {
            kind: r.kind.as_str().to_string(),
            kind_cn: r.kind.label_cn().to_string(),
            threshold: r.threshold,
            window_ms: r.window_ms,
            text: text.to_string(),
        }
    }
}

/// 本次监控的配置回执。
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub paths: Vec<String>,
    pub rules: Vec<RuleInfo>,
    pub backend: String,
    /// 增量来源（「USN Journal」/「目录监视」/「无」）
    pub incremental: String,
    pub started_ms: u64,
    pub interval_ms: u64,
    /// 变更流水落盘路径（`None` 表示只在内存里）
    pub changelog: Option<String>,
    /// 会话建立的告警（降级、游标自愈、日志写在监视目录内部…）
    pub warnings: Vec<String>,
    /// 为建立增量来源重新扫描过一次
    pub reopened: bool,
    /// 监控范围说明（整卷 / 某个子目录）
    pub scope: String,
}

/// 变更流水的一行。
#[derive(Debug, Clone, Serialize)]
pub struct ChangeRow {
    /// 单调递增序号。前端用它排序、去重（事件可能乱序到达）
    pub seq: u64,
    /// 相对监控起点的毫秒
    pub at_ms: u64,
    /// `"created"` / `"deleted"` / `"renamed"` / `"modified"` / …
    pub kind: String,
    pub kind_cn: String,
    pub is_dir: bool,
    pub path: String,
    pub name: String,
    /// 当前大小（文件才有意义）
    pub size: u64,
    /// 有符号的大小变化
    pub delta: i64,
    /// 「体积」的显示文本 —— 由 `core_monitor::event_size_text` 给出，
    /// 与 CLI 的变更行**同一个实现**（显示哪个数只能有一个答案）
    pub size_text: String,
    /// 合并条数 / 重命名前的旧名
    pub suffix: Option<String>,
    /// 合并进来的原始事件数（前端据此标"合并 N 条"）
    pub merged: u32,
    pub old_name: Option<String>,
}

/// 预警的一条。
#[derive(Debug, Clone, Serialize)]
pub struct AlertRow {
    pub seq: u64,
    pub at_ms: u64,
    pub kind: String,
    pub kind_cn: String,
    pub path: String,
    pub threshold: u64,
    pub current: u64,
    pub baseline: u64,
    pub observed: u64,
    pub message: String,
}

/// 状态快照（状态栏与面板头部用）。
#[derive(Debug, Clone, Serialize)]
pub struct MonitorSnapshot {
    pub running: bool,
    pub info: Option<MonitorInfo>,
    pub total_events: u64,
    pub total_alerts: u64,
    /// 队列溢出次数（文档 §9「事件零丢失」的对账依据）
    pub backpressure: u64,
    pub queue_dropped: u64,
    /// 环形缓冲挤掉的条数（只影响界面回放，不影响计数）
    pub evicted: u64,
    pub still_pending: usize,
    pub elapsed_ms: u64,
    /// 树的代数 —— 前端拿它决定要不要重画面积图
    pub generation: u64,
    /// 启动失败/运行中出错的原因
    pub error: Option<String>,
}

/// 树的实时总量（监控期间状态栏用）。
///
/// 监控会改树，所以扫描摘要里的那几个数字**会过期**。带上这个是让状态栏
/// 显示实时值 —— 否则用户会看着一个不再变化的"逻辑 1.2 TB"发呆。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TreeTotals {
    pub dirs: u64,
    pub files: u64,
    pub logical: u64,
    pub alloc: u64,
}

/// 一轮轮询的结果（推给前端）。
#[derive(Debug, Clone, Serialize)]
pub struct MonitorTick {
    pub elapsed_ms: u64,
    /// 本轮应用的事件数（可能大于 `rows.len()`，见 `truncated`）
    pub applied: usize,
    pub rows: Vec<ChangeRow>,
    /// 行被截断了（事件风暴）
    pub truncated: bool,
    pub alerts: Vec<AlertRow>,
    /// 合并后的预警通知（一行文本，前端拿来弹提示）
    pub notification: Option<String>,
    pub total_events: u64,
    pub total_alerts: u64,
    pub backpressure: bool,
    pub still_pending: usize,
    pub generation: u64,
    /// 树当前的实时总量
    pub totals: TreeTotals,
}

/// 分页取流水的返回。
#[derive(Debug, Clone, Serialize)]
pub struct ChangePage {
    pub rows: Vec<ChangeRow>,
    /// 符合筛选条件的总条数（不是返回的条数）
    pub matched: usize,
}

// ── 内部状态 ────────────────────────────────────────────────────────────────

/// 环形缓冲里的一条变更。存 `PendingEvent` 而不是渲染好的行：
/// CSV 导出走 `core_monitor::format_event_csv`，那一份实现要的是事件本身。
#[derive(Clone)]
struct Logged {
    seq: u64,
    at_ms: u64,
    ev: PendingEvent,
    path: String,
}

#[derive(Clone)]
struct LoggedAlert {
    seq: u64,
    at_ms: u64,
    alert: Alert,
}

#[derive(Default)]
struct Ring {
    /// 新的在**后面**（`push_back`），读取时反向遍历
    changes: VecDeque<Logged>,
    alerts: VecDeque<LoggedAlert>,
    total_events: u64,
    total_alerts: u64,
    evicted: u64,
    backpressure: u64,
    queue_dropped: u64,
    still_pending: usize,
    next_seq: u64,
    error: Option<String>,
}

impl Ring {
    fn push_change(&mut self, at_ms: u64, ev: PendingEvent, path: String) -> ChangeRow {
        self.next_seq += 1;
        let seq = self.next_seq;
        let row = change_row(seq, at_ms, &ev, &path);
        self.changes.push_back(Logged { seq, at_ms, ev, path });
        while self.changes.len() > RING_CAPACITY {
            self.changes.pop_front();
            self.evicted += 1;
        }
        row
    }

    fn push_alert(&mut self, at_ms: u64, alert: Alert) -> AlertRow {
        self.next_seq += 1;
        let seq = self.next_seq;
        let row = alert_row(seq, at_ms, &alert);
        self.alerts.push_back(LoggedAlert { seq, at_ms, alert });
        while self.alerts.len() > ALERT_CAPACITY {
            self.alerts.pop_front();
        }
        row
    }

    /// 新的在前。
    fn tail(&self, limit: usize, kind: Option<&str>) -> Vec<ChangeRow> {
        self.changes
            .iter()
            .rev()
            .filter(|l| kind.map(|k| l.ev.kind.as_str() == k).unwrap_or(true))
            .take(limit)
            .map(|l| change_row(l.seq, l.at_ms, &l.ev, &l.path))
            .collect()
    }

    fn matched(&self, kind: Option<&str>) -> usize {
        match kind {
            None => self.changes.len(),
            Some(k) => self
                .changes
                .iter()
                .filter(|l| l.ev.kind.as_str() == k)
                .count(),
        }
    }
}

fn change_row(seq: u64, at_ms: u64, ev: &PendingEvent, path: &str) -> ChangeRow {
    ChangeRow {
        seq,
        at_ms,
        kind: ev.kind.as_str().to_string(),
        kind_cn: ev.kind.label_cn().to_string(),
        is_dir: ev.is_dir,
        path: path.to_string(),
        name: ev.name.clone(),
        size: ev.new_size,
        delta: ev.size_delta,
        size_text: core_monitor::event_size_text(ev),
        suffix: core_monitor::event_suffix(ev),
        merged: ev.merged,
        old_name: ev.old_name.clone(),
    }
}

fn alert_row(seq: u64, at_ms: u64, a: &Alert) -> AlertRow {
    AlertRow {
        seq,
        at_ms,
        kind: a.kind.as_str().to_string(),
        kind_cn: a.kind.label_cn().to_string(),
        path: a.path.clone(),
        threshold: a.threshold,
        current: a.current_size,
        baseline: a.baseline_size,
        observed: a.observed,
        message: a.message.clone(),
    }
}

/// 监控会话的共享状态。Tauri 直接管理它的 `Arc`。
pub struct MonitorHub {
    stop: AtomicBool,
    running: AtomicBool,
    ring: Mutex<Ring>,
    info: Mutex<Option<MonitorInfo>>,
    /// 树变更代数的计数器（从 `Loaded` 那里借来的那一份）
    generation: Mutex<Option<Arc<AtomicU64>>>,
    started: Mutex<Option<Instant>>,
}

impl MonitorHub {
    pub fn new() -> Arc<MonitorHub> {
        Arc::new(MonitorHub {
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            ring: Mutex::new(Ring::default()),
            info: Mutex::new(None),
            generation: Mutex::new(None),
            started: Mutex::new(None),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 请求停止。幂等 —— 线程会在一个轮询周期内（最长 `interval_ms`）退出。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// 等线程把收尾做完（吐出剩余事件、保存游标、刷盘）。
    ///
    /// 返回是否真的退了。**绝不无限等**：退出流程里卡住比丢几秒变更更糟，
    /// 所以超时就放行。
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while self.is_running() {
            if t0.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    fn generation_of(&self) -> u64 {
        self.generation
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|a| a.load(Ordering::Acquire)))
            .unwrap_or(0)
    }

    fn snapshot(&self) -> MonitorSnapshot {
        let r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        MonitorSnapshot {
            running: self.is_running(),
            info: self.info.lock().ok().and_then(|i| i.clone()),
            total_events: r.total_events,
            total_alerts: r.total_alerts,
            backpressure: r.backpressure,
            queue_dropped: r.queue_dropped,
            evicted: r.evicted,
            still_pending: r.still_pending,
            elapsed_ms: self
                .started
                .lock()
                .ok()
                .and_then(|s| *s)
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0),
            generation: self.generation_of(),
            error: r.error.clone(),
        }
    }
}

// ── 时间 ────────────────────────────────────────────────────────────────────

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── 启动计划 ────────────────────────────────────────────────────────────────

/// 启动一次监控所需的全部输入（在命令里校验、在线程里执行）。
struct Plan {
    /// 归一化之后的绝对路径（已经确认在索引树里能找到）
    targets: Vec<String>,
    rules: Vec<Rule>,
    /// 规则原文（回显用，与 `rules` 一一对应）
    rule_texts: Vec<String>,
    changelog: Option<PathBuf>,
    interval_ms: u64,
    create_journal: bool,
}

/// 开始监控（异步）。立刻返回，进度与结果分别走 `scan:progress` / `monitor:*` 事件。
///
/// `paths` 是监控目标（必须是当前索引树里的路径）；`rules` 是人读写法
/// （`abs=1G` / `grow=500M` / `rate=10M/60s`），解析走 `core_monitor::spec`，
/// 与 CLI 的 `--rule` 完全同一条语法。
#[tauri::command]
pub fn monitor_start(
    app: AppHandle,
    state: State<'_, Shared>,
    hub: State<'_, Arc<MonitorHub>>,
    paths: Vec<String>,
    rules: Vec<String>,
    changelog: Option<String>,
    interval_ms: Option<u64>,
    create_journal: Option<bool>,
) -> Result<(), String> {
    if hub.is_running() {
        return Err("已经在监控中".into());
    }

    // 目标路径归一：调用方的 shell 可能换过分隔符（实测 git-bash 把
    // `C:\x` 传成 `C:/x`），不归一就会一路带进路径比较里。
    let targets: Vec<String> = paths
        .iter()
        .map(|p| p.trim().replace('/', "\\"))
        .filter(|p| !p.is_empty())
        .collect();
    if targets.is_empty() {
        return Err("至少要指定一个监控目标".into());
    }

    let parsed = core_monitor::parse_rules(&rules).map_err(|e| e)?;
    let texts: Vec<String> = rules
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let interval = interval_ms.unwrap_or(core_monitor::DEFAULT_POLL_INTERVAL_MS).max(MIN_INTERVAL_MS);

    {
        let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
        if st.scanning {
            return Err("正在扫描，等它结束后再开始监控".into());
        }
        if st.loaded.is_none() {
            return Err("还没有扫描结果：请先扫描一次，监控需要在索引树上注册目标".into());
        }
    }

    // 标记为运行中（真正的线程在下面起来）。放在这里而不是线程里，
    // 是为了让「两个请求同时进来」的竞态被这一层挡住。
    hub.running.store(true, Ordering::Release);
    hub.stop.store(false, Ordering::Release);
    {
        let mut r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
        *r = Ring::default();
    }
    *hub.info.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *hub.started.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());

    let plan = Plan {
        targets,
        rules: parsed,
        rule_texts: texts,
        changelog: changelog.filter(|s| !s.trim().is_empty()).map(PathBuf::from),
        interval_ms: interval,
        create_journal: create_journal.unwrap_or(false),
    };

    let hub2 = hub.inner().clone();
    std::thread::spawn(move || run(app, hub2, plan));
    Ok(())
}

/// 结束监控（收尾是异步的，但这里**等一小会儿**）。
///
/// 等这一下的理由：收尾要吐出 debounce 池里剩下的变更并保存游标，通常
/// 几十毫秒就完事。不等的话，用户点完「停止」立刻点「扫描」会被
/// [`start`] 的互斥检查挡回来（那时线程还没退），提示"正在监控"，
/// 看着像是按钮坏了。上限 800 ms，超时就放行。
#[tauri::command]
pub fn monitor_stop(hub: State<'_, Arc<MonitorHub>>) -> Result<bool, String> {
    if !hub.is_running() {
        return Ok(false);
    }
    hub.request_stop();
    hub.wait_idle(Duration::from_millis(800));
    Ok(true)
}

#[tauri::command]
pub fn monitor_snapshot(hub: State<'_, Arc<MonitorHub>>) -> MonitorSnapshot {
    hub.snapshot()
}

/// 分页取流水（新的在前）。`kind` 为空表示不筛选。
#[tauri::command]
pub fn monitor_changes(
    hub: State<'_, Arc<MonitorHub>>,
    limit: Option<usize>,
    kind: Option<String>,
) -> ChangePage {
    let k = kind.as_deref().filter(|s| !s.is_empty());
    let r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
    ChangePage {
        rows: r.tail(limit.unwrap_or(500).min(2000), k),
        matched: r.matched(k),
    }
}

#[tauri::command]
pub fn monitor_alerts(hub: State<'_, Arc<MonitorHub>>, limit: Option<usize>) -> Vec<AlertRow> {
    let r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
    let n = limit.unwrap_or(200).min(ALERT_CAPACITY);
    r.alerts
        .iter()
        .rev()
        .take(n)
        .map(|a| alert_row(a.seq, a.at_ms, &a.alert))
        .collect()
}

/// CSV 的默认导出目录：桌面（有的话）→ 用户目录。
///
/// 只给**目录**不给文件名：文件名里的时间戳由前端生成 —— 浏览器/WebView 的
/// `Date` 本来就会本地时区换算，而引擎里那个 `civil_from_days` 是 UTC 口径
/// （CLI 用它打带 `Z` 的时间戳）。让两边各管各擅长的部分，比在 Rust 里
/// 现写一遍本地时区换算靠谱。
///
/// 不做"另存为"对话框：那要引 `tauri-plugin-dialog`，为一个输入框拉一个
/// 插件不值得（这一层的依赖越少越好）。
#[tauri::command]
pub fn monitor_default_export_dir() -> String {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    let desktop = home.join("Desktop");
    if desktop.is_dir() {
        desktop.to_string_lossy().to_string()
    } else {
        home.to_string_lossy().to_string()
    }
}

/// 把当前内存里的流水导出成 CSV。
///
/// 表格生成用 `core_monitor::format_event_csv` —— 与引擎落盘的 JSONL 是
/// 同一份事件的两种写法，列的含义、转义规则都在那一处。
#[tauri::command]
pub fn monitor_export_csv(
    hub: State<'_, Arc<MonitorHub>>,
    path: String,
    kind: Option<String>,
) -> Result<usize, String> {
    let p = path.trim();
    if p.is_empty() {
        return Err("请填写导出路径".into());
    }
    let p = p.replace('/', "\\");
    let k = kind.as_deref().filter(|s| !s.is_empty());

    let (body, n) = {
        let r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = String::with_capacity(64 * 1024);
        s.push_str(core_monitor::changelog::CSV_HEADER);
        s.push_str("\r\n");
        let mut n = 0usize;
        // 导出按**时间正序**（流水文件的口径），与界面的倒序列表相反
        for l in r.changes.iter().filter(|l| k.map(|k| l.ev.kind.as_str() == k).unwrap_or(true)) {
            s.push_str(&core_monitor::changelog::format_event_csv(&l.ev, &l.path));
            s.push_str("\r\n");
            n += 1;
        }
        (s, n)
    };

    let path = PathBuf::from(&p);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
        }
    }
    std::fs::write(&path, body).map_err(|e| format!("写 CSV 失败：{e}"))?;
    Ok(n)
}

// ── 后台线程 ────────────────────────────────────────────────────────────────

/// 一轮「轮询 → 解析路径 → 记进环形缓冲」。
///
/// 抽成独立函数有两个理由：
/// 1. 线程里那段要同时持两把锁、还要发事件，混在一起不好读；
/// 2. **它能被测试直接驱动。** 界面点击没法自动化，而「轮询 → 路径 → 流水」
///    这条链的真实行为必须有真实目录监视器才能验证 —— 有了这个函数，
///    单测里就能真的建一个会话、真的写文件、真的等到行出现。
///
/// # 锁的顺序
///
/// 调用方按 **`AppState` → `ring`** 的顺序加锁。全模块只允许这一个顺序
/// （`monitor_changes` 等命令只碰 `ring`，不会反向去拿 `AppState`），
/// 所以不存在死锁路径。
///
/// 返回 `(载荷, 树是否被改过)`；`None` 表示轮询本身出错（错误已记进 ring）。
fn poll_once(
    session: &mut Session,
    ring: &mut Ring,
    elapsed_ms: u64,
    now: u64,
) -> Option<(MonitorTick, bool)> {
    let out = match session.poll(now) {
        Ok(o) => o,
        Err(e) => {
            ring.error = Some(format!("轮询出错：{e}"));
            return None;
        }
    };
    let dropped = session.engine.stats.queue_dropped;
    let totals = TreeTotals {
        dirs: session.tree.dir_count,
        files: session.tree.file_count,
        logical: session.tree.total_size,
        alloc: session.tree.total_alloc,
    };
    Some(absorb(session, ring, &out, elapsed_ms, totals, dropped))
}

/// 把一轮结果写进环形缓冲，并组装要推给前端的载荷。
///
/// `poll` 与收尾的 `shutdown` 都走它 —— 两条路的"记账"口径必须一致，
/// 否则收尾那几条变更会漏掉计数，或者少算预警。
fn absorb(
    session: &Session,
    ring: &mut Ring,
    out: &core_monitor::TickOutcome,
    elapsed_ms: u64,
    totals: TreeTotals,
    dropped: u64,
) -> (MonitorTick, bool) {
    // 先解析路径：只有拿到树才能做（`Tree::path_of` + 重映射）
    let remap = session.path_remap();
    let mut rows = Vec::with_capacity(out.events.len().min(MAX_ROWS_PER_EVENT));
    let mut all_visible: Vec<ChangeRow> = Vec::new();
    for ev in &out.events {
        let path = core_scan::paths::event_path(&session.tree, ev, remap.as_ref());
        let row = ring.push_change(elapsed_ms, ev.clone(), path);
        all_visible.push(row);
    }
    // 只把最近的 N 条推给前端：一轮上万条会把渲染线程淹掉（文档 R5）
    let truncated = all_visible.len() > MAX_ROWS_PER_EVENT;
    rows.extend(all_visible.split_off(all_visible.len().saturating_sub(MAX_ROWS_PER_EVENT)));

    let alert_rows: Vec<AlertRow> = out
        .alerts
        .iter()
        .map(|a| ring.push_alert(elapsed_ms, a.clone()))
        .collect();

    ring.total_events += out.applied as u64;
    ring.total_alerts += out.alerts.len() as u64;
    if out.backpressure {
        ring.backpressure += 1;
    }
    ring.queue_dropped = ring.queue_dropped.max(dropped);
    ring.still_pending = out.still_pending;

    let changed = !rows.is_empty() || !alert_rows.is_empty() || out.backpressure;
    let tick = MonitorTick {
        elapsed_ms,
        applied: out.applied,
        rows,
        truncated,
        alerts: alert_rows,
        notification: out
            .notification
            .as_ref()
            .map(core_monitor::alert::render_one_line),
        total_events: ring.total_events,
        total_alerts: ring.total_alerts,
        backpressure: out.backpressure,
        still_pending: ring.still_pending,
        // 由调用方盖章（只有它知道代数计数器）
        generation: 0,
        totals,
    };
    (tick, changed)
}

fn run(app: AppHandle, hub: Arc<MonitorHub>, plan: Plan) {
    let reopened = match ensure_source(&app, &plan) {
        Ok(v) => v,
        Err(e) => return fail(&app, &hub, e),
    };

    // 记住代数计数器：监控每应用一批事件就 +1，界面据此知道自己该重画。
    let gen_arc = {
        let handle = app.state::<Shared>();
        let g = match handle.inner().lock() {
            Ok(g) => g,
            Err(_) => return fail(&app, &hub, "状态锁被污染".into()),
        };
        match g.loaded.as_ref() {
            Some(l) => l.generation_arc(),
            None => return fail(&app, &hub, "会话在监控启动过程中消失了".into()),
        }
    };
    *hub.generation.lock().unwrap_or_else(|e| e.into_inner()) = Some(gen_arc.clone());

    // 注册监控目标 + 收集回执
    {
        let handle = app.state::<Shared>();
        let mut g = match handle.inner().lock() {
            Ok(g) => g,
            Err(_) => return fail(&app, &hub, "状态锁被污染".into()),
        };
        let Some(l) = g.loaded.as_mut() else {
            return fail(&app, &hub, "会话不见了".into());
        };
        let now = crate::commands::now_ms();
        let mut ok_paths = Vec::new();
        let mut missed = Vec::new();
        for t in &plan.targets {
            match l.session.add_target_by_path(t, plan.rules.clone(), now) {
                Some(_) => ok_paths.push(t.clone()),
                None => missed.push(t.clone()),
            }
        }
        if ok_paths.is_empty() {
            let msg = format!(
                "这些路径都不在索引树里，无法注册监控目标：{}（先扫描包含它们的目录）",
                missed.join("；")
            );
            return fail(&app, &hub, msg);
        }

        let mut warnings = l.session.warnings.clone();
        for m in &missed {
            warnings.push(format!("无法注册监控目标：{m} —— 索引树里找不到该路径"));
        }
        // 变更流水写在被监视目录内部是个很容易踩的坑：日志自身的增长会去顶
        // 阈值规则，而且"为什么一直有变更"根本查不出来。用户没写路径时
        // 也提醒一次可能的位置。
        if let Some(cl) = &l.session.cfg.changelog_path {
            let cl_s = cl.to_string_lossy().to_string();
            if let Some(hit) = ok_paths
                .iter()
                .find(|p| cl_s.to_lowercase().starts_with(&p.to_lowercase()))
            {
                warnings.push(format!(
                    "变更流水写在被监视目录内部（{cl_s} ← {hit}）：日志自身的增长会触发阈值规则"
                ));
            }
        }

        let scope = match l.session.cfg.scan_root.as_ref() {
            Some(r) => format!("目录 {}", r.to_string_lossy()),
            None => format!("整卷 {}", l.session.cfg.drive),
        };
        let info = MonitorInfo {
            paths: ok_paths,
            rules: plan
                .rules
                .iter()
                .zip(plan.rule_texts.iter())
                .map(|(r, t)| RuleInfo::of(r, t))
                .collect(),
            backend: l.session.backend.label_cn().to_string(),
            incremental: l.session.incremental_source_cn().to_string(),
            started_ms: now_ms(),
            interval_ms: plan.interval_ms,
            changelog: l
                .session
                .cfg
                .changelog_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            warnings,
            reopened,
            scope,
        };
        *hub.info.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());
        let _ = app.emit("monitor:started", info);
    }

    // 轮询循环
    let t0 = Instant::now();
    let epoch0 = now_ms();
    let mut last_ckpt = Instant::now();

    loop {
        if hub.stop.load(Ordering::Acquire) {
            break;
        }
        let elapsed = t0.elapsed().as_millis() as u64;
        let now = epoch0 + elapsed;

        // 取一轮结果。**锁的顺序固定为 AppState → ring**（见 poll_once 的注释）
        let step = {
            let handle = app.state::<Shared>();
            let mut guard = match handle.inner().lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            let Some(l) = guard.loaded.as_mut() else { break };
            let mut r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
            poll_once(&mut l.session, &mut r, elapsed, now)
        };

        if let Some((payload, changed)) = step {
            if changed {
                // 代数 +1：界面据此知道索引变了、面积图该重画
                let g = gen_arc.fetch_add(1, Ordering::AcqRel) + 1;
                let mut t = payload;
                t.generation = g;
                let _ = app.emit("monitor:tick", t);
            }

            if last_ckpt.elapsed() >= Duration::from_millis(CHECKPOINT_MS) {
                last_ckpt = Instant::now();
                let handle = app.state::<Shared>();
                if let Ok(mut g) = handle.inner().lock() {
                    if let Some(l) = g.loaded.as_mut() {
                        if let Err(e) = l.session.checkpoint() {
                            let mut r = hub.ring.lock().unwrap_or_else(|x| x.into_inner());
                            r.error = Some(format!("游标落盘失败：{e}"));
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(plan.interval_ms));
    }

    // 收尾：吐完 debounce 池里剩下的事件、保存游标
    {
        let handle = app.state::<Shared>();
        if let Ok(mut g) = handle.inner().lock() {
            if let Some(l) = g.loaded.as_mut() {
                let now = epoch0 + t0.elapsed().as_millis() as u64;
                match l.session.shutdown(now) {
                    Ok(out) => {
                        // 与轮询那条路走**同一个**记账函数：收尾这几条变更
                        // 的计数口径不能跟平时不一样
                        let elapsed = t0.elapsed().as_millis() as u64;
                        let totals = TreeTotals {
                            dirs: l.session.tree.dir_count,
                            files: l.session.tree.file_count,
                            logical: l.session.tree.total_size,
                            alloc: l.session.tree.total_alloc,
                        };
                        let dropped = l.session.engine.stats.queue_dropped;
                        let mut r = hub.ring.lock().unwrap_or_else(|x| x.into_inner());
                        absorb(&l.session, &mut r, &out, elapsed, totals, dropped);
                    }
                    Err(e) => {
                        let mut r = hub.ring.lock().unwrap_or_else(|x| x.into_inner());
                        r.error = Some(format!("收尾刷盘出错：{e}"));
                    }
                }
            }
        }
        gen_arc.fetch_add(1, Ordering::AcqRel);
    }

    hub.running.store(false, Ordering::Release);
    let _ = app.emit("monitor:stopped", hub.snapshot());
}

/// 保证会话**有增量来源**；没有就用带监视的配置重开一次。
///
/// 为什么需要重开：`Session::open` 在建立会话时就决定了增量从哪来
/// （USN Journal / `ReadDirectoryChangesW` / 没有）。界面上「同时建立实时监视」
/// 默认不勾（大目录上建路径索引是一笔真实开销），所以扫描完的会话很可能
/// **根本没有事件源** —— 那时 `poll` 只会空转，监控看起来"启动了但永远没动静"，
/// 是那种最费时间的假象。宁可重扫一次也不留这个坑。
fn ensure_source(app: &AppHandle, plan: &Plan) -> Result<bool, String> {
    let need_reopen = {
        let handle = app.state::<Shared>();
        let g = handle
            .inner()
            .lock()
            .map_err(|_| "状态锁被污染".to_string())?;
        let l = g
            .loaded
            .as_ref()
            .ok_or_else(|| "会话不见了".to_string())?;
        l.session.incremental.is_none() && l.session.watch.is_none()
    };
    if !need_reopen {
        return Ok(false);
    }

    // 沿用原来那次扫描的口径（盘符 / 扫描根 / 是否强制遍历），只把
    // 「建立监视」打开；USN Journal 的创建仍由用户显式勾选控制（文档 R3）。
    let mut cfg: SessionConfig = {
        let handle = app.state::<Shared>();
        let g = handle
            .inner()
            .lock()
            .map_err(|_| "状态锁被污染".to_string())?;
        let l = g
            .loaded
            .as_ref()
            .ok_or_else(|| "会话不见了".to_string())?;
        let mut c = l.session.cfg.clone();
        c.allow_traverse_watch = true;
        c.allow_create_journal = plan.create_journal;
        c.engine = core_monitor::EngineConfig {
            // 界面要看实时事件列表，必须让引擎把它们复制进 TickOutcome
            collect_events: true,
            ..l.session.cfg.engine.clone()
        };
        c
    };
    cfg.changelog_path = plan.changelog.clone();
    cfg.engine.write_changelog = plan.changelog.is_some();

    // 进度事件复用扫描那条通道：前端已经会处理它（进度条 + 不白屏）
    let app_p = app.clone();
    let mut last: Option<Instant> = None;
    let progress = move |phase: ScanPhase, done: u64, total: u64| {
        let now = Instant::now();
        if let Some(prev) = last {
            if now.duration_since(prev) < Duration::from_millis(60) {
                return;
            }
        }
        last = Some(now);
        let _ = app_p.emit(
            "scan:progress",
            crate::commands::ScanProgress {
                phase: format!("{}（建立增量来源）", match phase {
                    ScanPhase::Mft => "MFT 直读",
                    ScanPhase::Traverse => "目录遍历",
                }),
                done,
                total,
                elapsed_ms: 0,
            },
        );
    };

    let t0 = Instant::now();
    let session = core_scan::Session::open(cfg, progress).map_err(|e| format!("重新扫描失败：{e}"))?;
    let summary = crate::commands::summarize(&session, t0.elapsed().as_millis() as u64);

    let handle = app.state::<Shared>();
    let mut g = handle
        .inner()
        .lock()
        .map_err(|_| "状态锁被污染".to_string())?;
    g.loaded = Some(crate::state::Loaded::new(session));
    g.last = Some(summary.clone());
    drop(g);
    let _ = app.emit("scan:done", summary);
    // 注意：`Loaded` 被换成了新的，它的代数计数器也是新的 —— 调用方
    // 必须重新取一次（`run` 里就是这么做的），否则会一直盯着旧的计数器。
    Ok(true)
}

fn fail(app: &AppHandle, hub: &Arc<MonitorHub>, msg: String) {
    {
        let mut r = hub.ring.lock().unwrap_or_else(|e| e.into_inner());
        r.error = Some(msg.clone());
    }
    hub.running.store(false, Ordering::Release);
    let _ = app.emit("monitor:error", msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_monitor::debounce::PendingEvent;
    use core_usn::normalize::EventKind;

    fn ev(kind: EventKind) -> PendingEvent {
        PendingEvent {
            frn: 7,
            parent_frn: 5,
            kind,
            name: "a.txt".into(),
            old_name: None,
            is_dir: false,
            first_ms: 1,
            last_ms: 2,
            merged: 1,
            usn: 0,
            reason: 0,
            size_delta: 10,
            alloc_delta: 4096,
            new_size: 100,
            new_alloc: 4096,
        }
    }

    /// 环形的淘汰与「新的在前」是界面列表的正确性前提。
    /// 错了不会崩：只会让用户看到陈旧的行、或者行数永远停在某个值。
    #[test]
    fn ring_keeps_newest_first_and_evicts_oldest() {
        let mut r = Ring::default();
        for i in 0..(RING_CAPACITY + 5) {
            let mut e = ev(EventKind::Modified);
            e.name = format!("f{i}.txt");
            r.push_change(i as u64, e, format!("C:\\f{i}.txt"));
        }
        assert_eq!(r.changes.len(), RING_CAPACITY, "环形缓冲不该无限增长");
        assert_eq!(r.evicted, 5, "挤掉的条数要记账");

        let newest = r.tail(3, None);
        assert_eq!(newest.len(), 3);
        // 序号递减 = 新的在前
        assert!(newest[0].seq > newest[1].seq && newest[1].seq > newest[2].seq);
        assert!(newest[0].name.starts_with("f") && newest[0].name != "f0.txt");
    }

    #[test]
    fn ring_filters_by_kind() {
        let mut r = Ring::default();
        r.push_change(1, ev(EventKind::Created), r"C:\a.txt".to_string());
        r.push_change(2, ev(EventKind::Deleted), r"C:\b.txt".to_string());
        r.push_change(3, ev(EventKind::Created), r"C:\c.txt".to_string());

        assert_eq!(r.matched(None), 3);
        assert_eq!(r.matched(Some("created")), 2);
        assert_eq!(r.matched(Some("deleted")), 1);
        assert_eq!(r.matched(Some("renamed")), 0);

        let rows = r.tail(10, Some("created"));
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|x| x.kind == "created"));
    }

    /// 行里的 `size_text` 必须与 CLI 同源（`event_size_text`），
    /// 否则同一条变更在终端与界面上数字不一样。
    #[test]
    fn change_row_carries_the_shared_size_text() {
        let mut e = ev(EventKind::Modified);
        e.size_delta = 2048;
        let row = change_row(1, 0, &e, "C:\\a.txt");
        assert_eq!(row.size_text, core_monitor::event_size_text(&e));
        assert_eq!(row.size_text, "+2.00 KB");
        assert_eq!(row.path, "C:\\a.txt");
        assert_eq!(row.kind, "modified");
        assert_eq!(row.kind_cn, "修改");
    }

    /// 预警行要带上做判断所需的全部数字：只看一条「超阈值了」而看不到
    /// 阈值与当前值，用户没法判断要不要处理。
    #[test]
    fn alert_row_keeps_the_numbers() {
        let a = Alert {
            node: 5,
            path: "C:\\logs".into(),
            kind: core_monitor::RuleKind::Growth,
            threshold: 1_000,
            current_size: 5_000,
            baseline_size: 1_000,
            observed: 4_000,
            at_ms: 9,
            message: "增长超过阈值".into(),
        };
        let row = alert_row(3, 42, &a);
        assert_eq!(row.seq, 3);
        assert_eq!(row.at_ms, 42);
        assert_eq!(row.kind, "growth");
        assert_eq!(row.threshold, 1_000);
        assert_eq!(row.observed, 4_000);
        assert_eq!(row.path, "C:\\logs");
    }

    /// 规则回显：中文名与 CLI 同源，原文原样带回去。
    #[test]
    fn rule_info_echoes_the_input() {
        let r = core_monitor::parse_rule("grow=500M").unwrap();
        let i = RuleInfo::of(&r, "grow=500M");
        assert_eq!(i.kind, "growth");
        assert_eq!(i.text, "grow=500M");
        assert_eq!(i.threshold, 500 * 1024 * 1024);
        // 中文名不能是空的 —— 界面直接显示它
        assert!(!i.kind_cn.is_empty());
    }

    /// **真实目录**上的端到端：建会话 → 写文件 → 轮询 → 行出现在环形缓冲里。
    ///
    /// 这是这个模块里最值钱的一条测试。界面点击没法自动化，而这段的失败模式
    /// 几乎全是"不报错、只是没反应"：事件源没建起来（`poll` 空转）、
    /// 路径解析错了、debounce 窗口没到就被读走、环形缓冲塞进空行……
    /// 单元测试的夹具全都能过，只有真的挂上一次目录监视才看得出来。
    ///
    /// 走的是降级路径（`force_traverse` + `ReadDirectoryChangesW`）：
    /// 本机没有管理员权限，这恰好也是普通用户的实际路径。
    #[test]
    fn real_directory_change_reaches_the_ring() {
        let dir = std::env::temp_dir().join(format!("fw_gui_mon_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seed.txt"), b"seed").unwrap();

        let mut cfg = SessionConfig::default();
        cfg.drive = "C:".into();
        cfg.scan_root = Some(dir.clone());
        cfg.force_traverse = true; // 不碰裸设备句柄
        cfg.allow_traverse_watch = true; // 关键：建立事件源
        cfg.engine.collect_events = true; // 关键：让引擎把事件复制出来
        cfg.engine.write_changelog = false;
        cfg.changelog_path = None;

        let mut s = core_scan::Session::open(cfg, |_, _, _| {}).expect("会话应能建立");
        assert!(
            s.watch.is_some() || s.incremental.is_some(),
            "会话没有事件源 —— 监控会一直空转。warnings: {:?}",
            s.warnings
        );

        let mut ring = Ring::default();
        let t0 = Instant::now();
        let epoch = now_ms();

        // 真实变更：新建 + 追加写
        std::fs::write(dir.join("added.bin"), vec![b'x'; 4096]).unwrap();

        let mut seen_added = false;
        let mut seen_write = false;
        let mut wrote_again = false;
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(50));
            let elapsed = t0.elapsed().as_millis() as u64;
            let _ = poll_once(&mut s, &mut ring, elapsed, epoch + elapsed);

            if ring.changes.iter().any(|c| c.ev.name == "added.bin") {
                seen_added = true;
            }
            // 第一次看到「新增」之后再追加一次，验证「修改」也能到
            if seen_added && !wrote_again {
                std::fs::write(dir.join("seed.txt"), b"seed-and-more").unwrap();
                wrote_again = true;
            }
            if wrote_again && ring.changes.iter().any(|c| c.ev.name == "seed.txt") {
                seen_write = true;
                break;
            }
        }

        if let Some(row) = ring.changes.iter().find(|c| c.ev.name == "added.bin") {
            // 路径必须是**真实**的（不是合成路径），而且要以扫描根开头
            let expect = dir.join("added.bin");
            assert_eq!(
                row.path.to_lowercase(),
                expect.to_string_lossy().to_lowercase(),
                "事件的路径应是真实路径（走 PathRemap 换回扫描根）"
            );
            assert!(!row.path.contains(r"\\"), "路径不该出现双反斜杠：{}", row.path);
        }
        assert!(
            seen_added,
            "新建文件的事件没有到达环形缓冲（修改事件：{}）；已收到 {:?}",
            seen_write,
            ring.changes.iter().map(|c| c.ev.name.clone()).collect::<Vec<_>>()
        );
        assert!(ring.total_events > 0, "总计数也该跟着涨");

        // 收尾也走同一条记账路
        let elapsed = t0.elapsed().as_millis() as u64;
        let out = s.shutdown(epoch + elapsed).expect("收尾应成功");
        let totals = TreeTotals {
            dirs: s.tree.dir_count,
            files: s.tree.file_count,
            logical: s.tree.total_size,
            alloc: s.tree.total_alloc,
        };
        let (tick, _) = absorb(&s, &mut ring, &out, elapsed, totals, 0);
        assert_eq!(tick.totals.files, s.tree.file_count);
        assert!(tick.totals.logical > 0, "树上应当有文件字节数");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
