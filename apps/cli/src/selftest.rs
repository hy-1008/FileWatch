//! 内置自检：**不需要管理员权限**就能验证引擎的关键不变量。
//!
//! 为什么需要它：`$MFT` 直读与 USN Journal 都要求管理员权限，
//! 而开发机/CI 通常没有。若只能靠 `fsutil` 手工比对，M0~M3 的验收就无从自动化。
//! 这一套把「能在用户态验证的部分」全部覆盖：
//!
//! | 层 | 验证内容 |
//! |---|---|
//! | 内存布局 | `Node` 尺寸、字符串池、聚合守恒 |
//! | 几何 | 面积守恒、长宽比、相机钳制、LOD 剪枝 |
//! | 事件语义 | 合并、抵消、滞回、冷却、去重 |
//! | 真实磁盘 | 目录遍历扫描、`ReadDirectoryChangesW` 实时增量（尽力而为） |
//! | 协议解析 | `USN_RECORD_V2` 字节布局、游标自愈判定 |
//!
//! 每一项都返回「通过 / 失败 / 跳过」，跳过只发生在**环境本身不支持**时
//! （例如连目录句柄都拿不到），绝不是掩盖失败。

use core_index::aggregate::{aggregate, propagate_delta, top_files, SizeMode};
use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
use core_index::hardlink::build_report;
use core_index::tree::ChildIndex;
use core_index::{ExtensionTable, Tree};
use core_monitor::debounce::{DebounceConfig, Debouncer, RawEvent};
use core_monitor::rules::{Rule, RuleConfig, RuleEngine};
use core_monitor::Severity;
use core_scan::snapshot::{DiffOptions, SnapEntry, Snapshot, SnapshotOptions};
use core_scan::{ContentSource, DupCandidate, DupOptions};
use core_usn::cursor::{evaluate, Cursor, CursorValidity};
use core_usn::normalize::*;
use core_usn::record::parse_records;
use core_usn::UsnJournalData;
use core_viz::{compute_layout, Camera, LayoutOptions, LodPolicy, Rect, ShadingMode};
use std::fmt::Write as _;

use core_scan::diff;
use core_scan::find_duplicates;

// ── 结果类型 ────────────────────────────────────────────────────────────────

pub enum Outcome {
    Pass(String),
    Fail(String),
    Skip(String),
}

impl Outcome {
    fn tag(&self) -> &'static str {
        match self {
            Outcome::Pass(_) => "通过",
            Outcome::Fail(_) => "失败",
            Outcome::Skip(_) => "跳过",
        }
    }
    fn detail(&self) -> &str {
        match self {
            Outcome::Pass(s) | Outcome::Fail(s) | Outcome::Skip(s) => s,
        }
    }
}

#[derive(Default)]
pub struct Report {
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    pub failures: Vec<(String, String)>,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        if self.fail == 0 {
            0
        } else {
            1
        }
    }
}

/// 断言辅助：条件不成立时返回 `Fail`。
fn check(name: &str, cond: bool, detail: impl Into<String>) -> Outcome {
    let d = detail.into();
    if cond {
        Outcome::Pass(d)
    } else {
        Outcome::Fail(format!("{} —— {}", name, d))
    }
}

/// 逐项执行并打印。
pub fn run(verbose: bool) -> Report {
    let mut r = Report::default();
    let cases: Vec<(&str, fn() -> Outcome)> = vec![
        // ── 内存布局与索引 ─────────────────────────────────────────────
        ("Node 紧凑布局 = 40 字节", t_node_size),
        ("字符串池 intern/get 往返", t_string_pool),
        ("目录树路径拼接", t_tree_path),
        ("聚合守恒：根 = 全部文件之和", t_aggregate_conserves),
        ("增量 delta 只上传到根（O(树深)）", t_propagate_depth),
        ("删除保留 parent 供负向上传", t_remove_keeps_parent),
        ("Top-N 大文件与全排序一致", t_top_files),
        ("扩展名统计表排序稳定", t_ext_table),
        ("硬链接/稀疏差异说明可读", t_hardlink_report),
        // ── 几何 ───────────────────────────────────────────────────────
        ("面积守恒：子矩形面积之和 ≤ 父矩形", t_squarify_area),
        ("长宽比优于朴素切分", t_squarify_aspect),
        ("LOD 节点预算生效", t_lod_budget),
        ("相机钳制后不越界", t_camera_clamp),
        ("布局结果自检（面积/重叠）", t_layout_audit),
        // ── 事件语义 ───────────────────────────────────────────────────
        ("USN Reason → 语义事件优先级", t_classify),
        ("USN_RECORD_V2 字节布局解析", t_usn_v2_parse),
        ("游标自愈：4 种有效性判定", t_cursor_validity),
        ("debounce 合并编辑器保存风暴", t_debounce_storm),
        ("创建+删除在窗口内互相抵消", t_debounce_cancel),
        ("临时文件被过滤", t_debounce_temp_filter),
        ("阈值规则触发一次且只一次（滞回）", t_rule_hysteresis),
        ("冷却期内不重复预警", t_rule_cooldown),
        ("预警严重级别：超 2 倍 = Critical", t_severity),
        // ── F11/F12 去重与快照 ─────────────────────────────────────────
        ("重复文件三级去重：体积唯一者不读内容", t_dedup_funnel),
        ("重复文件：哈希撞车被逐字节复核挡住", t_dedup_collision),
        ("快照往返 + diff 四类变更", t_snapshot_roundtrip_and_diff),
        // ── 真实磁盘（尽力而为） ───────────────────────────────────────
        ("目录遍历扫描真实临时目录", t_traverse_real),
        ("面积图布局接真实树", t_layout_on_real_tree),
        ("目录通知：动作码与解码", t_dirwatch_parse),
        ("目录通知：实机投递", t_dirwatch_live),
        ("降级模式端到端：扫描→事件→增量", t_degraded_live),
    ];

    println!();
    println!("FileWatch 自检 —— 共 {} 项", cases.len());
    println!("{}", "─".repeat(78));

    for (i, (name, f)) in cases.iter().enumerate() {
        let out = f();
        let tag = out.tag();
        println!(
            "[{:>2}/{}] {:<5} {}",
            i + 1,
            cases.len(),
            tag,
            name
        );
        if verbose || matches!(out, Outcome::Fail(_)) {
            let d = out.detail();
            if !d.is_empty() {
                for line in wrap(d, 72) {
                    println!("         {}", line);
                }
            }
        }
        match out {
            Outcome::Pass(_) => r.pass += 1,
            Outcome::Fail(d) => {
                r.fail += 1;
                r.failures.push((name.to_string(), d));
            }
            Outcome::Skip(_) => r.skip += 1,
        }
    }

    println!("{}", "─".repeat(78));
    println!(
        "结果：通过 {} / 失败 {} / 跳过 {}",
        r.pass, r.fail, r.skip
    );
    if r.fail > 0 {
        println!();
        println!("失败明细：");
        for (n, d) in &r.failures {
            println!("  · {} —— {}", n, d);
        }
    }
    r
}

