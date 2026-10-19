//! # FileWatch CLI —— M0~M3 的主力验证工具
//!
//! 引擎（`crates/*`）全部是库，GUI 还没套上；在这个阶段，**命令行就是产品**。
//! 每一个里程碑的验收标准都能在这里找到对应的命令：
//!
//! | 命令 | 验收目标 | 文档章节 |
//! |---|---|---|
//! | `volumes` | 识别卷类型与可用后端 | §2.1 |
//! | `probe` | 引导扇区 / `$MFT` 记录 0 逐字段对照 `fsutil` | M0 |
//! | `journal` | USN Journal 查询 / 创建 / 删除 | M2 / R3 |
//! | `scan` | 全量扫描、Top-N、扩展名统计、面积图布局 | M1 / M4 预览 |
//! | `top` | 恒定内存找最大文件（不建树） | §5.1 |
//! | `monitor` | 实时增量 + 阈值预警 | M2 / M3 |
//! | `dupes` | 重复文件三级去重（体积 → 部分哈希 → 全哈希）+ 逐字节复核 | F12 |
//! | `snapshot` / `diff` | 快照落盘 与 「这一周谁变大了」 | F11 |
//! | `selftest` | 免管理员的全链路自检 | 全部 |
//!
//! ## 设计约束
//!
//! * **零第三方依赖**：网络受限的构建机上，`cargo build` 必须能过。
//!   所以参数解析、JSON 输出、表格对齐、Ctrl+C 处理全部自己写。
//! * **能力可降级但必须透明**：拿不到管理员权限时自动走目录遍历 +
//!   `ReadDirectoryChangesW`，但一定会把「为什么降级」「功能有什么差异」
//!   明确打印出来，而不是静默降级。

mod args;
mod render;
mod selftest;

use args::{parse_ms, parse_rule, parse_size, rule_label, Args};
use core_index::aggregate::{top_files, SizeMode};
use core_index::hardlink::build_report;
use core_index::tree::ChildIndex;
use core_index::{ExtensionTable, Tree, RECNO_MASK};
use core_io::hash::PARTIAL_BLOCK;
use core_io::util::{human_size, unix_ms, unix_secs};
use core_monitor::debounce::PendingEvent;
use core_monitor::rules::Alert;
use core_monitor::{PathRemap, Rule, RuleKind, TickOutcome};
use core_scan::{
    collect_candidates, find_duplicates, groups_to_csv, rows_to_csv, signed_size, DiffOptions,
    DiffOrder, DupOptions, DupProgress, DupReport, DupStage, FsSource, ScanBackend, ScanPhase,
    Session, SessionConfig, Snapshot, SnapshotOptions,
};
use core_usn::journal;
use core_viz::{compute_layout, LayoutOptions, LodPolicy, Rect, ShadingMode};
use render::{
    fmt_offset_ms, fmt_unix_stamp, fmt_unix_utc, pad_left, pad_right, render_ascii, render_legend,
    rule_line, truncate, truncate_keep_tail,
};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// FRN 低位掩码 → 节点下标（与引擎内部约定一致）。

// ─────────────────────────────────────────────────────────────────────────────
// 入口
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let a = Args::parse(std::env::args().skip(1));
    let code = match a.cmd.as_str() {
        "volumes" | "vols" | "vol" => cmd_volumes(&a),
        "probe" => cmd_probe(&a),
        "journal" | "usn" => cmd_journal(&a),
        "scan" => cmd_scan(&a),
        "top" => cmd_top(&a),
        "monitor" | "watch" => cmd_monitor(&a),
        "dupes" | "dup" => cmd_dupes(&a),
        "snapshot" | "snap" => cmd_snapshot(&a),
        "diff" => cmd_diff(&a),
        "selftest" | "self-test" | "check" => cmd_selftest(&a),
        "" | "help" | "-h" | "--help" => {
            print_help();
            0
        }
        "version" | "-v" | "--version" => {
            println!("filewatch {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH);
            0
        }
        other => {
            eprintln!("未知命令「{}」", other);
            eprintln!();
            print_help();
            2
        }
    };
    // 显式 exit：避免 DirWatcher 的后台线程把进程留住
    std::process::exit(code);
}

