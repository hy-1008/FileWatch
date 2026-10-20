//! Explorer selection through PIDLs, plus shell-driven "open with" for files.
//!
//! 菜单里凡是**执行**一个文件的地方（v0.1.1 的「打开方式…」/「用默认程序打开」）
//! 都走外壳自己的动词，绝不拼命令行：路径里有空格、`&`、中文括号时，
//! 命令行版本要猜的引号规则比这里能写对的更多。
use serde::Serialize;
use std::{fs, path::{Path, PathBuf}};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Common::ITEMIDLIST, ILFree, OPENASINFO, OAIF_ALLOW_REGISTRATION, OAIF_EXEC,
    SHOpenFolderAndSelectItems, SHOpenWithDialog, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

#[derive(Serialize)]
pub struct RevealResult { path: String, exact: bool }

/// 「在文件资源管理器打开」的结果：`path` 是**被打开的那个文件夹**，
/// 不是用户右键的那个文件 —— 这一点很重要，见 [`explore_target`]。
#[derive(Serialize)]
pub struct ExploreResult { path: String, exact: bool }

fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") { format!(r"\\{rest}") }
    else { s.strip_prefix(r"\\?\").unwrap_or(&s).to_string() }
}

fn resolve(path: &str) -> Result<RevealResult, String> {
    let input = path.trim();
    let normalized = display_path(Path::new(input));
    if input.contains('\0') || !Path::new(&normalized).is_absolute()
        || normalized.starts_with(r"\\.\") || normalized.starts_with(r"\\?\") {
        return Err("只能定位完整的文件或文件夹路径".into());
    }
    let mut candidate = PathBuf::from(normalized);
    let mut exact = true;
    loop {
        match fs::metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_file() && !metadata.is_dir() { return Err("目标不是普通文件或文件夹".into()); }
                let real = fs::canonicalize(&candidate).map_err(|e| e.to_string())?;
                return Ok(RevealResult { path: display_path(&real), exact });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                candidate = candidate.parent().ok_or("目标及上级目录均不存在")?.to_path_buf();
                exact = false;
            }
            Err(e) => return Err(format!("无法访问目标：{e}")),
        }
    }
}

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { CoUninitialize() }; } }
struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl { fn drop(&mut self) { unsafe { ILFree(Some(self.0)) }; } }

unsafe fn parse(path: &str) -> Result<Pidl, String> {
    let mut id = std::ptr::null_mut();
    SHParseDisplayName(&HSTRING::from(path), None, &mut id, 0, None).map_err(|e| format!("无法解析资源管理器路径：{e}"))?;
    if id.is_null() { return Err("资源管理器返回了空路径".into()); }
    Ok(Pidl(id))
}

/// 决定「在文件资源管理器打开」要打开**哪个文件夹**。
///
/// 传进来的是文件时就打开它所在的目录：把文件原样交给 `ShellExecuteW("open")`
/// 等于**用默认程序打开这个文件**（.exe 就直接跑起来了），那不是这个菜单项
/// 的意思 —— 这个按钮叫"在文件资源管理器打开"，它只负责打开一个文件夹窗口。
fn explore_target(path: &str) -> Result<ExploreResult, String> {
    let result = resolve(path)?;
    let is_dir = fs::metadata(&result.path).map(|m| m.is_dir()).unwrap_or(false);
    let dir = if is_dir {
        result.path.clone()
    } else {
        Path::new(&result.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| format!("「{}」没有所在目录，无法打开", result.path))?
    };
    Ok(ExploreResult { path: dir, exact: result.exact })
}

/// `ShellExecuteW("open")`：把目标交给外壳的"打开"动词，与在资源管理器里
/// 双击它是同一条路。返回值 > 32 表示成功；否则是一个**没有可读文本**的错误码
/// （`SE_ERR_*`），所以只能把码和动作名一起带回去。
fn shell_open(target: &str, action: &str) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| e.to_string())?;
        let _apartment = Apartment;
        let wide = HSTRING::from(target);
        let r = ShellExecuteW(None, windows::core::w!("open"), &wide, None, None, SW_SHOWNORMAL);
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(format!("{action}失败（代码 {}）", r.0 as isize))
        }
    }
}

/// 打开一个文件夹窗口。命令行的 `explorer <path>` 由外壳去猜参数，
/// 路径里有空格或 `&` 时它猜错的样子非常难查。
fn open_folder(dir: &str) -> Result<(), String> {
    shell_open(dir, "打开资源管理器")
}