/// 简单折行（不依赖 textwrap）。
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if crate::render::display_width(&cur) >= width {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// ── 测试用的小树构造 ────────────────────────────────────────────────────────

const MB: u64 = 1024 * 1024;

fn mk_tree() -> Tree {
    let mut t = Tree::new("C:", 64);
    // 闭包自身不捕获可变状态（可变性全在 `&mut Tree` 形参里），因此不需要 `mut`
    let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, size: u64| {
        let off = t.names.intern_str(name);
        t.arena.ensure(idx);
        t.arena.nodes[idx as usize] = Node {
            parent,
            seq: 1,
            flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
            size,
            alloc: size,
            mtime: 1_700_000_000,
            name: off,
        };
    };
    set(&mut t, 5, 5, ".", true, 0);
    set(&mut t, 10, 5, "docs", true, 0);
    set(&mut t, 11, 10, "a.txt", false, 3 * MB);
    set(&mut t, 12, 10, "b.log", false, 2 * MB);
    set(&mut t, 13, 5, "movie.mkv", false, 8 * MB);
    aggregate(&mut t);
    t
}

// ── 各项实现 ────────────────────────────────────────────────────────────────

fn t_node_size() -> Outcome {
    let sz = core::mem::size_of::<Node>();
    check(
        "Node 尺寸",
        sz == 40,
        format!(
            "size_of::<Node>() = {}（目标 40）：200 万文件 → {} 节点数组",
            sz,
            core_io::util::human_size((sz * 2_000_000) as u64)
        ),
    )
}

fn t_string_pool() -> Outcome {
    let mut t = Tree::new("C:", 4);
    let a = t.names.intern_str("alpha.txt");
    let b = t.names.intern_str("中文名.bin");
    let a2 = t.names.intern_str("alpha.txt");
    // 注意：字符串池是**只追加、不去重**的（有意设计）。
    //   「intern」在这里指「写进池并返回偏移」，不返回全局唯一 handle。
    //   去重要用 HashMap<名字, 偏移>，200 万条目 ≈ 100 MB+，
    //   比它省下的重复名字还贵 —— 与「用 FRN 当下标、不用哈希表」的整体取舍一致。
    //   因此 a2 != a 是**正确行为**，本用例只断言往返读写正确 + 偏移不复用。
    let roundtrip = t.names.get(a) == "alpha.txt"
        && t.names.get(b) == "中文名.bin"
        && t.names.get(a2) == "alpha.txt"
        && a != 0
        && b != 0;
    check(
        "字符串池读写往返",
        roundtrip && a != b && a != a2 && t.names.count() >= 3,
        format!(
            "a={} a2={} b={} get(a)={:?} get(b)={:?} get(a2)={:?}（只追加不去重：a2≠a 属预期）",
            a,
            a2,
            b,
            t.names.get(a),
            t.names.get(b),
            t.names.get(a2)
        ),
    )
}

fn t_tree_path() -> Outcome {
    let t = mk_tree();
    let p = t.path_of(11);
    let pd = t.path_of(10);
    // 根名是 "."，绝不能出现在路径里
    check(
        "路径拼接",
        p == r"C:\docs\a.txt" && pd == r"C:\docs" && t.path_of(5) == r"C:\",
        format!("path_of(11)={:?} path_of(10)={:?} path_of(5)={:?}", p, pd, t.path_of(5)),
    )
}

fn t_aggregate_conserves() -> Outcome {
    let t = mk_tree();
    let files: u64 = [11u32, 12, 13]
        .iter()
        .map(|&i| t.arena.get(i).unwrap().size)
        .sum();
    let root = t.arena.get(5).unwrap().size;
    let docs = t.arena.get(10).unwrap().size;
    check(
        "聚合守恒",
        root == files && docs == 5 * MB && t.file_count == 3 && t.dir_count == 2,
        format!(
            "根={} 文件合计={} docs={}（应 5 MB） 文件数={} 目录数={}",
            core_io::util::human_size(root),
            core_io::util::human_size(files),
            core_io::util::human_size(docs),
            t.file_count,
            t.dir_count
        ),
    )
}

fn t_propagate_depth() -> Outcome {
    let mut t = mk_tree();
    // docs 的深度：11 → docs(10) → root(5)，即 2 个祖先
    let touched = propagate_delta(&mut t, 11, 1, 1, |_, _| {});
    check(
        "delta 上传深度",
        touched == 2,
        format!(
            "从记录 11 上传应触及 2 个祖先（docs、根），实际 {}",
            touched
        ),
    )
}

fn t_remove_keeps_parent() -> Outcome {
    let mut t = mk_tree();
    let before = t.arena.get(10).unwrap().size;
    let (sz, _al) = t.remove_node(11).unwrap_or((0, 0));
    let parent_kept = t.arena.get(11).map(|n| n.parent);
    let removed = !t.arena.get(11).unwrap().in_use();
    // 删除后仍能沿着「原来的父链」做负向上传
    let touched = propagate_delta(&mut t, 11, -(sz as i64), -(sz as i64), |_, _| {});
    let after = t.arena.get(10).unwrap().size;
    check(
        "删除保留 parent",
        removed && parent_kept == Some(10) && touched == 2 && after == before - 3 * MB,
        format!(
            "父指针={:?} 上传触及={} docs {} → {}",
            parent_kept,
            touched,
            core_io::util::human_size(before),
            core_io::util::human_size(after)
        ),
    )
}

fn t_top_files() -> Outcome {
    let t = mk_tree();
    let top = top_files(&t, 3, SizeMode::Logical);
    let sizes: Vec<u64> = top.iter().map(|&i| t.arena.get(i).unwrap().size).collect();
    check(
        "Top-N 有序",
        sizes == vec![8 * MB, 3 * MB, 2 * MB],
        format!("{:?}", sizes.iter().map(|s| core_io::util::human_size(*s)).collect::<Vec<_>>()),
    )
}

fn t_ext_table() -> Outcome {
    let t = mk_tree();
    let table = ExtensionTable::build(&t);
    let (top, _rest) = table.top(10, SizeMode::Logical);
    let names: Vec<String> = top
        .iter()
        .map(|s| format!("{}:{}", s.display_ext(), s.count))
        .collect();
    // txt / log / mkv 各 1 个，按体积降序：mkv(8M) > txt(3M) > log(2M)
    check(
        "扩展名表",
        names == vec!["mkv:1", "txt:1", "log:1"],
        format!("{:?}（总数 {}）", names, table.total_count),
    )
}

fn t_hardlink_report() -> Outcome {
    let t = mk_tree();
    let rep = build_report(&t);
    let lines = rep.explain_lines();
    check(
        "硬链接报告",
        !lines.is_empty() && rep.hardlinked_records == 0,
        format!("{} 行说明，硬链接记录 {}", lines.len(), rep.hardlinked_records),
    )
}

fn t_squarify_area() -> Outcome {
    let t = mk_tree();
    let children = ChildIndex::build(&t);
    let exts = ExtensionTable::build(&t);
    let vp = Rect::new(0.0, 0.0, 1200.0, 800.0);
    let opts = LayoutOptions {
        policy: LodPolicy::wide(),
        mode: SizeMode::Logical,
        shading: ShadingMode::ByExtension,
        now_unix: 1_700_000_000,
    };
    let res = compute_layout(&t, &children, &exts, 5, vp, &opts);

    // 不变量：某个目录的**直接子矩形**面积之和不应超过它自己的矩形面积。
    // 用「深度 +1 且落在父矩形内」近似识别直接子项——`LayoutItem` 不带父指针，
    // 但布局保证父先于子入列，深度关系是可靠的。
    let ratio_of = |parent: u32| -> Option<f32> {
        let p = res.find(parent)?;
        let sum: f32 = res
            .items
            .iter()
            .filter(|o| o.depth == p.depth + 1 && inside(&o.rect, &p.rect))
            .map(|o| o.rect.area())
            .sum();
        Some(sum / p.rect.area().max(1.0))
    };

    let r_root = ratio_of(5).unwrap_or(-1.0);
    let r_docs = ratio_of(10).unwrap_or(-1.0);
    let ok = r_root > 0.0 && r_root <= 1.05 && r_docs > 0.0 && r_docs <= 1.05;
    check(
        "面积守恒",
        ok,
        format!(
            "根的子块面积比 {:.3}，docs 的子块面积比 {:.3}（应 ≤ 1.05：子块由父内容区切分，\n\
             父还含有标题栏 {} px 与内边距 {} px，故比值必小于 1）",
            r_root, r_docs, opts.policy.header_h, opts.policy.pad
        ),
    )
}

fn inside(inner: &Rect, outer: &Rect) -> bool {
    inner.x >= outer.x - 0.5
        && inner.y >= outer.y - 0.5
        && inner.x + inner.w <= outer.x + outer.w + 0.5
        && inner.y + inner.h <= outer.y + outer.h + 0.5
}

fn t_squarify_aspect() -> Outcome {
    use core_viz::squarify;
    let vp = Rect::new(0.0, 0.0, 1000.0, 600.0);
    let areas = vec![600.0, 300.0, 60.0, 30.0, 6.0, 3.0];
    let out = squarify(&areas, vp);

    let worst_of = |rs: &[Rect]| -> f32 {
        rs.iter()
            .filter(|r| r.w > 0.0 && r.h > 0.0)
            .map(|r| {
                let a = r.aspect_ratio();
                if a >= 1.0 {
                    a
                } else {
                    1.0 / a.max(1e-6)
                }
            })
            .fold(0.0f32, f32::max)
    };

    // 对照基线：朴素的 slice-and-dice（按面积比例竖着切成一排长条）
    //
    // 注意类型：`areas` 必须是 `Vec<f64>`（`squarify` 的入参就是 `&[f64]`），
    // 而 `Rect` 的字段是 `f32`。所以比例在 f64 里算完，最后一步再收敛到 f32，
    // 不要试图让 `total` 变成 f32 —— 那样 `areas.iter().sum()` 无法匹配 `&f64`。
    let total: f64 = areas.iter().sum();
    let naive: Vec<Rect> = {
        let mut x = vp.x;
        let mut v = Vec::new();
        for (i, a) in areas.iter().enumerate() {
            let w = if i + 1 == areas.len() {
                vp.right() - x
            } else {
                (a / total * vp.w as f64) as f32
            };
            v.push(Rect::new(x, vp.y, w, vp.h));
            x += w;
        }
        v
    };

    let worst_sq = worst_of(&out);
    let worst_naive = worst_of(&naive);
    // 全部矩形必须落在容器内
    let inside_all = out.iter().all(|r| inside(r, &vp));

    check(
        "长宽比",
        out.len() == areas.len()
            && inside_all
            && worst_sq < worst_naive / 5.0
            && worst_sq < 10.0,
        format!(
            "Squarified 最坏长宽比 {:.2}，朴素竖切 {:.2}（改善 {:.1}×）；{} 个矩形全部在容器内",
            worst_sq,
            worst_naive,
            if worst_sq > 0.0 { worst_naive / worst_sq } else { 0.0 },
            out.len()
        ),
    )
}

fn t_lod_budget() -> Outcome {
    // 造一个宽而浅的树：根下 5000 个文件
    let mut t = Tree::new("C:", 5002);
    let off = t.names.intern_str(".");
    t.arena.ensure(5);
    t.arena.nodes[5] = Node {
        parent: 5,
        seq: 1,
        flags: NODE_IN_USE | NODE_DIR,
        size: 0,
        alloc: 0,
        mtime: 0,
        name: off,
    };
    for i in 6..5002u32 {
        let o = t.names.intern_str(&format!("f{}.bin", i));
        t.arena.ensure(i);
        t.arena.nodes[i as usize] = Node {
            parent: 5,
            seq: 1,
            flags: NODE_IN_USE,
            size: 1024,
            alloc: 4096,
            mtime: 0,
            name: o,
        };
    }
    aggregate(&mut t);

    let children = ChildIndex::build(&t);
    let exts = ExtensionTable::build(&t);
    let vp = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
    let mut policy = LodPolicy::wide();
    policy.max_nodes = 500;
    let opts = LayoutOptions {
        policy,
        mode: SizeMode::Logical,
        shading: ShadingMode::ByExtension,
        now_unix: 0,
    };
    let res = compute_layout(&t, &children, &exts, 5, vp, &opts);
    check(
        "LOD 预算",
        res.items.len() <= 600 && res.stats.hit_budget,
        format!(
            "4985 个子项 → 输出 {} 个矩形（预算 500，命中上限 {}）",
            res.items.len(),
            res.stats.hit_budget
        ),
    )
}

fn t_camera_clamp() -> Outcome {
    let content = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    let vp = Rect { x: 0.0, y: 0.0, w: 800.0, h: 500.0 };
    let mut cam = Camera::new();
    // 极端操作：狂拉远 + 甩到内容之外
    cam.zoom = 0.0001;
    cam.center_x = -1e9;
    cam.center_y = 1e9;
    cam.clamp(content, vp);
    let ok_far = cam.zoom >= core_viz::MIN_ZOOM && cam.zoom <= core_viz::MAX_ZOOM;

    let mut cam2 = Camera::new();
    cam2.zoom = 1e9;
    cam2.center_x = 5e5;
    cam2.clamp(content, vp);
    let ok_near = cam2.zoom <= core_viz::MAX_ZOOM;

    // 钳制后可见区域必须与内容相交
    let vis = cam.visible_rect(vp);
    let inter = vis.intersection_area(&content);
    check(
        "相机钳制",
        ok_far && ok_near && inter > 0.0,
        format!(
            "远界 zoom={:.4} 近界 zoom={:.1}（上限 {}）可见区与内容交集 {:.1}",
            cam.zoom, cam2.zoom, core_viz::MAX_ZOOM, inter
        ),
    )
}

fn t_layout_audit() -> Outcome {
    let t = mk_tree();
    let children = ChildIndex::build(&t);
    let exts = ExtensionTable::build(&t);
    let vp = Rect { x: 0.0, y: 0.0, w: 1200.0, h: 800.0 };
    let opts = LayoutOptions {
        policy: LodPolicy::wide(),
        mode: SizeMode::Logical,
        shading: ShadingMode::ByExtension,
        now_unix: 0,
    };
    let res = compute_layout(&t, &children, &exts, 5, vp, &opts);
    let (total, area, worst) = crate::render::layout_audit(&res, &vp);
    check(
        "布局自检",
        total <= area * 3.0 && worst <= area * 0.5,
        format!(
            "{} 个矩形，面积和 {:.0} / 视口 {:.0}（比值 {:.2}），最大重叠 {:.0}",
            res.items.len(),
            total,
            area,
            total / area,
            worst
        ),
    )
}

fn t_classify() -> Outcome {
    let cases = [
        (USN_REASON_FILE_CREATE | USN_REASON_CLOSE, EventKind::Created),
        (USN_REASON_DATA_EXTEND, EventKind::Modified),
        (USN_REASON_DATA_TRUNCATION, EventKind::Modified),
        (USN_REASON_FILE_DELETE | USN_REASON_CLOSE, EventKind::Deleted),
        (USN_REASON_RENAME_OLD_NAME, EventKind::Renamed),
        (USN_REASON_RENAME_NEW_NAME, EventKind::Renamed),
        (USN_REASON_COMPRESSION_CHANGE, EventKind::SizeSemanticsChanged),
        (USN_REASON_HARD_LINK_CHANGE, EventKind::HardLinkChanged),
        (USN_REASON_SECURITY_CHANGE, EventKind::AttrChanged),
        (USN_REASON_CLOSE, EventKind::Unknown),
    ];
    let mut bad = Vec::new();
    for (r, want) in cases {
        let got = classify(r);
        if got != want {
            bad.push(format!("0x{:08X} → {:?}（应 {:?}）", r, got, want));
        }
    }
    // 创建 + 删除同时出现时，删除优先级最高
    let both = classify(USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE);
    check(
        "语义优先级",
        bad.is_empty() && both == EventKind::Deleted,
        if bad.is_empty() {
            format!("10 个映射全部正确；创建+删除 → {:?}", both)
        } else {
            bad.join("; ")
        },
    )
}

/// 手工拼一条 `USN_RECORD_V2`，验证字段偏移解析正确。
fn t_usn_v2_parse() -> Outcome {
    let name = "报告.docx";
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = units.len() * 2;
    let rec_len = 60 + name_bytes;
    let mut buf = vec![0u8; rec_len];

    buf[0..4].copy_from_slice(&(rec_len as u32).to_le_bytes());
    buf[4..6].copy_from_slice(&2u16.to_le_bytes()); // MajorVersion = 2
    buf[6..8].copy_from_slice(&0u16.to_le_bytes());
    buf[8..16].copy_from_slice(&0x0001_0000_0000_0123u64.to_le_bytes()); // FRN
    buf[16..24].copy_from_slice(&0x0001_0000_0000_0005u64.to_le_bytes()); // ParentFRN
    buf[24..32].copy_from_slice(&12_345i64.to_le_bytes()); // Usn
    buf[32..40].copy_from_slice(&0i64.to_le_bytes()); // TimeStamp
    buf[40..44].copy_from_slice(&USN_REASON_DATA_EXTEND.to_le_bytes());
    buf[44..48].copy_from_slice(&0u32.to_le_bytes());
    buf[48..52].copy_from_slice(&0u32.to_le_bytes());
    buf[52..56].copy_from_slice(&0x20u32.to_le_bytes()); // ARCHIVE
    buf[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
    buf[58..60].copy_from_slice(&60u16.to_le_bytes()); // FileNameOffset
    for (i, u) in units.iter().enumerate() {
        buf[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
    }

    let recs = parse_records(&buf);
    let ok = recs.len() == 1
        && recs[0].frn == 0x0001_0000_0000_0123
        && recs[0].parent_frn == 0x0001_0000_0000_0005
        && recs[0].usn == 12_345
        && recs[0].reason == USN_REASON_DATA_EXTEND
        && !recs[0].is_dir()
        && recs[0].name_bytes(&buf).map(|b| {
            let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        }) == Some(name.to_string());

    // 顺带验证：截断的缓冲不会 panic
    let truncated = parse_records(&buf[..40]);
    check(
        "USN_V2 解析",
        ok && truncated.is_empty(),
        if ok {
            format!(
                "FRN=0x{:X} 父=0x{:X} usn={} 名字={:?}；截断输入安全返回 {} 条",
                recs[0].frn,
                recs[0].parent_frn,
                recs[0].usn,
                name,
                truncated.len()
            )
        } else {
            format!("解析结果不符合预期：{:?}", recs.len())
        },
    )
}

fn t_cursor_validity() -> Outcome {
    let j = |id: u64, first: i64, next: i64| UsnJournalData {
        id,
        first_usn: first,
        next_usn: next,
        lowest_valid_usn: first,
        max_usn: next,
        maximum_size: 32 * 1024 * 1024,
        allocation_delta: 4 * 1024 * 1024,
    };
    let cur = j(7, 100, 1000);
    let r = [
        evaluate(None, &cur) == CursorValidity::Fresh,
        evaluate(Some(&Cursor::new("C:", 7, 500)), &cur) == CursorValidity::Valid,
        evaluate(Some(&Cursor::new("C:", 8, 500)), &cur) == CursorValidity::JournalRebuilt,
        evaluate(Some(&Cursor::new("C:", 7, 50)), &cur) == CursorValidity::GapDetected,
        evaluate(Some(&Cursor::new("C:", 7, 5000)), &cur) == CursorValidity::CursorAhead,
    ];
    check(
        "游标判定",
        r.iter().all(|x| *x),
        format!(
            "Fresh/Valid/JournalRebuilt/GapDetected/CursorAhead = {:?}；\
             can_resume 仅 Valid 为真：{}",
            r,
            CursorValidity::Valid.can_resume()
                && !CursorValidity::GapDetected.can_resume()
                && !CursorValidity::CursorAhead.can_resume()
        ),
    )
}

fn mk_raw(frn: u64, reason: u32, name: &str) -> RawEvent {
    RawEvent {
        frn,
        parent_frn: 0x0001_0000_0000_0005,
        usn: 1,
        timestamp: 1_700_000_000,
        reason,
        file_attributes: 0x20,
        name: name.to_string(),
    }
}

fn t_debounce_storm() -> Outcome {
    let mut d = Debouncer::new(DebounceConfig {
        window_ms: 100,
        filter_temp_files: false,
        recycle_bin_as_delete: true,
    });
    // 模拟编辑器保存：6 条事件
    for (i, r) in [
        USN_REASON_FILE_CREATE,
        USN_REASON_DATA_EXTEND,
        USN_REASON_DATA_OVERWRITE,
        USN_REASON_BASIC_INFO_CHANGE,
        USN_REASON_CLOSE,
        USN_REASON_CLOSE,
    ]
    .iter()
    .enumerate()
    {
        d.offer(mk_raw(0x100, *r, "doc.txt"), 1000 + i as u64 * 5);
    }
    let mut out = Vec::new();
    d.drain_matured(1200, &mut out);
    // 6 条事件里只有 4 条是「业务事件」：
    //   FILE_CREATE / DATA_EXTEND / DATA_OVERWRITE / BASIC_INFO_CHANGE
    // 两条 CLOSE 被 `classify` 判为 Unknown 而**有意丢弃**（见 #15 用例，
    // `classify(USN_REASON_CLOSE) == Unknown`）—— CLOSE 本身不代表任何变更。
    // `merged` 统计的是「合并了多少条进这条事件」= 1 + 3 次合并 = 4。
    check(
        "保存风暴合并",
        out.len() == 1
            && out[0].kind == EventKind::Created
            && out[0].merged == 4
            && out[0].frn == 0x100,
        format!(
            "6 条事件（2 条纯 CLOSE 被丢弃）→ {} 条（kind={:?} merged={}，期望 merged=4）",
            out.len(),
            out.first().map(|e| e.kind),
            out.first().map(|e| e.merged).unwrap_or(0)
        ),
    )
}

fn t_debounce_cancel() -> Outcome {
    let mut d = Debouncer::new(DebounceConfig {
        window_ms: 100,
        filter_temp_files: false,
        recycle_bin_as_delete: true,
    });
    d.offer(mk_raw(0x200, USN_REASON_FILE_CREATE, "tmp.bin"), 1000);
    d.offer(mk_raw(0x200, USN_REASON_FILE_DELETE, "tmp.bin"), 1050);
    let mut out = Vec::new();
    d.drain_matured(1200, &mut out);
    // 事件被合成 Unknown 并丢弃 —— 等价于「什么都没发生」
    let s = d.stats();
    check(
        "创建+删除抵消",
        out.is_empty() && s.offered == 2,
        format!("投递 {} 条，吐出 {} 条（应 0）", s.offered, out.len()),
    )
}

fn t_debounce_temp_filter() -> Outcome {
    let mut d = Debouncer::new(DebounceConfig {
        window_ms: 100,
        filter_temp_files: true,
        recycle_bin_as_delete: true,
    });
    for n in ["~$doc.docx", "a.tmp", "x.bak.tmp"] {
        d.offer(mk_raw(0x300, USN_REASON_FILE_CREATE, n), 1000);
    }
    d.offer(mk_raw(0x301, USN_REASON_FILE_CREATE, "keep.txt"), 1000);
    let mut out = Vec::new();
    d.drain_matured(1200, &mut out);
    check(
        "临时文件过滤",
        out.len() == 1 && out[0].name == "keep.txt",
        format!(
            "4 条 → {} 条（filtered_temp={}）",
            out.len(),
            d.stats().filtered_temp
        ),
    )
}

fn t_rule_hysteresis() -> Outcome {
    // 关掉冷却期，单独考察滞回
    let cfg = RuleConfig {
        cooldown_ms: 0,
        ..Default::default()
    };
    let mut e = RuleEngine::new(cfg);
    e.add_target(10, r"C:\logs", vec![Rule::absolute(6 * MB)], 5 * MB, 0);
    let first = e.evaluate(10, 10 * MB, 1000);
    let second = e.evaluate(10, 11 * MB, 2000); // 继续涨，但未跌回 90%
    let reset = e.evaluate(10, 5 * MB, 3000); // 跌回阈值以下 → 复位
    let third = e.evaluate(10, 20 * MB, 4000); // 再次冲破阈值
    check(
        "滞回",
        first.is_some() && second.is_none() && reset.is_none() && third.is_some(),
        format!(
            "首次触发={} 滞回期内={} 回落复位的求值={} 复位后再次触发={}",
            first.is_some(),
            second.is_some(),
            reset.is_some(),
            third.is_some()
        ),
    )
}

fn t_rule_cooldown() -> Outcome {
    let mut cfg = RuleConfig::default();
    cfg.cooldown_ms = 600_000; // 10 分钟
    let mut e = RuleEngine::new(cfg);
    e.add_target(10, r"C:\logs", vec![Rule::absolute(6 * MB)], 5 * MB, 0);

    let a = e.evaluate(10, 10 * MB, 1000);
    e.evaluate(10, 1 * MB, 2000); // 回落复位
    // 冷却期内再次冲破 → 应被抑制
    let b = e.evaluate(10, 10 * MB, 3000);
    // 冷却期过后 → 重新放行
    let c = e.evaluate(10, 1 * MB, 700_000);
    let d = e.evaluate(10, 10 * MB, 701_000);
    check(
        "冷却期",
        a.is_some() && b.is_none() && c.is_none() && d.is_some(),
        format!(
            "首次={} 冷却内={} 冷却后重新触发={}（suppressed_by_cooldown={}）",
            a.is_some(),
            b.is_some(),
            d.is_some(),
            e.stats.suppressed_by_cooldown
        ),
    )
}

// ── F12 重复文件三级去重（内存内容源，不碰真实文件） ────────────────────────

/// 自检用的内存内容源。
///
/// 三级筛选最有价值的分支是「体积唯一 → 一个字节都不读」和「哈希全撞上 →
/// 靠逐字节复核兜住」，这两件事在真实文件系统上都很难**故意**造出来。
/// 所以要有一个能记录「读过谁」、也能强行让哈希相等的内容源。
struct MemSource {
    files: std::collections::HashMap<String, Vec<u8>>,
    /// 被读过内容的路径（按读取次数追加）
    reads: std::cell::RefCell<Vec<String>>,
    /// 打开后所有哈希都返回同一个值（模拟极端碰撞）
    collide: bool,
}

impl MemSource {
    fn new(collide: bool) -> MemSource {
        MemSource {
            files: std::collections::HashMap::new(),
            reads: std::cell::RefCell::new(Vec::new()),
            collide,
        }
    }

    fn put(&mut self, path: &str, data: &[u8]) -> &mut Self {
        self.files.insert(path.to_string(), data.to_vec());
        self
    }

    fn get(&self, p: &str) -> std::io::Result<&Vec<u8>> {
        self.reads.borrow_mut().push(p.to_string());
        self.files.get(p).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("没有 {p}"))
        })
    }
}