fn print_help() {
    println!(
        r#"FileWatch —— Windows NTFS 文件增量监控工具（引擎 + CLI）

用法：
  filewatch <命令> [参数...]

命令：
  volumes                     列出本机所有卷、文件系统与可用后端
  probe <盘符>                读引导扇区 + $MFT 记录 0，逐字段对照 fsutil（M0 验收）
  journal <盘符> [选项]       查询 / 创建 / 删除 USN Journal
  scan <盘符> [选项]          全量扫描并输出统计（M1 验收）
  top <盘符> [选项]           恒定内存找最大的 N 个文件（不建树）
  monitor <路径> [选项]       实时监控 + 阈值预警（M2/M3 验收）
  dupes <盘符|目录> [选项]    查找内容相同的重复文件（F12）
  snapshot <盘符|目录> [选项] 建立快照，供以后对比（F11）
  diff <旧快照> <新快照>      对比两次快照，看这段时间谁变大了（F11）
  selftest [--verbose]        免管理员的全链路自检
  help / version

选项通用形式：
  --key=value   或   --key value     （后者仅限已知需要取值的选项）
  --flag                             （布尔开关）

scan 选项：
  --top=N            输出前 N 大文件（默认 20，0 = 不输出）
  --exts=N           输出扩展名 Top N（默认 15，0 = 不输出）
  --dedup            输出硬链接 / 稀疏 / 压缩的口径差异说明
  --layout=WxH       用字符网格预览面积图布局（如 --layout=100x30）
  --root=PATH        指定扫描根（自动切换为目录遍历模式，见下）
  --json             以 JSON 输出摘要（便于脚本消费）
  --max-depth=N      目录遍历模式的最大深度（默认 256）
  --chunk=BYTES      顺序读块大小（默认 16M）
  --no-prealloc      关闭节点数组预分配（内存峰值更低，速度更慢）
  --watch-max-nodes=N  实时增量的路径索引上限（默认 300000）

monitor 选项：
  --rule=abs=1G          绝对体积阈值
  --rule=grow=500M       窗口内增长量阈值
  --rule=rate=10M/60s    速率阈值（可重复给出多条规则）
  --duration=60s         运行时长；0 或缺省 = 一直运行到 Ctrl+C
  --interval=500ms       轮询间隔（默认 500ms）
  --changelog=PATH       变更流水落盘（JSONL，64 MB 轮转）
  --create-journal       允许自动创建 USN Journal（系统级变更，R3）
  --force-traverse       完全不碰裸设备句柄，强制走目录遍历 + 目录监视
  --no-watch             降级模式下关闭目录监视（只做一次性扫描）
  --no-subtree           只监视根目录直接子项，不递归
  --tree=0               关闭每 10 秒一次的索引树状态行
  --watch-buffer=64K     目录监视缓冲区（突发变更多时调大，防丢事件）
  --watch-max-nodes=N    路径索引上限，超出则不启用实时增量（默认 300000）

journal 选项：
  --status / --create / --delete
  --journal-size=32M     创建时的最大容量
  --journal-delta=4M     创建时的分配增量

dupes 选项：
  --min-size=1M          小于这个体积的文件不参与（默认 0 = 只排除空文件）
  --top=N                最多显示多少组（默认 200）
  --block=4K             部分哈希读首尾各多少字节（默认 4K）
  --max-per-group=N      每组最多列出几个文件（默认 50，总量仍如实统计）
  --max-candidates=N     候选上限，防内存失控（默认 0 = 不限）
  --csv=FILE             把结果写成 CSV
  --json                 以 JSON 输出
  --force-traverse       不碰裸设备，强制走目录遍历

snapshot 选项：
  --out=FILE             输出路径（默认 snapshot-<UTC 时间戳>.tsv）
  --max-entries=N        最多记录多少个文件（默认 0 = 不限）
  --force-traverse       不碰裸设备，强制走目录遍历

diff 选项：
  --top=N                最多输出多少行（默认 50）
  --min-delta=1K         忽略增量小于此值的改动，默认 0
  --by-abs               按变化幅度排序（默认按增量降序，即"谁变大了"）
  --by-path              按路径排序
  --csv=FILE             把差异行写成 CSV

示例：
  filewatch volumes
  filewatch probe C:
  filewatch scan C: --top=10 --exts=10 --layout=110x32
  filewatch scan D: --root=D:\projects --json
  filewatch monitor D:\projects --rule=abs=1G --rule=rate=5M/1m --duration=5m
  filewatch dupes D:\photos --min-size=1M --csv=dupes.csv
  filewatch snapshot D:\projects --out=proj-1014.tsv
  filewatch diff proj-1007.tsv proj-1014.tsv --top=30
  filewatch selftest --verbose
"#
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// volumes
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_volumes(_a: &Args) -> i32 {
    let vols = core_io::list_volumes();
    println!("检测到 {} 个卷", vols.len());
    println!("{}", rule_line(96));
    println!(
        "{} {} {} {} {} {} {}",
        pad_right("盘符", 6),
        pad_right("类型", 12),
        pad_right("文件系统", 10),
        pad_right("卷标", 20),
        pad_left("总容量", 11),
        pad_left("可用", 11),
        pad_left("已用", 7),
    );
    println!("{}", rule_line(96));
    for v in &vols {
        println!(
            "{} {} {} {} {} {} {}",
            pad_right(&v.drive, 6),
            pad_right(v.type_label(), 12),
            pad_right(
                if v.fs_name.is_empty() { "-" } else { &v.fs_name },
                10
            ),
            pad_right(&truncate(&v.label, 18), 20),
            pad_left(&human_size(v.total_bytes), 11),
            pad_left(&human_size(v.free_bytes), 11),
            pad_left(&format!("{:.1}%", v.used_ratio() * 100.0), 7),
        );
        println!("       └ 后端提示：{}", v.mode_hint());
    }
    println!("{}", rule_line(96));
    println!(
        "当前进程是否具备管理员权限（能否打开 \\\\.\\C:）：{}",
        if core_io::util::is_elevated_hint() {
            "是 —— 可以使用 MFT 直读 + USN Journal（快速路径）"
        } else {
            "否 —— 将自动降级为目录遍历 + ReadDirectoryChangesW（功能完整，速度慢）"
        }
    );
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// probe
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_probe(a: &Args) -> i32 {
    let drive = match a.positional.first() {
        Some(d) => d.clone(),
        None => {
            eprintln!("用法：filewatch probe <盘符>   例如 filewatch probe C:");
            return 2;
        }
    };
    let (norm, _) = match core_io::normalize_drive(&drive) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("盘符「{}」无效：{}", drive, e);
            return 2;
        }
    };

    println!("正在读取 {} 的引导扇区与 $MFT 记录 0 ……", norm);
    let vol = match core_io::Volume::open(&norm) {
        Ok(v) => v,
        Err(e) => {
            print_access_error(&norm, &e);
            return 1;
        }
    };
    match core_scan::probe_volume(&vol) {
        Ok(rep) => {
            println!();
            println!("{}", rule_line(70));
            print!("{}", rep.render_cn(&norm));
            println!("{}", rule_line(70));
            println!();
            println!("请把上面的数字与下面命令的输出逐项对照（需要管理员权限）：");
            println!("  fsutil fsinfo ntfsinfo {}", norm);
            println!("  fsutil fsinfo sectorinfo {}", norm);
            println!();
            println!(
                "判定标准：每扇区字节数 / 每簇字节数 / MFT 起始簇号 / 每 MFT 记录字节数\n\
                 / 卷序列号 / $MFT 记录总数 —— 六项全部一致即为 M0 通过。"
            );
            0
        }
        Err(e) => {
            print_access_error(&norm, &e);
            1
        }
    }
}

fn print_access_error(drive: &str, e: &std::io::Error) {
    eprintln!("读取 {} 失败：{}", drive, e);
    if e.raw_os_error() == Some(5) {
        eprintln!();
        eprintln!("这是 Windows 的错误 5（拒绝访问）。");
        eprintln!("原因：直读 \\\\.\\{} 设备需要**管理员权限**。", drive);
        eprintln!();
        eprintln!("两种解决办法：");
        eprintln!("  1) 用「以管理员身份运行」打开终端，再执行本命令（推荐，性能最好）；");
        eprintln!("  2) 不提权也可以 —— 直接用 `filewatch scan {}` ，", drive);
        eprintln!("     程序会自动降级为目录遍历 + ReadDirectoryChangesW，功能不缺、速度慢些。");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// journal
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_journal(a: &Args) -> i32 {
    let drive = match a.positional.first() {
        Some(d) => d.clone(),
        None => {
            eprintln!("用法：filewatch journal <盘符> [--status|--create|--delete]");
            return 2;
        }
    };
    let (norm, _) = match core_io::normalize_drive(&drive) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("盘符无效：{}", e);
            return 2;
        }
    };
    let vol = match core_io::Volume::open(&norm) {
        Ok(v) => v,
        Err(e) => {
            print_access_error(&norm, &e);
            return 1;
        }
    };

    if a.has("create") {
        let max = a
            .get("journal-size")
            .and_then(args::parse_size)
            .unwrap_or(journal::DEFAULT_MAXIMUM_SIZE);
        let delta = a
            .get("journal-delta")
            .and_then(args::parse_size)
            .unwrap_or(journal::DEFAULT_ALLOCATION_DELTA);
        println!(
            "将在 {} 上创建 USN Journal（最大 {}，分配增量 {}）……",
            norm,
            human_size(max),
            human_size(delta)
        );
        println!("注意：这是**系统级变更**，会影响该卷的所有应用；");
        println!("      退出监控后可用 `filewatch journal {} --delete` 恢复。", norm);
        match journal::create(&vol, max, delta) {
            Ok(()) => {
                println!("创建成功。");
                match journal::query(&vol) {
                    Ok(d) => {
                        print_journal(&norm, &d);
                        0
                    }
                    Err(e) => {
                        eprintln!("创建后查询失败：{}", e);
                        1
                    }
                }
            }
            Err(e) => {
                eprintln!("创建失败：{}", e);
                1
            }
        }
    } else if a.has("delete") {
        match journal::query(&vol) {
            Ok(d) => {
                println!(
                    "将删除 {} 上的 USN Journal（JournalID={}，{}）",
                    norm,
                    d.id,
                    human_size(d.maximum_size)
                );
                match journal::delete(&vol, d.id) {
                    Ok(()) => {
                        println!("已删除。");
                        0
                    }
                    Err(e) => {
                        eprintln!("删除失败：{}", e);
                        1
                    }
                }
            }
            Err(e) => {
                eprintln!("删除前必须先拿到 JournalID，但查询失败：{}", e);
                1
            }
        }
    } else {
        // 默认 = status
        match journal::query(&vol) {
            Ok(d) => {
                print_journal(&norm, &d);
                let cur = core_usn::Cursor::load(&norm);
                let v = core_usn::evaluate(cur.as_ref(), &d);
                println!();
                println!("本地游标：{}", core_usn::cursor::data_dir().display());
                match &cur {
                    Some(c) => println!(
                        "  JournalID={} NextUsn={}（保存于磁盘）",
                        c.journal_id, c.next_usn
                    ),
                    None => println!("  （尚未保存过游标）"),
                }
                println!("  可续跑判定：{} —— {}", v.can_resume(), v.reason_cn());
                0
            }
            Err(e) => {
                if e.raw_os_error() == Some(1179) {
                    println!("{} 上没有活动中的 USN Journal（错误 1179）。", norm);
                    println!("可用 `filewatch journal {} --create` 启用。", norm);
                    0
                } else {
                    eprintln!("查询失败：{}", e);
                    1
                }
            }
        }
    }
}

fn print_journal(drive: &str, d: &core_usn::UsnJournalData) {
    println!("{}", rule_line(60));
    println!("盘符              : {}", drive);
    println!("JournalID         : {}", d.id);
    println!("FirstUsn          : {}", d.first_usn);
    println!("NextUsn           : {}", d.next_usn);
    println!("LowestValidUsn    : {}", d.lowest_valid_usn);
    println!("MaxUsn            : {}", d.max_usn);
    println!("MaximumSize       : {}", human_size(d.maximum_size));
    println!("AllocationDelta   : {}", human_size(d.allocation_delta));
    println!("剩余可写空间       : {:.1}%", d.free_ratio() * 100.0);
    println!("{}", rule_line(60));
}

// ─────────────────────────────────────────────────────────────────────────────
// scan
// ─────────────────────────────────────────────────────────────────────────────

fn build_scan_config(a: &Args, drive: &str, watch: bool) -> Result<SessionConfig, String> {
    let mut cfg = SessionConfig::default();
    cfg.drive = drive.to_string();
    cfg.scan_root = a.path("root");
    cfg.allow_create_journal = a.has("create-journal");
    cfg.allow_traverse_watch = watch;
    // `--root` 与 MFT 直读互斥：$MFT 是整卷粒度的，给定子目录就只能走遍历。
    // 自动切换而不是报错，免得用户对着「--root 被忽略」这种沉默行为踩坑。
    cfg.force_traverse = a.has("force-traverse") || cfg.scan_root.is_some();
    cfg.changelog_path = a.path("changelog");
    cfg.engine.write_changelog = cfg.changelog_path.is_some();
    cfg.engine.collect_events = true;
    cfg.watch_max_nodes = a.num("watch-max-nodes", 300_000)?;
    cfg.watch_buffer_bytes = a
        .get("watch-buffer")
        .and_then(args::parse_size)
        .map(|b| b as usize)
        .unwrap_or(64 * 1024);
    cfg.watch_subtree = !a.has("no-subtree");
    cfg.traverse.max_depth = a.num("max-depth", 256)? as u32;
    if let Some(c) = a.get("chunk").and_then(args::parse_size) {
        cfg.scan.chunk_bytes = c as usize;
    }
    cfg.scan.preallocate = !a.has("no-prealloc");
    if let Some(i) = a.get("interval").and_then(parse_ms) {
        cfg.engine.poll_interval_ms = i;
    }
    Ok(cfg)
}

fn cmd_scan(a: &Args) -> i32 {
    let drive = a.positional.first().cloned().unwrap_or_else(|| "C:".to_string());
    let (norm, _) = match core_io::normalize_drive(&drive) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("盘符「{}」无效：{}", drive, e);
            return 2;
        }
    };

    // ── 先校验**全部**参数，再开始扫描 ──────────────────────────────────
    // 否则一个 `--top=abc` 会先让人等十分钟扫完整个盘，再报参数错误。
    let top_n = match a.num("top", 20) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let exts_n = match a.num("exts", 15) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let layout = match a.get("layout") {
        Some(spec) => match parse_layout_spec(spec) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("--layout 参数错误：{}", e);
                return 2;
            }
        },
        None => None,
    };
    let cfg = match build_scan_config(a, &norm, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };

    println!(
        "开始扫描 {}（根：{}）",
        norm,
        cfg.scan_root
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("{}\\", norm))
    );
    let t0 = Instant::now();
    let session = match Session::open(cfg, progress_to_stderr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!();
            eprintln!("扫描失败：{}", e);
            if e.raw_os_error() == Some(5) {
                print_access_error(&norm, &e);
            }
            return 1;
        }
    };
    // 清掉进度行
    eprintln!();
    let wall = t0.elapsed();

    for w in &session.warnings {
        println!("· {}", w);
    }
    println!();
    println!("{}", session.status_line());
    println!("总耗时：{} ms", wall.as_millis());

    if let Some(st) = &session.scan_stats {
        println!("{}", st.summary_cn());
    }
    if let Some(st) = &session.traverse_stats {
        println!("{}", st.summary_cn());
        if st.errors > 0 {
            println!(
                "  （{} 个条目因权限/占用/竞态被跳过——这是遍历模式下的正常现象）",
                st.errors
            );
        }
    }

    let remap = path_remap_of(&session.cfg);

    if a.has("json") {
        println!();
        println!(
            "{}",
            json_summary(&session, top_n, wall.as_millis(), remap.as_ref())
        );
        return 0;
    }

    if top_n > 0 {
        print_top_files(&session.tree, top_n, remap.as_ref());
    }
    if exts_n > 0 {
        print_exts(&session.tree, exts_n);
    }
    if a.has("dedup") {
        print_dedup(&session.tree);
    }
    if let Some((w, h)) = layout {
        print_layout(&session.tree, w, h);
    }
    0
}