fn explore(path: &str) -> Result<ExploreResult, String> {
    let target = explore_target(path)?;
    open_folder(&target.path)?;
    Ok(target)
}

#[tauri::command]
pub async fn explore_path(path: String) -> Result<ExploreResult, String> {
    tauri::async_runtime::spawn_blocking(move || explore(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// 「打开方式」/「用默认程序打开」的目标：必须是一个**真实存在的文件**。
///
/// 文件夹不走这条路 —— 它没有"用哪个程序打开"这回事，交给外壳的 open 动词
/// 也只会打开一个窗口；真正会被执行的是文件（.exe 直接跑起来），所以这里
/// 把类型判断做在唯一一处，两个命令共用。
fn file_target(path: &str) -> Result<String, String> {
    let result = resolve(path)?;
    let is_file = fs::metadata(&result.path).map(|m| m.is_file()).unwrap_or(false);
    if !is_file {
        return Err(format!(
            "「{}」不是文件，不能用程序打开（文件夹请用「在文件资源管理器打开」）",
            result.path
        ));
    }
    Ok(result.path)
}

/// 用户按「取消」时 SHOpenWithDialog 返回 `ERROR_CANCELLED`：
/// 那是一次正常的选择，不该在界面上变成一行红字。
const CANCELLED: u32 = 0x8007_04C7;

/// 「打开方式…」：弹系统原生的"你要如何打开这个文件？"对话框。
///
/// 为什么自己不去读注册表列出候选程序：`UserChoice`/`ProgID`/`Applications`
/// 三套键在 Windows 各版本上都不一样，读漏了就会给出一串点了没反应的菜单项；
/// 而这个对话框是系统自己画的，选中的程序也由系统去执行 —— 我们只递一个路径。
///
/// `owner` 是主窗口句柄（`0` = 没有）。对话框是从一条后台线程弹出来的，
/// 没有 owner 的话它可能被主窗口盖住 —— 用户会以为"点了没反应"。
fn open_with_dialog(path: &str, owner: isize) -> Result<(), String> {
    let file = file_target(path)?;
    // 这个对话框是 STA 组件。spawn_blocking 的线程不保证 COM 还没被初始化过，
    // 所以在一条**新**线程里做 CoInitializeEx，保证拿到 STA 而不是 CHANGED_MODE。
    std::thread::spawn(move || -> Result<(), String> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|e| e.to_string())?;
            let _apartment = Apartment;
            // `pcszFile` 是裸 PCWSTR：宽串必须活到对话框关闭之后（`Show()` 返回），
            // 这里的 `wide` 就是那个"住到函数结束"的缓冲。
            let wide = HSTRING::from(file.as_str());
            let info = OPENASINFO {
                pcszFile: PCWSTR(wide.as_ptr()),
                pcszClass: PCWSTR::null(),
                // ALLOW_REGISTRATION：允许「始终使用此应用」；
                // EXEC：选完就直接打开，否则对话框只改关联、文件不动。
                oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
            };
            let parent = if owner == 0 { None } else { Some(HWND(owner as *mut core::ffi::c_void)) };
            match SHOpenWithDialog(parent, &info) {
                Ok(()) => Ok(()),
                Err(e) if e.code().0 as u32 == CANCELLED => Ok(()),
                Err(e) => Err(format!("打开「打开方式」对话框失败：{e}")),
            }
        }
    })
    .join()
    .map_err(|_| "「打开方式」对话框异常退出".to_string())?
}

#[tauri::command]
pub async fn open_with_dialog_path(window: tauri::WebviewWindow, path: String) -> Result<(), String> {
    // HWND 不是 Send，所以只把它的整数地址递进线程，再在那里还原。
    let owner = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || open_with_dialog(&path, owner))
        .await
        .map_err(|e| e.to_string())?
}

/// 「用默认程序打开」：双击的等价物。
///
/// 这是唯一一个**真的会执行文件**的菜单项，所以界面上只对非可执行扩展名显示
/// （见 `context-menu.js` 的 `CONTEXT_EXECUTABLE`）；可执行文件只留「打开方式…」，
/// 让用户在系统对话框里再确认一次。
fn open_default(path: &str) -> Result<(), String> {
    let file = file_target(path)?;
    shell_open(&file, "用默认程序打开")
}

#[tauri::command]
pub async fn open_default_path(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || open_default(&path))
        .await
        .map_err(|e| e.to_string())?
}

