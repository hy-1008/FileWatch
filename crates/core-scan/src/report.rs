//! 单文件离线 HTML 报告（E3）。
//!
//! 产物是**一个** `.html`：内联样式 + 内联 SVG，没有任何外链（JS/CSS/图片/字体）。
//! 所以它可以塞进邮件、丢进共享盘、在断网的机器上双击打开 —— 这正是"给领导/同事
//! 看一眼磁盘到底被什么吃了"的实际用法。CSV 做不到这件事（要么自己画图，
//! 要么看不到比例）。
//!
//! # 为什么生成逻辑在引擎里而不是界面里
//!
//! CLI 的 `report` 与界面上的「导出报告」必须是**同一份排版**，否则用户会拿到
//! 两种格式的报告、然后开始怀疑数字对不上。所以模板只有这里一份，
//! 两边都调 [`build_html`]。
//!
//! # 三条如实呈现的约定
//!
//! 1. **面积图只画扫描根的直接子项**（默认前 30 项，其余合并成「其他」）。
//!    递归画到文件级会让报告变成一张几万个方块的图 —— 既看不清也打不开。
//! 2. **面积按"占用大小"**（磁盘上真正吃掉的簇），表里同时给逻辑大小：
//!    稀疏文件与压缩文件的这两个数会差很多，只给一个就是在误导。
//! 3. **颜色只表示名次**，不表示文件类型。界面里颜色 = 类型（`ext_id % 15`），
//!    但报告里的方块是"目录"，给目录套类型色需要递归统计每个目录里的主导类型，
//!    那既慢又容易被误读。所以这里用单色深浅，并在图注里写清楚。

use crate::snapshot::Progress;
use core_index::aggregate::{top_files, SizeMode};
use core_index::arena::NODE_IN_USE;
use core_index::ext_stats::ExtensionTable;
use core_index::{ChildIndex, Tree};
use core_io::util::{human_size, local_time};
use core_monitor::PathRemap;
use core_viz::rect::Rect;
use core_viz::squarify::squarify;

/// 报告参数。
#[derive(Debug, Clone)]
pub struct ReportOptions {
    pub title: String,
    /// 扫描根（显示路径）。空串时用 `tree.drive` 拼一个。
    pub root: String,
    /// 生成时间（Unix 秒）。0 = 不写时间。
    pub created_unix: i64,
    /// 每个榜最多多少行，也决定面积图画多少个方块。
    pub top: usize,
    pub width: f32,
    pub height: f32,
}

impl Default for ReportOptions {
    fn default() -> Self {
        ReportOptions {
            title: "FileWatch 扫描报告".to_string(),
            root: String::new(),
            created_unix: 0,
            top: 30,
            width: 900.0,
            height: 480.0,
        }
    }
}

/// 生成整份 HTML。
///
/// `ext` 由调用方建好传进来（`ExtensionTable::build` 是 O(n)，界面上本来就有一份，
/// 这里再建一次纯属浪费）。
pub fn build_html(
    tree: &Tree,
    ext: &ExtensionTable,
    remap: Option<&PathRemap>,
    opts: &ReportOptions,
) -> String {
    build_html_with(tree, ext, remap, opts, &mut |_, _| {})
}

