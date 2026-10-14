//! 前端可调用的 Tauri 命令（`invoke("...")` 的另一端）。
//!
//! 命名与文档 §3.1 的功能编号对应，方便回头查规格：
//! * `list_volumes`        F1 盘符选择
//! * `scan_start` / `scan_summary`   F2 全量扫描（进度用事件推给前端）
//! * `tree_rows` / `breadcrumb`      F3 文件夹树
//! * `layout`              F4 面积图（**二进制**返回，见 [`crate::wire`]）
//! * `ext_rows`            F5 文件类型列表
//! * `top_files`           F12 Top-N 大文件
//!
//! 规矩：命令里**只做编排**，算法一律在 `core-*` 里。这一层不允许出现
//! 「顺手在这里算一下」的逻辑，否则 CLI 与 GUI 会慢慢长出两套行为。

use crate::state::{Loaded, ScanSummary, Shared};
use crate::wire;
use core_index::aggregate::{sorted_children, top_files as top_files_impl, SizeMode};
use core_scan::{ScanPhase, Session, SessionConfig};
use core_viz::{compute_layout, LayoutOptions, LodPolicy, Rect, ShadingMode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, Manager, State};

/// 进度事件的最小间隔。
///
/// 扫描回调在 200 万文件下会触发上百次，每次都发一个 IPC 消息会把前端渲染
/// 线程淹掉（事件排队比绘制还慢）。60 ms ≈ 每帧最多一条，既满足「不白屏」
/// 又留足帧预算。**最终结果**不走这条路 —— 它由 `scan:done` 带着完整摘要发。
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(60);

// ── 传输对象 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct VolumeCard {
    pub drive: String,
    pub root: String,
    pub label: String,
    pub fs_name: String,
    pub total: u64,
    pub free: u64,
    /// 0~1
    pub used_ratio: f64,
    /// 是否支持 MFT 直读（本地固定盘 + NTFS）
    pub supports_mft: bool,
    /// 当前进程是否有权限走 MFT 直读（没有就只能降级）
    pub elevated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    /// `"MFT"` / `"目录遍历"`
    pub phase: String,
    pub done: u64,
    /// MFT 路径下已知总数；目录遍历下恒为 0（遍历前无法预知）
    pub total: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeRow {
    pub node: u32,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    /// 直接子项数
    pub children: usize,
    /// 子孙文件数（仅目录有意义）
    pub files: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Crumb {
    pub node: u32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtRow {
    pub ext: String,
    pub ext_id: u32,
    pub count: u64,
    pub size: u64,
    /// 占全盘比例 0~1
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub node: u32,
    pub name: String,
    /// 相对扫描根的路径（合成路径已按真实根重写）
    pub path: String,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub node: u32,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    pub children: usize,
    pub files: u64,
    pub depth: u32,
    pub mtime: i64,
}

#[derive(Debug, Deserialize)]
pub struct LayoutArgs {
    pub width: f32,
    pub height: f32,
    /// 视图根节点
    pub root: u32,
    /// `"logical"` / `"allocated"`
    pub mode: Option<String>,
    /// `"ext"` / `"depth"` / `"age"`
    pub shading: Option<String>,
    /// 是否递归展开（缩放到最外层时关掉可以只画一层）
    pub deep: Option<bool>,
    /// LOD 的展开阈值（像素）。界面的「细节」滑块直接映射到它：
    /// 调大 → 只展开大块（更精简）；调小 → 展开更深（更详细）。
    pub min_side: Option<f32>,
}

// ── 小工具 ──────────────────────────────────────────────────────────────────

fn size_mode(s: Option<&str>) -> SizeMode {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("allocated") | Some("alloc") | Some("占用") => SizeMode::Allocated,
        _ => SizeMode::Logical,
    }
}

fn shading_of(s: Option<&str>) -> ShadingMode {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("depth") => ShadingMode::ByDepth,
        Some("age") => ShadingMode::ByAge,
        _ => ShadingMode::ByExtension,
    }
}

fn phase_cn(p: ScanPhase) -> &'static str {
    match p {
        ScanPhase::Mft => "MFT 直读",
        ScanPhase::Traverse => "目录遍历",
    }
}

/// 由界面参数构造 LOD 策略。
///
/// `min_side` 会被夹到 `[4, 2000]`：太小会让 LOD 形同虚设（几十万个亚像素矩形
/// 全画出来，帧率直接崩），太大则任何目录都不展开、面积图只剩一层。
/// 夹紧比「相信调用方」便宜 —— 前端一个笔误不该让界面卡死。
fn policy_of(args: &LayoutArgs) -> LodPolicy {
    let min_side = args.min_side.unwrap_or(80.0).clamp(4.0, 2000.0);
    LodPolicy {
        min_side,
        // 缩到最外层时可以关掉递归，只画当前这一层
        max_depth: if args.deep.unwrap_or(true) { 16 } else { 1 },
        ..LodPolicy::default()
    }
}