/// 进度回调：写 stderr，避免污染 stdout 的机器可读输出。
fn progress_to_stderr(phase: ScanPhase, done: u64, total: u64) {
    let mut e = std::io::stderr();
    let tag = match phase {
        ScanPhase::Mft => "MFT 直读",
        ScanPhase::Traverse => "目录遍历",
    };
    if total > 0 {
        let pct = done as f64 / total as f64 * 100.0;
        let _ = write!(
            e,
            "\r  {} [{:>5.1}%] {}/{} 条记录          ",
            tag, pct, done, total
        );
    } else {
        let _ = write!(e, "\r  {} 已遍历 {} 个节点          ", tag, done);
    }
    let _ = e.flush();
}

fn print_top_files(tree: &Tree, n: usize, remap: Option<&PathRemap>) {
    let idx = top_files(tree, n, SizeMode::Logical);
    println!();
    println!("前 {} 大文件（按逻辑大小）", idx.len());
    println!("{}", rule_line(100));
    for (i, &node) in idx.iter().enumerate() {
        let (size, alloc) = tree.size_of(node).unwrap_or((0, 0));
        let raw = tree.path_of(node);
        let p = match remap {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        println!(
            "{:>3}. {}  {}  {}",
            i + 1,
            pad_left(&human_size(size), 10),
            pad_left(&format!("(占用 {})", human_size(alloc)), 18),
            truncate_keep_tail(&p, 60)
        );
    }
    println!("{}", rule_line(100));
    if tree.hardlink_records > 0 {
        println!(
            "注：卷上存在 {} 条硬链接记录，同一份数据可能被多个路径重复计入。",
            tree.hardlink_records
        );
    }
}

fn print_exts(tree: &Tree, n: usize) {
    let table = ExtensionTable::build(tree);
    let (top, rest) = table.top(n, SizeMode::Logical);
    println!();
    println!("扩展名 Top {}（按逻辑大小）", top.len());
    println!("{}", rule_line(84));
    println!(
        "{} {} {} {} {}",
        pad_right("扩展名", 14),
        pad_left("文件数", 10),
        pad_left("逻辑大小", 13),
        pad_left("占用大小", 13),
        pad_left("占比", 8)
    );
    println!("{}", rule_line(84));
    for s in &top {
        let id = table.id_of_ext(&s.ext).unwrap_or(0);
        println!(
            "{} {} {} {} {}",
            pad_right(&format!(".{}", s.display_ext()), 14),
            pad_left(&s.count.to_string(), 10),
            pad_left(&human_size(s.size), 13),
            pad_left(&human_size(s.alloc), 13),
            pad_left(&format!("{:.2}%", table.share(id, SizeMode::Logical) * 100.0), 8),
        );
    }
    if rest > 0 {
        println!(
            "{} {}",
            pad_right("（其他）", 14),
            pad_left(&human_size(rest), 10)
        );
    }
    println!("{}", rule_line(84));
    println!(
        "合计 {} 个文件 / {}，涉及 {} 种扩展名",
        table.total_count,
        human_size(table.total_size),
        table.len()
    );
}

fn print_dedup(tree: &Tree) {
    let rep = build_report(tree);
    println!();
    println!("口径差异说明（解释「为什么我算出来的大小和资源管理器不一样」）");
    println!("{}", rule_line(84));
    for line in rep.explain_lines() {
        println!("  {}", line);
    }
    println!("{}", rule_line(84));
}

fn parse_layout_spec(s: &str) -> Result<(usize, usize), String> {
    let (w, h) = s
        .split_once(['x', 'X', '*'])
        .ok_or_else(|| format!("格式应为 宽x高，例如 100x30，收到「{}」", s))?;
    let w: usize = w.trim().parse().map_err(|_| format!("宽度「{}」不是数字", w))?;
    let h: usize = h.trim().parse().map_err(|_| format!("高度「{}」不是数字", h))?;
    if w < 20 || h < 8 {
        return Err("画布太小，建议至少 40x12".to_string());
    }
    Ok((w.min(400), h.min(200)))
}

fn print_layout(tree: &Tree, w: usize, h: usize) {
    let children = ChildIndex::build(tree);
    let ext_id = ExtensionTable::build(tree);
    let viewport = Rect::new(0.0, 0.0, w as f32, h as f32);
    let opts = LayoutOptions {
        policy: LodPolicy::wide(),
        mode: SizeMode::Logical,
        shading: ShadingMode::ByExtension,
        now_unix: unix_secs(),
    };
    let res = compute_layout(tree, &children, &ext_id, tree.root, viewport, &opts);
    let (total, area, worst) = render::layout_audit(&res, &viewport);

    println!();
    println!("面积图布局预览（Squarified Treemap，{}×{} 字符）", w, h);
    println!("{}", rule_line(w));
    print!("{}", render_ascii(&res, w, h));
    println!("{}", rule_line(w));
    println!("{}", res.stats.summary_cn());
    println!("{}", render_legend(&res));
    println!(
        "矩形总面积 {:.0} / 画布 {:.0}（比值 {:.2}），同层最大重叠 {:.2}（Squarified 为严格分区，应为 0）",
        total,
        area,
        total / area.max(1.0),
        worst
    );
    println!(
        "说明：字符网格是 M4 的可视化预演——同一份 `LayoutResult` 交给 Canvas/WebGL 就是正式界面。"
    );
}

fn json_summary(s: &Session, top_n: usize, wall_ms: u128, remap: Option<&PathRemap>) -> String {
    use core_io::jsonlite::quote;
    let mut o = String::from("{");
    o.push_str(&format!("\"drive\":{},", quote(&s.cfg.drive)));
    o.push_str(&format!(
        "\"backend\":{},",
        quote(match s.backend {
            ScanBackend::Mft => "mft",
            ScanBackend::Traverse => "traverse",
        })
    ));
    o.push_str(&format!("\"files\":{},", s.tree.file_count));
    o.push_str(&format!("\"dirs\":{},", s.tree.dir_count));
    o.push_str(&format!("\"logical\":{},", s.tree.total_size));
    o.push_str(&format!("\"allocated\":{},", s.tree.total_alloc));
    o.push_str(&format!("\"memory_bytes\":{},", s.tree.memory_bytes()));
    o.push_str(&format!("\"hardlinks\":{},", s.tree.hardlink_records));
    o.push_str(&format!("\"orphans\":{}", s.tree.skipped_records));
    if let Some(st) = &s.scan_stats {
        o.push_str(&format!(
            ",\"mft\":{{\"slots\":{},\"parsed\":{},\"skipped\":{},\"corrupt\":{},\"bytes_read\":{},\"elapsed_ms\":{}}}",
            st.slots_seen, st.records_parsed, st.records_skipped, st.records_corrupt,
            st.bytes_read, st.elapsed_ms
        ));
    }
    if let Some(st) = &s.traverse_stats {
        o.push_str(&format!(
            ",\"traverse\":{{\"dirs\":{},\"files\":{},\"errors\":{},\"elapsed_ms\":{}}}",
            st.dirs_visited, st.files_visited, st.errors, st.elapsed_ms
        ));
    }
    o.push_str(&format!(",\"wall_ms\":{}", wall_ms));

    if top_n > 0 {
        let idx = top_files(&s.tree, top_n, SizeMode::Logical);
        let arr: Vec<String> = idx
            .iter()
            .map(|&n| {
                let (size, alloc) = s.tree.size_of(n).unwrap_or((0, 0));
                let raw = s.tree.path_of(n);
                let p = match remap {
                    Some(r) => r.apply(&raw),
                    None => raw,
                };
                format!(
                    "{{\"size\":{},\"alloc\":{},\"path\":{}}}",
                    size,
                    alloc,
                    quote(&p)
                )
            })
            .collect();
        o.push_str(&format!(",\"top\":[{}]", arr.join(",")));
    }
    o.push('}');
    o
}

// ─────────────────────────────────────────────────────────────────────────────
// top（恒定内存，不建树）
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_top(a: &Args) -> i32 {
    let drive = a.positional.first().cloned().unwrap_or_else(|| "C:".to_string());
    let (norm, _) = match core_io::normalize_drive(&drive) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("盘符无效：{}", e);
            return 2;
        }
    };
    let n = match a.num("n", 20) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let chunk = a
        .get("chunk")
        .and_then(args::parse_size)
        .unwrap_or(core_io::MFT_READ_CHUNK as u64) as usize;

    let vol = match core_io::Volume::open(&norm) {
        Ok(v) => v,
        Err(e) => {
            print_access_error(&norm, &e);
            return 1;
        }
    };
    let ntfs = match core_ntfs::NtfsVolume::mount(&vol) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("NTFS 挂载失败：{}", e);
            return 1;
        }
    };

    println!(
        "在 {} 上找最大的 {} 个文件（恒定内存，不构建整棵树）……",
        norm, n
    );
    let t0 = Instant::now();
    let items = match core_scan::top_n_files_via_mft(&vol, &ntfs, n, chunk) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("扫描失败：{}", e);
            return 1;
        }
    };
    let ms = t0.elapsed().as_millis();

    println!(
        "扫描 {} 条记录槽位，耗时 {} ms",
        ntfs.record_count, ms
    );
    println!("{}", rule_line(96));
    println!(
        "{} {} {} {}",
        pad_right("排名", 6),
        pad_left("逻辑大小", 12),
        pad_left("MFT 记录号", 12),
        pad_right("文件名", 24)
    );
    println!("{}", rule_line(96));
    for (i, (size, frn, _parent, name)) in items.iter().enumerate() {
        println!(
            "{} {} {} {}",
            pad_right(&format!("{}", i + 1), 6),
            pad_left(&human_size(*size), 12),
            pad_left(&format!("{}", frn & RECNO_MASK), 12),
            pad_right(&truncate_keep_tail(name, 22), 24),
        );
    }
    println!("{}", rule_line(96));
    println!(
        "提示：此模式只读 $MFT，不解析父链，因此只有文件名没有完整路径。\n\
         需要完整路径请用 `filewatch scan {} --root={}`。",
        norm, norm
    );
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// monitor
// ─────────────────────────────────────────────────────────────────────────────

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "system" fn on_ctrl_event(_t: u32) -> i32 {
    // 返回 1 表示「已处理」：系统不会立刻终止进程，
    // 我们有机会把游标与变更流水刷完再退出。
    INTERRUPTED.store(true, Ordering::SeqCst);
    1
}

