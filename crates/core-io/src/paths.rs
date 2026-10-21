//! 路径口径：**分隔符与「根前缀」的唯一实现**。
//!
//! # 为什么单独一个模块
//!
//! 引擎里凡是拼路径的地方，都必须回答三个问题：
//!
//! 1. 分隔符是什么？（Windows `\`，Linux `/`）
//! 2. 一个"卷键"（`C:` / `/` / `/home`）怎么变成带尾分隔符的根前缀？
//! 3. 尾分隔符什么时候留、什么时候去？
//!
//! 这三件事原来分散在 `Tree::path_of`、`paths::join_path`、`dirwatch::normalize_root`、
//! `session::norm_lookup`、`PathRemap::new` 五处，各自手写 `'\\'`。
//! 移植到 Linux 时如果逐处替换，必然出现「树里是 `/`、索引键里是 `\`」这种
//! **查不到又不会报错**的错位 —— 路径比较失败只会表现为「事件未解析」。
//!
//! 所以统一到这里：Windows 上行为与原来**逐字节一致**（`'\\'`），
//! Linux 上换成 `'/'`，调用方一行都不用改。
//!
//! # 「卷键」与「根前缀」
//!
//! | 平台 | 卷键 | 根前缀 |
//! |---|---|---|
//! | Windows | `C:` | `C:\` |
//! | Linux | `/` | `/` |
//! | Linux | `/home` | `/home/` |
//!
//! 注意 Linux 的 `/` **不能**再补一个 `/` 变成 `//`（POSIX 里 `//` 有实现自定义
//! 含义，且拼出来是 `//a.txt`，与用户看到的路径不一致）。

use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR};

/// 本平台的主路径分隔符。
pub const SEP: char = MAIN_SEPARATOR;

/// 本平台的主路径分隔符（`&str` 形态，供 `join` / `format!` 用）。
pub const SEP_STR: &str = MAIN_SEPARATOR_STR;

/// 默认的卷键：Windows `C:`，类 Unix `/`。
///
/// 只用于 CLI 的缺省参数与 [`Default`] 实现；Linux 上用户通常会显式给出
/// 挂载点（`filewatch scan /home`）。
pub const DEFAULT_VOLUME: &str = if cfg!(windows) { "C:" } else { "/" };

/// 去掉尾部的分隔符（可能多个）。**可能返回空串**，调用方自己决定怎么兜底。
///
/// Windows 上 `\\` 与 `/` **都算分隔符**：命令行写的 `--root=C:/a/b/` 与
/// `Tree::path_of` 拼出来的反斜杠路径必须能对齐（否则归因/去重这类按前缀
/// 切分的逻辑会静默地一条都切不出来）。Linux 上 `\\` 是合法文件名字符，
/// 不能当分隔符。
pub fn trim_sep(s: &str) -> &str {
    #[cfg(windows)]
    {
        s.trim_end_matches(&['\\', '/'][..])
    }
    #[cfg(not(windows))]
    {
        s.trim_end_matches(SEP)
    }
}

/// 卷键 → 带尾分隔符的根前缀。
///
/// * `"C:"` → `"C:\"`（Windows 的盘根语义要求带尾杠）
/// * `"/"` → `"/"`（**不**变成 `"//"`）
/// * `"/home"` → `"/home/"`
pub fn root_prefix(key: &str) -> String {
    let t = trim_sep(key);
    if t.is_empty() {
        // `""` 与 `"/"` 都归一到根
        return SEP_STR.to_string();
    }
    let mut s = String::with_capacity(t.len() + 1);
    s.push_str(t);
    s.push(SEP);
    s
}

/// 目录 + 名字 → 路径。根前缀已带尾分隔符时不再补。
///
/// `Tree::path_of` 的根恒为带尾分隔符的形态，所以这里是「不会拼出双分隔符」的
/// 唯一保证点。
pub fn join(dir: &str, name: &str) -> String {
    if name.is_empty() {
        return dir.to_string();
    }
    if dir.is_empty() {
        return name.to_string();
    }
    let mut s = String::with_capacity(dir.len() + 1 + name.len());
    s.push_str(dir);
    if !dir.ends_with(SEP) {
        s.push(SEP);
    }
    s.push_str(name);
    s
}

/// 监视根的归一形态：**卷根保留尾分隔符，其余一律去掉**。
///
/// 这是 `DirWatcher` 与路径索引的共同约定：归一之后 `根 + 分隔符 + 相对路径`
/// 的拼接只需要一个分支。
pub fn normalize_root(s: &str) -> String {
    let t = trim_sep(s);
    if t.is_empty() {
        return SEP_STR.to_string();
    }
    // 卷根：Windows `C:` / Linux 的 `/` 已经被 trim 成空串，走上面的分支；
    // 这里剩下的 `C:`（盘符）要补回尾杠，其余目录直接返回。
    if cfg!(windows) && t.len() == 2 && t.ends_with(':') {
        let mut s = t.to_string();
        s.push(SEP);
        return s;
    }
    t.to_string()
}

