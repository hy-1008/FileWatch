//! 相机系统：`(center, zoom)` 连续模型（文档 §5.6 / 风险 R9）。
//!
//! **不使用导航栈**，就是一个中心点 + 缩放倍率。
//!
//! 风险 R9 的坑：deep 嵌套双击时如果不做钳制，`center` 会跑到内容边界之外，
//! 加上 `zoom` 放大后坐标数值爆炸，整个视图变空白。
//! 所以这里规定——**任何一次相机变更之后都必须 `clamp()`**。

use crate::rect::Rect;

pub const MIN_ZOOM: f32 = 1.0;
pub const MAX_ZOOM: f32 = 5000.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// 视口中心对应的「世界坐标」
    pub center_x: f32,
    pub center_y: f32,
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera::new()
    }
}

impl Camera {
    pub fn new() -> Camera {
        Camera {
            center_x: 0.0,
            center_y: 0.0,
            zoom: MIN_ZOOM,
        }
    }

    /// 以内容矩形的中心为初始视角。
    pub fn fit(content: Rect) -> Camera {
        let (cx, cy) = content.center();
        Camera {
            center_x: cx,
            center_y: cy,
            zoom: MIN_ZOOM,
        }
    }

    /// 世界坐标 → 屏幕坐标。
    #[inline]
    pub fn world_to_screen(&self, wx: f32, wy: f32, vp: Rect) -> (f32, f32) {
        (
            (wx - self.center_x) * self.zoom + vp.w * 0.5,
            (wy - self.center_y) * self.zoom + vp.h * 0.5,
        )
    }

    /// 屏幕坐标 → 世界坐标。
    #[inline]
    pub fn screen_to_world(&self, sx: f32, sy: f32, vp: Rect) -> (f32, f32) {
        (
            (sx - vp.w * 0.5) / self.zoom + self.center_x,
            (sy - vp.h * 0.5) / self.zoom + self.center_y,
        )
    }

    /// 当前可见的世界矩形。
    pub fn visible_rect(&self, vp: Rect) -> Rect {
        let w = vp.w / self.zoom;
        let h = vp.h / self.zoom;
        Rect::new(self.center_x - w * 0.5, self.center_y - h * 0.5, w, h)
    }

    /// **钳制**：把 `zoom` 限制在 `[MIN_ZOOM, MAX_ZOOM]`，
    /// 把 `center` 限制在「可见区域不越出内容边界」的范围内。
    ///
    /// 如果内容比可见区域还小，就把 center 顶到内容中心（不允许平移出界）。
    pub fn clamp(&mut self, content: Rect, vp: Rect) {
        if !self.zoom.is_finite() {
            self.zoom = MIN_ZOOM;
        }
        self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);

        if !self.center_x.is_finite() {
            self.center_x = content.x + content.w * 0.5;
        }
        if !self.center_y.is_finite() {
            self.center_y = content.y + content.h * 0.5;
        }

        let vis_w = vp.w / self.zoom;
        let vis_h = vp.h / self.zoom;