/// 取节点名（字符串池里的零拷贝切片）。
fn name_of(tree: &core_index::Tree, node: u32) -> String {
    tree.node(node)
        .map(|n| tree.names.get(n.name).to_string())
        .unwrap_or_default()
}

// ── F1 盘符 ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_volumes() -> Vec<VolumeCard> {
    let elevated = core_io::util::is_elevated_hint();
    core_io::list_volumes()
        .into_iter()
        .map(|v| VolumeCard {
            drive: v.drive.clone(),
            root: v.root.clone(),
            label: v.label.clone(),
            fs_name: v.fs_name.clone(),
            total: v.total_bytes,
            free: v.free_bytes,
            used_ratio: v.used_ratio(),
            supports_mft: v.supports_mft(),
            elevated,
        })
        .collect()
}

// ── F2 扫描 ─────────────────────────────────────────────────────────────────

/// 开始一次全量扫描。立刻返回，结果通过事件推给前端：
/// * `scan:progress` —— 扫描中的计数（节流）
/// * `scan:done`     —— 完整摘要（成功）
/// * `scan:error`    —— 失败原因
///
/// `watch` 决定是否同时建立目录监视（F6 用）。默认关：大目录上建路径索引是笔
/// 真实的内存/时间开销，只看面积图的用户不该替它买单。
///
/// 注意 `rustc` **不允许在函数参数上写文档注释**（参数位只接受
/// `allow`/`cfg`/`deny`… 这类属性），所以参数的说明只能写在这里 ——
/// 一开始把 `///` 直接放在 `watch: Option<bool>,` 上面，直接编译不过。
#[tauri::command]
pub fn scan_start(
    app: AppHandle,
    state: State<'_, Shared>,
    hub: State<'_, std::sync::Arc<crate::monitor::MonitorHub>>,
    drive: String,
    root: Option<String>,
    watch: Option<bool>,
) -> Result<(), String> {
    // 扫描会换掉整棵索引树，而监控正挂在树上 —— 两者不能同时进行。
    // 与其让监控线程悄悄改一棵已经被丢弃的树，不如明确拒绝。
    if hub.is_running() {
        return Err("正在监控：请先停止监控再重新扫描（监控需要这棵索引树）".into());
    }
    {
        let mut st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
        if st.scanning {
            return Err("已有一次扫描在进行中，请等它结束".into());
        }
        st.scanning = true;
    }

    let watch = watch.unwrap_or(false);
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut cfg = SessionConfig::default();
        cfg.drive = drive.clone();
        cfg.scan_root = root.as_ref().map(PathBuf::from);
        // 指定了子目录就只能走遍历（`$MFT` 是整卷粒度）
        cfg.force_traverse = cfg.scan_root.is_some();
        // 界面**不**擅自改系统状态：创建 USN Journal 是系统级变更（文档 R3），
        // 以后要做也必须先弹一次明确的确认。
        cfg.allow_create_journal = false;
        cfg.allow_traverse_watch = watch;
        cfg.engine.write_changelog = false;
        cfg.changelog_path = None;

        let app_p = app.clone();
        let mut last: Option<Instant> = None;
        let progress = move |phase: ScanPhase, done: u64, total: u64| {
            let now = Instant::now();
            if let Some(prev) = last {
                if now.duration_since(prev) < PROGRESS_MIN_GAP {
                    return;
                }
            }
            last = Some(now);
            let _ = app_p.emit(
                "scan:progress",
                ScanProgress {
                    phase: phase_cn(phase).to_string(),
                    done,
                    total,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                },
            );
        };

        match Session::open(cfg, progress) {
            Ok(s) => {
                // 摘要在 `s` 被 move 进 Loaded 之前算完
                let summary = summarize(&s, t0.elapsed().as_millis() as u64);
                let loaded = Loaded::new(s);
                let shared = app.state::<Shared>();
                match shared.inner().lock() {
                    Ok(mut st) => {
                        st.loaded = Some(loaded);
                        st.last = Some(summary.clone());
                        st.scanning = false;
                        drop(st);
                        let _ = app.emit("scan:done", summary);
                    }
                    Err(_) => {
                        let _ = app.emit(
                            "scan:error",
                            "内部状态锁被污染（上一次扫描线程 panic 过），请重启程序".to_string(),
                        );
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                let shared = app.state::<Shared>();
                if let Ok(mut st) = shared.inner().lock() {
                    st.last = Some(ScanSummary::failed(msg.clone()));
                    st.scanning = false;
                }
                let _ = app.emit("scan:error", msg);
            }
        }
    });

    Ok(())
}

