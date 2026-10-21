//! `inotify` 事件的**纯数据**解析：不碰任何系统调用，两个平台都编译、都有单测。
//!
//! # 为什么单独拆出来
//!
//! `struct inotify_event` 的字节布局（定长头 + NUL 结尾的名字 + 4 字节对齐）
//! 是**协议**，不是系统调用。把它和 `inotify_init1` / `read` / `poll` 混在
//! 一起有个很实际的坏处：那些单元测试只能在 Linux 上跑，于是开发者
//! （主力环境是 Windows）改完这段代码**没有任何反馈**，只能等 CI 或等
//! 下一次开 WSL。
//!
//! 拆开之后，`Name` 解码、掩码 → 动作码的映射、相对路径拼接这三件最容易写错
//! 的事，在 Windows 上 `cargo test` 就能验。
//!
//! # 与 Windows 后端的对称关系
//!
//! | 这里 | Windows 那边 |
//! |---|---|
//! | [`for_each`]（变长记录链） | `parse_notifications`（`FILE_NOTIFY_INFORMATION` 链） |
//! | [`action_of`]（掩码 → 5 个动作码） | 内核直接给 `FILE_ACTION_*` |
//! | [`rel_of`]（绝对 → 相对根） | 内核直接给相对路径 |
//!
//! 也就是说 Linux 侧多做了两步翻译，而这两步都在这个文件里、都有测试。
//!
//! # 为什么允许 `dead_code`
//!
//! 运行时的使用者只有 `dirwatch/linux.rs`（`#[cfg(not(windows))]`），
//! 所以 Windows 构建下这些 `pub` 项没人调用。允许 dead_code 是为了让
//! **测试**在两个平台都能编译 —— 这比"只有 Linux 能验"值。

#![allow(dead_code)]

/// `struct inotify_event` 的固定头：`wd(4) + mask(4) + cookie(4) + len(4)`。
pub const EVENT_HEAD: usize = 16;

// ── 掩码（Linux 通用值） ────────────────────────────────────────────────────
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_UNMOUNT: u32 = 0x0000_2000;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ISDIR: u32 = 0x4000_0000;

/// 要监听的事件集合。
///
/// 带 `IN_CLOSE_WRITE` 是**故意的**：`IN_MODIFY` 在写入过程中会连发几十条，
/// 而 `IN_CLOSE_WRITE` 是一条"这次写完了"的确定信号。上层有 debounce，
/// 多一条不会重复计数，但少了它就可能对着半写状态的文件回读大小。
pub const WATCH_MASK: u32 =
    IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE
        | IN_DELETE_SELF
        | IN_MOVE_SELF;

/// 队列溢出用 `ENOSPC` 表示"内核侧事件丢了"，与 Windows 的
/// `ERROR_NOTIFY_ENUM_DIR`(1022) 同义：增量不可信，需要重新扫描。
pub const LOST_EVENTS_CODE: u64 = 28;

/// 从缓冲里解出来的一条事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub wd: i32,
    pub mask: u32,
    /// 文件名（不带目录）。目录自身的事件（`IN_DELETE_SELF` 等）是空串。
    ///
    /// 用 `String` 而不是 `&str`：Linux 的文件名是任意字节，非 UTF-8 时只能
    /// 走 lossy 转换（与 `Path::to_string_lossy` 同一口径）。整个引擎的名字
    /// 都是 UTF-8 字符串（`Tree` 的字符串池），所以这里**注定**要做一次转换，
    /// 不如在边界处做，别让非法字节流进上层。
    pub name: String,
}

/// 一批事件的遍历结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    /// 事件流已断（队列溢出 / 被卸载）：上层必须让 `exit_error()!=0`。
    Fatal,
}

/// 逐条解码 `struct inotify_event` 序列，对每条调用 `f`。
///
/// 防御要点：
/// * `len` 字段可能超出剩余缓冲（尾记录被截断）→ 直接停，**不猜**；
/// * 名字按第一个 NUL 截断（内核会按 4 字节对齐补齐，补齐字节是 0）；
/// * `len` 为 0 时名字为空串（`IN_DELETE_SELF` 这类针对目录自身的事件）。
pub fn for_each(buf: &[u8], mut f: impl FnMut(Event)) -> Outcome {
    let mut off = 0usize;
    while off + EVENT_HEAD <= buf.len() {
        let wd = i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let mask = u32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        // cookie 只用于给 MOVED_FROM/MOVED_TO 配对；上层用"旧名→新名"的顺序
        // 配对（与 Windows 的 RENAMED_OLD/NEW 一致），所以这里不往外传。
        let _cookie =
            u32::from_ne_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let len = u32::from_ne_bytes([
            buf[off + 12],
            buf[off + 13],
            buf[off + 14],
            buf[off + 15],
        ]) as usize;
        off += EVENT_HEAD;
        if off + len > buf.len() {
            break; // 截断的尾记录：丢掉，不 panic
        }
        let raw_name = &buf[off..off + len];
        off += len;

        if mask & IN_Q_OVERFLOW != 0 || mask & IN_UNMOUNT != 0 {
            return Outcome::Fatal;
        }
        f(Event {
            wd,
            mask,
            name: name_of(raw_name),
        });
    }
    Outcome::Continue
}

