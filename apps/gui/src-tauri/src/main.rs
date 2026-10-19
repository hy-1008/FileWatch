//! FileWatch 图形界面（Tauri 2）。
//!
//! # 这一层负责什么
//!
//! 只做三件事：**把引擎的状态暴露成命令**、**把布局结果按二进制推给前端**、
//! **把窗口开起来**。所有算法（扫描、聚合、布局、阈值）都在 `core-*` 里，
//! CLI 与 GUI 共用同一条装配路径 —— 这一点是刻意的，否则两个前端会慢慢长出
//! 两套行为，同一个目录在 CLI 和界面里显示出不同的大小。
//!
//! # 为什么引擎不在这个 workspace 里
//!
//! `crates/**` 坚持零第三方依赖；GUI 必然要用 Tauri。所以 `src-tauri` 自己
//! 就是一个 workspace 根（见 `Cargo.toml`），通过 path 依赖引用引擎，
//! 双方互不污染：在引擎根目录 `cargo build` 永远不会去编译 Tauri。
//!
//! # 构建
//!
//! ```text
//! cd apps/gui/src-tauri
//! cargo build --release      # 产物：target/release/filewatch-gui.exe
//! ```
//!
//! 两个容易踩的点：
//! * `icons/icon.ico` 必须存在，否则 `tauri-build` 直接失败（用
//!   `python apps/gui/tools/make-icon.py` 生成）；
//! * `webview2-com-sys` 的构建脚本会把 `WebView2Loader.dll` 复制到 exe 旁边 ——
//!   分发时要连它一起带上，否则启动即报缺 DLL。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyze;
mod autostart;
mod commands;
mod elevate;
mod monitor;
mod independent;
mod prefs;
mod search;
mod profile;
mod picker;
mod explorer;
mod scheduler;
mod state;
mod tray;
mod wire;

use analyze::{DiffHub, DupHub};
use monitor::MonitorHub;
use state::AppState;
use std::sync::{Arc, Mutex};
use tauri::Manager;

