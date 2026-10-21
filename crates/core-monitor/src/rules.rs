//! 阈值规则引擎（文档 §5.4）。
//!
//! 三种规则可任意组合：
//!
//! | 规则 | 判据 | 用途 |
//! |---|---|---|
//! | 绝对阈值 | `总大小 > 阈值` | 容量管控 |
//! | 增量阈值 | `总大小 − 监控起点基线 > 阈值` | 变化量管控 |
//! | 速率阈值 | `(总大小 − 窗口起点大小) / 窗口秒数 > 阈值` | 异常写入检测 |
//!
//! ## 为什么求值是 O(1)
//!
//! 关键在调用方式：**引擎只在「沿父链上传 delta」的过程中、遇到被监控目录时**才调用
//! `evaluate()`（见文档 §5.4 伪代码）。因此不需要重算子树，单次求值纯算术。
//!
//! ## 抑制机制（文档 §3.2 / §5.4）
//!
//! * **滞回**：触发后必须跌回阈值 90% 以下才允许再次触发，避免在阈值附近抖动刷屏；
//! * **冷却期**：同一目标同一规则 10 分钟内只报一次；
//! * **同批合并**：由 `alert::build_batch_notification` 负责。

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleKind {
    Absolute,
    Growth,
    Rate,
}

impl RuleKind {
    pub const ALL: [RuleKind; 3] = [RuleKind::Absolute, RuleKind::Growth, RuleKind::Rate];

    #[inline]
    pub const fn slot(self) -> usize {
        match self {
            RuleKind::Absolute => 0,
            RuleKind::Growth => 1,
            RuleKind::Rate => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Absolute => "absolute",
            RuleKind::Growth => "growth",
            RuleKind::Rate => "rate",
        }
    }

