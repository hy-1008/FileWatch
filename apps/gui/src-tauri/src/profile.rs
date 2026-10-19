//! 运行模式：**轻量 / 标准 / 性能**，以及由它派生出来的一组运行参数。
//!
//! # 为什么参数要在这一处算
//!
//! 「轻量模式省内存、性能模式多跑后台」听起来是界面上的两个开关，但真正
//! 省内存的是**具体数字**：面积图少铺多少个矩形、侧栏一次取多少行、扫描时
//! 要不要顺手建一把路径索引。这些数字一旦在前端再抄一份，两边迟早对不上，
//! 而且对不上时不报错 —— 只表现为「切了模式但没变化」。
//!
//! 所以这里是**唯一的定义处**：前端切换模式后调 `profile_budget` 把数字取回去，
//! 自己不做换算。
//!
//! # 三个模式各自放弃了什么
//!
//! * **轻量**：不建实时监视（路径索引那笔内存全省下）、面积图铺得更粗（矩形
//!   少 → 布局结果与 IPC 字节都少）、侧栏行数减半且拖到空闲时才加载。
//!   代价是"看不细"，但索引树本身（约 40 B/节点）是看面积图的前提，**省不掉** ——
//!   这点必须如实告诉用户，不能让"轻量"听起来像能把 200 万文件压成几十 MB。
//! * **标准**：当前行为，什么都不后台跑。
//! * **性能**：面积图铺得更细、行数放宽、扫描默认带上实时监视（换来"随时能
//!   开始监控"），并起一个后台周期任务刷新各盘符容量、可轮询转全量扫描。
//!   代价是常驻时多占一点内存、后台偶尔吃 IO。

use serde::Serialize;

/// 运行模式。字符串形态与 `GuiPrefs::profile` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 极致的空间占用：按需加载、能不建的东西都不建。
    Lean,
    /// 默认。
    Balanced,
    /// 自动异步懒加载 + 后台周期任务。
    Performance,
}

impl Profile {
    /// 从偏好里的字符串解析。**不认识的一律当标准** —— 一个笔误不该让界面
    /// 进入某种半残状态，和 `commands::size_mode` 是同一个处理口径。
    pub fn parse(s: Option<&str>) -> Profile {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("lean") | Some("light") | Some("轻量") => Profile::Lean,
            Some("performance") | Some("perf") | Some("性能") => Profile::Performance,
            _ => Profile::Balanced,
        }
    }

    /// 存回偏好的键名。前端下拉框的 `value` 就用它，两边对账在
    /// `tools/check-admin.js` 里。
    pub fn key(&self) -> &'static str {
        match self {
            Profile::Lean => "lean",
            Profile::Balanced => "balanced",
            Profile::Performance => "performance",
        }
    }

    pub fn label_cn(&self) -> &'static str {
        match self {
            Profile::Lean => "轻量模式",
            Profile::Balanced => "标准模式",
            Profile::Performance => "性能模式",
        }
    }

    /// 一句话说明代价。界面上直接显示，免得用户以为「轻量」是免费的。
    pub fn cost_cn(&self) -> &'static str {
        match self {
            Profile::Lean => "看得粗一些：面积图只铺大块、侧栏行数减半、不建实时监视。索引树本身（约 40 B/节点）是看面积图的前提，省不掉。",
            Profile::Balanced => "当前行为：不跑后台任务，需要什么加载什么。",
            Profile::Performance => "常驻多占一点内存，后台会周期性刷新盘符容量（可另开轮流全量扫描，会吃 IO）。",
        }
    }

    // ── 派生参数 ──────────────────────────────────────────────────────────

    /// 面积图 LOD 阈值的**倍率**（乘在「细节」滑块给出的阈值上）。
    ///
    /// 阈值越大 → 只铺大块 → 矩形越少。轻量放大（省内存、省 IPC 字节），
    /// 性能缩小（铺得更细）。
    pub fn lod_scale(&self) -> f32 {
        match self {
            Profile::Lean => 2.0,
            Profile::Balanced => 1.0,
            Profile::Performance => 0.5,
        }
    }

    /// 列表类面板一次取多少行的**倍率**（作用在前端给出的基数上）。
    pub fn row_scale(&self) -> f32 {
        match self {
            Profile::Lean => 0.5,
            Profile::Balanced => 1.0,
            Profile::Performance => 2.0,
        }
    }

    /// 扫描时是否默认勾上「同时建立实时监视」。
    ///
    /// 建监视要在索引树之外再挂一份路径索引（内存与时间都是实打实的），
    /// 轻量模式不该替它买单。性能模式反过来：留着它，用户想监控时点一下就行，
    /// 不用为了「补一个事件源」再重扫一遍（那次重扫才是真的慢）。
    pub fn default_watch(&self) -> bool {
        match self {
            Profile::Lean => false,
            Profile::Balanced => false,
            Profile::Performance => true,
        }
    }

    /// 侧栏（文件类型 / 最大的文件）是否拖到空闲时再加载。
    /// 轻量模式下先出面积图，侧栏等浏览器空闲；其余模式照旧一起加载。
    pub fn lazy_panels(&self) -> bool {
        matches!(self, Profile::Lean)
    }

    /// 是否起后台周期任务（刷新各盘符容量）。只有性能模式跑。
    pub fn background(&self) -> bool {
        matches!(self, Profile::Performance)
    }
}

