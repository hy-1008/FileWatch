//! 敏感文件盘点（E5）：按**文件名与扩展名**找出"可能不该躺在这台机器上"的东西。
//!
//! 这不是杀毒软件，也不读文件内容 —— 它只是把索引里已经有的路径过一遍规则表。
//! 所以：
//!
//! * **零 IO**：不打开任何文件，200 万条索引扫一遍是毫秒级；
//! * **必然有假阳性**：一个叫 `key.pem` 的文件可能真是证书，也可能是测试夹具。
//!   所以每条命中都带「为什么要看它」，由人决定要不要管；
//! * **不判断内容**：文件里真有私钥还是只有一行 `TODO`，这里一概不知道，
//!   界面上也不能暗示知道。
//!
//! 规则表是静态的、可枚举的 —— 用户能看见"它在找什么"，才不会把"没报警"
//! 误解成"没有风险"。

use crate::arena::NODE_IN_USE;
use crate::tree::Tree;
use core_io::paths::file_name;

/// 命中的严重程度。只用于排序与配色，不是"风险评分"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 私钥、密码库、钱包：拿到即等于拿到凭据
    High,
    /// 凭据类配置、证件材料：通常仍需要额外条件才能利用
    Medium,
    /// 备份/导出等"看一眼再判断"的
    Low,
}

impl Severity {
    pub fn label_cn(self) -> &'static str {
        match self {
            Severity::High => "高",
            Severity::Medium => "中",
            Severity::Low => "低",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

/// 一条匹配规则。
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    /// 归类名（界面上的分组标题）
    pub category: &'static str,
    pub severity: Severity,
    /// 为什么值得看一眼（一句话，给非安全专业的人看）
    pub why: &'static str,
    /// 文件名**完全相等**（小写，含点）
    pub names: &'static [&'static str],
    /// 文件名**前缀**：`.env` 要能命中 `.env.local`
    pub name_prefixes: &'static [&'static str],
    /// 文件名**包含**：中文证件名往往是 `身份证正反面.jpg` 这种
    pub name_contains: &'static [&'static str],
    /// 扩展名（小写，不含点）
    pub exts: &'static [&'static str],
}

/// 规则表。顺序即匹配优先级（越靠前越具体）。
///
/// 新增规则的两个要求：
/// 1. `why` 要写"为什么"，不能只写"敏感" —— 用户看不懂的分类等于噪声；
/// 2. 宁可少一条，不要加 `.bak` / `.sql` 这类满地都是的扩展名：
///    一屏全是假阳性之后，真的一条也会被划过去。
pub const RULES: &[Rule] = &[
    Rule {
        category: "SSH / 代码签名私钥",
        severity: Severity::High,
        why: "私钥文件。拿到它可以冒充你登录服务器或签名发布物",
        names: &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "secring.gpg"],
        name_prefixes: &[],
        name_contains: &[],
        exts: &["pem", "key", "ppk", "p12", "pfx", "jks", "keystore", "p8"],
    },
    Rule {
        category: "密码库 / 加密容器",
        severity: Severity::High,
        why: "密码管理器或加密卷。里面通常是「一套全部账号」",
        names: &[],
        name_prefixes: &[],
        name_contains: &[],
        exts: &["kdbx", "kdb", "1pif", "opvault", "psafe3", "keychain", "keychain-db"],
    },
    Rule {
        category: "钱包文件",
        severity: Severity::High,
        why: "加密货币钱包。文件本身就是资产，复制走就能花",
        names: &["wallet.dat"],
        name_prefixes: &[],
        name_contains: &[],
        exts: &["wallet"],
    },
    Rule {
        category: "云与代码托管凭据",
        severity: Severity::High,
        why: "里面常带长期有效的访问令牌（git / 云厂商 / 包仓库）",
        names: &[
            ".git-credentials",
            ".netrc",
            "credentials",
            "credentials.json",
            "serviceaccount.json",
            "service-account.json",
        ],
        name_prefixes: &[],
        name_contains: &[],
        exts: &[],
    },
    Rule {
        category: "环境变量 / 密钥配置",
        severity: Severity::Medium,
        why: "项目里放密钥的常见位置。提交进仓库或被同步走就等于泄露",
        names: &[".env", "secrets.yml", "secrets.yaml", "secret.json"],
        name_prefixes: &[".env"],
        name_contains: &[],
        exts: &["env"],
    },
    Rule {
        category: "包管理器令牌",
        severity: Severity::Medium,
        why: "npm / PyPI 的发布令牌常直接写在这些文件里",
        names: &[".npmrc", ".pypirc", ".yarnrc.yml"],
        name_prefixes: &[],
        name_contains: &[],
        exts: &[],
    },
    Rule {
        category: "证件与隐私材料",
        severity: Severity::Medium,
        why: "身份证/护照/户口的扫描件。这类文件泄露的后果是长期的",
        names: &[],
        name_prefixes: &[],
        name_contains: &["身份证", "护照", "户口", "驾驶证", "行驶证", "社保卡", "银行卡"],
        exts: &[],
    },
];

