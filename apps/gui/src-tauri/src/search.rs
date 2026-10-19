//! 内容搜索（F13 的后半段）：在**纯文本类文件**里找关键字。
//!
//! # 为什么必须是后台线程 + 事件
//!
//! 名字搜索是纯内存的（几十毫秒），内容搜索要**真读文件**：几千个 txt / 日志
//! 读下来是几秒到几十秒。放在命令里同步做，窗口就会变成"未响应"——而那正是
//! 用户以为程序坏掉的时刻。所以与重复文件、快照对比同一套节奏：
//! 命令只校验并开工，进度与结果走 `content:progress` / `content:done` /
//! `content:error`，另有 `content_search_status` 供切回页签时补一次。
//!
//! # 候选从哪来
//!
//! 从**已扫出来的索引树**里挑（`is_text_name` + 体积上限），挑完就把锁放掉
//! 再去读文件 —— 读盘可能要十几秒，拿着状态锁会让扫描/监控全部排队。
//! 候选上限是硬的：一个 200 万文件的盘上，`.txt` 也可能有几十万个，
//! 全部读完对用户没有意义，进度条也会变成"永远走不完"。

use crate::state::Shared;
use core_scan::content::{self, Pattern};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// 进度事件节流：与扫描/去重同一档（每帧最多一条）。
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(60);
/// 一次最多考虑多少个候选文件。
const DEFAULT_LIMIT_FILES: usize = 20_000;
/// 一次最多带回多少个命中文件。
const DEFAULT_MAX_HITS: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct LineDto {
    pub line_no: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentHitRow {
    pub node: u32,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub lines: Vec<LineDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentView {
    pub query: String,
    pub hits: Vec<ContentHitRow>,
    /// 真正读过的文件数
    pub scanned: u64,
    /// 候选文件数（白名单 + 体积过筛后的）
    pub candidates: u64,
    /// 命中数超过 max_hits 时置位
    pub truncated: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentProgressDto {
    pub scanned: u64,
    pub total: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentStatusDto {
    pub running: bool,
    pub progress: Option<ContentProgressDto>,
    pub result: Option<ContentView>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Job {
    running: bool,
    progress: Option<ContentProgressDto>,
    result: Option<ContentView>,
    error: Option<String>,
}

pub struct ContentHub(Mutex<Job>);

impl ContentHub {
    pub fn new() -> Arc<ContentHub> {
        Arc::new(ContentHub(Mutex::new(Job::default())))
    }
    fn is_running(&self) -> bool {
        self.0.lock().map(|g| g.running).unwrap_or(false)
    }
    fn view(&self) -> ContentStatusDto {
        match self.0.lock() {
            Ok(g) => ContentStatusDto {
                running: g.running,
                progress: g.progress.clone(),
                result: g.result.clone(),
                error: g.error.clone(),
            },
            Err(_) => ContentStatusDto {
                running: false,
                progress: None,
                result: None,
                error: Some("状态锁被污染".into()),
            },
        }
    }
}

/// 开工。参数类错误（空关键字、正则写错）**当场**返回 —— 让用户立刻看到
/// "你的正则少了一个括号"，而不是等几秒再从事件里收到一句话。
#[tauri::command]
pub fn content_search_start(
    app: AppHandle,
    hub: State<'_, Arc<ContentHub>>,
    text: String,
    regex: Option<bool>,
    limit_files: Option<usize>,
    max_hits: Option<usize>,
) -> Result<(), String> {
    let pattern = Pattern::new(&text, regex.unwrap_or(false))?;
    if hub.is_running() {
        return Err("已有一次内容搜索在进行中".into());
    }
    {
        // 没有扫描结果 / 正在扫描：都要在开工前挡住
        let shared = app.state::<Shared>();
        let st = shared.lock().map_err(|_| "状态锁被污染".to_string())?;
        if st.scanning {
            return Err("正在扫描：扫描结束后再搜（那时索引树才是稳定的）".into());
        }
        let _ = st.loaded()?;
    }
    {
        let mut g = hub.0.lock().map_err(|_| "状态锁被污染".to_string())?;
        g.running = true;
        g.progress = None;
        g.result = None;
        g.error = None;
    }

    let hub_arc: Arc<ContentHub> = Arc::clone(&*hub);
    let limit_files = limit_files.unwrap_or(DEFAULT_LIMIT_FILES).clamp(1, 200_000);
    let max_hits = max_hits.unwrap_or(DEFAULT_MAX_HITS).clamp(1, 2_000);
    let query_text = text.trim().to_string();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let fail = |msg: String| {
            if let Ok(mut g) = hub_arc.0.lock() {
                g.running = false;
                g.error = Some(msg.clone());
            }
            let _ = app.emit("content:error", msg);
        };

        // ── 1. 挑候选（持锁，但只做内存遍历）──────────────────────────
        let candidates = {
            let shared = app.state::<Shared>();
            let st = match shared.lock() {
                Ok(g) => g,
                Err(_) => return fail("状态锁被污染".into()),
            };
            let l = match st.loaded() {
                Ok(l) => l,
                Err(e) => return fail(e),
            };
            let tree = l.tree();
            let remap = l.session.path_remap();
            let mut out: Vec<(u32, String, String, u64, i64)> = Vec::new();
            for (i, node) in tree.arena.nodes.iter().enumerate() {
                if out.len() >= limit_files {
                    break;
                }
                if !node.in_use() || !node.is_file() || node.size > content::MAX_FILE_BYTES {
                    continue;
                }
                let name = tree.name(i as u32);
                if !content::is_text_name(name) {
                    continue;
                }
                let raw = tree.path_of(i as u32);
                let path = remap.as_ref().map(|r| r.apply(&raw)).unwrap_or(raw);
                out.push((i as u32, name.to_string(), path, node.size, node.mtime));
            }
            out
        };

        let total = candidates.len() as u64;
        let mut hits: Vec<ContentHitRow> = Vec::new();
        let mut scanned = 0u64;
        let mut truncated = false;
        let mut last = Instant::now();

        for (node, name, path, size, mtime) in candidates {
            if hits.len() >= max_hits {
                truncated = true;
                break;
            }
            scanned += 1;
            let found = content::search_file(std::path::Path::new(&path), &pattern);
            if !found.is_empty() {
                hits.push(ContentHitRow {
                    node,
                    name,
                    path,
                    size,
                    mtime,
                    lines: found
                        .into_iter()
                        .map(|h| LineDto { line_no: h.line_no, text: h.text })
                        .collect(),
                });
            }
            // 进度：按时间节流，但**最后一个文件必须报**（否则条停在 99%）
            let done = scanned == total;
            if done || last.elapsed() >= PROGRESS_MIN_GAP {
                last = Instant::now();
                let p = ContentProgressDto {
                    scanned,
                    total,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                };
                if let Ok(mut g) = hub_arc.0.lock() {
                    g.progress = Some(p.clone());
                }
                let _ = app.emit("content:progress", p);
            }
        }

        let view = ContentView {
            query: query_text,
            hits,
            scanned,
            candidates: total,
            truncated,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        };
        if let Ok(mut g) = hub_arc.0.lock() {
            g.running = false;
            g.result = Some(view.clone());
        }
        let _ = app.emit("content:done", view);
    });
    Ok(())
}

#[tauri::command]
pub fn content_search_status(hub: State<'_, Arc<ContentHub>>) -> ContentStatusDto {
    hub.view()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_defaults_to_idle() {
        let hub = ContentHub::new();
        let v = hub.view();
        assert!(!v.running && v.result.is_none() && v.error.is_none());
    }

    #[test]
    fn running_flag_blocks_a_second_job() {
        let hub = ContentHub::new();
        {
            let mut g = hub.0.lock().unwrap();
            g.running = true;
        }
        assert!(hub.is_running(), "第二个作业必须被挡住（否则两个线程抢同一份结果）");
    }
}