/// 降级模式下 `Tree` 里的路径前缀是**盘符**（合成路径），
/// 而真实扫描根可能是一个深目录。显示时把前缀换回真实根，
/// 否则用户会看到一堆并不存在的 `C:\incoming\...`。
///
/// 实现放在 `core_monitor::PathRemap`：引擎写变更流水时也要用同一套替换，
/// 各写一份迟早会漂移（曾经就是这样 —— 屏幕换对了、日志里还是旧前缀）。
fn path_remap_of(cfg: &SessionConfig) -> Option<PathRemap> {
    let root = cfg.scan_root.as_ref()?;
    PathRemap::new(&cfg.drive, &root.to_string_lossy())
}

/// 变更流水被写进**被监视目录内部**时给出提示。
///
/// 后果有两层：日志里会多一条「自己诞生」的记录（无害），更麻烦的是
/// **日志自身的增长会被计进被监视目录的体积** —— 用户什么都没改，却看到
/// `abs` / `grow` 规则被顶穿。这不是能靠代码消除的（用户就是想放那儿），
/// 但必须说清楚，否则会变成一条查不明白的误报。
///
/// 监视根是**盘符**时返回 `None`：整盘监视下「日志在盘内」是必然的，
/// 提示只会变成噪音，而且那份日志相对整盘体积可以忽略。
fn warn_if_changelog_inside(root: &str, changelog: Option<&str>) -> Option<String> {
    let log = changelog?;
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    let r = norm(root);
    let l = norm(log);
    if r.len() <= 2 {
        return None; // 裸盘符
    }
    if l.starts_with(&format!("{}\\", r)) {
        Some(format!(
            "变更流水写在被监视目录内部（{}）—— 日志自身的增长会计入该目录体积，\
             可能顶穿阈值规则造成误报。建议把 --changelog 指到监视范围之外。",
            log
        ))
    } else {
        None
    }
}

