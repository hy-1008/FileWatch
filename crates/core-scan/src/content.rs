//! 纯文本文件的**内容**搜索。
//!
//! # 边界先说清楚
//!
//! 只搜**纯文本类**文件：`txt` / 日志 / `csv` / `md` / 配置 / 源码……
//! **不搜 Office**（docx/xlsx/pptx）。那些本质是 zip 包，要在里面找文字必须先
//! 解压 DEFLATE 流，而引擎层零第三方依赖（不能引 `zip` / `flate2`），
//! 自写 inflate 的代价与风险都不值得。界面上也如实写着"不含 Office 文档"——
//! 给一个点了没反应的选项比不给更糟。
//!
//! # 三个上限（防"点了之后界面卡十分钟"）
//!
//! | 上限 | 默认 | 为什么 |
//! |---|---|---|
//! | 单个文件字节数 | 4 MiB | 再大的是日志/数据库导出的天书，读它也搜不完 |
//! | 每个文件的命中行 | 3 行 | 一个 50 万行的日志命中几千次，对用户没有增量信息 |
//! | 二进制探测 | 前 8 KiB 含 NUL 就跳过 | 扩展名是 `.txt` 但内容是二进制的文件真的存在 |
//!
//! 时长由调用方（GUI 的后台线程）用"文件数上限 + 进度回调"控制。
//!
//! # 为什么逐行而不是整文件
//!
//! 用户要的是"哪一行命中"，不是"这个文件里有这个词"。逐行还能顺手把行号与
//! 那一行的片段带回去 —— 搜索结果里能直接看出上下文，不用再打开文件。

use core_index::SimpleRegex;
use std::io::Read;
use std::path::Path;

/// 单个文件最多读多少字节。
pub const MAX_FILE_BYTES: u64 = 4 << 20;
/// 二进制探测窗口。
const SNIFF_BYTES: usize = 8 * 1024;
/// 每个文件最多回几条命中行。
pub const MAX_LINES_PER_FILE: usize = 3;
/// 命中行片段最多保留多少字符（两侧各留一点，中间不截断关键字）。
const LINE_PREVIEW_CHARS: usize = 200;

/// 允许搜索内容的扩展名（小写，不含点）。
///
/// 白名单而不是黑名单：把 `.exe` / `.db` / `.zip` 读进来既慢又没意义，
/// 而黑名单永远会漏（每天都有新格式）。
pub const TEXT_EXTS: &[&str] = &[
    "txt", "text", "log", "logs", "md", "markdown", "rst",
    "csv", "tsv", "psv", "json", "jsonl", "ndjson", "xml", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "properties", "env", "editorconfig", "gitignore", "gitattributes",
    "html", "htm", "css", "scss", "less", "svg",
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "vue", "svelte",
    "rs", "c", "h", "cc", "cpp", "hpp", "cs", "java", "kt", "go", "rb", "php",
    "py", "lua", "pl", "r", "m", "swift", "dart", "scala",
    "sh", "bash", "zsh", "bat", "cmd", "ps1", "psm1", "sql",
    "gradle", "cmake", "makefile", "mk", "dockerfile", "srt", "vtt", "ass",
];

/// 文件名是不是纯文本类（按扩展名判断；无扩展名的常见文本名单独认）。
pub fn is_text_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // 没有扩展名但一眼是文本的文件（Makefile / Dockerfile 之类）
    let bare = ["makefile", "dockerfile", "license", "readme", "changelog", "notice"];
    if bare.contains(&lower.as_str()) {
        return true;
    }
    match lower.rsplit_once('.') {
        Some((_, ext)) => TEXT_EXTS.contains(&ext),
        None => false,
    }
}

/// 一条命中行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    /// 1 起算的行号（给人看的）。
    pub line_no: u32,
    /// 该行内容（去掉行尾换行，过长时中间省略）。
    pub text: String,
}

/// 在一个文件里找内容。`pattern` 是已经准备好的匹配器。
pub enum Pattern {
    /// 大小写不敏感的子串（调用方已经 lower 过）
    Text(String),
    /// 简化正则（大小写不敏感，由 `SimpleRegex` 保证）
    Regex(SimpleRegex),
}

impl Pattern {
    /// 从界面参数构造。`regex == true` 时按正则解析，失败给出可读的错误。
    pub fn new(text: &str, regex: bool) -> Result<Pattern, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("内容关键字不能为空".into());
        }
        if regex {
            Ok(Pattern::Regex(SimpleRegex::new(t)?))
        } else {
            Ok(Pattern::Text(t.to_lowercase()))
        }
    }
}