    pub fn label_cn(self) -> &'static str {
        match self {
            RuleKind::Absolute => "绝对阈值",
            RuleKind::Growth => "增量阈值",
            RuleKind::Rate => "速率阈值",
        }
    }

    pub fn parse(s: &str) -> Option<RuleKind> {
        match s.to_ascii_lowercase().as_str() {
            "absolute" | "abs" | "绝对" => Some(RuleKind::Absolute),
            "growth" | "grow" | "增量" => Some(RuleKind::Growth),
            "rate" | "速率" => Some(RuleKind::Rate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub kind: RuleKind,
    /// 阈值（字节）。`Rate` 规则的阈值单位是「字节 / 秒」。
    pub threshold: u64,
    /// 仅 `Rate` 规则使用：滑动窗口长度（毫秒）
    pub window_ms: u64,
    pub enabled: bool,
}

impl Rule {
    pub fn absolute(bytes: u64) -> Rule {
        Rule {
            kind: RuleKind::Absolute,
            threshold: bytes,
            window_ms: 0,
            enabled: true,
        }
    }

    pub fn growth(bytes: u64) -> Rule {
        Rule {
            kind: RuleKind::Growth,
            threshold: bytes,
            window_ms: 0,
            enabled: true,
        }
    }

    pub fn rate(bytes_per_sec: u64, window_ms: u64) -> Rule {
        Rule {
            kind: RuleKind::Rate,
            threshold: bytes_per_sec,
            window_ms,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RuleConfig {
    /// 滞回系数：跌到 `阈值 × ratio` 以下才复位（文档 §3.2：90%）
    pub hysteresis_ratio: f64,
    /// 冷却期：同目标同规则在此时间内只报一次（文档：10 分钟）
    pub cooldown_ms: u64,
}

impl Default for RuleConfig {
    fn default() -> Self {
        RuleConfig {
            hysteresis_ratio: 0.9,
            cooldown_ms: 10 * 60 * 1000,
        }
    }
}

/// 一个被监控的目标（任意目录）。
#[derive(Debug, Clone)]
pub struct Target {
    /// 节点下标（= MFT 记录号），与被监控目录一一对应
    pub node: u32,
    pub path: String,
    pub rules: Vec<Rule>,
    /// 监控起点大小（增量阈值的基线）
    pub baseline_size: u64,
    /// 速率窗口起点
    pub window_start_ms: u64,
    pub window_start_size: u64,
    /// 各规则的「已触发」状态（滞回用）
    firing: [bool; 3],
    cooldown_until: [u64; 3],
    /// 最近一次求值时的总大小
    pub last_size: u64,
    /// 触发历史计数
    pub fire_count: [u64; 3],
}

impl Target {
    pub fn new(node: u32, path: impl Into<String>, rules: Vec<Rule>, current_size: u64, now_ms: u64) -> Target {
        Target {
            node,
            path: path.into(),
            rules,
            baseline_size: current_size,
            window_start_ms: now_ms,
            window_start_size: current_size,
            firing: [false; 3],
            cooldown_until: [0; 3],
            last_size: current_size,
            fire_count: [0; 3],
        }
    }

    /// 重新设置基线（用户点「开始监控」或手动清零时）。
    pub fn rebaseline(&mut self, current_size: u64, now_ms: u64) {
        self.baseline_size = current_size;
        self.window_start_size = current_size;
        self.window_start_ms = now_ms;
    }

    #[inline]
    pub fn growth(&self, current: u64) -> u64 {
        current.saturating_sub(self.baseline_size)
    }

    #[inline]
    pub fn is_firing(&self, kind: RuleKind) -> bool {
        self.firing[kind.slot()]
    }
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub node: u32,
    pub path: String,
    pub kind: RuleKind,
    pub threshold: u64,
    pub current_size: u64,
    pub baseline_size: u64,
    /// 实际观测到的量：绝对=总量；增量=增量；速率=字节/秒
    pub observed: u64,
    pub at_ms: u64,
    pub message: String,
}

impl Alert {
    /// 去重键：同目标 + 同规则视为「相同内容」。
    pub fn dedup_key(&self) -> String {
        format!("{}|{}|{}", self.node, self.kind.as_str(), self.threshold)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RuleStats {
    pub evaluations: u64,
    pub fired: u64,
    pub suppressed_by_cooldown: u64,
    pub suppressed_by_hysteresis: u64,
    pub resets: u64,
}

/// 规则引擎。只对**被注册的节点**求值，其余节点的调用直接返回 `None`（O(1) 查表）。
#[derive(Debug)]
pub struct RuleEngine {
    cfg: RuleConfig,
    targets: Vec<Target>,
    index: HashMap<u32, usize>,
    pub stats: RuleStats,
}

impl RuleEngine {
    pub fn new(cfg: RuleConfig) -> RuleEngine {
        RuleEngine {
            cfg,
            targets: Vec::new(),
            index: HashMap::new(),
            stats: RuleStats::default(),
        }
    }

    pub fn config(&self) -> RuleConfig {
        self.cfg
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn contains(&self, node: u32) -> bool {
        self.index.contains_key(&node)
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    pub fn target(&self, node: u32) -> Option<&Target> {
        self.index.get(&node).and_then(|&i| self.targets.get(i))
    }

    /// 注册/覆盖一个监控目标，返回其下标。
    pub fn add_target(
        &mut self,
        node: u32,
        path: impl Into<String>,
        rules: Vec<Rule>,
        current_size: u64,
        now_ms: u64,
    ) -> usize {
        let t = Target::new(node, path, rules, current_size, now_ms);
        match self.index.get(&node).copied() {
            Some(i) => {
                self.targets[i] = t;
                i
            }
            None => {
                self.targets.push(t);
                let i = self.targets.len() - 1;
                self.index.insert(node, i);
                i
            }
        }
    }

    /// 移除监控目标（swap_remove，保持其余下标稳定通过 `index` 重算）。
    pub fn remove_target(&mut self, node: u32) -> bool {
        match self.index.remove(&node) {
            Some(i) => {
                self.targets.remove(i);
                // remove 后其后的元素整体前移，重建索引
                self.index.clear();
                for (k, t) in self.targets.iter().enumerate() {
                    self.index.insert(t.node, k);
                }
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.targets.clear();
        self.index.clear();
    }

    /// 重新设置某目标的基线。
    pub fn rebaseline(&mut self, node: u32, current_size: u64, now_ms: u64) -> bool {
        match self.index.get(&node).copied() {
            Some(i) => {
                self.targets[i].rebaseline(current_size, now_ms);
                true
            }
            None => false,
        }
    }

    /// 对某个节点求值。**只有被监控的节点才会真正计算**；其余立即返回 `None`。
    ///
    /// 由 `core-scan` 在「沿父链上传 delta」的循环里调用（文档 §5.4）。
    pub fn evaluate(&mut self, node: u32, total_size: u64, now_ms: u64) -> Option<Alert> {
        let i = *self.index.get(&node)?;
        self.stats.evaluations += 1;

        let cfg = self.cfg;
        let t = &mut self.targets[i];
        t.last_size = total_size;

        let mut fired: Option<Alert> = None;

        // 规则按固定顺序求值，保证行为可预测
        for rule in t.rules.clone() {
            if !rule.enabled {
                continue;
            }
            let slot = rule.kind.slot();

            let (observed, over) = match rule.kind {
                RuleKind::Absolute => {
                    let over = total_size > rule.threshold;
                    (total_size, over)
                }
                RuleKind::Growth => {
                    let g = total_size.saturating_sub(t.baseline_size);
                    (g, g > rule.threshold)
                }
                RuleKind::Rate => {
                    let secs = if rule.window_ms == 0 {
                        0.0
                    } else {
                        (rule.window_ms as f64) / 1000.0
                    };
                    if secs <= 0.0 {
                        continue;
                    }
                    let grew = total_size.saturating_sub(t.window_start_size) as f64;
                    let rate = grew / secs;
                    (rate as u64, rate > rule.threshold as f64)
                }
            };

            // 滞回复位
            let reset_below = (rule.threshold as f64 * cfg.hysteresis_ratio) as u64;
            let below = match rule.kind {
                RuleKind::Absolute => total_size < reset_below,
                RuleKind::Growth => {
                    total_size.saturating_sub(t.baseline_size) < reset_below
                }
                RuleKind::Rate => observed < reset_below,
            };
            if below && t.firing[slot] {
                t.firing[slot] = false;
                self.stats.resets += 1;
            }

            if !over {
                continue;
            }
            if t.firing[slot] {
                self.stats.suppressed_by_hysteresis += 1;
                continue;
            }
            if now_ms < t.cooldown_until[slot] {
                self.stats.suppressed_by_cooldown += 1;
                continue;
            }

            // 触发
            t.firing[slot] = true;
            t.cooldown_until[slot] = now_ms.saturating_add(cfg.cooldown_ms);
            t.fire_count[slot] += 1;
            self.stats.fired += 1;

            if fired.is_none() {
                fired = Some(Alert {
                    node,
                    path: t.path.clone(),
                    kind: rule.kind,
                    threshold: rule.threshold,
                    current_size: total_size,
                    baseline_size: t.baseline_size,
                    observed,
                    at_ms: now_ms,
                    message: build_message(
                        &t.path,
                        rule.kind,
                        rule.threshold,
                        total_size,
                        t.baseline_size,
                        observed,
                    ),
                });
            }
        }

        // 速率规则的滑动窗口推进
        for rule in t.rules.clone() {
            if rule.kind == RuleKind::Rate && rule.window_ms > 0 {
                if now_ms.saturating_sub(t.window_start_ms) >= rule.window_ms {
                    t.window_start_ms = now_ms;
                    t.window_start_size = total_size;
                }
            }
        }

        fired
    }
}

/// 拼一条人话预警消息。**消息里带路径**：批量通知的正文只列出各条消息，
/// 那时的标题是「N 条」而不含路径，不写路径就不知道说的是谁。
pub(crate) fn build_message(
    path: &str,
    kind: RuleKind,
    threshold: u64,
    current: u64,
    baseline: u64,
    observed: u64,
) -> String {
    use core_io::util::human_size;
    match kind {
        RuleKind::Absolute => format!(
            "{} 总大小 {}，超过绝对阈值 {}",
            path,
            human_size(current),
            human_size(threshold)
        ),
        RuleKind::Growth => format!(
            "{} 自监控起点增长 {}（当前 {}），超过增量阈值 {}",
            path,
            human_size(current.saturating_sub(baseline)),
            human_size(current),
            human_size(threshold)
        ),
        RuleKind::Rate => format!(
            "{} 增长速率约 {}/秒，超过速率阈值 {}/秒",
            path,
            human_size(observed),
            human_size(threshold)
        ),
    }
}

// ── 规则试算（E4）──────────────────────────────────────────────────────────
//
// 写规则的门槛不在语法，在**判断**：`abs=1G` 到底会不会立刻刷屏、`grow=500M`
// 是不是比这台机器一天的正常写入量还低。解析器只能告诉你"语法对不对"，
// 试算才能告诉你"会不会报"。
//
// 与 `RuleEngine::evaluate` 的关系：**判据完全一致，但不带状态**。试算不读也不写
// 触发状态（firing）与冷却期，所以它回答的是"以现在的数据，这条规则**会不会**
// 触发"，而不是"此刻轮询到它会不会真的弹窗"（后者还要看抑制状态）。
// 这个区别必须保留：带状态就变成"试算一次等于消耗一次冷却"，没人敢点第二下。

/// 试算的一个目标样本。
///
/// 由调用方从真实运行状态里取（监控目标的基线、速率窗口起点），不要自己编 ——
/// 编出来的数只能证明算术，证明不了规则好不好使。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub path: String,
    /// 当前总大小
    pub size: u64,
    /// 增量规则的基线（监控起点大小）
    pub baseline: u64,
    /// 速率窗口起点大小
    pub window_size: u64,
    /// 监控已经跑了多久（毫秒）。0 = 刚开始，速率规则还判断不了。
    pub elapsed_ms: u64,
}

impl Sample {
    /// "没有历史"的样本：基线与窗口都等于当前大小。
    ///
    /// 没开始监控时只能这样 —— 于是增量规则算出 0、速率规则判不了，这是**如实**的
    /// 结果，而不是"没超阈值"。界面必须把这一点说出来，否则用户会以为规则设得太松。
    pub fn instant(path: impl Into<String>, size: u64) -> Sample {
        Sample {
            path: path.into(),
            size,
            baseline: size,
            window_size: size,
            elapsed_ms: 0,
        }
    }
}

/// 单条规则的试算结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub path: String,
    /// 在传入的 `rules` 里的下标（界面要能回指到用户写的那一行）
    pub rule_index: usize,
    pub kind: RuleKind,
    pub threshold: u64,
    /// 实际观测到的量：绝对=总量；增量=增量；速率=字节/秒
    pub observed: u64,
    /// 与阈值的差：正数=超出，负数=还差多少（取绝对值即"差多少"）
    pub gap: i64,
    /// 会不会立刻触发
    pub fires: bool,
    /// 这条规则在这个样本上能不能判断（速率规则没有时间跨度时为 false）
    pub judgeable: bool,
}

/// 对一批样本试算一组规则。顺序与 `samples × rules` 的嵌套顺序一致，
/// 界面按这个顺序渲染就行，不需要自己再配一遍。
pub fn simulate(rules: &[Rule], samples: &[Sample]) -> Vec<Verdict> {
    let mut out = Vec::with_capacity(rules.len() * samples.len());
    for s in samples {
        for (i, rule) in rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            let (observed, judgeable) = match rule.kind {
                RuleKind::Absolute => (s.size, true),
                RuleKind::Growth => (s.size.saturating_sub(s.baseline), true),
                RuleKind::Rate => {
                    if rule.window_ms == 0 || s.elapsed_ms == 0 {
                        // 分母是**规则写的窗口长度**，与 `evaluate` 一致：
                        // 试算若用实际经过时长，就会和运行时的判据不一致，
                        // 那这个功能就没有意义了。
                        (0, false)
                    } else {
                        let grew = s.size.saturating_sub(s.window_size);
                        (grew.saturating_mul(1000) / rule.window_ms, true)
                    }
                }
            };
            let fires = judgeable && observed > rule.threshold;
            let gap = observed as i64 - rule.threshold as i64;
            out.push(Verdict {
                path: s.path.clone(),
                rule_index: i,
                kind: rule.kind,
                threshold: rule.threshold,
                observed,
                gap,
                fires,
                judgeable,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    fn engine() -> RuleEngine {
        RuleEngine::new(RuleConfig::default())
    }

    // ── 规则试算（E4）────────────────────────────────────────────────────

    #[test]
    fn simulate_mirrors_the_runtime_judgement() {
        let rules = vec![Rule::absolute(10 * GB), Rule::growth(2 * GB)];
        let s = Sample {
            path: "C:\\logs".into(),
            size: 12 * GB,
            baseline: 9 * GB,
            window_size: 9 * GB,
            elapsed_ms: 60_000,
        };
        let v = simulate(&rules, &[s]);
        assert_eq!(v.len(), 2);
        // 绝对：12 > 10 → 立刻触发，超出 2 GB
        assert!(v[0].fires);
        assert_eq!(v[0].observed, 12 * GB);
        assert_eq!(v[0].gap, 2 * GB as i64);
        // 增量：只涨了 3 GB > 2 GB → 也触发
        assert!(v[1].fires);
        assert_eq!(v[1].observed, 3 * GB);
    }

    #[test]
    fn simulate_reports_how_far_off_a_rule_is() {
        let v = simulate(
            &[Rule::absolute(10 * GB)],
            &[Sample {
                path: "C:\\a".into(),
                size: 4 * GB,
                baseline: 4 * GB,
                window_size: 4 * GB,
                elapsed_ms: 0,
            }],
        );
        assert!(!v[0].fires);
        assert_eq!(v[0].gap, -(6 * GB as i64), "还差 6 GB 才到阈值");
    }

    #[test]
    fn rate_rule_needs_a_window_to_be_judged() {
        let rule = Rule::rate(10 * MB, 60_000);
        // 没有时间跨度（刚点开始监控）→ 不能判断，而不是"不触发"
        let v = simulate(
            &[rule],
            &[Sample {
                path: "C:\\a".into(),
                size: 5 * GB,
                baseline: 5 * GB,
                window_size: 5 * GB,
                elapsed_ms: 0,
            }],
        );
        assert!(!v[0].judgeable);
        assert!(!v[0].fires);

        // 60 秒里涨了 1.2 GB → 20 MB/s > 10 MB/s
        let v = simulate(
            &[rule],
            &[Sample {
                path: "C:\\a".into(),
                size: 5 * GB + 1_200 * MB,
                baseline: 5 * GB,
                window_size: 5 * GB,
                elapsed_ms: 60_000,
            }],
        );
        assert!(v[0].judgeable);
        assert!(v[0].fires);
        assert_eq!(v[0].observed, 20 * MB);
    }

    #[test]
    fn simulate_keeps_rule_order_and_target_order() {
        let rules = vec![Rule::absolute(1), Rule::growth(1), Rule::rate(1, 60_000)];
        let samples = vec![
            Sample { path: "a".into(), size: 9, baseline: 0, window_size: 0, elapsed_ms: 60_000 },
            Sample { path: "b".into(), size: 9, baseline: 0, window_size: 0, elapsed_ms: 60_000 },
        ];
        let v = simulate(&rules, &samples);
        assert_eq!(v.len(), 6, "每个目标 × 每条规则各一条结论");
        assert_eq!(v[0].path, "a");
        assert_eq!(v[3].path, "b");
        assert_eq!(v[0].rule_index, 0);
        assert_eq!(v[2].rule_index, 2);
    }

    #[test]
    fn only_monitored_nodes_are_evaluated() {
        let mut e = engine();
        e.add_target(10, "C:\\logs", vec![Rule::absolute(GB)], 0, 0);
        assert!(e.evaluate(99, 999 * GB, 1000).is_none());
        assert_eq!(e.stats.evaluations, 0, "未监控节点不应产生求值");
        assert!(e.evaluate(10, 999 * GB, 1000).is_some());
        assert_eq!(e.stats.evaluations, 1);
    }

    #[test]
    fn absolute_threshold_fires_once() {
        let mut e = engine();
        e.add_target(10, "C:\\logs", vec![Rule::absolute(10 * GB)], 0, 0);
        // 第一次超阈值 → 触发
        let a = e.evaluate(10, 11 * GB, 1000).expect("应触发");
        assert_eq!(a.kind, RuleKind::Absolute);
        assert!(a.message.contains("绝对阈值"));
        // 仍在阈值之上 → 滞回抑制
        assert!(e.evaluate(10, 12 * GB, 2000).is_none());
        assert!(e.stats.suppressed_by_hysteresis > 0);
    }

    #[test]
    fn hysteresis_requires_drop_below_90_percent() {
        let mut e = engine();
        e.add_target(10, "p", vec![Rule::absolute(10 * GB)], 0, 0);
        assert!(e.evaluate(10, 11 * GB, 1000).is_some());
        // 降到 9.5GB（> 90% = 9GB）→ 不应复位
        assert!(e.evaluate(10, 95 * GB / 10, 2000).is_none());
        // 降到 8GB（< 9GB）→ 复位
        assert!(e.evaluate(10, 8 * GB, 3000).is_none());
        assert_eq!(e.stats.resets, 1);
        // 再次超阈值 → 可以再触发
        assert!(e.evaluate(10, 11 * GB, 4_000_000_000).is_some());
    }

    #[test]
    fn cooldown_suppresses_repeat_alerts() {
        let mut e = engine();
        e.add_target(10, "p", vec![Rule::absolute(1 * GB)], 0, 0);
        assert!(e.evaluate(10, 2 * GB, 0).is_some());
        // 复位
        assert!(e.evaluate(10, 0, 1).is_none());
        // 冷却期内再超阈值 → 抑制
        assert!(e.evaluate(10, 2 * GB, 1000).is_none());
        assert!(e.stats.suppressed_by_cooldown > 0);
        // 冷却期满（10 分钟）→ 可以触发
        assert!(e.evaluate(10, 2 * GB, 10 * 60 * 1000 + 1).is_some());
    }

    #[test]
    fn growth_threshold_uses_baseline() {
        let mut e = engine();
        // 基线 5GB，增量阈值 2GB
        e.add_target(10, "p", vec![Rule::growth(2 * GB)], 5 * GB, 0);
        assert!(e.evaluate(10, 6 * GB, 100).is_none(), "只增了 1GB");
        let a = e.evaluate(10, 8 * GB, 200).expect("增了 3GB，应触发");
        assert_eq!(a.kind, RuleKind::Growth);
        assert_eq!(a.observed, 3 * GB);
        assert!(a.message.contains("增量阈值"));
    }

    #[test]
    fn rate_threshold_uses_sliding_window() {
        let mut e = engine();
        // 5 分钟窗口，速率阈值 100MB/s
        e.add_target(10, "p", vec![Rule::rate(100 * MB, 5 * 60 * 1000)], 0, 0);
        // 5 分钟增长 1GB → 速率 ≈ 3.4MB/s，不触发
        assert!(e.evaluate(10, 1 * GB, 5 * 60 * 1000).is_none());
        // 下一个窗口暴涨 100GB → 速率 ≈ 341MB/s，触发
        let a = e.evaluate(10, 101 * GB, 10 * 60 * 1000).expect("应触发");
        assert_eq!(a.kind, RuleKind::Rate);
        assert!(a.message.contains("速率"));
    }

    #[test]
    fn rate_rule_without_window_is_skipped() {
        let mut e = engine();
        let mut r = Rule::rate(1, 0);
        r.window_ms = 0;
        e.add_target(10, "p", vec![r], 0, 0);
        assert!(e.evaluate(10, u64::MAX / 2, 1000).is_none());
    }

    #[test]
    fn multiple_rules_first_match_wins() {
        let mut e = engine();
        e.add_target(
            10,
            "p",
            vec![Rule::absolute(1 * GB), Rule::growth(1 * MB)],
            0,
            0,
        );
        let a = e.evaluate(10, 2 * GB, 10).unwrap();
        assert_eq!(a.kind, RuleKind::Absolute, "规则按声明顺序求值");
    }

    #[test]
    fn disabled_rules_do_not_fire() {
        let mut e = engine();
        let mut r = Rule::absolute(1);
        r.enabled = false;
        e.add_target(10, "p", vec![r], 0, 0);
        assert!(e.evaluate(10, u64::MAX, 10).is_none());
    }

    #[test]
    fn rebaseline_resets_growth() {
        let mut e = engine();
        e.add_target(10, "p", vec![Rule::growth(1 * GB)], 0, 0);
        assert!(e.evaluate(10, 2 * GB, 10).is_some());
        assert!(e.rebaseline(10, 2 * GB, 20));
        // 基线重置后，同样的 2GB 不再是「增长」
        assert_eq!(e.target(10).unwrap().growth(2 * GB), 0);
    }

    #[test]
    fn add_target_overwrites_existing() {
        let mut e = engine();
        e.add_target(10, "a", vec![Rule::absolute(1)], 0, 0);
        e.add_target(10, "b", vec![Rule::absolute(2)], 0, 0);
        assert_eq!(e.len(), 1);
        assert_eq!(e.target(10).unwrap().path, "b");
    }

    #[test]
    fn remove_target_keeps_index_consistent() {
        let mut e = engine();
        e.add_target(1, "a", vec![Rule::absolute(MB)], 0, 0);
        e.add_target(2, "b", vec![Rule::absolute(MB)], 0, 0);
        e.add_target(3, "c", vec![Rule::absolute(MB)], 0, 0);
        assert!(e.remove_target(2));
        assert!(!e.remove_target(2));
        assert_eq!(e.len(), 2);
        // 索引应已重建：1 和 3 仍可求值，2 不可
        assert!(e.evaluate(1, 2 * MB, 10).is_some());
        assert!(e.evaluate(2, 2 * MB, 10).is_none());
        assert!(e.evaluate(3, 2 * MB, 10).is_some());
    }

    #[test]
    fn dedup_key_groups_same_target_and_rule() {
        let mut e = engine();
        e.add_target(10, "p", vec![Rule::absolute(1)], 0, 0);
        let a1 = e.evaluate(10, 2, 0).unwrap();
        assert_eq!(a1.dedup_key(), "10|absolute|1");
    }

    #[test]
    fn fire_counts_are_tracked() {
        let mut e = engine();
        e.add_target(10, "p", vec![Rule::absolute(1 * MB)], 0, 0);
        assert!(e.evaluate(10, 2 * MB, 0).is_some()); // fire #1
        assert!(e.evaluate(10, 0, 1).is_none()); // reset
        assert!(e.evaluate(10, 2 * MB, 20 * 60 * 1000).is_some()); // fire #2
        assert_eq!(e.target(10).unwrap().fire_count[RuleKind::Absolute.slot()], 2);
    }

    #[test]
    fn kind_parsing() {
        assert_eq!(RuleKind::parse("absolute"), Some(RuleKind::Absolute));
        assert_eq!(RuleKind::parse("增量"), Some(RuleKind::Growth));
        assert_eq!(RuleKind::parse("rate"), Some(RuleKind::Rate));
        assert_eq!(RuleKind::parse("nope"), None);
    }

    #[test]
    fn message_is_human_readable() {
        let mut e = engine();
        e.add_target(10, r"C:\Windows\Logs", vec![Rule::absolute(1 * GB)], 0, 0);
        let a = e.evaluate(10, 3 * GB, 0).unwrap();
        assert!(a.message.contains(r"C:\Windows\Logs"));
        assert!(a.message.contains("GB"));
    }
}