fn main() {
    // 诊断入口：不开窗口、不进 GUI，直接把权限现状打到 stdout。
    // 存在的理由见 `commands::elevation_report` —— 权限检测这段没法用点击
    // 覆盖，必须留一个能在命令行里断言的口子。
    if std::env::args().any(|a| a == "--check-elevation") {
        commands::print_elevation_report();
        return;
    }

    let hub = MonitorHub::new();
    let independent = independent::IndependentHub::new();
    // 后台周期任务（只有性能模式会真的起线程）。放进 manage 是为了让
    // `set_gui_prefs` 能在用户切模式时立刻对齐 —— 切回轻量模式必须**当场停**，
    // 而不是等下次启动。
    let scheduler = scheduler::SchedulerSlot::default();
    // 重复文件查找的状态与监控、扫描都分开：它有自己的后台线程和进度，
    // 而且**不需要**持有索引树的锁（候选表在启动线程时就复制出来了）。
    // 每次运行程序只有一份，所以用 `Arc` 而不是再造一个全局。
    let dup = DupHub::new();
    // 快照作业（存快照 / 对比）的状态：与 dup 同理，独立一份，
    // 因为它有自己的后台线程与进度事件。
    let diff = DiffHub::new();
    // 内容搜索（F13 后半段）：读文件要几秒到几十秒，所以与 dup/diff 同一套
    // 「后台线程 + 进度事件」的节奏。
    let content = search::ContentHub::new();
    let app = tauri::Builder::default()
        // 阈值预警要能在窗口收起时也通知到人（Windows 通知中心）
        .plugin(tauri_plugin_notification::init())
        .manage(Mutex::new(AppState::default()))
        // 监控状态独立于扫描状态：它的环形缓冲由监控线程写、命令读，
        // 与扫描结果（`Mutex<AppState>`）不是一回事，混在一起只会让锁的
        // 持有时间变长。
        .manage(hub.clone())
        .manage(independent.clone())
        .manage(Arc::clone(&dup))
        .manage(Arc::clone(&diff))
        .manage(Arc::clone(&content))
        .manage(scheduler)
        .invoke_handler(tauri::generate_handler![
            // F1 盘符
            commands::list_volumes,
            picker::choose_path,
            explorer::reveal_path,
            explorer::explore_path,
            independent::independent_snapshot,
            independent::independent_upsert,
            independent::independent_remove,
            // F2 扫描
            commands::scan_start,
            commands::scan_summary,
            // F3 文件夹树
            commands::tree_rows,
            commands::breadcrumb,
            // F4 面积图
            commands::layout,
            commands::layout_info,
            // F5 类型列表 / F12 Top-N
            commands::ext_rows,
            commands::top_files,
            // F13 文件搜索（在内存索引里搜，不碰磁盘）
            commands::search_nodes,
            // F13 内容搜索（后台线程 + content:progress / content:done / content:error）
            search::content_search_start,
            search::content_search_status,
            // 详情与定位
            commands::node_names,
            commands::node_detail,
            commands::find_by_path,
            commands::node_parent,
            // F0 管理员权限（进入软件时申请）
            commands::elevation_info,
            commands::relaunch_elevated,
            commands::gui_prefs,
            commands::set_gui_prefs,
            // 运行模式（轻量 / 标准 / 性能）与开机自启动
            commands::profile_budget,
            commands::autostart_status,
            commands::set_autostart,
            // F6/F7/F8 实时监控 / 阈值预警 / 变更流水
            monitor::monitor_start,
            monitor::monitor_stop,
            monitor::monitor_snapshot,
            monitor::monitor_changes,
            monitor::monitor_alerts,
            monitor::monitor_export_csv,
            monitor::monitor_default_export_dir,
            // F12 重复文件（后台线程 + dup:progress / dup:done / dup:error）
            analyze::dup_start,
            analyze::dup_view,
            analyze::dup_export_csv,
            // F11 快照对比（后台线程 + diff:progress / diff:done / diff:saved / diff:error）
            analyze::analyze_default_path,
            analyze::snapshot_save,
            analyze::snapshot_compare,
            analyze::diff_status,
            analyze::diff_export_csv,
        ])
        // 点「×」时是藏到托盘还是真的退出，由偏好决定（见 `tray::handle_close_requested`）：
        // 判定放在后端，界面正忙（扫描中 / 事件洪水）时关窗口也不会关错。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if tray::handle_close_requested(window.app_handle()) {
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("FileWatch GUI 初始化失败");

    // 诊断入口：跑一遍"预警提醒"全链路（系统通知 + 托盘闪动 + 菜单记一条），
    // 把最近预警写进 %TEMP% 下的文件再退出。
    // 为什么需要它：通知弹没弹、图标闪没闪，这两件事没法用单测覆盖，只能实跑。
    if std::env::args().any(|a| a == "--selftest-alert") {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            crate::tray::notify_alert(&handle, "自检：C:/示例目录 增长 1.2 GB，超过阈值 1 GB");
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let recent = crate::tray::recent_alerts(&handle);
            let out = std::env::temp_dir().join("filewatch-alert-selftest.txt");
            let _ = std::fs::write(&out, recent.join("\n"));
            std::thread::sleep(std::time::Duration::from_millis(4000));
            handle.exit(0);
        });
    }

    // 托盘**始终**存在：选了"最小化到托盘"之后，退出只能走它的右键菜单，
    // 少了这条，程序就会变成一个关不掉的幽灵。
    if let Err(e) = tray::build(app.handle()) {
        // 托盘建不起来不该让程序起不来 —— 它只是个便利。
        eprintln!("[FileWatch] {e}");
    }

    // 退出前给监控线程一点时间收尾：`Session::shutdown` 会吐出 debounce 池里
    // 剩下的变更并保存 USN 游标。不这么做的话，关窗口 = 丢掉最后几秒的变更，
    // 下次启动还要从旧游标重放一遍。
    // `run` 的回调要求 `'static + Send`，所以把 Arc 复制一份进闭包
    // （`arc.clone()` 只是引用计数 +1，不是复制缓冲）。
    let hub_exit = Arc::clone(&hub);
    // 启动后按当前偏好决定要不要跑后台任务（性能模式）。
    // 周期任务的线程每秒醒一次看停止标志，所以退出时最多等它一秒出头 ——
    // 和监控线程共用同一个 1.2 s 预算，关窗口不该卡住。
    let startup = app.handle().clone();
    let startup_prefs = prefs::load();
    app.state::<scheduler::SchedulerSlot>().sync(&startup, &startup_prefs);

    // 预警出口：托盘通知/闪动。必须在 resume_saved 之前 —— 恢复的独立监控
    // 一起来就可能马上触发预警。
    independent.attach(app.handle().clone());
    independent.resume_saved(app.handle().clone());
    app.run(move |_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            independent.stop_all();
            hub_exit.request_stop();
            hub_exit.wait_idle(std::time::Duration::from_millis(1200));
            // 退出回调里拿的是一个引用句柄，`state` 需要所有权，所以用 `try_state`
            if let Some(slot) = _handle.try_state::<scheduler::SchedulerSlot>() {
                slot.stop_all();
            }
        }
    });
}