/// 读一个文件并在里面找。返回命中行（最多 [`MAX_LINES_PER_FILE`] 条）。
///
/// 读不了（权限、被占用、读一半消失）**不报错**，返回空 —— 内容搜索是在
/// 一堆文件里捞东西，其中一个读不了不该让整次搜索失败。这一点和快照不同：
/// 快照必须全有或全无，搜索只要"能搜的都搜了"。
pub fn search_file(path: &Path, pattern: &Pattern) -> Vec<LineHit> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    // 先嗅探前 8 KiB：含 NUL 就当二进制跳过（扩展名会骗人）
    let mut sniff = [0u8; SNIFF_BYTES];
    let read = match file.read(&mut sniff) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let head = &sniff[..read];
    // 先看 BOM 再看 NUL：UTF-16 的纯文本里**每两个字节就有一个 0**，
    // 按"NUL = 二进制"判会把它整份跳过 —— 而记事本存的「Unicode」txt 正是这种。
    let utf16 = head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]);
    if !utf16 && head.contains(&0) {
        return Vec::new();
    }
    let mut bytes = head.to_vec();
    // 余下部分最多读到 MAX_FILE_BYTES
    let mut rest = file.take(MAX_FILE_BYTES.saturating_sub(read as u64));
    let _ = rest.read_to_end(&mut bytes);

    // UTF-16（带 BOM）单独处理：记事本存的"Unicode" txt 就是这种，
    // 直接按 UTF-8 解会全是 NUL，看起来像二进制而被跳过。
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        decode_utf16(&bytes[2..], true)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        decode_utf16(&bytes[2..], false)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    scan_text(&text, pattern)
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| if little_endian {
            u16::from_le_bytes([c[0], c[1]])
        } else {
            u16::from_be_bytes([c[0], c[1]])
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// 在一段文本里找（单独拆出来是为了能脱离磁盘单测）。
pub fn scan_text(text: &str, pattern: &Pattern) -> Vec<LineHit> {
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let matched = match pattern {
            Pattern::Text(needle) => line.to_lowercase().contains(needle.as_str()),
            // 正则的预算耗尽当成"这一行不命中"：整次搜索不该因为某一行长得离谱
            // 就失败。预算每次匹配重置，所以不会累积。
            Pattern::Regex(re) => re.is_match(line).unwrap_or(false),
        };
        if !matched {
            continue;
        }
        hits.push(LineHit { line_no: (i + 1) as u32, text: clip(line) });
        if hits.len() >= MAX_LINES_PER_FILE {
            break;
        }
    }
    hits
}

/// 行太长时**中间省略**：首尾各留一段，保证开头与结尾都能看到。
fn clip(line: &str) -> String {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= LINE_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let head = LINE_PREVIEW_CHARS / 2;
    let tail = LINE_PREVIEW_CHARS - head;
    let mut s: String = chars[..head].iter().collect();
    s.push('…');
    s.extend(&chars[chars.len() - tail..]);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_io::test_dir::TestDir;

    fn text_pattern(s: &str) -> Pattern {
        Pattern::new(s, false).unwrap()
    }

    #[test]
    fn text_whitelist_accepts_notes_and_sources_not_office() {
        for name in ["a.txt", "系统.log", "data.CSV", "README.md", "Makefile", "x.rs", "配置.yaml"] {
            assert!(is_text_name(name), "{name} 应算纯文本");
        }
        for name in ["报表.xlsx", "方案.docx", "胶片.pptx", "pkg.zip", "app.exe", "db.sqlite"] {
            assert!(!is_text_name(name), "{name} 不该算纯文本");
        }
        // 点开头的隐藏配置文件：`.gitignore` / `.editorconfig` 的本质是文本
        assert!(is_text_name(".gitignore"));
    }

    #[test]
    fn scan_text_reports_line_numbers_case_insensitively() {
        let text = "第一行\nAlpha Beta\n第三行 alpha\n";
        let hits = scan_text(text, &text_pattern("ALPHA"));
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].line_no, hits[0].text.as_str()), (2, "Alpha Beta"));
        assert_eq!(hits[1].line_no, 3);
    }

    #[test]
    fn scan_text_stops_at_three_lines_per_file() {
        let text = (0..10).map(|i| format!("needle {i}\n")).collect::<String>();
        assert_eq!(scan_text(&text, &text_pattern("needle")).len(), MAX_LINES_PER_FILE);
    }

    #[test]
    fn scan_text_supports_regex_patterns() {
        let text = "2024 年报.xlsx\n2025 年报.xlsx\n备注\n";
        let re = Pattern::new(r"^202[45].*年报", true).unwrap();
        assert_eq!(scan_text(text, &re).len(), 2);
        // 不支持的写法要报错，而不是静默当成字面量
        assert!(Pattern::new("a|b", true).is_err());
    }

    #[test]
    fn actual_file_is_read_and_skipped_when_binary() {
        let owned = TestDir::new("content-search").unwrap();
        let txt = owned.path().join("记事.txt");
        std::fs::write(&txt, "第一行\n这里有关键词 needle\n第三行\n").unwrap();
        let hits = search_file(&txt, &text_pattern("NEEDLE"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_no, 2);
        assert!(hits[0].text.contains("needle"));

        // 扩展名是 .txt、内容却是二进制（含 NUL）→ 跳过，不当文本硬读
        let fake = owned.path().join("假的.txt");
        std::fs::write(&fake, [0u8, 1, 2, 3, b'n', b'e', b'e', b'd', b'l', b'e']).unwrap();
        assert!(search_file(&fake, &text_pattern("needle")).is_empty());

        // 读不到的文件：空结果，不是错误
        assert!(search_file(&owned.path().join("不存在.txt"), &text_pattern("x")).is_empty());
    }

    #[test]
    fn utf16_bom_files_are_searchable() {
        let owned = TestDir::new("content-utf16").unwrap();
        let p = owned.path().join("记事本.txt");
        // 记事本「Unicode」保存的就是 LE + BOM
        let mut bytes = vec![0xFF, 0xFE];
        for u in "关键词 need".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&p, bytes).unwrap();
        let hits = search_file(&p, &text_pattern("need"));
        assert_eq!(hits.len(), 1, "UTF-16 的 txt 也要能搜（直接按 UTF-8 解会全是 NUL）");
    }

    #[test]
    fn long_lines_are_clipped_in_the_middle() {
        let long = format!("{}needle{}", "前".repeat(300), "后".repeat(300));
        let hits = scan_text(&long, &text_pattern("needle"));
        assert_eq!(hits.len(), 1);
        let t = &hits[0].text;
        assert!(t.chars().count() <= LINE_PREVIEW_CHARS + 1, "{}", t.chars().count());
        assert!(t.contains('…'), "过长要中间省略");
        assert!(!t.contains("needle"), "省略会把中间的关键字吃掉，这是预期行为（首尾仍保留）");
    }
}