/// 一条命中。
///
/// 类型名用 `Finding` 而不是 `Hit`：`core_index::search` 里已经有一个 `Hit`
/// （搜索命中），两者字段完全不同。同名会让静态检查器（`tools/check_api.py`）
/// 的符号表互相覆盖，然后报出一堆"未声明字段"的假错 —— 名字撞了就是撞了，
/// 换个更贴切的名字比加别名干净。
#[derive(Debug, Clone)]
pub struct Finding {
    /// 节点下标（= MFT 记录号），路径由调用方用 `Tree::path_of` + `PathRemap` 拼
    pub node: u32,
    /// 命中的规则（`&'static`，因为规则表是常量）
    pub rule: &'static Rule,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
}

impl Finding {
    pub fn severity(&self) -> Severity {
        self.rule.severity
    }
}

/// 盘点选项。
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// 最多返回多少条（0 = 不限）。默认 500：再多人也看不过来，
    /// 而且界面一次塞几千行会卡。
    pub max: usize,
    /// 小于这个体积的文件不算（默认 0：0 字节的 `.env` 也是线索）
    pub min_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max: 500,
            min_size: 0,
        }
    }
}

/// 盘点结果。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub hits: Vec<Finding>,
    /// 扫过的文件数（"在这 120 万个文件里找到 N 条"比只报 N 有用）
    pub files: u64,
    /// 命中数超过 `max` 被截断了
    pub truncated: bool,
}

/// 单条路径是否命中规则。
///
/// 只看文件名与扩展名，不看目录 —— 目录名匹配会让 `D:\证件\` 下所有文件都报警，
/// 那反而把真正的重点淹掉。
pub fn classify(path: &str) -> Option<&'static Rule> {
    let name = file_name(path);
    if name.is_empty() {
        return None;
    }
    let mut low = String::with_capacity(name.len());
    low.push_str(name);
    low.make_ascii_lowercase();
    let ext = crate::tree::extension_of(&low);
    hit_rule(&low, ext)
}

fn hit_rule(name_low: &str, ext_low: &str) -> Option<&'static Rule> {
    for r in RULES {
        if r.names.iter().any(|n| *n == name_low) {
            return Some(r);
        }
        if r.name_prefixes.iter().any(|p| name_low.starts_with(p)) {
            return Some(r);
        }
        if r.name_contains.iter().any(|c| name_low.contains(c)) {
            return Some(r);
        }
        if !ext_low.is_empty() && r.exts.iter().any(|e| *e == ext_low) {
            return Some(r);
        }
    }
    None
}

