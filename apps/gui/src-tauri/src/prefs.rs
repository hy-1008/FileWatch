//! GUI 的持久化偏好。
//!
//! 只有一项：**进入软件时要不要自动问"以管理员身份运行"**。
//!
//! 位置：`%APPDATA%\FileWatch\gui-prefs.json`。
//! 之所以要落盘，是因为每次启动都弹一次权限窗很快就会变成噪音 —— 用户第一次
//! 点了「不再询问」之后，界面必须记住这件事。但"不再自动问"**不等于"没地方问"**：
//! 状态栏右下角的「权限」按钮任何时候都能把同一个窗重新调出来，
//! 并且会把这一项**打开回去**，所以用户永远不会被自己的选择锁死。
//!
//! 读取一律**容错**：文件不存在、被截断、被手改成非法 JSON，都退回默认值。
//! 一个偏好文件不该有能力让程序起不来。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuiPrefs {
    /// 启动时若没有管理员权限，是否自动弹窗询问。
    pub ask_admin_on_start: bool,
}

impl Default for GuiPrefs {
    fn default() -> GuiPrefs {
        // 默认**问一次**：本工具的核心优势（MFT 直读 + USN 增量）全都要管理员权限，
        // 不主动问的话，大多数用户会一直跑在慢 1~2 个数量级的降级模式里，
        // 并且会以为"这软件就这么慢"。
        GuiPrefs { ask_admin_on_start: true }
    }
}

/// `%APPDATA%\FileWatch\gui-prefs.json`。
///
/// 取不到 `APPDATA`（极少见）时退到 `%USERPROFILE%\.filewatch\`，
/// 再不行退到当前目录 —— 宁可把配置写在别处，也不要因此 panic。
pub fn prefs_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".filewatch")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("FileWatch").join("gui-prefs.json")
}

/// 从磁盘读偏好。**任何异常都退回默认值**，不向上抛错。
pub fn load() -> GuiPrefs {
    load_from(&prefs_path())
}

pub fn load_from(path: &Path) -> GuiPrefs {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<GuiPrefs>(&s).unwrap_or_default(),
        Err(_) => GuiPrefs::default(),
    }
}

/// 落盘。先写临时文件再改名，避免「写到一半断电」留下半个 JSON
/// （那个文件下次会被当作损坏文件、静默退回默认值，用户会觉得偏好莫名其妙的丢了）。
pub fn save(p: &GuiPrefs) -> Result<PathBuf, String> {
    let path = prefs_path();
    save_to(&path, p)?;
    Ok(path)
}

pub fn save_to(path: &Path, p: &GuiPrefs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建配置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(p).map_err(|e| format!("序列化失败：{e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写配置失败：{e}"))?;
    // Windows 上 `rename` 到已存在的文件是允许的（MoveFileEx 语义）
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换配置失败：{e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fw_prefs_{}_{}", std::process::id(), name))
    }

    #[test]
    fn default_asks_once() {
        assert!(
            GuiPrefs::default().ask_admin_on_start,
            "默认必须问一次 —— 不问的话用户会一直待在降级模式里"
        );
    }

    #[test]
    fn round_trip() {
        let p = tmp("round");
        let _ = std::fs::remove_file(&p);
        let v = GuiPrefs { ask_admin_on_start: false };
        save_to(&p, &v).expect("应能写入");
        assert_eq!(load_from(&p), v);
        // 再写回 true，确认不是单向的
        save_to(&p, &GuiPrefs { ask_admin_on_start: true }).expect("应能写入");
        assert!(load_from(&p).ask_admin_on_start);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let p = tmp("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p), GuiPrefs::default());
    }

    /// 损坏的配置**不能**让程序起不来，也不能让"不再询问"变成"永远询问"
    /// 之外的第三种状态 —— 退回默认（问一次）是唯一合理的解释。
    #[test]
    fn corrupt_file_falls_back_to_default() {
        for junk in ["{ not json", "", "[1,2,3]", r#"{"ask_admin_on_start": "yes"}"#] {
            let p = tmp("corrupt");
            std::fs::write(&p, junk).unwrap();
            assert_eq!(
                load_from(&p),
                GuiPrefs::default(),
                "损坏内容 {:?} 应退回默认值",
                junk
            );
            let _ = std::fs::remove_file(&p);
        }
    }

    /// 缺字段也按默认处理（以后加字段时老文件仍可读）
    #[test]
    fn partial_json_falls_back() {
        let p = tmp("partial");
        std::fs::write(&p, "{}").unwrap();
        assert!(load_from(&p).ask_admin_on_start);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn prefs_path_is_under_appdata() {
        let s = prefs_path().to_string_lossy().to_string();
        assert!(s.ends_with("FileWatch\\gui-prefs.json") || s.ends_with("FileWatch/gui-prefs.json"),
            "偏好文件应落在 FileWatch 子目录里，实际 {s}");
    }
}
