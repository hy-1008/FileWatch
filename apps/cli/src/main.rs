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

use args::{parse_ms, parse_rule, rule_label, Args};
use core_index::aggregate::{top_files, SizeMode};
use core_index::hardlink::build_report;
use core_index::tree::ChildIndex;
use core_index::{ExtensionTable, Tree};
use core_io::util::human_size;
use core_monitor::debounce::PendingEvent;
use core_monitor::rules::Alert;
use core_monitor::{PathRemap, Rule, RuleKind, TickOutcome};
use core_scan::{ScanBackend, ScanPhase, Session, SessionConfig};
use core_usn::journal;
use core_usn::normalize::EventKind;
use core_viz::{compute_layout, LayoutOptions, LodPolicy, Rect, ShadingMode};
use render::{
    fmt_offset_ms, fmt_unix_utc, pad_left, pad_right, render_ascii, render_legend, rule_line,
    truncate, truncate_keep_tail,
};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// FRN 低位掩码 → 节点下标（与引擎内部约定一致）。
const RECNO_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

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

示例：
  filewatch volumes
  filewatch probe C:
  filewatch scan C: --top=10 --exts=10 --layout=110x32
  filewatch scan D: --root=D:\projects --json
  filewatch monitor D:\projects --rule=abs=1G --rule=rate=5M/1m --duration=5m
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
        now_unix: unix_now(),
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
    let mut raw = match a.positional.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("用法：filewatch monitor <路径> [--rule=abs=1G] [--duration=60s]");
            eprintln!("  路径可以是盘符（C:）或目录（D:\\projects）");
            return 2;
        }
    };
    // 分隔符归一：Windows 两种都认，但 `C:/a\b` 这种混合形态只是难读，
    // 而且会一路带进横幅、变更行、预警行与落盘日志。调用方的 shell 可能已经
    // 换过一次分隔符（实测 git-bash 会把 `--root=C:\x` 传成 `C:/x`），
    // 所以在入口统一，后面就不必到处判断。
    raw = raw.replace('/', "\\");

    // 从路径里拆出盘符：D:\projects → ("D:", Some("D:\projects"))
    let drive = drive_of(&raw);
    let is_bare_drive = raw.len() <= 3 && raw.ends_with(':');
    let scan_root = if is_bare_drive {
        None
    } else {
        Some(std::path::PathBuf::from(&raw))
    };

    let mut cfg = match build_scan_config(a, &drive, !a.has("no-watch")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };
    cfg.scan_root = scan_root;
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
    println!("开始时间 : {}", fmt_unix_utc(unix_now()));
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
        if is_bare_drive {
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
        let now = base_ms() + elapsed_ms;

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
    match session.shutdown(base_ms() + elapsed_ms) {
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
    let parent = tree.path_of((ev.parent_frn & RECNO_MASK) as u32);
    let raw = join_path(&parent, &ev.name);
    let path = match remap {
        Some(r) => r.apply(&raw),
        None => raw,
    };
    // 目录的体积是聚合值，逐条事件里的 new_size 对它没有意义，不显示
    let size = if ev.is_dir {
        String::new()
    } else if ev.kind == EventKind::Deleted {
        format!("-{}", human_size((-ev.size_delta).max(0) as u64))
    } else if ev.size_delta > 0 {
        format!("+{}", human_size(ev.size_delta as u64))
    } else if ev.size_delta < 0 {
        format!("-{}", human_size((-ev.size_delta) as u64))
    } else {
        human_size(ev.new_size)
    };
    let mut suffix = String::new();
    if ev.merged > 1 {
        suffix.push_str(&format!("（合并 {} 条）", ev.merged));
    }
    if let (Some(old), EventKind::Renamed) = (&ev.old_name, ev.kind) {
        suffix.push_str(&format!(" ← {}", old));
    }
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

/// 从任意路径里取出盘符（`D:\x\y` → `D:`；`\\?\C:\x` → `C:`）。
fn drive_of(p: &str) -> String {
    let bytes: Vec<char> = p.chars().collect();
    for i in 0..bytes.len() {
        if bytes[i] == ':' && i >= 1 {
            let c = bytes[i - 1];
            if c.is_ascii_alphabetic() {
                return format!("{}:", c.to_ascii_uppercase());
            }
        }
    }
    "C:".to_string()
}

/// `C:\docs` + `a.txt` → `C:\docs\a.txt`（`path_of` 对根返回 `C:\`，不能再补一个杠）。
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    if dir.ends_with('\\') {
        format!("{}{}", dir, name)
    } else {
        format!("{}\\{}", dir, name)
    }
}

fn base_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn unix_now() -> i64 {
    (base_ms() / 1000) as i64
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

    #[test]
    fn drive_of_extracts_letter() {
        assert_eq!(drive_of("C:"), "C:");
        assert_eq!(drive_of(r"D:\projects\a"), "D:");
        assert_eq!(drive_of(r"\\?\C:\x"), "C:");
        assert_eq!(drive_of("no-drive-here"), "C:");
        assert_eq!(drive_of("e:\\"), "E:");
    }

    #[test]
    fn join_path_handles_root_and_subdir() {
        assert_eq!(join_path(r"C:\", "a.txt"), r"C:\a.txt");
        assert_eq!(join_path(r"C:\docs", "a.txt"), r"C:\docs\a.txt");
        assert_eq!(join_path("", "a.txt"), "a.txt");
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
