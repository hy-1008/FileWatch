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
//! | `history` | 目录级历史采样 + 基线异常 | E6 |
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
use std::path::{Path, PathBuf};
use core_scan::{
    collect_candidates, csv_field, find_duplicates, groups_to_csv, rows_to_csv, signed_size,
    DiffOptions, DiffOrder, DupOptions, DupProgress, DupReport, DupStage, FsSource, ScanBackend,
    ScanPhase, Session, SessionConfig, Snapshot, SnapshotOptions,
};
use core_usn::journal;
use core_viz::{compute_layout, LayoutOptions, LodPolicy, Rect, ShadingMode};
use render::{
    fmt_local_stamp, fmt_local_time, fmt_offset_ms, pad_left, pad_right, render_ascii, render_legend,
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
        "report" => cmd_report(&a),
        "sensitive" | "secrets" => cmd_sensitive(&a),
        "history" | "hist" => cmd_history(&a),
        "search" | "find" => cmd_search(&a),
        "content" | "grep" => cmd_content(&a),
        "rules" | "simulate" => cmd_rules(&a),
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
    // 平台差异只在**能力**上：Windows 有 MFT 直读 + USN，Linux 只有遍历 + inotify。
    // 把这件事写在标题下第一行，比在每处命令说明里各补一句更不容易漏。
    let backend = if core_io::HAS_MFT_BACKEND {
        "Windows 后端：MFT 直读 + USN Journal（无权限时自动降级为目录遍历 + ReadDirectoryChangesW）"
    } else {
        "Linux 后端：目录遍历 + inotify（$MFT / USN Journal 仅 Windows 可用，probe 只能读镜像文件）"
    };
    println!(
        r#"FileWatch —— 文件增量监控工具（引擎 + CLI）
{backend}

用法：
  filewatch <命令> [参数...]

命令：
  volumes                     列出本机所有卷、文件系统与可用后端
  probe <盘符>                读引导扇区 + $MFT 记录 0，逐字段对照 fsutil（M0 验收）
  journal <盘符> [选项]       查询 / 创建 / 删除 USN Journal
  scan <盘符|目录> [选项]     全量扫描并输出统计（M1 验收）
  top <盘符> [选项]           恒定内存找最大的 N 个文件（不建树）
  monitor <路径> [选项]       实时监控 + 阈值预警（M2/M3 验收）
  dupes <盘符|目录> [选项]    查找内容相同的重复文件（F12）
  snapshot <盘符|目录> [选项] 建立快照，供以后对比（F11）
  diff <旧快照> <新快照>      对比两次快照，看这段时间谁变大了（F11）
  report <盘符|目录> [选项]   导出单文件离线 HTML 报告（概览 + 面积图 + 三张榜）
  sensitive <盘符|目录>       按文件名/扩展名盘点敏感文件（不读内容）
  history <盘符|目录> [选项]  目录级历史：趋势与「今天长得不正常吗」（E6）
  search <盘符|目录> [选项]   在已扫出的索引里按文件名搜（E8/E9）
  content <盘符|目录> [选项]  在纯文本文件的内容里搜（不解析 Office）
  rules <盘符|目录> [选项]    规则试算：拿现在的索引算一遍会不会触发
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

search 选项（E9，与界面搜索同一套条件语法）：
  --text=关键词          必填；支持包含 / 模糊 / 正则（--mode）
  --mode=contains|fuzzy|regex   匹配方式（默认 contains）
  --scope=files|dirs|both       搜文件 / 文件夹 / 两者（默认 files）
  --limit=N              最多列出多少条（默认 50，计数不受影响）
  --min-size=10M         体积下界；--max-size=1G 上界（1024 进制）
  --after=7d             修改时间起点；--before=2026-06-30 终点
                         （相对：7d / 12h / 30m / 2w；绝对：2026-01-01 [12:30]，本地时间）
  --exts=mp4,mkv         扩展名白名单（大小写随意，带不带点都行）
  --csv=FILE             把命中写成 CSV（路径,字节,是否目录,修改时间）

content 选项（E9，纯文本类：txt / 日志 / CSV / 源码…；不含 Office）：
  --text=关键词          必填；--regex 把它当简化正则解析
  --limit-files=N        最多读多少个候选文件（默认 20000）
  --max-hits=N           最多列出多少个命中文件（默认 200）
  --min-size / --max-size / --after / --before / --exts
                         与 search 同一套（候选阶段就筛掉，白读的文件不读）
  --csv=FILE             把命中写成 CSV（路径,字节,修改时间,行号,行内容）

rules 选项（E9，规则试算 —— 不触发任何提醒，只回答"会不会"）：
  --rule=abs=1G          绝对体积阈值（可重复多条，同 monitor）
  --rule=grow=500M       增长量阈值：CLI 刚起跑没有基线，恒得 0
  --rule=rate=10M/60s    速率阈值：没有时间跨度时如实标"判断不了"
  --top=N                拿最大的 N 个一级子目录当样本（默认 20，含根）

search / content / rules 的公共选项：
  --from-snapshot=FILE   从快照读树，**跳过扫描**：
                           filewatch snapshot D:\data --out=s.tsv     ← 扫一次（几分钟）
                           filewatch search D:\data --text=mp4 --from-snapshot=s.tsv
                           filewatch content D:\data --text=TODO --from-snapshot=s.tsv
                           filewatch rules D:\data --rule=abs=1G --from-snapshot=s.tsv
                         快照只记 路径/体积/修改时间，所以重建出来的树：
                           * 路径与体积和实扫**逐行一致**（e2e 有对拍）；
                           * 没有"占用大小"，按逻辑大小算（稀疏/压缩文件会偏大）；
                           * **空目录不出现**（快照只记文件）。
                         给了这个参数时位置参数不参与扫描，只用于显示。

平台差异：
  Linux：scan / monitor / dupes / snapshot 直接接受绝对路径：
         filewatch scan /home/me/projects
         （没有盘符概念；"整个卷"就是挂载点本身，如 /home）
  Linux 不可用：
         top     恒定内存依赖 $MFT 直读 → 用 scan <目录> --top=N 代替
         journal USN Journal 是 NTFS 的 Windows 实现特性（Linux 用 inotify）
         probe   仍可用，但只能读块设备或磁盘镜像文件（块设备需要 root）

位置参数怎么解释（Windows）：
  只有 c / C: / C:\ 三种写法算"盘符" → 扫整卷（可能走 $MFT）。
  其它一律当目录（相对路径按当前目录算），而且**必须存在**：
      filewatch search crates --text=main      ← 只扫 crates 这一个目录
      filewatch report . --out=r.html          ← 只扫当前目录
  写错名字（既不是盘符也不是存在的目录）会直接报错退出 ——
  绝不会悄悄退化成"扫整卷"（那是最坏的默认值：又慢又不像他要的）。

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
    // USN Journal 是 NTFS 的 Windows 实现特性：`ntfs3` / `ntfs-3g` 都不暴露它。
    // 与其让用户看到一个 `Unsupported` 的系统错误，不如直接说清"这个平台上没有"。
    if !core_io::HAS_MFT_BACKEND {
        eprintln!("journal 命令仅 Windows 可用：USN Journal 是 NTFS 的 Windows 实现特性。");
        eprintln!("Linux 的增量监控走 inotify，既不需要、也无法创建这种日志。");
        eprintln!("直接 `filewatch monitor <目录>` 即可（会自动用 inotify）。");
        return 2;
    }
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
    let target = a
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| core_io::paths::DEFAULT_VOLUME.to_string());
    // 位置参数的含义两个平台不同：
    // * Windows：盘符（子目录要用 `--root=`，整卷才可能走 $MFT）；
    // * Linux：**路径**（`scan /home/me/x` 就只扫那个目录）—— 没有盘符概念，
    //   硬套 Windows 语义会让用户为了扫一个目录还得再写一次 --root。
    //
    // 两种都收在 `resolve_scan_target` 里：`scan crates` 这种"给个目录名"的写法
    // 不会被取首字母变成 `C:`（那会扫整卷）。
    let (norm, positional_root) = match resolve_scan_target(&target) {
        Ok(v) => v,
        Err(code) => return code,
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
    let mut cfg = match build_scan_config(a, &norm, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return 2;
        }
    };
    // Linux：位置参数本身就是一个目录 → 当作扫描根（`--root` 仍然优先）。
    if cfg.scan_root.is_none() {
        cfg.scan_root = positional_root;
    }

    println!(
        "开始扫描 {}（根：{}）",
        norm,
        cfg.scan_root
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| core_io::paths::root_prefix(&norm))
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
    // `top` 的全部价值在于「不建树、恒定内存」—— 那只有 $MFT 直读做得到。
    // Linux 上换一条路就没有这个性质了，所以**不假装能用**，直接指路。
    if !core_io::HAS_MFT_BACKEND {
        eprintln!("top 命令依赖 $MFT 直读（恒定内存扫描），仅 Windows 可用。");
        eprintln!("Linux 请用等价命令：");
        eprintln!("  filewatch scan {} --top={}", drive, a.num("n", 20).unwrap_or(20));
        return 2;
    }
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

#[cfg(windows)]
extern "system" fn on_ctrl_event(_t: u32) -> i32 {
    // 返回 1 表示「已处理」：系统不会立刻终止进程，
    // 我们有机会把游标与变更流水刷完再退出。
    INTERRUPTED.store(true, Ordering::SeqCst);
    1
}

/// 注册「退出前先把游标/流水刷完」的处理器。
///
/// * Windows：`SetConsoleCtrlHandler`（**不是** `signal`）—— 它还能捕获
///   关闭控制台窗口、注销、关机这几种事件，而 `signal` 只覆盖 Ctrl+C；
/// * Linux：`signal(SIGINT)`。`sigaction` 更严谨，但这里只做一件事
///   （置一个原子布尔），`signal` 的语义足够，且不必写平台相关的 `sigaction`
///   结构体（glibc 的布局跨架构不一致，手写容易错）。
fn install_interrupt_handler() {
    #[cfg(windows)]
    unsafe {
        core_io::winapi::SetConsoleCtrlHandler(Some(on_ctrl_event), 1);
    }
    #[cfg(not(windows))]
    {
        // SAFETY: 处理器只做一次原子写，是 async-signal-safe 的。
        unsafe {
            unix_signal(UNIX_SIGINT, on_unix_signal);
        }
    }
}

#[cfg(not(windows))]
const UNIX_SIGINT: i32 = 2;

#[cfg(not(windows))]
extern "C" {
    #[link_name = "signal"]
    fn unix_signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

#[cfg(not(windows))]
extern "C" fn on_unix_signal(_sig: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
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
    let norm = |s: &str| cli_path(s).trim_end_matches(['\\', '/']).to_lowercase();
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
    let raw = cli_path(&raw);

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
    println!("开始时间 : {}", fmt_local_time(unix_secs()));
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

    // 注册 Ctrl+C（平台实现见 install_interrupt_handler）
    let mut interrupted = false;
    install_interrupt_handler();

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

/// 把用户从 shell 拿到的路径归一成**本平台**形态。
///
/// * Windows：`/` → `\`（实测 git-bash 会把 `D:\x` 传成 `D:/x`，
///   不归一后面每一处显示与比较都要防一次）；
/// * Linux：**原样返回**。`\` 在这里是合法的文件名字符，
///   把它换成 `/` 等于给文件改名。
fn cli_path(raw: &str) -> String {
    if cfg!(windows) {
        raw.replace('/', "\\")
    } else {
        raw.to_string()
    }
}

/// 位置参数 → `(卷键, 扫描根)`。
///
/// * Windows：`C:` / `D:\` → `("C:", None)` 整卷；`D:\projects` → 子目录
/// * Linux：`/` 或某个挂载点 → `(挂载点, None)`；`/home/me/x` → 挂载点 + 子目录
///
/// 这是「用户给的那个字符串到底指什么」的**唯一**解释处：`monitor` / `dupes` /
/// `snapshot` 都用它。各写一份的话，迟早出现「monitor 认 `C:` 是整卷、
/// dupes 却当成子目录」这种谁都没注意到的分叉。
#[cfg(windows)]
fn split_target(raw: &str) -> Result<(String, Option<std::path::PathBuf>), String> {
    let norm = cli_path(&raw);
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
                "无法从「{raw}」认出盘符：应当形如 C: 或 D:\\目录（Windows 版只支持盘符路径）"
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

/// Linux：绝对路径 → `(所属挂载点, 扫描根)`。
///
/// 与 Windows 版的对应关系：
///
/// | Windows | Linux |
/// |---|---|
/// | `D:`（整卷） | `/home`（整个挂载点）→ `("/home", None)` |
/// | `D:\projects` | `/home/me/projects` → `("/home", Some(路径))` |
///
/// 「卷键」必须取**挂载点**而不是输入路径本身：索引树的前缀是卷根，
/// 扫描根是子目录时靠 [`PathRemap`] 换回真实路径 —— 这正是降级模式
/// 原有的补偿机制，Linux 只是复用它。
///
/// 挂载点取**最长匹配**并落在路径分量边界上：`/home2` 不能被当成
/// `/home` 的子目录（否则重映射会把前缀换错，路径全部指向不存在的位置）。
#[cfg(not(windows))]
fn split_target(raw: &str) -> Result<(String, Option<std::path::PathBuf>), String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("路径为空".to_string());
    }
    if !t.starts_with('/') {
        return Err(format!(
            "「{raw}」不是绝对路径：Linux 版请给绝对路径（例如 /home/me/projects）"
        ));
    }
    let mp = best_mount(t);
    let bare = t.trim_end_matches('/');
    if bare.is_empty() || bare == mp.trim_end_matches('/') {
        return Ok((mp, None));
    }
    Ok((mp, Some(std::path::PathBuf::from(t))))
}

/// `path` 所属的挂载点（最长前缀匹配，按路径分量比较）。
#[cfg(not(windows))]
fn best_mount(path: &str) -> String {
    let mut best = "/".to_string();
    for v in core_io::list_volumes() {
        let d = v.drive.trim_end_matches('/');
        let is_root = d.is_empty();
        let hit = is_root || path == d || path.starts_with(&format!("{d}/"));
        if hit && v.drive.len() > best.len() {
            best = v.drive.clone();
        }
    }
    // 兜底：根永远能匹配，所以 best 至少是 "/"；再归一一次防 `//`
    core_io::paths::normalize_root(&best)
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
    // 与 search / report 同一套判定：`c` / `C:` / `C:\` 才算盘符，其它当目录
    let (drive, scan_root) = match resolve_scan_target(&raw) {
        Ok(v) => v,
        Err(code) => return code,
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
    println!("查找范围 : {}", cli_path(&raw));
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
    // 与 search / report 同一套判定：`c` / `C:` / `C:\` 才算盘符，其它当目录
    let (drive, scan_root) = match resolve_scan_target(&raw) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let created = unix_secs();
    let out: std::path::PathBuf = match a.get("out") {
        Some(p) => std::path::PathBuf::from(p),
        // 默认文件名带时间戳：连拍几次不会互相覆盖，而且按名字排序就是时间顺序
        None => std::path::PathBuf::from(format!("snapshot-{}.tsv", fmt_local_stamp(created))),
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
    println!("扫描范围 : {}", cli_path(&raw));
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
    // 快照里记的根必须是**扫描根的真实路径**，不是用户敲的那串字：
    // 以前记的是 `crates`（相对路径），于是 `--from-snapshot` 从里面认不出卷，
    // 而且拿去和别人给的快照对比时根也对不上。
    let display_root = session
        .cfg
        .scan_root
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| drive.clone());
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
    println!("时间戳   : {}", fmt_local_time(created));
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
        // 归因要全量行，所以只能在 diff 里顺带算（`--attrib` 才开）
        attr: if a.has("attrib") {
            Some(core_scan::AttrOptions::default())
        } else {
            None
        },
    };
    let rep = core_scan::diff(&old, &new, &opts);

    println!("═══ FileWatch 快照对比 ═══");
    println!(
        "  旧 : {}  {}（{} 条）",
        fmt_local_time(old.created_unix),
        old.root,
        old.len()
    );
    println!(
        "  新 : {}  {}（{} 条）",
        fmt_local_time(new.created_unix),
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
    if let Some(attr) = &rep.attribution {
        print_attribution(attr);
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

/// 打印增长归因的三个榜（`diff --attrib`）。
///
/// 与界面上的归因卡是同一份数据（`core_scan::attribution`），只是这里排成
/// 终端能看的形状。三个榜的顺序固定：目录 → 类型 → 时段，因为人看的时候
/// 就是先问"哪儿"、再问"什么东西"、最后问"什么时候"。
fn print_attribution(attr: &core_scan::Attribution) {
    println!();
    println!("── 增长归因（目录聚合到 {} 层）──", attr.dir_depth);
    if attr.dirs_up.is_empty() && attr.dirs_down.is_empty() {
        println!("  没有落在扫描根范围内的变化行。");
        return;
    }
    if !attr.dirs_up.is_empty() {
        println!("  增长最多的目录：");
        for d in attr.dirs_up.iter().take(10) {
            println!(
                "    {:>12}  {}  （{} 个文件）",
                signed_size(d.delta),
                truncate_keep_tail(&d.dir, 64),
                d.files
            );
        }
    }
    if let Some(d) = attr.dirs_down.first() {
        println!("  减少最多的目录：{}  {}", truncate_keep_tail(&d.dir, 60), signed_size(d.delta));
    }
    if attr.dirs_total as usize > attr.dirs_up.len() {
        println!("    （共 {} 个目录有变化，这里只列前十）", attr.dirs_total);
    }
    if !attr.exts.is_empty() {
        println!("  按类型：");
        for e in attr.exts.iter().filter(|e| e.delta != 0).take(8) {
            let name = if e.ext.is_empty() { "(无扩展名)" } else { &e.ext };
            println!("    {:>12}  {}", signed_size(e.delta), name);
        }
    }
    if !attr.bursts.is_empty() {
        println!("  变化最集中的时段（按文件修改时间归类，不是删除时间）：");
        for b in attr.bursts.iter().take(3) {
            println!(
                "    {}  {}（{} 个文件）",
                fmt_local_time(b.hour_unix),
                signed_size(b.delta),
                b.files
            );
        }
    }
}

/// 建一次扫描会话。`report` / `sensitive` 共用 —— 它们都只需要一棵扫好的树。
///
/// 返回 `Err(退出码)`：调用方直接 `return` 那个码。参数校验与 `scan` 走同一套
/// （`build_scan_config`），所以 `--root` / `--force-traverse` / `--chunk` 的行为一致。
/// 位置参数看起来像盘符吗（`c` / `C:` / `C:\` 三种，别的都不算）。
///
/// 这个判定必须发生在 [`core_io::normalize_drive`] **之前**：它是"取第一个字母"，
/// 所以 `search crates` 会被读成 `C:` 然后开始遍历整个 C 盘 —— 用户以为自己只扫了
/// 一个目录，实际在等 150 万条（本机实测）。
fn looks_like_drive(s: &str) -> bool {
    let b = s.trim().as_bytes();
    match b.len() {
        1 => b[0].is_ascii_alphabetic(),
        2 => b[0].is_ascii_alphabetic() && b[1] == b':',
        3 => b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'),
        _ => false,
    }
}

/// 位置参数 → (盘符, 可选的扫描根)。
///
/// Windows 上位置参数的传统约定是**盘符**，但"给一个目录"才是人的第一反应
/// （`report C:\Users\me\proj`、`search crates`）。规则：
///
/// * 只有 `c` / `C:` / `C:\` 算盘符 → 整卷，才可能走 `$MFT`；
/// * 其它一律当目录，**并且必须存在** —— 不存在就报错退出，绝不退化成"扫整卷"。
///   静默扫整卷是最坏的默认值：又慢又不是他要的，而且看起来像成功了。
/// * 相对路径按当前目录转成绝对，免得扫描根与显示路径一个相对一个绝对。
///
/// Linux 没有盘符概念，位置参数本身就是路径（[`split_target`]）。
fn resolve_scan_target(target: &str) -> Result<(String, Option<std::path::PathBuf>), i32> {
    if !cfg!(windows) {
        return match split_target(target) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!("{}", e);
                Err(2)
            }
        };
    }
    // `c` 这种只有字母的写法先补上冒号：盘符就是盘符，不该因为少一个冒号
    // 被当成"当前目录下叫 c 的文件夹"。
    let t = target.trim();
    let as_drive = if t.len() == 1 && t.as_bytes()[0].is_ascii_alphabetic() {
        format!("{}:", t.to_ascii_uppercase())
    } else {
        t.to_string()
    };
    if looks_like_drive(&as_drive) {
        return match split_target(&as_drive) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!("{e}");
                Err(2)
            }
        };
    }
    let abs = match std::path::absolute(t) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("「{}」无法解析为路径：{}", target, e);
            return Err(2);
        }
    };
    if !abs.is_dir() {
        eprintln!(
            "「{}」既不是盘符（应形如 C:），也不是存在的目录（解析为 {}）。",
            target,
            abs.display()
        );
        eprintln!("想扫当前目录写 `.`；想扫子目录写目录名或完整路径。");
        return Err(2);
    }
    match abs.to_string_lossy().chars().next() {
        Some(c) if c.is_ascii_alphabetic() => Ok((format!("{}:", c.to_ascii_uppercase()), Some(abs))),
        // UNC（`\\server\share`）没有盘符，按卷扫描走不通；降级路径是 --root=
        _ => {
            eprintln!("「{}」不在盘符路径上（网络路径请用 --root=）", target);
            Err(2)
        }
    }
}

