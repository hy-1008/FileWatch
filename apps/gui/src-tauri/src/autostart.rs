//! 开机自启动：往 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 写一行。
//!
//! # 为什么用 HKCU 而不是计划任务 / 服务
//!
//! 开机自启动只有「用户登录后自动跑起来」这一个诉求，HKCU 的 `Run` 键就是为
//! 它设计的：不需要管理员权限、不需要额外进程、用户自己也能在任务管理器里
//! 关掉。计划任务和服务都是**系统级**改动，为了一个"开机起来"去动它们不划算，
//! 而且会给"这只是个绿色小工具"这件事添一个说不清的尾巴。
//!
//! # 为什么手写 FFI 而不引第三方库
//!
//! 这里只用到五个 Win32 函数，手写 `extern "system"` 就够了 —— 为一个
//! 「写一行注册表」拉进一整个 `windows` crate 的构建时间，不值得。
//! `advapi32` 是 MinGW/GNU 工具链默认链接的系统库之一，不需要额外链接参数。
//!
//! # 路径会失效，这一点要如实说
//!
//! 写进去的是**绝对路径**。便携包被挪到别处（或者解压目录被删）之后，这一行
//! 就指向一个不存在的文件 —— Windows 会静默跳过它，用户只会觉得"自启动没生效"。
//! 所以 [`status`] 会把「已启用但指向的文件不存在/不是本程序」单独报成 `stale`，
//! 界面上提示重新开关一次，而不是让它一直挂着。

use serde::Serialize;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const HKEY_CURRENT_USER: usize = 0x8000_0001;
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// 登录即运行（用户级，不需要管理员）。
pub const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// 注册表里的值名。改它等于放弃旧版本留下来的那一行，别随手改。
pub const VALUE_NAME: &str = "FileWatch";

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        key: usize,
        sub_key: *const u16,
        options: u32,
        desired: u32,
        result: *mut usize,
    ) -> i32;
    fn RegSetValueExW(
        key: usize,
        value_name: *const u16,
        reserved: u32,
        ty: u32,
        data: *const u16,
        bytes: u32,
    ) -> i32;
    fn RegQueryValueExW(
        key: usize,
        value_name: *const u16,
        reserved: *mut u32,
        ty: *mut u32,
        data: *mut u16,
        bytes: *mut u32,
    ) -> i32;
    fn RegDeleteValueW(key: usize, value_name: *const u16) -> i32;
    fn RegCloseKey(key: usize) -> i32;
}

/// 自启动现状。
#[derive(Debug, Clone, Serialize)]
pub struct AutoStartInfo {
    /// 注册表里有没有这一行。
    pub enabled: bool,
    /// 注册表里那一行的内容（没有则空串）。
    pub command: String,
    /// 本程序自身的路径。
    pub exe: String,
    /// 写了，但指向的不是**现在这个**程序（便携包挪过位置）。
    /// Windows 遇到这种条目会静默跳过，界面必须把它说出来。
    pub stale: bool,
    /// 一句人话说明（给状态栏/toast 用）。
    pub note: String,
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 写进注册表的命令行。**路径必须带引号**：不带的话
/// `C:\Program Files\X\app.exe` 会被拆成两个参数，登录时启动失败且不报错。
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// 从命令行反解出程序路径。
///
/// 按 Windows 自己的解析规则来：**程序路径到第一个空白为止**（除非用引号
/// 括起来）。所以 `C:\a\fw.exe --min` 的程序是 `C:\a\fw.exe`，参数不算在里面。
/// 我们写进去的值总是带引号，但值可能是别人写的（用户手改的、另一个程序
/// 留下的），那种形态也得认得出来 —— 否则"是不是指向当前程序"就永远判成
/// 不一样，界面会一直显示"路径已失效"。
pub fn exe_from_command(cmd: &str) -> PathBuf {
    let t = cmd.trim();
    let body = if t.starts_with('"') {
        match t[1..].find('"') {
            Some(end) => return PathBuf::from(&t[1..1 + end]),
            // 只有一个引号（坏值）：去掉引号再按无引号处理，别整串当成路径
            None => t.trim_matches('"'),
        }
    } else {
        t
    };
    PathBuf::from(body.split_whitespace().next().unwrap_or(""))
}

fn open_key(write: bool) -> Result<usize, String> {
    let mut key: usize = 0;
    let desired = if write { KEY_QUERY_VALUE | KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    let rc = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, desired, &mut key)
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("打不开自启动注册表键（错误 {rc}）"));
    }
    Ok(key)
}

fn read_command() -> Option<String> {
    let key = open_key(false).ok()?;
    let name = wide(VALUE_NAME);
    let mut ty: u32 = 0;
    let mut bytes: u32 = 0;
    // 先问长度（传空缓冲），再按长度取一次
    let rc = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut bytes)
    };
    if rc != ERROR_SUCCESS || bytes == 0 {
        unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize / 2) + 2];
    let rc = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr(), &mut bytes)
    };
    unsafe { RegCloseKey(key) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    Some(from_wide(&buf))
}

