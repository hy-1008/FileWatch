//! # core-viz —— 面积图布局（纯函数，可单测）
//!
//! 分层（文档 §4.2）：**不依赖 UI**，输入是树 + 矩形，输出是矩形列表。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`rect`] | 矩形基元与几何运算 |
//! | [`squarify`] | Squarified Treemap 算法（Bruls et al. 2000） |
//! | [`lod`] | 屏幕空间递归的常量与剪枝策略 |
//! | [`layout`] | 目录树 → 屏幕矩形扁平数组 |
//! | [`camera`] | `(center, zoom)` 相机与**强制钳制** |
//!
//! 渲染策略（文档 §5.6）：
//! * 一期 Canvas 2D —— LOD 后同屏只有几千个矩形，60 FPS 无压力；
//! * 二期 WebGL2 instanced —— 扁平数组可直接打包成 `Float32Array`，
//!   一次 `drawArraysInstanced` 画完，十万级矩形仍在预算内。
//! * 传输走**二进制通道**而非 JSON：200 万节点做 JSON 序列化会直接卡死。

pub mod camera;
pub mod layout;
pub mod lod;
pub mod rect;
pub mod squarify;

pub use camera::{Camera, MAX_ZOOM, MIN_ZOOM};
pub use layout::{
    compute_layout, LayoutItem, LayoutOptions, LayoutResult, ShadingMode, SHADE_SLOTS,
};
pub use lod::{LayoutStats, LodPolicy};
pub use rect::Rect;
pub use squarify::squarify;