/// 一次查询的**数据来源**：现扫一棵树，或者从快照重建一棵（`--from-snapshot`）。
///
/// `search` / `content` / `rules` 需要的都只是"一棵能查的树"，差别只在树从哪来。
/// 抽成一个类型是为了让「快照模式不做 `PathRemap`」这条规矩只有一处 ——
/// 快照里的路径本来就是绝对路径，再换一次会把根拼两遍。
enum QuerySource {
    Live(Box<Session>),
    Snapshot {
        tree: Box<core_index::Tree>,
        /// 快照根在树里的下标。重建的树以**卷根**为根，而 `rules` 的样本是
        /// "扫描根 + 它的一级子目录" —— 拿卷根当样本会得出完全不同的结论。
        root: u32,
    },
}

impl QuerySource {
    fn tree(&self) -> &core_index::Tree {
        match self {
            QuerySource::Live(s) => &s.tree,
            QuerySource::Snapshot { tree, .. } => tree,
        }
    }
    /// 快照模式返回 `None`：路径已经是真实的，不需要（也不能）再换
    fn remap(&self) -> Option<core_monitor::PathRemap> {
        match self {
            QuerySource::Live(s) => s.path_remap(),
            QuerySource::Snapshot { .. } => None,
        }
    }
    /// 目录聚合的起点：现扫是树根，快照是快照记的那个根
    fn root(&self) -> u32 {
        match self {
            QuerySource::Live(s) => s.tree.root,
            QuerySource::Snapshot { root, .. } => *root,
        }
    }
}

