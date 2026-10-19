//! 屏幕空间递归布局：把目录树投影成一组屏幕矩形。
//!
//! 流程（文档 §5.6）：
//!
//! ```text
//! compute_layout(node, screen_rect, depth):
//!     if depth > MAX_DEPTH or screen_rect too small: return
//!     if node is file: emit rect; return
//!     content = screen_rect.shrink(header=16px, pad=2px, border=1px)
//!     children = node.children sorted by size desc
//!     squarify(children, content)          # 每个子节点在屏幕像素下的矩形
//!     for child: 太小则 LOD 剪枝（折叠），否则递归
//!     draw_header_on_top(node)             # header 最后画，不被子块遮挡
//! ```
//!
//! 输出是**扁平数组**（而非嵌套），便于一次性传给前端 / WebGL instancing。
//! 父节点一定排在子节点**之前**，因此渲染器只需正序画背景、倒序做命中测试。

use crate::lod::{LayoutStats, LodPolicy};
use crate::rect::Rect;
use crate::squarify::squarify;
use core_index::aggregate::{sorted_children, SizeMode};
use core_index::{ChildIndex, ExtensionTable, Node, Tree};

/// 着色模式（文档 §2.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadingMode {
    /// 按扩展名分类着色（配合 F5 类型列表高亮）
    ByExtension,
    /// 按目录深度渐变
    ByDepth,
    /// 按文件年龄的热力图（越旧越冷）
    ByAge,
}

/// 色板槽位数（前端按此渲染分类色板 / 热力图）。
pub const SHADE_SLOTS: usize = 16;

/// 年龄热力图的天数上限（3 年）。
pub const AGE_NORM_DAYS: f32 = 365.0 * 3.0;

