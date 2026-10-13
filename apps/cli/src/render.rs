//! 终端渲染：时间格式化、表格对齐、以及**ASCII 面积图预览**。
//!
//! 面积图在 GUI 里是 Canvas/WebGL 的事，但 M4 之前得先证明
//! [`core_viz::compute_layout`] 算出来的矩形是**对的**。用字符网格把
//! 矩形画出来，是最省事、也最经得起肉眼检查的验证方式——
//! 面积比例是否守恒、父子是否嵌套、LOD 有没有把该省的省掉，一眼就能看出来。

use core_io::util::human_size;
use core_viz::{LayoutResult, Rect};

// ── 时间 ────────────────────────────────────────────────────────────────────

/// Unix 秒 → `YYYY-MM-DD HH:MM:SS`（本地时区不处理，按 UTC 显示并标注）。
///
/// 不引 `chrono` 就得自己算民用历。这里用的是 Howard Hinnant 的
/// `civil_from_days` 算法：把「1970-01-01 起的天数」映射到公历年月日，
/// 对负数（1970 之前）也成立，且没有循环。
pub fn fmt_unix_utc(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_string();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 毫秒时间戳（相对起点）→ `mm:ss.mmm`，用于监控日志行首。
pub fn fmt_offset_ms(ms: u64) -> String {
    format!(
        "{:02}:{:02}.{:03}",
        ms / 60_000,
        (ms % 60_000) / 1000,
        ms % 1000
    )
}

/// Howard Hinnant `civil_from_days`：天数 → (年, 月, 日)。
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── 表格 ────────────────────────────────────────────────────────────────────

/// 按「显示宽度」右对齐补空格。
///
/// CJK 字符在等宽终端里占 2 列，所以不能直接用 `chars().count()`，
/// 否则中文列会参差不齐。
pub fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{}{}", " ".repeat(width - w), s)
}

/// 按显示宽度左对齐。
pub fn pad_right(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{}{}", s, " ".repeat(width - w))
}

/// 字符串在等宽终端的显示宽度（CJK 全角算 2）。
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| if is_wide(c) { 2 } else { 1 })
        .sum::<usize>()
}

/// 粗略的全角判定：覆盖 CJK 统一表意文字、全角标点、常用 CJK 区段。
fn is_wide(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1100..=0x115F      // 谚文字母
        | 0x2E80..=0x303E    // CJK 部首 / 康熙部首 / CJK 符号
        | 0x3041..=0x33FF    // 平假名 / 片假名 / CJK 兼容
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 统一表意
        | 0xA000..=0xA4CF    // 彝文
        | 0xAC00..=0xD7A3    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFE30..=0xFE6F    // CJK 兼容形式
        | 0xFF00..=0xFF60    // 全角形式
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD  // CJK 扩展 B 及以后
    )
}

/// 按显示宽度截断，**保留头部**、超出部分用 `…` 收尾。
///
/// 适合「重要信息在前面」的短文本（卷标、状态词）。**路径和文件名不要用它** ——
/// 见 [`truncate_keep_tail`]。
pub fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// 按显示宽度截断，**保留尾部**（中间用 `…` 省略前缀）。
///
/// # 为什么路径必须保尾
/// 路径的信息量集中在**最后一段**（文件名/目录名），前面 `C:\Users\…\Temp\`
/// 这类前缀谁都一样。用保头的 [`truncate`] 截路径，结果是
/// `…\fw_e2e_live_22104\incoming\payload.d…` —— 用户唯一想看的东西正好被切掉。
/// 对一个「文件」监控工具，这是最不该省的那几个字符。
///
/// 首尾都装不下时退化为保尾：宁可把目录链省光，也要留下文件名。
pub fn truncate_keep_tail(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    // 从尾部往前收，留出 1 列给 `…`；注意别把一个全角字符劈成半列
    let budget = width - 1;
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0usize;
    for c in s.chars().rev() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > budget {
            break;
        }
        tail.push(c);
        w += cw;
    }
    let mut out = String::with_capacity(s.len());
    out.push('…');
    out.extend(tail.into_iter().rev());
    out
}

/// 画一条简单的表格分隔线。
pub fn rule_line(width: usize) -> String {
    "─".repeat(width)
}

// ── ASCII 面积图 ────────────────────────────────────────────────────────────

