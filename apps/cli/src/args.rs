//! 自包含命令行解析（零依赖，不引 `clap`）。
//!
//! 支持三种形式：
//! * `--key=value` —— 明确的键值对
//! * `--key value` —— 仅当 `key` 在 [`VALUE_KEYS`] 白名单里（避免把位置参数吞掉）
//! * `--flag`      —— 布尔开关
//!
//! 位置参数按顺序进 [`Args::positional`]，可重复出现的选项（如 `--rule`）
//! 进 [`Args::multi`]。

use core_monitor::{Rule, RuleKind};
use std::collections::HashMap;

/// 会「吃掉」后面一个 token 作为值的选项名。
///
/// 白名单而不是「看下一个是不是 `--` 开头」：后者会把
/// `filewatch monitor C:\data --quiet` 里的路径误吞。
const VALUE_KEYS: &[&str] = &[
    "root",
    "rule",
    "n",
    "top",
    "exts",
    "layout",
    "duration",
    "interval",
    "changelog",
    "chunk",
    "max-depth",
    "watch-max-nodes",
    "watch-buffer",
    "format",
    "mode",
    "limit",
    "tree",
    "journal-size",
    "journal-delta",
    "out",
    // F12 重复文件查找
    "min-size",
    "block",
    "max-per-group",
    "max-candidates",
    "csv",
    // F11 快照对比
    "max-entries",
    "min-delta",
];

#[derive(Debug, Default)]
pub struct Args {
    /// 子命令
    pub cmd: String,
    /// 位置参数
    pub positional: Vec<String>,
    /// 布尔开关
    pub flags: Vec<String>,
    /// 单值选项
    pub opts: HashMap<String, String>,
    /// 可重复选项（`--rule` 等）
    pub multi: HashMap<String, Vec<String>>,
}

impl Args {
    /// 解析**已去掉程序名**的参数序列：第一个元素就是子命令。
    ///
    /// 调用方一律传 `std::env::args().skip(1)`（见 `main.rs`）。
    /// 这个函数**不负责**剔除 `argv[0]` —— 早期这里的文档注释写成「跳过 argv[0]」，
    /// 与实现不符，结果测试助手照着注释多塞了一个 `"filewatch"` 进去，
    /// 于是 `cmd` 变成了 `"filewatch"`、位置参数整体错位。文档写错会直接
    /// 生产出错误的测试，所以这里把契约写清楚。
    pub fn parse<I: Iterator<Item = String>>(mut it: I) -> Args {
        let mut a = Args::default();
        a.cmd = it.next().unwrap_or_default();
        let rest: Vec<String> = it.collect();
        let mut i = 0usize;
        while i < rest.len() {
            let t = &rest[i];
            if let Some(body) = t.strip_prefix("--") {
                if let Some((k, v)) = body.split_once('=') {
                    a.put(k, v);
                } else if VALUE_KEYS.contains(&body) && i + 1 < rest.len() {
                    a.put(body, &rest[i + 1]);
                    i += 1;
                } else {
                    a.flags.push(body.to_string());
                }
            } else if let Some(body) = t.strip_prefix('-') {
                // 单横线短选项：`-n=20` 也接受
                if let Some((k, v)) = body.split_once('=') {
                    a.put(k, v);
                } else {
                    a.flags.push(body.to_string());
                }
            } else {
                a.positional.push(t.clone());
            }
            i += 1;
        }
        a
    }

    fn put(&mut self, k: &str, v: &str) {
        self.opts.insert(k.to_string(), v.to_string());
        self.multi
            .entry(k.to_string())
            .or_default()
            .push(v.to_string());
    }

    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// 取单值选项：**同名重复出现时取最后一个**（与 `getopts` / `clap` 一致）。
    ///
    /// 需要「全部取值」请用 [`Args::get_all`]（`--rule` 这类可重复选项就该用它）。
    pub fn get(&self, k: &str) -> Option<&str> {
        self.opts.get(k).map(|s| s.as_str())
    }

    pub fn get_all(&self, k: &str) -> &[String] {
        self.multi.get(k).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 取数值选项，缺省时用 `default`；解析失败返回错误信息。
    pub fn num(&self, k: &str, default: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => v
                .parse::<usize>()
                .map_err(|_| format!("--{} 需要一个非负整数，收到「{}」", k, v)),
        }
    }

    pub fn path(&self, k: &str) -> Option<std::path::PathBuf> {
        self.get(k).map(std::path::PathBuf::from)
    }
}