#[derive(Debug, Clone, Copy)]
pub struct LayoutOptions {
    pub policy: LodPolicy,
    pub mode: SizeMode,
    pub shading: ShadingMode,
    /// 当前时间（Unix 秒），`ShadingMode::ByAge` 用
    pub now_unix: i64,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        LayoutOptions {
            policy: LodPolicy::default(),
            mode: SizeMode::Logical,
            shading: ShadingMode::ByExtension,
            now_unix: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutItem {
    /// 节点下标（= MFT 记录号），与树形表/类型列表共享同一 id 空间
    pub node: u32,
    /// 块体矩形
    pub rect: Rect,
    /// 目录标题栏矩形（文件为 0 矩形）；渲染器应在最后一遍绘制它
    pub header: Rect,
    pub depth: u16,
    pub is_dir: bool,
    /// 扩展名 id（`u32::MAX` = 无/其他）
    pub ext_id: u32,
    /// 色板槽位（分类着色）
    pub shade_slot: u8,
    /// 热力图强度 0~1（年龄/深度着色用）
    pub shade_t: f32,
    pub size: u64,
    /// 有子节点但因 LOD 未展开
    pub collapsed: bool,
    pub has_children: bool,
    /// 该块是否够大以显示文字
    pub labeled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LayoutResult {
    pub items: Vec<LayoutItem>,
    pub stats: LayoutStats,
}

impl LayoutResult {
    /// 命中测试：返回**最深**（最后绘制）的命中节点。
    ///
    /// 因为 `items` 是按深度递增排列的，所以倒序扫描第一个命中的就是最深的那个。
    pub fn hit_test(&self, x: f32, y: f32) -> Option<u32> {
        self.items
            .iter()
            .rev()
            .find(|it| it.rect.contains(x, y))
            .map(|it| it.node)
    }

    /// 查找某个节点对应的布局项。
    pub fn find(&self, node: u32) -> Option<&LayoutItem> {
        self.items.iter().find(|it| it.node == node)
    }

    /// 某个节点是否在本次布局中可见。
    pub fn is_visible(&self, node: u32) -> bool {
        self.find(node).is_some()
    }

    /// 给定画布区域，返回与之相交的项（视口裁剪，供 Canvas/WebGL 用）。
    pub fn visible_in(&self, viewport: &Rect) -> Vec<&LayoutItem> {
        self.items
            .iter()
            .filter(|it| it.rect.intersection_area(viewport) > 0.0)
            .collect()
    }
}

/// 计算一次屏幕空间布局。
///
/// * `root` —— 视图对应的根节点（下钻时就是被点中的那个目录）
/// * `viewport` —— 画布像素矩形
pub fn compute_layout(
    tree: &Tree,
    children: &ChildIndex,
    exts: &ExtensionTable,
    root: u32,
    viewport: Rect,
    opts: &LayoutOptions,
) -> LayoutResult {
    let mut result = LayoutResult {
        items: Vec::with_capacity(1024),
        stats: LayoutStats::default(),
    };

    // 根节点本身也要给出一个矩形，供面包屑/回退使用
    visit(
        tree,
        children,
        exts,
        opts,
        &mut result,
        root,
        viewport,
        0,
    );

    result.stats.emitted = result.items.len();
    result
}

/// 迭代友好：父节点先入 `items`，再递归子节点。
#[allow(clippy::too_many_arguments)]
fn visit(
    tree: &Tree,
    children: &ChildIndex,
    exts: &ExtensionTable,
    opts: &LayoutOptions,
    out: &mut LayoutResult,
    node_idx: u32,
    rect: Rect,
    depth: u32,
) {
    if out.items.len() >= opts.policy.max_nodes {
        out.stats.hit_budget = true;
        return;
    }
    if rect.is_degenerate() {
        return;
    }

    // 取节点快照（Node 是 Copy，避免与 out 的可变借用冲突）
    let node: Node = match tree.node(node_idx) {
        Some(n) if n.in_use() => *n,
        _ => return,
    };

    let is_dir = node.is_dir();
    let size = opts.mode.of(&node);
    let ext_id = if is_dir {
        u32::MAX
    } else {
        exts.id_of_node(tree, node_idx)
    };
    let (shade_slot, shade_t) = shade_of(opts, &node, ext_id, depth);

    let header = if is_dir && depth > 0 && rect.h > opts.policy.header_h {
        Rect::new(rect.x, rect.y, rect.w, opts.policy.header_h)
    } else {
        Rect::zero()
    };

    // 判断能否展开（纯读取，不与 out 冲突）
    let kids = children.children(node_idx);
    let has_children = !kids.is_empty();
    let can_descend = is_dir
        && has_children
        && opts.policy.can_descend(&rect, depth, out.items.len())
        && opts.policy.content_is_usable(&rect);

    if is_dir && has_children && !can_descend {
        if depth >= opts.policy.max_depth {
            out.stats.collapsed_depth += 1;
        } else {
            out.stats.collapsed_small += 1;
        }
    }

    out.stats.max_depth_reached = out.stats.max_depth_reached.max(depth);

    out.items.push(LayoutItem {
        node: node_idx,
        rect,
        header,
        depth: depth.min(u16::MAX as u32) as u16,
        is_dir,
        ext_id,
        shade_slot,
        shade_t,
        size,
        collapsed: is_dir && has_children && !can_descend,
        has_children,
        labeled: opts.policy.should_label(&rect),
    });

    if !can_descend {
        return;
    }

    // 目录：先扣掉 header/padding/border，再 squarify
    let content = opts.policy.content_of(&rect);
    if content.is_degenerate() {
        return;
    }

    let ordered = sorted_children(tree, kids, opts.mode);
    let areas: Vec<f64> = ordered
        .iter()
        .map(|&i| {
            tree.node(i)
                .map(|n| opts.mode.of(n) as f64)
                .unwrap_or(0.0)
        })
        .collect();

    let rects = squarify(&areas, content);

    for (child, r) in ordered.iter().zip(rects.iter()) {
        if out.items.len() >= opts.policy.max_nodes {
            out.stats.hit_budget = true;
            break;
        }
        if r.is_degenerate() || r.short_side() < 1.0 {
            continue;
        }
        visit(tree, children, exts, opts, out, *child, *r, depth + 1);
    }
}

/// 计算着色参数。
fn shade_of(opts: &LayoutOptions, node: &Node, ext_id: u32, depth: u32) -> (u8, f32) {
    match opts.shading {
        ShadingMode::ByExtension => {
            let slot = if ext_id == u32::MAX {
                (SHADE_SLOTS - 1) as u8
            } else {
                (ext_id as usize % (SHADE_SLOTS - 1)) as u8
            };
            (slot, 0.5)
        }
        ShadingMode::ByDepth => {
            let slot = depth.min((SHADE_SLOTS - 1) as u32) as u8;
            (slot, (depth as f32 / 8.0).clamp(0.0, 1.0))
        }
        ShadingMode::ByAge => {
            let days = if node.mtime > 0 && opts.now_unix > node.mtime {
                (opts.now_unix - node.mtime) as f32 / 86_400.0
            } else {
                0.0
            };
            let t = (days / AGE_NORM_DAYS).clamp(0.0, 1.0);
            (0, t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
    use core_index::aggregate::aggregate;

    /// 构一棵三层的树：
    /// root(5) ─┬─ DirA(10) ─┬─ f(11) 1000000
    ///          │            └─ f(12) 500000
    ///          ├─ DirB(20) ─── f(21) 300000
    ///          └─ f(30) 200000
    fn sample_tree() -> Tree {
        let mut t = Tree::new("C:", 64);
        t.arena.ensure(40);

        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size: if dir { 0 } else { size },
                alloc: if dir { 0 } else { size },
                mtime: 1_700_000_000,
                name: off,
            };
        };

        set(&mut t, 5, 5, ".", true, 0);
        set(&mut t, 10, 5, "DirA", true, 0);
        set(&mut t, 11, 10, "big.bin", false, 1_000_000);
        set(&mut t, 12, 10, "mid.bin", false, 500_000);
        set(&mut t, 20, 5, "DirB", true, 0);
        set(&mut t, 21, 20, "small.log", false, 300_000);
        set(&mut t, 30, 5, "root.txt", false, 200_000);

        aggregate(&mut t);
        t
    }

    fn setup() -> (Tree, ChildIndex, ExtensionTable) {
        let t = sample_tree();
        let ci = ChildIndex::build(&t);
        let exts = ExtensionTable::build(&t);
        (t, ci, exts)
    }

    fn viewport() -> Rect {
        Rect::new(0.0, 0.0, 1200.0, 800.0)
    }

    #[test]
    fn layout_includes_root_and_children() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        assert!(r.find(5).is_some(), "根节点必须在结果里");
        assert!(r.find(10).is_some(), "DirA 应该可见");
        assert!(r.find(11).is_some(), "DirA 下的文件应该可见");
    }

    #[test]
    fn all_rects_inside_viewport() {
        let (t, ci, e) = setup();
        let vp = viewport();
        let opts = LayoutOptions::default();
        let r = compute_layout(&t, &ci, &e, 5, vp, &opts);
        for it in &r.items {
            assert!(it.rect.x >= vp.x - 1.0, "{:?}", it);
            assert!(it.rect.y >= vp.y - 1.0, "{:?}", it);
            assert!(it.rect.right() <= vp.right() + 1.0, "{:?}", it);
            assert!(it.rect.bottom() <= vp.bottom() + 1.0, "{:?}", it);
        }
    }

    #[test]
    fn parents_come_before_children() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let pos = |n: u32| r.items.iter().position(|it| it.node == n);
        assert!(pos(5).unwrap() < pos(10).unwrap());
        assert!(pos(10).unwrap() < pos(11).unwrap());
    }

    #[test]
    fn lod_prunes_small_boxes() {
        let (t, ci, e) = setup();
        // 阈值必须落在「根的短边」与「DirA 的短边」之间，否则测的不是 LOD 剪枝：
        //   根   = 整个视口 1200×800            → 短边 800
        //   内容 = 800 - header 16 - 2*inset 3  = 778 高
        //   三个孩子 1.5M / 0.3M / 0.2M 做 squarify，最大那个独占左列：
        //        宽 = 面积 / 内容高 ≈ 895，高 = 778 → 短边 778
        // 取 790：根能展开（800 ≥ 790），DirA 不能（778 < 790）。
        // 原来写 500 是拍脑袋的数 —— 500 比 778 小，DirA 照样会展开，测试必然失败。
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 790.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        assert!(r.find(10).is_some());
        assert!(r.find(11).is_none(), "DirA 应该被折叠，其子项不该出现");
        let dir_a = r.find(10).unwrap();
        assert!(dir_a.collapsed);
        assert!(dir_a.has_children);
        assert!(r.stats.collapsed_small > 0);
    }

    /// 边界：阈值恰好等于短边时**应当**展开（`can_descend` 用的是 `>=`）。
    ///
    /// 这一条把上面那个「拍脑袋阈值」的隐患钉死：改 `content_of` 的内缩量、
    /// 或改 squarify 的取整方向，都会让 DirA 的实际短边偏离 778，从而在这里暴露。
    #[test]
    fn lod_threshold_boundary_is_inclusive() {
        let (t, ci, e) = setup();
        let dir_a_side = {
            let opts = LayoutOptions {
                policy: LodPolicy { min_side: 1.0, ..Default::default() },
                ..Default::default()
            };
            let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
            r.find(10).unwrap().rect.short_side()
        };
        assert_eq!(dir_a_side, 778.0, "DirA 短边应由内容高决定");

        // 阈值 == 短边 → 展开
        let incl = LayoutOptions {
            policy: LodPolicy { min_side: dir_a_side, ..Default::default() },
            ..Default::default()
        };
        assert!(compute_layout(&t, &ci, &e, 5, viewport(), &incl).find(11).is_some());

        // 阈值比短边大一点点 → 折叠
        let excl = LayoutOptions {
            policy: LodPolicy { min_side: dir_a_side + 0.5, ..Default::default() },
            ..Default::default()
        };
        assert!(compute_layout(&t, &ci, &e, 5, viewport(), &excl).find(11).is_none());
    }

    #[test]
    fn max_depth_is_respected() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 1.0, max_depth: 1, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let max_d = r.items.iter().map(|i| i.depth).max().unwrap();
        assert!(max_d <= 1, "最大深度应为 1，实际 {}", max_d);
    }

    #[test]
    fn node_budget_caps_output() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { max_nodes: 2, min_side: 1.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        assert!(r.items.len() <= 2);
        assert!(r.stats.hit_budget);
    }

    #[test]
    fn hit_test_returns_deepest_node() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let big = r.find(11).expect("big.bin 可见");
        let (cx, cy) = big.rect.center();
        let hit = r.hit_test(cx, cy).unwrap();
        assert_eq!(hit, 11, "命中最深的矩形应是 big.bin");
    }