        // X 轴
        if content.w <= vis_w || vis_w <= 0.0 {
            self.center_x = content.x + content.w * 0.5;
        } else {
            let lo = content.x + vis_w * 0.5;
            let hi = content.x + content.w - vis_w * 0.5;
            self.center_x = self.center_x.clamp(lo, hi);
        }
        // Y 轴
        if content.h <= vis_h || vis_h <= 0.0 {
            self.center_y = content.y + content.h * 0.5;
        } else {
            let lo = content.y + vis_h * 0.5;
            let hi = content.y + content.h - vis_h * 0.5;
            self.center_y = self.center_y.clamp(lo, hi);
        }
    }

    /// 以屏幕点 `(sx, sy)` 为锚点缩放：该点下的世界坐标保持不动。
    pub fn zoom_at(&mut self, sx: f32, sy: f32, factor: f32, vp: Rect, content: Rect) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let (wx, wy) = self.screen_to_world(sx, sy, vp);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // 反解 center，使 (wx, wy) 仍映射到 (sx, sy)
        self.center_x = wx - (sx - vp.w * 0.5) / self.zoom;
        self.center_y = wy - (sy - vp.h * 0.5) / self.zoom;
        self.clamp(content, vp);
    }

    /// 按屏幕像素平移（**拖拽手势**）。`dx/dy` 是鼠标位移，向右/下为正。
    ///
    /// 语义是「抓住画布拖动」，不是「把视口往右推」：鼠标向右 ⇒ 内容跟着向右
    /// ⇒ 看到的是内容**更左侧**的部分，所以 `center` 要**减**。
    /// （地图/看图类应用一律是这个方向；反过来会让人觉得光标和内容各走各的。）
    pub fn pan_pixels(&mut self, dx: f32, dy: f32, content: Rect, vp: Rect) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.center_x -= dx / self.zoom;
        self.center_y -= dy / self.zoom;
        self.clamp(content, vp);
    }

    /// 直接定位到某个世界矩形（双击下钻 / 面包屑）。
    pub fn focus_rect(&mut self, target: Rect, vp: Rect, content: Rect) {
        if target.w <= 0.0 || target.h <= 0.0 {
            return;
        }
        let (cx, cy) = target.center();
        self.center_x = cx;
        self.center_y = cy;
        // 让 target 刚好铺满视口，并留 6% 余量
        let zx = vp.w / target.w;
        let zy = vp.h / target.h;
        self.zoom = zx.min(zy) * 0.94;
        self.clamp(content, vp);
    }

    /// 复位到 1× 全览。
    pub fn reset(&mut self, content: Rect, vp: Rect) {
        let (cx, cy) = content.center();
        self.center_x = cx;
        self.center_y = cy;
        self.zoom = MIN_ZOOM;
        self.clamp(content, vp);
    }

    /// 供 UI 显示（如「240%」）。
    pub fn zoom_percent(&self) -> u32 {
        (self.zoom * 100.0).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> Rect {
        Rect::new(0.0, 0.0, 1600.0, 900.0)
    }
    fn vp() -> Rect {
        Rect::new(0.0, 0.0, 1600.0, 900.0)
    }

    #[test]
    fn roundtrip_world_screen() {
        let mut c = Camera::fit(content());
        c.zoom = 3.0;
        c.clamp(content(), vp());
        let (wx, wy) = c.screen_to_world(400.0, 300.0, vp());
        let (sx, sy) = c.world_to_screen(wx, wy, vp());
        assert!((sx - 400.0).abs() < 0.01);
        assert!((sy - 300.0).abs() < 0.01);
    }

    #[test]
    fn zoom_is_clamped_to_range() {
        let mut c = Camera::fit(content());
        c.zoom = 0.0001;
        c.clamp(content(), vp());
        assert_eq!(c.zoom, MIN_ZOOM);

        c.zoom = 1e9;
        c.clamp(content(), vp());
        assert_eq!(c.zoom, MAX_ZOOM);
    }

    #[test]
    fn center_is_clamped_inside_content() {
        let mut c = Camera::fit(content());
        c.zoom = 4.0;
        c.center_x = -99999.0;
        c.center_y = 99999.0;
        c.clamp(content(), vp());
        let vis = c.visible_rect(vp());
        assert!(vis.x >= content().x - 0.01);
        assert!(vis.right() <= content().right() + 0.01);
        assert!(vis.y >= content().y - 0.01);
        assert!(vis.bottom() <= content().bottom() + 0.01);
    }

    #[test]
    fn deep_double_click_does_not_blank_view() {
        // 模拟风险 R9：反复下钻到很深的矩形
        let mut c = Camera::fit(content());
        let mut target = Rect::new(10.0, 10.0, 20.0, 12.0);
        for _ in 0..40 {
            c.focus_rect(target, vp(), content());
            // 每次都必须在边界内
            let vis = c.visible_rect(vp());
            assert!(vis.w.is_finite() && vis.w > 0.0);
            assert!(vis.h.is_finite() && vis.h > 0.0);
            assert!(vis.x.is_finite() && vis.y.is_finite());
            assert!(vis.x >= -0.5 && vis.right() <= content().right() + 0.5);
            // 下一层再小一半
            target = Rect::new(target.x, target.y, target.w * 0.5, target.h * 0.5);
        }
        assert!(c.zoom <= MAX_ZOOM);
    }

    #[test]
    fn zoom_at_keeps_anchor_point_stable() {
        let mut c = Camera::fit(content());
        let (ax, ay) = (500.0f32, 400.0f32);
        let before = c.screen_to_world(ax, ay, vp());
        c.zoom_at(ax, ay, 2.0, vp(), content());
        let after = c.screen_to_world(ax, ay, vp());
        assert!((before.0 - after.0).abs() < 0.5, "{:?} vs {:?}", before, after);
        assert!((before.1 - after.1).abs() < 0.5);
    }

    #[test]
    fn pan_moves_view_and_stays_clamped() {
        let mut c = Camera::fit(content());
        c.zoom = 8.0;
        c.clamp(content(), vp());
        // 内容与视口同尺寸（1600×900）且 8× 时，可见世界宽 = 1600/8 = 200，
        // 于是 center_x 被钳在 [100, 1500]，fit 给的 800 仍在范围内。
        assert_eq!(c.center_x, 800.0);
        let x0 = c.visible_rect(vp()).x;
        assert_eq!(x0, 700.0);

        // 「抓住画布向右拖 200px」⇒ 内容右移 200px ⇒ 看到**更左侧**的内容，
        // center 减少 200/zoom = 25 个世界像素
        c.pan_pixels(200.0, 0.0, content(), vp());
        let x1 = c.visible_rect(vp()).x;
        assert_eq!(c.center_x, 775.0);
        assert_eq!(x1, 675.0);
        assert!(
            (x0 - x1 - 200.0 / 8.0).abs() < 0.01,
            "位移量应等于 屏幕位移 / zoom：{} → {}",
            x0,
            x1
        );

        // 反向拖同样距离应当原路返回（两段位移完全抵消）
        c.pan_pixels(-200.0, 0.0, content(), vp());
        assert!((c.visible_rect(vp()).x - x0).abs() < 0.01, "反向拖拽应回到原位");

        // 狂拖不应越界
        for _ in 0..100 {
            c.pan_pixels(-500.0, -500.0, content(), vp());
        }
        let vis = c.visible_rect(vp());
        assert!(vis.x >= -0.5 && vis.right() <= content().right() + 0.5);
        assert!(vis.y >= -0.5 && vis.bottom() <= content().bottom() + 0.5);

        // 反方向狂拖也要被钳住
        for _ in 0..100 {
            c.pan_pixels(500.0, 500.0, content(), vp());
        }
        let vis = c.visible_rect(vp());
        assert!(vis.x >= -0.5 && vis.right() <= content().right() + 0.5);
        assert!(vis.y >= -0.5 && vis.bottom() <= content().bottom() + 0.5);
    }

    #[test]
    fn content_smaller_than_viewport_centers() {
        let mut c = Camera::new();
        let small = Rect::new(100.0, 100.0, 200.0, 100.0);
        c.center_x = 0.0;
        c.center_y = 0.0;
        c.clamp(small, vp());
        assert_eq!(c.center_x, 200.0);
        assert_eq!(c.center_y, 150.0);
    }

    #[test]
    fn reset_returns_to_overview() {
        let mut c = Camera::fit(content());
        c.focus_rect(Rect::new(0.0, 0.0, 10.0, 10.0), vp(), content());
        assert!(c.zoom > MIN_ZOOM);
        c.reset(content(), vp());
        assert_eq!(c.zoom, MIN_ZOOM);
        assert_eq!(c.center_x, 800.0);
    }

    #[test]
    fn non_finite_values_are_sanitized() {
        let mut c = Camera::fit(content());
        c.zoom = f32::NAN;
        c.center_x = f32::INFINITY;
        c.clamp(content(), vp());
        assert!(c.zoom.is_finite() && c.zoom >= MIN_ZOOM);
        assert!(c.center_x.is_finite() && c.center_y.is_finite());
    }

    #[test]
    fn zoom_percent_display() {
        let mut c = Camera::new();
        c.zoom = 2.4;
        assert_eq!(c.zoom_percent(), 240);
    }

    #[test]
    fn invalid_zoom_factor_is_ignored() {
        let mut c = Camera::fit(content());
        let z = c.zoom;
        c.zoom_at(100.0, 100.0, 0.0, vp(), content());
        assert_eq!(c.zoom, z);
        c.zoom_at(100.0, 100.0, f32::NAN, vp(), content());
        assert_eq!(c.zoom, z);
    }
}