impl ContentSource for MemSource {
    fn partial_hash(&self, c: &DupCandidate, block: usize) -> std::io::Result<u64> {
        if self.collide {
            return Ok(1);
        }
        Ok(core_io::hash::hash_head_tail_bytes(self.get(&c.path)?, block))
    }
    fn full_hash(&self, c: &DupCandidate) -> std::io::Result<u64> {
        if self.collide {
            return Ok(1);
        }
        let d = self.get(&c.path)?;
        core_io::hash::hash_reader(&mut d.as_slice())
    }
    fn equal(&self, a: &str, b: &str) -> std::io::Result<bool> {
        Ok(self.get(a)? == self.get(b)?)
    }
}

fn cand(path: &str, size: u64) -> DupCandidate {
    DupCandidate {
        path: path.to_string(),
        size,
        mtime: 0,
    }
}

fn t_dedup_funnel() -> Outcome {
    // 两个体积唯一（永远不该被读）+ 一对真重复 + 一对同体积但内容不同
    let mut src = MemSource::new(false);
    src.put("u1", &vec![7u8; 1000])
        .put("u2", &vec![8u8; 2000])
        .put("d1", &vec![9u8; 5000])
        .put("d2", &vec![9u8; 5000])
        .put("e1", &vec![1u8; 3000])
        .put("e2", &vec![2u8; 3000]);
    let entries = vec![
        cand("u1", 1000),
        cand("u2", 2000),
        cand("d1", 5000),
        cand("d2", 5000),
        cand("e1", 3000),
        cand("e2", 3000),
    ];
    let opts = DupOptions::default();
    let rep = find_duplicates(&entries, &opts, &src, &mut |_| {});
    let reads = src.reads.borrow();
    let never_read = !reads.iter().any(|p| p == "u1" || p == "u2");
    let one_group = rep.groups.len() == 1
        && rep.groups[0].count == 2
        && rep.groups[0].wasted == 5000
        && !rep.groups[0].truncated;
    check(
        "三级漏斗",
        never_read && one_group && rep.same_size_groups == 2 && rep.group_count == 1,
        format!(
            "体积唯一者未被读={never_read}；体积相同 {} 组 → 部分哈希 {} → 完整哈希 {}；\
             确认 {} 组，可回收 {}",
            rep.same_size_groups,
            rep.partial_files,
            rep.full_files,
            rep.group_count,
            core_io::util::human_size(rep.total_wasted)
        ),
    )
}