/// 有 `--from-snapshot=` 就读快照（毫秒级），否则照旧现扫一遍。
///
/// 这条选项存在的理由很实际：大卷扫一次要几分钟，而"再问一个问题"不该
/// 再等几分钟。快照里已经有 `path / size / mtime`，足够重建一棵能做名字搜索、
/// 内容候选、规则试算的树。
fn open_query_source(a: &Args) -> Result<QuerySource, i32> {
    let p = match a.get("from-snapshot") {
        Some(v) if !v.trim().is_empty() => v,
        _ => return open_session_for(a).map(|(s, _)| QuerySource::Live(Box::new(s))),
    };
    let path = std::path::PathBuf::from(cli_path(p));
    let snap = match Snapshot::load(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读快照失败：{e}");
            return Err(1);
        }
    };
    // 卷键从快照自己的根推：Windows 是盘符，Linux 是挂载点
    let (drive, _) = match split_target(&snap.root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("快照的根「{}」认不出卷：{}", snap.root, e);
            return Err(2);
        }
    };
    let (tree, root) = match core_scan::tree_from_snapshot(&snap, &drive) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };
    eprintln!(
        "从快照读取 {} 个条目（根 {}，建立于 {}）—— **不扫描磁盘**",
        snap.entries.len(),
        snap.root,
        fmt_local_time(snap.created_unix)
    );
    if snap.truncated > 0 {
        eprintln!(
            "注意：这份快照当时就有 {} 个文件没记进来，结果会比真实情况少",
            snap.truncated
        );
    }
    Ok(QuerySource::Snapshot { tree: Box::new(tree), root })
}