/// 派生的具体数字。**只在这里产生**，前端拿去直接用。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Budget {
    pub profile: &'static str,
    pub label_cn: &'static str,
    pub cost_cn: &'static str,
    /// LOD 阈值倍率
    pub lod_scale: f32,
    /// 列表行数倍率
    pub row_scale: f32,
    /// 扫描时是否默认建立实时监视
    pub default_watch: bool,
    /// 侧栏是否空闲时才加载
    pub lazy_panels: bool,
    /// 是否跑后台周期任务
    pub background: bool,
}

impl Budget {
    pub fn of(p: Profile) -> Budget {
        Budget {
            profile: p.key(),
            label_cn: p.label_cn(),
            cost_cn: p.cost_cn(),
            lod_scale: p.lod_scale(),
            row_scale: p.row_scale(),
            default_watch: p.default_watch(),
            lazy_panels: p.lazy_panels(),
            background: p.background(),
        }
    }

    /// 列表行数：基数 × 倍率。**下限 1**：倍率再小也不能返回 0 行
    /// （前端拿到 0 会显示一个空面板，看起来像"加载失败"）。
    pub fn rows(&self, base: usize) -> usize {
        let n = (base as f32 * self.row_scale).round();
        if n < 1.0 { 1 } else { n as usize }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_text_is_balanced() {
        // 一个笔误（或把显示名当键传进来）不该让界面进入半残状态
        for s in ["", "  ", "LEAN MODE", "turbo", "轻量模式", "标准模式"] {
            assert_eq!(Profile::parse(Some(s)), Profile::Balanced, "{s:?} 不认识 → 标准");
        }
        assert_eq!(Profile::parse(None), Profile::Balanced);
        assert_eq!(Profile::parse(Some("lean")), Profile::Lean);
        assert_eq!(Profile::parse(Some("轻量")), Profile::Lean);
        assert_eq!(Profile::parse(Some("  PERFORMANCE ")), Profile::Performance);
        assert_eq!(Profile::parse(Some("perf")), Profile::Performance);
    }

    #[test]
    fn keys_round_trip() {
        for p in [Profile::Lean, Profile::Balanced, Profile::Performance] {
            assert_eq!(Profile::parse(Some(p.key())), p, "{:?} 的键名必须能解析回自己", p);
        }
    }

    /// 轻量的三个省内存开关必须真的"更省"，性能必须真的"更放"。
    /// 只断言"在范围内"对"三个模式给同样的数"是无感的 —— 那正是这条要挡的。
    #[test]
    fn lean_really_costs_less_than_performance() {
        let lean = Budget::of(Profile::Lean);
        let perf = Budget::of(Profile::Performance);
        let std = Budget::of(Profile::Balanced);

        assert!(lean.lod_scale > std.lod_scale, "轻量的 LOD 阈值必须更粗（矩形更少）");
        assert!(perf.lod_scale < std.lod_scale, "性能的 LOD 阈值必须更细（铺得更满）");

        assert!(lean.rows(200) < std.rows(200), "轻量取的行数必须更少");
        assert!(perf.rows(200) > std.rows(200), "性能取的行数必须更多");

        assert!(!lean.default_watch, "轻量不该默认建实时监视（那是笔内存）");
        assert!(perf.default_watch, "性能模式留着实时监视，省一次重扫");

        assert!(lean.lazy_panels, "轻量把侧栏拖到空闲时加载");
        assert!(!std.lazy_panels);

        assert!(!lean.background, "轻量不跑任何后台任务");
        assert!(!std.background, "标准不跑后台任务");
        assert!(perf.background, "性能模式才跑后台周期任务");
    }

    /// 倍率再小也不能给出 0 行：前端拿到 0 会显示空面板，看起来像加载失败。
    #[test]
    fn row_limit_never_drops_to_zero() {
        for p in [Profile::Lean, Profile::Balanced, Profile::Performance] {
            let b = Budget::of(p);
            for base in [0usize, 1, 30, 60, 200] {
                assert!(b.rows(base) >= 1, "{:?} 在基数 {base} 下行数不能为 0", p);
            }
        }
    }

    #[test]
    fn labels_are_not_empty() {
        for p in [Profile::Lean, Profile::Balanced, Profile::Performance] {
            assert!(!p.label_cn().is_empty());
            assert!(!p.cost_cn().is_empty(), "代价说明必须写清楚，别让轻量听起来是免费的");
        }
    }
}
