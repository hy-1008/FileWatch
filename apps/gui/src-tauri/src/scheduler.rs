//! 后台周期任务（**只在性能模式下跑**）。
//!
//! # 干什么
//!
//! 每隔 N 分钟做两件事：
//!
//! 1. 重新读一遍所有盘符的容量，推 `volumes:tick` —— 便宜（几次
//!    `GetDiskFreeSpaceEx`）、没有副作用，界面上的「可用 / 总空间」因此一直是活的；
//! 2. 若开着「轮流扫描」，扫一块盘，让面积图的数据保持新鲜。
//!
//! # 为什么不是「轮流把每个盘都扫一遍」
//!
//! 一次扫描会**替换**掉进程里那份唯一的扫描结果（`AppState::loaded` 只有一个槽位）。
//! 所以"后台扫 D 盘"的代价是：用户正在看的 C 盘视图被换成 D 盘。那是
//! 「程序偷偷把人正在看的东西换掉」，比"数据旧一点"糟得多。
//!
//! 于是规则是：[`next_scan_target`] ——
//! * 用户正在看某块盘 → 刷新**那一块**（同一块盘，视图原地更新，不打扰）；
//! * 还没有任何扫描结果（没东西可被换掉）→ 才按轮转挑一块盘预取。
//!
//! 各盘符的"覆盖"由第 1 步保证：容量是每轮都全量刷新的。
//!
//! # 为什么不 sleep 整个周期
//!
//! 线程每秒醒一次看停止标志：退出时必须能在 1 秒内收尾（`main.rs` 里等 1.2 s），
//! 否则关窗口会卡住；而一睡 60 分钟的线程是叫不醒的。

use crate::commands::{list_volumes, spawn_scan};
use crate::prefs::{self, GuiPrefs};
use crate::profile::Profile;
use crate::state::Shared;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 醒来的间隔。纯粹是为了能及时看到停止标志（`main.rs` 退出时只等 1.2 s）。
const WAKE: Duration = Duration::from_secs(1);

/// 一个正在跑的周期任务。三个开关都可以在跑着的时候改（用户改偏好不用重启）。
pub struct Scheduler {
    stop: Arc<AtomicBool>,
    /// 周期（分钟）
    minutes: Arc<AtomicU32>,
    /// 是否轮流扫描
    scan: Arc<AtomicBool>,
}

