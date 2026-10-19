//! GUI 的持久化偏好。
//!
//! 三类：**进入软件时要不要自动问"以管理员身份运行"**、**盘符栏是否折叠**、
//! **运行模式（轻量 / 标准 / 性能）与性能模式下的后台周期任务**。
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

/// 加字段时**必须**带上 `#[serde(default)]`（这里挂在 struct 上）：没有它，
/// 老配置文件（缺新字段）会整个反序列化失败、退回默认值 —— 用户升级一次就得
/// 重新点一遍「不再自动询问权限」。有了它，缺的字段按 [`Default`] 补，
/// 用户已经做过的选择原样保留。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    /// 启动时若没有管理员权限，是否自动弹窗询问。
    pub ask_admin_on_start: bool,
    /// 盘符栏是否折叠（把横向空间让给文件夹树）。
    /// 纯界面偏好；长什么样全在 `style.css` 的 `body.drives-collapsed` 里。
    pub drives_collapsed: bool,
    /// 监控面板的「阈值预警」是否折叠（把纵向空间让给变更流水）。
    /// 折叠后只留标题栏 —— 条数还看得见，所以不会把预警藏没。
    pub alerts_collapsed: bool,
    /// 运行模式：`"lean"` / `"balanced"` / `"performance"`。
    /// 具体参数不在前端另算一遍 —— 由 [`crate::profile::Profile::parse`] 解释，
    /// 派生数字走 `profile_budget` 命令下发。非法值按标准处理。
    pub profile: String,
    /// 性能模式下是否**轮流全量扫描**各盘符（会吃 IO，但能一直有最新结果）。
    /// 默认开：这是「性能模式」被选中的理由，用户随时能关。
    pub periodic_scan: bool,
    /// 后台周期任务的间隔（分钟）。夹紧见 [`GuiPrefs::clamped`]。
    pub periodic_minutes: u32,
    /// 点窗口右上角「×」时做什么：`"tray"` = 最小化到托盘（监控继续跑），
    /// `"quit"` = 退出所有进程。非法值按 [`GuiPrefs::clamped`] 归一成 `"quit"`。
    ///
    /// 默认**退出**：常规软件的关闭就该是关掉，常驻后台是个明确的选择。
    /// 选了托盘之后，退出走托盘图标的右键菜单 —— 那条路**永远**在，
    /// 所以"最小化到托盘"不会把程序变成一个关不掉的幽灵。
    pub close_behavior: String,
}

impl Default for GuiPrefs {
    fn default() -> GuiPrefs {
        // 默认**问一次**：本工具的核心优势（MFT 直读 + USN 增量）全都要管理员权限，
        // 不主动问的话，大多数用户会一直跑在慢 1~2 个数量级的降级模式里，
        // 并且会以为"这软件就这么慢"。
        // 盘符栏默认**展开** —— 折叠是个明确的选择，不该替用户做。
        // 模式默认**标准**：轻量会看不细、性能会跑后台，两者都不该替用户选。
        GuiPrefs {
            ask_admin_on_start: true,
            drives_collapsed: false,
            alerts_collapsed: false,
            profile: crate::profile::Profile::Balanced.key().to_string(),
            periodic_scan: true,
            periodic_minutes: 60,
            close_behavior: CLOSE_QUIT.to_string(),
        }
    }
}

/// 关闭行为的两个取值。**一处定义**：后端判定、前端回填、守卫对账都用这两个字符串。
pub const CLOSE_QUIT: &str = "quit";
pub const CLOSE_TRAY: &str = "tray";

impl GuiPrefs {
    /// 把数值夹进合理区间。周期任务的间隔尤其要夹：
    /// `0` 会变成一个烧 CPU 的死循环，`u32::MAX` 则等于永远不跑 —— 两者
    /// 都不会报错，只是悄悄把机器拖慢 / 把功能变成摆设。
    pub fn clamped(&self) -> GuiPrefs {
        let mut p = self.clone();
        p.periodic_minutes = p.periodic_minutes.clamp(1, 24 * 60);
        // 未知取值一律当成"退出"：宁可退出，也不要把一个看不懂的值解释成
        // "最小化到托盘"，那样用户会以为程序坏了（点了 × 却没退出）。
        if p.close_behavior != CLOSE_TRAY {
            p.close_behavior = CLOSE_QUIT.to_string();
        }
        p
    }

    /// 点「×」时是否只藏到托盘。
    pub fn close_to_tray(&self) -> bool {
        self.close_behavior == CLOSE_TRAY
    }
}