/// 带进度回调的 [`build_html`]（`done`/`total` 是**遍历到的节点数**）。
///
/// 进度只在两处真正耗时的地方报：建子节点索引（O(n)）与取 Top-N 大文件（O(n)）。
pub fn build_html_with(
    tree: &Tree,
    ext: &ExtensionTable,
    remap: Option<&PathRemap>,
    opts: &ReportOptions,
    on: &mut Progress<'_>,
) -> String {
    let root_label = if opts.root.is_empty() {
        format!("{}\\", tree.drive) // Linux 上 drive 本身就是挂载点，尾巴多个斜杠不影响阅读
    } else {
        opts.root.clone()
    };

    let idx = ChildIndex::build(tree);
    on(tree.arena.nodes.len() as u64, tree.arena.nodes.len() as u64);
    let kids = idx.children(tree.root);
    let total = tree.total_alloc.max(1);

    // ── 面积图：根的直接子项，前 N 个 + 其他 ─────────────────────────────
    let mut sorted: Vec<u32> = kids.to_vec();
    sorted.sort_unstable_by(|&a, &b| {
        let sa = tree.arena.get(a).map(|n| n.alloc).unwrap_or(0);
        let sb = tree.arena.get(b).map(|n| n.alloc).unwrap_or(0);
        sb.cmp(&sa)
    });
    let shown = opts.top.max(1).min(sorted.len());
    let (head, tail) = sorted.split_at(shown);
    let rest_bytes: u64 = tail
        .iter()
        .map(|&i| tree.arena.get(i).map(|n| n.alloc).unwrap_or(0))
        .sum();

    let mut areas: Vec<f64> = head
        .iter()
        .map(|&i| tree.arena.get(i).map(|n| n.alloc).unwrap_or(0) as f64)
        .collect();
    if rest_bytes > 0 {
        areas.push(rest_bytes as f64);
    }
    let rects = squarify(
        &areas,
        Rect::new(0.0, 0.0, opts.width.max(320.0), opts.height.max(180.0)),
    );

    let mut svg = String::new();
    for (n, &node) in head.iter().enumerate() {
        let r = rects[n];
        let bytes = tree.arena.get(node).map(|x| x.alloc).unwrap_or(0);
        // 方块上的字一定被截断，所以把完整路径放进 <title>：鼠标悬停能看全名。
        let full = path_of(tree, remap, node);
        tile_svg(&mut svg, r, n, tree.name(node), bytes, Some(&full));
    }
    if rest_bytes > 0 {
        let r = rects[head.len()];
        tile_svg(&mut svg, r, 0, "其他", rest_bytes, None);
    }

    // ── 表格 ─────────────────────────────────────────────────────────────
    let mut dir_rows = String::new();
    for &node in head.iter() {
        let bytes = tree.arena.get(node).map(|n| n.alloc).unwrap_or(0);
        let logical = tree.arena.get(node).map(|n| n.size).unwrap_or(0);
        dir_rows.push_str(&format!(
            "<tr><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td></tr>",
            escape_html(tree.name(node)),
            human_size(bytes),
            human_size(logical),
            pct(bytes, total)
        ));
    }

    let tops = top_files(tree, opts.top.max(1), SizeMode::Allocated);
    on(tree.arena.nodes.len() as u64, tree.arena.nodes.len() as u64);
    let mut file_rows = String::new();
    for &node in &tops {
        let n = match tree.arena.get(node) {
            Some(n) if n.flags & NODE_IN_USE != 0 => n,
            _ => continue,
        };
        let path = path_of(tree, remap, node);
        file_rows.push_str(&format!(
            "<tr><td>{}</td><td class=\"p\">{}</td><td class=n>{}</td><td class=n>{}</td></tr>",
            escape_html(tree.name(node)),
            escape_html(&path),
            human_size(n.alloc),
            ymd(n.mtime)
        ));
    }

    let (exts, _rest) = ext.top(opts.top.max(1), SizeMode::Allocated);
    let mut ext_rows = String::new();
    for e in &exts {
        ext_rows.push_str(&format!(
            "<tr><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td></tr>",
            escape_html(e.display_ext()),
            e.count,
            human_size(e.alloc),
            pct(e.alloc, total)
        ));
    }

    let stamp = if opts.created_unix > 0 {
        format!(
            "<p class=meta>扫描根 <code>{}</code> · 生成于 {} · 文件 {} 个 / 目录 {} 个</p>",
            escape_html(&root_label),
            escape_html(&local_time(opts.created_unix)),
            tree.file_count,
            tree.dir_count
        )
    } else {
        format!(
            "<p class=meta>扫描根 <code>{}</code> · 文件 {} 个 / 目录 {} 个</p>",
            escape_html(&root_label),
            tree.file_count,
            tree.dir_count
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root{{--ink:#1b1f24;--dim:#5a6470;--line:#dfe3e8;--acc:#185fa5;--bg:#fff}}
*{{box-sizing:border-box}}
body{{margin:0;padding:28px 20px 40px;background:var(--bg);color:var(--ink);
 font:14px/1.65 -apple-system,"Segoe UI","Microsoft YaHei",sans-serif}}
main{{max-width:960px;margin:0 auto}}
h1{{font-size:20px;font-weight:600;margin:0 0 4px}}
h2{{font-size:15px;font-weight:600;margin:28px 0 10px;padding-bottom:6px;
 border-bottom:1px solid var(--line)}}
.meta{{color:var(--dim);font-size:12px;margin:0}}
.meta code{{background:#f2f4f7;padding:1px 5px;border-radius:4px}}
.cards{{display:flex;flex-wrap:wrap;gap:10px;margin:18px 0 4px}}
.card{{flex:1 1 150px;border:1px solid var(--line);border-radius:10px;padding:10px 14px}}
.card b{{display:block;font-size:18px;font-weight:600;font-variant-numeric:tabular-nums}}
.card span{{color:var(--dim);font-size:12px}}
svg{{width:100%;height:auto;display:block;border:1px solid var(--line);border-radius:10px}}
.cap{{color:var(--dim);font-size:12px;margin:8px 0 0}}
table{{width:100%;border-collapse:collapse;font-size:13px}}
th{{text-align:left;font-weight:500;color:var(--dim);font-size:12px;
 border-bottom:1px solid var(--line);padding:6px 8px;white-space:nowrap}}
td{{border-bottom:1px solid #f0f2f4;padding:5px 8px;vertical-align:top}}
td.n{{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}}
td.p{{color:var(--dim);font-size:12px;word-break:break-all}}
footer{{margin-top:32px;color:var(--dim);font-size:12px;border-top:1px solid var(--line);padding-top:10px}}
</style></head><body><main>
<h1>{title}</h1>
{stamp}
<section class=cards>
<div class=card><b>{alloc}</b><span>占用大小</span></div>
<div class=card><b>{logical}</b><span>逻辑大小</span></div>
<div class=card><b>{files}</b><span>文件</span></div>
<div class=card><b>{dirs}</b><span>目录</span></div>
</section>
<h2>目录面积图</h2>
<svg viewBox="0 0 {w} {h}" role="img" aria-label="按占用大小排列的目录面积图">{svg}</svg>
<p class=cap>面积 = 占用大小（磁盘上真正分配的簇）；只画扫描根的直接子项，前 {shown} 项，其余合并为「其他」。
颜色深浅只表示名次，<b>不表示文件类型</b>（界面里颜色 = 类型，这里不是）。</p>
<h2>最大的目录</h2>
<table><thead><tr><th>目录</th><th class=n>占用</th><th class=n>逻辑</th><th class=n>占比</th></tr></thead>
<tbody>{dir_rows}</tbody></table>
<h2>最大的文件</h2>
<table><thead><tr><th>文件</th><th>路径</th><th class=n>占用</th><th class=n>修改</th></tr></thead>
<tbody>{file_rows}</tbody></table>
<h2>按类型</h2>
<table><thead><tr><th>类型</th><th class=n>文件数</th><th class=n>占用</th><th class=n>占比</th></tr></thead>
<tbody>{ext_rows}</tbody></table>
<footer>只读扫描：FileWatch 没有修改、移动或删除任何文件。这份报告是单个 HTML 文件，
不含脚本与外链，可以离线打开或直接转发。</footer>
</main></body></html>
"#,
        title = escape_html(&opts.title),
        stamp = stamp,
        alloc = human_size(tree.total_alloc),
        logical = human_size(tree.total_size),
        files = tree.file_count,
        dirs = tree.dir_count,
        w = opts.width.max(320.0),
        h = opts.height.max(180.0),
        svg = svg,
        shown = shown,
        dir_rows = dir_rows,
        file_rows = file_rows,
        ext_rows = ext_rows,
    )
}

/// 一块方块：底色按名次取单色深浅，文字在浅底上用深色、深底上用白色。
///
/// `tooltip` 是完整路径（方块上的名字被截断过），`None` 表示"其他"这种合成项。
fn tile_svg(out: &mut String, r: Rect, rank: usize, name: &str, bytes: u64, tooltip: Option<&str>) {
    // 越大越深：第一名用最饱和的那一档。反过来（第一名最浅）会让整张图
    // 看起来"最大的那块最不起眼"，实测在真实目录上非常明显。
    let (fill, ink) = match (tooltip.is_some(), rank) {
        (false, _) => ("#d3d1c7", "#2c2c2a"), // 「其他」
        (true, 0) => ("#378add", "#ffffff"),
        (true, 1..=2) => ("#85b7eb", "#042c53"),
        (true, 3..=6) => ("#b5d4f4", "#042c53"),
        (true, _) => ("#e6f1fb", "#042c53"),
    };
    tile_svg_plain(out, r, name, bytes, fill, ink, tooltip);
}

/// 画一块方块（含标签与数值）。尺寸太小的方块只填色不写字，避免字叠在一起。
fn tile_svg_plain(
    out: &mut String,
    r: Rect,
    name: &str,
    bytes: u64,
    fill: &str,
    ink: &str,
    tooltip: Option<&str>,
) {
    if r.w < 1.0 || r.h < 1.0 {
        return;
    }
    out.push_str("<g>");
    if let Some(t) = tooltip {
        out.push_str(&format!("<title>{}</title>", escape_html(t)));
    }
    out.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{}\" stroke=\"#fff\" stroke-width=\"1\"/>",
        r.x, r.y, r.w, r.h, fill
    ));
    if r.w < 46.0 || r.h < 24.0 {
        out.push_str("</g>");
        return;
    }
    // 字宽估算：CJK 按 12px、ASCII 按 6.5px。宁可少写几个字，也不要溢出到
    // 隔壁方块上 —— SVG 的 text 不会自动裁剪。
    let label = fit_chars(name, (r.w - 10.0) as f64, 12.0, 6.5);
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{}\" font-size=\"12\" font-family=\"-apple-system,'Segoe UI','Microsoft YaHei',sans-serif\">{}</text>",
        r.x + 5.0,
        r.y + 15.0,
        ink,
        escape_html(&label)
    ));
    if r.h >= 40.0 {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{}\" font-size=\"11\" opacity=\"0.85\" font-family=\"-apple-system,'Segoe UI','Microsoft YaHei',sans-serif\">{}</text>",
            r.x + 5.0,
            r.y + 31.0,
            ink,
            human_size(bytes)
        ));
    }
    out.push_str("</g>");
}

/// 按可用宽度截断名字，超出部分用 `…`。
///
/// 逐字符累加宽度：CJK/全角按 12px，其余按 6.5px。不做真正的字体度量 ——
/// 报告里没有字体文件，估宽比"量错字宽"更可控（差几个字符不会破版）。
pub fn fit_chars(s: &str, avail: f64, wide: f64, narrow: f64) -> String {
    // 连一个宽字符都放不下：返回空串，让调用方"不写字"。
    // 这时候硬塞一个省略号只会得到"只有一个 … 的方块"，比不写更难懂。
    if avail < wide {
        return String::new();
    }
    let mut w = 0.0;
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let cw = if (*c as u32) > 0x2E80 { wide } else { narrow };
        // 留 12px 给省略号
        if w + cw > avail - 12.0 && i + 1 < chars.len() {
            out.push('…');
            return out;
        }
        out.push(*c);
        w += cw;
    }
    out
}