impl Scheduler {
    /// 请求停止。**不等待**：退出路径上由 `main.rs` 统一等一小会儿。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 按新偏好调整。周期或开关变了不用重启线程。
    pub fn apply(&self, p: &GuiPrefs) {
        self.minutes.store(p.periodic_minutes.clamp(1, 24 * 60), Ordering::SeqCst);
        self.scan.store(p.periodic_scan, Ordering::SeqCst);
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 盘符的比较键：`C:\` 与 `C:` 是同一块盘，`D:\projects` 也算 D 盘。
/// 取不到盘符（UNC、卷 GUID）返回 `None` —— 那种目标不参与轮转。
pub fn drive_key(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(format!("{}:", b[0].to_ascii_uppercase() as char))
    } else {
        None
    }
}

/// 下一轮该扫哪块盘。**纯函数**，好测。
///
/// * `view` —— 用户当前正在看的盘（没有则 `None`）
/// * `volumes` —— 现有盘符列表
/// * `cursor` —— 轮转游标
///
/// 返回 `(盘符, 新游标)`。
pub fn next_scan_target(
    view: Option<&str>,
    volumes: &[String],
    cursor: usize,
) -> Option<(String, usize)> {
    if volumes.is_empty() {
        return None;
    }
    // 用户正在看某块盘 → 只刷它。扫别的盘会把他的视图换掉（见模块文档）。
    if let Some(v) = view.and_then(drive_key) {
        if let Some(hit) = volumes.iter().find(|d| drive_key(d).as_deref() == Some(v.as_str())) {
            return Some((hit.clone(), cursor));
        }
        // 视图所在的盘已经不在列表里（U 盘拔了）→ 退回轮转
    }
    let i = cursor % volumes.len();
    Some((volumes[i].clone(), (i + 1) % volumes.len()))
}

/// 起一个周期任务。**只有性能模式该调它**：轻量模式的全部意义就是不跑后台。
pub fn start(app: AppHandle, p: &GuiPrefs) -> Scheduler {
    let minutes = Arc::new(AtomicU32::new(p.periodic_minutes.clamp(1, 24 * 60)));
    let scan = Arc::new(AtomicBool::new(p.periodic_scan));
    let stop = Arc::new(AtomicBool::new(false));

    let th_stop = stop.clone();
    let th_minutes = minutes.clone();
    let th_scan = scan.clone();
    thread::spawn(move || {
        let mut next = Instant::now() + Duration::from_millis(prefs::periodic_ms(&GuiPrefs {
            periodic_minutes: th_minutes.load(Ordering::SeqCst),
            ..GuiPrefs::default()
        }));
        let mut cursor = 0usize;
        while !th_stop.load(Ordering::SeqCst) {
            if Instant::now() >= next {
                let mins = th_minutes.load(Ordering::SeqCst);
                next = Instant::now() + Duration::from_millis(prefs::periodic_ms(&GuiPrefs {
                    periodic_minutes: mins,
                    ..GuiPrefs::default()
                }));

                // 1) 容量：每轮都全量刷新，便宜且无副作用
                let cards = list_volumes();
                let drives: Vec<String> = cards.iter().map(|c| c.drive.clone()).collect();
                let _ = app.emit("volumes:tick", cards);

                // 2) 扫描：只在开着的时候做，且只扫"不会换掉用户视图"的那块盘
                if th_scan.load(Ordering::SeqCst) {
                    let view = app
                        .state::<Shared>()
                        .inner()
                        .lock()
                        .ok()
                        .and_then(|st| st.last_drive.clone());
                    if let Some((d, next_cursor)) = next_scan_target(view.as_deref(), &drives, cursor)
                    {
                        cursor = next_cursor;
                        // 失败是可以接受的：正在扫描 / 正在监控 / 盘被拔了，
                        // 下一轮再来就是。后台任务不该弹错误给用户。
                        let _ = spawn_scan(&app, &d, None, false);
                    }
                }
            }
            thread::sleep(WAKE);
        }
    });

    Scheduler { stop, minutes, scan }
}

/// 当前偏好该不该跑后台任务。**一处判定**：启动与用户改偏好都用它，
/// 免得两处对"性能模式"的理解分叉。
pub fn should_run(p: &GuiPrefs) -> bool {
    Profile::parse(Some(&p.profile)).background()
}

/// 进程里那份周期任务的槽位（最多一个）。放进 `tauri::manage` 供命令取用。
///
/// 用 `Option` 而不是"永远在跑、内部自己判断"：轻量模式下**连线程都不该有** ——
/// 那是"轻量"这个词唯一能兑现的部分。
#[derive(Default)]
pub struct SchedulerSlot(pub Mutex<Option<Scheduler>>);

impl SchedulerSlot {
    /// 把后台任务对齐到给定偏好：该跑就跑（已在跑就改参数）、不该跑就停。
    pub fn sync(&self, app: &AppHandle, p: &GuiPrefs) {
        let mut guard = match self.0.lock() {
            Ok(g) => g,
            // 锁被污染说明上一次 panic 过：这种时候什么也不做，比继续折腾好
            Err(e) => e.into_inner(),
        };
        if should_run(p) {
            match guard.as_mut() {
                Some(s) => s.apply(p),
                None => *guard = Some(start(app.clone(), p)),
            }
        } else if let Some(s) = guard.take() {
            s.stop();
        }
    }

    /// 退出时停掉。不等线程真的结束 —— `main.rs` 那边统一等一小会儿。
    pub fn stop_all(&self) {
        if let Ok(mut g) = self.0.lock() {
            if let Some(s) = g.take() {
                s.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_volumes_means_no_target() {
        assert!(next_scan_target(None, &[], 0).is_none());
        assert!(next_scan_target(Some("C:"), &[], 3).is_none());
    }

    /// 用户正在看某块盘时，**绝不**去扫别的盘 —— 那会把他的视图换掉。
    #[test]
    fn refreshes_the_disk_the_user_is_looking_at() {
        let vols = v(&["C:", "D:", "E:"]);
        for cursor in 0..6 {
            let (got, next) = next_scan_target(Some("D:\\projects"), &vols, cursor).unwrap();
            assert_eq!(got, "D:", "游标 {cursor} 下也应刷用户正在看的那块盘");
            assert_eq!(next, cursor, "刷新当前盘不该推动轮转游标");
        }
    }

    #[test]
    fn drive_letter_forms_are_equivalent() {
        let vols = v(&["C:\\", "D:"]);
        // 视图给的是 `D:\x`，盘符表里是 `D:` —— 归一后要认出来
        assert_eq!(next_scan_target(Some("D:\\x"), &vols, 0).unwrap().0, "D:");
        // 大小写不等价是 Windows 的常态
        assert_eq!(next_scan_target(Some("c:"), &vols, 0).unwrap().0, "C:\\");
    }

    /// 视图所在盘不在列表里（拔掉的 U 盘）→ 退回轮转，而不是什么都不做
    #[test]
    fn falls_back_to_rotation_when_view_disk_is_gone() {
        let vols = v(&["C:", "D:"]);
        let (got, next) = next_scan_target(Some("F:"), &vols, 1).unwrap();
        assert_eq!(got, "D:");
        assert_eq!(next, 0, "游标应推进，下一轮换下一块盘");
    }

    #[test]
    fn rotation_visits_every_disk() {
        let vols = v(&["C:", "D:", "E:"]);
        let mut cursor = 0;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (d, c) = next_scan_target(None, &vols, cursor).unwrap();
            seen.push(d);
            cursor = c;
        }
        assert_eq!(seen, vec!["C:", "D:", "E:"]);
        // 再一圈从头开始
        assert_eq!(next_scan_target(None, &vols, cursor).unwrap().0, "C:");
    }

    #[test]
    fn drive_key_normalizes() {
        assert_eq!(drive_key("C:").as_deref(), Some("C:"));
        assert_eq!(drive_key("c:\\").as_deref(), Some("C:"));
        assert_eq!(drive_key("D:\\a\\b").as_deref(), Some("D:"));
        // UNC / 卷 GUID / 相对路径都不是盘符，不参与轮转
        assert_eq!(drive_key(r"\\server\share"), None);
        assert_eq!(drive_key(r"\\?\Volume{123}"), None);
        assert_eq!(drive_key("relative/path"), None);
    }

    /// 只有性能模式跑后台 —— 轻量模式的全部意义就是不跑。
    #[test]
    fn only_performance_runs_background() {
        let mut p = GuiPrefs::default();
        assert!(!should_run(&p), "标准模式不跑后台");
        p.profile = "lean".to_string();
        assert!(!should_run(&p), "轻量模式绝不跑后台");
        p.profile = "performance".to_string();
        assert!(should_run(&p), "性能模式才跑后台");
        p.profile = "乱写的".to_string();
        assert!(!should_run(&p), "不认识的模式按标准处理");
    }
}
