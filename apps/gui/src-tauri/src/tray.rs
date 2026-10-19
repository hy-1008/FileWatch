//! 系统托盘：图标、右键菜单、「点 × 之后去哪」，以及**阈值预警的提醒**。
//!
//! # 为什么要有托盘
//!
//! 这个程序的主要用途之一是**长时间挂着的监控**：盯着一个目录，等它涨到
//! 阈值再报。这种用法下「关掉窗口 = 关掉监控」是错的 —— 用户想的是
//! "先把窗口收起来，别占任务栏"，不是"别监控了"。
//!
//! 所以「关闭软件」有两种：最小化到托盘（监控继续跑）与关闭所有进程。
//!
//! # 关机路径只有一条，而且永远能走
//!
//! 选了托盘之后，退出的入口是托盘图标的右键菜单 —— 它**不受**偏好影响，
//! 一直都在。少了这条，"最小化到托盘"就会把程序变成一个关不掉的幽灵：
//! 窗口没了、进程还在、用户找不到它在哪。这一点比"记住用户的选择"更重要。
//!
//! # 预警要"人不在跟前也看得见"
//!
//! 监控的意义是"我不盯着的时候它替我盯"。所以阈值命中时做三件事：
//!
//! 1. **系统通知**（Windows 通知中心）—— 窗口收在托盘里时唯一能的提示；
//! 2. **托盘图标闪动** —— 通知被系统静音/错过时的第二道提示。做法是交替
//!    设置一个"带红点"的图标与正常图标（不改工具提示文字，免得刷屏）；
//! 3. **把最近几条预警写进托盘右键菜单** —— 用户右键就能看到"刚才报了什么"，
//!    点一下打开窗口并切到监控页。不打开窗口也能知道大概。
//!
//! 三件事都失败也不能影响监控本身：通知被系统策略拦下、图标被资源管理器
//! 重启丢掉，都只是"没提醒到"，不该让监控线程崩掉。所以这里的每一步都是
//! `let _ =`。

use crate::prefs;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};

const MAIN_WINDOW: &str = "main";
/// 托盘菜单里最多列几条最近的预警。
const RECENT_MAX: usize = 5;
/// 菜单里每条预警最多显示多少字符（菜单项宽度有限，长路径会把它撑爆）。
const MENU_TEXT_MAX: usize = 38;
const NOTIFY_TITLE: &str = "FileWatch 阈值预警";
/// 闪动节奏：亮 320 ms、灭 320 ms，来回 6 次。
const FLASH_STEP: std::time::Duration = std::time::Duration::from_millis(320);
const FLASH_TIMES: u32 = 6;

/// 托盘的可变部分。`TrayIcon` 本身是句柄（内部 Arc），存起来才能在运行时
/// 换图标、换菜单；两张图片预先算好，闪动时不需要重新画。
pub struct TrayState {
    icon: Mutex<Option<TrayIcon>>,
    normal: Mutex<Option<Image<'static>>>,
    alarm: Mutex<Option<Image<'static>>>,
    /// 最近的预警文本（**新的在前**）。菜单与 `tray:alert` 事件都从这里取。
    recent: Mutex<VecDeque<String>>,
    /// 一次只允许一个闪动线程，否则连着来两条预警会互相抢图标，
    /// 最后停在告警图标上（看起来像"报警没停"）。
    flashing: AtomicBool,
}

impl Default for TrayState {
    fn default() -> TrayState {
        TrayState {
            icon: Mutex::new(None),
            normal: Mutex::new(None),
            alarm: Mutex::new(None),
            recent: Mutex::new(VecDeque::new()),
            flashing: AtomicBool::new(false),
        }
    }
}

impl TrayState {
    pub fn recent_alerts(&self) -> Vec<String> {
        self.recent.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default()
    }
}

/// 把主窗口重新调出来：显示 → 取消最小化 → 抢焦点。
///
/// 三步缺一不可：只 `show()` 的话，之前被最小化过的窗口仍然是最小化的，
/// 出来的还是一个看不见的窗口；不给焦点的话，窗口会在别的窗口后面，
/// 用户会以为"点了没反应"。
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 菜单文本：单行、超长中间省略、去掉换行。
///
/// 托盘菜单不是列表，行宽有限；一条"路径 + 消息"动辄几万字，原样塞进去
/// 会把菜单撑到屏幕外，用户连「退出」都找不到。
fn menu_text(text: &str) -> String {
    let flat = text.replace(['\r', '\n', '\t'], " ");
    let trimmed = flat.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= MENU_TEXT_MAX {
        return trimmed.to_string();
    }
    let head = MENU_TEXT_MAX * 2 / 3;
    let tail = MENU_TEXT_MAX - head - 1;
    let mut s: String = chars[..head].iter().collect();
    s.push('…');
    s.extend(&chars[chars.len() - tail..]);
    s
}