/// 扫描收尾摘要。**公开**给 `monitor` 用：监控为建立增量来源重扫时，
/// 走的是同一条摘要逻辑 —— 否则界面在"重扫"之后看到的状态栏数字会与
/// 正常扫描不一致。
pub fn summarize(s: &Session, wall_ms: u64) -> ScanSummary {
    let (errors, skipped, all_ok) = match s.traverse_stats.as_ref() {
        Some(t) => (t.errors, t.skipped_reparse, true),
        None => (0, 0, true),
    };
    ScanSummary {
        ok: all_ok,
        backend: s.backend.label_cn().to_string(),
        root_node: s.tree.root,
        root: s
            .cfg
            .scan_root
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| s.cfg.drive.clone()),
        dirs: s.tree.dir_count,
        files: s.tree.file_count,
        logical: s.tree.total_size,
        alloc: s.tree.total_alloc,
        memory_bytes: s.tree.memory_bytes() as u64,
        wall_ms,
        errors,
        skipped_reparse: skipped,
        warnings: s.warnings.clone(),
        incremental: s.incremental_source_cn().to_string(),
        error: None,
    }
}

#[tauri::command]
pub fn scan_summary(state: State<'_, Shared>) -> Option<ScanSummary> {
    state.lock().ok().and_then(|s| s.last.clone())
}

// ── F3 文件夹树 ─────────────────────────────────────────────────────────────

/// 取某个目录的直接子项，按大小降序，分页返回。
///
/// 分页是必须的：一个 `node_modules` 目录可能几万个子目录，一次性塞给前端
/// 会把 DOM 与 IPC 都拖垮。
#[tauri::command]
pub fn tree_rows(
    state: State<'_, Shared>,
    parent: u32,
    offset: Option<usize>,
    limit: Option<usize>,
    mode: Option<String>,
) -> Result<Vec<TreeRow>, String> {
    let mut st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded_mut()?;
    // 监控会改树；派生索引先与当前代数对齐，否则表格里是过期数据
    l.refresh_derived();
    let tree = l.tree();
    let m = size_mode(mode.as_deref());

    let kids = l.children().children(parent);
    let ordered = sorted_children(tree, kids, m);
    let off = offset.unwrap_or(0).min(ordered.len());
    let lim = limit.unwrap_or(500).min(4096);

    Ok(ordered[off..(off + lim).min(ordered.len())]
        .iter()
        .map(|&n| {
            let node = tree.node(n);
            TreeRow {
                node: n,
                name: name_of(tree, n),
                is_dir: node.map(|x| x.is_dir()).unwrap_or(false),
                size: node.map(|x| x.size).unwrap_or(0),
                alloc: node.map(|x| x.alloc).unwrap_or(0),
                children: l.children().child_count(n),
                files: tree
                    .dir_file_counts
                    .get(n as usize)
                    .copied()
                    .unwrap_or(0) as u64,
                mtime: node.map(|x| x.mtime).unwrap_or(0),
            }
        })
        .collect())
}

/// 面包屑：从视图根一路回溯到扫描根。
///
/// 上限 64 层是**防御性**的 —— `Tree::path_of` 自己也带环保护，这里再兜一层，
/// 免得损坏的索引让界面无限循环。
#[tauri::command]
pub fn breadcrumb(state: State<'_, Shared>, node: u32) -> Result<Vec<Crumb>, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded()?;
    let tree = l.tree();

    let mut chain: Vec<Crumb> = Vec::new();
    let mut cur = node;
    for _ in 0..64 {
        let Some(n) = tree.node(cur) else { break };
        if !n.in_use() {
            break;
        }
        let name = name_of(tree, cur);
        chain.push(Crumb {
            node: cur,
            // 根节点的 `$FILE_NAME` 就是 "."，显示出来很怪 —— 换成盘符/扫描根
            name: if name == "." || name.is_empty() {
                tree.drive.clone()
            } else {
                name
            },
        });
        if cur == tree.root || n.parent == cur || n.parent == core_index::arena::INVALID_PARENT {
            break;
        }
        cur = n.parent;
    }
    chain.reverse();
    Ok(chain)
}

// ── F4 面积图 ───────────────────────────────────────────────────────────────

/// 计算一次屏幕空间布局，以**二进制**返回（见 [`crate::wire`]）。
///
/// 前端拿到的是一个 `ArrayBuffer`，用 `DataView` 按固定偏移读 —— 没有 JSON
/// 解析、没有逐字段字符串分配。
#[tauri::command]
pub fn layout(state: State<'_, Shared>, args: LayoutArgs) -> Result<Response, String> {
    if !args.width.is_finite()
        || !args.height.is_finite()
        || args.width <= 0.0
        || args.height <= 0.0
    {
        return Err(format!(
            "视口尺寸非法：{} × {}",
            args.width, args.height
        ));
    }

    let mut st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded_mut()?;
    // 布局要用扩展名 id 着色，所以先确保表已建
    l.ensure_exts();

    let opts = LayoutOptions {
        policy: policy_of(&args),
        mode: size_mode(args.mode.as_deref()),
        shading: shading_of(args.shading.as_deref()),
        now_unix: now_unix(),
    };

    let viewport = Rect::new(0.0, 0.0, args.width, args.height);
    let result = compute_layout(
        &l.session.tree,
        l.children(),
        l.ext_table(),
        args.root,
        viewport,
        &opts,
    );

    Ok(Response::new(wire::encode_layout(args.root, &result.items)))
}

