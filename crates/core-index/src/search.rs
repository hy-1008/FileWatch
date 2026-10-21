//! 在**已扫出来的索引树**里搜文件名。
//!
//! 为什么不去遍历磁盘：扫描一次已经把 200 万个目录项（名字、大小、时间）
//! 装进了内存，搜名字就是扫一遍内存里的字符串池 —— 百万级节点几十毫秒。
//! 重新 `FindFirstFile` 一遍要几十秒到几分钟，而且结果可能和界面上看到的
//! 面积图对不上（两次遍历之间磁盘会变）。
//!
//! # 三种模式
//!
//! | 模式 | 语义 | 适合 |
//! |---|---|---|
//! | `Contains` | 大小写不敏感的子串 | "我知道名字里有什么" |
//! | `Fuzzy` | 子序列（`rpt` 命中 `report`），连续命中加分 | "我记得大概是这几个字母" |
//! | `Regex` | 简化正则（见 [`SimpleRegex`]） | `^202[45].*\.xlsx$` |
//!
//! # 为什么正则是自己写的
//!
//! 引擎层坚持零第三方依赖（`crates/**` 不引入 `regex` 这类库），而"搜索"
//! 又不该只在界面里能用 —— CLI 早晚要一条同样的命令，两套正则方言迟早
//! 会给出不同结果。所以这里实现一个**够用的子集**：字面量、`.`、`*`、`+`、
//! `?`、`[abc]`、`[a-z]`、`[^...]`、`\` 转义、`^`/`$` 锚点。
//! 不支持 `|`、`()`、`{n,m}` —— 遇到就报错说清楚，而不是悄悄当成字面量。

use crate::tree::Tree;

/// 匹配方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Contains,
    Fuzzy,
    Regex,
}

impl MatchMode {
    pub fn parse(s: &str) -> MatchMode {
        match s {
            "fuzzy" => MatchMode::Fuzzy,
            "regex" => MatchMode::Regex,
            _ => MatchMode::Contains,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            MatchMode::Contains => "contains",
            MatchMode::Fuzzy => "fuzzy",
            MatchMode::Regex => "regex",
        }
    }
}

/// 搜文件、搜目录，还是都搜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Files,
    Dirs,
    Both,
}

impl Scope {
    pub fn parse(s: &str) -> Scope {
        match s {
            "dirs" => Scope::Dirs,
            "both" => Scope::Both,
            _ => Scope::Files,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Scope::Files => "files",
            Scope::Dirs => "dirs",
            Scope::Both => "both",
        }
    }
    fn accepts(self, is_dir: bool) -> bool {
        match self {
            Scope::Files => !is_dir,
            Scope::Dirs => is_dir,
            Scope::Both => true,
        }
    }
}

/// 一条命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub node: u32,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: i64,
    /// 相关度（越大越靠前）。正则模式统一给 600。
    pub score: i32,
}

/// 搜索统计。`matched` 是**全部**命中数，`hits` 只留前 `limit` 条 ——
/// 界面上"共 3,214 个命中，只列前 500 个"就来自这两个数。
#[derive(Debug, Clone)]
pub struct Outcome {
    pub hits: Vec<Hit>,
    pub scanned: u64,
    pub matched: u64,
    pub truncated: bool,
}

pub struct Query<'a> {
    pub text: &'a str,
    pub mode: MatchMode,
    pub scope: Scope,
    pub limit: usize,
}

/// 名字之外的附加条件（E8）：体积 / 修改时间 / 扩展名。
///
/// 这些条件在**同一个循环里**判，而不是先搜再过滤：过滤放在外面的话，
/// `limit` 会先被不合格的命中占满，用户看到"只找到 3 个"而其实有 300 个 ——
/// 这类"数字对不上"最难解释。所以 `matched` 只统计**通过全部条件**的条目。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// 体积下界（含），字节
    pub min_size: Option<u64>,
    /// 体积上界（含），字节
    pub max_size: Option<u64>,
    /// 修改时间下界（含），Unix 秒
    pub after: Option<i64>,
    /// 修改时间上界（含），Unix 秒
    pub before: Option<i64>,
    /// 扩展名白名单（小写、不带点）。空 = 不限
    pub exts: Vec<String>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.min_size.is_none()
            && self.max_size.is_none()
            && self.after.is_none()
            && self.before.is_none()
            && self.exts.is_empty()
    }

    /// 解析 `--exts=mp4,mkv,.AVI` 这种写法：统一小写、去掉前导点、去空项。
    ///
    /// 前端与 CLI 都走它，免得一边接受 `.MP4` 另一边不接受。
    pub fn parse_exts(s: &str) -> Vec<String> {
        s.split(|c| c == ',' || c == ';' || c == ' ')
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    }

    fn accepts(&self, name: &str, size: u64, mtime: i64) -> bool {
        if let Some(v) = self.min_size {
            if size < v {
                return false;
            }
        }
        if let Some(v) = self.max_size {
            if size > v {
                return false;
            }
        }
        if let Some(v) = self.after {
            if mtime < v {
                return false;
            }
        }
        if let Some(v) = self.before {
            if mtime > v {
                return false;
            }
        }
        if !self.exts.is_empty() {
            let ext = crate::tree::extension_of(name).to_lowercase();
            if !self.exts.iter().any(|e| *e == ext) {
                return false;
            }
        }
        true
    }
}