/// 在正常图标右上角画一个红点，得到"告警版"图标。
///
/// 不改整张图（不调色、不缩放）：托盘图标在 16~32 px 下细节本来就少，
/// 加一个高对比红点是最容易被一眼认出来的改动。
fn alarm_variant(base: &Image<'_>) -> Image<'static> {
    let (w, h) = (base.width(), base.height());
    let mut rgba = base.rgba().to_vec();
    let radius = (w.min(h) / 3).max(3) as i32;
    let cx = w as i32 - radius - 1;
    let cy = radius + 1;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            if i + 3 >= rgba.len() {
                continue;
            }
            // 红点带一圈深色描边：浅色托盘背景上也能看清
            let edge = dx * dx + dy * dy > (radius - 1) * (radius - 1);
            if edge {
                rgba[i] = 0x7f;
                rgba[i + 1] = 0x1d;
                rgba[i + 2] = 0x1d;
            } else {
                rgba[i] = 0xE5;
                rgba[i + 1] = 0x3E;
                rgba[i + 2] = 0x3E;
            }
            rgba[i + 3] = 0xFF;
        }
    }
    Image::new_owned(rgba, w, h)
}

/// 建菜单。`recent` 非空时在中间列出最近几条预警（点击都是"打开并看监控"）。
fn menu_for(app: &AppHandle, recent: &[String]) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "open", "打开 FileWatch", true, None::<&str>)?;
    let monitor = MenuItem::with_id(app, "monitor", "监控事件…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 FileWatch", true, None::<&str>)?;
    let mut builder = MenuBuilder::new(app).item(&open).item(&monitor);
    if !recent.is_empty() {
        builder = builder.separator();
        // 固定 id：菜单每次重建，用 String 生成的 id 会一路泄漏（MenuId 是 'static 语义）
        for (slot, text) in recent.iter().take(3).enumerate() {
            let id = match slot {
                0 => "alert0",
                1 => "alert1",
                _ => "alert2",
            };
            let item = MenuItem::with_id(app, id, menu_text(text), true, None::<&str>)?;
            builder = builder.item(&item);
        }
    }
    builder.separator().item(&quit).build()
}

/// 按「最近预警」重建托盘菜单。没有 state（托盘没建起来）时什么都不做。
fn rebuild_menu(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else { return };
    let recent = state.recent_alerts();
    let app2 = app.clone();
    // 菜单属于 UI 线程的对象：统一派发回主线程，避免在监控线程里直接动它
    let _ = app.run_on_main_thread(move || {
        let Some(state) = app2.try_state::<TrayState>() else { return };
        let Ok(menu) = menu_for(&app2, &recent) else { return };
        let tray = state.icon.lock().ok().and_then(|g| g.clone());
        drop(state);
        if let Some(tray) = tray {
            let _ = tray.set_menu(Some(menu));
        }
    });
}

/// 交替设置告警图标与正常图标。已经在闪就跳过（不排队）。
pub fn flash(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else { return };
    if state.flashing.swap(true, Ordering::SeqCst) {
        return;
    }
    let normal = state.normal.lock().ok().and_then(|g| g.clone());
    let alarm = state.alarm.lock().ok().and_then(|g| g.clone());
    let Some(normal) = normal else {
        state.flashing.store(false, Ordering::SeqCst);
        return;
    };
    let app2 = app.clone();
    std::thread::spawn(move || {
        for step in 0..FLASH_TIMES * 2 {
            let image = if step % 2 == 0 { alarm.clone() } else { Some(normal.clone()) };
            let app3 = app2.clone();
            let _ = app2.run_on_main_thread(move || {
                let Some(state) = app3.try_state::<TrayState>() else { return };
                let tray = state.icon.lock().ok().and_then(|g| g.clone());
                drop(state);
                if let Some(tray) = tray {
                    let _ = tray.set_icon(image);
                }
            });
            std::thread::sleep(FLASH_STEP);
        }
        // 收尾一定是"正常图标"：闪动线程被打断时也不会留下一个红点
        let app4 = app2.clone();
        let _ = app2.run_on_main_thread(move || {
            let Some(state) = app4.try_state::<TrayState>() else { return };
            let normal = state.normal.lock().ok().and_then(|g| g.clone());
            let tray = state.icon.lock().ok().and_then(|g| g.clone());
            drop(state);
            if let (Some(tray), Some(normal)) = (tray, normal) {
                let _ = tray.set_icon(Some(normal));
            }
        });
        if let Some(state) = app2.try_state::<TrayState>() {
            state.flashing.store(false, Ordering::SeqCst);
        }
    });
}

/// 阈值预警的统一出口：记一条 → 重建菜单 → 闪图标 → 系统通知 → 通知界面。
///
/// 顺序有讲究：先记、再闪、最后发系统通知。通知走的是系统服务，偶尔会慢
/// （几百毫秒），先做内存里的那几件事，用户至少立刻看到菜单变了、图标在闪。
pub fn notify_alert(app: &AppHandle, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<TrayState>() {
        if let Ok(mut recent) = state.recent.lock() {
            recent.push_front(text.to_string());
            while recent.len() > RECENT_MAX {
                recent.pop_back();
            }
        }
    }
    rebuild_menu(app);
    flash(app);
    // 系统通知：被策略拦下/系统关掉了通知都只是"没提醒到"，不影响监控
    {
        use tauri_plugin_notification::NotificationExt;
        let _ = app.notification().builder().title(NOTIFY_TITLE).body(text).show();
    }
    // 窗口在前台时，界面自己也会提示（后端已经有一条 toast，这条给"托盘菜单
    // 点进来的"用：先打开窗口，再由前端决定怎么高亮）
    let _ = app.emit("tray:alert", text.to_string());
}