/// 布局统计（前端用来解释「为什么这里没展开」）。
///
/// **注意它会重跑一遍布局。** 前端不要每帧都调它 —— 只在用户点开
/// 「为什么没展开」这类说明时调一次。像素数据走 [`layout`]。
#[derive(Debug, Clone, Serialize)]
pub struct LayoutInfo {
    pub emitted: usize,
    pub collapsed_small: usize,
    pub collapsed_depth: usize,
    pub hit_budget: bool,
    pub max_depth_reached: u32,
}

#[tauri::command]
pub fn layout_info(state: State<'_, Shared>, args: LayoutArgs) -> Result<LayoutInfo, String> {
    let mut st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded_mut()?;
    l.ensure_exts();
    let opts = LayoutOptions {
        policy: policy_of(&args),
        mode: size_mode(args.mode.as_deref()),
        shading: shading_of(args.shading.as_deref()),
        now_unix: now_unix(),
    };
    let viewport = Rect::new(0.0, 0.0, args.width.max(1.0), args.height.max(1.0));
    let r = compute_layout(
        &l.session.tree,
        l.children(),
        l.ext_table(),
        args.root,
        viewport,
        &opts,
    );
    Ok(LayoutInfo {
        emitted: r.stats.emitted,
        collapsed_small: r.stats.collapsed_small,
        collapsed_depth: r.stats.collapsed_depth,
        hit_budget: r.stats.hit_budget,
        max_depth_reached: r.stats.max_depth_reached,
    })
}

// ── F5 类型列表 / F12 Top-N ─────────────────────────────────────────────────

#[tauri::command]
pub fn ext_rows(
    state: State<'_, Shared>,
    limit: Option<usize>,
    mode: Option<String>,
) -> Result<Vec<ExtRow>, String> {
    let mut st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded_mut()?;
    l.ensure_exts();
    let m = size_mode(mode.as_deref());
    let n = limit.unwrap_or(50).min(4096);
    let (top, total) = l.ext_table().top(n, m);

    // `ext_id` 必须与面积图里那个 id **同一个来源**（`ExtensionTable`），
    // 否则「点击类型高亮面积图」会高亮到别的块上。注意查询用的是
    // `ExtStat.ext` 原始值（无扩展名就是空串），不是显示用的「（无扩展名）」。
    let ids: Vec<u32> = top
        .iter()
        .map(|e| l.ext_table().id_of_ext(&e.ext).unwrap_or(u32::MAX))
        .collect();

    Ok(top
        .iter()
        .zip(ids)
        .map(|(e, ext_id)| ExtRow {
            ext: e.display_ext().to_string(),
            ext_id,
            count: e.count,
            size: e.value(m),
            share: if total == 0 {
                0.0
            } else {
                e.value(m) as f64 / total as f64
            },
        })
        .collect())
}

#[tauri::command]
pub fn top_files(
    state: State<'_, Shared>,
    limit: Option<usize>,
    mode: Option<String>,
) -> Result<Vec<FileRow>, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded()?;
    let tree = l.tree();
    let m = size_mode(mode.as_deref());
    let n = limit.unwrap_or(200).min(10_000);
    Ok(top_files_impl(tree, n, m)
        .into_iter()
        .map(|node| {
            let nd = tree.node(node);
            FileRow {
                node,
                name: name_of(tree, node),
                path: tree.path_of(node),
                size: nd.map(|x| x.size).unwrap_or(0),
                alloc: nd.map(|x| x.alloc).unwrap_or(0),
                mtime: nd.map(|x| x.mtime).unwrap_or(0),
            }
        })
        .collect())
}

// ── 详情面板 ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn node_detail(state: State<'_, Shared>, node: u32) -> Result<NodeInfo, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded()?;
    let tree = l.tree();
    let n = tree
        .node(node)
        .filter(|n| n.in_use())
        .ok_or_else(|| format!("节点 {node} 不在索引里"))?;
    Ok(NodeInfo {
        node,
        name: name_of(tree, node),
        path: tree.path_of(node),
        is_dir: n.is_dir(),
        size: n.size,
        alloc: n.alloc,
        children: l.children().child_count(node),
        files: tree
            .dir_file_counts
            .get(node as usize)
            .copied()
            .unwrap_or(0) as u64,
        depth: tree.depth_of(node),
        mtime: n.mtime,
    })
}

/// 从任意路径查找节点（点击面积图/树之外的入口，比如粘贴一个路径）。
#[tauri::command]
pub fn find_by_path(state: State<'_, Shared>, path: String) -> Result<Option<u32>, String> {
    let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
    let l = st.loaded()?;
    Ok(l.session.find_node_by_path(&path))
}

// ── F0 管理员权限（进入软件时申请）─────────────────────────────────────────
//
// 文档 R2 的对策原来是「嵌 UAC 清单自动提权」，代价是每次启动都过一遍 UAC。
// 这里改成进软件时问一次：清单保持 `asInvoker`，用户同意后用 `runas` 重启自己。
// 细节与防死循环的措施见 [`crate::elevate`]。