fn reveal(path: &str) -> Result<RevealResult, String> {
    let result = resolve(path)?;
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(|e| e.to_string())?;
        let _apartment = Apartment;
        let id = parse(&result.path)?;
        // cidl=0 selects a fully specified item in its parent folder, without opening the file.
        // https://learn.microsoft.com/windows/win32/api/shlobj_core/nf-shlobj_core-shopenfolderandselectitems
        SHOpenFolderAndSelectItems(id.0, None, 0).map_err(|e| format!("资源管理器定位失败：{e}"))?;
    }
    Ok(result)
}

#[tauri::command]
pub async fn reveal_path(path: String) -> Result<RevealResult, String> {
    tauri::async_runtime::spawn_blocking(move || reveal(&path)).await.map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_filesystem_paths() {
        for path in ["", "relative.txt", "C:", "shell:AppsFolder", "https://example.com", r"\\.\C:", "C:\\file\0tail"] {
            assert!(resolve(path).is_err(), "{path}");
        }
    }
    #[test]
    fn explore_target_opens_the_folder_and_never_the_file() {
        let owned = core_io::test_dir::TestDir::new("explorer-open").unwrap();
        let dir = owned.path().join("子目录");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("报表 & 汇总.xlsx");
        fs::write(&file, b"keep").unwrap();

        // 文件夹 → 打开它自己
        let d = explore_target(&dir.to_string_lossy()).unwrap();
        assert!(d.exact);
        assert!(d.path.ends_with("子目录"), "{}", d.path);

        // 文件 → 打开**所在目录**，返回值绝不能是那个文件
        // （把文件交给 open 动词 = 用默认程序打开它，那不是这个菜单项的意思）
        let f = explore_target(&file.to_string_lossy()).unwrap();
        assert!(f.exact);
        assert!(f.path.ends_with("子目录") && !f.path.ends_with(".xlsx"), "{}", f.path);
        assert_ne!(f.path, file.to_string_lossy());

        // 目标已被删掉 → 退到仍存在的上级，并如实说"不精确"
        let gone = explore_target(&dir.join("没了.txt").to_string_lossy()).unwrap();
        assert!(!gone.exact);
        assert!(gone.path.ends_with("子目录"), "{}", gone.path);
    }

    #[test]
    fn explore_target_rejects_non_filesystem_paths() {
        for path in ["", "relative.txt", "C:", "https://example.com", r"\\.\C:"] {
            assert!(explore_target(path).is_err(), "{path}");
        }
    }

    #[test]
    fn file_target_requires_an_existing_file() {
        let owned = core_io::test_dir::TestDir::new("explorer-open-with").unwrap();
        let dir = owned.path().join("文档");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("报表 & 汇总.docx");
        fs::write(&file, b"keep").unwrap();

        // 文件 → 拿到规范化后的完整路径，且内容一个字节都没动
        let target = file_target(&file.to_string_lossy()).unwrap();
        assert!(target.ends_with("报表 & 汇总.docx"), "{target}");
        assert_eq!(fs::read(&file).unwrap(), b"keep");

        // 文件夹没有「用哪个程序打开」这回事
        let e = file_target(&dir.to_string_lossy()).unwrap_err();
        assert!(e.contains("不是文件"), "{e}");

        // 文件已被删 → resolve 退到还存在的上级目录，但目录不是文件，
        // 不能拿"上级目录"顶替用户点的那个文件去打开
        let e = file_target(&dir.join("没了.docx").to_string_lossy()).unwrap_err();
        assert!(e.contains("不是文件"), "{e}");

        for path in ["", "relative.txt", "C:", "https://example.com", r"\\.\C:", "C:\\a\0b"] {
            assert!(file_target(path).is_err(), "{path}");
        }
    }

    #[test]
    fn existing_and_missing_paths_resolve_without_commands_or_file_execution() {
        let owned = core_io::test_dir::TestDir::new("explorer-resolve").unwrap();
        let path = owned.path().join("中文 & comma, test.txt");
        fs::write(&path, b"keep").unwrap();
        let result = resolve(&path.to_string_lossy()).unwrap();
        assert!(result.exact);
        assert!(result.path.ends_with("中文 & comma, test.txt"));
        let missing = resolve(&owned.path().join("missing/sub.txt").to_string_lossy()).unwrap();
        assert!(!missing.exact);
        assert_eq!(missing.path, display_path(&fs::canonicalize(owned.path()).unwrap()));
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        // Exercise native PIDL parsing and RAII release only; never open Explorer in tests.
        std::thread::spawn(move || unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _apartment = Apartment;
            let _id = parse(&result.path).unwrap();
        }).join().unwrap();
    }
}