fn cmd_monitor(a: &Args) -> i32 {
    let raw = match a.positional.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("用法：filewatch monitor <路径> [--rule=abs=1G] [--duration=60s]");
            eprintln!("  路径可以是盘符（C:）或目录（D:\\projects）");
            return 2;
        }
    };
    // 「这个字符串指的是整卷还是某个子目录」只有一处解释（见 `split_target`）。
    let (drive, scan_root) = match split_target(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let raw = raw.replace('/', "\\");

    let mut cfg = match build_scan_config(a, &drive, !a.has("no-watch")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };
    cfg.scan_root = scan_root.clone();
    cfg.engine.collect_events = true;
    if a.has("no-journal") || a.has("force-traverse") {
        // 完全不碰裸设备句柄，强制走目录遍历 + ReadDirectoryChangesW。
        // 用于在没有管理员权限的机器上验证降级链路。
        cfg.force_traverse = true;
        cfg.allow_create_journal = false;
    } else if cfg.scan_root.is_some() {
        // 监视子目录只能用遍历后端（$MFT 是整卷粒度）
        cfg.force_traverse = true;
    } else {
        cfg.allow_create_journal = a.has("create-journal");
    }

    let rules: Vec<Rule> = {
        let mut v = Vec::new();
        for s in a.get_all("rule") {
            match parse_rule(s) {
                Ok(r) => v.push(r),
                Err(e) => {
                    eprintln!("规则错误：{}", e);
                    return 2;
                }
            }
        }
        v
    };
    let duration = a.get("duration").and_then(parse_ms).unwrap_or(0);
    let interval = cfg.engine.poll_interval_ms;
    let show_status = a.get("tree").map(|v| v != "0").unwrap_or(true);

    println!("═══ FileWatch 监控 ═══");
    println!("开始时间 : {}", fmt_unix_utc(unix_secs()));
    println!("监视路径 : {}", raw);
    println!(
        "规则     : {}",
        if rules.is_empty() {
            "（未设置，只记录变更不预警）".to_string()
        } else {
            rules.iter().map(rule_label).collect::<Vec<_>>().join("；")
        }
    );
    println!(
        "时长     : {}",
        if duration == 0 {
            "直到 Ctrl+C".to_string()
        } else {
            format!("{} ms", duration)
        }
    );
    println!("轮询间隔 : {} ms", interval);
    println!();

    let t0 = Instant::now();
    let mut session = match Session::open(cfg, progress_to_stderr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!();
            eprintln!("会话建立失败：{}", e);
            return 1;
        }
    };
    eprintln!();

    for w in &session.warnings {
        println!("· {}", w);
    }
    // 变更流水写进被监视目录内部是个容易踩的坑，单独提示
    if let Some(w) = warn_if_changelog_inside(&raw, a.get("changelog")) {
        println!("⚠ {}", w);
    }
    println!();
    println!("{}", session.status_line());
    println!("增量来源：{}", session.incremental_source_cn());

    // 注册监控目标
    if rules.is_empty() {
        // 没有规则也要注册目标？不需要——但保留一条提示
        println!();
        println!("提示：未提供 --rule，本次只输出变更日志，不做阈值预警。");
    } else {
        let mut targets: Vec<String> = Vec::new();
        // 整卷监视时要把目标写成 `C:\`（带尾反斜杠）—— `C:` 在 Windows 上的
        // 语义是「C 盘的当前目录」，不是同一个东西（设计规则第 10 条）。
        if scan_root.is_none() {
            targets.push(format!("{}\\", drive));
        } else {
            targets.push(raw.clone());
        }
        for extra in a.positional.iter().skip(1) {
            targets.push(extra.clone());
        }
        println!();
        for t in &targets {
            match session.add_target_by_path(t, rules.clone(), 0) {
                Some((node, _)) => {
                    let cur = session.tree.size_of(node).unwrap_or((0, 0)).0;
                    println!(
                        "已注册监控目标：{}（节点 {}，当前 {}）",
                        t,
                        node,
                        human_size(cur)
                    );
                }
                None => {
                    println!(
                        "⚠ 无法注册监控目标：{} —— 索引树里找不到该路径（可能不在扫描范围内）",
                        t
                    );
                }
            }
        }
    }

    // 注册 Ctrl+C
    let mut interrupted = false;
    unsafe {
        core_io::winapi::SetConsoleCtrlHandler(Some(on_ctrl_event), 1);
    }

    println!();
    println!("{}", rule_line(100));
    println!("开始监听变更……（Ctrl+C 结束）");

    let mut last_status = Instant::now();
    let mut total_events = 0usize;
    let mut total_alerts = 0usize;
    let remap = path_remap_of(&session.cfg);

    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        if duration > 0 && elapsed_ms >= duration {
            break;
        }
        let now = unix_ms() + elapsed_ms;

        match session.poll(now) {
            Ok(out) => {
                total_events += out.applied;
                total_alerts += out.alerts.len();
                render_tick(&out, &session.tree, elapsed_ms, remap.as_ref());
            }
            Err(e) => {
                eprintln!("[{:>9}] 轮询出错：{}", fmt_offset_ms(elapsed_ms), e);
            }
        }

        if show_status && last_status.elapsed() >= Duration::from_secs(10) {
            last_status = Instant::now();
            println!("[{:>9}] {}", fmt_offset_ms(elapsed_ms), session.status_line());
        }

        std::thread::sleep(Duration::from_millis(interval.max(20)));
    }

    // 收尾
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    match session.shutdown(unix_ms() + elapsed_ms) {
        Ok(out) => {
            total_events += out.applied;
            total_alerts += out.alerts.len();
            render_tick(&out, &session.tree, elapsed_ms, remap.as_ref());
        }
        Err(e) => eprintln!("收尾刷盘出错：{}", e),
    }

    println!("{}", rule_line(100));
    println!(
        "{} 监控结束（{}）",
        if interrupted { "已中断：" } else { "正常退出：" },
        fmt_offset_ms(elapsed_ms)
    );
    println!("{}", session.status_line());
    println!(
        "累计：应用变更 {} 条，触发预警 {} 次，内存中 {} 个节点",
        total_events,
        total_alerts,
        session.tree.arena.used()
    );
    println!("增量来源：{}", session.incremental_source_cn());
    if session.journal_created {
        println!();
        println!("注意：本次为卷启用了 USN Journal。如需恢复系统原状：");
        println!("  filewatch journal {} --delete", drive);
    }
    0
}

fn render_tick(out: &TickOutcome, tree: &Tree, elapsed_ms: u64, remap: Option<&PathRemap>) {
    for ev in &out.events {
        println!(
            "[{:>9}] {}",
            fmt_offset_ms(elapsed_ms),
            render_event(ev, tree, remap)
        );
    }
    if let Some(n) = &out.notification {
        println!(
            "[{:>9}] {}",
            fmt_offset_ms(elapsed_ms),
            core_monitor::alert::render_one_line(n)
        );
    }
    for al in &out.alerts {
        print!("{}", render_alert(al, elapsed_ms));
    }
    if out.backpressure {
        println!(
            "[{:>9}] ⚠ 背压：本轮事件过多，部分已丢弃（建议缩小监视范围或调大队列）",
            fmt_offset_ms(elapsed_ms)
        );
    }
}

fn render_event(ev: &PendingEvent, tree: &Tree, remap: Option<&PathRemap>) -> String {
    // 路径构造走 core_scan::paths、体积说法走 core_monitor —— 界面用的是同一份。
    // 「显示哪个数」不是排版问题：同一条变更在终端与界面上数字不一致的话，
    // 用户没法判断该信哪个。
    let path = core_scan::paths::event_path(tree, ev, remap);
    let size = core_monitor::event_size_text(ev);
    let suffix = match core_monitor::event_suffix(ev) {
        Some(s) => format!("（{}）", s),
        None => String::new(),
    };
    format!(
        "{} {}  {}{}",
        pad_right(ev.kind.label_cn(), 10),
        pad_left(&size, 11),
        truncate_keep_tail(&path, 68),
        suffix
    )
}

/// 渲染一条预警明细行。
///
/// **不套 [`PathRemap`]**：`Alert.path` 是注册监控目标时给的路径，本来就是
/// 真实路径（用户敲的那一条），不是 `Tree::path_of` 拼出来的合成路径。
/// 早先这里也套了一层替换，后果是把真实路径又当成合成路径改了一遍 ——
/// `C:\...\fw_livecheck2` 变成 `C:\...\fw_livecheck2\Users\...`，
/// 而且因为只出错在「路径恰好以盘根开头」的情形，一直没被发现。
fn render_alert(a: &Alert, elapsed_ms: u64) -> String {
    format!(
        "[{:>9}] ⚠ 预警 {}｜{}｜当前 {} / 基线 {}（超出 +{}）｜{}\n",
        fmt_offset_ms(elapsed_ms),
        a.kind.label_cn(),
        match a.kind {
            RuleKind::Absolute => format!("阈值 {}", human_size(a.threshold)),
            RuleKind::Growth => format!("增量阈值 {}", human_size(a.threshold)),
            RuleKind::Rate => format!("速率阈值 {}/s", human_size(a.threshold)),
        },
        human_size(a.current_size),
        human_size(a.baseline_size),
        human_size(a.observed),
        truncate_keep_tail(&a.path, 60)
    )
}