/// 权限现状。前端拿它决定**要不要弹权限窗**，以及窗里的文案。
#[derive(Debug, Clone, Serialize)]
pub struct ElevationInfo {
    /// 已经能走 MFT 直读 —— 唯一决定"弹不弹"的字段。
    pub privileged: bool,
    /// 令牌本身是提权的（`TokenIsElevated`）
    pub token_elevated: bool,
    /// `\\.\C:` 真的能打开。与上一项**可能不一致**：系统关掉 UAC 时令牌报
    /// 未提权，而卷其实读得到。所以两个都摆出来，别只看一个。
    pub volume_readable: bool,
    /// 值不值得去申请（不在管理员组时仍然给机会：`runas` 会转而要密码）
    pub can_request: bool,
    /// `"admin"` / `"filtered"` / `"standard"` / `"unknown"` —— 给界面挂样式用
    pub kind: String,
    /// 短标签（状态栏徽章）
    pub label: String,
    /// 一句话说明**现在**是什么状态
    pub hint: String,
    /// 点「以管理员身份重启」之后会发生什么（不同账户类型文案不同）
    pub action_hint: String,
    /// `域名\用户名`
    pub account: String,
    pub exe: String,
    /// 重启时实际会传给新进程的完整命令行 —— 摆出来给用户看，证明不加私货
    pub command_line: String,
    /// 本次启动已经尝试过提权（是则**不再自动弹窗**，见 `elevate` 的防死循环）
    pub attempted: bool,
}

#[tauri::command]
pub fn elevation_info() -> ElevationInfo {
    build_elevation_info()
}

/// 抽成普通函数，方便单测（`#[tauri::command]` 的包装函数没法直接调）。
fn build_elevation_info() -> ElevationInfo {
    use crate::elevate::{status, ElevationKind};

    let s = status();
    let (kind, label, hint, action_hint) = match s.kind {
        ElevationKind::Admin => (
            "admin",
            "管理员",
            "已具备管理员权限：MFT 直读与 USN 增量监控都可用。".to_string(),
            String::new(),
        ),
        ElevationKind::Filtered => (
            "filtered",
            "降级模式",
            "当前是普通权限：账户在管理员组里，但被 UAC 过滤掉了，读不了卷设备。".to_string(),
            "重启时 Windows 会弹一个权限确认框，点「是」就行 —— 不需要输入密码。".to_string(),
        ),
        ElevationKind::Standard => (
            "standard",
            "降级模式",
            "当前是普通权限：这个账户不在管理员组里。".to_string(),
            "重启时 Windows 会要求输入一个管理员账户的密码（UAC 的凭据提示）。".to_string(),
        ),
        ElevationKind::Unknown => (
            "unknown",
            "未知",
            "没能读到权限令牌信息，无法判断当前权限。".to_string(),
            "可以试一次；失败的话当前窗口会保持原样，不会有副作用。".to_string(),
        ),
    };

    // 令牌说未提权、但卷真的打得开（典型是系统关了 UAC）→ 按有权限处理，
    // 否则会出现「明明是管理员却一直让你提权」的荒谬提示。
    let mut hint = hint;
    if !s.token_elevated && s.volume_readable {
        hint = format!("{hint}（实测卷设备可以直接打开，按有权限处理）");
    }

    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // 展示的命令行 = 现在的程序 + 重启时会带的参数（含防死循环标记）
    let mut args = crate::elevate::original_args();
    args.push(crate::elevate::ATTEMPTED_FLAG.to_string());
    let command_line = if exe.is_empty() {
        crate::elevate::build_command_line(&args)
    } else {
        format!("{} {}", crate::elevate::quote_arg(&exe), crate::elevate::build_command_line(&args))
    };

    let account = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => match std::env::var("USERNAME") {
            Ok(u) => format!("{d}\\{u}"),
            Err(_) => d,
        },
        _ => std::env::var("USERNAME").unwrap_or_else(|_| "（未知账户）".to_string()),
    };

    ElevationInfo {
        privileged: s.privileged(),
        token_elevated: s.token_elevated,
        volume_readable: s.volume_readable,
        can_request: s.can_request(),
        kind: kind.to_string(),
        label: label.to_string(),
        hint,
        action_hint,
        account,
        exe,
        command_line,
        attempted: s.attempted,
    }
}

/// 用 `runas` 动词重启自己（此时才弹 UAC）。
///
/// **立刻返回**，真实结果通过 `admin:relaunch` 事件回传。原因是
/// `ShellExecuteW("runas")` 会一直阻塞到用户在 UAC 框上做出选择 ——
/// 直接在这个命令里等，界面线程会被冻住好几秒（用户会以为程序卡死了）。
/// 所以这里只做三件快事：判能不能申请、判有没有在扫描、开线程。
#[tauri::command]
pub fn relaunch_elevated(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    if build_elevation_info().privileged {
        return Err("当前已经是管理员权限，不用重启".into());
    }
    {
        // 重启会把扫描一起丢掉，所以正在扫的时候不接这个请求。
        // 这不是"功能限制"，只是避免用户把一次几分钟的扫描误杀掉。
        let st = state.lock().map_err(|_| "状态锁被污染".to_string())?;
        if st.scanning {
            return Err("正在扫描，等它结束后再切换权限（重启会中断扫描）".into());
        }
    }

    std::thread::spawn(move || {
        let outcome = crate::elevate::relaunch_elevated();
        let _ = app.emit("admin:relaunch", outcome.clone());
        if outcome.started {
            // 留一点时间让前端把「正在重启」画出来，然后退出本进程：
            // 两个窗口并存只会让人困惑（旧的是降级实例，新的是管理员实例）。
            std::thread::sleep(Duration::from_millis(400));
            std::process::exit(0);
        }
    });
    Ok(())
}