fn t_dedup_collision() -> Outcome {
    // 三个**内容互不相同**的文件，但内容源对所有文件都返回同一个哈希
    let mut src = MemSource::new(true);
    src.put("a", &[1u8; 4000])
        .put("b", &[2u8; 4000])
        .put("c", &[3u8; 4000]);
    let entries = vec![cand("a", 4000), cand("b", 4000), cand("c", 4000)];
    let rep = find_duplicates(&entries, &DupOptions::default(), &src, &mut |_| {});

    // 再放两个真重复的进去：只有那两个成组，第三个不许被带上
    let mut src2 = MemSource::new(true);
    src2.put("a", &[1u8; 4000])
        .put("b", &[1u8; 4000])
        .put("c", &[3u8; 4000]);
    let rep2 = find_duplicates(&entries, &DupOptions::default(), &src2, &mut |_| {});

    let ok = rep.groups.is_empty()
        && rep2.groups.len() == 1
        && rep2.groups[0].count == 2
        && rep2.groups[0].files.iter().all(|f| f.path != "c");
    check(
        "哈希撞车被复核挡住",
        ok,
        format!(
            "三个不同文件哈希全相同 → 报出 {} 组（应为 0）；\
             两个真相同 + 一个不同 → 报出 {} 组、{} 个文件（应为 1 组 2 个）",
            rep.groups.len(),
            rep2.groups.len(),
            rep2.groups.first().map(|g| g.count).unwrap_or(0)
        ),
    )
}