/// 位置参数 → `(盘符, 扫描根)`。
///
/// * `C:` / `D:\` → `("C:", None)`：整卷，可以走 `$MFT` 直读（快得多）
/// * `D:\projects` → `("D:", Some(D:\projects))`：子目录，只能走遍历
///
/// 这是「用户给的那个字符串到底指什么」的**唯一**解释处：`monitor` / `dupes` /
/// `snapshot` 都用它。各写一份的话，迟早出现「monitor 认 `C:` 是整卷、
/// dupes 却当成子目录」这种谁都没注意到的分叉。
///
/// 顺手把 `/` 归一成 `\`：调用方的 shell 未必守规矩（实测 git-bash 会把
/// `D:\x` 传成 `D:/x`），不归一后面每一处显示都要防一次。
fn split_target(raw: &str) -> Result<(String, Option<std::path::PathBuf>), String> {
    let norm = raw.replace('/', "\\");
    let trimmed = norm.trim();
    if trimmed.is_empty() {
        return Err("路径为空".to_string());
    }
    // 盘符必须**显式**给出。这里的旧实现是一个「从任意字符里找 C~Z 后面的冒号、
    // 找不到就兜底成 C:」的函数 —— 那个兜底会把 `/home/user` 静默地变成
    // 「扫描整个 C 盘」。宁可报错。
    let bytes: Vec<char> = trimmed.chars().collect();
    let letter = match bytes.first() {
        Some(c) if c.is_ascii_alphabetic() => *c,
        _ => {
            return Err(format!(
                "无法从「{raw}」认出盘符：应当形如 C: 或 D:\\目录（本工具只支持 Windows 盘符路径）"
            ))
        }
    };
    if bytes.get(1) != Some(&':') {
        return Err(format!(
            "无法从「{raw}」认出盘符：{} 后面应当紧跟冒号，例如 {}:\\",
            letter, letter
        ));
    }
    let (drive, _) = core_io::normalize_drive(&format!("{letter}:"))
        .map_err(|e| format!("盘符无效：{e}"))?;
    let is_bare = trimmed.len() <= 3; // `C:` 或 `C:\`
    let scan_root = if is_bare {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    };
    Ok((drive, scan_root))
}

// ─────────────────────────────────────────────────────────────────────────────
// dupes（F12：重复文件三级去重）
// ─────────────────────────────────────────────────────────────────────────────

/// 进度节流间隔。哈希可能要读几十 GB，每读一个文件刷一次行会把终端写崩。
const DUP_PROGRESS_GAP_MS: u64 = 150;

fn dup_options(a: &Args) -> Result<DupOptions, String> {
    let mut o = DupOptions::default();
    o.min_size = match a.get("min-size") {
        Some(s) => parse_size(s).ok_or_else(|| format!("--min-size 需要体积，如 1M / 512K，收到「{s}」"))?,
        None => 0,
    };
    o.block = match a.get("block") {
        Some(s) => {
            let v = parse_size(s).ok_or_else(|| format!("--block 需要体积，如 4K，收到「{s}」"))?;
            // 0 会让部分哈希退化成"什么都不读"，那时所有同体积文件都要走整文件哈希
            (v.max(1)) as usize
        }
        None => PARTIAL_BLOCK,
    };
    o.max_per_group = a.num("max-per-group", 50)?;
    o.max_groups = a.num("top", 200)?;
    o.max_candidates = a.num("max-candidates", 0)?;
    Ok(o)
}

fn cmd_dupes(a: &Args) -> i32 {
    let raw = match a.positional.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("用法：filewatch dupes <盘符|目录> [--min-size=1M] [--top=50] [--csv=FILE] [--json]");
            eprintln!("  盘符（C:）走 $MFT 直读，需要管理员权限；目录走遍历，普通权限即可");
            eprintln!("  示例：filewatch dupes D:\\photos --min-size=1M");
            return 2;
        }
    };
    let (drive, scan_root) = match split_target(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let opts = match dup_options(a) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };

    let mut cfg = match build_scan_config(a, &drive, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };
    // 子目录只能走遍历（$MFT 是整卷粒度的）；整卷则可以直读 MFT ——
    // 这正是去重最需要的能力：先拿到全部文件的体积，才能把"体积唯一"的文件
    // 直接排除掉，一个字节都不用读。
    let sub = scan_root.is_some();
    cfg.force_traverse = a.has("force-traverse") || cfg.scan_root.is_some() || sub;
    cfg.allow_create_journal = a.has("create-journal");
    cfg.scan_root = scan_root.clone();
    // 去重会自己去读文件内容，不需要监控引擎收集事件
    cfg.engine.collect_events = false;
    cfg.allow_traverse_watch = false;

    println!("═══ FileWatch 重复文件查找 ═══");
    // 显示时统一成反斜杠：调用方的 shell 可能传进来 `D:/photos`，
    // 而下面每一行都跟着它走（只有一处归一，别让用户看到两种写法）。
    println!("查找范围 : {}", raw.replace('/', "\\"));
    println!(
        "最小体积 : {}",
        if opts.min_size == 0 {
            "1 字节（0 字节文件不参与）".to_string()
        } else {
            human_size(opts.min_size)
        }
    );
    println!();
    println!("① 建立文件清单…");

    let t0 = Instant::now();
    let session = match Session::open(cfg, progress_to_stderr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!();
            eprintln!("扫描失败：{}", e);
            if e.raw_os_error() == Some(5) {
                print_access_error(&drive, &e);
            }
            return 1;
        }
    };
    eprintln!();
    for w in &session.warnings {
        println!("· {}", w);
    }
    let remap = session.path_remap();
    let set = collect_candidates(&session.tree, remap.as_ref(), &opts);
    println!(
        "   候选 {} 个文件（排除：{} 个小于下限、{} 个空文件、{} 个符号链接{}）",
        set.items.len(),
        set.skipped_small,
        set.skipped_empty,
        set.skipped_reparse,
        if set.truncated > 0 {
            format!("；因超过 --max-candidates 截断 {} 个", set.truncated)
        } else {
            String::new()
        }
    );
    println!("   清单耗时 {} ms", t0.elapsed().as_millis());

    // ── 三级筛选（读盘部分） ──
    println!();
    println!("② 体积 → 部分哈希 → 完整哈希 → 逐字节复核");
    let mut last = Instant::now();
    let mut last_stage: Option<DupStage> = None;
    let rep = find_duplicates(&set.items, &opts, &FsSource, &mut |p: DupProgress| {
        let now = Instant::now();
        let stage_changed = last_stage != Some(p.stage);
        // 每一级的**首行**与**走满那一行**都要打：否则快速跑完时终端上只会留下
        // 「部分哈希 14.3% 1/7」这种像是卡住了的残影。
        let finished = p.done == p.total;
        if !stage_changed && !finished && now.duration_since(last).as_millis() < DUP_PROGRESS_GAP_MS as u128 {
            return;
        }
        last = now;
        if stage_changed {
            last_stage = Some(p.stage);
            eprintln!();
        }
        let mut e = std::io::stderr();
        let _ = write!(
            e,
            "\r   {} {:>5.1}%  {}/{}          ",
            p.stage.label_cn(),
            if p.total == 0 { 100.0 } else { p.done as f64 / p.total as f64 * 100.0 },
            p.done,
            p.total
        );
        let _ = e.flush();
    });
    eprintln!();

    // ── 输出 ──
    println!();
    println!("{}", core_scan::dedup::summary_cn(&rep));
    println!(
        "   实际读取 {}（只读候选文件；体积唯一的文件一个字节都没读）",
        human_size(rep.bytes_hashed)
    );
    if rep.unreadable > 0 {
        println!(
            "   有 {} 个文件读不到（权限/被占用/中途被删），没有计入任何一组",
            rep.unreadable
        );
    }
    if let Some(path) = a.get("csv") {
        match std::fs::write(path, groups_to_csv(&rep.groups)) {
            Ok(_) => println!("   CSV 已写入 {}", path),
            Err(e) => eprintln!("写 CSV 失败：{}", e),
        }
    }
    if a.has("json") {
        println!();
        println!("{}", dup_json(&rep));
        return 0;
    }

    print_dups(&rep);
    0
}

