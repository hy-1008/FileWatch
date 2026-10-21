//! 降级模式的增量来源：`ReadDirectoryChangesW`（文档风险 R2 的解法）。
//!
//! # 为什么需要它
//!
//! `$MFT` 直读 + USN Journal 是**快速路径**，但它要求管理员权限。
//! 普通用户双击运行 GUI 时拿不到 `\\.\C:` 句柄，如果没有这一层，
//! 「降级模式」就只能做一次全量扫描、之后永远是死图 —— 功能被打折。
//!
//! `ReadDirectoryChangesW` 的妙处在于：**它不需要任何特权**，
//! 只要对该目录有读权限即可递归监视整棵子树，且由内核做事件聚合，
//! 不会像轮询那样随文件数增长而变慢。
//!
//! # 与 USN 的差异（这些差异决定了上层要做什么补偿）
//!
//! | 维度 | USN Journal | ReadDirectoryChangesW |
//! |---|---|---|
//! | 标识 | 带 FRN（全局唯一、O(1) 定位） | **只有路径字符串** |
//! | 权限 | 管理员 | 普通用户 |
//! | 缓冲区 | 内核维护 32 MB 滚动日志，可断点续读 | **纯内存队列，溢出即丢** |
//! | 离线补偿 | 有（游标 + 重放） | **无**（进程不在 = 事件不在） |
//! | 大小变化 | 无，需回读 MFT | 无，需回读文件系统 |
//!
//! 因此上层（[`crate::watcher`]）必须维护一张 `路径 → 节点下标` 的映射，
//! 把「路径事件」翻译成引擎能吃的 FRN 事件。这也是本模块只做
//! **「收事件」**、不做任何语义解释的原因。
//!
//! # 实现要点
//!
//! * 用 **异步（OVERLAPPED）** 调用。这不是性能考虑，而是**正确性**考虑：
//!   内核只报告「读取挂上之后」发生的变更，而线程从 `spawn` 到被真正调度
//!   之间有不确定的延迟。若把「挂第一次读取」留给线程做，`start()` 返回后
//!   调用方立刻写文件就可能被**整条永久丢掉**（降级模式没有游标可重放，
//!   `ReadDirectoryChangesW` 也没有离线补偿）。用 OVERLAPPED 之后
//!   `ReadDirectoryChangesW` 立刻返回 `ERROR_IO_PENDING`，但读取已经在
//!   内核里排队了 —— 于是 `start()` 可以保证「返回即已就位」，窗口为零。
//! * 缓冲区与 `OVERLAPPED` 一起放进 `Box`：内核会一直引用它们的地址直到
//!   完成通知，移动它们等于让内核写到错的地方（UB，表现为偶发崩溃）。
//! * 停止时先置停止位、再 `CancelIoEx` 打醒、再 `join`，最后关句柄。
//!   顺序反了会永久卡在等 I/O 上；少了停止位，线程还会在取消之后补挂一次
//!   新的读取，然后永远没人再取消它。
//! * 单次返回的缓冲里是一条**变长记录链**（`FILE_NOTIFY_INFORMATION`），
//!   靠 `NextEntryOffset == 0` 判定结束；记录**不保证 UTF-16 对齐**，
//!   所以一律按 `u32` 逐字节读，不做 `#[repr(C)]` 结构体强转。

use super::{normalize_root, FsEvent};
use core_io::util::to_wide;
use core_io::winapi::*;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

// WinError.h: directory notification details are no longer trustworthy.
const ERROR_NOTIFY_ENUM_DIR: u64 = 1022;

fn completion_error(bytes: u32, stopping: bool) -> u64 {
    if bytes == 0 && !stopping { ERROR_NOTIFY_ENUM_DIR } else { 0 }
}

#[test]
fn zero_byte_completion_requires_rescan_unless_stopping() {
    assert_eq!(completion_error(0, false), ERROR_NOTIFY_ENUM_DIR);
    assert_eq!(completion_error(0, true), 0);
    assert_eq!(completion_error(64, false), 0);
}
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// `HANDLE` 裸指针本身不是 `Send`，但 Win32 句柄是进程级资源、
/// **可以在线程间传递所有权**（只是不能并发同时读写同一个句柄的同一 I/O）。
///
/// 这里的使用模式是「一条线程专做读、主线程只在停止时 CancelIoEx」，
/// 因此包装成 `Send + Sync` 是安全的。
#[derive(Clone, Copy)]
struct RawHandle(HANDLE);