/// 读偏好。文件损坏/缺失一律退回默认值（见 [`crate::prefs`]）。
#[tauri::command]
pub fn gui_prefs() -> crate::prefs::GuiPrefs {
    crate::prefs::load()
}

/// 写偏好。
///
/// 参数名不叫 `prefs`：那会和 `crate::prefs` 模块同名，`prefs::save` 就没法写了。
#[tauri::command]
pub fn set_gui_prefs(value: crate::prefs::GuiPrefs) -> Result<crate::prefs::GuiPrefs, String> {
    crate::prefs::save(&value)?;
    Ok(value)
}

/// 权限现状的纯文本报告（`filewatch-gui.exe --check-elevation`）。
///
/// 存在的理由有两个，都不是"顺手加个调试开关"：
///
/// 1. **这一段在正常使用中无法自动验证。** 界面点击没法自动化，而权限检测
///    整条链（手写 FFI → 令牌类型 → 是否可读卷 → 判定）恰恰是最容易静默写错的
///    部分：判错了不会崩，只会让用户一直看到错的那句说明。有了它，就能在
///    命令行里对着真实机器断言；
/// 2. 用户报"为什么一直提示我没有管理员权限"时，让它跑一下比来回问快得多。
///
/// 输出走 `write_all` 而不是 `println!`：这个程序是 `windows_subsystem="windows"`
/// 的，双击启动时没有 stdout，`println!` 会 panic —— 一个诊断命令不该以
/// 崩溃收场。
pub fn elevation_report() -> String {
    let i = build_elevation_info();
    let p = crate::prefs::load();
    let yes = |b: bool| if b { "是" } else { "否" };
    let mut s = String::new();
    s.push_str("FileWatch 权限检测\n");
    s.push_str(&format!("  账户            : {}\n", i.account));
    s.push_str(&format!("  令牌已提权      : {}\n", yes(i.token_elevated)));
    s.push_str(&format!("  卷设备可直读    : {}\n", yes(i.volume_readable)));
    s.push_str(&format!("  判定            : {}（{}）\n", i.label, i.kind));
    s.push_str(&format!("  可以去申请提权  : {}\n", yes(i.can_request)));
    s.push_str(&format!("  本次启动带标记  : {}\n", yes(i.attempted)));
    s.push_str(&format!("  状态说明        : {}\n", i.hint));
    if !i.action_hint.is_empty() {
        s.push_str(&format!("  点「重启」后    : {}\n", i.action_hint));
    }
    s.push_str(&format!("  将要执行的命令  : {}\n", i.command_line));
    s.push_str(&format!(
        "  偏好文件        : {}\n",
        crate::prefs::prefs_path().display()
    ));
    s.push_str(&format!(
        "  当前偏好        : 进入软件时自动询问 = {}\n",
        yes(p.ask_admin_on_start)
    ));
    s
}

/// 把 [`elevation_report`] 打到 stdout。返回给 [`crate::main`] 用。
pub fn print_elevation_report() {
    use std::io::Write;
    let text = elevation_report();
    let _ = std::io::stdout().write_all(text.as_bytes());
    let _ = std::io::stdout().flush();
}