/// 最近几条预警（新的在前）。诊断入口 `--selftest-alert` 用它把结果写出去，
/// 这样"预警到底进没进托盘菜单"可以被断言，而不是靠肉眼看任务栏。
pub fn recent_alerts(app: &AppHandle) -> Vec<String> {
    app.try_state::<TrayState>().map(|s| s.recent_alerts()).unwrap_or_default()
}

/// 建托盘图标。**任何一步失败都不让程序起不来**：托盘是个便利，
/// 没有它程序照样能用；因为图标加载失败就白屏，是最坏的一种失败方式。
pub fn build(app: &AppHandle) -> Result<(), String> {
    app.manage(TrayState::default());

    let menu = menu_for(app, &[]).map_err(|e| format!("建托盘菜单失败：{e}"))?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("FileWatch")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            // 「监控事件…」与最近的预警条目：都是"打开窗口，然后看监控页"
            "monitor" | "alert0" | "alert1" | "alert2" => {
                show_window(app);
                let _ = app.emit("tray:open-monitor", ());
            }
            // 走 `app.exit(0)`（不是 `window.close()`）：前者会触发
            // `RunEvent::ExitRequested`，监控线程的收尾（保存 USN 游标）在那条路上。
            "quit" => app.exit(0),
            _ => {}
        });

    // 图标取窗口图标：不重复引一份图片资源，也就不会出现
    // "托盘的图标和窗口的图标不是同一个"这种小问题。
    if let Some(icon) = app.default_window_icon() {
        // 窗口图标是借来的（生命周期挂在 app 上），拷成 owned 再存：
        // 托盘状态要活在 AppHandle 里，不能带一个"指向 app 内部"的借用。
        let normal = Image::new_owned(icon.rgba().to_vec(), icon.width(), icon.height());
        let alarm = alarm_variant(&normal);
        builder = builder.icon(normal.clone());
        if let Some(state) = app.try_state::<TrayState>() {
            if let Ok(mut g) = state.normal.lock() {
                *g = Some(normal);
            }
            if let Ok(mut g) = state.alarm.lock() {
                *g = Some(alarm);
            }
        }
    }

    let tray = builder
        .on_tray_icon_event(|tray, event| {
            // 左键单击 = 打开窗口（托盘上的惯例）；右键交给菜单自己弹。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        })
        .build(app)
        .map_err(|e| format!("建托盘图标失败：{e}"))?;

    if let Some(state) = app.try_state::<TrayState>() {
        if let Ok(mut g) = state.icon.lock() {
            *g = Some(tray.clone());
        }
    }
    drop(tray);
    Ok(())
}

/// 点窗口右上角「×」时该做什么。
///
/// 判定在后端、读的是**落盘的偏好**：前端此刻可能正在忙（扫描中、事件洪水），
/// 而"关一下窗口"这种事不该因为界面忙就变成关不掉或关错。
pub fn handle_close_requested(app: &AppHandle) -> bool {
    if prefs::load().close_to_tray() {
        if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
            let _ = window.hide();
            return true; // 已接管：调用方要 prevent_close()
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_text_is_one_short_line() {
        let long = format!("C:\\{}\n第二行", "很长的目录".repeat(20));
        let t = menu_text(&long);
        assert!(t.chars().count() <= MENU_TEXT_MAX, "{}", t.chars().count());
        assert!(!t.contains('\n'), "菜单项不能带换行");
        assert!(t.contains('…'));
        assert_eq!(menu_text(" 短消息 "), "短消息");
    }

    #[test]
    fn alarm_icon_puts_a_red_dot_in_the_corner() {
        // 16×16 的透明底图：只验证"右上角出现红点、左下角没被动过"
        let base = Image::new_owned(vec![0u8; 16 * 16 * 4], 16, 16);
        let alarm = alarm_variant(&base);
        assert_eq!((alarm.width(), alarm.height()), (16, 16));
        let px = |img: &Image<'_>, x: u32, y: u32| {
            let i = ((y * img.width() + x) * 4) as usize;
            let d = img.rgba();
            (d[i], d[i + 1], d[i + 2], d[i + 3])
        };
        let (r, g, b, a) = px(&alarm, 12, 4);
        assert!(r > 150 && g < 90 && b < 90 && a == 255, "右上角应是红点：{r},{g},{b},{a}");
        assert_eq!(px(&alarm, 1, 1), (0, 0, 0, 0), "其余像素不该被改动");
        // 原图不能被改（我们复制了缓冲区）
        assert_eq!(px(&base, 12, 4), (0, 0, 0, 0));
    }
}
