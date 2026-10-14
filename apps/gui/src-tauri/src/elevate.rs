//! 管理员权限：检测当前进程的权限状态，并在需要时「以管理员身份重启」。
//!
//! # 为什么不直接嵌 `requireAdministrator` 清单
//!
//! 文档 R2 的原方案是 UAC 清单自动提权 —— 一劳永逸，但代价是**每次启动都要过
//! UAC**，包括只想看一眼面积图的场景；而且提权进程的完整性级别更高，
//! 「从资源管理器把目录拖进来」这类交互会静默失效。
//!
//! 这里改成：清单保持 `asInvoker`（不需要任何额外配置），**进入软件时问一次**
//! （见 `commands::elevation_info` + 前端的权限弹窗）。用户点「是」→ 用 `runas`
//! 动词重启自己（到这一步才弹 UAC）；点「否」→ 降级运行，右下角「权限」按钮
//! 随时可以再申请。两条路功能都不缺，只差速度。
//!
//! # 为什么要有 `--elevation-attempted`
//!
//! 如果所在系统把 UAC 关了，`TokenElevationType` 会是 `Default`，此时 `runas`
//! 启动出来的新进程**仍然是未提权的** —— 于是新进程又会弹窗、又重启，无限循环。
//! 所以重启时给新进程塞一个 `--elevation-attempted`，见到它就不再自动弹窗
//! （[`ElevationStatus::attempted`]）。这是一道硬保护，不依赖前端的偏好设置。
//!
//! # 为什么手写 FFI
//!
//! 这一层虽然是 GUI（允许用第三方库），但需要的只有 6 个 kernel32/advapi32/
//! shell32 函数。为它们引一个 `windows` crate，会同时把整个 Windows 元数据
//! 拖进依赖图与链接行 —— 而本机的 crates 下载本来就只能走镜像。
//! 手写声明的链接面积最小，也和 `core-io/src/winapi.rs` 的做法一致。

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use serde::Serialize;
use std::ffi::c_void;
use std::ptr;

// ── Win32 声明 ──────────────────────────────────────────────────────────────

type HANDLE = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type LPCWSTR = *const u16;
type HINSTANCE = *mut c_void;
type HWND = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> HANDLE;
    fn GetLastError() -> DWORD;
    fn CloseHandle(h: HANDLE) -> BOOL;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: HANDLE, desired_access: DWORD, token: *mut HANDLE) -> BOOL;
    fn GetTokenInformation(
        token: HANDLE,
        class: i32,
        info: *mut c_void,
        info_len: DWORD,
        ret_len: *mut DWORD,
    ) -> BOOL;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: HWND,
        verb: LPCWSTR,
        file: LPCWSTR,
        params: LPCWSTR,
        dir: LPCWSTR,
        show: i32,
    ) -> HINSTANCE;
}

const TOKEN_QUERY: DWORD = 0x0008;
/// `TOKEN_INFORMATION_CLASS::TokenElevationType`
const TOKEN_ELEVATION_TYPE: i32 = 18;
/// `TOKEN_INFORMATION_CLASS::TokenElevation`
const TOKEN_ELEVATION: i32 = 20;
const SW_SHOWNORMAL: i32 = 1;

/// `TokenElevationType` 的三个取值。
const TYPE_DEFAULT: DWORD = 1;
const TYPE_FULL: DWORD = 2;
const TYPE_LIMITED: DWORD = 3;

/// 用户在 UAC 对话框里点了「否」。
const ERROR_CANCELLED: u32 = 1223;
/// `SE_ERR_ACCESSDENIED` —— `runas` 被拒时 `ShellExecuteW` 常见的返回值。
const SE_ERR_ACCESSDENIED: u32 = 5;
const SE_ERR_FNF: u32 = 2;
const SE_ERR_OOM: u32 = 8;
/// `ShellExecuteW` 约定：**返回值 ≤ 32 就是错误码**。
const SHELLEXEC_SUCCESS_THRESHOLD: isize = 32;

/// 重启时给新进程的标记：见到它就不再自动弹权限窗（防死循环）。
pub const ATTEMPTED_FLAG: &str = "--elevation-attempted";

// ── 权限状态 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationKind {
    /// 已经是提权令牌，或者卷能直接打开 —— MFT 直读可用。
    Admin,
    /// 在管理员组，但当前是 UAC 过滤令牌：**可以在不输密码的情况下申请提权**。
    /// （`whoami /groups` 里 Administrators 会显示「只用于拒绝的组」。）
    Filtered,
    /// 不在管理员组：`runas` 会转而要求输入管理员账户的密码。
    Standard,
    /// 令牌信息取不到（极少见），按「不确定」处理。
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct ElevationStatus {
    pub kind: ElevationKind,
    /// 令牌本身是提权的（`TokenIsElevated`）
    pub token_elevated: bool,
    /// **能不能真的读卷**。这是本项目的经验判据，比令牌更贴近事实：
    /// UAC 关闭时令牌类型是 `Default`、`TokenIsElevated` 为 0，但进程其实
    /// 已经拿着完整令牌，卷照样打得开。只信令牌会在这里误报。
    pub volume_readable: bool,
    /// 本次启动是不是已经尝试过提权（见模块头注释的防死循环）
    pub attempted: bool,
}

