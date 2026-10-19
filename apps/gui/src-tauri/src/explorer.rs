//! Explorer selection through PIDLs. Never executes a target file or builds a shell command.
use serde::Serialize;
use std::{fs, path::{Path, PathBuf}};
use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Common::ITEMIDLIST, ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName,
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

/// 打开一个文件夹窗口。`ShellExecuteW("open")` 走的是外壳的"打开"动词，
/// 与在资源管理器里双击这个文件夹是同一条路；命令行的 `explorer <path>`
/// 则由外壳去猜参数，路径里有空格或 `&` 时它猜错的样子非常难查。
fn open_folder(dir: &str) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| e.to_string())?;
        let _apartment = Apartment;
        let wide = HSTRING::from(dir);
        // 返回值 > 32 表示成功；否则是错误码（<= 32），但没有可读的文本，
        // 所以只能把码带回去。
        let r = ShellExecuteW(
            None,
            windows::core::w!("open"),
            &wide,
            None,
            None,
            SW_SHOWNORMAL,
        );
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(format!("打开资源管理器失败（代码 {}）", r.0 as isize))
        }
    }
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