fn open_session_for(a: &Args) -> Result<(Session, String), i32> {
    let target = a
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| core_io::paths::DEFAULT_VOLUME.to_string());
    let (norm, positional_root) = match resolve_scan_target(&target) {
        Ok(v) => v,
        Err(code) => return Err(code),
    };
    let mut cfg = match build_scan_config(a, &norm, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("参数错误：{}", e);
            return Err(2);
        }
    };
    // 位置参数指到目录时，它就是扫描根（`--root=` 仍然优先）
    if cfg.scan_root.is_none() {
        cfg.scan_root = positional_root;
    }
    match Session::open(cfg, progress_to_stderr) {
        Ok(s) => Ok((s, norm)),
        Err(e) => {
            eprintln!();
            eprintln!("扫描失败：{}", e);
            if e.raw_os_error() == Some(5) {
                print_access_error(&norm, &e);
            }
            Err(1)
        }
    }
}

/// 导出单文件离线 HTML 报告（E3）。与界面的「导出报告」用同一份模板。
fn cmd_report(a: &Args) -> i32 {
    let top = match a.num("top", 30) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let (session, _norm) = match open_session_for(a) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!();
    let remap = path_remap_of(&session.cfg);
    let ext = ExtensionTable::build(&session.tree);
    let opts = core_scan::ReportOptions {
        root: session
            .cfg
            .scan_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| session.tree.path_of(session.tree.root)),
        created_unix: unix_secs(),
        top,
        ..Default::default()
    };
    let html = core_scan::build_html(&session.tree, &ext, remap.as_ref(), &opts);
    let out = a
        .get("out")
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("filewatch-report-{}.html", unix_secs()));
    if let Err(e) = std::fs::write(&out, &html) {
        eprintln!("写入 {} 失败：{}", out, e);
        return 1;
    }
    println!(
        "报告已写入 {}（{}，{} 个文件 / {} 个目录）",
        out,
        human_size(html.len() as u64),
        session.tree.file_count,
        session.tree.dir_count
    );
    0
}