/// 在整个索引里盘点。**零 IO**：只读 `Tree` 与名字池。
pub fn scan(tree: &Tree, opts: &Options) -> Report {
    let mut rep = Report::default();
    let mut low = String::new();
    for (i, node) in tree.arena.nodes.iter().enumerate() {
        if node.flags & NODE_IN_USE == 0 || node.is_dir() {
            continue;
        }
        rep.files += 1;
        if node.size < opts.min_size {
            continue;
        }
        let name = tree.name(i as u32);
        if name.is_empty() {
            continue;
        }
        low.clear();
        low.push_str(name);
        low.make_ascii_lowercase();
        let ext = crate::tree::extension_of(&low);
        if let Some(rule) = hit_rule(&low, ext) {
            rep.hits.push(Finding {
                node: i as u32,
                rule,
                size: node.size,
                alloc: node.alloc,
                mtime: node.mtime,
            });
        }
    }
    // 先按严重程度，再按体积：体积大的通常更值得先处理（也更可能是真的）。
    rep.hits.sort_by(|a, b| {
        a.rule
            .severity
            .cmp(&b.rule.severity)
            .then_with(|| b.alloc.cmp(&a.alloc))
            .then_with(|| a.node.cmp(&b.node))
    });
    if opts.max > 0 && rep.hits.len() > opts.max {
        rep.hits.truncate(opts.max);
        rep.truncated = true;
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::aggregate;
    use crate::tree::Tree;
    use core_ntfs::filename::make_frn;
    use core_ntfs::record::MftRecord;

    fn mk(recno: u64, parent_frn: u64, name: &str, is_dir: bool, size: u64) -> (MftRecord, Vec<u8>) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut buf = vec![0u8; 0x30 + 0x42 + units.len() * 2];
        let off = 0x30 + 0x42;
        for (i, u) in units.iter().enumerate() {
            buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        (
            MftRecord {
                frn: make_frn(recno, 1),
                recno,
                seq: 1,
                flags: 1 | if is_dir { 2 } else { 0 },
                hard_link_count: 1,
                parent_frn,
                namespace: 1,
                name_off: off as u32,
                name_chars: units.len() as u16,
                real_size: size,
                alloc_size: size,
                mtime: 1_700_000_000,
                fn_flags: 0,
                has_attr_list: false,
                resident_data: false,
                has_named_stream: false,
            },
            buf,
        )
    }

    #[test]
    fn classify_catches_the_usual_suspects() {
        let cases = [
            (r"C:\Users\HY\.ssh\id_rsa", "SSH / 代码签名私钥"),
            (r"C:\work\server.pem", "SSH / 代码签名私钥"),
            (r"C:\Users\HY\.env", "环境变量 / 密钥配置"),
            (r"C:\work\.env.local", "环境变量 / 密钥配置"),
            (r"C:\work\prod.env", "环境变量 / 密钥配置"),
            (r"C:\Users\HY\.npmrc", "包管理器令牌"),
            (r"C:\Users\HY\.git-credentials", "云与代码托管凭据"),
            (r"D:\backup\my.kdbx", "密码库 / 加密容器"),
            (r"D:\wallet\wallet.dat", "钱包文件"),
            (r"D:\照片\身份证正反面.jpg", "证件与隐私材料"),
        ];
        for (path, want) in cases {
            let r = classify(path).unwrap_or_else(|| panic!("没命中：{path}"));
            assert_eq!(r.category, want, "{path}");
        }
    }

    #[test]
    fn classify_ignores_ordinary_files() {
        for path in [
            r"C:\Windows\notepad.exe",
            r"C:\work\README.md",
            r"C:\照片\IMG_0001.JPG",
            r"C:\work\main.rs",
            r"C:\work\keyboard.txt", // 含 key 但不是 .key 扩展名
        ] {
            assert!(classify(path).is_none(), "不该命中：{path}");
        }
    }

    #[test]
    fn classify_is_case_insensitive_and_handles_both_separators() {
        assert!(classify("C:/Users/HY/.SSH/ID_RSA").is_some());
        assert!(classify(r"D:\a\Secret.KDBX").is_some());
        assert!(classify("D:/a/setup.P12").is_some());
    }

    /// root(5) ─┬─ logs(10) ── a.txt(11)
    ///          ├─ id_rsa(12)
    ///          └─ .env(13)
    fn sample() -> Tree {
        let mut t = Tree::new("C:", 64);
        let root = make_frn(5, 1);
        for (recno, parent, name, is_dir, size) in [
            (5u64, root, ".", true, 0),
            (10, root, "logs", true, 0),
            (11, make_frn(10, 1), "a.txt", false, 10),
            (12, root, "id_rsa", false, 2000),
            (13, root, ".env", false, 30),
        ] {
            let (r, b) = mk(recno, parent, name, is_dir, size);
            t.insert(&r, &b);
        }
        aggregate(&mut t);
        t
    }

    #[test]
    fn scan_only_reports_rule_hits_and_counts_scanned_files() {
        let t = sample();
        let rep = scan(&t, &Options::default());
        assert_eq!(rep.files, 3, "只数文件，不数目录");
        assert_eq!(rep.hits.len(), 2);
        assert!(!rep.truncated);
        // 私钥（高）排在 .env（中）前面
        assert_eq!(rep.hits[0].rule.category, "SSH / 代码签名私钥");
        assert_eq!(rep.hits[1].rule.category, "环境变量 / 密钥配置");
    }

    #[test]
    fn scan_respects_max_and_min_size() {
        let t = sample();
        let rep = scan(
            &t,
            &Options {
                max: 1,
                min_size: 0,
            },
        );
        assert_eq!(rep.hits.len(), 1);
        assert!(rep.truncated, "被截断必须说出来");

        let rep = scan(
            &t,
            &Options {
                max: 0,
                min_size: 1000,
            },
        );
        assert_eq!(rep.hits.len(), 1, "只有 id_rsa 超过 1000 字节");
        assert_eq!(rep.hits[0].rule.category, "SSH / 代码签名私钥");
    }
}
