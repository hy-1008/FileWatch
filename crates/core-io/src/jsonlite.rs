//! 极简 JSON 工具（写入 + 扁平对象解析）。
//!
//! workspace 零外部依赖，所以不引入 `serde_json`。这里只提供引擎真正需要的两件事：
//! * **写**：给游标文件、变更流水（JSONL）、CLI 的 `--json` 输出做转义与拼装；
//! * **读**：解析游标文件这种「一层扁平对象」。
//!
//! 不做的是：嵌套数组/对象的通用解析。CLI 输出是只写不读的。

/// JSON 字符串转义（不含两侧引号）。
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{08}' => o.push_str("\\b"),
            '\u{0C}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// 转义并加上双引号。
pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    o.push_str(&escape(s));
    o.push('"');
    o
}

/// 解析一层扁平 JSON 对象，返回 `(key, value)` 列表。
///
/// * 字符串值会被反转义；
/// * 数字/布尔/`null` 原样返回文本；
/// * 任何解析异常都**静默停止**并返回已解析出的部分（容错优先）。
pub fn parse_flat_object(input: &str) -> Vec<(String, String)> {
    let b: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();

    skip_ws(&b, &mut i);
    if i < b.len() && b[i] == '{' {
        i += 1;
    }

    loop {
        skip_ws(&b, &mut i);
        if i >= b.len() || b[i] == '}' {
            break;
        }
        if b[i] != '"' {
            break;
        }
        let key = match parse_string(&b, &mut i) {
            Some(k) => k,
            None => break,
        };
        skip_ws(&b, &mut i);
        if i >= b.len() || b[i] != ':' {
            break;
        }
        i += 1;
        skip_ws(&b, &mut i);

        let val = if i < b.len() && b[i] == '"' {
            match parse_string(&b, &mut i) {
                Some(v) => v,
                None => break,
            }
        } else {
            let start = i;
            while i < b.len() && b[i] != ',' && b[i] != '}' {
                i += 1;
            }
            b[start..i].iter().collect::<String>().trim().to_string()
        };
        out.push((key, val));

        skip_ws(&b, &mut i);
        if i < b.len() && b[i] == ',' {
            i += 1;
        }
    }
    out
}

/// 从扁平对象里取字符串字段。
pub fn get<'a>(obj: &'a [(String, String)], key: &str) -> Option<&'a str> {
    obj.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// 从扁平对象里取 u64 字段。
pub fn get_u64(obj: &[(String, String)], key: &str) -> Option<u64> {
    get(obj, key)?.parse().ok()
}

/// 从扁平对象里取 i64 字段。
pub fn get_i64(obj: &[(String, String)], key: &str) -> Option<i64> {
    get(obj, key)?.parse().ok()
}

fn skip_ws(b: &[char], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], ' ' | '\t' | '\n' | '\r') {
        *i += 1;
    }
}

fn parse_string(b: &[char], i: &mut usize) -> Option<String> {
    if *i >= b.len() || b[*i] != '"' {
        return None;
    }
    *i += 1;
    let mut o = String::new();
    while *i < b.len() {
        let c = b[*i];
        *i += 1;
        match c {
            '"' => return Some(o),
            '\\' => {
                if *i >= b.len() {
                    return None;
                }
                let e = b[*i];
                *i += 1;
                match e {
                    '"' => o.push('"'),
                    '\\' => o.push('\\'),
                    '/' => o.push('/'),
                    'n' => o.push('\n'),
                    'r' => o.push('\r'),
                    't' => o.push('\t'),
                    'b' => o.push('\u{08}'),
                    'f' => o.push('\u{0C}'),
                    'u' => {
                        if *i + 4 > b.len() {
                            return None;
                        }
                        let hex: String = b[*i..*i + 4].iter().collect();
                        *i += 4;
                        let cp = u32::from_str_radix(&hex, 16).ok()?;
                        o.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                    }
                    other => o.push(other),
                }
            }
            c => o.push(c),
        }
    }
    None
}

/// 把一定量的字节转成人类可读字符串的 JSON 值（保留原始数值）。
pub fn num_u64(v: u64) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_specials() {
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("line\nbreak"), "line\\nbreak");
        assert_eq!(escape("back\\slash"), "back\\\\slash");
        assert_eq!(escape("tab\tx"), "tab\\tx");
        assert_eq!(quote("hi"), "\"hi\"");
    }

    #[test]
    fn escapes_control_chars() {
        assert_eq!(escape("\u{01}"), "\\u0001");
    }

    #[test]
    fn parses_flat_object() {
        let s = r#"{"journal_id": 123456789, "next_usn": "42", "volume": "C:", "ok": true}"#;
        let o = parse_flat_object(s);
        assert_eq!(get_u64(&o, "journal_id"), Some(123456789));
        assert_eq!(get(&o, "next_usn"), Some("42"));
        assert_eq!(get(&o, "volume"), Some("C:"));
        assert_eq!(get(&o, "ok"), Some("true"));
    }

    #[test]
    fn parses_escaped_strings_roundtrip() {
        let original = "C:\\Users\\测试 \"x\"\n";
        let json = format!("{{\"p\": {}}}", quote(original));
        let o = parse_flat_object(&json);
        assert_eq!(get(&o, "p"), Some(original));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_flat_object("").is_empty());
        assert!(parse_flat_object("{").is_empty());
        assert!(parse_flat_object("not json").is_empty());
        // 截断的 JSON：返回已解析部分
        let o = parse_flat_object(r#"{"a": 1, "b":"#);
        assert_eq!(get(&o, "a"), Some("1"));
    }

    #[test]
    fn negative_numbers_ok() {
        let o = parse_flat_object(r#"{"n": -5}"#);
        assert_eq!(get_i64(&o, "n"), Some(-5));
    }
}