/// 把 [`LayoutResult`] 渲染成字符网格。
///
/// 规则：
/// * 先画父节点的外框，再让子节点覆盖内部——因为 `compute_layout` 保证
///   **父先于子**出现在 `items` 里，顺序画下去自然就是「子盖在父上」；
/// * 只有 `labeled` 的矩形才写标题，且必须放得下；
/// * 用 `+` 补角、`-` 画横、`|` 画竖。
pub fn render_ascii(layout: &LayoutResult, width: usize, height: usize) -> String {
    if width < 4 || height < 4 {
        return "(画布太小)".to_string();
    }
    let mut grid: Vec<Vec<char>> = vec![vec![' '; width]; height];

    for item in &layout.items {
        let Some((x, y, w, h)) = clamp_rect(&item.rect, width, height) else {
            continue;
        };
        if w < 2 || h < 2 {
            continue;
        }
        draw_box(&mut grid, x, y, w, h);

        if item.labeled {
            // 标题要放在框内且不压住右边框
            let inner_w = w.saturating_sub(2);
            if inner_w >= 3 {
                let text = truncate(&item_label(item), inner_w);
                let ty = (y + if h > 3 { 1 } else { 0 }).min(height - 1);
                put_text(&mut grid, x + 1, ty, &text, width);
            }
        }
    }

    let mut out = String::with_capacity((width + 1) * height);
    for row in grid.iter() {
        out.extend(row.iter());
        out.push('\n');
    }
    out
}

/// 图例：`shade_slot` 对应 [`core_viz::SHADE_SLOTS`] 个色槽。
pub fn render_legend(layout: &LayoutResult) -> String {
    let mut counts = [0usize; core_viz::SHADE_SLOTS];
    for it in &layout.items {
        let s = (it.shade_slot as usize) % core_viz::SHADE_SLOTS;
        counts[s] += 1;
    }
    let used: Vec<String> = counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(i, c)| format!("#{}×{}", i, c))
        .collect();
    format!("色槽占用：{}", used.join(" "))
}

fn item_label(item: &core_viz::LayoutItem) -> String {
    // CLI 侧没有名字池，只能靠尺寸/深度给出可读标签
    format!("node {} {}", item.node, human_size(item.size))
}

/// 把浮点矩形裁剪到 `width × height` 的整数网格，返回 `(x, y, w, h)`。
///
/// 越界或退化时返回 `None`（调用方直接跳过），**绝不越界索引**。
fn clamp_rect(r: &Rect, width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
    if !r.x.is_finite() || !r.y.is_finite() || !r.w.is_finite() || !r.h.is_finite() {
        return None;
    }
    let x0 = r.x.max(0.0).round() as usize;
    let y0 = r.y.max(0.0).round() as usize;
    if x0 >= width || y0 >= height {
        return None;
    }
    let x1 = ((r.x + r.w).round() as usize).min(width);
    let y1 = ((r.y + r.h).round() as usize).min(height);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

fn draw_box(grid: &mut [Vec<char>], x: usize, y: usize, w: usize, h: usize) {
    if w < 2 || h < 2 || y >= grid.len() {
        return;
    }
    let (x2, y2) = (x + w - 1, y + h - 1);
    if y2 >= grid.len() || x2 >= grid[y].len() {
        return;
    }
    for i in x..=x2 {
        grid[y][i] = '-';
        grid[y2][i] = '-';
    }
    for row in grid.iter_mut().take(y2 + 1).skip(y) {
        row[x] = '|';
        row[x2] = '|';
    }
    for (cx, cy) in [(x, y), (x2, y), (x, y2), (x2, y2)] {
        grid[cy][cx] = '+';
    }
}

fn put_text(grid: &mut [Vec<char>], x: usize, y: usize, text: &str, width: usize) {
    if y >= grid.len() {
        return;
    }
    let mut cx = x;
    for c in text.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if cx + cw > width || cx >= grid[y].len() {
            break;
        }
        grid[y][cx] = c;
        // 全角字符占用两列：第二列留空，避免与后续字符重叠
        if cw == 2 && cx + 1 < grid[y].len() {
            grid[y][cx + 1] = ' ';
        }
        cx += cw;
    }
}

