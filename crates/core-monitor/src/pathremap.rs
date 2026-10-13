//! 合成路径 → 真实路径的重映射。
//!
//! # 为什么需要
//!
//! `$MFT` 直读是按**整卷**做的，索引树里每条路径的前缀就是盘符
//! （`C:\Users\x\a.bin`），这正是磁盘上的真实路径。
//!
//! 但降级模式的目录遍历可以指定一个**深目录**做扫描根
//! （`--root=D:\projects`）。这时 `Tree::path_of` 拼出来的前缀**仍然**是盘符 ——
//! 于是拿到的是 `C:\incoming\a.bin`，一个磁盘上并不存在的路径。
//!
//! 补一次前缀替换就能还原：`D:\projects\incoming\a.bin`。
//!
//! # 为什么必须同时用在「显示」和「落盘」两处
//!
//! 只修显示会留下更隐蔽的不一致：屏幕上路径是对的，而变更流水 JSONL 里
//! 写的是一堆不存在的路径 —— 用户拿日志回溯问题时才发现对不上。
//! 所以 CLI 的变更行/预警行和引擎的流水落盘都走这一个实现。

/// `from`（合成的盘根前缀）→ `to`（真实扫描根）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRemap {
    from: String,
    to: String,
}

impl PathRemap {
    /// 用「盘符」与「真实扫描根」构造。
    ///
    /// `scan_root` 为空（去掉分隔符后什么都不剩）时返回 `None` —— 没有可替换的东西。
    /// 扫描根本身就是盘根、或与盘符相同的情形**不**特殊处理：那样替换前后完全等价
    /// （`C:\a` → `C:\a`），多一层判断只是多一个出错的地方。
    ///
    /// `to` 里的正斜杠会被归一成反斜杠：Windows 两种分隔符都认，但
    /// `C:/a\b` 这种混合形态只是难读 —— 而调用方拿到的路径可能被 shell
    /// 做过转换（实测 git-bash 会把 `C:\x` 传成 `C:/x`）。
    pub fn new(drive: &str, scan_root: &str) -> Option<PathRemap> {
        let mut to = scan_root
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\");
        if to.is_empty() {
            return None;
        }
        // 盘根要**保留**尾反斜杠。`C:` 在 Windows 上的语义是「C 盘的当前目录」，
        // 与 `C:\` 不是一个东西；而索引树里的盘根路径正是 `C:\`。
        // 削掉尾杠会让扫描根为盘根时输出退化成 `C:` —— 与全项目
        // 「盘根带尾杠」的约定（`dirwatch::normalize_root`）正好相反。
        if to.len() == 2 && to.ends_with(':') {
            to.push('\\');
        }
        let from = format!("{}\\", drive.trim_end_matches(['\\', '/']));
        Some(PathRemap { from, to })
    }

    /// 把合成路径换回真实路径。不属于该前缀的路径原样返回。
    ///
    /// **幂等**：已经落在真实根下的路径会原样返回。这不是可有可无的洁癖 ——
    /// `C:\...\proj` 这样的真实路径同样以盘根开头，不加这道判断就会被再改一次，
    /// 变成 `C:\...\proj\Users\...\proj`。曾经真的发生过（预警行路径自我 append），
    /// 而且只在那一条显示路径上出错，最难发现。
    pub fn apply(&self, p: &str) -> String {
        // 已经是真的 → 不动。判断必须落在**路径分量边界**上：
        // 否则根是 `C:\a\proj` 时，`C:\a\projectX` 会被误认为「已在根下」。
        if p.len() >= self.to.len() {
            let head_ok = p
                .get(..self.to.len())
                .map(|h| h.eq_ignore_ascii_case(&self.to))
                .unwrap_or(false);
            if head_ok {
                let rest = &p[self.to.len()..];
                if rest.is_empty() || rest.starts_with('\\') {
                    return p.to_string();
                }
            }
        }
        // `get(..n)` 而不是 `[..n]`：前缀长度可能落在非字符边界上，
        // 直接切片会 panic（`C:\` 只有 3 字节，但 p 可能以多字节字符开头）。
        let Some(head) = p.get(..self.from.len()) else {
            return p.to_string();
        };
        if !head.eq_ignore_ascii_case(&self.from) {
            return p.to_string();
        }
        let tail = &p[self.from.len()..];
        if tail.is_empty() {
            // 被映射的就是盘根本身 → 直接给出扫描根
            return self.to.clone();
        }
        if self.to.ends_with('\\') {
            format!("{}{}", self.to, tail)
        } else {
            format!("{}\\{}", self.to, tail)
        }
    }