/// 路径查询键：**去尾分隔符 + 小写**（卷根不能削成空串）。
///
/// 两个来源必须归一到同一个键，否则「以卷根为扫描根时查 `C:\` 会返回 None」：
/// 用户输入可能写成 `C:\`、`C:\Users\x\`、`C:`；而 `Tree::path_of` 的卷根
/// 返回的是带分隔符的形态。
///
/// 比较时不做大小写折叠在 Linux 上是**无害**的（大小写不同的文件本来就少），
/// 但保留它让两个平台的索引键形状一致，也让换平台的测试不必改断言。
pub fn lookup_key(s: &str) -> String {
    let t = trim_sep(s);
    if t.is_empty() {
        SEP_STR.to_string()
    } else {
        t.to_lowercase()
    }
}

/// 「这个键是不是卷根」——用于区分 `C:` / `/` 这类没有父目录的根。
pub fn is_volume_root(key: &str) -> bool {
    let t = trim_sep(key);
    if t.is_empty() {
        return true; // Linux 的 `/`
    }
    cfg!(windows) && t.len() == 2 && t.ends_with(':')
}

/// 从完整路径取最后一段（文件名 / 目录名）。
///
/// **两种分隔符都认**：快照与 CSV 是纯文本，用户可能手工编辑过；
/// 而且 Windows 对路径里的 `/` 是宽容的（`C:/Users` 与 `C:\Users` 等价）。
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// 剥掉 `root` 前缀，返回**相对部分**（不含开头分隔符）。
///
/// 三个必须处理对的细节，任何一个错了都只会表现为「归因榜里多出一堆奇怪的目录」，
/// 不会报错：
///
/// 1. **分量边界**：`C:\Users\HY` 不能匹配 `C:\Users\HY2\a.txt`；
/// 2. **卷根形态**：`root` 可能带尾分隔符（`C:\`、`/home/`），也可能不带；
///    Linux 的 `/` 去掉尾分隔符会变成空串，必须单独认；
/// 3. **大小写**：Windows 上按 ASCII 忽略大小写（用户手工把 `C:\users` 写成小写
///    也该能对上），Linux 上严格 —— 那边大小写不同就是两个不同的文件。
pub fn strip_root_prefix<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if root.is_empty() || path.is_empty() {
        return None;
    }
    // 文件系统根（Linux `/`）：去掉它自己就是相对部分。
    if root == SEP_STR {
        return path.strip_prefix(SEP);
    }
    let r = trim_sep(root);
    if r.is_empty() {
        return None;
    }
    let head = path.get(..r.len())?;
    // 比较必须**容忍分隔符混用**：扫描根可能来自命令行（`C:/a/b`），而
    // `Tree::path_of` 拼出来的路径是 `C://a//b`。原来这里按字节比，结果
    // 「根」与「每一条路径」一个字符都不等 —— 归因的目录榜整列空掉，
    // 而且不报错（对比表、CSV 全都正常，只有归因没了）。
    let hit = head
        .as_bytes()
        .iter()
        .zip(r.as_bytes())
        .all(|(a, b)| same_char(*a, *b));
    if !hit {
        return None;
    }
    let rest = &path[r.len()..];
    if rest.is_empty() {
        return Some("");
    }
    // 分量边界：剩下这部分必须从分隔符开始，否则 `HY2` 会被当成 `HY` 的子项。
    let c = rest.chars().next()?;
    if c == SEP || (cfg!(windows) && c == '/') {
        Some(&rest[c.len_utf8()..])
    } else {
        None
    }
}

/// 两个字节是否表示同一个字符（分隔符等价 + 平台大小写规则）。
#[inline]
fn same_char(a: u8, b: u8) -> bool {
    #[cfg(windows)]
    {
        let norm = |c: u8| if c == b'/' { b'\\' } else { c };
        norm(a).eq_ignore_ascii_case(&norm(b))
    }
    #[cfg(not(windows))]
    {
        // Linux 上 `\\` 是合法的文件名字符，**不能**当成分隔符
        a == b
    }
}