/// `1G` / `512MB` / `1024` → 字节数；`500ms` / `30s` / `5m` → 毫秒；
/// `abs=1G` / `rate=10M/60s` → [`Rule`]。
///
/// **这三个函数住在 `core_monitor::spec` 里**，这里只是转出去给 CLI 用。
/// 它们原来长在这个文件里，后来 GUI 的阈值输入框要用同一套语法 ——
/// 一份语法两处实现，迟早会出现「CLI 认 `abs=1G`、界面不认」这种别扭事。
/// 规则的解析属于规则域，所以搬到了规则所在的 crate。
pub use core_monitor::spec::{parse_ms, parse_rule, parse_size};

/// 规则的中文描述（回显用）。
///
/// 留在这里而不是跟着解析一起搬走：它要做**列对齐**和体积格式化，那是
/// 终端的事。规则的名字（绝对/增量/速率）仍取自 `RuleKind::label_cn()`，
/// 所以措辞只有一处来源。
pub fn rule_label(r: &Rule) -> String {
    match r.kind {
        RuleKind::Absolute => format!("绝对阈值 {}", human(r.threshold)),
        RuleKind::Growth => format!("增长量 {}", human(r.threshold)),
        RuleKind::Rate => format!(
            "速率 {}/{}",
            human(r.threshold),
            human_ms(r.window_ms)
        ),
    }
}

fn human(b: u64) -> String {
    core_io::util::human_size(b)
}

fn human_ms(ms: u64) -> String {
    if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{}ms", ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 `filewatch <子命令> ...` 的**真实形态**构造参数：和 `main.rs` 一样
    /// 传「去掉程序名之后」的序列。以前这里会先插一个 `"filewatch"` 当 `argv[0]`，
    /// 但 `Args::parse` 并不剔除它 —— 于是 `cmd` 成了 `"filewatch"`、位置参数
    /// 整体后移一位，三条测试因此长期误报失败。
    fn parse(line: &str) -> Args {
        Args::parse(line.split_whitespace().map(|s| s.to_string()))
    }

    #[test]
    fn parses_subcommand_and_positional() {
        let a = parse("probe C:");
        assert_eq!(a.cmd, "probe");
        assert_eq!(a.positional, vec!["C:".to_string()]);
    }

    /// 契约：`Args::parse` 的**第一个元素就是子命令**，剔除 `argv[0]` 是调用方
    /// 的责任（`main.rs` 传 `.skip(1)`）。
    ///
    /// 这条测试专门用来防「文档说跳过 argv[0]、实现却没跳」这类文档/代码不一致：
    /// 一旦有人在 `parse` 里「顺手」加一句丢弃首个元素，这里立刻红。
    #[test]
    fn first_element_is_the_subcommand_not_argv0() {
        let a = Args::parse(["probe", "C:"].iter().map(|s| s.to_string()));
        assert_eq!(a.cmd, "probe", "第一个元素必须是子命令");
        assert_eq!(a.positional, vec!["C:".to_string()]);

        // 一个参数都没有也不能 panic
        let empty = Args::parse(std::iter::empty::<String>());
        assert_eq!(empty.cmd, "");
        assert!(empty.positional.is_empty());
        assert!(empty.flags.is_empty());
    }

    #[test]
    fn parses_eq_form_and_space_form() {
        let a = parse("scan C: --top=50 --exts 10");
        assert_eq!(a.get("top"), Some("50"));
        assert_eq!(a.get("exts"), Some("10"));
    }

    #[test]
    fn does_not_swallow_positional_after_unknown_key() {
        // --quiet 不在白名单里，后面的 C:\data 必须仍是位置参数
        let a = parse("monitor --quiet C:\\data");
        assert!(a.has("quiet"));
        assert_eq!(a.positional, vec!["C:\\data".to_string()]);
    }

    #[test]
    fn collects_repeatable_rules() {
        let a = parse("monitor C:\\data --rule=abs=1G --rule=grow=500M");
        assert_eq!(a.get_all("rule"), &["abs=1G".to_string(), "grow=500M".to_string()]);
        // 单值取「最后一个」；要全部就用 get_all
        assert_eq!(a.get("rule"), Some("grow=500M"));
        // 子命令与位置参数不能被可重复选项挤掉
        assert_eq!(a.cmd, "monitor");
        assert_eq!(a.positional, vec!["C:\\data".to_string()]);
    }

    #[test]
    fn num_reports_clear_error() {
        let a = parse("scan C: --top=abc");
        let e = a.num("top", 20).unwrap_err();
        assert!(e.contains("--top"));
        assert!(e.contains("abc"));
        assert_eq!(parse("scan C:").num("top", 20).unwrap(), 20);
    }

    #[test]
    fn rule_label_is_readable() {
        assert!(rule_label(&Rule::absolute(1024 * 1024)).contains("1.00 MB"));
        assert!(rule_label(&Rule::rate(1024, 60_000)).contains("1m"));
    }
}
