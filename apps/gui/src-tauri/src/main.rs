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

mod commands;
mod state;
mod wire;

use state::AppState;
use std::sync::Mutex;

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(AppState::default()))
        .invoke_handler(tauri::generate_handler![
            // F1 盘符
            commands::list_volumes,
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
            // 详情与定位
            commands::node_detail,
            commands::find_by_path,
        ])
        .run(tauri::generate_context!())
        .expect("FileWatch GUI 启动失败");
}