/// 敏感文件盘点（E5）。只按文件名与扩展名匹配，**不读内容**。
fn cmd_sensitive(a: &Args) -> i32 {
    let min_size = match a.get("min-size") {
        Some(s) => match parse_size(s) {
            Some(v) => v,
            None => {
                eprintln!("--min-size 需要体积，如 1K，收到「{s}」");
                return 2;
            }
        },
        None => 0,
    };
    let max = match a.num("max", 500) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    let (session, _norm) = match open_session_for(a) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!();
    let remap = path_remap_of(&session.cfg);
    let rep = core_index::sensitive::scan(
        &session.tree,
        &core_index::sensitive::Options {
            max,
            min_size,
        },
    );
    println!("═══ FileWatch 敏感文件盘点 ═══");
    println!(
        "扫过 {} 个文件，命中 {} 条（按 {} 条规则：文件名 / 扩展名）{}",
        rep.files,
        rep.hits.len(),
        core_index::SENSITIVE_RULES.len(),
        if rep.truncated { "，结果已截断" } else { "" }
    );
    if rep.hits.is_empty() {
        println!();
        println!("没有命中。注意这不等于「没有风险」：本命令只看文件名与扩展名，");
        println!("文件内容里有没有密钥、有没有无扩展名的私钥，它一概不知道。");
        return 0;
    }
    println!();
    for h in &rep.hits {
        let raw = session.tree.path_of(h.node);
        let path = match remap.as_ref() {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        println!(
            "[{}] {:<22} {:>10}  {}",
            h.rule.severity.label_cn(),
            h.rule.category,
            human_size(h.alloc),
            path
        );
        println!("      ↳ {}", h.rule.why);
    }
    0
}


// ─────────────────────────────────────────────────────────────────────────────
// E9 搜索 / 内容搜索 / 规则试算 —— 与界面的同名功能共用引擎的那一份实现
// ─────────────────────────────────────────────────────────────────────────────

/// 三个新命令共用的过滤器解析。
///
/// 写法**只有一份**：体积走 [`parse_size`]（与阈值规则同源），时间走
/// [`core_index::SearchFilters::parse_when`]（与界面同一处）。CLI 在这里
/// 自己再写一套解析的话，用户在界面上学会的 `7d` 到终端就不认了。
fn cli_search_filters(a: &Args) -> Result<core_index::SearchFilters, String> {
    let size_of = |key: &str| -> Result<Option<u64>, String> {
        match a.get(key) {
            None => Ok(None),
            Some(s) if s.trim().is_empty() => Ok(None),
            Some(s) => parse_size(s.trim()).map(Some).ok_or_else(|| {
                format!("--{key} 需要体积，如 10M / 512K / 1073741824，收到「{s}」")
            }),
        }
    };
    let when_of = |key: &str| -> Result<Option<i64>, String> {
        match a.get(key) {
            None => Ok(None),
            Some(s) if s.trim().is_empty() => Ok(None),
            Some(s) => {
                core_index::SearchFilters::parse_when(s.trim(), unix_secs())
                    .map(Some)
                    .ok_or_else(|| {
                        format!(
                            "--{key} 看不懂。写法示例：7d / 12h / 2026-01-01 / 2026-01-01 12:30，收到「{s}」"
                        )
                    })
            }
        }
    };
    Ok(core_index::SearchFilters {
        min_size: size_of("min-size")?,
        max_size: size_of("max-size")?,
        after: when_of("after")?,
        before: when_of("before")?,
        exts: core_index::SearchFilters::parse_exts(a.get("exts").unwrap_or("")),
    })
}

/// 把过滤器翻译成给人看的一行（回显用，不做换算）。
fn filter_summary(f: &core_index::SearchFilters) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = f.min_size {
        parts.push(format!("体积 ≥ {}", human_size(v)));
    }
    if let Some(v) = f.max_size {
        parts.push(format!("体积 ≤ {}", human_size(v)));
    }
    if let Some(v) = f.after {
        parts.push(format!("修改于 {} 起", fmt_local_time(v)));
    }
    if let Some(v) = f.before {
        parts.push(format!("修改于 {} 止", fmt_local_time(v)));
    }
    if !f.exts.is_empty() {
        parts.push(format!("扩展名 {}", f.exts.join(",")));
    }
    if parts.is_empty() {
        "无条件".to_string()
    } else {
        parts.join("　")
    }
}

