//! 降级模式的增量来源：把**文件系统通知**翻译成统一的 [`FsEvent`]。
//!
//! # 两条后端，一条通道
//!
//! ```text
//! Windows: ReadDirectoryChangesW → FILE_NOTIFY_INFORMATION 链 ─┐
//!                                                              ├→ FsEvent → watcher.rs
//! Linux:   inotify               → struct inotify_event      ─┘
//! ```
//!
//! 上层（[`crate::watcher`]）只认 [`FsEvent`]：`action` + 相对路径。
//! 两个后端的差异全部在这里吃掉：
//!
//! | 维度 | ReadDirectoryChangesW | inotify |
//! |---|---|---|
//! | 递归 | 一个句柄覆盖整棵子树 | **每个目录一个 watch** |
//! | 新增子目录 | 自动覆盖 | **必须自己补 watch** |
//! | 数量上限 | 无（受内存） | `max_user_watches`（默认 8192~65536） |
//! | 溢出语义 | 成功但 0 字节 = 丢事件 | `IN_Q_OVERFLOW` |
//! | 改名配对 | 两条相邻记录（旧名/新名） | `cookie` 相同的一对 |
//!
//! 「每个目录一个 watch」是 Linux 上最容易漏的一条：目录被创建/移动进来时
//! 必须立刻补 watch，否则那棵子树后续的变更**一条都收不到**，
//! 而且不报错 —— 界面看上去"监控正常运行"，实际上那张图再也不动了。
//!
//! # 为什么是 `FsEvent` 而不是直接用内核结构
//!
//! 上层要的是「这棵子树里谁动了、怎么动的」。内核结构（变长记录链 /
//! 定长头 + 变长名字）是**后端细节**，让 `watcher.rs` 认识两种结构，
//! 等于把两个平台的差异扩散到整个事件翻译层。
//!
//! # 停止语义（两边一致）
//!
//! * `stop()` 可以被重复调用；
//! * 停止时先置停止位，再打醒阻塞的读，再 `join`，最后关句柄/fd；
//! * **正常停止恒让 `exit_error()==0`** —— "被取消"不是错误。
//!   非 0 表示事件流真的断了（缓冲区溢出、句柄被外部关闭），
//!   此时增量不可信，上层必须提示用户重新扫描。

use core_io::consts::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ACTION_REMOVED,
};

/// 单条目录变更通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    /// `FILE_ACTION_ADDED` / `REMOVED` / `MODIFIED` / `RENAMED_OLD_NAME` / `RENAMED_NEW_NAME`
    /// （取值与 Win32 一致，Linux 的 inotify 掩码在 `dirwatch/linux.rs` 里映射成这套）
    pub action: u32,
    /// 相对被监视根的路径（Windows `sub\a.txt` / Linux `sub/a.txt`）
    pub rel: String,
}

impl FsEvent {
    pub fn label_cn(&self) -> &'static str {
        match self.action {
            FILE_ACTION_ADDED => "新增",
            FILE_ACTION_REMOVED => "删除",
            FILE_ACTION_MODIFIED => "修改",
            FILE_ACTION_RENAMED_OLD_NAME => "改名（旧）",
            FILE_ACTION_RENAMED_NEW_NAME => "改名（新）",
            _ => "未知",
        }
    }

    pub fn is_dir_change(&self) -> bool {
        matches!(self.action, FILE_ACTION_ADDED | FILE_ACTION_REMOVED)
    }
}

/// 归一化监视根：**卷根保留尾分隔符**（`C:\` / `/`），其余一律去掉（`C:\sub`）。
///
/// 统一成这一种形态，后续 `根 + 分隔符 + 相对路径` 的拼接才不会出现 `C:\\a`
/// 或 `//a`。
///
/// 注意 `CreateFileW` / `inotify_add_watch` 本身两种形态都接受，
/// 这里归一化纯粹是为了后续拼接少判断一次 —— 所以卷根**保留**尾分隔符，
/// 而不是像 [`crate::watcher::parent_key`] 那样去掉。
/// 实现见 [`core_io::paths::normalize_root`]（与 `Tree::path_of` 同一口径）。
pub use core_io::paths::normalize_root;

/// `inotify` 事件的纯数据解析（掩码映射 / 名字解码 / 相对路径）。
///
/// **两个平台都编译、都有单测**：它只处理字节，不碰系统调用。
/// 运行时的使用者只有 Linux 后端，所以 Windows 上允许 dead_code ——
/// 这样改这段逻辑时在主力开发环境（Windows）`cargo test` 就有反馈，
/// 不必等下一次开 WSL。详见该文件头部说明。
#[allow(dead_code)]
pub(crate) mod inotify_parse;

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{parse_notifications, DirWatcher};

#[cfg(not(windows))]
mod linux;

#[cfg(not(windows))]
pub use linux::DirWatcher;

#[cfg(test)]
mod tests {
    use super::*;

    /// 动作码必须与 Win32 一致：`watcher.rs` 用它们做 `match`，
    /// Linux 的映射也以这几个数为目标。
    #[test]
    fn action_codes_match_win32() {
        assert_eq!(FILE_ACTION_ADDED, 1);
        assert_eq!(FILE_ACTION_REMOVED, 2);
        assert_eq!(FILE_ACTION_MODIFIED, 3);
        assert_eq!(FILE_ACTION_RENAMED_OLD_NAME, 4);
        assert_eq!(FILE_ACTION_RENAMED_NEW_NAME, 5);
    }

    #[test]
    fn labels_are_human_readable() {
        let ev = |a| FsEvent { action: a, rel: "x".into() };
        assert_eq!(ev(FILE_ACTION_ADDED).label_cn(), "新增");
        assert_eq!(ev(FILE_ACTION_RENAMED_OLD_NAME).label_cn(), "改名（旧）");
        assert_eq!(ev(999).label_cn(), "未知");
        assert!(ev(FILE_ACTION_ADDED).is_dir_change());
        assert!(!ev(FILE_ACTION_MODIFIED).is_dir_change());
    }

    /// 归一化：卷根保留尾分隔符，普通目录去掉，空串退回根。
    #[test]
    fn normalize_root_rules() {
        let r = core_io::paths::root_prefix(core_io::paths::DEFAULT_VOLUME);
        assert_eq!(normalize_root(&r), r);
        let sub = core_io::paths::join(&r, "sub");
        assert_eq!(normalize_root(&format!("{sub}{}", core_io::paths::SEP)), sub);
        assert_eq!(normalize_root(&sub), sub);
        assert_eq!(normalize_root(""), core_io::paths::SEP_STR.to_string());
    }
}
