//! Tauri 构建脚本。
//!
//! 它做三件事：读 `tauri.conf.json` 生成上下文、把 `capabilities/` 编成权限清单、
//! 生成 Windows 资源文件（图标 / 版本信息）。
//!
//! 注意 `icons/icon.ico` 是**必需**的：缺了它会直接构建失败
//! （`required for generating a Windows Resource file`）。这个坑在 GNU 目标下
//! 同样存在，与工具链无关。
//!
//! ── 前端资源（`../src/**`）为什么**不必**在这里声明 rerun-if-changed ──
//!
//! `tauri::generate_context!()`（`src/main.rs`）在**编译主 crate 时**读取
//! `frontendDist` 指向的前端目录并把内容嵌进 exe。Tauri 2 会把它们压缩，
//! 所以 **exe 里 grep 不到明文** —— 别用「搜字符串」判断前端有没有编进去。
//!
//! 关键是：前端源文件会被 rustc 记进 **dep-info**，可以直接查看：
//!
//! ```text
//! target/<triple>/release/.fingerprint/filewatch-gui-<hash>/dep-bin-filewatch-gui
//!    …\apps\gui\src-tauri\..\src\app.js
//!    …\apps\gui\src-tauri\..\src\wire.js          ← 每个前端文件一条
//! ```
//!
//! cargo 的 fingerprint 就建立在这份清单上，于是「只改前端、不动任何 `.rs`」
//! 也会重编主 crate，宏随即重新读源文件、重新嵌入。实测确认过
//! （`build.rs` 未动、只改 `src/view-rule.js`）：构建输出里出现
//! `Compiling filewatch-gui`。
//!
//! 所以**不要**在这里补 `rerun-if-changed=../src`：多余，而且会让构建脚本
//! 也重跑一遍，白多编一次。

fn main() {
    tauri_build::build()
}