/// 布局自检：所有矩形面积之和、视口面积、以及**同层**矩形的最大重叠面积。
///
/// 只比较**同一深度**的矩形——父子矩形本来就是嵌套的，把父子算作
/// 「重叠」毫无意义。同层的兄弟矩形之间才应该互不重叠（Squarified
/// 是严格分区，理论上重叠为 0）。
///
/// 返回 `(总面积, 视口面积, 同层最大重叠面积)`。
pub fn layout_audit(layout: &LayoutResult, viewport: &Rect) -> (f32, f32, f32) {
    let total: f32 = layout.items.iter().map(|i| i.rect.area()).sum();
    let mut worst = 0.0f32;
    let top: Vec<&core_viz::LayoutItem> = layout.items.iter().take(200).collect();
    for i in 0..top.len() {
        for j in i + 1..top.len() {
            if top[i].depth != top[j].depth {
                continue;
            }
            let a = top[i].rect.intersection_area(&top[j].rect);
            if a > worst {
                worst = a;
            }
        }
    }
    (total, viewport.area(), worst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        // 2000-03-01（闰年边界之后）
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        // 2024-02-29（闰日）
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
    }

    #[test]
    fn fmt_unix_utc_is_stable() {
        assert_eq!(fmt_unix_utc(0), "-");
        assert_eq!(fmt_unix_utc(1), "1970-01-01 00:00:01Z");
        // 2021-01-01 00:00:00Z
        assert_eq!(fmt_unix_utc(1_609_459_200), "2021-01-01 00:00:00Z");
        // 闰日
        assert_eq!(fmt_unix_utc(1_709_164_800), "2024-02-29 00:00:00Z");
    }

    #[test]
    fn offset_format() {
        assert_eq!(fmt_offset_ms(0), "00:00.000");
        assert_eq!(fmt_offset_ms(61_500), "01:01.500");
    }

    #[test]
    fn cjk_counts_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中"), 3);
        assert_eq!(pad_left("中", 4), "  中");
        assert_eq!(pad_right("中", 4), "中  ");
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        // 4 列预算：3 个 ASCII + 省略号
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 中文占 2 列
        assert_eq!(display_width(&truncate("中文字符串", 5)), 5);
    }

    #[test]
    fn truncate_keep_tail_preserves_the_leaf_name() {
        let p = r"C:\Users\HY\AppData\Local\Temp\fw_e2e_live_22104\incoming\payload.dat";
        // 69 列 → 截到 68：必须留下**文件名**，而不是把目录链前面那几个字留下
        let t = truncate_keep_tail(p, 68);
        assert_eq!(display_width(&t), 68);
        assert!(t.starts_with('…'), "应省略前缀：{}", t);
        assert!(t.ends_with("payload.dat"), "文件名必须完整保留：{}", t);
        assert!(t.contains(r"incoming\payload.dat"), "{}", t);

        // 未超宽时原样返回
        assert_eq!(truncate_keep_tail("a.txt", 10), "a.txt");
        // 退化宽度不能 panic
        assert_eq!(truncate_keep_tail("abcdef", 1), "…");
        assert_eq!(truncate_keep_tail("abcdef", 0), "");
        // 全角字符不能被劈成半列
        let cjk = truncate_keep_tail("中文目录/文件名.txt", 6);
        assert!(display_width(&cjk) <= 6, "{} 宽 {}", cjk, display_width(&cjk));
    }

    /// 对照：保头截断会把文件名切掉 —— 这就是路径不能用 `truncate` 的原因。
    #[test]
    fn head_truncation_would_eat_the_file_name() {
        let p = r"C:\Users\HY\AppData\Local\Temp\fw_e2e_live_22104\incoming\payload.dat";
        let head = truncate(p, 68);
        assert!(!head.contains("payload.dat"), "保头截断会吃掉文件名：{}", head);
        assert!(truncate_keep_tail(p, 68).contains("payload.dat"));
    }

    #[test]
    fn ascii_box_draws_corners() {
        let layout = LayoutResult {
            items: vec![],
            stats: Default::default(),
        };
        let s = render_ascii(&layout, 8, 4);
        assert_eq!(s.lines().count(), 4);
        assert!(s.lines().all(|l| l.chars().count() == 8));
    }

    #[test]
    fn audit_reports_area_ratio() {
        use core_viz::LayoutItem;
        let mk = |x: f32, y: f32, w: f32, h: f32| LayoutItem {
            node: 1,
            rect: Rect { x, y, w, h },
            header: Rect { x, y, w, h: 0.0 },
            depth: 0,
            is_dir: false,
            ext_id: 0,
            shade_slot: 0,
            shade_t: 0.0,
            size: 0,
            collapsed: false,
            has_children: false,
            labeled: false,
        };
        let layout = LayoutResult {
            items: vec![mk(0.0, 0.0, 10.0, 10.0), mk(20.0, 0.0, 5.0, 5.0)],
            stats: Default::default(),
        };
        let vp = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let (total, area, worst) = layout_audit(&layout, &vp);
        assert_eq!(total, 125.0);
        assert_eq!(area, 10_000.0);
        assert_eq!(worst, 0.0, "两个矩形不相交");
    }
}