// SAFETY: 见 `RawHandle` 文档——句柄是进程级内核对象引用，
// 跨线程传递不涉及 Rust 的内存别名规则。
unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

/// 一次异步目录读取所需的、**地址必须固定**的两样东西。
///
/// 发起 I/O 之后内核会一直引用 `OVERLAPPED` 与缓冲区的地址，直到
/// `GetOverlappedResult` 报告完成。所以必须先在堆上把地址定下来、再发起第一次
/// 读取，之后交给线程 —— 移动 `Box` 只是移动指针，地址不变。
///
/// 反面教材：在线程闭包**里面**才建 `OVERLAPPED` 和缓冲区（那正是最初
/// 「挂第一次读取」的写法），或者把它们建在 `start()` 的栈上再整体 `move`
/// 进闭包 —— 后者等于允许内核写入一个已经失效的栈地址。
struct IoSlot {
    ov: OVERLAPPED,
    buf: Vec<u8>,
}

// SAFETY: `ov.hEvent` 是内核管理的句柄字段，我们只往内核交地址、从不 deref；
// `buf` 是独占的 `Vec`。整个 `IoSlot` 只会被一条线程使用（`start()` 里发起
// 第一次读取后就不再有第二个使用者），符合 `Send` 的语义要求。
unsafe impl Send for IoSlot {}

