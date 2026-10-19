//! 固定路径的独立监控。没有 Shared/AppState，也不访问卷设备或文件内容。
//! 每个目标的通知句柄、索引、阈值基线和工作线程均独立于面积图扫描。
use crate::monitor::{alert_row, change_row, AlertRow, ChangeRow};
use core_monitor::{PendingEvent, RuleEngine};
use core_scan::{DirWatcher, Session, SessionConfig};
use core_usn::normalize::EventKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

type Notify = Arc<dyn Fn() + Send + Sync>;
const MAX_TARGETS: usize = 32;
const INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub id: u64,
    pub path: String,
    pub is_dir: bool,
    pub rules: Vec<String>,
    pub enabled: bool,
}

#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub state: String,
    pub size: u64,
    pub baseline: u64,
    pub events: u64,
    pub updated_ms: u64,
    pub error: Option<String>,
}

struct Control {
    stop: AtomicBool,
    status: Mutex<Status>,
}

struct Entry { config: Config, control: Arc<Control> }

#[derive(Serialize)]
pub struct TargetView {
    #[serde(flatten)]
    pub config: Config,
    pub status: Status,
}

#[derive(Clone, Serialize)]
pub struct Change {
    #[serde(flatten)]
    pub row: ChangeRow,
    pub target_id: u64,
    pub target_path: String,
}