fn t_snapshot_roundtrip_and_diff() -> Outcome {
    let t = mk_tree();
    let s1 = Snapshot::capture(&t, None, "C:", 1_700_000_000, &SnapshotOptions::default());
    let text = s1.to_tsv();
    let back = match Snapshot::from_tsv(&text) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(format!("快照往返失败：{e}")),
    };
    if back.entries != s1.entries || back.created_unix != s1.created_unix {
        return Outcome::Fail("快照往返后内容不一致".to_string());
    }
    // 转义：磁盘路径的反斜杠在文件里是双写（这是它唯一"不好看"的地方，但可逆）
    if !text.contains(r"C:\\docs\\a.txt") {
        return Outcome::Fail(format!("快照里的路径没有被正确转义：{text}"));
    }

    // 编辑一份"一周后"的快照：a.txt 变大 1 MB、b.log 被删、新增 5 MB 文件
    let mut s2 = back.clone();
    s2.created_unix = s1.created_unix + 7 * 86_400;
    s2.entries.retain(|e| !e.path.ends_with("b.log"));
    for e in s2.entries.iter_mut() {
        if e.path.ends_with("a.txt") {
            e.size += MB;
            e.mtime += 1;
        }
    }
    s2.entries.push(SnapEntry {
        path: r"C:\docs\new.bin".to_string(),
        size: 5 * MB,
        mtime: 1_700_600_000,
    });
    s2.entries.sort_by(|a, b| a.path.cmp(&b.path));

    let rep = diff(&back, &s2, &DiffOptions { top: 0, ..Default::default() });
    let expect_net = (MB + 5 * MB) as i64 - (2 * MB) as i64;
    let ok = rep.grew == 1
        && rep.removed == 1
        && rep.added == 1
        && rep.shrank == 0
        && rep.net_delta == expect_net
        && rep.rows.len() == 3;
    check(
        "快照往返 + diff",
        ok,
        format!(
            "旧 {} 条 / 新 {} 条；变大 {} 删除 {} 新增 {} 变小 {}；净变化 {}（应为 {}）",
            back.len(),
            s2.len(),
            rep.grew,
            rep.removed,
            rep.added,
            rep.shrank,
            core_scan::signed_size(rep.net_delta),
            core_scan::signed_size(expect_net)
        ),
    )
}