/// 一次搜索。空关键字直接返回空结果（不把"所有文件"倒进列表）。
pub fn search(tree: &Tree, q: &Query<'_>) -> Result<Outcome, String> {
    search_filtered(tree, q, &Filters::default())
}

/// [`search`] + 附加条件（体积 / 时间 / 扩展名）。
pub fn search_filtered(tree: &Tree, q: &Query<'_>, f: &Filters) -> Result<Outcome, String> {
    let text = q.text.trim();
    if text.is_empty() {
        return Ok(Outcome { hits: Vec::new(), scanned: 0, matched: 0, truncated: false });
    }
    let needle = text.to_lowercase();
    let re = if q.mode == MatchMode::Regex {
        Some(SimpleRegex::new(text)?)
    } else {
        None
    };
    let limit = q.limit.clamp(1, 5000);
    let mut out = Outcome { hits: Vec::new(), scanned: 0, matched: 0, truncated: false };

    for (i, node) in tree.arena.nodes.iter().enumerate() {
        if !node.in_use() || i == 0 {
            continue; // 0 号是占位；根节点也不是"搜索结果"
        }
        let is_dir = node.is_dir();
        if !q.scope.accepts(is_dir) {
            continue;
        }
        out.scanned += 1;
        let name = tree.name(i as u32);
        if name.is_empty() {
            continue;
        }
        // 附加条件先判：它们比（可能带正则的）名字匹配便宜得多
        if !f.is_empty() && !f.accepts(name, node.size, node.mtime) {
            continue;
        }
        let score = match q.mode {
            MatchMode::Contains => contains_score(name, &needle),
            MatchMode::Fuzzy => fuzzy_score(name, &needle),
            MatchMode::Regex => match re.as_ref().unwrap().is_match(name)? {
                true => Some(600),
                false => None,
            },
        };
        let Some(score) = score else { continue };
        out.matched += 1;
        if out.hits.len() >= limit {
            out.truncated = true;
            continue;
        }
        out.hits.push(Hit {
            node: i as u32,
            name: name.to_string(),
            // 只有命中的才构造完整路径：path_of 要沿父链拼字符串，
            // 对 200 万个节点全做一遍等于白烧几百毫秒。
            path: tree.path_of(i as u32),
            size: node.size,
            is_dir,
            mtime: node.mtime,
            score,
        });
    }

    // 相关度高的在前；同分时大文件在前（同样叫 report.pdf，用户多半在找大的那个），
    // 再同分按名字（保证结果稳定，不会因为遍历顺序变了而跳动）。
    out.hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.size.cmp(&a.size))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// 子串匹配的相关度：
/// 完全相等 > 前缀 > 中间出现（越靠前分越高）。
fn contains_score(name: &str, needle_lower: &str) -> Option<i32> {
    let lower = name.to_lowercase();
    let pos = lower.find(needle_lower)?;
    Some(if lower == *needle_lower {
        1000
    } else if pos == 0 {
        900
    } else {
        // 位置越靠后越不重要，但差距不能大到盖过"相等/前缀"这两档
        700 - (pos.min(100) as i32)
    })
}