fn path_of(tree: &Tree, remap: Option<&PathRemap>, node: u32) -> String {
    let raw = tree.path_of(node);
    match remap {
        Some(r) => r.apply(&raw),
        None => raw,
    }
}

fn pct(part: u64, total: u64) -> String {
    if total == 0 {
        return "—".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / total as f64)
}

/// `YYYY-MM-DD`（**本地时间**）。报告里不需要时分 —— 一屏几十行的表格，
/// 秒级精度只是噪声。
///
/// 用本地时间而不是 UTC：这份报告是拿去跟人说的（"我周五下午扫的"），
/// 而资源管理器里显示的是本地时间，两边差 8 小时就会被当成 bug。
fn ymd(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_string();
    }
    let s = local_time(unix);
    s.get(..10).unwrap_or(&s).to_string()
}

/// HTML 转义。**与 TSV 的转义不是一回事**（那边转 `\t`/`\n`/`\\`），所以不复用
/// `snapshot::escape` —— 名字一旦通用就会有人传错参数，转义规则错了是"看起来
/// 少了个字符"这种最难发现的 bug。
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::aggregate::aggregate;
    use core_ntfs::filename::make_frn;
    use core_ntfs::record::MftRecord;

    fn mk(
        recno: u64,
        parent_frn: u64,
        name: &str,
        is_dir: bool,
        size: u64,
        alloc: u64,
    ) -> (MftRecord, Vec<u8>) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut buf = vec![0u8; 0x30 + 0x42 + units.len() * 2];
        let off = 0x30 + 0x42;
        for (i, u) in units.iter().enumerate() {
            buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        (
            MftRecord {
                frn: make_frn(recno, 1),
                recno,
                seq: 1,
                flags: 1 | if is_dir { 2 } else { 0 },
                hard_link_count: 1,
                parent_frn,
                namespace: 1,
                name_off: off as u32,
                name_chars: units.len() as u16,
                real_size: size,
                alloc_size: alloc,
                mtime: 1_700_000_000,
                fn_flags: 0,
                has_attr_list: false,
                resident_data: false,
                has_named_stream: false,
            },
            buf,
        )
    }

    /// root(5) ─┬─ Windows(10) ─┬─ a.log(11) 1000/4096
    ///          │               └─ <b>.log(12) 2000/4096
    ///          └─ pagefile.sys(20) 5000/8192
    fn sample() -> Tree {
        let mut t = Tree::new("C:", 64);
        let root = make_frn(5, 1);
        for (recno, parent, name, is_dir, size, alloc) in [
            (5u64, root, ".", true, 0, 0),
            (10, root, "Windows", true, 0, 0),
            (11, make_frn(10, 1), "a.log", false, 1000, 4096),
            (12, make_frn(10, 1), "<b>.log", false, 2000, 4096),
            (20, root, "pagefile.sys", false, 5000, 8192),
        ] {
            let (r, b) = mk(recno, parent, name, is_dir, size, alloc);
            t.insert(&r, &b);
        }
        aggregate(&mut t);
        t
    }

    #[test]
    fn escape_html_covers_the_five_dangerous_chars() {
        assert_eq!(escape_html("a<b>&c\"d'e"), "a&lt;b&gt;&amp;c&quot;d&#39;e");
        // 没有特殊字符时不该改动原文（中文、空格、路径分隔符都原样）
        assert_eq!(escape_html("C://用户//a b.txt"), "C://用户//a b.txt");
    }

    #[test]
    fn fit_chars_keeps_short_names_and_ellipsizes_long_ones() {
        assert_eq!(fit_chars("Windows", 200.0, 12.0, 6.5), "Windows");
        // 30 个 ASCII 字符在 100px 里放不下 → 截断并带省略号
        let long = "abcdefghijklmnopqrstuvwxyz0123";
        let s = fit_chars(long, 100.0, 12.0, 6.5);
        assert!(s.ends_with('…'), "{s}");
        assert!(!s.contains("z0"), "截断后不该还留着尾部：{s}");
        // 宽度不足以放下省略号时给空串，而不是画一个破方块
        assert_eq!(fit_chars("abc", 8.0, 12.0, 6.5), "");
    }

    #[test]
    fn html_is_self_contained() {
        let t = sample();
        let ext = ExtensionTable::build(&t);
        let opts = ReportOptions {
            created_unix: 1_700_000_000,
            ..Default::default()
        };
        let html = build_html(&t, &ext, None, &opts);

        // 单文件：不许有任何外链（否则"断网可看"就是假的）
        assert!(!html.contains("http://"), "不许有外链");
        assert!(!html.contains("https://"), "不许有外链");
        // 结构：面积图 + 三张表
        assert!(html.contains("<svg viewBox=\"0 0 900 480\""), "缺少面积图");
        assert_eq!(html.matches("<table>").count(), 3, "应该是目录/文件/类型三张表");
        // 文件名里带 `<` 必须被转义，否则整页解析就断了
        assert!(html.contains("&lt;b&gt;.log"), "文件名没有转义");
        assert!(!html.contains("<b>.log"), "未转义的文件名漏进了 HTML");
    }

    #[test]
    fn root_children_are_drawn_and_rest_is_merged() {
        let t = sample();
        let ext = ExtensionTable::build(&t);
        // 只画 1 个方块：Windows 与 pagefile.sys 抢第一个位置，另一个进「其他」
        let opts = ReportOptions {
            top: 1,
            ..Default::default()
        };
        let html = build_html(&t, &ext, None, &opts);
        assert!(html.contains("其他"), "被挤掉的子项要合并成「其他」");
        assert_eq!(html.matches("<rect ").count(), 2, "1 个真实方块 + 1 个「其他」");
    }
}