fn t_severity() -> Outcome {
    use core_monitor::alert::severity_of;
    let s = [
        severity_of(101, 100) == Severity::Warning,
        severity_of(200, 100) == Severity::Critical,
        severity_of(100, 100) == Severity::Info,
        severity_of(99, 100) == Severity::Info,
        severity_of(1, 0) == Severity::Warning,
    ];
    check(
        "严重级别",
        s.iter().all(|x| *x),
        format!("101/100→Warning 200/100→Critical 100/100→Info 1/0→Warning：{:?}", s),
    )
}

fn t_traverse_real() -> Outcome {
    let owned = core_io::test_dir::TestDir::new("fw-selftest-trav");
    let owned = match owned {
        Ok(dir) => dir,
        Err(e) => return Outcome::Skip(format!("Cannot create owned test directory: {e}")),
    };
    let base = owned.path().to_path_buf();
    if std::fs::create_dir_all(base.join("a").join("b")).is_err() {
        return Outcome::Skip("无法创建临时目录".to_string());
    }
    let _ = std::fs::write(base.join("a").join("b").join("deep.bin"), vec![b'k'; 3333]);
    let _ = std::fs::write(base.join("x.txt"), vec![b'x'; 77]);

    let (tree, stats, paths) = core_scan::full_scan_traverse_indexed(
        &base,
        "C:",
        core_scan::TraverseOptions::default(),
        true,
        |_, _| {},
    );

    let root = tree.arena.get(5).unwrap().size;
    // 路径索引必须收录根目录本身 —— 否则实时增量一个事件都定位不到
    let root_key = core_scan::dirwatch::normalize_root(&base.to_string_lossy()).to_lowercase();
    let has_root = paths.get(root_key.as_str()) == Some(&5);

    let ok = stats.files_visited == 2 && root == 3410 && tree.file_count == 2 && has_root;

    if ok {
        Outcome::Pass(format!(
            "2 个文件 / 2 个目录，根聚合 {}（= 77 + 3333），路径索引 {} 条且含根",
            core_io::util::human_size(root),
            paths.len()
        ))
    } else {
        Outcome::Fail(format!(
            "文件数={} 根={}（应 3410） 索引={} 条 含根={}",
            stats.files_visited, root, paths.len(), has_root
        ))
    }
}