#[derive(Clone, Serialize)]
pub struct Warning {
    #[serde(flatten)]
    pub row: AlertRow,
    pub target_id: u64,
    pub target_path: String,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub targets: Vec<TargetView>,
    pub rows: Vec<Change>,
    pub alerts: Vec<Warning>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Registry {
    entries: BTreeMap<u64, Entry>,
    rows: VecDeque<Change>,
    alerts: VecDeque<Warning>,
    seq: u64,
    load_error: Option<String>,
}

pub struct IndependentHub {
    registry: Mutex<Registry>,
    mutation: Mutex<()>,
    path: PathBuf,
    /// 建窗口之后才有。预警要靠它发系统通知 + 闪托盘图标 —— 但它是可选：
    /// 单测里没有 AppHandle，`notify_alert` 那一路直接跳过。
    app: Mutex<Option<tauri::AppHandle>>,
}

fn control(enabled: bool) -> Arc<Control> {
    Arc::new(Control { stop: AtomicBool::new(false), status: Mutex::new(Status {
        state: if enabled { "starting" } else { "paused" }.into(), ..Status::default()
    }) })
}

fn key(path: &str) -> String { display_path(Path::new(path)).trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase() }

fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") { format!(r"\\{rest}") }
    else { s.strip_prefix(r"\\?\").unwrap_or(&s).to_string() }
}

fn ordinary_metadata(path: &Path) -> Result<fs::Metadata, String> {
    use std::os::windows::fs::MetadataExt;
    let m = fs::symlink_metadata(path).map_err(|e| format!("无法读取目标 {}：{e}", path.display()))?;
    if m.file_type().is_symlink() || m.file_attributes() & 0x400 != 0 {
        return Err("请选择普通文件或文件夹，独立监控不跟随符号链接和重解析点".into());
    }
    if !m.is_dir() && !m.is_file() { return Err("监控目标必须是文件或文件夹".into()); }
    Ok(m)
}

impl IndependentHub {
    pub fn new() -> Arc<Self> {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        Self::load(base.join("FileWatch").join("independent-monitor.json"))
    }

    fn load(path: PathBuf) -> Arc<Self> {
        let mut registry = Registry::default();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Config>>(&bytes) {
                Ok(configs) if configs.len() <= MAX_TARGETS => {
                    for config in configs {
                        if !Path::new(&config.path).is_absolute() || core_monitor::parse_rules(&config.rules).is_err()
                            || registry.entries.contains_key(&config.id) {
                            registry.load_error = Some("独立监控配置无效；请检查配置文件后重启，文件已保留".into());
                            registry.entries.clear(); break;
                        }
                        registry.entries.insert(config.id, Entry { control: control(config.enabled), config });
                    }
                }
                _ => registry.load_error = Some("独立监控配置无法解析，原文件已保留；请检查配置文件后重启".into()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => registry.load_error = Some(format!("独立监控配置读取失败：{e}")),
        }
        Arc::new(Self {
            registry: Mutex::new(registry),
            mutation: Mutex::new(()),
            path,
            app: Mutex::new(None),
        })
    }

    fn save(&self, configs: &[Config]) -> Result<(), String> {
        let dir = self.path.parent().ok_or("配置目录无效")?;
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let part = self.path.with_extension("json.part");
        let bytes = serde_json::to_vec_pretty(configs).map_err(|e| e.to_string())?;
        fs::write(&part, bytes).map_err(|e| format!("保存监控配置失败：{e}"))?;
        fs::rename(part, &self.path).map_err(|e| format!("替换监控配置失败：{e}"))
    }

    fn snapshot(&self) -> Snapshot {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        Snapshot {
            targets: registry.entries.values().map(|e| TargetView {
                config: e.config.clone(), status: e.control.status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            }).collect(),
            rows: registry.rows.iter().rev().take(800).cloned().collect(),
            alerts: registry.alerts.iter().rev().take(300).cloned().collect(),
            error: registry.load_error.clone(),
        }
    }

    fn upsert(self: &Arc<Self>, id: Option<u64>, path: String, rules: Vec<String>, enabled: bool, notify: Notify) -> Result<u64, String> {
        let _gate = self.mutation.lock().map_err(|_| "配置锁失效")?;
        core_monitor::parse_rules(&rules)?;
        let path = path.trim();
        if path.is_empty() || !Path::new(path).is_absolute() { return Err("请输入完整的文件或文件夹路径".into()); }
        let old = {
            let registry = self.registry.lock().map_err(|_| "监控状态锁失效")?;
            if let Some(error) = &registry.load_error { return Err(error.clone()); }
            if id.is_some() && !registry.entries.contains_key(&id.unwrap()) { return Err("这项监控已经被移除".into()); }
            if id.is_none() && registry.entries.len() >= MAX_TARGETS { return Err(format!("最多同时保存 {MAX_TARGETS} 个监控目标")); }
            registry.entries.get(&id.unwrap_or(0)).map(|e| e.config.clone())
        };
        let (path, is_dir) = match ordinary_metadata(Path::new(path)) {
            Ok(m) => (display_path(&fs::canonicalize(path).map_err(|e| e.to_string())?), m.is_dir()),
            Err(error) => match old.as_ref().filter(|o| key(&o.path) == key(path)) {
                Some(old) => (old.path.clone(), old.is_dir),
                None => return Err(error),
            },
        };
        let mut registry = self.registry.lock().map_err(|_| "监控状态锁失效")?;
        if registry.entries.values().any(|e| Some(e.config.id) != id && key(&e.config.path) == key(&path)) {
            return Err("这个路径已经在独立监控列表中".into());
        }
        let id = id.unwrap_or_else(|| registry.entries.keys().next_back().copied().unwrap_or(0).saturating_add(1));
        let config = Config { id, path, is_dir, rules, enabled };
        let mut configs: Vec<_> = registry.entries.values().filter(|e| e.config.id != id).map(|e| e.config.clone()).collect();
        configs.push(config.clone());
        self.save(&configs)?;
        let entry = Entry { config: config.clone(), control: control(enabled) };
        let ctl = entry.control.clone();
        if let Some(old) = registry.entries.insert(id, entry) { old.control.stop.store(true, Ordering::Release); }
        drop(registry);
        if enabled { self.launch(config, ctl, notify.clone()); }
        notify();
        Ok(id)
    }

    fn remove(&self, id: u64) -> Result<(), String> {
        let _gate = self.mutation.lock().map_err(|_| "配置锁失效")?;
        let mut registry = self.registry.lock().map_err(|_| "监控状态锁失效")?;
        if let Some(error) = &registry.load_error { return Err(error.clone()); }
        if !registry.entries.contains_key(&id) { return Err("这项监控已经被移除".into()); }
        let configs: Vec<_> = registry.entries.values().filter(|e| e.config.id != id).map(|e| e.config.clone()).collect();
        self.save(&configs)?;
        if let Some(entry) = registry.entries.remove(&id) { entry.control.stop.store(true, Ordering::Release); }
        Ok(())
    }

    pub fn resume_saved(self: &Arc<Self>, app: AppHandle) {
        let notify: Notify = Arc::new(move || { let _ = app.emit("independent:changed", ()); });
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<_> = registry.entries.values().filter(|e| e.config.enabled).map(|e| (e.config.clone(), e.control.clone())).collect();
        drop(registry);
        for (config, ctl) in entries { self.launch(config, ctl, notify.clone()); }
    }

    pub fn stop_all(&self) {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        for entry in registry.entries.values() { entry.control.stop.store(true, Ordering::Release); }
    }

    fn launch(self: &Arc<Self>, config: Config, ctl: Arc<Control>, notify: Notify) {
        let hub = self.clone();
        let fallback = ctl.clone();
        if let Err(error) = std::thread::Builder::new().name(format!("fixed-watch-{}", config.id))
            .spawn(move || run(hub, config, ctl, notify)) {
            let mut status = fallback.status.lock().unwrap_or_else(|e| e.into_inner());
            status.state = "error".into(); status.error = Some(format!("无法创建监控线程：{error}"));
        }
    }

    /// 把一轮结果记进环形缓冲。**返回**这一轮触发的预警文本（没有就是 None）——
    /// 通知要等锁放掉之后发：`notify_alert` 会走主线程重建托盘菜单，
    /// 拿着 registry 锁去等主线程是自找麻烦。
    fn record(&self, config: &Config, ctl: &Arc<Control>, events: Vec<(PendingEvent, String)>, alert: Option<core_monitor::Alert>) -> Option<String> {
        let mut r = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if !r.entries.get(&config.id).map(|e| Arc::ptr_eq(&e.control, ctl)).unwrap_or(false) || ctl.stop.load(Ordering::Acquire) { return None; }
        for (event, path) in events {
            r.seq += 1;
            let row = change_row(r.seq, 0, &event, &path);
            r.rows.push_back(Change { row, target_id: config.id, target_path: config.path.clone() });
        }
        while r.rows.len() > crate::monitor::RING_CAPACITY { r.rows.pop_front(); }
        let fired = alert.map(|alert| {
            r.seq += 1;
            let row = alert_row(r.seq, 0, &alert);
            let text = format!("{}　{}", config.path, alert.message);
            r.alerts.push_back(Warning { row, target_id: config.id, target_path: config.path.clone() });
            text
        });
        while r.alerts.len() > crate::monitor::ALERT_CAPACITY { r.alerts.pop_front(); }
        fired
    }

    /// 建窗口之后调一次：预警才有"发通知 / 闪托盘"的出口。
    pub fn attach(&self, app: tauri::AppHandle) {
        if let Ok(mut slot) = self.app.lock() {
            *slot = Some(app);
        }
    }

    fn notify_alert(&self, text: &str) {
        let app = self.app.lock().ok().and_then(|g| g.clone());
        if let Some(app) = app {
            crate::tray::notify_alert(&app, text);
        }
    }
}

enum Source {
    Directory(Box<Session>),
    File { watcher: DirWatcher, parent: PathBuf, last_size: u64, existed: bool },
}

#[derive(Debug)]
struct OpenError { message: String, retryable: bool }
impl From<String> for OpenError {
    fn from(message: String) -> Self { Self { message, retryable: true } }
}
impl From<&str> for OpenError {
    fn from(message: &str) -> Self { message.to_string().into() }
}

impl Source {
    fn open(config: &Config) -> Result<Self, OpenError> { Self::open_with_budget(config, 300_000) }

    fn open_with_budget(config: &Config, watch_max_nodes: usize) -> Result<Self, OpenError> {
        let path = Path::new(&config.path);
        let metadata = ordinary_metadata(path)?;
        if metadata.is_dir() != config.is_dir { return Err("目标类型已改变，请修改这项监控".into()); }
        if config.is_dir {
            let drive = config.path.get(..2).filter(|p| p.ends_with(':')).unwrap_or("C:").to_string();
            let mut cfg = SessionConfig { drive, scan_root: Some(path.to_path_buf()), force_traverse: true, watch_max_nodes, ..Default::default() };
            cfg.engine.collect_events = true;
            cfg.engine.write_changelog = false;
            cfg.changelog_path = None;
            let session = Session::open(cfg, |_, _, _| {}).map_err(|e| e.to_string())?;
            if session.watch.is_none() {
                return Err(OpenError { message: format!("无法建立独立目录监控（索引上限 {watch_max_nodes} 个节点）。请检查权限或缩小目标范围后重新启用；已停止自动重试，避免重复扫描。"), retryable: false });
            }
            Ok(Self::Directory(Box::new(session)))
        } else {
            let parent = path.parent().ok_or("文件没有父目录")?.to_path_buf();
            let watcher = DirWatcher::start(&parent, 64 * 1024, false).map_err(|e| e.to_string())?;
            Ok(Self::File { watcher, parent, last_size: metadata.len(), existed: true })
        }
    }

    fn size(&self) -> u64 {
        match self { Self::Directory(s) => s.tree.total_size, Self::File { last_size, .. } => *last_size }
    }

    fn poll(&mut self, config: &Config, now: u64) -> Result<Vec<(PendingEvent, String)>, String> {
        match self {
            Self::Directory(session) => {
                let metadata = ordinary_metadata(Path::new(&config.path))?;
                if !metadata.is_dir() { return Err("目标文件夹已不存在或类型改变，等待恢复".into()); }
                let out = session.poll(now).map_err(|e| e.to_string())?;
                if out.backpressure { return Err("事件队列溢出，正在重新建立独立监控基线".into()); }
                let remap = session.path_remap();
                Ok(out.events.into_iter().map(|event| {
                    let path = core_scan::event_path(&session.tree, &event, remap.as_ref()); (event, path)
                }).collect())
            }
            Self::File { watcher, parent, last_size, existed } => {
                if watcher.exit_error() != 0 { return Err("文件通知已中断，正在重新建立独立监控".into()); }
                let mut events = Vec::new(); watcher.drain(4096, &mut events);
                let target_key = key(&config.path);
                let relevant = events.iter().filter(|e| key(&parent.join(&e.rel).to_string_lossy()) == target_key).count();
                let metadata = match ordinary_metadata(Path::new(&config.path)) {
                    Ok(m) if m.is_file() => Some(m),
                    Ok(_) => return Err("目标类型改变，请修改监控项".into()),
                    Err(_) if fs::symlink_metadata(&config.path).err().map(|e| e.kind() == std::io::ErrorKind::NotFound).unwrap_or(false) => None,
                    Err(error) => return Err(error),
                };
                let exists = metadata.is_some();
                let size = metadata.map(|m| m.len()).unwrap_or(0);
                let kind = if *existed && !exists { Some(EventKind::Deleted) }
                    else if !*existed && exists { Some(EventKind::Created) }
                    else if exists && (relevant > 0 || size != *last_size) { Some(EventKind::Modified) } else { None };
                let out = kind.map(|kind| {
                    let ev = file_event(kind, Path::new(&config.path), *last_size, size, now, relevant as u32);
                    vec![(ev, config.path.clone())]
                }).unwrap_or_default();
                *last_size = size; *existed = exists;
                Ok(out)
            }
        }
    }
}

fn file_event(kind: EventKind, path: &Path, before: u64, after: u64, now: u64, merged: u32) -> PendingEvent {
    PendingEvent {
        frn: 1, parent_frn: 0, kind, name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        old_name: None, is_dir: false, first_ms: now, last_ms: now, merged: merged.max(1), usn: 0, reason: 0,
        size_delta: (after as i128 - before as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        alloc_delta: 0, new_size: after, new_alloc: 0,
    }
}

fn run(hub: Arc<IndependentHub>, config: Config, ctl: Arc<Control>, notify: Notify) {
    let mut source = None;
    let mut rules = RuleEngine::new(Default::default());
    let mut last_notice = Instant::now();
    while !ctl.stop.load(Ordering::Acquire) {
        if source.is_none() {
            match Source::open(&config) {
                Ok(new) => {
                    if ctl.stop.load(Ordering::Acquire) { break; }
                    let baseline = new.size();
                    rules.clear();
                    rules.add_target(1, config.path.clone(), core_monitor::parse_rules(&config.rules).unwrap_or_default(), baseline, core_io::util::unix_ms());
                    let mut status = ctl.status.lock().unwrap_or_else(|e| e.into_inner());
                    status.state = "running".into(); status.error = None; status.baseline = baseline; status.size = baseline;
                    source = Some(new);
                    drop(status); notify();
                }
                Err(error) => {
                    let mut status = ctl.status.lock().unwrap_or_else(|e| e.into_inner());
                    status.state = if error.retryable { "waiting" } else { "error" }.into(); status.error = Some(error.message);
                    drop(status); notify();
                    if !error.retryable { return; }
                    for _ in 0..20 { if ctl.stop.load(Ordering::Acquire) { break; } std::thread::sleep(Duration::from_millis(50)); }
                    continue;
                }
            }
        }
        let now = core_io::util::unix_ms();
        let active = source.as_mut().unwrap();
        match active.poll(&config, now) {
            Ok(events) => {
                let size = active.size();
                let alert = rules.evaluate(1, size, now);
                let changed = !events.is_empty() || alert.is_some();
                let count = events.len() as u64;
                let fired = hub.record(&config, &ctl, events, alert);
                if let Some(text) = fired { hub.notify_alert(&text); }
                let mut status = ctl.status.lock().unwrap_or_else(|e| e.into_inner());
                status.size = size; status.events += count; status.updated_ms = now;
                drop(status);
                if changed || last_notice.elapsed() >= Duration::from_secs(1) { notify(); last_notice = Instant::now(); }
            }
            Err(error) => {
                source = None;
                let mut status = ctl.status.lock().unwrap_or_else(|e| e.into_inner());
                status.state = "waiting".into(); status.error = Some(error);
                drop(status); notify();
            }
        }
        let deadline = Instant::now() + INTERVAL;
        while Instant::now() < deadline && !ctl.stop.load(Ordering::Acquire) { std::thread::sleep(Duration::from_millis(25)); }
    }
}

#[tauri::command]
pub fn independent_snapshot(hub: State<'_, Arc<IndependentHub>>) -> Snapshot { hub.snapshot() }

#[tauri::command]
pub async fn independent_upsert(app: AppHandle, hub: State<'_, Arc<IndependentHub>>, id: Option<u64>, path: String, rules: Vec<String>, enabled: bool) -> Result<u64, String> {
    let notify = Arc::new(move || { let _ = app.emit("independent:changed", ()); });
    let hub = hub.inner().clone();
    tauri::async_runtime::spawn_blocking(move || hub.upsert(id, path, rules, enabled, notify)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn independent_remove(app: AppHandle, hub: State<'_, Arc<IndependentHub>>, id: u64) -> Result<(), String> {
    let hub = hub.inner().clone();
    tauri::async_runtime::spawn_blocking(move || hub.remove(id)).await.map_err(|e| e.to_string())??;
    let _ = app.emit("independent:changed", ());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(path: &Path, is_dir: bool) -> Config {
        Config { id: 1, path: display_path(path), is_dir, rules: vec![], enabled: true }
    }
    fn until(mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !check() { assert!(Instant::now() < deadline, "等待独立监控超时"); std::thread::sleep(Duration::from_millis(30)); }
    }
    #[test]
    fn file_watch_allows_writes_and_reports_same_size_delete_and_recreate() {
        let owned = core_io::test_dir::TestDir::new("independent-file").unwrap();
        let path = owned.path().join("watched.txt");
        fs::write(&path, b"aaaa").unwrap();
        let cfg = config(&path, false);
        let mut source = Source::open(&cfg).unwrap();
        // Open/write succeeds while notifications are active. Content is never read by Source.
        fs::write(&path, b"bbbb").unwrap();
        let mut rows = vec![];
        until(|| { rows.extend(source.poll(&cfg, core_io::util::unix_ms()).unwrap()); !rows.is_empty() });
        assert!(rows.iter().any(|(e, _)| e.kind == EventKind::Modified));
        assert_eq!(source.size(), 4);
        // Strict audited deletion: only this owned fixture, after helper verifies ownership and boundaries.
        owned.remove_file("watched.txt").unwrap();
        rows.clear();
        until(|| { rows.extend(source.poll(&cfg, core_io::util::unix_ms()).unwrap()); rows.iter().any(|(e, _)| e.kind == EventKind::Deleted) });
        assert_eq!(source.size(), 0);
        fs::write(&path, b"again").unwrap(); rows.clear();
        until(|| { rows.extend(source.poll(&cfg, core_io::util::unix_ms()).unwrap()); rows.iter().any(|(e, _)| e.kind == EventKind::Created) });
        assert_eq!(source.size(), 5);
        drop(source); // Close notification handles before the fixture guard cleans its own directory.
    }
    #[test]
    fn directory_index_and_another_scan_are_independent() {
        let owned = core_io::test_dir::TestDir::new("independent-directory").unwrap();
        let root = owned.path().join("watched"); fs::create_dir(&root).unwrap();
        fs::write(root.join("seed.txt"), b"seed").unwrap();
        let cfg = config(&root, true);
        let mut source = Source::open(&cfg).unwrap();
        // A second complete Session may be opened while the independent source stays alive.
        let mut scan = SessionConfig { scan_root: Some(owned.path().to_path_buf()), force_traverse: true, allow_traverse_watch: false, ..Default::default() };
        scan.engine.write_changelog = false;
        let separate = Session::open(scan, |_, _, _| {}).unwrap();
        let original_size = separate.tree.total_size;
        fs::write(root.join("new.txt"), b"hello").unwrap();
        let mut rows = vec![];
        until(|| { rows.extend(source.poll(&cfg, core_io::util::unix_ms()).unwrap()); source.size() == 9 && !rows.is_empty() });
        assert_eq!(separate.tree.total_size, original_size, "独立索引不能改变扫描树");
        assert!(rows.iter().all(|(_, p)| key(p).starts_with(&key(&cfg.path))));
        drop(separate); drop(source);
    }

    #[test]
    fn unsupported_directory_budget_stops_automatic_rescanning() {
        let owned = core_io::test_dir::TestDir::new("independent-budget").unwrap();
        fs::write(owned.path().join("seed.txt"), b"x").unwrap();
        let error = Source::open_with_budget(&config(owned.path(), true), 1).err().expect("root and file exceed one node");
        assert!(!error.retryable, "预算不足时必须停止，不得循环重扫整个目录");
        assert!(error.message.contains("停止自动重试"));
    }
    #[test]
    fn configs_crud_persist_without_touching_targets_and_reject_invalid_rules() {
        let owned = core_io::test_dir::TestDir::new("independent-config").unwrap();
        let target = owned.path().join("keep.txt"); fs::write(&target, b"keep").unwrap();
        let path = owned.path().join("settings.json");
        let hub = IndependentHub::load(path.clone());
        let quiet: Notify = Arc::new(|| {});
        let id = hub.upsert(None, display_path(&target), vec!["grow=1M".into()], false, quiet.clone()).unwrap();
        assert!(hub.upsert(None, display_path(&target), vec![], false, quiet.clone()).is_err());
        assert!(hub.upsert(Some(id), display_path(&target), vec!["invalid".into()], false, quiet.clone()).is_err());
        hub.upsert(Some(id), display_path(&target), vec!["abs=2G".into()], false, quiet).unwrap();
        let reloaded = IndependentHub::load(path.clone());
        assert_eq!(reloaded.snapshot().targets[0].config.rules, vec!["abs=2G"]);
        hub.remove(id).unwrap();
        assert!(IndependentHub::load(path).snapshot().targets.is_empty());
        assert_eq!(fs::read(&target).unwrap(), b"keep", "移除配置不能删除或修改被监控文件");
    }
    #[test]
    fn worker_records_alerts_and_stale_worker_cannot_write_after_edit() {
        let owned = core_io::test_dir::TestDir::new("independent-worker").unwrap();
        let target = owned.path().join("worker.txt"); fs::write(&target, b"1234").unwrap();
        let hub = IndependentHub::load(owned.path().join("settings.json"));
        let quiet: Notify = Arc::new(|| {});
        let id = hub.upsert(None, display_path(&target), vec!["abs=1B".into(), "grow=1G".into()], false, quiet.clone()).unwrap();
        let (cfg, ctl) = { let r = hub.registry.lock().unwrap(); let e = &r.entries[&id]; (e.config.clone(), e.control.clone()) };
        struct Worker { ctl: Arc<Control>, join: Option<std::thread::JoinHandle<()>> }
        impl Drop for Worker { fn drop(&mut self) { self.ctl.stop.store(true, Ordering::Release); if let Some(join) = self.join.take() { join.join().unwrap(); } } }
        let worker_hub = hub.clone(); let worker_cfg = cfg.clone(); let worker_ctl = ctl.clone();
        let worker = Worker { ctl: ctl.clone(), join: Some(std::thread::spawn(move || run(worker_hub, worker_cfg, worker_ctl, Arc::new(|| {})))) };
        until(|| !hub.snapshot().alerts.is_empty());
        let snap = hub.snapshot();
        assert_eq!(snap.alerts.len(), 1, "abs 满足即触发，grow 不满足也不阻止：规则为或");
        assert!(snap.alerts[0].row.time_ms > 1_000_000_000_000);
        fs::write(&target, b"1234567").unwrap();
        until(|| !hub.snapshot().rows.is_empty());
        hub.upsert(Some(id), display_path(&target), vec![], false, quiet).unwrap();
        let count = hub.snapshot().rows.len();
        hub.record(&cfg, &ctl, vec![(file_event(EventKind::Modified, &target, 7, 8, core_io::util::unix_ms(), 1), cfg.path.clone())], None);
        assert_eq!(hub.snapshot().rows.len(), count);
        drop(worker); // Join before TestDir is dropped, including panic unwinding.
    }
}