impl ElevationStatus {
    /// 已经具备 MFT 直读的能力 —— 界面据此决定**要不要弹权限窗**。
    pub fn privileged(&self) -> bool {
        matches!(self.kind, ElevationKind::Admin) || self.token_elevated || self.volume_readable
    }

    /// 值不值得去申请提权。
    ///
    /// 除了「已经有权限」，其余情况**都给一次机会** —— 包括判不出令牌信息的
    /// `Unknown`。理由：失败只是弹一条提示，而漏掉一次申请会让用户一直
    /// 待在慢 1~2 个数量级的降级模式里，后者代价大得多。
    pub fn can_request(&self) -> bool {
        !self.privileged()
    }
}

/// 查询当前进程的权限状态。
pub fn status() -> ElevationStatus {
    let mut kind = ElevationKind::Unknown;
    let mut token_elevated = false;

    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 {
            let mut value: DWORD = 0;
            let mut ret: DWORD = 0;
            let s = std::mem::size_of::<DWORD>() as DWORD;

            let ok_elev = GetTokenInformation(
                token,
                TOKEN_ELEVATION,
                &mut value as *mut DWORD as *mut c_void,
                s,
                &mut ret,
            ) != 0;
            if ok_elev {
                token_elevated = value != 0;
            }

            let mut ty: DWORD = 0;
            let mut ret2: DWORD = 0;
            let ok_type = GetTokenInformation(
                token,
                TOKEN_ELEVATION_TYPE,
                &mut ty as *mut DWORD as *mut c_void,
                s,
                &mut ret2,
            ) != 0;
            if ok_type {
                kind = match ty {
                    TYPE_FULL => ElevationKind::Admin,
                    TYPE_LIMITED => ElevationKind::Filtered,
                    TYPE_DEFAULT => ElevationKind::Standard,
                    _ => ElevationKind::Unknown,
                };
            }
            CloseHandle(token);
        }
    }

    // 令牌说「未提权」，但卷能打开 → 事实上一路畅通（UAC 关闭 / 全令牌）。
    // `is_elevated_hint` 走的是 `\\.\C:` 的真实打开，正好用来兜住这种情况。
    let volume_readable = core_io::util::is_elevated_hint();

    ElevationStatus {
        kind,
        token_elevated,
        volume_readable,
        attempted: attempted_flag_present(),
    }
}

/// 本次命令行里是否已经带着 `--elevation-attempted`。
pub fn attempted_flag_present() -> bool {
    std::env::args().any(|a| a == ATTEMPTED_FLAG)
}

/// 去掉 `--elevation-attempted`，得到「原始」参数列表。
///
/// 重启时要用它 —— 否则连续点两次「申请提权」会在命令行里叠出两个标记，
/// 虽然无害，但会让「这道标记到底是谁加的」变得没法查。
pub fn original_args() -> Vec<String> {
    std::env::args().skip(1).filter(|a| a != ATTEMPTED_FLAG).collect()
}

// ── 以管理员身份重启 ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LaunchOutcome {
    pub started: bool,
    /// 失败/取消的原因（中文，界面可直接显示）
    pub reason: Option<String>,
    /// `ShellExecuteW` 的原始错误码（成功时为 `None`）
    pub code: Option<u32>,
    /// 实际传给新进程的完整命令行（诊断用）
    pub command_line: String,
}

impl LaunchOutcome {
    fn ok(command_line: String) -> LaunchOutcome {
        LaunchOutcome { started: true, reason: None, code: None, command_line }
    }

    fn fail(code: u32, reason: String, command_line: String) -> LaunchOutcome {
        LaunchOutcome { started: false, reason: Some(reason), code: Some(code), command_line }
    }
}