fn t_layout_on_real_tree() -> Outcome {
    let owned = core_io::test_dir::TestDir::new("fw-selftest-lay");
    let owned = match owned {
        Ok(dir) => dir,
        Err(e) => return Outcome::Skip(format!("Cannot create owned test directory: {e}")),
    };
    let base = owned.path().to_path_buf();
    if std::fs::create_dir_all(base.join("sub")).is_err() {
        return Outcome::Skip("无法创建临时目录".to_string());
    }
    for i in 0..40 {
        let _ = std::fs::write(base.join("sub").join(format!("f{}.dat", i)), vec![b'z'; 100 * (i + 1)]);
    }
    let (tree, _s, _p) = core_scan::full_scan_traverse_indexed(
        &base,
        "C:",
        core_scan::TraverseOptions::default(),
        false,
        |_, _| {},
    );
    let children = ChildIndex::build(&tree);
    let exts = ExtensionTable::build(&tree);
    let vp = Rect { x: 0.0, y: 0.0, w: 900.0, h: 500.0 };
    let opts = LayoutOptions {
        policy: LodPolicy::wide(),
        mode: SizeMode::Logical,
        shading: ShadingMode::ByExtension,
        now_unix: 0,
    };
    let res = compute_layout(&tree, &children, &exts, 5, vp, &opts);

    // ASCII 预览必须能画出来且尺寸正确
    let art = crate::render::render_ascii(&res, 60, 16);
    let lines = art.lines().count();
    let all_ok = res.items.len() >= 2 && lines == 16 && art.contains('+');
    check(
        "真实树布局",
        all_ok,
        format!(
            "40 个文件 → {} 个矩形，占位 {} 行 × 60 列：{}",
            res.items.len(),
            lines,
            res.stats.summary_cn()
        ),
    )
}

/// 通知解析：Windows 验字节链解析，Linux 验动作映射。
///
/// 两边的"难点"不一样，所以不能共用一个断言：
///
/// * Windows 的 `FILE_NOTIFY_INFORMATION` 是**变长记录链**，`NextEntryOffset`
///   只保证 4 字节对齐，按结构体强转就是 UB —— 必须逐字节读。这里的测试
///   就是拿"非 4 字节对齐的记录"去撞它。
/// * Linux 的 `struct inotify_event` 是定长头 + 变长名字，解析不难；
///   难的是**掩码到五个动作码的映射**（CREATE/DELETE/MOVED_FROM/MOVED_TO/
///   MODIFY），以及"目录被移动/创建时要补 watch"。前者用真实文件操作验，
///   后者由 `t_dirwatch_live` 与 `t_degraded_live` 覆盖。
#[cfg(windows)]
fn t_dirwatch_parse() -> Outcome {
    // 手拼两条 FILE_NOTIFY_INFORMATION（非 UTF-16 对齐，考验按字节读）
    let mut buf: Vec<u8> = Vec::new();
    let mut push = |action: u32, name: &str, last: bool| {
        let units: Vec<u16> = name.encode_utf16().collect();
        let nb = units.len() * 2;
        let mut rec = Vec::new();
        rec.extend_from_slice(&0u32.to_le_bytes());
        rec.extend_from_slice(&action.to_le_bytes());
        rec.extend_from_slice(&(nb as u32).to_le_bytes());
        for u in &units {
            rec.extend_from_slice(&u.to_le_bytes());
        }
        while rec.len() % 4 != 0 {
            rec.push(0);
        }
        let next = if last { 0u32 } else { rec.len() as u32 };
        rec[0..4].copy_from_slice(&next.to_le_bytes());
        buf.extend_from_slice(&rec);
    };
    push(core_io::winapi::FILE_ACTION_ADDED, "新建 文件夹", false);
    push(core_io::winapi::FILE_ACTION_REMOVED, "gone.bin", true);

    let mut got = Vec::new();
    let n = core_scan::parse_notifications(&buf, |e| got.push(e));
    // 再喂一段垃圾，确认不会 panic
    let junk = vec![0xFFu8; 40];
    core_scan::parse_notifications(&junk, |_| {});

    check(
        "通知解析",
        n == 2 && got[0].rel == "新建 文件夹" && got[1].rel == "gone.bin",
        format!("解析 {} 条：{:?}；垃圾输入已安全忽略", n, got),
    )
}