/// `search` —— 在已扫出的索引里按文件名搜（E9）。
///
/// 默认先扫一遍再搜：CLI 的运行模型就是"一次一条命令"，与 GUI 的
/// "常驻进程搜内存"不同。扫描本身占大头时间，搜索只有几十毫秒 ——
/// 所以大卷上反复查的时候用 `--from-snapshot=` 跳过扫描。
fn cmd_search(a: &Args) -> i32 {
    let text = match a.get("text") {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("需要 --text=关键词（在已扫出的索引里按文件名搜）");
            return 2;
        }
    };
    let mode = core_index::MatchMode::parse(a.get("mode").unwrap_or("contains"));
    let scope = core_index::Scope::parse(a.get("scope").unwrap_or("files"));
    let limit = match a.num("limit", 50) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let filters = match cli_search_filters(a) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let src = match open_query_source(a) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!();
    let t0 = Instant::now();
    let out = match core_index::search_filtered(
        src.tree(),
        &core_index::Query { text: &text, mode, scope, limit },
        &filters,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    println!("═══ FileWatch 文件搜索 ═══");
    println!(
        "「{}」（{} / {}）　条件：{}",
        text,
        match mode {
            core_index::MatchMode::Contains => "包含",
            core_index::MatchMode::Fuzzy => "模糊",
            core_index::MatchMode::Regex => "正则",
        },
        match scope {
            core_index::Scope::Files => "文件",
            core_index::Scope::Dirs => "文件夹",
            core_index::Scope::Both => "两者",
        },
        filter_summary(&filters)
    );
    println!(
        "命中 {} 个{}　比过 {} 个节点，用时 {} ms",
        out.matched,
        if out.truncated { "（已达上限，只列这些）" } else { "" },
        out.scanned,
        t0.elapsed().as_millis()
    );
    if out.hits.is_empty() {
        return 0;
    }
    println!();
    let mut csv = match a.get("csv") {
        Some(p) => match std::fs::File::create(cli_path(p)) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("创建 CSV 失败：{e}");
                return 1;
            }
        },
        None => None,
    };
    if let Some(f) = csv.as_mut() {
        writeln!(f, "path,bytes,is_dir,mtime").ok();
    }
    // 只算一次 remap：每次 `apply` 都要重算前缀，放在循环里是白算
    let remap = src.remap();
    for h in &out.hits {
        // 快照模式下 remap 是 None —— 重建出来的路径本来就是绝对路径
        let path = match remap.as_ref() {
            Some(r) => r.apply(&h.path),
            None => h.path.clone(),
        };
        println!(
            "[{}] {:>10}  {}  {}",
            if h.is_dir { "目录" } else { "文件" },
            human_size(h.size),
            fmt_local_time(h.mtime),
            path
        );
        if let Some(f) = csv.as_mut() {
            writeln!(
                f,
                // 落盘给人看（Excel 里打开），所以时间写本地文本而不是 Unix 秒：
                // 同一个数字在屏幕上和 CSV 里长得不一样，是最容易被读错的一种错。
                "{},{},{},{}",
                csv_field(&path),
                h.size,
                h.is_dir,
                fmt_local_time(h.mtime)
            )
            .ok();
        }
    }
    if let Some(p) = a.get("csv") {
        println!();
        println!("CSV 已写入 {p}");
    }
    0
}

/// `content` —— 在纯文本文件的内容里搜（E9）。
///
/// 候选挑选与 GUI 的 ContentHub 相同口径：白名单 + 体积上限 + 附加条件，
/// **先筛后读盘**。逐文件打印最多 [`core_scan::content::MAX_LINES_PER_FILE`]
/// 条命中行，与界面一致。
fn cmd_content(a: &Args) -> i32 {
    use core_scan::content::{self, Pattern};
    let text = match a.get("text") {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            eprintln!("需要 --text=关键词（在文件内容里找，只搜纯文本类）");
            return 2;
        }
    };
    let pattern = match Pattern::new(&text, a.has("regex")) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let limit_files = match a.num("limit-files", 20_000) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let max_hits = match a.num("max-hits", 200) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let filters = match cli_search_filters(a) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let src = match open_query_source(a) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!();
    let t0 = Instant::now();
    // 挑候选：与界面同一套判定（含附加条件），超出 limit-files 就停
    let mut candidates: Vec<(u32, u64, i64)> = Vec::new();
    for (i, node) in src.tree().arena.nodes.iter().enumerate() {
        if candidates.len() >= limit_files {
            break;
        }
        if !node.in_use() || !node.is_file() || node.size > content::MAX_FILE_BYTES {
            continue;
        }
        let name = src.tree().name(i as u32);
        if !content::is_text_name(name) || !filters.accepts(name, node.size, node.mtime) {
            continue;
        }
        candidates.push((i as u32, node.size, node.mtime));
    }
    let total = candidates.len();
    println!("═══ FileWatch 内容搜索 ═══");
    println!(
        "「{}」{}　条件：{}",
        text,
        if a.has("regex") { "（正则）" } else { "" },
        filter_summary(&filters)
    );
    println!(
        "候选 {} 个纯文本文件（上限 {limit_files}），命中上限 {max_hits}",
        total
    );
    println!("注意：不含 Office 文档（docx/xlsx/pptx 是压缩包，引擎不解析）。");
    println!();
    let mut hits = 0usize;
    let mut truncated = false;
    let mut csv = match a.get("csv") {
        Some(p) => std::fs::File::create(cli_path(p)).map(Some).unwrap_or_else(|e| {
            eprintln!("创建 CSV 失败：{e}");
            None
        }),
        None => None,
    };
    if csv.is_some() {
        writeln!(csv.as_mut().unwrap(), "path,bytes,mtime,line_no,text").ok();
    }
    let remap = src.remap();
    for (node, size, mtime) in candidates {
        if hits >= max_hits {
            truncated = true;
            break;
        }
        let raw = src.tree().path_of(node);
        let path = match remap.as_ref() {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        let lines = content::search_file(std::path::Path::new(&path), &pattern);
        if lines.is_empty() {
            continue;
        }
        hits += 1;
        println!("{}　{:>10}　{}", path, human_size(size), fmt_local_time(mtime));
        for l in &lines {
            println!("    {:>5}  {}", l.line_no, l.text);
        }
        if let Some(f) = csv.as_mut() {
            for l in &lines {
                writeln!(
                    f,
                    // 同上：时间写本地文本，屏幕与 CSV 一个口径
                    "{},{},{},{},{}",
                    csv_field(&path),
                    size,
                    fmt_local_time(mtime),
                    l.line_no,
                    csv_field(&l.text)
                )
                .ok();
            }
        }
    }
    println!();
    println!(
        "命中 {} 个文件（读过 {} 个候选，用时 {} ms）{}",
        hits,
        total,
        t0.elapsed().as_millis(),
        if truncated { "，已达上限" } else { "" }
    );
    if let Some(p) = a.get("csv") {
        println!("CSV 已写入 {p}");
    }
    0
}