/// 在一个 `IoSlot` 上发起一次**异步**目录读取。
///
/// 返回 `Ok(())` 表示内核已受理：要么已立刻完成，要么回 `ERROR_IO_PENDING`。
/// **不要在这里等结果** —— 这个函数的全部意义就是「挂上并从内核视角立刻返回」。
/// 失败时返回原始 Win32 错误码（不是 `io::Error`），因为调用方往往还要
/// 结合「是不是正在停止」来判断该记 0 还是记错误。
fn arm_read(h: HANDLE, slot: &mut IoSlot, buf_bytes: usize, subtree: BOOL) -> Result<(), DWORD> {
    // SAFETY: 句柄由调用方保证有效（`stop()` 是先取消、后关闭）；
    // `slot` 位于堆上且在本次 I/O 完成前不会被移动或释放。
    // `lpBytesReturned` 传 NULL：异步模式下字节数由 `GetOverlappedResult` 给出，
    // 传一个栈上局部量的指针反而是隐患（内核在函数返回后不再写它，但没必要冒险）。
    let ok = unsafe {
        ReadDirectoryChangesW(
            h,
            slot.buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf_bytes as DWORD,
            subtree,
            FILE_NOTIFY_CHANGE_ALL,
            std::ptr::null_mut(),
            &mut slot.ov as *mut OVERLAPPED as *mut core::ffi::c_void,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let err = unsafe { GetLastError() };
    if err == ERROR_IO_PENDING {
        return Ok(());
    }
    Err(err)
}

/// 一个正在运行的目录监视器。
pub struct DirWatcher {
    h: Arc<RawHandle>,
    /// 被监视根的真实绝对路径（不带结尾反斜杠）
    pub root: String,
    /// 是否递归监视子树
    pub watch_subtree: bool,
    rx: Receiver<FsEvent>,
    join: Option<JoinHandle<()>>,
    /// 内核返回后被我们解析出来的通知总数
    received: Arc<AtomicU64>,
    /// 因通道/缓冲问题丢弃的通知数（对账用）
    dropped: Arc<AtomicU64>,
    /// 请求停止的标志：线程在补挂下一次读取之前会检查它
    stopping: Arc<AtomicBool>,
    /// 监视线程的退出码（正常停止恒为 0，见 [`DirWatcher::exit_error`]）
    exit_error: Arc<AtomicU64>,
}

impl DirWatcher {
    /// 打开目录句柄并启动监视线程。
    ///
    /// * `buffer_bytes` —— 单次 `ReadDirectoryChangesW` 的缓冲大小。
    ///   太小会在**突发变更**时返回 `ERROR_NOTIFY_ENUM_DIR`（事件丢失），
    ///   所以默认给 64 KB。
    /// * `watch_subtree` —— 是否递归。关闭后只报告根目录**直接子项**的变更，
    ///   在大盘上能显著降低事件量。
    pub fn start(root: &Path, buffer_bytes: usize, watch_subtree: bool) -> io::Result<DirWatcher> {
        let root_str = normalize_root(&root.to_string_lossy());
        let wide = to_wide(&root_str);

        // SAFETY: `wide` 以 NUL 结尾且在此处存活到调用结束；
        // 其余参数都是按值传的常量，无指针别名问题。
        // `FILE_FLAG_OVERLAPPED` 是必需的：见文件头「实现要点」第一条，
        // 只有异步形态才能在 `start` 返回之前就把第一次读取挂到内核上。
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let handle = Arc::new(RawHandle(h));
        let (tx, rx) = mpsc::channel::<FsEvent>();
        let received = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let exit_error = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));

        let buf_bytes = buffer_bytes.clamp(4096, 4 * 1024 * 1024);
        let subtree: BOOL = if watch_subtree { 1 } else { 0 };

        // ── 关键一步：在 `start` 返回**之前**就把第一次读取挂到内核上 ────────
        // 内核只报告「读取挂上之后」发生的变更。若把这一步留在线程里做，
        // 「`start()` 返回 → 调用方立刻写文件 → 线程才被调度」这段窗口里的
        // 变更会**永久**丢失：降级模式既没有 USN 游标可以重放，这个 API 本身
        // 也不缓存「挂上之前」的历史。用 OVERLAPPED 之后这次调用立刻返回
        // `ERROR_IO_PENDING`，但读取已在内核排队 —— 窗口收敛到零。
        //
        // 在同一台机器上并行跑多个测试、或机器本身负载高时，线程调度延迟会
        // 从微秒级涨到毫秒级，这个窗口就从「理论存在」变成「稳定复现」。
        let mut slot: Box<IoSlot> = Box::new(IoSlot {
            ov: OVERLAPPED::zeroed(),
            buf: vec![0u8; buf_bytes],
        });
        if let Err(code) = arm_read(handle.0, &mut slot, buf_bytes, subtree) {
            // SAFETY: 句柄尚未交给任何线程。
            unsafe {
                CloseHandle(handle.0);
            }
            return Err(io::Error::from_raw_os_error(code as i32));
        }

        let thread_handle = handle.clone();
        let thread_received = received.clone();
        let thread_dropped = dropped.clone();
        let thread_exit = exit_error.clone();
        let thread_stopping = stopping.clone();

        let join = std::thread::Builder::new()
            .name("filewatch-dirwatch".to_string())
            .spawn(move || {
                // `slot` 的地址在 `Box` 里已经定下来了，把它 move 进闭包
                // 移动的只是那个指针 —— 内核持有的地址不变。
                let mut slot = slot;
                loop {
                    let mut ret: DWORD = 0;
                    // SAFETY: 句柄此刻必然有效（`stop()` 是先取消、后 join、
                    // 最后才 CloseHandle）；`slot` 在堆上且本循环期间不移动。
                    let ok = unsafe {
                        GetOverlappedResult(
                            thread_handle.0,
                            &mut slot.ov as *mut OVERLAPPED,
                            &mut ret,
                            1, // bWait：阻塞到本次 I/O 完成（含被取消）
                        )
                    };
                    if ok == 0 {
                        let err = unsafe { GetLastError() };
                        // 被 `CancelIoEx` 中止是**正常停止**，不是错误。
                        // 把它们混在一起（原实现就存了 995）会让「监视线程异常
                        // 退出」这条告警每次关程序都误报一次，久了就没人看了。
                        let code =
                            if err == ERROR_OPERATION_ABORTED || thread_stopping.load(Ordering::Acquire)
                            {
                                0
                            } else {
                                err as u64
                            };
                        thread_exit.store(code, Ordering::Relaxed);
                        break;
                    }
                    // Microsoft documents a successful zero-byte completion as a
                    // lost notification buffer. Require a rescan, never silently
                    // keep using the now-stale index.
                    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw
                    let lost = completion_error(ret, thread_stopping.load(Ordering::Acquire));
                    if lost != 0 {
                        thread_exit.store(lost, Ordering::Relaxed);
                        break;
                    }
                    if ret > 0 {
                        // 记录链在缓冲区里的长度就是 `ret`；解析器自己防御截断/坏链
                        parse_notifications(&slot.buf[..ret as usize], |ev| {
                            thread_received.fetch_add(1, Ordering::Relaxed);
                            // 接收端已销毁 → 计数后丢弃（不 panic，也不要退出循环）
                            if tx.send(ev).is_err() {
                                thread_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                    // 正在停止就**不要**再挂新的读取：否则 `stop()` 的那次
                    // `CancelIoEx` 会扑空，而这次新挂上的 I/O 再也没人取消，
                    // `join()` 就永久卡住了。
                    if thread_stopping.load(Ordering::Acquire) {
                        break;
                    }
                    // 立刻补上下一次读取，中间不做任何别的动作 ——
                    // 这段时间里内核仍在把变更攒进句柄的缓冲区，但间隔越短越好。
                    if let Err(code) = arm_read(thread_handle.0, &mut slot, buf_bytes, subtree) {
                        let code = if thread_stopping.load(Ordering::Acquire) {
                            0
                        } else {
                            code as u64
                        };
                        thread_exit.store(code, Ordering::Relaxed);
                        break;
                    }
                }
            })?;

        Ok(DirWatcher {
            h: handle,
            root: root_str,
            watch_subtree,
            rx,
            join: Some(join),
            received,
            dropped,
            stopping,
            exit_error,
        })
    }

    /// 非阻塞取走最多 `max` 条通知，返回实际取到的条数。
    pub fn drain(&mut self, max: usize, out: &mut Vec<FsEvent>) -> usize {
        let mut n = 0;
        while n < max {
            match self.rx.try_recv() {
                Ok(ev) => {
                    out.push(ev);
                    n += 1;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        n
    }

    /// 内核侧累计产出的通知数。
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// 因主线程通道被丢弃的通知数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 监视线程的退出码。**正常停止恒为 0**（被 `CancelIoEx` 中止不算错误），
    /// 非 0 才说明是 `ReadDirectoryChangesW` 真的失败了（如缓冲区溢出、
    /// 句柄被外部关闭）—— 此时增量是不可信的，上层应当提示用户重新全量扫描。
    pub fn exit_error(&self) -> u64 {
        self.exit_error.load(Ordering::Relaxed)
    }

    /// 停止监视并回收线程。可重复调用。
    pub fn stop(&mut self) {
        if self.join.is_none() {
            return;
        }
        // ① 先置停止位。这一步必须在取消之前：线程在「等到了上一次结果、
        //    准备补挂下一次读取」的窄缝里会检查它，从而主动退出而不是又挂上
        //    一次没人取消的 I/O。
        self.stopping.store(true, Ordering::Release);
        // ② 打醒可能正挂着的异步读取
        // SAFETY: 句柄仍有效（尚未 CloseHandle），lpOverlapped = NULL 表示
        // 取消该句柄上所有挂起的 I/O。
        let cancel = |h: HANDLE| unsafe {
            CancelIoEx(h, std::ptr::null_mut());
        };
        cancel(self.h.0);
        // ③ 等线程退出，避免线程还在用句柄时被关闭。
        //    **不能只取消一次**：线程可能正好落在「刚拿到完成结果、还没走到
        //    停止位检查」的窄缝里，那一次 CancelIoEx 会扑空，而线程紧接着挂上的
        //    新读取就没人取消了。所以补几次，每次都能把新挂上的 I/O 取消掉。
        if let Some(j) = self.join.take() {
            for _ in 0..100 {
                if j.is_finished() {
                    break;
                }
                cancel(self.h.0);
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = j.join();
        }
        // ④ 最后关句柄
        // SAFETY: 此刻已无其他使用者。
        unsafe {
            CloseHandle(self.h.0);
        }
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for DirWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirWatcher")
            .field("root", &self.root)
            .field("subtree", &self.watch_subtree)
            .field("received", &self.received())
            .finish()
    }
}

/// 解析一条变长 `FILE_NOTIFY_INFORMATION` 链，对每条记录调用 `f`。
///
/// 返回解析出的记录条数。
///
/// 记录布局见 [`core_io::winapi::FILE_NOTIFY_INFORMATION`]。这里刻意**不做
/// 结构体强转**：把 `&[u8]` 直接 cast 成 `#[repr(C)]` 结构体要求 4 字节对齐，
/// 而 `NextEntryOffset` 只保证 4 字节对齐、`FileName` 后面紧跟的记录头未必
/// 与缓冲起点同余，一旦 UB 就是「偶发崩溃」，最难查的那类 bug。
pub fn parse_notifications(buf: &[u8], mut f: impl FnMut(FsEvent)) -> usize {
    let mut off = 0usize;
    let mut count = 0usize;
    // 防御：损坏的 offset 链可能成环，最多转 1 万圈
    for _ in 0..10_000 {
        if off + FILE_NOTIFY_NAME_OFFSET > buf.len() {
            break;
        }
        let next = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let action = u32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let name_len =
            u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]) as usize;

        // FileNameLength 是**字节数**，且可能超过剩余缓冲（截断的尾记录）
        let avail = buf.len() - (off + FILE_NOTIFY_NAME_OFFSET);
        let take = name_len.min(avail);
        // 奇数长度说明记录损坏，直接丢弃
        if take >= 2 {
            let raw = &buf[off + FILE_NOTIFY_NAME_OFFSET..off + FILE_NOTIFY_NAME_OFFSET + take];
            count += 1;
            f(FsEvent {
                action,
                rel: utf16le_to_string(raw),
            });
        }

        if next == 0 {
            break;
        }
        // 偏移必须前进，否则认为链损坏
        let next = next as usize;
        if next % 4 != 0 || next < FILE_NOTIFY_NAME_OFFSET {
            break;
        }
        off += next;
    }
    count
}

/// UTF-16LE 字节 → `String`（非法代理对替换为 U+FFFD）。
fn utf16le_to_string(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一条 `FILE_NOTIFY_INFORMATION` 记录链，验证解析器按偏移走链、
    /// 且名字按「字节长度」而非字符数截取。
    fn encode(records: &[(DWORD, &str)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for (i, (action, name)) in records.iter().enumerate() {
            let units: Vec<u16> = name.encode_utf16().collect();
            let name_bytes = units.len() * 2;
            let mut rec = Vec::with_capacity(FILE_NOTIFY_NAME_OFFSET + name_bytes);
            rec.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset 占位
            rec.extend_from_slice(&action.to_le_bytes());
            rec.extend_from_slice(&(name_bytes as u32).to_le_bytes());
            for u in &units {
                rec.extend_from_slice(&u.to_le_bytes());
            }
            // 记录之间按 4 字节对齐
            while rec.len() % 4 != 0 {
                rec.push(0);
            }
            let is_last = i + 1 == records.len();
            let next = if is_last { 0u32 } else { rec.len() as u32 };
            rec[0..4].copy_from_slice(&next.to_le_bytes());
            out.extend_from_slice(&rec);
        }
        out
    }

    #[test]
    fn parses_single_record() {
        let buf = encode(&[(FILE_ACTION_ADDED, "a.txt")]);
        let mut got = Vec::new();
        let n = parse_notifications(&buf, |e| got.push(e));
        assert_eq!(n, 1);
        assert_eq!(got[0].action, FILE_ACTION_ADDED);
        assert_eq!(got[0].rel, "a.txt");
    }

    #[test]
    fn parses_multi_record_chain() {
        let buf = encode(&[
            (FILE_ACTION_ADDED, "sub"),
            (FILE_ACTION_ADDED, r"sub\b.bin"),
            (FILE_ACTION_MODIFIED, r"sub\b.bin"),
            (FILE_ACTION_RENAMED_OLD_NAME, r"sub\b.bin"),
            (FILE_ACTION_RENAMED_NEW_NAME, r"sub\c.bin"),
        ]);
        let mut got = Vec::new();
        parse_notifications(&buf, |e| got.push(e));
        assert_eq!(got.len(), 5);
        assert_eq!(got[1].rel, r"sub\b.bin");
        assert_eq!(got[3].action, FILE_ACTION_RENAMED_OLD_NAME);
        assert_eq!(got[4].rel, r"sub\c.bin");
    }

    #[test]
    fn handles_multibyte_names_by_byte_length() {
        // 「新建文件夹」5 个 CJK 字符 = 5 个 UTF-16 码元 = 10 字节，
        // 若误按字符数截取就会只剩一半
        let buf = encode(&[(FILE_ACTION_ADDED, "新建文件夹")]);
        let mut got = Vec::new();
        parse_notifications(&buf, |e| got.push(e));
        assert_eq!(got[0].rel, "新建文件夹");
    }

    #[test]
    fn truncated_tail_does_not_panic() {
        let full = encode(&[(FILE_ACTION_ADDED, "aaa.txt"), (FILE_ACTION_ADDED, "bbb.txt")]);
        // 砍掉最后 4 字节，制造被截断的尾记录
        let cut = &full[..full.len() - 4];
        let mut got = Vec::new();
        parse_notifications(cut, |e| got.push(e));
        // 至少第一条完好
        assert!(!got.is_empty());
        assert_eq!(got[0].rel, "aaa.txt");
    }

    #[test]
    fn garbage_buffer_is_safe() {
        let buf = vec![0xFFu8; 64];
        let mut got = Vec::new();
        parse_notifications(&buf, |e| got.push(e));
        // 不 panic 即达标（0xFF 链会被 next % 4 检查挡住）
        let _ = got;
    }

    #[test]
    fn empty_buffer_is_safe() {
        let mut got = Vec::new();
        assert_eq!(parse_notifications(&[], |e| got.push(e)), 0);
        assert!(got.is_empty());
    }

    #[test]
    fn normalize_root_keeps_drive_root_backslash() {
        assert_eq!(normalize_root("C:\\"), "C:\\");
        assert_eq!(normalize_root("C:\\sub\\"), "C:\\sub");
        assert_eq!(normalize_root("C:\\sub"), "C:\\sub");
    }

    #[test]
    fn watcher_starts_and_stops_on_temp_dir() {
        let owned = core_io::test_dir::TestDir::new("watcher-start-stop").unwrap();
        let base = owned.path().to_path_buf();
        let mut w = match DirWatcher::start(&base, 64 * 1024, false) {
            Ok(w) => w,
            Err(e) => {
                // 极少数受限环境下连目录句柄都拿不到，跳过而不是误报失败
                eprintln!("跳过：无法打开目录句柄 ({})", e);
                return;
            }
        };
        // 制造一次真实变更
        let probe = base.join(format!("fw_watch_probe_{}", std::process::id()));
        std::fs::write(&probe, b"hi").unwrap();

        // 内核投递有延迟，最多等 2 秒
        let mut got: Vec<FsEvent> = Vec::new();
        for _ in 0..40 {
            w.drain(256, &mut got);
            if got.iter().any(|e| e.rel.contains("fw_watch_probe_")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        w.stop();
        assert!(
            got.iter().any(|e| e.rel.contains("fw_watch_probe_")),
            "应捕获到刚创建的文件，实际收到 {:?}",
            got
        );
    }

    /// 回归：`start()` 返回之后**立刻**发生的变更不能丢。
    ///
    /// 这正是修复前会稳定复现的失败：内核只报告「第一次读取挂上之后」发生的
    /// 变更，而旧实现把「挂第一次读取」这件事留给了监视线程 —— 线程被调度之前
    /// 发生的创建会被**永久**丢掉（降级模式既没有 USN 游标可重放，
    /// 这个 API 也不补发「挂上之前」的历史）。修复后第一次读取在 `start()`
    /// 内部就已挂到内核上，于是「返回即已就位」。
    ///
    /// 跑 5 轮是因为只跑一轮有可能是运气 —— 线程偶尔调度得很快，掩盖问题。
    #[test]
    fn start_is_armed_before_returning() {
        let owned = core_io::test_dir::TestDir::new("fw-armed").unwrap();
        let base = owned.path().to_path_buf();
        std::fs::create_dir_all(&base).unwrap();

        for round in 0..5 {
            let mut w = match DirWatcher::start(&base, 64 * 1024, true) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("跳过：无法打开目录句柄 ({})", e);
                    return;
                }
            };

            let name = format!("armed_{}.txt", round);
            // 关键：`start` 一返回就写，中间不留任何缓冲时间
            std::fs::write(base.join(&name), b"x").unwrap();

            let mut got: Vec<FsEvent> = Vec::new();
            for _ in 0..40 {
                w.drain(256, &mut got);
                if got.iter().any(|e| e.rel.contains(&name)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let hit = got.iter().any(|e| e.rel.contains(&name));
            w.stop();

            // 正常停止不该被记成错误（旧实现会把 CancelIoEx 的 995 存进去）
            assert_eq!(w.exit_error(), 0, "正常停止时退出码必须是 0");

            assert!(
                hit,
                "第 {} 轮：`start` 返回后立刻创建的 {} 被丢了（收到的 {} 条：{:?}）",
                round,
                name,
                got.len(),
                got
            );
            owned.remove_file(&name).unwrap();
        }

    }
}