/// 模糊（子序列）匹配：`q` 的字符按顺序全部出现即可。
///
/// 分数由三件事决定：**连续命中的段落**（人打 `rpt` 时心里想的是 `report`
/// 里的连续片段）、**名字长度**（同样命中时短名更可能是想要的）、
/// **命中位置**（从头开始匹配的更像）。
fn fuzzy_score(name: &str, needle_lower: &str) -> Option<i32> {
    let chars: Vec<char> = name.to_lowercase().chars().collect();
    let mut want = needle_lower.chars().peekable();
    let mut score = 500i32;
    let mut run = 0i32; // 当前连续命中长度
    let mut first_hit: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        let next = match want.peek() {
            Some(n) => *n,
            None => break,
        };
        if *c == next {
            want.next();
            run += 1;
            score += 3 + run * 2; // 连续越久，每个字符加得越多
            if first_hit.is_none() {
                first_hit = Some(i);
            }
        } else {
            run = 0;
        }
    }
    if want.peek().is_some() {
        return None; // 没匹配完
    }
    // 名字越长，越可能是"凑巧"命中一堆字符
    score -= (chars.len().min(80) as i32) / 4;
    if let Some(i) = first_hit {
        score -= (i.min(40) as i32) / 2;
    }
    Some(score)
}

// ── 简化正则 ────────────────────────────────────────────────────────────────

/// 一个匹配**单个字符**的原子。
#[derive(Debug, Clone)]
enum Atom {
    Char(char),
    Any,
    Class { negative: bool, ranges: Vec<(char, char)> },
}

#[derive(Debug, Clone)]
enum Token {
    One(Atom),
    /// `*`：0 次或多次
    Star(Atom),
    /// `+`：1 次或多次
    Plus(Atom),
    /// `?`：0 次或 1 次
    Opt(Atom),
}

/// 简化正则。语法见模块文档。
#[derive(Debug, Clone)]
pub struct SimpleRegex {
    tokens: Vec<Token>,
    anchored_start: bool,
    anchored_end: bool,
}

/// 回溯预算。`(a+)+` 这类写法在长名字上会指数爆炸 —— 这是自写正则最容易
/// 踩的坑，所以每一万步检查一次，超了直接报错（而不是让界面卡死）。
const STEP_BUDGET: u32 = 200_000;

