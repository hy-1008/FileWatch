//! Windows 原生文件 / 文件夹选择器。只返回路径，不扫描也不修改所选目标。
//!
//! 一条命令覆盖全部用途（`choose_path`）：**标题和扩展名过滤必须跟着用途变**。
//! 早先是 `choose_file` / `choose_folder` 两条写死标题的命令，用它们去选快照时
//! 对话框上写着「选择要监控的文件」—— 能选对，但用户没法确认自己选的是什么。
//! 分成两条还带来第二个问题：加过滤只能加在其中一条上，另一条永远没有。
//! 现在只有一处实现，`kind` 决定行为。
//!
//! 所有传给 COM 的字符串都是 `PCWSTR`（裸指针），**缓冲区必须活到 `Show()` 之后**。
//! 所以宽字符串的 `Vec<u16>` 一律由调用方持有，见 `choose()` 里的 `_keep`。

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;

/// 是选文件还是选文件夹。别用 bool 传（`folder: true` 在调用点看不出含义），
/// 也别让它静默：不认识的值直接报错，免得「想要文件夹却弹了文件框」。
const KIND_FILE: &str = "file";
const KIND_FOLDER: &str = "folder";

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) { unsafe { CoUninitialize() }; }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 扩展名过滤项。只接受 `tsv` 这种纯扩展名（`*.tsv` 与 `C:\a.tsv` 都可能被
/// 传进来，拼进过滤串会造出一个永远选不到东西的对话框）。
fn filter_spec(ext: &str) -> Option<String> {
    let e = ext.trim().trim_start_matches('*').trim_start_matches('.');
    if e.is_empty() || e.contains(['\\', '/', ':', ' ', '*', '?', '"']) {
        return None;
    }
    Some(format!("*.{e}"))
}

/// 建对话框并设好选项与标题。**不设过滤器**（那需要更多缓冲区，调用方自己加）。
unsafe fn create_dialog(kind: &str, title: &[u16]) -> windows::core::Result<IFileOpenDialog> {
    let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    let mut options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM;
    if kind == KIND_FOLDER { options |= FOS_PICKFOLDERS; }
    dialog.SetOptions(options)?;
    dialog.SetTitle(PCWSTR::from_raw(title.as_ptr()))?;
    Ok(dialog)
}

fn choose(owner: isize, kind: &str, title: &str, exts: &[String]) -> Result<Option<String>, String> {
    if kind != KIND_FILE && kind != KIND_FOLDER {
        return Err(format!("选择器只认 file / folder，收到 {kind:?}"));
    }
    // All COM objects are created, used and released on this worker's STA.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()
            .map_err(|e| format!("无法初始化选择器：{e}"))?;
        let _apartment = Apartment;

        // 下面每个 `wide()` 的结果都必须活到 Show() 之后，所以集中放在这里，
        // 而不是散在各条 Set* 调用旁边（那样一行结束就释放了，COM 拿到野指针）
        let keep_title = wide(title);
        let mut keep_name: Vec<Vec<u16>> = Vec::new();
        let mut keep_spec: Vec<Vec<u16>> = Vec::new();
        let mut keep_default: Vec<Vec<u16>> = Vec::new();

        let dialog = create_dialog(kind, &keep_title)
            .map_err(|e| format!("无法创建选择器：{e}"))?;

        // 过滤只给「选文件」：一个选文件夹的对话框上挂一排扩展名是自相矛盾的
        if kind == KIND_FILE {
            for ext in exts {
                if let Some(pat) = filter_spec(ext) {
                    keep_name.push(wide(&format!("{ext} 文件")));
                    keep_spec.push(wide(&pat));
                }
            }
            let specs: Vec<COMDLG_FILTERSPEC> = (0..keep_name.len())
                .map(|i| COMDLG_FILTERSPEC {
                    pszName: PCWSTR::from_raw(keep_name[i].as_ptr()),
                    pszSpec: PCWSTR::from_raw(keep_spec[i].as_ptr()),
                })
                .collect();
            if !specs.is_empty() {
                dialog.SetFileTypes(&specs).map_err(|e| format!("无法设置扩展名过滤：{e}"))?;
                // 手填的文件名不带扩展名时由它补上
                let first = exts.iter().filter_map(|e| filter_spec(e)).next()
                    .and_then(|p| p.strip_prefix("*.").map(str::to_string));
                if let Some(d) = first {
                    keep_default.push(wide(&d));
                    dialog.SetDefaultExtension(PCWSTR::from_raw(keep_default[0].as_ptr()))
                        .map_err(|e| format!("无法设置默认扩展名：{e}"))?;
                }
            }
        }

        match dialog.Show(Some(HWND(owner as *mut _))) {
            Ok(()) => {}
            // 0x800704C7 = ERROR_CANCELLED：用户自己点的取消，不是错误
            Err(e) if e.code() == windows::core::HRESULT(0x800704c7u32 as i32) => return Ok(None),
            Err(e) => return Err(format!("选择失败：{e}")),
        }
        let item = dialog.GetResult().map_err(|e| e.to_string())?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(|e| e.to_string())?;
        let result = path.to_string().map_err(|e| e.to_string());
        CoTaskMemFree(Some(path.0.cast()));
        result.map(Some)
    }
}

