//! 合成路径的拼接，以及「USN 事件 → 人看得懂的路径」。
//!
//! # 为什么值得单独一个模块
//!
//! 这条链路已经出过两次事故，两次都是「屏幕对、数据错」：
//!
//! 1. 变更流水落盘时写的是 `Tree::path_of` 拼出来的**合成路径**
//!    （`--root=D:\projects` 时会写成 `C:\incoming\a.bin`，磁盘上并不存在）；
//! 2. 预警行把已经是真实的路径又套了一遍 [`PathRemap`]，
//!    变成 `C:\a\proj\a\proj`。
//!
//! 根因一样：**同一件事（把事件说成一条人看得懂的路径）在两个地方各写了一遍**。
//! 所以这里给出唯一的实现，CLI 与界面都取它，显示与落盘也共用它。
//!
//! 关键约定（与 `core-index` 一致）：`Tree::path_of` 的前缀恒为**盘符**
//! （`C:\`），而且盘根保留尾反斜杠 —— 所以拼子项时不能再补一个 `\`。

use crate::session::Session;
use core_index::{recno_of, Tree};
use core_monitor::{PathRemap, PendingEvent};

/// 目录 + 名字 → 路径。`C:\` + `a.txt` = `C:\a.txt`（**不是** `C:\\a.txt`）。
///
/// 看起来琐碎，但它是「盘根一律保留尾反斜杠」这条约定的唯一落点：
/// 少了它，根目录下的每个文件都会多一个反斜杠。
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// 事件所属的**合成**路径（前缀是盘符，尚未重映射）。
///
/// 单独暴露是为了测试与诊断：想知道「重映射之前长什么样」时要用它。
pub fn raw_event_path(tree: &Tree, ev: &PendingEvent) -> String {
    let parent = tree.path_of(recno_of(ev.parent_frn));
    join_path(&parent, &ev.name)
}

/// 事件最终给人看的路径。
///
/// `remap` 只在「扫描根是某个子目录」时非空 —— 那时 `Tree` 里的前缀是盘符，
/// 需要换回用户真正指定的那个目录。
pub fn event_path(tree: &Tree, ev: &PendingEvent, remap: Option<&PathRemap>) -> String {
    let raw = raw_event_path(tree, ev);
    match remap {
        Some(r) => r.apply(&raw),
        None => raw,
    }
}

impl Session {
    /// 本次会话的重映射。整卷扫描时是 `None`（前缀已经是真实的）。
    ///
    /// 循环里要处理成批事件时，**先取一次**再逐条用 [`event_path`] ——
    /// 每条都重新构造一遍会把字符串分配做成平方级。
    pub fn path_remap(&self) -> Option<PathRemap> {
        let root = match self.cfg.scan_root.as_ref() {
            Some(p) => p.to_string_lossy().to_string(),
            // 整卷扫描：合成前缀就是真实前缀，不需要替换
            None => return None,
        };
        PathRemap::new(&self.cfg.drive, &root)
    }

    /// 单条事件的显示路径（便捷版，内部等价于 [`Session::path_remap`] + [`event_path`]）。
    pub fn event_path(&self, ev: &PendingEvent) -> String {
        let remap = self.path_remap();
        event_path(&self.tree, ev, remap.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
    use core_monitor::debounce::PendingEvent;
    use core_usn::normalize::EventKind;

    fn ev(parent_frn: u64, name: &str, is_dir: bool) -> PendingEvent {
        PendingEvent {
            frn: 100,
            parent_frn,
            kind: EventKind::Created,
            name: name.to_string(),
            old_name: None,
            is_dir,
            first_ms: 1,
            last_ms: 2,
            merged: 1,
            usn: 0,
            reason: 0,
            size_delta: 0,
            alloc_delta: 0,
            new_size: 0,
            new_alloc: 0,
        }
    }

    /// 盘根（记录 5，名字是 `.`）+ 一个 `logs` 子目录。
    fn tree() -> Tree {
        let mut t = Tree::new("C:", 16);
        t.arena.ensure(11);
        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size: 0,
                alloc: 0,
                mtime: 0,
                name: off,
            };
        };
        set(&mut t, 5, 5, ".", true);
        set(&mut t, 10, 5, "logs", true);
        t.root = 5;
        t
    }

    #[test]
    fn join_path_handles_root_and_subdir() {
        assert_eq!(join_path(r"C:\", "a.txt"), r"C:\a.txt");
        assert_eq!(join_path(r"C:\docs", "a.txt"), r"C:\docs\a.txt");
        assert_eq!(join_path("", "a.txt"), "a.txt");
        // 盘根绝不能拼出双反斜杠 —— 设计规则里点名的一条
        assert!(!join_path(r"C:\", "x").contains(r"\\"));
    }

    /// 事件挂在**盘根**下时不许多出一个反斜杠。
    /// 这条曾经真的出错：`path_of` 返回 `C:\`，再补一个 `\` 就成了
    /// `C:\\created.dat` —— 屏幕上只是难看，但这条路径还要落进 JSONL 与 CSV。
    #[test]
    fn root_children_do_not_double_the_backslash() {
        let t = tree();
        // parent_frn 的低 48 位是记录号；高 16 位序列号必须被掩掉
        let p = raw_event_path(&t, &ev((5u64 << 48) | 5, "created.dat", false));
        assert_eq!(p, r"C:\created.dat");
    }

    #[test]
    fn subdir_children_are_joined_once() {
        let t = tree();
        let p = raw_event_path(&t, &ev(10, "app.log", false));
        assert_eq!(p, r"C:\logs\app.log");
    }

    /// 扫描根是深目录时，前缀换回真实根；**只换一次**。
    #[test]
    fn remap_rewrites_the_drive_prefix() {
        let t = tree();
        let r = PathRemap::new("C:", r"D:\projects").expect("应建立重映射");
        let p = event_path(&t, &ev(10, "app.log", false), Some(&r));
        assert_eq!(p, r"D:\projects\logs\app.log");
        // 再套一次不应改变任何东西（幂等）—— `Alert.path` 那次事故就是漏了这条
        assert_eq!(r.apply(&p), p);
    }

    /// 整卷扫描（没有 scan_root）时不做任何替换：前缀本来就是真实的。
    #[test]
    fn no_scan_root_means_no_remap() {
        let t = tree();
        let p = event_path(&t, &ev(10, "app.log", false), None);
        assert_eq!(p, r"C:\logs\app.log");
    }
}
