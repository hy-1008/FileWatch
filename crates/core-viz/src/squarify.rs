//! **Squarified Treemap** 布局（Bruls / Huizing / van Wijk, 2000）。
//!
//! 把一个矩形递归细分成若干子矩形，使每个子矩形的**长宽比尽可能接近 1:1**。
//! 相比朴素的 slice-and-dice（会产生大量细长条），方块的**可读性与可点击性**
//! 都高得多（文档 §2.4）。
//!
//! 算法：每一步把待放元素按面积降序，尝试追加进「当前行」；
//! 计算该行的最差长宽比 `worst()`，若加入下一个元素会让它变差，就封行、换新行。
//!
//! 本模块是**纯函数**：`(面积数组, 矩形) → 矩形数组`，输出顺序与输入**严格一一对应**。

use crate::rect::Rect;

/// 计算一行的「最差长宽比」，越小越好。
///
/// * `row` —— 该行各元素的面积
/// * `side` —— 该行所依附的边长
fn worst(row: &[f64], side: f64) -> f64 {
    if row.is_empty() || side <= 0.0 {
        return f64::INFINITY;
    }
    let sum: f64 = row.iter().sum();
    if sum <= 0.0 {
        return f64::INFINITY;
    }
    let rmax = row.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let rmin = row.iter().cloned().fold(f64::INFINITY, f64::min);
    if rmin <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * rmax / s2).max(s2 / (w2 * rmin))
}

/// 把一行元素铺进 `free`，并把产生的矩形追加到 `out`。
///
/// 规则：沿 `free` 的**短边**方向铺（这样行本身是细长的，但元素接近方形）。
fn layout_row(row: &[f64], free: &mut Rect, out: &mut Vec<Rect>) {
    let sum: f64 = row.iter().sum();
    if sum <= 0.0 || free.is_degenerate() {
        return;
    }

    if free.w <= free.h {
        // 沿宽度铺：整行占满 free.w，行高 = sum / free.w
        let h = (sum / free.w as f64) as f32;
        let h = h.min(free.h);
        let mut x = free.x;
        for (i, &v) in row.iter().enumerate() {
            let w = if i + 1 == row.len() {
                // 最后一个吃满剩余宽度，避免累积浮点误差留下缝隙
                (free.right() - x).max(0.0)
            } else {
                ((v / sum) as f32 * free.w).min(free.right() - x).max(0.0)
            };
            out.push(Rect::new(x, free.y, w, h));
            x += w;
        }
        free.y += h;
        free.h = (free.h - h).max(0.0);
    } else {
        // 沿高度铺：整行占满 free.h，行宽 = sum / free.h
        let w = (sum / free.h as f64) as f32;
        let w = w.min(free.w);
        let mut y = free.y;
        for (i, &v) in row.iter().enumerate() {
            let h = if i + 1 == row.len() {
                (free.bottom() - y).max(0.0)
            } else {
                ((v / sum) as f32 * free.h).min(free.bottom() - y).max(0.0)
            };
            out.push(Rect::new(free.x, y, w, h));
            y += h;
        }
        free.x += w;
        free.w = (free.w - w).max(0.0);
    }
}

