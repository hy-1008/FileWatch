//! Tauri 构建脚本。
//!
//! 它做三件事：读 `tauri.conf.json` 生成上下文、把 `capabilities/` 编成权限清单、
//! 生成 Windows 资源文件（图标 / 版本信息）。
//!
//! 注意 `icons/icon.ico` 是**必需**的：缺了它会直接构建失败
//! （`required for generating a Windows Resource file`）。这个坑在 GNU 目标下
//! 同样存在，与工具链无关。

fn main() {
    tauri_build::build()
}