fn default_title(kind: &str) -> &'static str {
    if kind == KIND_FOLDER { "选择文件夹" } else { "选择文件" }
}

/// 打开文件 / 文件夹选择器，返回所选路径（用户取消则返回 `null`）。
///
/// * `kind` —— `file` 或 `folder`
/// * `title` —— 对话框标题，省略则按 `kind` 给个通称
/// * `exts` —— 只对 `file` 有效，如 `["tsv"]`
#[tauri::command]
pub async fn choose_path(
    window: tauri::WebviewWindow,
    kind: String,
    title: Option<String>,
    exts: Option<Vec<String>>,
) -> Result<Option<String>, String> {
    let owner = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let t = title.unwrap_or_else(|| default_title(&kind).to_string());
    let e = exts.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || choose(owner, &kind, &t, &e))
        .await.map_err(|e| format!("选择器线程出错：{e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_spec_accepts_plain_extensions_only() {
        assert_eq!(filter_spec("tsv").as_deref(), Some("*.tsv"));
        // 手填时最常见的几种写法，都得归一到同一串
        assert_eq!(filter_spec(".tsv").as_deref(), Some("*.tsv"));
        assert_eq!(filter_spec("*.tsv").as_deref(), Some("*.tsv"));
        assert_eq!(filter_spec(" tsv ").as_deref(), Some("*.tsv"));
        for bad in ["", " ", "*", ".", "*.", "C:\\a.tsv", "a/b", "ts v", "ts*"] {
            assert_eq!(filter_spec(bad), None, "{bad:?} 不该被当成扩展名");
        }
    }

    #[test]
    fn unknown_kind_is_rejected_before_any_dialog() {
        // 拼错 kind 时不能「弹出一个默认的文件框了事」：那会让用户以为在选文件夹
        for kind in ["", "File", "FILE", "dir", "folder "] {
            assert!(choose(0, kind, "t", &[]).is_err(), "{kind:?} 应被拒绝");
        }
    }

    #[test]
    fn native_dialog_initializes_for_filesystem_folders_without_showing() {
        std::thread::spawn(|| unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _apartment = Apartment;
            // 每个标题缓冲都要活过 SetTitle —— 这里用绑定持有，正是 choose() 的做法
            let t_folder = wide("选择文件夹");
            let t_file = wide("选择文件");
            let folder = create_dialog(KIND_FOLDER, &t_folder).unwrap();
            let options = folder.GetOptions().unwrap();
            assert_ne!((options & FOS_PICKFOLDERS).0, 0);
            assert_ne!((options & FOS_FORCEFILESYSTEM).0, 0);
            let file = create_dialog(KIND_FILE, &t_file).unwrap();
            assert_eq!((file.GetOptions().unwrap() & FOS_PICKFOLDERS).0, 0);
        }).join().unwrap();
    }
}