    /// 真实扫描根。
    pub fn root(&self) -> &str {
        &self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_synthetic_prefix() {
        let r = PathRemap::new("C:", r"C:\Users\me\Downloads").expect("应建立重映射");
        assert_eq!(
            r.apply(r"C:\incoming\a.bin"),
            r"C:\Users\me\Downloads\incoming\a.bin"
        );
        // 盘根本身映射成扫描根
        assert_eq!(r.apply(r"C:\"), r"C:\Users\me\Downloads");
        // 大小写不敏感（Windows 路径语义）
        assert_eq!(r.apply(r"c:\docs\x"), r"C:\Users\me\Downloads\docs\x");
        // 不属于该盘符的路径原样返回
        assert_eq!(r.apply(r"D:\other\x"), r"D:\other\x");
        // 非字符边界 / 超短输入不得 panic
        assert_eq!(r.apply(""), "");
        assert_eq!(r.apply("中"), "中");
        assert_eq!(r.apply("中文字符串测试"), "中文字符串测试");
    }

    #[test]
    fn empty_scan_root_has_nothing_to_remap() {
        assert!(PathRemap::new("C:", "").is_none());
        assert!(PathRemap::new("C:", "\\").is_none());
        assert!(PathRemap::new("C:", "/").is_none());
    }

    /// 扫描根与盘根相同 → 替换是恒等的（不特殊处理，但结果必须正确）
    #[test]
    fn drive_root_scan_root_is_identity() {
        let r = PathRemap::new("C:", r"C:\").expect("应建立重映射");
        assert_eq!(r.apply(r"C:\a\b.txt"), r"C:\a\b.txt");
        assert_eq!(r.apply(r"C:\"), r"C:\");
    }

    /// 正斜杠归一：调用方拿到的路径可能被 shell 转换过
    /// （实测 git-bash 把 `--root=C:\x` 传成 `C:/x`），
    /// 直接拼会输出 `C:/x\a\b` 这种混合分隔符。
    #[test]
    fn forward_slashes_are_normalized() {
        let r = PathRemap::new("C:", "C:/Users/me/Downloads/").expect("应建立重映射");
        assert_eq!(r.root(), r"C:\Users\me\Downloads");
        assert_eq!(
            r.apply(r"C:\incoming\a.bin"),
            r"C:\Users\me\Downloads\incoming\a.bin"
        );
        assert!(!r.apply(r"C:\x").contains('/'), "输出不应残留正斜杠");
    }

    /// 幂等：真实路径不能被二次替换。
    ///
    /// 真实路径同样以盘根开头，所以「匹配前缀就替换」会把它再改一遍 ——
    /// `C:\a\proj` → `C:\a\proj\a\proj`。这条曾经在预警行上真的发生过。
    #[test]
    fn apply_is_idempotent() {
        let r = PathRemap::new("C:", r"C:\a\proj").expect("应建立重映射");
        let once = r.apply(r"C:\incoming\x.bin");
        assert_eq!(once, r"C:\a\proj\incoming\x.bin");
        let twice = r.apply(&once);
        assert_eq!(twice, once, "真实路径不该被再替换一次");

        // 报警/事件里给的本来就是真实路径（用户敲的那条）→ 原样返回
        assert_eq!(r.apply(r"C:\a\proj"), r"C:\a\proj");
        assert_eq!(r.apply(r"C:\a\proj\sub\deep.bin"), r"C:\a\proj\sub\deep.bin");
        // 大小写不同也算同一个根
        assert_eq!(r.apply(r"c:\A\PROJ\sub"), r"c:\A\PROJ\sub");
    }

    /// 边界判定必须落在**路径分量**上。
    ///
    /// 输入契约是「合成路径 `C:\...` 或已在真实根下的路径」，两者之外的行为
    /// 不作保证；这条测试锁的是「不会把 `C:\a\projectX` 误判成已在 `C:\a\proj` 下」——
    /// 否则幂等判断会在看似相同的字符串前缀上放过一个本该替换的合成路径。
    #[test]
    fn idempotence_guard_requires_component_boundary() {
        let r = PathRemap::new("C:", r"C:\a\proj").expect("应建立重映射");
        // 字符串前缀相同但**不是**同一个路径分量 → 不算「已在根下」
        assert!(r.apply(r"C:\a\projectX").starts_with(r"C:\a\proj\"));
        // 真正的分量边界（`\` 或串尾）才算
        assert_eq!(r.apply(r"C:\a\proj\"), r"C:\a\proj\");
        assert_eq!(r.apply(r"C:\a\proj"), r"C:\a\proj");
    }
}