fn print_dups(rep: &DupReport) {
    if rep.groups.is_empty() {
        println!();
        println!("没有发现内容相同的重复文件。");
        return;
    }
    println!();
    println!("重复文件组（按可回收空间降序，最多 {} 组）", rep.groups.len());
    println!("{}", rule_line(104));
    for (i, g) in rep.groups.iter().enumerate() {
        println!(
            "{:>3}. 每组 {} × {} = 可回收 {}",
            i + 1,
            human_size(g.size),
            g.count,
            human_size(g.wasted)
        );
        for f in &g.files {
            print!("      ");
            print!("{}", truncate_keep_tail(&f.path, 96));
            println!();
        }
        if g.truncated {
            println!("      ……（还有 {} 个同样的文件未列出）", g.count - g.files.len() as u64);
        }
    }
    println!("{}", rule_line(104));
    if rep.group_count > rep.groups.len() as u64 {
        println!(
            "共 {} 组，此处只显示前 {} 组（用 --top=N 调整）。",
            rep.group_count,
            rep.groups.len()
        );
    }
    println!(
        "提示：删除前请自行确认 —— 内容相同不代表可以删（快捷方式、字体、\
         程序数据目录里的同名文件都可能各自必需）。本工具只做查找，不删任何文件。"
    );
}

fn dup_json(rep: &DupReport) -> String {
    use core_io::jsonlite::quote;
    let mut o = String::from("{");
    o.push_str(&format!("\"considered\":{},", rep.same_size_files));
    o.push_str(&format!("\"same_size_groups\":{},", rep.same_size_groups));
    o.push_str(&format!("\"partial_pass\":{},", rep.partial_files));
    o.push_str(&format!("\"full_pass\":{},", rep.full_files));
    o.push_str(&format!("\"unreadable\":{},", rep.unreadable));
    o.push_str(&format!("\"bytes_hashed\":{},", rep.bytes_hashed));
    o.push_str(&format!("\"groups\":{},", rep.group_count));
    o.push_str(&format!("\"total_wasted\":{},", rep.total_wasted));
    let arr: Vec<String> = rep
        .groups
        .iter()
        .map(|g| {
            let files: Vec<String> = g.files.iter().map(|f| quote(&f.path)).collect();
            format!(
                "{{\"size\":{},\"count\":{},\"wasted\":{},\"files\":[{}]}}",
                g.size,
                g.count,
                g.wasted,
                files.join(",")
            )
        })
        .collect();
    o.push_str(&format!("\"items\":[{}]", arr.join(",")));
    o.push('}');
    o
}

// ─────────────────────────────────────────────────────────────────────────────
// snapshot / diff（F11：快照与对比）
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_snapshot(a: &Args) -> i32 {
    let raw = match a.positional.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("用法：filewatch snapshot <盘符|目录> [--out=快照.tsv] [--max-entries=N]");
            eprintln!("  建立的快照用于以后 `filewatch diff 旧.tsv 新.tsv`");
            return 2;
        }
    };
    let (drive, scan_root) = match split_target(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let created = unix_secs();
    let out: std::path::PathBuf = match a.get("out") {
        Some(p) => std::path::PathBuf::from(p),
        // 默认文件名带时间戳：连拍几次不会互相覆盖，而且按名字排序就是时间顺序
        None => std::path::PathBuf::from(format!("snapshot-{}.tsv", fmt_unix_stamp(created))),
    };
    let snap_opts = SnapshotOptions {
        max_entries: match a.get("max-entries") {
            Some(s) => parse_size(s)
                .ok_or_else(|| format!("--max-entries 需要数字，收到「{s}」"))
                .unwrap_or(0) as usize,
            None => 0,
        },
    };

    let mut cfg = match build_scan_config(a, &drive, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };
    cfg.force_traverse = a.has("force-traverse") || cfg.scan_root.is_some() || scan_root.is_some();
    cfg.allow_create_journal = a.has("create-journal");
    cfg.scan_root = scan_root;
    cfg.engine.collect_events = false;
    cfg.allow_traverse_watch = false;

    println!("═══ FileWatch 建立快照 ═══");
    println!("扫描范围 : {}", raw.replace('/', "\\"));
    let t0 = Instant::now();
    let session = match Session::open(cfg, progress_to_stderr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!();
            eprintln!("扫描失败：{}", e);
            if e.raw_os_error() == Some(5) {
                print_access_error(&drive, &e);
            }
            return 1;
        }
    };
    eprintln!();
    for w in &session.warnings {
        println!("· {}", w);
    }
    let remap = session.path_remap();
    let display_root = raw.replace('/', "\\");
    let snap = Snapshot::capture(&session.tree, remap.as_ref(), &display_root, created, &snap_opts);
    match snap.save(&out) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("写快照失败：{}", e);
            return 1;
        }
    }
    println!();
    println!(
        "已写入 {}：{} 个文件，共 {}，耗时 {} ms",
        out.display(),
        snap.len(),
        human_size(session.tree.total_size),
        t0.elapsed().as_millis()
    );
    // 明确标 UTC：引擎里所有时间都是 Unix 秒（UTC），本地时区换算会引入
    // 一个「显示 13:41、文件名 05:41」的不一致，不如一开始就说清楚。
    println!("时间戳   : {}（UTC）", fmt_unix_utc(created));
    if snap.truncated > 0 {
        println!(
            "注意     : 有 {} 个文件因 --max-entries 没记进去，用它对比会得到不完整的结论",
            snap.truncated
        );
    }
    println!();
    println!("以后用 `filewatch diff {} <更晚的快照>` 就能看到这一段时间里谁变大了。", out.display());
    0
}

