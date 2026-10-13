//! 矩形基元。
//!
//! 屏幕空间布局全程使用 `f32` 像素坐标，`y` 轴向下（与 Canvas 一致）。

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[inline]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[inline]
    pub const fn zero() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }
    }

    #[inline]
    pub fn area(&self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    #[inline]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    #[inline]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    #[inline]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    #[inline]
    pub fn short_side(&self) -> f32 {
        self.w.min(self.h)
    }

    #[inline]
    pub fn is_degenerate(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// 内缩（上右下左），结果不会出现负宽高。
    #[inline]
    pub fn shrink(&self, top: f32, right: f32, bottom: f32, left: f32) -> Rect {
        let w = (self.w - left - right).max(0.0);
        let h = (self.h - top - bottom).max(0.0);
        Rect {
            x: self.x + left,
            y: self.y + top,
            w,
            h,
        }
    }

    #[inline]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// 长宽比（≥ 1；越接近 1 越「方正」）。
    #[inline]
    pub fn aspect_ratio(&self) -> f32 {
        if self.w <= 0.0 || self.h <= 0.0 {
            return f32::INFINITY;
        }
        if self.w > self.h {
            self.w / self.h
        } else {
            self.h / self.w
        }
    }

    /// 与另一个矩形的交叠面积（用于命中测试/裁剪判断）。
    pub fn intersection_area(&self, other: &Rect) -> f32 {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            0.0
        } else {
            (x1 - x0) * (y1 - y0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 70.0);
        assert_eq!(r.area(), 5000.0);
        assert_eq!(r.center(), (60.0, 45.0));
        assert_eq!(r.short_side(), 50.0);
    }

    #[test]
    fn shrink_clamps_at_zero() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        let s = r.shrink(20.0, 20.0, 20.0, 20.0);
        assert_eq!(s.w, 0.0);
        assert_eq!(s.h, 0.0);
    }

    #[test]
    fn contains_uses_half_open_interval() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(9.99, 9.99));
        assert!(!r.contains(10.0, 5.0));
        assert!(!r.contains(-0.01, 5.0));
    }

    #[test]
    fn aspect_ratio_is_symmetric() {
        assert_eq!(Rect::new(0.0, 0.0, 100.0, 50.0).aspect_ratio(), 2.0);
        assert_eq!(Rect::new(0.0, 0.0, 50.0, 100.0).aspect_ratio(), 2.0);
        assert_eq!(Rect::new(0.0, 0.0, 10.0, 10.0).aspect_ratio(), 1.0);
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).aspect_ratio().is_infinite());
    }

    #[test]
    fn intersection() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersection_area(&b), 25.0);
        let c = Rect::new(20.0, 20.0, 5.0, 5.0);
        assert_eq!(a.intersection_area(&c), 0.0);
    }
}
