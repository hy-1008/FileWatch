//! Linux 后端：`inotify`。
//!
//! 字节级解析在 [`super::inotify_parse`]（平台无关、两个平台都有单测）；
//! 这个文件只做**系统调用**与**运行期补偿**。
//!
//! # 与 `ReadDirectoryChangesW` 的三个关键差异（每条都对应一段补偿代码）
//!
//! ## ① 一个 watch 只覆盖**一个目录**，不递归
//!
//! 所以 `start()` 里必须先把整棵子树跑一遍、逐目录 `inotify_add_watch`。
//! 更要紧的是**运行期**：新建目录（`IN_CREATE|IN_ISDIR`）或被移入的目录
//! （`IN_MOVED_TO|IN_ISDIR`）必须立刻补挂，否则那棵子树之后的变更
//! **一条也收不到**，而且**不报错** —— 界面看起来"监控正常运行"，
//! 实际那张图再也不动了。这类"静默失效"是本项目最忌讳的故障形态。
//!
//! ## ② watch 数量有上限（`fs.inotify.max_user_watches`）
//!
//! 老系统默认 8192。大盘上必然挂不满，此时 `inotify_add_watch` 返回
//! `ENOSPC`。这里**不**把它当成致命错误：能挂多少算多少，把失败目录数
//! 记在 [`DirWatcher::failed_dirs`] 里，由上层明确告诉用户
//! "有 N 个目录没被监控"。静默少挂等于静默漏事件。
//!
//! ## ③ 队列溢出是**独立事件**
//!
//! 事件率超过读取速度时内核回一条 `IN_Q_OVERFLOW`（`wd == -1`），
//! 之后的事件已经丢了。这时必须让 `exit_error()!=0`，让上层提示重新扫描
//! —— 与 Windows 的「成功但 0 字节」是同一种语义。
//!
//! # 为什么用 `poll` 而不是阻塞 `read`
//!
//! `stop()` 要能把线程**立刻**打醒。阻塞在 `read` 上时唯一干净的办法是
//! 关 fd（会让线程读到 `EBADF`，与真正的错误混在一起）。所以用
//! `poll(inotify_fd, wake_pipe)`：停止时往 pipe 写一个字节，
//! 线程从 `poll` 返回后看到停止标志就退出，`exit_error` 保持 0。