/// Squarified 布局。
///
/// * `areas` —— 各元素的权重（通常就是字节数）。**建议降序**，
///   降序时结果最优；非降序也能正确布局，但方块会不那么方正。
/// * `rect` —— 容器矩形（通常已经是扣掉内边距后的内容区）
///
/// 返回的矩形数组与 `areas` **索引一一对应**（含 0 面积元素，得到退化矩形）。
pub fn squarify(areas: &[f64], rect: Rect) -> Vec<Rect> {
    // 先按输入长度铺一张全零的底：零面积元素天然就是零矩形，
    // 后面只把正面积元素的结果**按索引回填**进去。
    let mut out: Vec<Rect> = vec![Rect::zero(); areas.len()];
    if areas.is_empty() || rect.is_degenerate() {
        return out;
    }

    let total: f64 = areas.iter().filter(|a| **a > 0.0).sum();
    if total <= 0.0 {
        // 全部为 0：统一给 0 面积（与 `out` 的初值一致，直接返回）
        return out;
    }

    let scale = (rect.w as f64 * rect.h as f64) / total;
    let vals: Vec<f64> = areas.iter().map(|a| if *a > 0.0 { a * scale } else { 0.0 }).collect();

    // 铺排结果先攒进 `produced`，最后才回填。
    //
    // **为什么不能边铺边 push**：零面积元素会在「当前行还没封行」的时候就被
    // 写进输出，抢在它前面那些元素的矩形之前 —— 于是 `out[i]` 拿到的根本不是
    // `areas[i]` 的矩形。症状很隐蔽：一个 0 字节文件/空目录会抢走别人的方块，
    // 而真正的大目录拿到一个退化矩形、在后继的 `is_degenerate()` 判断里被丢掉，
    // 整个图看起来「少了一块、错了一块」，却不报任何错。
    // 契约写的是「与输入索引一一对应」，所以必须回填。
    let mut produced: Vec<Rect> = Vec::with_capacity(areas.len());

    let mut free = rect;
    let mut idx = 0usize;
    let mut row: Vec<f64> = Vec::new();

    while idx < vals.len() {
        let v = vals[idx];

        if v <= 0.0 {
            // 零面积元素：不参与铺排，回填阶段自然保持 `Rect::zero()`
            idx += 1;
            continue;
        }

        let side = free.short_side() as f64;
        if row.is_empty() {
            row.push(v);
            idx += 1;
            continue;
        }

        let cur = worst(&row, side);
        let mut trial = row.clone();
        trial.push(v);
        let next = worst(&trial, side);

        if next <= cur {
            row.push(v);
            idx += 1;
        } else {
            // 封行：铺出去，然后从 idx 重新开始
            let mut row_out: Vec<Rect> = Vec::with_capacity(row.len());
            layout_row(&row, &mut free, &mut row_out);
            debug_assert_eq!(row_out.len(), row.len());
            produced.extend(row_out);
            row.clear();
            // 若铺完没有剩余空间，余下元素保持零矩形（`out` 的初值）
            if free.is_degenerate() {
                break;
            }
        }
    }

    if !row.is_empty() {
        let mut row_out: Vec<Rect> = Vec::with_capacity(row.len());
        layout_row(&row, &mut free, &mut row_out);
        produced.extend(row_out);
    }

    // 按索引回填：`produced` 里只有正面积元素，且顺序与它们在 `areas` 里的
    // 出现顺序一致（`idx` 单调递增，行永远是连续区间）。
    let mut it = produced.into_iter();
    for (i, v) in vals.iter().enumerate() {
        if *v > 0.0 {
            match it.next() {
                Some(r) => out[i] = r,
                // 空间用尽 → 后面的元素保持零矩形
                None => break,
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container() -> Rect {
        Rect::new(0.0, 0.0, 1000.0, 600.0)
    }

    #[test]
    fn empty_input() {
        assert!(squarify(&[], container()).is_empty());
    }

    #[test]
    fn zero_area_container() {
        // 容器退化时**仍然一一对应**：每个输入得到一枚退化矩形。
        // 模块文档写的是「输出顺序与输入严格一一对应」，这条对任何路径都成立，
        // 不能因为「提前返回」就变成空数组 —— 两种提前返回给两种长度，
        // 正是上一版把索引对应关系搞乱的那种不一致。
        let rects = squarify(&[1.0, 2.0], Rect::new(0.0, 0.0, 0.0, 100.0));
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|r| r.area() == 0.0));
    }

    #[test]
    fn output_length_matches_input() {
        let areas = vec![100.0, 50.0, 25.0, 12.5, 6.25];
        let rects = squarify(&areas, container());
        assert_eq!(rects.len(), areas.len());
    }

    #[test]
    fn all_rects_within_container() {
        let areas = vec![500.0, 300.0, 200.0, 100.0, 50.0, 25.0, 10.0, 5.0];
        let c = container();
        for r in squarify(&areas, c) {
            assert!(r.x >= c.x - 0.5, "x 越界: {:?}", r);
            assert!(r.y >= c.y - 0.5, "y 越界: {:?}", r);
            assert!(r.right() <= c.right() + 0.5, "右越界: {:?}", r);
            assert!(r.bottom() <= c.bottom() + 0.5, "下越界: {:?}", r);
        }
    }

    #[test]
    fn total_area_is_preserved() {
        let areas = vec![500.0, 300.0, 200.0, 100.0];
        let c = container();
        let rects = squarify(&areas, c);
        let sum: f32 = rects.iter().map(|r| r.area()).sum();
        let expected = c.area();
        assert!(
            (sum - expected).abs() / expected < 0.01,
            "面积和 {} 与容器 {} 偏差过大",
            sum,
            expected
        );
    }

    #[test]
    fn areas_are_proportional_to_weights() {
        let areas = vec![300.0, 100.0];
        let c = Rect::new(0.0, 0.0, 400.0, 400.0);
        let rects = squarify(&areas, c);
        let r0 = rects[0].area();
        let r1 = rects[1].area();
        assert!((r0 / r1 - 3.0).abs() < 0.3, "比例应约为 3，实际 {}", r0 / r1);
    }

    #[test]
    fn squarified_beats_slice_and_dice_on_aspect() {
        // 用一个极端例子验证「方块化」效果
        let areas = vec![100.0, 100.0, 100.0, 100.0, 100.0];
        let c = Rect::new(0.0, 0.0, 500.0, 400.0);
        let rects = squarify(&areas, c);
        let worst_aspect = rects
            .iter()
            .map(|r| r.aspect_ratio())
            .fold(0.0f32, f32::max);
        // 朴素切片会得到 5 条 100x400（比例 4）；squarified 应明显更好
        assert!(
            worst_aspect < 3.0,
            "最差长宽比 {} 应优于朴素切分的 4.0",
            worst_aspect
        );
    }

    #[test]
    fn single_element_fills_container() {
        let c = container();
        let rects = squarify(&[42.0], c);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].area() - c.area()).abs() < 1.0);
    }

    #[test]
    fn handles_zero_weight_entries() {
        let areas = vec![100.0, 0.0, 50.0];
        let rects = squarify(&areas, container());
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[1].area(), 0.0);
    }

    /// 回归：零面积元素**不能**打乱「索引一一对应」。
    ///
    /// 修复前的实现是「边铺边 push」：零面积元素会在当前行**封行之前**就被写进
    /// 输出，抢走了本该属于前面那个元素的槽位。修复前这里的 `rects[1]` 会是
    /// 权重 100 那份的矩形（面积 400000），而 `rects[2]` 才是 0 —— 也就是说
    /// 索引 2 的元素拿到了索引 0 的方块，而它自己的方块凭空消失。
    ///
    /// 在真实数据上这意味着：一个 0 字节文件 / 空目录会抢走一个大目录的方块，
    /// 大目录拿到退化矩形后被 `visit` 的 `is_degenerate()` 丢掉 —— 画面上
    /// 「少一块 + 错一块」，却什么错都不报。
    #[test]
    fn zero_weight_entry_does_not_shift_index_mapping() {
        let areas = vec![100.0, 0.0, 50.0];
        let c = container();
        let rects = squarify(&areas, c);

        let sum: f32 = rects.iter().map(|r| r.area()).sum();
        assert!((sum - c.area()).abs() < 1.0, "总面积 {} 应铺满容器 {}", sum, c.area());
        assert_eq!(rects[1].area(), 0.0, "中间那个零面积元素必须拿到退化矩形");

        // 100 : 50 = 2 : 1 —— 用比例确认「谁拿到了谁的方块」
        let ratio = rects[0].area() / rects[2].area();
        assert!((ratio - 2.0).abs() < 0.05, "第 0 与第 2 个元素面积比应为 2，实际 {}", ratio);
        // 位置顺序也要对：第 0 个在左、第 2 个在右
        assert!(rects[0].x < rects[2].x, "{:?} vs {:?}", rects[0], rects[2]);
    }

    /// 零面积元素在**开头**或**结尾**（开头那条是修复前必错的路径）
    #[test]
    fn zero_weight_at_both_ends() {
        let c = container();

        let head = squarify(&[0.0, 100.0, 50.0], c);
        assert_eq!(head.len(), 3);
        assert_eq!(head[0].area(), 0.0, "开头的零面积元素必须拿到退化矩形");
        let r = head[1].area() / head[2].area();
        assert!((r - 2.0).abs() < 0.05, "剩下的两个仍应大者更大，实际 {}", r);

        let tail = squarify(&[100.0, 50.0, 0.0], c);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[2].area(), 0.0, "结尾的零面积元素必须拿到退化矩形");
        let r = tail[0].area() / tail[1].area();
        assert!((r - 2.0).abs() < 0.05, "实际 {}", r);
    }

    /// 全是零权重 → 全部退化矩形（不 panic、不越界）
    #[test]
    fn all_zero_weights_yield_degenerate_rects() {
        let rects = squarify(&[0.0, 0.0, 0.0], container());
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(|r| r.area() == 0.0));
    }

    /// 零权重 + 大量正权重混在一起（真实场景：空目录夹在真实目录之间）
    #[test]
    fn zero_weights_do_not_break_realistic_layout() {
        let mut areas = vec![5000.0, 0.0, 3000.0, 0.0, 1500.0, 0.0, 800.0];
        areas.extend(std::iter::repeat(20.0).take(30));
        areas.push(0.0);
        let c = Rect::new(0.0, 0.0, 1600.0, 900.0);
        let rects = squarify(&areas, c);
        assert_eq!(rects.len(), areas.len());

        for (a, r) in areas.iter().zip(rects.iter()) {
            if *a == 0.0 {
                assert_eq!(r.area(), 0.0, "零权重必须得到退化矩形");
            } else {
                assert!(r.area() > 0.0, "正权重必须得到真实矩形：{} → {:?}", a, r);
            }
        }
        let sum: f32 = rects.iter().map(|r| r.area()).sum();
        assert!((sum - c.area()).abs() / c.area() < 0.02, "总面积 {} 偏离容器 {}", sum, c.area());
    }

    #[test]
    fn strips_are_avoided_for_realistic_mix() {
        // 模拟真实磁盘：少数大目录 + 大量小文件
        let mut areas = vec![5000.0, 3000.0, 1500.0, 800.0];
        areas.extend(std::iter::repeat(20.0).take(40));
        let c = Rect::new(0.0, 0.0, 1600.0, 900.0);
        let rects = squarify(&areas, c);
        assert_eq!(rects.len(), areas.len());
        let sum: f32 = rects.iter().map(|r| r.area()).sum();
        assert!((sum - c.area()).abs() / c.area() < 0.02);
    }

    #[test]
    fn does_not_panic_on_extreme_ratios() {
        let areas: Vec<f64> = (0..200).map(|i| (200 - i) as f64).collect();
        let rects = squarify(&areas, Rect::new(0.0, 0.0, 1920.0, 8.0));
        assert_eq!(rects.len(), 200);
    }
}