/// 掩码 → 统一动作码（与 Win32 的 `FILE_ACTION_*` 同值）。
///
/// **一条事件只映射成一个动作**，按"最具体优先"：`IN_CREATE|IN_ISDIR` 是新增，
/// 而不是"新增且修改"。
pub fn action_of(mask: u32) -> Option<u32> {
    use core_io::consts::*;
    if mask & IN_CREATE != 0 {
        return Some(FILE_ACTION_ADDED);
    }
    if mask & IN_DELETE != 0 {
        return Some(FILE_ACTION_REMOVED);
    }
    if mask & IN_MOVED_FROM != 0 {
        return Some(FILE_ACTION_RENAMED_OLD_NAME);
    }
    if mask & IN_MOVED_TO != 0 {
        return Some(FILE_ACTION_RENAMED_NEW_NAME);
    }
    if mask & IN_DELETE_SELF != 0 {
        return Some(FILE_ACTION_REMOVED);
    }
    if mask & IN_MOVE_SELF != 0 {
        return Some(FILE_ACTION_RENAMED_OLD_NAME);
    }
    if mask & (IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE) != 0 {
        return Some(FILE_ACTION_MODIFIED);
    }
    None
}

/// `inotify_event.name` 是 NUL 结尾、按 4 字节对齐补齐的字节串。
pub fn name_of(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// 绝对目录 → 相对监视根的路径（根自身返回空串）。
pub fn rel_of(root: &str, abs: &str) -> String {
    use core_io::paths::SEP;
    if abs == root {
        return String::new();
    }
    let r = root.trim_end_matches(SEP);
    if let Some(rest) = abs.strip_prefix(r) {
        return rest.trim_start_matches(SEP).to_string();
    }
    abs.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_io::consts::*;

    /// 手工拼一条 inotify 事件（头 + 名字 + 4 字节对齐填充）。
    pub fn encode(wd: i32, mask: u32, name: &str) -> Vec<u8> {
        let mut bytes = name.as_bytes().to_vec();
        bytes.push(0);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // cookie
        out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
        out.extend_from_slice(&bytes);
        out
    }

    #[test]
    fn mask_mapping_covers_every_action() {
        assert_eq!(action_of(IN_CREATE), Some(FILE_ACTION_ADDED));
        assert_eq!(action_of(IN_DELETE), Some(FILE_ACTION_REMOVED));
        assert_eq!(action_of(IN_MOVED_FROM), Some(FILE_ACTION_RENAMED_OLD_NAME));
        assert_eq!(action_of(IN_MOVED_TO), Some(FILE_ACTION_RENAMED_NEW_NAME));
        assert_eq!(action_of(IN_MODIFY), Some(FILE_ACTION_MODIFIED));
        assert_eq!(action_of(IN_CLOSE_WRITE), Some(FILE_ACTION_MODIFIED));
        assert_eq!(action_of(IN_ATTRIB), Some(FILE_ACTION_MODIFIED));
        assert_eq!(action_of(IN_DELETE_SELF), Some(FILE_ACTION_REMOVED));
        // 只有 IN_ISDIR 位时不该产生动作
        assert_eq!(action_of(IN_ISDIR), None);
    }

    /// 名字必须按长度截取、按 NUL 收尾 —— 中文名与"恰好 4 字节对齐"的名都要对。
    #[test]
    fn names_are_decoded_by_nul_not_by_length() {
        let buf = {
            let mut b = encode(9, IN_CREATE, "新建 文件夹");
            b.extend_from_slice(&encode(9, IN_MODIFY, "abcd")); // 名字刚好 5 字节 → 补到 8
            b
        };
        let mut got = Vec::new();
        let out = for_each(&buf, |e| got.push((e.mask, e.name)));
        assert_eq!(out, Outcome::Continue);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, "新建 文件夹");
        assert_eq!(got[1].1, "abcd");
    }

    /// 队列溢出必须是 `Fatal`，不能悄悄少报。
    #[test]
    fn overflow_is_fatal() {
        let buf = encode(-1, IN_Q_OVERFLOW, "");
        let mut n = 0;
        let out = for_each(&buf, |_| n += 1);
        assert_eq!(out, Outcome::Fatal);
        assert_eq!(n, 0);
    }

    /// 截断的尾记录：不 panic、不产出半条事件、前面的完整记录照常处理。
    #[test]
    fn truncated_tail_is_dropped() {
        let mut buf = encode(7, IN_CREATE, "ok.txt");
        let full = encode(7, IN_CREATE, "abc.txt");
        buf.extend_from_slice(&full[..full.len() - 4]);
        let mut got = Vec::new();
        let out = for_each(&buf, |e| got.push(e.name));
        assert_eq!(out, Outcome::Continue);
        assert_eq!(got, vec!["ok.txt".to_string()], "只有完整记录算数");
    }

    /// 相对路径必须用本平台分隔符拼，且根自身是空串。
    #[test]
    fn relative_paths_are_built_on_separator() {
        use core_io::paths::SEP_STR;
        let root = "/home/me/proj".to_string();
        assert_eq!(rel_of(&root, &root), "");
        let child = format!("{root}{SEP_STR}sub");
        assert_eq!(rel_of(&root, &child), "sub");
        // 根带尾分隔符时也必须给出干净结果
        let with_sep = format!("{root}{SEP_STR}");
        assert_eq!(rel_of(&with_sep, &child), "sub");
    }
}