/// 现状。**任何异常都当作"没启用"** —— 一个读不出来的注册表项不该让界面起不来，
/// 也不该假装"已经开了"。
pub fn status() -> AutoStartInfo {
    let exe = std::env::current_exe().unwrap_or_default();
    let command = read_command().unwrap_or_default();
    let enabled = !command.is_empty();
    // 写了、但指向的不是现在这个程序：大概率是便携包挪过位置
    let stale = enabled && !same_path(&exe_from_command(&command), &exe);
    let note = if !enabled {
        String::new()
    } else if stale {
        String::from("已写进自启动，但指向的路径不是当前这个程序（便携包挪过位置？）—— 关掉再开一次即可修正")
    } else {
        String::from("登录时自动启动")
    };
    AutoStartInfo {
        enabled,
        command,
        exe: exe.to_string_lossy().to_string(),
        stale,
        note,
    }
}

/// 路径比较：Windows 上大小写不等价、分隔符可能是 `/`，所以先归一再比。
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    !a.as_os_str().is_empty() && norm(a) == norm(b)
}

pub fn enable() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("取不到程序路径：{e}"))?;
    let cmd = command_for(&exe);
    let key = open_key(true)?;
    let name = wide(VALUE_NAME);
    let data = wide(&cmd);
    // REG_SZ 的数据要含结尾的 NUL；长度按**字节**算
    let bytes = (data.len() * 2) as u32;
    let rc = unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr(), bytes) };
    unsafe { RegCloseKey(key) };
    if rc != ERROR_SUCCESS {
        return Err(format!("写入自启动失败（错误 {rc}）"));
    }
    Ok(())
}

/// 关掉。**幂等**：本来就没写也算成功 —— 用户连点两下不该看到报错。
pub fn disable() -> Result<(), String> {
    let key = open_key(true)?;
    let name = wide(VALUE_NAME);
    let rc = unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { RegCloseKey(key) };
    if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
        return Err(format!("删除自启动项失败（错误 {rc}）"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径带空格是常态（`C:\Program Files\...`），不带引号会被拆成两个参数，
    /// 登录时启动失败且不报任何错 —— 这条就是钉这个的。
    #[test]
    fn command_is_quoted() {
        let c = command_for(Path::new(r"C:\Program Files\FileWatch\FileWatch.exe"));
        assert!(c.starts_with('"') && c.ends_with('"'), "命令行必须整体带引号，实际 {c}");
        assert_eq!(exe_from_command(&c), PathBuf::from(r"C:\Program Files\FileWatch\FileWatch.exe"));
    }

    #[test]
    fn command_round_trips() {
        for p in [
            r"C:\a\FileWatch.exe",
            r"D:\my tools\fw\FileWatch.exe",
            r"\\?\C:\a\FileWatch.exe",
        ] {
            assert_eq!(exe_from_command(&command_for(Path::new(p))), PathBuf::from(p), "{p} 往返失败");
        }
    }

    #[test]
    fn unquoted_command_still_parses() {
        assert_eq!(exe_from_command(r"C:\a\fw.exe"), PathBuf::from(r"C:\a\fw.exe"));
        // 带参数的命令行：只取程序那一段
        assert_eq!(
            exe_from_command(r"C:\a\fw.exe --min"),
            PathBuf::from(r"C:\a\fw.exe")
        );
    }

    #[test]
    fn path_compare_ignores_separator_and_case() {
        assert!(same_path(Path::new(r"C:/a/fw.exe"), Path::new(r"C:\A\FW.EXE")));
        assert!(!same_path(Path::new(r"C:\a\fw.exe"), Path::new(r"D:\a\fw.exe")));
        assert!(!same_path(Path::new(""), Path::new("")));
    }

    /// 真机往返：写入 → 读到「已启用」→ 删除 → 读到「未启用」。
    /// 标 `#[ignore]`：它会动用户注册表（虽然是 HKCU、且测试完自己清理），
    /// 不该在每次 `cargo test` 时发生。跑法：
    /// `cargo test --release -- --ignored autostart`
    #[test]
    #[ignore]
    fn enable_disable_round_trip_on_real_registry() {
        // 先记下原本有没有，测完恢复原状 —— 测试不该改变用户的设置
        let before = status();
        enable().expect("应能写入 HKCU Run");
        let on = status();
        assert!(on.enabled, "写完之后必须是已启用");
        assert!(!on.stale, "刚写进去的路径就是当前程序，不该判成失效");
        assert_eq!(exe_from_command(&on.command), exe_from_command(&command_for(&std::env::current_exe().unwrap())));

        disable().expect("应能删除");
        let off = status();
        assert!(!off.enabled, "删除之后必须是未启用");

        // 幂等：再删一次不该报错
        disable().expect("重复删除应视为成功");

        if before.enabled {
            // 用户原本开着的：把原来那一行写回去
            let key = open_key(true).unwrap();
            let name = wide(VALUE_NAME);
            let data = wide(&before.command);
            let rc = unsafe {
                RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr(), (data.len() * 2) as u32)
            };
            unsafe { RegCloseKey(key) };
            assert_eq!(rc, ERROR_SUCCESS, "还原用户原本的自启动项失败");
        }
    }
}