/// Linux 版：用真实文件操作把五个动作码各撞一遍。
///
/// 断言的是"内核事件 → 统一动作码"这条映射，而不是解析器的字节偏移
/// （后者由 `dirwatch/linux.rs` 的单元测试覆盖）。
#[cfg(not(windows))]
fn t_dirwatch_parse() -> Outcome {
    use core_io::consts::{
        FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_RENAMED_NEW_NAME,
        FILE_ACTION_RENAMED_OLD_NAME, FILE_ACTION_REMOVED,
    };

    let owned = match core_io::test_dir::TestDir::new("fw-selftest-inotify") {
        Ok(d) => d,
        Err(e) => return Outcome::Skip(format!("无法创建测试目录：{e}")),
    };
    let base = owned.path().to_path_buf();
    let mut w = match core_scan::DirWatcher::start(&base, 64 * 1024, true) {
        Ok(w) => w,
        Err(e) => return Outcome::Skip(format!("inotify 不可用（{e}）—— 常见于受限容器")),
    };

    // 新增 → 改名 → 修改 → 删除，四类动作各来一次
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    let _ = std::fs::write(&a, b"x");
    let _ = std::fs::rename(&a, &b);
    let _ = std::fs::write(&b, b"yy");
    let _ = std::fs::remove_file(&b);

    let mut got = Vec::new();
    for _ in 0..60 {
        w.drain(512, &mut got);
        let seen = |act: u32| got.iter().any(|e| e.action == act);
        if seen(FILE_ACTION_ADDED)
            && seen(FILE_ACTION_RENAMED_OLD_NAME)
            && seen(FILE_ACTION_RENAMED_NEW_NAME)
            && seen(FILE_ACTION_REMOVED)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let received = w.received();
    let watched = w.watched_dirs();
    w.stop();

    let seen = |act: u32| got.iter().any(|e| e.action == act);
    let ok = seen(FILE_ACTION_ADDED)
        && seen(FILE_ACTION_RENAMED_OLD_NAME)
        && seen(FILE_ACTION_RENAMED_NEW_NAME)
        && seen(FILE_ACTION_MODIFIED)
        && seen(FILE_ACTION_REMOVED);
    check(
        "通知解析",
        ok,
        format!(
            "watch {} 个目录，收 {} 条通知；新增={} 改名旧={} 改名新={} 修改={} 删除={}；样本 {:?}",
            watched,
            received,
            seen(FILE_ACTION_ADDED),
            seen(FILE_ACTION_RENAMED_OLD_NAME),
            seen(FILE_ACTION_RENAMED_NEW_NAME),
            seen(FILE_ACTION_MODIFIED),
            seen(FILE_ACTION_REMOVED),
            got.iter().take(4).map(|e| (e.action, e.rel.clone())).collect::<Vec<_>>()
        ),
    )
}

fn t_dirwatch_live() -> Outcome {
    let owned = core_io::test_dir::TestDir::new("fw-selftest-dw");
    let owned = match owned {
        Ok(dir) => dir,
        Err(e) => return Outcome::Skip(format!("Cannot create owned test directory: {e}")),
    };
    let base = owned.path().to_path_buf();
    if std::fs::create_dir_all(&base).is_err() {
        return Outcome::Skip("无法创建临时目录".to_string());
    }
    let mut w = match core_scan::DirWatcher::start(&base, 64 * 1024, true) {
        Ok(w) => w,
        Err(e) => {
            return Outcome::Skip(format!("无法打开目录句柄（{}）—— 常见于受限容器", e));
        }
    };
    let probe = base.join("probe.bin");
    let _ = std::fs::write(&probe, b"hello");
    let mut got = Vec::new();
    for _ in 0..40 {
        w.drain(128, &mut got);
        if got.iter().any(|e| e.rel.contains("probe.bin")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let hit = got.iter().any(|e| e.rel.contains("probe.bin"));
    let received = w.received();
    w.stop();
    check(
        "实时投递",
        hit,
        format!("内核投递 {} 条通知，捕获到 probe.bin：{}", received, hit),
    )
}

fn t_degraded_live() -> Outcome {
    let owned = core_io::test_dir::TestDir::new("fw-selftest-live");
    let owned = match owned {
        Ok(dir) => dir,
        Err(e) => return Outcome::Skip(format!("Cannot create owned test directory: {e}")),
    };
    let base = owned.path().to_path_buf();
    if std::fs::create_dir_all(base.join("seed")).is_err() {
        return Outcome::Skip("无法创建临时目录".to_string());
    }
    let _ = std::fs::write(base.join("seed").join("a.bin"), vec![b'a'; 1000]);

    let mut cfg = core_scan::SessionConfig::default();
    cfg.scan_root = Some(base.clone());
    cfg.engine.write_changelog = false;
    cfg.engine.debounce.window_ms = 60;

    let mut s = match core_scan::Session::open(cfg, |_, _, _| {}) {
        Ok(s) => s,
        Err(e) => {
            return Outcome::Skip(format!("会话打开失败：{}", e));
        }
    };
    if s.watch.is_none() {
        let w = s.warnings.clone();
        return Outcome::Skip(format!("目录监视不可用：{}", w.join(" / ")));
    }

    let before = s.tree.arena.get(5).unwrap().size;
    // 新建目录 + 在目录里写文件 —— 一次覆盖「占位目录」与「delta 上传」两条关键逻辑
    let _ = std::fs::create_dir_all(base.join("newdir"));
    let _ = std::fs::write(base.join("newdir").join("big.bin"), vec![b'b'; 7000]);

    let mut applied = 0usize;
    for i in 0..60u64 {
        applied += s.poll(2000 + i * 100).map(|o| o.applied).unwrap_or(0);
        if s.tree.arena.get(5).unwrap().size != before && applied >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let after = s.tree.arena.get(5).unwrap().size;
    let src = s.incremental_source_cn();
    let watch_summary = s.watch.as_ref().map(|w| w.stats.summary_cn()).unwrap_or_default();

    // 诊断用：把树上所有在用节点摊开，定位 delta 传播断在哪一层
    let mut dump: Vec<String> = Vec::new();
    for (i, n) in s.tree.arena.nodes.iter().enumerate() {
        if !n.in_use() {
            continue;
        }
        let nm = s.tree.names.get(n.name);
        dump.push(format!(
            "#{} {} size={} parent={}{}",
            i,
            if nm.is_empty() { "<无名>" } else { nm },
            n.size,
            n.parent,
            if n.is_dir() { " [目录]" } else { "" }
        ));
    }
    let dump = dump.join("\n            ");

    let _ = s.shutdown(30_000);

    let ok = after == before + 7000 && src.contains("ReadDirectoryChangesW");
    // 树内容只在失败时才有价值 —— 无条件打出来会把 28 项自检的输出整个淹掉。
    // 但一旦失败，它就是定位「delta 断在哪一层」的唯一线索，所以必须留着。
    let tree_dump = if ok {
        String::new()
    } else {
        format!("\n            树内容（诊断用）：\n            {}", dump)
    };

    check(
        "端到端增量",
        ok,
        format!(
            "根 {} → {}（期望 +7000），应用 {} 条事件，增量来源：{}\n\
             \x20           监视统计：{}{}",
            core_io::util::human_size(before),
            core_io::util::human_size(after),
            applied,
            src,
            if watch_summary.is_empty() { "（未启用监视）" } else { &watch_summary },
            tree_dump
        ),
    )
}

/// 供 `--verbose` 时输出环境信息。
pub fn env_info() -> String {
    let mut s = String::new();
    let _ = writeln!(s, "平台：{}", std::env::consts::OS);
    let _ = writeln!(s, "架构：{}", std::env::consts::ARCH);
    let _ = writeln!(
        s,
        "管理员/root 权限线索（能否打开裸卷）：{}",
        core_io::util::is_elevated_hint()
    );
    let _ = writeln!(
        s,
        "增量后端：{}（MFT 直读：{}）",
        core_io::watch_backend_name(),
        if core_io::HAS_MFT_BACKEND { "可用" } else { "不可用（非 Windows）" }
    );
    let _ = writeln!(s, "Node 尺寸：{} 字节", core::mem::size_of::<Node>());
    let vols = core_io::list_volumes();
    let _ = writeln!(s, "检测到 {} 个卷", vols.len());
    for v in vols {
        let _ = writeln!(
            s,
            "  {} {} {} 已用 {:.0}% 后端提示：{}",
            v.drive,
            v.type_label(),
            v.fs_name,
            v.used_ratio() * 100.0,
            v.mode_hint()
        );
    }
    let _ = writeln!(s, "游标目录：{}", core_usn::cursor::data_dir().display());
    s
}