use super::inotify_parse::{self as ip, WATCH_MASK};
use super::{normalize_root, FsEvent};
use core_io::paths::SEP;
use std::collections::HashMap;
use std::io;
use std::os::raw::{c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ── libc 手写声明（零第三方依赖） ───────────────────────────────────────────

extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const c_char, mask: u32) -> i32;
    fn read(fd: i32, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: i32, buf: *const c_void, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn pipe2(fds: *mut i32, flags: i32) -> i32;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x001;

/// `O_CLOEXEC` / `O_NONBLOCK`（Linux 通用值，x86_64 / aarch64 一致）。
const O_CLOEXEC: i32 = 0o2_000_000;
const O_NONBLOCK: i32 = 0o4_000;

/// 一个正在运行的目录监视器。
pub struct DirWatcher {
    /// 只有读线程持有 inotify fd（它退出时自己关闭），主结构只留唤醒管道写端。
    wake_w: i32,
    /// 被监视根的真实绝对路径（不带结尾分隔符，卷根除外）
    pub root: String,
    /// 是否递归监视子树
    pub watch_subtree: bool,
    rx: Receiver<FsEvent>,
    join: Option<JoinHandle<()>>,
    received: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    exit_error: Arc<AtomicU64>,
    watched_dirs: Arc<AtomicU64>,
    failed_dirs: Arc<AtomicU64>,
}

/// 线程侧共享的统计量。
#[derive(Clone)]
struct Shared {
    received: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    exit_error: Arc<AtomicU64>,
}

impl DirWatcher {
    /// 打开 inotify 实例、**同步**挂好整棵子树的 watch，然后启动读线程。
    ///
    /// 挂 watch 在 `start()` 返回**之前**完成，理由与 Windows 版相同：
    /// 内核只报告"挂上之后"的变更，若把这一步留给线程，`start()` 返回到
    /// 线程被调度之间发生的变更会**永久**丢失 —— 降级模式没有日志可以重放。
    ///
    /// * `buffer_bytes` —— 单次 `read` 的缓冲大小（默认 64 KB）。
    ///   太小会在突发变更时丢事件（内核回 `IN_Q_OVERFLOW`），所以下限 4096。
    /// * `watch_subtree` —— 是否递归。关闭后只监视根目录**直接子项**。
    pub fn start(root: &Path, buffer_bytes: usize, watch_subtree: bool) -> io::Result<DirWatcher> {
        let root_str = normalize_root(&root.to_string_lossy());
        let root_path = PathBuf::from(&root_str);

        // SAFETY: `inotify_init1` 只返回 fd 或 -1，无指针参数。
        let fd = unsafe { inotify_init1(O_CLOEXEC | O_NONBLOCK) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!("inotify_init1 失败: {e}（容器里可能需要放开 seccomp）"),
            ));
        }

        // 唤醒管道：停止时写一个字节，把阻塞在 poll 上的线程叫醒
        let mut pipes = [0i32; 2];
        // SAFETY: `pipes` 是长度为 2 的数组，pipe2 只写这两个元素。
        if unsafe { pipe2(pipes.as_mut_ptr(), O_CLOEXEC | O_NONBLOCK) } != 0 {
            let e = io::Error::last_os_error();
            // SAFETY: fd 尚未交给任何线程。
            unsafe { close(fd) };
            return Err(e);
        }
        let (wake_r, wake_w) = (pipes[0], pipes[1]);

        // ── 同步挂 watch：根 + （可选）整棵子树 ─────────────────────────────
        let mut map: HashMap<i32, String> = HashMap::new();
        let mut failed: u64 = 0;
        match add_one(fd, &root_path, &root_str, &mut map) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(28) => {
                // 根都没挂上（watch 上限已达）——记下来继续跑，由上层用
                // failed_dirs 明确告警。静默少挂等于静默漏事件。
                failed += 1;
            }
            Err(e) => {
                // 打不开 / 不是目录 → 这个监视器没有意义，直接失败（不假装成功）
                // SAFETY: 三个 fd 都还没交给线程。
                unsafe {
                    close(fd);
                    close(wake_r);
                    close(wake_w);
                }
                return Err(io::Error::new(
                    e.kind(),
                    format!("无法监视 {}: {}", root_str, e),
                ));
            }
        }
        if watch_subtree && !map.is_empty() {
            failed += add_tree(fd, &root_path, &root_str, &mut map, 0);
        }

        let (tx, rx) = mpsc::channel::<FsEvent>();
        let shared = Shared {
            received: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            stopping: Arc::new(AtomicBool::new(false)),
            exit_error: Arc::new(AtomicU64::new(0)),
        };
        let watched_dirs = Arc::new(AtomicU64::new(map.len() as u64));
        let failed_dirs = Arc::new(AtomicU64::new(failed));

        let thread_root = root_str.clone();
        let thread_shared = shared.clone();
        let thread_watched = watched_dirs.clone();
        let thread_failed = failed_dirs.clone();
        let buf_bytes = buffer_bytes.clamp(4096, 4 * 1024 * 1024);

        let join = std::thread::Builder::new()
            .name("filewatch-inotify".to_string())
            .spawn(move || {
                let mut buf = vec![0u8; buf_bytes];
                let mut map = map;
                loop {
                    let mut fds = [
                        PollFd {
                            fd,
                            events: POLLIN,
                            revents: 0,
                        },
                        PollFd {
                            fd: wake_r,
                            events: POLLIN,
                            revents: 0,
                        },
                    ];
                    // SAFETY: fds 是长度为 2 的数组，poll 只读 events、写 revents。
                    let rc = unsafe { poll(fds.as_mut_ptr(), 2, 200) };
                    if rc < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        thread_shared.exit_error.store(
                            e.raw_os_error().unwrap_or(5) as u64,
                            Ordering::Relaxed,
                        );
                        break;
                    }
                    if rc == 0 {
                        // 超时：只用来检查停止位
                        if thread_shared.stopping.load(Ordering::Acquire) {
                            break;
                        }
                        continue;
                    }
                    if fds[1].revents & POLLIN != 0 {
                        break; // 停止信号
                    }
                    if fds[0].revents & POLLIN == 0 {
                        continue;
                    }

                    // SAFETY: buf 独占、长度为 buf.len()，read 最多写这么多字节。
                    let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        // 非阻塞 fd 在 poll 之后仍可能 EAGAIN（另一个读者抢走了），
                        // 这不是错误。
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        let code = if thread_shared.stopping.load(Ordering::Acquire) {
                            0
                        } else {
                            e.raw_os_error().unwrap_or(5) as u64
                        };
                        thread_shared.exit_error.store(code, Ordering::Relaxed);
                        break;
                    }
                    if n == 0 {
                        continue;
                    }

                    let outcome = ip::for_each(&buf[..n as usize], |ev| {
                        handle_event(
                            ev,
                            fd,
                            &mut map,
                            &thread_root,
                            &thread_shared,
                            &tx,
                            watch_subtree,
                            &thread_watched,
                            &thread_failed,
                        );
                    });
                    if outcome == ip::Outcome::Fatal {
                        thread_shared
                            .exit_error
                            .store(ip::LOST_EVENTS_CODE, Ordering::Relaxed);
                        break;
                    }
                    if thread_shared.stopping.load(Ordering::Acquire) {
                        break;
                    }
                }
                // SAFETY: 线程退出前独占这两个 fd（停止流程会 join 之后才 close）。
                unsafe {
                    close(fd);
                    close(wake_r);
                }
            })?;

        Ok(DirWatcher {
            wake_w,
            root: root_str,
            watch_subtree,
            rx,
            join: Some(join),
            received: shared.received,
            dropped: shared.dropped,
            stopping: shared.stopping,
            exit_error: shared.exit_error,
            watched_dirs,
            failed_dirs,
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

    /// 向主线程通道投递失败（接收端已销毁）而被丢弃的通知数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 读线程的退出码。**正常停止恒为 0**；非 0 说明事件流真的断了
    /// （队列溢出 28 / fd 被外部关闭等），增量不可信。
    pub fn exit_error(&self) -> u64 {
        self.exit_error.load(Ordering::Relaxed)
    }

    /// 成功挂上 watch 的目录数。
    pub fn watched_dirs(&self) -> u64 {
        self.watched_dirs.load(Ordering::Relaxed)
    }

    /// 因 `max_user_watches` 上限而**没能**挂上 watch 的目录数。
    ///
    /// 非 0 就必须在界面上说实话：这些目录里的变更不会产生任何事件。
    pub fn failed_dirs(&self) -> u64 {
        self.failed_dirs.load(Ordering::Relaxed)
    }

    /// 停止监视并回收线程。可重复调用。
    pub fn stop(&mut self) {
        if self.join.is_none() {
            return;
        }
        // ① 置停止位；② 写唤醒字节把 poll 打醒
        self.stopping.store(true, Ordering::Release);
        let byte = [b'x'];
        // SAFETY: wake_w 仍有效（尚未 close），写入 1 字节。
        unsafe {
            write(self.wake_w, byte.as_ptr() as *const c_void, 1);
        }
        // ③ 等线程退出（它自己会关掉 inotify fd 与读端）
        if let Some(j) = self.join.take() {
            for _ in 0..600 {
                if j.is_finished() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = j.join();
        }
        // ④ 关掉写端。inotify fd 与读端已由线程关闭 —— 关闭 inotify fd 会
        //    隐式移除该实例的全部 watch，所以不需要逐个 `inotify_rm_watch`。
        // SAFETY: 此刻已无其他使用者。
        unsafe {
            close(self.wake_w);
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
            .field("watched_dirs", &self.watched_dirs())
            .field("failed_dirs", &self.failed_dirs())
            .field("received", &self.received())
            .finish()
    }
}

// ── 单条事件的处理（含"补挂子树 watch"） ────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_event(
    ev: ip::Event,
    fd: i32,
    map: &mut HashMap<i32, String>,
    root: &str,
    shared: &Shared,
    tx: &mpsc::Sender<FsEvent>,
    watch_subtree: bool,
    watched_dirs: &Arc<AtomicU64>,
    failed_dirs: &Arc<AtomicU64>,
) {
    // watch 被移除（目录被删 / 显式移除）：摘掉映射，不当成事件
    if ev.mask & ip::IN_IGNORED != 0 {
        map.remove(&ev.wd);
        watched_dirs.store(map.len() as u64, Ordering::Relaxed);
        return;
    }
    let Some(dir_abs) = map.get(&ev.wd).cloned() else {
        // 不认识的 wd：可能是刚补挂、还没登记（不该发生），跳过
        return;
    };

    // 目录被创建 / 移入 → **先补挂**再投递事件。
    // 顺序很重要：若先投递再补挂，调用方可能在事件处理中立刻往新目录里写文件，
    // 而那时 watch 还没挂上，那几条变更就永久丢了。
    if watch_subtree && ev.mask & ip::IN_ISDIR != 0 && !ev.name.is_empty() {
        let created = ev.mask & (ip::IN_CREATE | ip::IN_MOVED_TO) != 0;
        if created {
            let child = PathBuf::from(&dir_abs).join(&ev.name);
            let child_abs = core_io::paths::join(&dir_abs, &ev.name);
            match add_one(fd, &child, &child_abs, map) {
                Ok(_) => {
                    let f = add_tree(fd, &child, &child_abs, map, 0);
                    if f > 0 {
                        failed_dirs.fetch_add(f, Ordering::Relaxed);
                    }
                    watched_dirs.store(map.len() as u64, Ordering::Relaxed);
                }
                Err(_) => {
                    failed_dirs.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    let Some(action) = ip::action_of(ev.mask) else {
        return;
    };
    let dir_rel = ip::rel_of(root, &dir_abs);
    let rel = if ev.name.is_empty() {
        dir_rel
    } else if dir_rel.is_empty() {
        ev.name
    } else {
        format!("{dir_rel}{SEP}{}", ev.name)
    };

    shared.received.fetch_add(1, Ordering::Relaxed);
    if tx.send(FsEvent { action, rel }).is_err() {
        shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

// ── watch 管理 ──────────────────────────────────────────────────────────────

/// 给单个目录挂 watch，成功则登记 `wd → 绝对路径`。
fn add_one(fd: i32, path: &Path, abs: &str, map: &mut HashMap<i32, String>) -> io::Result<i32> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径里含 NUL 字节"))?;
    // SAFETY: `c` 以 NUL 结尾且存活到调用结束。
    let wd = unsafe { inotify_add_watch(fd, c.as_ptr(), WATCH_MASK) };
    if wd < 0 {
        return Err(io::Error::last_os_error());
    }
    map.insert(wd, abs.to_string());
    Ok(wd)
}

/// 递归给子树挂 watch。返回**失败**的目录数。
///
/// 不跟进符号链接（与遍历模式的 `skip_reparse` 同一口径）：一个指向上级的链接
/// 就能让这里递归到爆栈；更现实的问题是 watch 数量会被瞬间打满。
fn add_tree(fd: i32, dir: &Path, abs: &str, map: &mut HashMap<i32, String>, depth: u32) -> u64 {
    if depth > 256 {
        return 0;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut failed = 0u64;
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let child_abs = core_io::paths::join(abs, &name.to_string_lossy());
        match add_one(fd, &entry.path(), &child_abs, map) {
            Ok(_) => failed += add_tree(fd, &entry.path(), &child_abs, map, depth + 1),
            // ENOSPC = 达到 max_user_watches，继续尝试没有意义但也不致命
            Err(_) => failed += 1,
        }
    }
    failed
}
