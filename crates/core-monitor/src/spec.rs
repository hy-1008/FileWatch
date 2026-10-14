//! 「人读写法 → 规则」的解析：`abs=1G` / `grow=500M` / `rate=10M/60s`。
//!
//! # 为什么在这里，而不是各自的入口
//!
//! 这两件事（体积写法、时间窗写法）原本长在 CLI 的 `args.rs` 里。GUI 的阈值
//! 输入框要用**同一套语法** —— 不然同一条规则在两个 front end 里会出现两种
//! 拼法，用户在 CLI 里学会的写法到界面上就不认了。所以规则域的东西放在
//! 规则所在的 crate 里，两个前端都从这里取。
//!
//! 本 crate 不依赖 `core-io`（见 `lib.rs` 的分层约定），所以这里**只解析、
//! 不格式化**：解析是语义，格式化是各前端自己的事（CLI 要对齐成列，
//! 界面要拼 HTML）。规则的**名字**是语义，统一用 [`RuleKind::label_cn`]。

use crate::rules::{Rule, RuleKind};

/// `1G` / `512MB` / `1024` → 字节数。使用 1024 进制（与资源管理器口径一致）。
pub fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let digits_end = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(digits_end);
    let base: f64 = num.parse().ok()?;
    let mul: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((base * mul) as u64)
}

/// `500ms` / `30s` / `5m` / `1h` → 毫秒。无单位按**秒**处理。
pub fn parse_ms(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits_end);
    let n: u64 = num.parse().ok()?;
    let mul: u64 = match unit {
        "" | "s" | "sec" | "secs" => 1000,
        "ms" => 1,
        "m" | "min" | "mins" => 60_000,
        "h" | "hr" => 3_600_000,
        _ => return None,
    };
    Some(n.saturating_mul(mul))
}

/// `abs=1G` / `grow=500M` / `rate=10M/60s` → [`Rule`]。
///
/// 错误信息是**给用户看**的：一定要带上正确的写法示例，否则用户只能猜。
pub fn parse_rule(s: &str) -> Result<Rule, String> {
    let (kind, body) = s
        .split_once('=')
        .or_else(|| s.split_once(':'))
        .ok_or_else(|| format!("规则「{}」缺少 `=`，应为 abs=1G / grow=500M / rate=10M/60s", s))?;
    let kind = kind.trim().to_ascii_lowercase();
    let body = body.trim();

    match kind.as_str() {
        "abs" | "absolute" | "绝对" => {
            let b = parse_size(body)
                .ok_or_else(|| format!("无法解析体积「{}」，示例：1G / 500MB / 2147483648", body))?;
            Ok(Rule::absolute(b))
        }
        "grow" | "growth" | "增长" => {
            let b = parse_size(body)
                .ok_or_else(|| format!("无法解析体积「{}」，示例：500M / 2G", body))?;
            Ok(Rule::growth(b))
        }
        "rate" | "速率" => {
            let (sz, win) = body
                .split_once('/')
                .ok_or_else(|| format!("速率规则需要写成 `rate=10M/60s`，收到「{}」", body))?;
            let bytes = parse_size(sz).ok_or_else(|| format!("无法解析速率「{}」", sz))?;
            let window = parse_ms(win).ok_or_else(|| format!("无法解析时间窗「{}」", win))?;
            if window == 0 {
                return Err("速率规则的时间窗不能为 0".to_string());
            }
            let per_sec = (bytes as u128 * 1000 / window as u128) as u64;
            Ok(Rule::rate(per_sec, window))
        }
        other => Err(format!(
            "未知规则类型「{}」，可选：abs（绝对阈值）/ grow（增长量）/ rate（速率）",
            other
        )),
    }
}

/// 批量解析（`--rule=a=1G` 可重复；界面输入框里是一行一条）。
///
/// 遇到第一条错误就返回 —— 半个规则集比没有规则更危险：用户会以为
/// 三条都生效了，其实只有前两条。错误里带上序号，便于对上输入。
pub fn parse_rules<I, S>(items: I) -> Result<Vec<Rule>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for (i, s) in items.into_iter().enumerate() {
        let s = s.as_ref().trim();
        if s.is_empty() {
            continue; // 空行/纯空格跳过：界面上换行多加一行不该算错
        }
        match parse_rule(s) {
            Ok(r) => out.push(r),
            Err(e) => return Err(format!("第 {} 条规则：{}", i + 1, e)),
        }
    }
    Ok(out)
}