    #[test]
    fn hit_test_outside_returns_none() {
        let (t, ci, e) = setup();
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &LayoutOptions::default());
        assert!(r.hit_test(99999.0, 99999.0).is_none());
    }

    #[test]
    fn directory_headers_are_fixed_height() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let d = r.find(10).unwrap();
        assert_eq!(d.header.h, 16.0, "标题栏高度必须恒定 16px");
        assert!(d.header.w > 0.0);
    }

    #[test]
    fn file_items_have_no_header() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let f = r.find(11).unwrap();
        assert!(!f.is_dir);
        assert_eq!(f.header.area(), 0.0);
    }

    #[test]
    fn area_share_matches_size_share() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let big = r.find(11).unwrap().rect.area();
        let mid = r.find(12).unwrap().rect.area();
        // 1_000_000 : 500_000 = 2 : 1
        assert!((big / mid - 2.0).abs() < 0.15, "面积比 {}", big / mid);
    }

    /// 回归：0 字节文件**不能**抢走同级目录的方块。
    ///
    /// `squarify` 早期实现在「边铺边 push」时会把零面积元素写到错误的下标上，
    /// 于是面积大的目录拿到退化矩形、在 `visit` 里被 `is_degenerate()` 丢掉，
    /// 而 0 字节文件拿到了整个内容区。表现是「目录凭空消失、一个空文件占满屏」
    /// —— 一个不报任何错的静默错误。
    ///
    /// 这里是端到端复现：树 → 布局 → 断言大目录拿到内容区。
    #[test]
    fn zero_size_child_does_not_steal_sibling_rect() {
        let mut t = Tree::new("C:", 32);
        t.arena.ensure(20);
        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size: if dir { 0 } else { size },
                alloc: if dir { 0 } else { size },
                mtime: 1_700_000_000,
                name: off,
            };
        };
        set(&mut t, 5, 5, ".", true, 0);
        set(&mut t, 10, 5, "DirA", true, 0);
        set(&mut t, 11, 10, "big.bin", false, 1_000_000);
        set(&mut t, 12, 5, "empty.txt", false, 0); // 0 字节 —— 面积应为 0
        aggregate(&mut t);

        let ci = ChildIndex::build(&t);
        let e = ExtensionTable::build(&t);
        let policy = LodPolicy { min_side: 10.0, ..Default::default() };
        let opts = LayoutOptions { policy, ..Default::default() };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);

        let dir_a = r.find(10).expect("大目录必须出现在布局里");
        assert!(dir_a.rect.area() > 0.0, "大目录不该拿到退化矩形：{:?}", dir_a.rect);
        // 0 字节文件要么拿到退化矩形被跳过，要么拿到 0 面积；总之不能占据画面
        if let Some(z) = r.find(12) {
            assert!(z.rect.area() < 1.0, "0 字节文件不该占据画面：{:?}", z.rect);
        }
        // 内容区应当完全归大目录
        let content = policy.content_of(&viewport());
        assert!(
            (dir_a.rect.area() - content.area()).abs() < 1.0,
            "内容区 {:?} 应完整给大目录，实际 {:?}",
            content,
            dir_a.rect
        );
    }

    #[test]
    fn drilling_into_subtree_shows_its_children() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        // 下钻到 DirA
        let r = compute_layout(&t, &ci, &e, 10, viewport(), &opts);
        assert!(r.find(11).is_some());
        assert!(r.find(12).is_some());
        // 这次布局里不应出现 root 的其它孩子
        assert!(r.find(30).is_none());
    }

    #[test]
    fn visible_in_filters_viewport() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let all = r.visible_in(&viewport()).len();
        assert_eq!(all, r.items.len());
        let tiny = r.visible_in(&Rect::new(0.0, 0.0, 1.0, 1.0)).len();
        assert!(tiny < all);
    }

    #[test]
    fn shading_modes_produce_bounded_slots() {
        let (t, ci, e) = setup();
        for shading in [ShadingMode::ByExtension, ShadingMode::ByDepth, ShadingMode::ByAge] {
            let opts = LayoutOptions {
                policy: LodPolicy { min_side: 10.0, ..Default::default() },
                shading,
                now_unix: 1_700_000_000 + 86_400 * 100,
                ..Default::default()
            };
            let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
            for it in &r.items {
                assert!((it.shade_slot as usize) < SHADE_SLOTS);
                assert!((0.0..=1.0).contains(&it.shade_t));
            }
        }
    }

    /// **按类型着色必须真的分出颜色来。**
    ///
    /// 上面那条只检查「槽位在范围内」——上限永远满足，所以它对
    /// 「所有文件都落进同一个槽位」完全无感。而那个 bug 真的发生过：
    /// `ExtensionTable::id_of_node` 拿整个文件名去查以扩展名为键的表，
    /// 每个文件都拿到 `u32::MAX`，整张面积图一个颜色。
    ///
    /// 因此这里断言的是「**不同扩展名的文件拿到不同槽位**」，不是范围。
    #[test]
    fn by_extension_actually_splits_colors() {
        let (t, ci, e) = setup();
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            shading: ShadingMode::ByExtension,
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);

        // 样本树里有 .bin / .log / .txt 三种扩展名
        let slots: Vec<u8> = [11u32, 12, 21, 30]
            .iter()
            .filter_map(|n| r.find(*n))
            .map(|it| it.shade_slot)
            .collect();
        assert_eq!(slots.len(), 4, "四个文件都该出现在布局里");
        let mut uniq = slots.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert!(
            uniq.len() >= 2,
            "三种扩展名至少该分到两个色槽，实际 {slots:?}（0 个或 1 个色槽 = 整片同色）"
        );
        assert!(
            !slots.iter().all(|s| *s == (SHADE_SLOTS - 1) as u8),
            "全部落进「其他」桶 = 按名字查扩展名失败：{slots:?}"
        );
    }

    /// 无扩展名的文件有自己的槽位，**不该**和「其他」桶混在一起。
    #[test]
    fn extensionless_files_are_not_the_other_bucket() {
        let mut t = Tree::new("C:", 8);
        t.arena.ensure(4);
        let set = |t: &mut Tree, idx: u32, name: &str, dir: bool, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent: 0,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size,
                alloc: size,
                mtime: 1_700_000_000,
                name: off,
            };
        };
        set(&mut t, 0, ".", true, 0);
        set(&mut t, 1, "Makefile", false, 1_000_000);
        aggregate(&mut t);
        let ci = ChildIndex::build(&t);
        let e = ExtensionTable::build(&t);
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            shading: ShadingMode::ByExtension,
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 0, viewport(), &opts);
        let it = r.find(1).expect("Makefile 应有方块");
        assert_eq!(
            it.ext_id,
            e.id_of_ext("").expect("无扩展名应有独立的分组"),
            "无扩展名要归到空扩展名那一组，而不是「其他」"
        );
        assert_ne!(it.shade_slot as usize, SHADE_SLOTS - 1);
    }

    #[test]
    fn by_age_reflects_timestamps() {
        let (mut t, _, _) = setup();
        // 把 11 改成 3 年前
        t.arena.nodes[11].mtime = 1_700_000_000 - (86_400 * 365 * 3);
        let ci = ChildIndex::build(&t);
        let e = ExtensionTable::build(&t);
        let opts = LayoutOptions {
            policy: LodPolicy { min_side: 10.0, ..Default::default() },
            shading: ShadingMode::ByAge,
            now_unix: 1_700_000_000,
            ..Default::default()
        };
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &opts);
        let old = r.find(11).unwrap();
        let new = r.find(12).unwrap();
        assert!(old.shade_t > new.shade_t, "更旧的文件热度值应更大");
    }

    #[test]
    fn stats_summary_is_readable() {
        let (t, ci, e) = setup();
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &LayoutOptions::default());
        let s = r.stats.summary_cn();
        assert!(s.contains("矩形"));
    }

    #[test]
    fn empty_tree_does_not_panic() {
        let t = Tree::new("C:", 8);
        let ci = ChildIndex::build(&t);
        let e = ExtensionTable::build(&t);
        let r = compute_layout(&t, &ci, &e, 5, viewport(), &LayoutOptions::default());
        assert!(r.items.is_empty());
    }

    #[test]
    fn degenerate_viewport_yields_nothing() {
        let (t, ci, e) = setup();
        let r = compute_layout(&t, &ci, &e, 5, Rect::new(0.0, 0.0, 0.0, 0.0), &LayoutOptions::default());
        assert!(r.items.is_empty());
    }
}