/// 把路径里的分隔符统一成本平台的主分隔符（Windows 把 `/` 换成 `\\`）。
///
/// 用途只有一个：**让"同一个路径的两种写法"在比较与显示前对齐**。
/// 典型来源是命令行参数 `--root=C:/a/b` 与 `Tree::path_of` 拼出的 `C://a//b`。
pub fn to_native(s: &str) -> String {
    #[cfg(windows)]
    {
        s.replace('/', SEP_STR)
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_never_doubles_the_separator() {
        let root = root_prefix(DEFAULT_VOLUME);
        let p = join(&root, "a.txt");
        assert!(!p.contains(&format!("{}{}", SEP, SEP)), "拼出了双分隔符：{p}");
        assert!(p.ends_with("a.txt"));
        // 空名字 / 空目录都不能 panic
        assert_eq!(join(&root, ""), root);
        assert_eq!(join("", "x"), "x");
    }

    #[test]
    fn root_prefix_shape() {
        let r = root_prefix(DEFAULT_VOLUME);
        assert!(r.ends_with(SEP), "根前缀必须带尾分隔符：{r}");
        assert_eq!(root_prefix(""), SEP_STR.to_string());
        // 已经是根的不再叠加
        assert_eq!(root_prefix(&r), r);
        // 去冗余尾分隔符
        let doubled = format!("{r}{SEP}");
        assert_eq!(root_prefix(&doubled), r);
    }

    #[test]
    fn normalize_root_keeps_volume_root_only() {
        let r = root_prefix(DEFAULT_VOLUME);
        assert_eq!(normalize_root(&r), r.trim_end_matches(SEP).to_string() + SEP_STR);
        // 普通目录去掉尾分隔符
        let sub = join(&r, "sub");
        assert_eq!(normalize_root(&format!("{sub}{SEP}")), sub);
        assert_eq!(normalize_root(&sub), sub);
        assert_eq!(normalize_root(""), SEP_STR.to_string());
    }

    /// `lookup_key` 的回退值不能是空串 —— 空串是所有路径的前缀。
    #[test]
    fn lookup_key_never_returns_empty() {
        assert_eq!(lookup_key(""), SEP_STR.to_string());
        assert_eq!(lookup_key(SEP_STR), SEP_STR.to_string());
        assert!(!lookup_key("").is_empty());
        // 大小写折叠 + 去尾分隔符
        let k = lookup_key(&join(&root_prefix(DEFAULT_VOLUME), "Users"));
        assert!(!k.ends_with(SEP), "尾部有分隔符时要去掉：{k}");
    }

    #[test]
    fn volume_root_detection() {
        assert!(is_volume_root(SEP_STR));
        assert!(!is_volume_root(&join(&root_prefix(DEFAULT_VOLUME), "x")));
    }

    #[test]
    fn file_name_takes_the_last_segment() {
        assert_eq!(file_name("a/b.txt"), "b.txt");
        assert_eq!(file_name("a\\b.txt"), "b.txt");
        assert_eq!(file_name("b.txt"), "b.txt");
        // 只写文件名时原样返回（rsplit 至少给一段）
        assert_eq!(file_name(""), "");
        // 以分隔符结尾的目录路径没有"文件名"这一段 —— 这里如实返回空串，
        // 调用方（比如扩展名统计）要自己判空，不能指望它替你回退到上一级。
        assert_eq!(file_name("a/b/"), "");
    }

    #[test]
    fn strip_root_prefix_respects_component_boundaries() {
        let root = root_prefix(DEFAULT_VOLUME);
        let users = join(&root, "Users");
        assert_eq!(strip_root_prefix(&users, &root), Some("Users"));
        // 目录本身剥完是空串，不是 None
        assert_eq!(strip_root_prefix(&users, &users), Some(""));

        // 关键：`Users\HY` 不能匹配 `Users\HY2`
        let hy = join(&users, "HY");
        let hy2 = format!("{hy}2");
        assert_eq!(strip_root_prefix(&hy2, &hy), None);
        assert_eq!(strip_root_prefix(&join(&hy, "a.txt"), &hy), Some("a.txt"));
    }

    #[test]
    fn strip_root_prefix_accepts_both_shapes_of_root() {
        // Windows 的 `C:` / `C:\`、Linux 的 `/home` / `/home/` 都要认
        let root = root_prefix(DEFAULT_VOLUME);
        let p = join(&root, "x");
        assert!(strip_root_prefix(&p, &root).is_some(), "带尾分隔符的根");
        let no_tail = trim_sep(&root);
        if !no_tail.is_empty() {
            assert_eq!(strip_root_prefix(&p, no_tail), Some("x"));
        }
        // 空根/空路径：没有"前缀"可言，一律 None 而不是 Some(全部)
        assert_eq!(strip_root_prefix(&p, ""), None);
        assert_eq!(strip_root_prefix("", &root), None);
    }

    /// 根与路径的分隔符混用**必须**能对上：命令行 `--root=C:/a/b` 建的快照，
    /// 里面每条路径却是 `C://a//b`。按字节比会一条都切不出来 —— 归因的
    /// 目录榜整列空掉，而且不报错。
    #[cfg(windows)]
    #[test]
    fn strip_root_prefix_tolerates_mixed_separators() {
        // Rust 源码里的 "\\" 是一个反斜杠；这里用 chr 拼是为了不被
        // 上层工具的转义规则改写（踩过两次）。
        assert_eq!(strip_root_prefix("C:\\a\\b\\c.txt", "C:/a/b"), Some("c.txt"));
        assert_eq!(strip_root_prefix("C:\\a\\b\\c.txt", "C:\\a/b"), Some("c.txt"));
        assert_eq!(strip_root_prefix("C:\\a\\b", "C:/a/b/"), Some(""));
        assert_eq!(to_native("C:/a/b"), "C:\\a\\b");
    }

    #[cfg(windows)]
    #[test]
    fn strip_root_prefix_is_case_insensitive_on_windows() {
        assert_eq!(strip_root_prefix("c:\\users\\a.txt", "C:\\"), Some("users\\a.txt"));
        assert_eq!(strip_root_prefix("C:\\Users\\A.TXT", "c:\\users"), Some("A.TXT"));
    }
}