fn cmd_diff(a: &Args) -> i32 {
    let (old_p, new_p) = match (a.positional.first(), a.positional.get(1)) {
        (Some(o), Some(n)) => (o.clone(), n.clone()),
        _ => {
            eprintln!("用法：filewatch diff <旧快照> <新快照> [--top=N] [--min-delta=1K] [--by-abs] [--by-path] [--csv=FILE]");
            eprintln!("  快照由 `filewatch snapshot <盘符> --out=...` 建立");
            return 2;
        }
    };
    let old = match Snapshot::load(std::path::Path::new(&old_p)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    let new = match Snapshot::load(std::path::Path::new(&new_p)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };

    let min_delta = match a.get("min-delta") {
        Some(s) => match parse_size(s) {
            Some(v) => v,
            None => {
                eprintln!("--min-delta 需要体积，如 1K，收到「{s}」");
                return 2;
            }
        },
        None => 0,
    };
    let opts = DiffOptions {
        min_delta,
        top: match a.num("top", 50) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("{}", e);
                return 2;
            }
        },
        order: if a.has("by-path") {
            DiffOrder::ByPath
        } else if a.has("by-abs") {
            DiffOrder::ByAbsChange
        } else {
            DiffOrder::ByGrowth
        },
    };
    let rep = core_scan::diff(&old, &new, &opts);

    println!("═══ FileWatch 快照对比 ═══");
    println!(
        "  旧 : {}  {}（{} 条）",
        fmt_unix_utc(old.created_unix),
        old.root,
        old.len()
    );
    println!(
        "  新 : {}  {}（{} 条）",
        fmt_unix_utc(new.created_unix),
        new.root,
        new.len()
    );
    // 根不同通常是拿错文件了。不报错（用户可能确实换了挂载点），但必须说一句 ——
    // 否则「整盘少了一半」这种结论会被当成真的。
    if !old.root.is_empty() && !new.root.is_empty() && old.root != new.root {
        println!(
            "  ⚠ 两份快照的扫描根不同（{} ↔ {}），对比结果可能没有意义",
            old.root, new.root
        );
    }
    println!();
    println!("{}", core_scan::snapshot::summary_cn(&rep));
    if rep.same > 0 {
        println!("   （另有 {} 个文件完全没变，未列出）", rep.same);
    }
    if let Some(path) = a.get("csv") {
        match std::fs::write(path, rows_to_csv(&rep.rows)) {
            Ok(_) => println!("   CSV 已写入 {}（{} 行）", path, rep.rows.len()),
            Err(e) => eprintln!("写 CSV 失败：{}", e),
        }
    }

    if rep.rows.is_empty() {
        println!();
        println!("没有符合条件的差异行。");
        return 0;
    }
    println!();
    let order_cn = match opts.order {
        DiffOrder::ByGrowth => "按增量降序",
        DiffOrder::ByAbsChange => "按变化幅度降序",
        DiffOrder::ByPath => "按路径升序",
    };
    println!("差异明细（{}，最多 {} 行）", order_cn, rep.rows.len());
    println!("{}", rule_line(104));
    println!(
        "{} {} {} {}",
        pad_right("类型", 6),
        pad_left("增量", 12),
        pad_left("新大小", 12),
        "路径"
    );
    println!("{}", rule_line(104));
    for r in &rep.rows {
        println!(
            "{} {} {} {}",
            pad_right(r.kind.label_cn(), 6),
            pad_left(&signed_size(r.delta), 12),
            pad_left(&human_size(r.new_size), 12),
            truncate_keep_tail(&r.path, 68)
        );
    }
    println!("{}", rule_line(104));
    if rep.rows_total > rep.rows.len() as u64 {
        println!(
            "共 {} 行差异，此处显示 {} 行（用 --top=N 调整，--min-delta=1K 过滤小改动）。",
            rep.rows_total,
            rep.rows.len()
        );
    }
    if rep.old_truncated > 0 || rep.new_truncated > 0 {
        println!(
            "注意：快照被截断过（旧缺 {} / 新缺 {} 个文件），对比结果不完整。",
            rep.old_truncated, rep.new_truncated
        );
    }
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// selftest
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_selftest(a: &Args) -> i32 {
    let verbose = a.has("verbose") || a.has("v");
    if verbose {
        println!("{}", selftest::env_info());
    }
    let rep = selftest::run(verbose);
    rep.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `split_target` 是「用户给的字符串指什么」的唯一解释处，
    /// 所以边界要逐条钉住：整卷 vs 子目录、反斜杠归一、以及**不能瞎兜底**。
    #[test]
    fn split_target_separates_drive_from_root() {
        // 整卷：没有扫描根 → 可以走 $MFT
        assert_eq!(split_target("C:").unwrap(), ("C:".to_string(), None));
        assert_eq!(split_target("c:").unwrap(), ("C:".to_string(), None), "盘符要归一成大写");
        assert_eq!(split_target("d:\\").unwrap(), ("D:".to_string(), None), "带尾杠也算整卷");

        // 子目录：必须给出扫描根，而且分隔符要归一
        let (d, r) = split_target(r"D:\projects\a").unwrap();
        assert_eq!(d, "D:");
        assert_eq!(r.unwrap().to_string_lossy(), r"D:\projects\a");
        let (_, r) = split_target("D:/projects/a").unwrap();
        assert_eq!(r.unwrap().to_string_lossy(), r"D:\projects\a", "/ 要归一成 \\");

        // **不能兜底**：以前 `drive_of` 认不出盘符时会静默当 C 盘 ——
        // 那会把一个 UNIX 风格路径变成"扫描整个 C 盘"。
        assert!(split_target("").is_err());
        assert!(split_target("/home/user").is_err());
        assert!(split_target("no-drive-here").is_err());
        assert!(split_target("C").is_err(), "只有字母没有冒号不算盘符");
    }

    #[test]
    fn layout_spec_parses_and_validates() {
        assert_eq!(parse_layout_spec("100x30").unwrap(), (100, 30));
        assert_eq!(parse_layout_spec("120X40").unwrap(), (120, 40));
        assert!(parse_layout_spec("100").is_err());
        assert!(parse_layout_spec("10x4").is_err());
        assert!(parse_layout_spec("axb").is_err());
        // 上限保护
        assert_eq!(parse_layout_spec("9000x9000").unwrap(), (400, 200));
    }

    #[test]
    fn alert_line_contains_key_numbers() {
        let a = Alert {
            node: 10,
            path: r"C:\logs".to_string(),
            kind: RuleKind::Absolute,
            threshold: 1024 * 1024,
            current_size: 3 * 1024 * 1024,
            baseline_size: 512 * 1024,
            observed: 3 * 1024 * 1024,
            at_ms: 0,
            message: String::new(),
        };
        let s = render_alert(&a, 1234);
        assert!(s.contains("预警"));
        assert!(s.contains(r"C:\logs"));
        assert!(s.contains("3.00 MB"));
    }

    /// 回归：预警行里的路径**不能**被二次替换。
    ///
    /// `Alert.path` 来自「注册监控目标」时用户敲的那条路径，本来就是真实路径。
    /// 早先这里也套了一层合成路径→真实路径的替换，于是深目录扫描根下会出现
    /// `C:\...\proj` → `C:\...\proj\Users\...\proj` 这种自我append。
    /// 一直没被发现，是因为 git-bash 传参时把 `\` 换成了 `/`，前缀正好对不上、
    /// 替换空转 —— 分隔符归一化之后才暴露出来。
    #[test]
    fn alert_line_does_not_double_remap_real_path() {
        let real = r"C:\Users\HY\AppData\Local\Temp\fw_livecheck2";
        let a = Alert {
            node: 5,
            path: real.to_string(),
            kind: RuleKind::Absolute,
            threshold: 1024 * 1024,
            current_size: 1208_000,
            baseline_size: 1000,
            observed: 1208_000,
            at_ms: 0,
            message: String::new(),
        };
        // 即便给了重映射参数也不该再改一遍（现在签名的第二个参数已经去掉，
        // 这条测试锁的是「输出里路径恰好出现一次」这个可观测结果）
        let s = render_alert(&a, 1234);
        assert_eq!(s.matches(real).count(), 1, "路径应只出现一次：{}", s);
        assert!(
            !s.contains(r"fw_livecheck2\Users"),
            "路径被二次替换了：{}",
            s
        );
    }

    /// 端到端：`session.cfg` 里给了深目录扫描根时，CLI 显示层拿到的是能用
    /// 的重映射（真正的替换逻辑在 `core_monitor::PathRemap`，那里有完整单测）。
    #[test]
    fn root_remap_rewrites_synthetic_prefix() {
        let mut cfg = SessionConfig::default();
        cfg.drive = "C:".to_string();
        cfg.scan_root = Some(std::path::PathBuf::from(r"C:\Users\me\Downloads"));
        let r = path_remap_of(&cfg).expect("应建立重映射");

        assert_eq!(r.apply(r"C:\incoming\a.bin"), r"C:\Users\me\Downloads\incoming\a.bin");
        assert_eq!(r.apply(r"C:\"), r"C:\Users\me\Downloads");
        // 不属于该盘符的路径原样返回
        assert_eq!(r.apply(r"D:\other\x"), r"D:\other\x");
    }

    #[test]
    fn changelog_inside_monitor_root_is_warned() {
        let root = r"D:\projects";
        // 监视根内部 → 必须提示
        assert!(warn_if_changelog_inside(root, Some(r"D:\projects\.fw\events.jsonl")).is_some());
        // 大小写与分隔符不影响判断
        assert!(warn_if_changelog_inside(root, Some("d:/Projects/events.jsonl")).is_some());
        // 就地写在根下（不带子目录）也要提示
        assert!(warn_if_changelog_inside(root, Some(r"D:\projects\events.jsonl")).is_some());

        // 监视范围之外 → 不提示
        assert!(warn_if_changelog_inside(root, Some(r"D:\logs\events.jsonl")).is_none());
        // 同前缀但不是子目录（`projects2`）→ 绝不能误判
        assert!(warn_if_changelog_inside(root, Some(r"D:\projects2\events.jsonl")).is_none());
        // 没给 --changelog
        assert!(warn_if_changelog_inside(root, None).is_none());
        // 监视整盘时不提示（日志在盘内是必然的，提了只是噪音）
        assert!(warn_if_changelog_inside("D:", Some(r"D:\logs\events.jsonl")).is_none());
        assert!(warn_if_changelog_inside(r"D:\", Some(r"D:\logs\e.jsonl")).is_none());
    }

    #[test]
    fn root_remap_is_none_without_scan_root() {
        let cfg = SessionConfig::default();
        assert!(path_remap_of(&cfg).is_none());
    }

    #[test]
    fn scan_config_forces_traverse_when_root_given() {
        let a = Args::parse(
            ["scan", "C:", r"--root=C:\tmp"]
                .iter()
                .map(|s| s.to_string()),
        );
        let cfg = build_scan_config(&a, "C:", false).unwrap();
        assert!(cfg.force_traverse, "给了 --root 就必须走遍历后端");
        assert!(cfg.scan_root.is_some());
    }

    #[test]
    fn scan_config_keeps_mft_path_by_default() {
        let a = Args::parse(["scan", "C:"].iter().map(|s| s.to_string()));
        let cfg = build_scan_config(&a, "C:", false).unwrap();
        assert!(!cfg.force_traverse);
        assert!(cfg.scan_root.is_none());
    }
}