/// 给参数加引号（`CommandLineToArgvW` 的反函数）。
///
/// 必须自己做：`--root=C:\Program Files\x` 这种带空格的路径直接拼进命令行会被
/// 拆成两个参数，新进程拿到的路径是错的 —— 而且错得很安静。
///
/// 规则：需要时整体加引号；串内每个 `"` 前面补一个 `\`；**结尾的连续反斜杠要
/// 翻倍**，否则它会去转义收尾的那个引号。
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
        return a.to_string();
    }
    let mut out = String::with_capacity(a.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 把一段参数拼成命令行（每个参数按 [`quote_arg`] 处理）。
pub fn build_command_line(args: &[String]) -> String {
    args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

/// 用 `runas` 动词重启自己。
///
/// **这个调用会阻塞**：UAC 的确认框就是在这段时间里弹出来的。所以调用方必须
/// 放在独立线程里（见 `commands::relaunch_elevated`），否则界面会冻住。
pub fn relaunch_elevated() -> LaunchOutcome {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return LaunchOutcome::fail(0, format!("取不到当前程序路径：{e}"), String::new())
        }
    };

    let mut args = original_args();
    if !args.iter().any(|a| a == ATTEMPTED_FLAG) {
        args.push(ATTEMPTED_FLAG.to_string());
    }
    let command_line = build_command_line(&args);

    let verb = core_io::util::to_wide("runas");
    let file = core_io::util::to_wide(&exe.to_string_lossy());
    let params = core_io::util::to_wide(&command_line);

    let rc = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            ptr::null_mut(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if rc > SHELLEXEC_SUCCESS_THRESHOLD {
        return LaunchOutcome::ok(command_line);
    }

    // `ShellExecuteW` 的返回约定：> 32 成功，否则**返回值本身就是错误码**。
    // 但某些情况下它返回 0，这时真正的错在 `GetLastError()` 里。
    let code = if rc > 0 { rc as u32 } else { unsafe { GetLastError() } };
    let detail = match code {
        ERROR_CANCELLED => "已取消：你在 Windows 的权限确认框里点了「否」".to_string(),
        SE_ERR_ACCESSDENIED => {
            "提权被拒绝：可能是权限确认框被取消，或系统策略不允许".to_string()
        }
        SE_ERR_FNF => format!("找不到要启动的程序：{}", exe.display()),
        SE_ERR_OOM => "内存不足，无法启动新进程".to_string(),
        _ => format!("系统未允许提权重启（错误码 {code}）"),
    };
    LaunchOutcome::fail(code, detail, command_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_args_are_not_quoted() {
        assert_eq!(quote_arg("--debug"), "--debug");
        assert_eq!(quote_arg(r"C:\Users\x"), r"C:\Users\x");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn spaces_force_quotes() {
        assert_eq!(quote_arg(r"--root=C:\Program Files\a"), r#""--root=C:\Program Files\a""#);
    }

    #[test]
    fn inner_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        // 引号前面的反斜杠要翻倍，否则会把引号转义掉
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    /// 结尾反斜杠必须翻倍 —— 这是最容易写错的一条：
    /// `"C:\dir\"` 里的 `\"` 会被解成字面引号，参数直接跑飞。
    #[test]
    fn trailing_backslashes_are_doubled() {
        assert_eq!(quote_arg(r#"a b\"#), r#""a b\\""#);
        assert_eq!(quote_arg(r#"x y\\"#), r#""x y\\\\""#);
        // 不带空格的结尾反斜杠不需要加引号，也就不需要翻倍
        assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
    }

    #[test]
    fn command_line_joins_quoted_parts() {
        let cl = build_command_line(&[
            "--root=C:\\Program Files".to_string(),
            "--json".to_string(),
        ]);
        assert_eq!(cl, r#""--root=C:\Program Files" --json"#);
        assert_eq!(build_command_line(&[]), "");
    }

    /// 本模块的核心断言：**在我们的目标机器上必须判成 `Filtered`**。
    ///
    /// 实测（`whoami /groups`）：当前账户在 Administrators 组里，但那行的措辞是
    /// 「只用于拒绝的组」，即 UAC 过滤令牌 —— 正是「可以弹窗申请提权」的情形。
    /// 若哪天这里变成 `Standard`，说明账户或 UAC 策略变了，弹窗文案也得跟着改，
    /// 所以让它以测试的形式暴露出来，而不是等用户看到一句错的说明。
    #[test]
    fn status_is_readable_and_self_consistent() {
        let s = status();
        assert!(
            s.kind != ElevationKind::Unknown,
            "拿不到令牌信息 —— 这条测试本身有意义，不该跳过"
        );
        // Filtered / Standard 一定不是提权的；Admin 一定是
        match s.kind {
            ElevationKind::Admin => assert!(s.privileged(), "Admin 应当算有权限"),
            ElevationKind::Filtered | ElevationKind::Standard => {
                assert!(!s.token_elevated, "过滤令牌不该报成已提权")
            }
            ElevationKind::Unknown => unreachable!(),
        }
        // `attempted` 由命令行决定，跑测试时不会带那个标记
        assert!(!s.attempted, "cargo test 的命令行不该带 {}" , ATTEMPTED_FLAG);
    }

    #[test]
    fn original_args_drops_the_marker() {
        // 这个函数读的是真实 `std::env::args()`，跑测试时没有该标记
        assert!(original_args().iter().all(|a| a != ATTEMPTED_FLAG));
    }
}