impl SimpleRegex {
    pub fn new(pattern: &str) -> Result<SimpleRegex, String> {
        let mut chars: Vec<char> = pattern.chars().collect();
        let mut anchored_start = false;
        let mut anchored_end = false;
        if chars.first() == Some(&'^') {
            anchored_start = true;
            chars.remove(0);
        }
        if chars.last() == Some(&'$') {
            anchored_end = true;
            chars.pop();
        }
        let mut tokens: Vec<Token> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let atom = match chars[i] {
                '.' => { i += 1; Atom::Any }
                '\\' => {
                    let escaped = chars.get(i + 1).ok_or("正则以 \\ 结尾：后面缺少要转义的字符")?;
                    i += 2;
                    Atom::Char(lower(*escaped))
                }
                '[' => {
                    let (atom, next) = parse_class(&chars, i)?;
                    i = next;
                    atom
                }
                '*' | '+' | '?' => {
                    return Err(format!(
                        "「{}」前面没有可重复的字符（不支持的写法：连续的 * + ?）",
                        chars[i]
                    ))
                }
                '|' | '(' | ')' | '{' | '}' => {
                    return Err(format!(
                        "暂不支持「{}」：这个搜索框只认 字面量 . * + ? [abc] [a-z] [^…] ^ $ 与 \\ 转义",
                        chars[i]
                    ))
                }
                c => { i += 1; Atom::Char(lower(c)) }
            };
            // 紧跟的重复符号
            let token = match chars.get(i) {
                Some('*') => { i += 1; Token::Star(atom) }
                Some('+') => { i += 1; Token::Plus(atom) }
                Some('?') => { i += 1; Token::Opt(atom) }
                _ => Token::One(atom),
            };
            tokens.push(token);
        }
        Ok(SimpleRegex { tokens, anchored_start, anchored_end })
    }

    /// 是否命中。`Err` 只可能来自回溯预算耗尽。
    pub fn is_match(&self, text: &str) -> Result<bool, String> {
        let chars: Vec<char> = text.chars().map(lower).collect();
        if self.anchored_start {
            return self.run(&chars, 0);
        }
        for start in 0..=chars.len() {
            if self.run(&chars, start)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn run(&self, text: &[char], start: usize) -> Result<bool, String> {
        let mut budget = STEP_BUDGET;
        let ok = self.match_tokens(&self.tokens, text, start, &mut budget)?;
        if ok && self.anchored_end {
            // `$` 只在"匹配吃掉了整串"时成立：交给 match_tokens 的
            // end 检查，见下面的 `end_ok`
            return Ok(true);
        }
        Ok(ok)
    }

    fn match_tokens(
        &self,
        tokens: &[Token],
        text: &[char],
        pos: usize,
        budget: &mut u32,
    ) -> Result<bool, String> {
        if *budget == 0 {
            return Err("正则太复杂，回溯超出预算 —— 换一个更明确的写法（比如去掉嵌套的 * +）".into());
        }
        *budget -= 1;

        let Some((tok, rest)) = tokens.split_first() else {
            return Ok(if self.anchored_end { pos == text.len() } else { true });
        };
        match tok {
            Token::One(atom) => {
                if pos < text.len() && atom_matches(atom, text[pos]) {
                    self.match_tokens(rest, text, pos + 1, budget)
                } else {
                    Ok(false)
                }
            }
            Token::Opt(atom) => {
                // 先试"吃一个"（贪婪），失败再跳过
                if pos < text.len() && atom_matches(atom, text[pos])
                    && self.match_tokens(rest, text, pos + 1, budget)?
                {
                    return Ok(true);
                }
                self.match_tokens(rest, text, pos, budget)
            }
            Token::Star(atom) | Token::Plus(atom) => {
                let min = if matches!(tok, Token::Plus(_)) { 1 } else { 0 };
                // 先一路吃到不能再吃，再逐个回退 —— 经典回溯。
                let mut end = pos;
                while end < text.len() && atom_matches(atom, text[end]) {
                    end += 1;
                    if *budget == 0 {
                        return Err("正则太复杂，回溯超出预算".into());
                    }
                    *budget -= 1;
                }
                loop {
                    if end - pos < min {
                        return Ok(false);
                    }
                    if self.match_tokens(rest, text, end, budget)? {
                        return Ok(true);
                    }
                    if end == pos {
                        return Ok(false);
                    }
                    end -= 1;
                }
            }
        }
    }
}

fn atom_matches(atom: &Atom, c: char) -> bool {
    match atom {
        Atom::Char(x) => *x == c,
        Atom::Any => true, // 简化：`.` 也匹配不到 `\n` 无意义（文件名没有换行）
        Atom::Class { negative, ranges } => {
            let hit = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi);
            hit != *negative
        }
    }
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn parse_class(chars: &[char], open: usize) -> Result<(Atom, usize), String> {
    let mut i = open + 1;
    let negative = chars.get(i) == Some(&'^');
    if negative {
        i += 1;
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Ok((Atom::Class { negative, ranges }, i + 1));
        }
        first = false;
        let lo = if c == '\\' {
            i += 1;
            *chars.get(i).ok_or("字符类里的 \\ 后面缺少字符")?
        } else {
            c
        };
        // `a-z`：中间要有 '-' 且后面还有一个字符（`]` 之前）
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|c| *c != ']') {
            let hi = chars[i + 2];
            ranges.push((lower(lo), lower(hi)));
            i += 3;
        } else {
            ranges.push((lower(lo), lower(lo)));
            i += 1;
        }
    }
    Err("字符类没有闭合：缺一个 `]`".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{Node, INVALID_PARENT, NODE_DIR, NODE_IN_USE};
    use crate::tree::Tree;

    fn tree_with(names: &[(&str, bool, u64)]) -> Tree {
        let mut t = Tree::new("C:", 64);
        t.arena.ensure(names.len() as u32 + 2);
        // 0 是占位（空名字），1 = 根
        let root_name = t.names.intern_str("C:////");
        t.arena.nodes[1] = Node {
            parent: INVALID_PARENT,
            seq: 1,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: root_name,
        };
        for (i, (name, is_dir, size)) in names.iter().enumerate() {
            let idx = (i + 2) as u32;
            let flags = if *is_dir { NODE_IN_USE | NODE_DIR } else { NODE_IN_USE };
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent: 1,
                seq: 1,
                flags,
                size: *size,
                alloc: *size,
                mtime: 1_700_000_000,
                name: off,
            };
        }
        t
    }

    fn names_of(o: &Outcome) -> Vec<&str> {
        o.hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn contains_is_case_insensitive_and_ranks_exact_first() {
        let t = tree_with(&[("report.pdf", false, 10), ("Report.pdf", false, 20), ("my-report.txt", false, 5)]);
        let o = search(&t, &Query { text: "REPORT", mode: MatchMode::Contains, scope: Scope::Files, limit: 10 }).unwrap();
        assert_eq!(names_of(&o), vec!["Report.pdf", "report.pdf", "my-report.txt"]);
        assert_eq!(o.matched, 3);
    }

    #[test]
    fn fuzzy_matches_subsequence_and_prefers_contiguous() {
        let t = tree_with(&[("report.pdf", false, 1), ("r-e-p-o-r-t.txt", false, 1), ("xyz.txt", false, 1)]);
        // `r-e-p-o-r-t` 里 r→p→t 也算子序列，所以两条都该命中 ——
        // 但紧凑、并且从开头起连着的 `report.pdf` 必须排在前面。
        let o = search(&t, &Query { text: "rpt", mode: MatchMode::Fuzzy, scope: Scope::Files, limit: 10 }).unwrap();
        assert_eq!(names_of(&o), vec!["report.pdf", "r-e-p-o-r-t.txt"]);
        assert_eq!(o.matched, 2);
        assert!(!names_of(&o).contains(&"xyz.txt"), "xyz 里没有 r→p→t");
        let o2 = search(&t, &Query { text: "report", mode: MatchMode::Fuzzy, scope: Scope::Files, limit: 10 }).unwrap();
        assert_eq!(o2.hits[0].name, "report.pdf", "连续命中要排在中间夹字符的前面");
    }

    #[test]
    fn scope_filters_dirs_and_files() {
        let t = tree_with(&[("Docs", true, 0), ("docs.txt", false, 7)]);
        let dirs = search(&t, &Query { text: "docs", mode: MatchMode::Contains, scope: Scope::Dirs, limit: 10 }).unwrap();
        assert_eq!(names_of(&dirs), vec!["Docs"]);
        let both = search(&t, &Query { text: "docs", mode: MatchMode::Contains, scope: Scope::Both, limit: 10 }).unwrap();
        assert_eq!(both.matched, 2);
    }

    #[test]
    fn limit_truncates_but_counts_everything() {
        let names: Vec<(String, bool, u64)> =
            (0..30).map(|i| (format!("file{i:02}.log"), false, i)).collect();
        let refs: Vec<(&str, bool, u64)> = names.iter().map(|(n, d, s)| (n.as_str(), *d, *s)).collect();
        let t = tree_with(&refs);
        let o = search(&t, &Query { text: "file", mode: MatchMode::Contains, scope: Scope::Files, limit: 10 }).unwrap();
        assert_eq!(o.matched, 30);
        assert_eq!(o.hits.len(), 10);
        assert!(o.truncated);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let t = tree_with(&[("a.txt", false, 1)]);
        let o = search(&t, &Query { text: "   ", mode: MatchMode::Contains, scope: Scope::Both, limit: 10 }).unwrap();
        assert!(o.hits.is_empty() && o.matched == 0);
    }

    // ── 简化正则 ────────────────────────────────────────────────────────────

    fn m(pat: &str, text: &str) -> bool {
        SimpleRegex::new(pat).unwrap().is_match(text).unwrap()
    }

    #[test]
    fn regex_literals_dot_and_anchors() {
        assert!(m("2024", "报告 2024 汇总.xlsx"));
        assert!(m("^2024", "2024 汇总.xlsx"));
        assert!(!m("^2024", "报告 2024.xlsx"));
        assert!(m(r"\.xlsx$", "汇总.xlsx"));
        assert!(!m(r"\.xlsx$", "汇总.xlsx.bak"));
        assert!(m("a.c", "abc"), ". 匹配任意一个字符");
        assert!(!m("a.c", "ac"));
    }

    #[test]
    fn regex_repetition_and_classes() {
        assert!(m("a*", "bbb"), "* 允许 0 次");
        assert!(m("^ab+c$", "abbbc"));
        assert!(!m("^ab+c$", "ac"), "+ 至少要一次");
        assert!(m("^colou?r$", "color"));
        assert!(m("^colou?r$", "colour"));
    }

    #[test]
    fn regex_rejects_unsupported_syntax_with_a_readable_message() {
        for pat in ["a|b", "(ab)*", "a{2,3}", "*abc"] {
            let e = SimpleRegex::new(pat).unwrap_err();
            assert!(!e.is_empty(), "{pat}");
        }
    }

    #[test]
    fn regex_class_ranges_negation_and_case_insensitivity() {
        assert!(m(r"^[0-9][0-9]-[a-z]+\.txt$", "12-abcd.txt"));
        assert!(m(r"^[^0-9]+$", "abc"), "[^…] 取反");
        assert!(!m(r"^[^0-9]+$", "ab3"));
        assert!(m("REPORT", "report.pdf"), "正则默认大小写不敏感（Windows 文件名语义）");
    }

    #[test]
    fn regex_budget_stops_pathological_patterns() {
        // (a+)+ 我们根本不支持括号，这里用 a*a*a*a*a*a*a*a*a*a*b 制造回溯
        let pat = "a*a*a*a*a*a*a*a*a*a*b";
        let text = "a".repeat(60);
        let e = SimpleRegex::new(pat).unwrap().is_match(&text);
        // 要么正常判 false，要么明确报预算耗尽 —— 都不能卡死
        assert!(matches!(e, Ok(false) | Err(_)), "{e:?}");
    }

    // ── E8 附加条件 ──────────────────────────────────────────────────────

    fn q(text: &str) -> Query<'_> {
        Query { text, mode: MatchMode::Contains, scope: Scope::Files, limit: 10 }
    }

    #[test]
    fn filters_by_size_and_extension() {
        let t = tree_with(&[
            ("a.mp4", false, 10 * 1024 * 1024),
            ("b.mkv", false, 20 * 1024 * 1024),
            ("c.txt", false, 5 * 1024 * 1024),
        ]);
        // 6 MB 的下界挡掉 5 MB 的 c.txt；留下的两个按体积降序（大文件先）
        let big = Filters { min_size: Some(6 * 1024 * 1024), ..Default::default() };
        let o = search_filtered(&t, &q("."), &big).unwrap();
        assert_eq!(names_of(&o), vec!["b.mkv", "a.mp4"]);

        // 扩展名大小写与前导点都要能认（前端传的是输入框原文）
        let exts = Filters::parse_exts("MP4, .mkv;");
        assert_eq!(exts, vec!["mp4", "mkv"]);
        let o2 = search_filtered(&t, &q("."), &Filters { exts, ..Default::default() }).unwrap();
        let mut got = names_of(&o2);
        got.sort();
        assert_eq!(got, vec!["a.mp4", "b.mkv"]);
    }

    #[test]
    fn filters_by_time() {
        let mut t = tree_with(&[("old.log", false, 1), ("new.log", false, 1)]);
        // 两个文件的 mtime 在 helper 里是同一个值，这里手动拉开
        t.arena.nodes[2].mtime = 1_600_000_000;
        t.arena.nodes[3].mtime = 1_700_000_000;
        let o = search_filtered(&t, &q("log"), &Filters { after: Some(1_650_000_000), ..Default::default() }).unwrap();
        assert_eq!(names_of(&o), vec!["new.log"]);
        let o2 = search_filtered(&t, &q("log"), &Filters { before: Some(1_650_000_000), ..Default::default() }).unwrap();
        assert_eq!(names_of(&o2), vec!["old.log"]);
    }

    /// 被条件挡掉的条目**不能占掉 limit 名额**。
    ///
    /// 否则计数会骗人：明明有 3 个同名文件符合体积条件，界面却说"共 1 个" ——
    /// 或者反过来，"只找到 1 个"而其实有 300 个。这类数字对不上最难解释。
    #[test]
    fn filtered_out_items_do_not_consume_the_limit() {
        let t = tree_with(&[("x.bin", false, 1), ("x.bin", false, 2), ("x.bin", false, 1_000_000)]);
        let f = Filters { min_size: Some(1_000_000), ..Default::default() };
        let mut query = q("x");
        query.limit = 1;
        let o = search_filtered(&t, &query, &f).unwrap();
        assert_eq!(o.matched, 1, "只有 1 个真的合格");
        assert!(!o.truncated, "不是被 limit 截断的");
        assert_eq!(o.hits.len(), 1);
    }

    #[test]
    fn empty_filters_behave_like_plain_search() {
        let t = tree_with(&[("a.txt", false, 1), ("b.txt", false, 2)]);
        let a = search(&t, &q("txt")).unwrap();
        let b = search_filtered(&t, &q("txt"), &Filters::default()).unwrap();
        assert_eq!(names_of(&a), names_of(&b));
        assert!(Filters::default().is_empty());
        assert!(!Filters { exts: vec!["txt".into()], ..Default::default() }.is_empty());
    }

}