/// 后台周期任务的间隔（毫秒）。**一处定义**：调度器与「下一次什么时候」的
/// 显示都用它，免得两边算出不同的时间。
pub fn periodic_ms(p: &GuiPrefs) -> u64 {
    p.periodic_minutes.clamp(1, 24 * 60) as u64 * 60_000
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
    // 读出来就夹紧：文件是可以被手改的，而"改出一个非法值"不该让界面显示
    // 出第三种状态。夹紧放在**读**这一侧，所有调用方（界面回填、调度器、
    // 关闭行为判定）拿到的都是同一份合法值。
    let p = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<GuiPrefs>(&s).unwrap_or_default(),
        Err(_) => GuiPrefs::default(),
    };
    p.clamped()
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

    #[test]
    fn default_asks_once() {
        assert!(
            GuiPrefs::default().ask_admin_on_start,
            "默认必须问一次 —— 不问的话用户会一直待在降级模式里"
        );
    }

    #[test]
    fn unknown_close_behavior_falls_back_to_quit() {
        let owned = core_io::test_dir::TestDir::new("prefs-close").unwrap();
        let p = owned.path().join("prefs.json");
        // 手改出来的非法值：必须被解释成"退出"，而不是"藏起来"
        std::fs::write(&p, r#"{"close_behavior":"minimize-to-nowhere"}"#).unwrap();
        let back = load_from(&p);
        assert_eq!(back.close_behavior, CLOSE_QUIT);
        assert!(!back.close_to_tray());

        let tray = GuiPrefs { close_behavior: CLOSE_TRAY.to_string(), ..GuiPrefs::default() };
        save_to(&p, &tray).unwrap();
        assert!(load_from(&p).close_to_tray());
        // 老配置（没有这个字段）必须按默认处理，而不是"什么都不是"
        std::fs::write(&p, r#"{"ask_admin_on_start":false}"#).unwrap();
        assert_eq!(load_from(&p).close_behavior, CLOSE_QUIT);
    }

    #[test]
    fn round_trip() {
        let owned = core_io::test_dir::TestDir::new("prefs-round").unwrap();
        let p = owned.path().join("prefs.json");
        let v = GuiPrefs { ask_admin_on_start: false, drives_collapsed: true, ..GuiPrefs::default() };
        save_to(&p, &v).expect("应能写入");
        assert_eq!(load_from(&p), v);
        // 再写回默认，确认不是单向的
        save_to(&p, &GuiPrefs::default()).expect("应能写入");
        let back = load_from(&p);
        assert!(back.ask_admin_on_start);
        assert!(!back.drives_collapsed);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let owned = core_io::test_dir::TestDir::new("prefs-missing").unwrap();
        let p = owned.path().join("prefs.json");
        assert_eq!(load_from(&p), GuiPrefs::default());
    }

    /// 损坏的配置**不能**让程序起不来，也不能让"不再询问"变成"永远询问"
    /// 之外的第三种状态 —— 退回默认（问一次）是唯一合理的解释。
    #[test]
    fn corrupt_file_falls_back_to_default() {
        for junk in ["{ not json", "", "[1,2,3]", r#"{"ask_admin_on_start": "yes"}"#] {
            let owned = core_io::test_dir::TestDir::new("prefs-corrupt").unwrap();
            let p = owned.path().join("prefs.json");
            std::fs::write(&p, junk).unwrap();
            assert_eq!(
                load_from(&p),
                GuiPrefs::default(),
                "损坏内容 {:?} 应退回默认值",
                junk
            );
        }
    }

    /// 缺字段按默认补（`#[serde(default)]`），而不是整个文件作废
    #[test]
    fn partial_json_falls_back() {
        let owned = core_io::test_dir::TestDir::new("prefs-partial").unwrap();
        let p = owned.path().join("prefs.json");
        std::fs::write(&p, "{}").unwrap();
        assert!(load_from(&p).ask_admin_on_start);
    }

    /// **升级路径**：老配置文件里没有 `drives_collapsed`，读出来必须是
    /// 「新字段取默认、老字段原样保留」。做不到的话，用户每升级一次就要重新
    /// 点一遍「不再自动询问权限」—— 而那恰恰是他上次特意关掉的东西。
    #[test]
    fn older_file_without_new_fields_keeps_what_the_user_chose() {
        let owned = core_io::test_dir::TestDir::new("prefs-older").unwrap();
        let p = owned.path().join("prefs.json");
        std::fs::write(&p, r#"{"ask_admin_on_start": false}"#).unwrap();
        let got = load_from(&p);
        assert!(!got.ask_admin_on_start, "老字段必须原样读回，不能被默认值顶掉");
        assert!(!got.drives_collapsed, "缺失的新字段按默认（展开）");
    }

    /// 盘符栏折叠是纯界面开关，但它是**用户做过的选择**：要能落盘往返，
    /// 而且和权限那一项互不干扰（两者共用一个 JSON 文件）。
    #[test]
    fn drives_collapsed_round_trip() {
        let owned = core_io::test_dir::TestDir::new("prefs-collapse").unwrap();
        let p = owned.path().join("prefs.json");
        save_to(&p, &GuiPrefs { ask_admin_on_start: false, drives_collapsed: true, ..GuiPrefs::default() })
            .expect("应能写入");
        let got = load_from(&p);
        assert!(
            got.drives_collapsed && !got.ask_admin_on_start,
            "折叠状态与权限偏好必须互不干扰地存下来，实际 {got:?}"
        );
    }

    /// 预警区折叠同样是用户做过的选择，且必须与盘符折叠互不影响
    /// （两个折叠开关各自独立，不能一个覆盖另一个）。
    #[test]
    fn alerts_collapsed_round_trip() {
        let owned = core_io::test_dir::TestDir::new("prefs-alerts").unwrap();
        let p = owned.path().join("prefs.json");
        save_to(&p, &GuiPrefs {
            alerts_collapsed: true,
            drives_collapsed: false,
            ..GuiPrefs::default()
        }).expect("应能写入");
        let got = load_from(&p);
        assert!(got.alerts_collapsed, "预警折叠必须存下来");
        assert!(!got.drives_collapsed, "盘符折叠不该被带着一起变");
    }

    /// 运行模式是**用户做过的选择**，要能落盘往返。
    #[test]
    fn profile_round_trip() {
        let owned = core_io::test_dir::TestDir::new("prefs-profile").unwrap();
        let p = owned.path().join("prefs.json");
        save_to(&p, &GuiPrefs { profile: "lean".to_string(), ..GuiPrefs::default() })
            .expect("应能写入");
        assert_eq!(load_from(&p).profile, "lean");

        save_to(&p, &GuiPrefs { profile: "performance".to_string(), periodic_minutes: 15, ..GuiPrefs::default() })
            .expect("应能写入");
        let got = load_from(&p);
        assert_eq!(got.profile, "performance");
        assert_eq!(got.periodic_minutes, 15);
    }

    /// **升级路径**：老配置文件里没有 `profile` / `periodic_*`。
    #[test]
    fn older_file_without_profile_keeps_what_the_user_chose() {
        let owned = core_io::test_dir::TestDir::new("prefs-noprofile").unwrap();
        let p = owned.path().join("prefs.json");
        std::fs::write(&p, r#"{"ask_admin_on_start": false, "drives_collapsed": true}"#).unwrap();
        let got = load_from(&p);
        assert!(!got.ask_admin_on_start, "老字段必须原样读回");
        assert!(got.drives_collapsed, "老字段必须原样读回");
        assert!(!got.alerts_collapsed, "缺失的新字段按默认（预警区展开）");
        assert_eq!(got.profile, "balanced", "缺失的新字段按默认（标准模式）");
        assert!(got.periodic_scan, "缺失的新字段按默认（周期扫描开）");
        assert_eq!(got.periodic_minutes, 60);
    }

    /// 周期间隔必须夹紧：`0` 是烧 CPU 的死循环，`u32::MAX` 等于永远不跑，
    /// 两者都不报错，只是悄悄把机器拖慢 / 把功能变成摆设。
    #[test]
    fn periodic_interval_is_clamped() {
        let tiny = GuiPrefs { periodic_minutes: 0, ..GuiPrefs::default() };
        assert!(periodic_ms(&tiny) >= 60_000, "下限 1 分钟");
        let huge = GuiPrefs { periodic_minutes: u32::MAX, ..GuiPrefs::default() };
        assert!(periodic_ms(&huge) <= 24 * 60 * 60_000, "上限 1 天");
        let ok = GuiPrefs { periodic_minutes: 30, ..GuiPrefs::default() };
        assert_eq!(periodic_ms(&ok), 30 * 60_000);
        assert_eq!(GuiPrefs { periodic_minutes: 0, ..GuiPrefs::default() }.clamped().periodic_minutes, 1);
    }

    /// 模式默认是标准：轻量会看不细、性能会跑后台，都不该替用户选。
    #[test]
    fn default_profile_is_balanced() {
        assert_eq!(GuiPrefs::default().profile, "balanced");
    }

    #[test]
    fn prefs_path_is_under_appdata() {
        let s = prefs_path().to_string_lossy().to_string();
        assert!(s.ends_with("FileWatch\\gui-prefs.json") || s.ends_with("FileWatch/gui-prefs.json"),
            "偏好文件应落在 FileWatch 子目录里，实际 {s}");
    }
}