/// `rules` —— 规则试算（E9）：拿**现在的索引**算一遍这套规则会不会触发。
///
/// 样本 = 扫描根自身 + 最大的 N 个一级子目录（`Sample::instant`：没有监控
/// 基线）。于是 abs 规则当场就能判；grow 恒得 0（刚起跑，没有增量）；
/// rate 如实标"判断不了" —— 与界面「规则试算」同一份引擎代码
/// （[`core_monitor::simulate`]），两种前端不可能给出两套数。
fn cmd_rules(a: &Args) -> i32 {
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
    if rules.is_empty() {
        eprintln!("需要至少一条 --rule=abs=1G / grow=500M / rate=10M/60s");
        return 2;
    }
    let top = match a.num("top", 20) {
        Ok(n) => n.max(1),
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let src = match open_query_source(a) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!();
    let tree = src.tree();
    let remap = src.remap();
    let root = src.root();
    // 样本：根 + 一级子目录按体积降序取前 N 个
    let index = core_index::tree::ChildIndex::build(tree);
    let kids = core_index::aggregate::sorted_children(
        tree,
        index.children(root),
        core_index::aggregate::SizeMode::Logical,
    );
    let mut samples: Vec<core_monitor::Sample> = Vec::new();
    for &i in kids.iter().take(top) {
        let node = match tree.arena.get(i) {
            Some(n) if n.is_dir() => n,
            _ => continue,
        };
        let raw = tree.path_of(i);
        let path = match remap.as_ref() {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        samples.push(core_monitor::Sample::instant(path, node.size));
    }
    {
        let raw = tree.path_of(root);
        let path = match remap.as_ref() {
            Some(r) => r.apply(&raw),
            None => raw,
        };
        let size = tree.arena.get(root).map(|n| n.size).unwrap_or(0);
        samples.push(core_monitor::Sample::instant(path, size));
    }
    let verdicts = core_monitor::simulate(&rules, &samples);
    println!("═══ FileWatch 规则试算 ═══");
    println!(
        "样本：{} 个一级子目录 + 扫描根　规则 {} 条",
        samples.len() - 1,
        rules.len()
    );
    println!(
        "说明：CLI 没有监控基线 —— grow 恒为 0（刚起跑），rate 标「判断不了」；"
    );
    println!("      abs 按当前体积当场判。与界面「规则试算」同一份引擎代码。");
    println!();
    let mut fires = 0usize;
    let mut unjudgeable = 0usize;
    for v in &verdicts {
        let label = rules
            .get(v.rule_index)
            .map(rule_label)
            .unwrap_or_default();
        if !v.judgeable {
            unjudgeable += 1;
            println!(
                "[判断不了] {}　{}　没有时间跨度，速率规则要跑满一个窗口才有意义",
                label, v.path
            );
        } else if v.fires {
            fires += 1;
            println!(
                "[会触发]  {}　{}　实测 {}，超出 {}",
                label,
                v.path,
                human_size(v.observed),
                human_size(v.gap as u64)
            );
        } else {
            println!(
                "[不会触发] {}　{}　实测 {}，差 {}",
                label,
                v.path,
                human_size(v.observed),
                human_size((-v.gap) as u64)
            );
        }
    }
    println!();
    println!(
        "结论：{} 条会触发 / {} 条判断不了 / 共 {} 条结论",
        fires,
        unjudgeable,
        verdicts.len()
    );
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// E6 目录历史
// ─────────────────────────────────────────────────────────────────────────────

/// `history` —— 记录一个样本，或读历史看趋势与基线异常。
///
/// 两个动作刻意分开：
/// * `--record` 才扫描（扫描是重活），并把样本追加进历史；
/// * 不带它只读文件、出报表 —— 想看一眼趋势不该先等一次全盘扫描。
fn cmd_history(a: &Args) -> i32 {
    let root = a
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| core_io::paths::DEFAULT_VOLUME.to_string());
    let path = match a.get("file") {
        Some(p) => PathBuf::from(cli_path(p)),
        None => core_scan::history::default_path(&root),
    };
    let days = match a.num("days", core_scan::history::DEFAULT_LOOKBACK as usize) {
        Ok(n) => n as u32,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let depth = match a.num("depth", 3) {
        Ok(n) => n as u32,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let top = match a.num("top", 12) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    let mut hist = match core_scan::history::History::load(&path) {
        Ok(h) => h,
        Err(e) => {
            // 坏文件**不覆盖**：先改名留证，再从空历史开始。
            // 直接覆盖等于把用户几个月的数据扔了，而且他连哪一步弄坏的都不知道。
            let bad = path.with_extension(format!("tsv.bad-{}", fmt_local_stamp(unix_secs())));
            eprintln!("历史文件读不出来：{e}");
            eprintln!("已改名保留为 {}，本次从空历史开始", bad.display());
            let _ = std::fs::rename(&path, &bad);
            core_scan::history::History::default()
        }
    };

    if a.has("record") {
        let (session, norm) = match open_session_for(a) {
            Ok(v) => v,
            Err(code) => return code,
        };
        // 显示用的根取**扫描根**（`--root=` 或含分隔符的位置参数），
        // 不是盘符：`history D://projects` 记的历史，根就该是那个目录。
        let label = core_io::paths::to_native(
            &session
                .cfg
                .scan_root
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| norm.clone()),
        );
        if hist.root.is_empty() {
            hist.root = label.clone();
        }
        if hist.root != label {
            eprintln!(
                "注意：历史文件里的根是「{}」，这次扫的是「{}」—— 会记进同一份历史，趋势可能对不上",
                hist.root, label
            );
        }
        let remap = path_remap_of(&session.cfg);
        let sample = core_scan::history::capture(
            &session.tree,
            remap.as_ref(),
            unix_secs(),
            depth,
            core_scan::history::MAX_DIRS,
        );
        let dirs = sample.dirs.len();
        let buckets = sample.dirs_total;
        hist.push(sample);
        if let Err(e) = hist.save(&path) {
            eprintln!("保存历史失败：{e}");
            return 1;
        }
        println!(
            "已记录：{} 个目录（共 {} 个桶，按体积取前 {}）",
            dirs,
            buckets,
            core_scan::history::MAX_DIRS
        );
        println!();
    }

    print_history(&hist, &path, days, top);
    0
}

fn print_history(h: &core_scan::history::History, path: &Path, days: u32, top: usize) {
    println!("═══ FileWatch 目录增长历史 ═══");
    println!("历史文件 : {}", path.display());
    if h.samples.is_empty() {
        println!();
        println!("还没有任何样本。");
        println!("  filewatch history <路径> --record    让它在这次扫描后开始记录");
        println!();
        println!("提示：界面在「性能模式」下会按周期自动记录（同一根 30 分钟最多一次）。");
        return;
    }

    let first = h.samples.first().map(|s| s.at_unix).unwrap_or(0);
    let last = h.samples.last().map(|s| s.at_unix).unwrap_or(0);
    println!(
        "根       : {}",
        if h.root.is_empty() { "（未记录）" } else { &h.root }
    );
    println!(
        "样本     : {} 个（{} → {}）{}",
        h.samples.len(),
        fmt_local_time(first),
        fmt_local_time(last),
        if h.dropped > 0 {
            format!("，跳过 {} 个不完整样本", h.dropped)
        } else {
            String::new()
        }
    );
    if let Some(s) = h.samples.last() {
        println!(
            "最新     : {} 总 {} / {} 个文件，记了 {} / {} 个目录",
            fmt_local_time(s.at_unix),
            human_size(s.total),
            s.files,
            s.dirs.len(),
            s.dirs_total
        );
    }

    // ── 最近几天的总量 ──────────────────────────────────────────────
    let daily = h.daily_totals();
    if daily.len() >= 2 {
        println!();
        println!("最近{}天的总量：", daily.len().min(8));
        let tail = &daily[daily.len().saturating_sub(8)..];
        for w in tail.windows(2) {
            let (_, v0) = w[0];
            let (d1, v1) = w[1];
            let delta = v1 as i64 - v0 as i64;
            let sign = if delta >= 0 { "+" } else { "−" };
            let (y, m, dd) = core_io::util::civil_from_days(d1);
            println!(
                "  {:04}-{:02}-{:02}  {:>10}  {}{}",
                y,
                m,
                dd,
                human_size(v1),
                sign,
                human_size(delta.unsigned_abs())
            );
        }
    }

    // ── 基线异常 ────────────────────────────────────────────────────
    let list = core_scan::history::anomalies(h, days, top);
    println!();
    println!("今天 vs 前 {} 天基线（日增量）：", days);
    if list.is_empty() {
        println!("  没有发现异常增长。");
        return;
    }
    println!(
        "  {:<44} {:>10} {:>10} {:>8} {:>6}",
        "目录", "今天", "基线均值", "σ", "z"
    );
    for x in &list {
        let sign = if x.delta >= 0 { "+" } else { "−" };
        let z = match x.z {
            Some(z) => format!("{z:.1}"),
            // 不能显示 0：0 的意思是"和基线一样"，而这里的意思是"还不知道"
            None => format!("差{}天", x.days_needed),
        };
        println!(
            "  {:<44} {:>9}{:>9} {:>10} {:>8} {:>6}",
            tail_chars(&x.dir, 44),
            sign,
            human_size(x.delta.unsigned_abs()),
            human_size(x.base_mean.unsigned_abs()),
            human_size(x.base_sd.unsigned_abs()),
            z
        );
    }
    if list.iter().any(|x| x.z.is_none()) {
        println!();
        println!(
            "「差 N 天」= 基线还不够 {} 天，判不了。现在给一个 z 值只是更精致的猜测。",
            core_scan::history::MIN_BASELINE_DAYS
        );
    }
}

/// 取路径末尾 `n` 个字符（保尾：目录名比盘符有信息量）。
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let skip = count - n + 1;
    format!("…{}", s.chars().skip(skip).collect::<String>())
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
    #[cfg(windows)]
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

    /// Linux 版同一组契约：必须绝对路径；挂载点整体 → `None`；子目录 → `Some`。
    #[cfg(not(windows))]
    #[test]
    fn split_target_uses_longest_mount() {
        // 根下的深目录：卷键必须是 "/"，扫描根原样
        let (d, r) = split_target("/tmp/filewatch-smoke").unwrap();
        assert_eq!(d, "/");
        assert_eq!(r.unwrap().to_string_lossy(), "/tmp/filewatch-smoke");

        // 根自身 → 整卷（等价 Windows 的 `C:`）
        let (d, r) = split_target("/").unwrap();
        assert_eq!(d, "/");
        assert!(r.is_none());

        // 相对路径 / 空串必须报错，**不能**兜底成 "/"（那等于扫整个盘）
        assert!(split_target("").is_err());
        assert!(split_target("tmp/x").is_err());
        assert!(split_target("./x").is_err());

        // 挂载点匹配必须落在分量边界：`/home2` 不能被当成 `/home` 的子目录。
        // （没有 /home 挂载的机器上这条自动成立，断言的是"结果一定以 / 或
        //   某个真实挂载点为前缀，且不会出现 /home2 被换成 /home/"）
        let (mp, root) = split_target("/home2/x").unwrap();
        assert!(
            root.map(|p| p.to_string_lossy().starts_with(&mp)).unwrap_or(true),
            "扫描根必须在卷键之下：{mp}"
        );
    }

    /// 位置参数到底是盘符还是目录 —— 这是「扫整卷」与「扫一个目录」的分水岭。
    ///
    /// 真正的 bug 就出在这里：`search crates` 被 `normalize_drive` 取首字母变成
    /// `C:`，然后开始遍历整个 C 盘（本机实测 150 万条）。所以 `looks_like_drive`
    /// 只认三种写法，别的都必须走目录那条路。
    #[test]
    fn only_three_spellings_count_as_a_drive() {
        for yes in ["c", "C", "c:", "C:", "d:\\", "D:/"] {
            assert!(looks_like_drive(yes), "{yes} 应当被认成盘符");
        }
        for no in ["crates", "projects", ".", "..", "C:\\Users", "no-drive-here", ""] {
            assert!(!looks_like_drive(no), "{no} 不该被认成盘符（否则会扫整卷）");
        }
    }

    /// 目录那一路：存在的相对目录 → 扫描根 = 它的**绝对**路径；
    /// 不存在的 → 报错退出，**绝不**退化成"扫整卷"。
    #[test]
    #[cfg(windows)]
    fn directory_positional_becomes_scan_root() {
        let cwd = std::env::current_dir().unwrap();
        let (drive, root) = resolve_scan_target(".").expect("`.` 必须被当成当前目录");
        assert_eq!(root.as_deref(), Some(cwd.as_path()), "相对路径要转成绝对");
        let want = format!("{}:", cwd.to_string_lossy().chars().next().unwrap().to_ascii_uppercase());
        assert_eq!(drive, want);

        // 盘符仍然走"整卷"那条路
        assert_eq!(resolve_scan_target("C:").unwrap(), ("C:".to_string(), None));
        assert_eq!(resolve_scan_target("c").unwrap(), ("C:".to_string(), None), "少个冒号也是盘符");
        assert_eq!(resolve_scan_target("D:\\").unwrap(), ("D:".to_string(), None), "带尾杠仍是整卷");

        // 不存在 → 参数错误（码 2），不是"扫整卷"
        assert_eq!(resolve_scan_target("no-such-dir-xyzzy").unwrap_err(), 2);
    }

    #[test]
    fn layout_spec_parses_and_validates() {        assert_eq!(parse_layout_spec("100x30").unwrap(), (100, 30));
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