/// 规则的简述（不含体积格式化，见模块头注释）。
///
/// 给的是**语义**而不是排版：`Absolute / 1073741824 / 0`。
/// 前端各自把它渲染成「绝对阈值 1.00 GB」。
pub fn rule_shape(r: &Rule) -> (RuleKind, u64, u64) {
    (r.kind, r.threshold, r.window_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_size_units() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1024));
        assert_eq!(parse_size("1M"), Some(1024 * 1024));
        assert_eq!(parse_size("1.5M"), Some(1024 * 1024 * 3 / 2));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1T"), Some(1024u64.pow(4)));
        // 大小写与空白都认
        assert_eq!(parse_size("  1 gb "), Some(1024 * 1024 * 1024));
        // 错的要老实说错，不能猜
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("   "), None);
        assert_eq!(parse_size("1X"), None);
        assert_eq!(parse_size("M"), None);
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_ms("500ms"), Some(500));
        assert_eq!(parse_ms("30s"), Some(30_000));
        // 无单位按秒 —— 这是 CLI 的老约定，别改
        assert_eq!(parse_ms("30"), Some(30_000));
        assert_eq!(parse_ms("5m"), Some(300_000));
        assert_eq!(parse_ms("1h"), Some(3_600_000));
        assert_eq!(parse_ms("xx"), None);
        assert_eq!(parse_ms(""), None);
    }

    #[test]
    fn parses_rules() {
        let r = parse_rule("abs=1G").unwrap();
        assert_eq!(r.kind, RuleKind::Absolute);
        assert_eq!(r.threshold, 1024 * 1024 * 1024);

        let g = parse_rule("grow=500M").unwrap();
        assert_eq!(g.kind, RuleKind::Growth);
        assert_eq!(g.threshold, 500 * 1024 * 1024);

        let t = parse_rule("rate=10M/60s").unwrap();
        assert_eq!(t.kind, RuleKind::Rate);
        assert_eq!(t.window_ms, 60_000);
        // 10 MiB / 60 s
        assert_eq!(t.threshold, 10 * 1024 * 1024 * 1000 / 60_000);

        // 冒号写法也认
        assert!(parse_rule("abs:2M").is_ok());
        assert!(parse_rule("bogus=1G").is_err());
        assert!(parse_rule("abs").is_err());
        assert!(parse_rule("rate=10M").is_err());
        assert!(parse_rule("rate=10M/0s").is_err());
    }

    /// 中文别名是给界面输入框留的：用户更可能敲「绝对=1G」而不是 `abs=1G`。
    #[test]
    fn accepts_chinese_aliases() {
        assert_eq!(parse_rule("绝对=1G").unwrap().kind, RuleKind::Absolute);
        assert_eq!(parse_rule("增长=500M").unwrap().kind, RuleKind::Growth);
        assert_eq!(parse_rule("速率=10M/60s").unwrap().kind, RuleKind::Rate);
    }

    /// 错误信息必须**带示例**。用户拿到「无法解析」这四个字是没法自救的。
    #[test]
    fn errors_are_actionable() {
        let e = parse_rule("abs=abc").unwrap_err();
        assert!(e.contains("1G") || e.contains("示例"), "错误里要有示例：{e}");
        let e = parse_rule("abs").unwrap_err();
        assert!(e.contains("abs=1G"), "错误里要写出正确形态：{e}");
        let e = parse_rule("rate=10M").unwrap_err();
        assert!(e.contains("rate=10M/60s"), "错误里要写出正确形态：{e}");
        let e = parse_rule("nope=1G").unwrap_err();
        assert!(e.contains("abs") && e.contains("grow") && e.contains("rate"),
            "未知类型要列出可选项：{e}");
    }

    #[test]
    fn batch_parse_skips_blanks_but_stops_on_error() {
        // 空行（界面输入框里多敲一个回车）不算错
        let v = parse_rules(["abs=1G", "", "  ", "grow=500M"]).unwrap();
        assert_eq!(v.len(), 2);

        // 一错全停：半个规则集比没有规则更危险
        let e = parse_rules(["abs=1G", "grow=xx", "rate=1M/1s"]).unwrap_err();
        assert!(e.contains("第 2 条"), "错误要指出是第几条：{e}");

        assert!(parse_rules(Vec::<String>::new()).unwrap().is_empty());
    }

    #[test]
    fn rule_shape_is_semantic_not_formatted() {
        let (k, bytes, win) = rule_shape(&Rule::rate(1024, 60_000));
        assert_eq!(k, RuleKind::Rate);
        assert_eq!(bytes, 1024);
        assert_eq!(win, 60_000);
    }
}
