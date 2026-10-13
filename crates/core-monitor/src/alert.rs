//! 预警分发与抑制（文档 §3.2 / §5.4）。
//!
//! 三层抑制：
//! 1. **滞回 + 冷却** —— 在 [`crate::rules`] 里，按「目标 × 规则」维度；
//! 2. **内容去重** —— 这里，相同内容 1 分钟内不重复弹；
//! 3. **同批合并** —— 这里，一个 tick 内的多条预警合成一条通知。

use crate::rules::Alert;
use core_io::util::human_size;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    pub fn color_hint(self) -> &'static str {
        match self {
            Severity::Info => "#2f80ed",
            Severity::Warning => "#f2994a",
            Severity::Critical => "#eb5757",
        }
    }
}

/// 超出阈值的比例越高，级别越高。**单纯超阈值 = Warning**，超 2 倍 = Critical。
pub fn severity_of(observed: u64, threshold: u64) -> Severity {
    if threshold == 0 {
        return Severity::Warning;
    }
    let ratio = observed as f64 / threshold as f64;
    if ratio >= 2.0 {
        Severity::Critical
    } else if ratio > 1.0 {
        Severity::Warning
    } else {
        Severity::Info
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// 合并了多少条预警
    pub count: usize,
    pub severity: Severity,
    pub at_ms: u64,
    /// 原始预警（UI 可展开详情）
    pub alerts: Vec<Alert>,
}

/// 内容去重器：相同 `dedup_key` 在 `dedup_window_ms` 内只放行一次。
#[derive(Debug)]
pub struct AlertSink {
    dedup_window_ms: u64,
    recent: HashMap<String, u64>,
    pub emitted: u64,
    pub suppressed: u64,
}

impl AlertSink {
    /// `dedup_window_ms` 默认取文档 §5.4 的「1 分钟」。
    pub fn new(dedup_window_ms: u64) -> AlertSink {
        AlertSink {
            dedup_window_ms,
            recent: HashMap::new(),
            emitted: 0,
            suppressed: 0,
        }
    }

    /// 是否应该放行这条预警。
    pub fn should_emit(&mut self, a: &Alert, now_ms: u64) -> bool {
        let key = a.dedup_key();
        if let Some(&last) = self.recent.get(&key) {
            if now_ms.saturating_sub(last) < self.dedup_window_ms {
                self.suppressed += 1;
                return false;
            }
        }
        self.recent.insert(key, now_ms);
        self.emitted += 1;
        true
    }

    /// 清理过期的去重记录，避免长期运行内存缓慢增长。
    pub fn gc(&mut self, now_ms: u64) {
        let w = self.dedup_window_ms;
        self.recent.retain(|_, &mut t| now_ms.saturating_sub(t) < w);
    }

    pub fn tracked_keys(&self) -> usize {
        self.recent.len()
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }
}

/// 把一个 tick 内的预警合并成一条通知。
///
/// * 0 条 → `None`
/// * 1 条 → 直接用它
/// * N 条 → 标题「文件监控预警（N 条）」，正文列出前 5 条 + 「还有 …」
pub fn build_batch_notification(alerts: &[Alert]) -> Option<Notification> {
    if alerts.is_empty() {
        return None;
    }

    let severity = alerts
        .iter()
        .map(|a| severity_of(a.observed, a.threshold))
        .max()
        .unwrap_or(Severity::Warning);
    let at_ms = alerts.iter().map(|a| a.at_ms).max().unwrap_or(0);

    if alerts.len() == 1 {
        let a = &alerts[0];
        return Some(Notification {
            // 标题只给「哪一类预警」，路径由正文负责。
            // 曾经标题里也带路径（`【增量阈值】C:\x`），而正文第一条又是
            // `<路径> 自监控起点增长 …` —— 于是 `render_one_line` 拼出来的
            // 一行里同一个路径出现两遍，读起来像故障。正文承担「在哪、差多少」
            // 之后，通知无论单独看还是并进批量列表都能自洽。
            title: format!("【{}】", a.kind.label_cn()),
            body: a.message.clone(),
            count: 1,
            severity,
            at_ms,
            alerts: vec![a.clone()],
        });
    }

    const MAX_LINES: usize = 5;
    let mut body = String::new();
    for a in alerts.iter().take(MAX_LINES) {
        body.push_str("· ");
        body.push_str(&a.message);
        body.push('\n');
    }
    if alerts.len() > MAX_LINES {
        body.push_str(&format!("… 另有 {} 条同类预警", alerts.len() - MAX_LINES));
    }

    let total_excess: u64 = alerts
        .iter()
        .map(|a| a.observed.saturating_sub(a.threshold))
        .sum();

    Some(Notification {
        title: format!("文件监控预警（{} 条，超阈值合计 {}）", alerts.len(), human_size(total_excess)),
        body,
        count: alerts.len(),
        severity,
        at_ms,
        alerts: alerts.to_vec(),
    })
}

/// 把通知渲染成一行文本（CLI / 日志用）。
pub fn render_one_line(n: &Notification) -> String {
    format!(
        "[{}] {} — {}",
        n.severity.as_str().to_uppercase(),
        n.title,
        n.body.replace('\n', " | ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleKind;

    fn alert(node: u32, kind: RuleKind, observed: u64, threshold: u64, at_ms: u64) -> Alert {
        Alert {
            node,
            path: format!("C:\\p{}", node),
            kind,
            threshold,
            current_size: observed,
            baseline_size: 0,
            observed,
            at_ms,
            message: format!("p{} 超阈值", node),
        }
    }

    #[test]
    fn dedup_suppresses_within_window() {
        let mut s = AlertSink::new(60_000);
        let a = alert(1, RuleKind::Absolute, 100, 50, 0);
        assert!(s.should_emit(&a, 1000));
        assert!(!s.should_emit(&a, 2000), "1 分钟内重复不应放行");
        assert_eq!(s.suppressed, 1);
        assert!(s.should_emit(&a, 62_000), "超过窗口应放行");
        assert_eq!(s.emitted, 2);
    }

    #[test]
    fn different_targets_are_not_deduplicated() {
        let mut s = AlertSink::new(60_000);
        assert!(s.should_emit(&alert(1, RuleKind::Absolute, 100, 50, 0), 1000));
        assert!(s.should_emit(&alert(2, RuleKind::Absolute, 100, 50, 0), 1000));
        assert_eq!(s.emitted, 2);
    }

    #[test]
    fn different_rules_are_not_deduplicated() {
        let mut s = AlertSink::new(60_000);
        assert!(s.should_emit(&alert(1, RuleKind::Absolute, 100, 50, 0), 1000));
        assert!(s.should_emit(&alert(1, RuleKind::Growth, 100, 50, 0), 1000));
        assert_eq!(s.emitted, 2);
    }

    #[test]
    fn gc_drops_expired_keys() {
        let mut s = AlertSink::new(1000);
        s.should_emit(&alert(1, RuleKind::Absolute, 1, 1, 0), 0);
        assert_eq!(s.tracked_keys(), 1);
        s.gc(5000);
        assert_eq!(s.tracked_keys(), 0);
        // gc 后可以重新触发
        assert!(s.should_emit(&alert(1, RuleKind::Absolute, 1, 1, 0), 5000));
    }

    #[test]
    fn severity_scales_with_excess() {
        assert_eq!(severity_of(90, 100), Severity::Info);
        assert_eq!(severity_of(150, 100), Severity::Warning);
        assert_eq!(severity_of(200, 100), Severity::Critical);
        assert_eq!(severity_of(500, 100), Severity::Critical);
        assert_eq!(severity_of(10, 0), Severity::Warning);
    }

    #[test]
    fn single_alert_notification() {
        let a = alert(1, RuleKind::Absolute, 300, 100, 123);
        let n = build_batch_notification(&[a]).unwrap();
        assert_eq!(n.count, 1);
        assert_eq!(n.severity, Severity::Critical);
        assert!(n.title.contains("绝对阈值"));
        assert_eq!(n.at_ms, 123);
    }

    #[test]
    fn batch_merges_into_one_notification() {
        let alerts: Vec<Alert> = (1..=8)
            .map(|i| alert(i, RuleKind::Growth, 300, 100, 1000))
            .collect();
        let n = build_batch_notification(&alerts).unwrap();
        assert_eq!(n.count, 8);
        assert!(n.title.contains("8 条"));
        assert!(n.body.contains("另有 3 条"));
        assert_eq!(n.alerts.len(), 8);
    }

    #[test]
    fn empty_batch_yields_none() {
        assert!(build_batch_notification(&[]).is_none());
    }

    #[test]
    fn batch_severity_is_max() {
        let alerts = vec![
            alert(1, RuleKind::Absolute, 110, 100, 0), // Warning
            alert(2, RuleKind::Absolute, 500, 100, 0), // Critical
            alert(3, RuleKind::Absolute, 90, 100, 0),  // Info
        ];
        let n = build_batch_notification(&alerts).unwrap();
        assert_eq!(n.severity, Severity::Critical);
    }

    #[test]
    fn batch_reports_total_excess() {
        let alerts = vec![
            alert(1, RuleKind::Absolute, 300, 100, 0),
            alert(2, RuleKind::Absolute, 250, 100, 0),
        ];
        let n = build_batch_notification(&alerts).unwrap();
        // (300-100) + (250-100) = 350 字节
        assert!(n.title.contains(&human_size(350)));
    }

    #[test]
    fn one_line_rendering() {
        let n = build_batch_notification(&[alert(1, RuleKind::Rate, 300, 100, 0)]).unwrap();
        let line = render_one_line(&n);
        assert!(line.contains("CRITICAL"));
        assert!(!line.contains('\n'));
    }

    /// 回归：单条预警渲染成一行时，路径只能出现**一次**。
    ///
    /// 标题带路径 + 正文也以路径开头 → `render_one_line` 拼出的
    /// `[CRITICAL] 【增量阈值】C:\x — C:\x 自监控起点增长 …` 里路径出现两遍，
    /// 是纯粹的自伤。这里用「出现次数」把它钉住。
    ///
    /// 消息用生产路径上真正的 [`build_message`](crate::rules::build_message) 拼，
    /// 而不是手写一个假的 —— 否则「消息里到底有没有路径」这个前提就测不到。
    #[test]
    fn single_alert_line_mentions_path_only_once() {
        let mut a = alert(7, RuleKind::Growth, 1_000, 100, 0);
        a.message = crate::rules::build_message(&a.path, a.kind, a.threshold, a.current_size, a.baseline_size, a.observed);
        assert!(a.message.contains(r"C:\p7"), "前提：消息里带路径，实际 {}", a.message);

        let n = build_batch_notification(&[a]).unwrap();
        let line = render_one_line(&n);
        assert!(n.title.contains("增量阈值"), "标题应点名预警类型：{}", n.title);
        assert_eq!(line.matches(r"C:\p7").count(), 1, "路径应只出现一次：{}", line);
    }

    /// 多条合并时，正文里的每一条都必须自带路径 —— 那时标题是「N 条」，
    /// 不写路径根本不知道说的是谁。所以消息里**保留**路径是有意为之。
    #[test]
    fn batch_body_keeps_path_per_alert() {
        let mk = |node: u32, observed: u64| {
            let mut a = alert(node, RuleKind::Absolute, observed, 100, 0);
            a.message = crate::rules::build_message(
                &a.path,
                a.kind,
                a.threshold,
                a.current_size,
                a.baseline_size,
                a.observed,
            );
            a
        };
        let n = build_batch_notification(&[mk(1, 300), mk(2, 250)]).unwrap();
        assert!(n.body.contains(r"C:\p1"), "正文：{}", n.body);
        assert!(n.body.contains(r"C:\p2"), "正文：{}", n.body);
    }
}