/// 当前时间（epoch 毫秒）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Loaded;

    /// 参数映射：这些「字符串 → 枚举」的翻译只有一处，错了界面就会用错口径
    /// 或者画错着色，而画面本身不会报错 —— 所以单独钉住。
    #[test]
    fn option_mappers() {
        assert_eq!(size_mode(None), SizeMode::Logical);
        assert_eq!(size_mode(Some("logical")), SizeMode::Logical);
        assert_eq!(size_mode(Some("allocated")), SizeMode::Allocated);
        assert_eq!(size_mode(Some("Alloc")), SizeMode::Allocated);

        assert_eq!(shading_of(None), ShadingMode::ByExtension);
        assert_eq!(shading_of(Some("depth")), ShadingMode::ByDepth);
        assert_eq!(shading_of(Some("age")), ShadingMode::ByAge);
    }

    /// `min_side` 必须被夹住：前端一个笔误不该让 LOD 失效（几十万个亚像素矩形
    /// 全画出来 = 帧率崩）或让任何目录都不展开（面积图只剩一层）。
    #[test]
    fn min_side_is_clamped() {
        let mk = |v: Option<f32>| {
            policy_of(&LayoutArgs {
                width: 100.0,
                height: 100.0,
                root: 5,
                mode: None,
                shading: None,
                deep: None,
                min_side: v,
            })
            .min_side
        };
        assert_eq!(mk(None), 80.0, "缺省应是引擎默认值");
        assert_eq!(mk(Some(1.0)), 4.0, "过小要夹到下限");
        assert_eq!(mk(Some(99999.0)), 2000.0, "过大要夹到上限");
        assert_eq!(mk(Some(32.0)), 32.0, "正常值原样透传");
    }

    /// 端到端：**引擎 → 布局 → 二进制**。
    ///
    /// 为什么需要它：界面点击无法自动化，所以真正能自动验证的边界就是这一段。
    /// 它同时覆盖三件容易坏的事：
    ///   ① `Session::open` 的参数装配（与 `scan_start` 走同一条路）；
    ///   ② `compute_layout` 的调用形状（参数顺序、借用拆分）；
    ///   ③ 线格式的长度/偏移（前端按硬编码偏移读，错一个字节就全乱）。
    #[test]
    fn engine_to_layout_to_wire() {
        let dir = std::env::temp_dir().join(format!("fw_gui_wire_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.bin"), vec![b'a'; 4096]).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), vec![b'b'; 1024]).unwrap();

        let mut cfg = SessionConfig::default();
        cfg.drive = "C:".into();
        cfg.scan_root = Some(dir.clone());
        // 与本机真实情况一致：非管理员只能走遍历
        cfg.force_traverse = true;
        cfg.allow_traverse_watch = false;
        cfg.engine.write_changelog = false;

        let s = Session::open(cfg, |_, _, _| {}).expect("会话应能打开");
        // 注意顺序：摘要必须在 `s` 被 move 进 Loaded 之前算出来 ——
        // `scan_start` 里也是这个顺序，这里顺带把这条约束也测了。
        let sum = summarize(&s, 7);
        assert_eq!(sum.files, 2, "应看到两个文件");
        assert!(sum.logical >= 5120, "逻辑大小应含两个文件的字节数");
        let root = sum.root_node;

        let mut loaded = Loaded::new(s);
        loaded.ensure_exts();

        let vp = Rect::new(0.0, 0.0, 1200.0, 800.0);
        let opts = LayoutOptions {
            policy: LodPolicy::default(),
            mode: SizeMode::Logical,
            shading: ShadingMode::ByExtension,
            now_unix: 0,
        };
        let r = compute_layout(
            loaded.tree(),
            loaded.children(),
            loaded.ext_table(),
            root,
            vp,
            &opts,
        );
        assert!(
            r.items.len() >= 3,
            "根 + sub + 两个文件，至少 3 个方块，实际 {}",
            r.items.len()
        );
        assert_eq!(r.items[0].node, root, "第一项必须是视图根（前端靠这个起点做面包屑）");
        for it in &r.items {
            assert!(
                it.rect.x >= -1.0 && it.rect.right() <= vp.w + 1.0 && it.rect.y >= -1.0
                    && it.rect.bottom() <= vp.h + 1.0,
                "矩形越出视口：{:?}",
                it.rect
            );
        }

        let bytes = wire::encode_layout(root, &r.items);
        assert_eq!(bytes.len(), wire::HEADER_BYTES + r.items.len() * wire::ITEM_BYTES);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            wire::MAGIC
        );
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize,
            r.items.len()
        );
        assert_eq!(
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            root,
            "头里的 root 要与第 0 项一致"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 面包屑必须从扫描根一路挂到目标节点，且**根在最前**（界面靠它决定
    /// 「已经在最外层了」的边界）。
    #[test]
    fn breadcrumb_reaches_the_root() {
        let dir = std::env::temp_dir().join(format!("fw_gui_crumb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub").join("deep")).unwrap();
        std::fs::write(dir.join("sub").join("deep").join("f.txt"), b"x").unwrap();

        let mut cfg = SessionConfig::default();
        cfg.drive = "C:".into();
        cfg.scan_root = Some(dir.clone());
        cfg.force_traverse = true;
        cfg.allow_traverse_watch = false;
        cfg.engine.write_changelog = false;
        let s = Session::open(cfg, |_, _, _| {}).unwrap();
        let root = s.tree.root;
        let mut loaded = Loaded::new(s);
        loaded.refresh_derived();

        // 找到 deep 目录
        let deep = loaded
            .children()
            .children(root)
            .iter()
            .copied()
            .find(|&n| super::name_of(loaded.tree(), n) == "sub")
            .expect("应有 sub 目录");
        let deep = loaded
            .children()
            .children(deep)
            .iter()
            .copied()
            .find(|&n| super::name_of(loaded.tree(), n) == "deep")
            .expect("应有 deep 目录");

        let tree = loaded.tree();
        let mut chain = Vec::new();
        let mut cur = deep;
        for _ in 0..64 {
            let n = match tree.node(cur) {
                Some(n) if n.in_use() => n,
                _ => break,
            };
            chain.push(cur);
            if cur == tree.root || n.parent == cur || n.parent == core_index::arena::INVALID_PARENT {
                break;
            }
            cur = n.parent;
        }
        chain.reverse();
        assert_eq!(chain[0], root, "链条顶端必须是扫描根");
        assert_eq!(*chain.last().unwrap(), deep, "链条末端必须是目标节点");
        assert_eq!(chain.len(), 3, "root → sub → deep");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 权限信息的自洽性。
    ///
    /// 弹不弹权限窗**只看 `privileged`**，而这个字段是两个判据的并集
    /// （令牌 + 卷能不能打开）。判据一多就容易自相矛盾 —— 比如
    /// 「已提权」却还标着「可以申请」，界面就会一边显示管理员一边弹窗追问。
    /// 这些互相咬合的约束全部钉在这里。
    #[test]
    fn elevation_info_is_self_consistent() {
        let i = build_elevation_info();

        assert!(
            ["admin", "filtered", "standard", "unknown"].contains(&i.kind.as_str()),
            "kind 只允许这四个取值，实际 {}",
            i.kind
        );
        assert!(!i.hint.is_empty(), "hint 是给用户看的说明，不能为空");
        assert!(!i.account.is_empty(), "账户名不能为空");
        assert!(
            i.exe.ends_with("filewatch-gui.exe") || i.exe.to_lowercase().ends_with(".exe"),
            "exe 应当是当前程序，实际 {}",
            i.exe
        );
        // 展示的完整命令行必须带上防死循环标记，否则用户看到一个"没有私货"
        // 的命令行会以为我们藏着参数
        assert!(
            i.command_line.contains(crate::elevate::ATTEMPTED_FLAG),
            "命令行应含 {}：{}",
            crate::elevate::ATTEMPTED_FLAG,
            i.command_line
        );
        assert!(
            i.command_line.contains("--elevation-attempted")
                && !i.command_line.contains("--elevation-attempted --elevation-attempted"),
            "标记只该出现一次：{}",
            i.command_line
        );

        if i.privileged {
            assert!(!i.can_request, "已经有权限就不该再让人申请");
            assert_eq!(i.kind, "admin", "有权限时 kind 必须是 admin，实际 {}", i.kind);
            assert!(i.action_hint.is_empty(), "有权限时不该有『点了会怎样』的说明");
        } else {
            assert_eq!(i.label, "降级模式");
            assert!(
                i.kind == "filtered" || i.kind == "standard",
                "没权限时只该是 filtered/standard，实际 {}",
                i.kind
            );
            assert!(
                !i.action_hint.is_empty(),
                "没权限时要说清楚点『重启』会发生什么"
            );
        }

        // 跑测试时命令行不带标记
        assert!(!i.attempted, "cargo test 不该带 {}", crate::elevate::ATTEMPTED_FLAG);
    }

    /// 本机实测：账户在管理员组但被 UAC 过滤 → 必须判成 `filtered`，
    /// 也即「能弹窗申请、不用输密码」。这条断言把机器的实际情况钉住，
    /// 万一哪天变成了 `standard`，弹窗文案会立刻露馅 —— 而那种错
    /// 只有用户看得见，测试是唯一的拦网。
    #[test]
    fn this_machine_can_request_elevation() {
        let i = build_elevation_info();
        assert_eq!(
            i.kind, "filtered",
            "本机账户在 Administrators 组（whoami /groups 显示『只用于拒绝的组』），\
             应判成 filtered；实际 {}，hint={}",
            i.kind, i.hint
        );
        assert!(!i.privileged, "非提权时不该报成有权限");
        assert!(i.can_request, "filtered 必须可以去申请");
    }

    /// `--check-elevation` 的输出必须**能被脚本断言**，也必须和界面用的
    /// 那一份数据是同一个来源。少印一行、或者判定与 `elevation_info` 不一致，
    /// 都会让这个诊断口子变成误导。
    #[test]
    fn elevation_report_matches_info() {
        let text = elevation_report();
        let i = build_elevation_info();

        // 每一行该有的都得在 —— 报告的价值全在"齐全且是真值"
        for key in [
            "账户",
            "令牌已提权",
            "卷设备可直读",
            "判定",
            "可以去申请提权",
            "本次启动带标记",
            "将要执行的命令",
            "偏好文件",
        ] {
            assert!(text.contains(key), "报告缺少「{key}」这一行：\n{text}");
        }

        // 判定必须与接口返回的一致（不能各算各的）
        assert!(
            text.contains(&format!("{}（{}）", i.label, i.kind)),
            "报告里的判定与 elevation_info 不一致：{}\n{}",
            i.kind,
            text
        );
        assert!(
            text.contains(&i.account),
            "报告应含账户名 {}：\n{}",
            i.account,
            text
        );

        // 不能有未替换的占位符或者 "None"
        assert!(!text.contains("None"), "报告里漏出了 None：\n{text}");
        assert!(!text.contains("{}"), "报告里有没填上的格式占位符：\n{text}");
    }
}
