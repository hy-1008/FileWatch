//! LOD（Level of Detail）策略：决定「展开到哪一层」以及「画面预算」。
//!
//! 文档 §2.4 的三个关键技巧：
//! 1. **屏幕空间递归**——每层都用**屏幕像素**重跑布局，标题栏恒定 16 px、
//!    间距恒定 1~2 px，所以缩放时不会变形；
//! 2. **惰性 LOD**——只展开屏幕尺寸够大的节点，离屏节点剪枝；
//! 3. **全局节点预算上限**——保证任意缩放级别下同屏矩形数都有上界。

use crate::rect::Rect;

/// 布局常量与预算。默认值直接采用文档 §5.6 给出的数字。
#[derive(Debug, Clone, Copy)]
pub struct LodPolicy {
    /// 目录标题栏高度（恒定像素，文档：16 px）
    pub header_h: f32,
    /// 子块之间的间距
    pub pad: f32,
    /// 边框宽度
    pub border: f32,
    /// 继续下钻所需的最小边长（文档：面积 > 80 px ⇒ 这里用边长阈值）
    pub min_side: f32,
    /// 最大递归深度（文档：16）
    pub max_depth: u32,
    /// 单次布局产出的最大矩形数（性能兜底）
    pub max_nodes: usize,
    /// 显示文字标签所需的最小宽/高
    pub label_min_w: f32,
    pub label_min_h: f32,
}

impl Default for LodPolicy {
    fn default() -> Self {
        LodPolicy {
            header_h: 16.0,
            pad: 2.0,
            border: 1.0,
            min_side: 80.0,
            max_depth: 16,
            max_nodes: 20_000,
            label_min_w: 44.0,
            label_min_h: 15.0,
        }
    }
}

impl LodPolicy {
    /// 「大扫描」预设：允许更多矩形，用于缩放到最外层时看全盘。
    pub fn wide() -> Self {
        LodPolicy {
            max_nodes: 60_000,
            min_side: 24.0,
            ..Default::default()
        }
    }

    /// 去掉 header / padding / border 后的内容区。
    ///
    /// 全部是**固定像素**，与缩放无关——这正是「屏幕空间递归」的要害。
    ///
    /// 顶部的内缩量是 `header_h + inset`，不是 `header_h`：标题栏占掉最上面
    /// `header_h`，**再**往里让出 `pad + border` 才是内容。少加这个 `inset`
    /// 会让内容区比它该有的位置高 `inset` 像素、正好压在标题栏下沿的边框上，
    /// 而且和 [`LodPolicy::content_is_usable`] 用的阈值（`header_h + 2*inset`）
    /// 对不上 —— 那边按「上下各让 inset」算，这边少让一次。
    #[inline]
    pub fn content_of(&self, r: &Rect) -> Rect {
        let inset = self.pad + self.border;
        r.shrink(self.header_h + inset, inset, inset, inset)
    }

    /// 是否值得继续展开这个节点。
    #[inline]
    pub fn can_descend(&self, r: &Rect, depth: u32, emitted: usize) -> bool {
        depth < self.max_depth && emitted < self.max_nodes && r.short_side() >= self.min_side
    }

    /// 该矩形是否够大，能显示文字标签。
    #[inline]
    pub fn should_label(&self, r: &Rect) -> bool {
        r.w >= self.label_min_w && r.h >= self.label_min_h
    }

    /// 内容区是否还有意义（小于 1 px 就没必要继续）。
    #[inline]
    pub fn content_is_usable(&self, r: &Rect) -> bool {
        r.short_side() > self.header_h + 2.0 * (self.pad + self.border) + 2.0
    }
}

/// 一次布局的统计信息（用于调优与「为什么这里没展开」的解释）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutStats {
    /// 实际产出的矩形数
    pub emitted: usize,
    /// 因为太小而被折叠的节点数
    pub collapsed_small: usize,
    /// 因为达到深度上限而被折叠的节点数
    pub collapsed_depth: usize,
    /// 是否触发了全局预算上限
    pub hit_budget: bool,
    /// 达到的最大深度
    pub max_depth_reached: u32,
}

impl LayoutStats {
    pub fn summary_cn(&self) -> String {
        format!(
            "绘制 {} 个矩形；太小折叠 {}；深度折叠 {}；最大深度 {}{}",
            self.emitted,
            self.collapsed_small,
            self.collapsed_depth,
            self.max_depth_reached,
            if self.hit_budget { "；已触发节点预算上限" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let p = LodPolicy::default();
        assert_eq!(p.header_h, 16.0);
        assert_eq!(p.pad, 2.0);
        assert_eq!(p.border, 1.0);
        assert_eq!(p.min_side, 80.0);
        assert_eq!(p.max_depth, 16);
    }

    #[test]
    fn content_shrinks_by_fixed_pixels() {
        let p = LodPolicy::default();
        let c = p.content_of(&Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(c.x, 3.0); // pad 2 + border 1
        assert_eq!(c.y, 16.0 + 3.0);
        assert_eq!(c.w, 100.0 - 6.0);
        assert_eq!(c.h, 100.0 - 16.0 - 6.0);
    }

    #[test]
    fn content_inset_is_scale_independent() {
        // 关键性质：不管是 100px 还是 10000px 的矩形，内缩量完全相同
        let p = LodPolicy::default();
        let small = p.content_of(&Rect::new(0.0, 0.0, 200.0, 200.0));
        let big = p.content_of(&Rect::new(0.0, 0.0, 2000.0, 2000.0));
        assert_eq!(small.x, big.x);
        assert_eq!(small.y, big.y);
    }

    #[test]
    fn descend_rules() {
        let p = LodPolicy::default();
        let big = Rect::new(0.0, 0.0, 500.0, 500.0);
        let small = Rect::new(0.0, 0.0, 40.0, 500.0);
        assert!(p.can_descend(&big, 0, 0));
        assert!(!p.can_descend(&small, 0, 0), "短边不足 80px 不应展开");
        assert!(!p.can_descend(&big, 16, 0), "超过最大深度不应展开");
        assert!(!p.can_descend(&big, 0, 20_000), "超出节点预算不应展开");
    }

    #[test]
    fn label_threshold() {
        let p = LodPolicy::default();
        assert!(p.should_label(&Rect::new(0.0, 0.0, 100.0, 30.0)));
        assert!(!p.should_label(&Rect::new(0.0, 0.0, 20.0, 30.0)));
        assert!(!p.should_label(&Rect::new(0.0, 0.0, 100.0, 5.0)));
    }

    #[test]
    fn usable_content_check() {
        let p = LodPolicy::default();
        // 正好等于 header + 边距 → 不可用
        assert!(!p.content_is_usable(&Rect::new(0.0, 0.0, 100.0, 22.0)));
        assert!(p.content_is_usable(&Rect::new(0.0, 0.0, 100.0, 300.0)));
    }

    #[test]
    fn wide_preset_allows_more() {
        let w = LodPolicy::wide();
        assert!(w.max_nodes > LodPolicy::default().max_nodes);
        assert!(w.min_side < LodPolicy::default().min_side);
    }
}
