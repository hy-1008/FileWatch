//! 宽字符与字符串工具（不引入任何外部 crate）。

/// UTF-8 `&str` → 以 NUL 结尾的 UTF-16 缓冲（供 `*W` API 使用）。
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 定长 / 不定长 UTF-16 切片 → `String`（非法代理对替换为 U+FFFD）。
pub fn from_utf16_lossy(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf)
}

/// UTF-16 切片 → `String`（遇到 NUL 停止）。
pub fn from_utf16_until_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 把 UTF-16LE 字节切片解码为 UTF-8 字节，直接追加进 `out`，返回写入的起始偏移与长度。
///
/// 这是 MFT `$FILE_NAME` 与 USN `FileName` 的共用路径：
/// 关键在于**不分配中间 `String`**，直接扩写字符串池。
pub fn utf16le_bytes_to_utf8(out: &mut Vec<u8>, raw: &[u8]) -> (u32, u32) {
    let start = out.len() as u32;
    let n = raw.len() / 2;

    // 常见情形：纯 ASCII 文件名（无代理对），走零分配快速路径。
    let mut all_ascii = true;
    for i in 0..n {
        let c = raw[i * 2];
        let hi = raw[i * 2 + 1];
        if hi != 0 || c >= 0x80 {
            all_ascii = false;
            break;
        }
    }
    if all_ascii {
        for i in 0..n {
            out.push(raw[i * 2]);
        }
        return (start, (out.len() as u32) - start);
    }

    // 慢路径：完整 UTF-16 解码（含代理对）。
    let units: Vec<u16> = (0..n)
        .map(|i| u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]))
        .collect();
    let s = String::from_utf16_lossy(&units);
    out.extend_from_slice(s.as_bytes());
    (start, (out.len() as u32) - start)
}

/// 人类可读的字节数（与资源管理器口径一致的 1024 进制）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

/// 取小写扩展名（不含点）。无扩展名返回空串。
pub fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(p) if p > 0 && p + 1 < name.len() => name[p + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 把 `v` 向上对齐到 `a` 的整数倍。`a == 0` 时原样返回（不做除法）。
///
/// 遍历模式没有真实的"分配簇数"，只能按簇大小向上取整估算体积占用；
/// 这个换算是**唯一实现**（遍历、重复文件、快照都可能用到）。
pub fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 || v == 0 {
        return v;
    }
    v.div_ceil(a) * a
}

/// `FILETIME` 计数（1601-01-01 起的 100 ns）→ Unix 秒。
///
/// 这是 NTFS / USN 与"人看得懂的时间"之间唯一的换算点：MFT 的
/// `$STANDARD_INFORMATION`、`USN_RECORD::TimeStamp`、以及 Windows 的
/// `Metadata::last_write_time` 都是这个口径。
///
/// `core-usn::record` 直接重导出本函数，不再自己写一份 —— 两个地方各写一遍
/// 时曾经出现过 1 秒的偏差（`/10_000_000` 与 `/1e7` 的取整差别）。
pub fn filetime_to_unix(ticks: i64) -> i64 {
    if ticks <= 0 {
        return 0;
    }
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    ticks / 10_000_000 - EPOCH_DIFF_SECS
}

/// 冒泡出当前进程是否具备管理员权限的最可靠判据：
/// 直接尝试打开卷设备。成功 = 有权限。
///
/// * Windows：能打开 `\\.\C:` 就说明令牌已提权；
/// * Linux：`geteuid() == 0`（`/dev` 下的块设备只对 root 开放读）。
///
/// 判据本身放在平台实现里（[`crate::volume::probe_elevated`]），
/// 因为"什么叫做有权限"两个平台问的不是同一件事。
pub fn is_elevated_hint() -> bool {
    crate::volume::probe_elevated()
}

// ── 时间 ────────────────────────────────────────────────────────────────────
//
// 引擎里所有时间都是 Unix 秒（**UTC**）：MFT 的 `$STANDARD_INFORMATION`、
// USN 记录、快照文件都是这个口径。要做"显示"就得自己算民用历 ——
// 零依赖意味着不能引 `chrono`。
//
// 这里既管"取现在几点"（[`unix_ms`] / [`unix_secs`]），也管"怎么把它写出来"
// （[`unix_utc`] / [`unix_stamp`]）。放在一起是有原因的：CLI 与界面**共用**
// 同一份换算，两处各算一遍的话，跨年/闰年时会出现"标题写 2027、文件名写 2026"
// 这种只在特定日期才看得见的错。
//
// 单位约定（写错过一次，所以写下来）：
// * `_ms` 结尾 = 毫秒，`u64`。变更流水的时间戳、阈值窗口都用它。
// * `_secs` / `unix` 说的是秒，`i64`。快照文件、MFT / USN 时间戳用它。

/// 当前 `Unix` 毫秒。
///
/// 放在这里而不是各处自己写 `SystemTime::now()`：CLI 与界面都要拿"现在"去
/// 给变更流水打时间戳、算阈值窗口、生成快照文件名。三份各写一遍的时候，
/// 单位（秒 / 毫秒）和"取不到时钟时返回什么"就很容易各说各话。
///
/// 取不到时钟返回 0（系统时间被调到 1970 之前时才会发生）。
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前 `Unix` 秒。等于 [`unix_ms`] 除以 1000，**不是**另一个时钟。
pub fn unix_secs() -> i64 {
    (unix_ms() / 1000) as i64
}

/// 「1970-01-01 起的天数」→ `(年, 月, 日)`。
///
/// 用的是 Howard Hinnant 的 `civil_from_days`：纯整数运算、无循环、无查表，
/// 且对负数（1970 年之前）同样成立。
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix 秒 → `2026-10-14 13:15:00Z`。非正数（未设置/无意义）返回 `-`。
pub fn unix_utc(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_string();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Unix 秒 → `20261014-131500`，给文件名用（没有冒号、空格这些非法字符）。
///
/// 与 [`unix_utc`] 走同一份换算：按名字排序即时间顺序，而且不会出现
/// 「同一次操作里两个时间对不上」。
pub fn unix_stamp(secs: i64) -> String {
    if secs <= 0 {
        return "00000000-000000".to_string();
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Unix 秒 → **本地** `2026-10-14 21:15:00`。非正数返回 `-`。
///
/// # 为什么需要另一个函数
///
/// `std` 里没有任何时区信息，[`unix_utc`] 只能给 UTC。但用户在资源管理器、
/// 任务栏、手机照片里看到的都是**本地**时间 —— 差 8 小时就会被当成 bug
/// （实测反馈：导出的报告写「生成于 15:09Z」，而系统时钟是 23:09）。
///
/// Windows 上走 `SystemTimeToTzSpecificLocalTime`，按**目标时刻**的夏令时
/// 规则换算（不是简单加一个固定偏移）。其它平台没有这份能力，退回 UTC
/// 文本（带 `Z` 后缀）—— 那是"明确没有"，不是"假装本地"。
pub fn local_time(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_string();
    }
    #[cfg(windows)]
    {
        if let Some(st) = local_systemtime(secs) {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
            );
        }
        // Win32 调用失败（极少见）：退回 UTC，而不是给一个空串让人以为没时间
        return unix_utc(secs);
    }
    #[cfg(not(windows))]
    {
        unix_utc(secs)
    }
}

/// Unix 秒 → **本地** `20261014-211500`（文件名用）。
///
/// 与 [`local_time`] 同一份换算：快照正文里写的时间与文件名上的时间必须是
/// 同一时区，否则"这一份是什么时候存的"会出现两个答案。
pub fn local_stamp(secs: i64) -> String {
    if secs <= 0 {
        return "00000000-000000".to_string();
    }
    let t = local_time(secs);
    // `YYYY-MM-DD HH:MM:SS` → `YYYYMMDD-HHMMSS`；取不到（UTC 回退也满足这个
    // 形状）就退回 unix_stamp，绝不拼出一个半截的名字。
    let b = t.as_bytes();
    if b.len() < 19 || b[10] != b' ' {
        return unix_stamp(secs);
    }
    format!(
        "{}{}{}-{}{}{}",
        &t[0..4],
        &t[5..7],
        &t[8..10],
        &t[11..13],
        &t[14..16],
        &t[17..19]
    )
}

/// UTC 的 Unix 秒 → 本地 `SYSTEMTIME`（仅 Windows）。失败返回 `None`。
#[cfg(windows)]
fn local_systemtime(secs: i64) -> Option<crate::winapi::SYSTEMTIME> {
    use crate::winapi::{
        FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime, FILETIME, SYSTEMTIME,
    };
    // 1601-01-01 → 1970-01-01 相差 11_644_473_600 秒；FILETIME 的单位是 100ns，
    // 所以还要乘 10^7 —— 漏掉这一步会得到"1601-01-01"这种看起来能格式化、
    // 但差了 10^7 倍的时间（第一版就是这么错的）。
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let since_1601 = secs.checked_add(EPOCH_DIFF_SECS)?;
    let ticks = (since_1601 as u64).checked_mul(10_000_000)?;
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&ft, &mut utc) } == 0 {
        return None;
    }
    let mut local = SYSTEMTIME::default();
    // 第一个参数传 NULL = 用当前系统时区（含目标时刻的夏令时）。
    if unsafe { SystemTimeToTzSpecificLocalTime(core::ptr::null(), &utc, &mut local) } == 0 {
        return None;
    }
    Some(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_works() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(1024 * 1024), "1.00 MB");
    }

    #[test]
    fn extension_works() {
        assert_eq!(extension_of("a.txt"), "txt");
        assert_eq!(extension_of("A.TXT"), "txt");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
    }

    #[test]
    fn utf16_fast_path() {
        let mut out = Vec::new();
        let raw: Vec<u8> = "hello.txt"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let (off, len) = utf16le_bytes_to_utf8(&mut out, &raw);
        assert_eq!(off, 0);
        assert_eq!(len, 9);
        assert_eq!(&out[..], b"hello.txt");
    }

    #[test]
    fn utf16_slow_path_cjk() {
        let mut out = Vec::new();
        let raw: Vec<u8> = "测试文件"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let (_, _) = utf16le_bytes_to_utf8(&mut out, &raw);
        assert_eq!(String::from_utf8(out).unwrap(), "测试文件");
    }

    /// 民用历换算：正常日期、闰日、世纪闰年规则（1900 不是闰年、2000 是）、
    /// 以及 1970 之前。
    #[test]
    fn civil_calendar_handles_the_awkward_cases() {
        let day = |s: &str| -> i64 {
            // 2021-01-01 = 18628 天；用已知锚点反推，避免手算天数出错
            match s {
                "1970-01-01" => 0,
                "2021-01-01" => 18_628,
                "2024-02-29" => 19_782,
                "2000-02-29" => 11_016,
                "1900-03-01" => -25_508,
                "1969-12-31" => -1,
                _ => panic!("没有这个锚点 {s}"),
            }
        };
        assert_eq!(civil_from_days(day("1970-01-01")), (1970, 1, 1));
        assert_eq!(civil_from_days(day("2021-01-01")), (2021, 1, 1));
        assert_eq!(civil_from_days(day("2024-02-29")), (2024, 2, 29), "闰日");
        assert_eq!(civil_from_days(day("2000-02-29")), (2000, 2, 29), "世纪闰年");
        assert_eq!(civil_from_days(day("1900-03-01")), (1900, 3, 1));
        assert_eq!(civil_from_days(day("1969-12-31")), (1969, 12, 31), "1970 之前");
    }

    #[test]
    fn unix_utc_is_stable() {
        assert_eq!(unix_utc(0), "-");
        assert_eq!(unix_utc(-5), "-");
        assert_eq!(unix_utc(1), "1970-01-01 00:00:01Z");
        assert_eq!(unix_utc(1_609_459_200), "2021-01-01 00:00:00Z");
        assert_eq!(unix_utc(1_709_164_800), "2024-02-29 00:00:00Z");
        // 一天里的最后一秒：不能靠 div/rem 的符号行为蒙对，要显式用 euclid
        assert_eq!(unix_utc(1_609_459_200 - 1), "2020-12-31 23:59:59Z");
    }

    /// 秒与毫秒必须是**同一个时钟**：`unix_secs` 是 `unix_ms` 的除法，
    /// 不是第二次 `SystemTime::now()`。否则在一次很慢的操作前后调用两者，
    /// 会得到"文件名比标题晚 1 秒"这种无法复现的错位。
    #[test]
    fn secs_is_just_ms_divided_by_a_thousand() {
        let before = unix_secs();
        let ms = unix_ms();
        let after = unix_secs();
        assert!(ms / 1000 >= before as u64);
        assert!(ms / 1000 <= after as u64);
        // 单调不减：时钟只会往前走（除非有人改系统时间）
        assert!(after >= before);
        // 现代机器上这个值一定远大于 2020-01-01，用来抓"单位写错"（秒当成毫秒）
        assert!(ms > 1_577_836_800_000, "毫秒值看起来不像毫秒：{ms}");
        assert!(unix_secs() > 1_577_836_800, "秒值看起来不像秒");
    }

    /// 两个格式化函数必须是**同一份换算**：同一个 Unix 秒拆出来的年月日时分秒
    /// 必须一致（这是"文件名与标题对不上"那类错的唯一防线）。
    #[test]
    fn stamp_and_utc_agree() {
        for secs in [1_i64, 1_609_459_200, 1_709_164_800, 1_792_000_123, 2_000_000_000] {
            let utc = unix_utc(secs);
            let stamp = unix_stamp(secs);
            let digits: String = utc
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>();
            let stamp_digits: String = stamp.chars().filter(|c| c.is_ascii_digit()).collect();
            assert_eq!(digits, stamp_digits, "{utc} 与 {stamp} 的日期时间不一致");
        }
        assert_eq!(unix_stamp(0), "00000000-000000");
    }
}

#[cfg(test)]
mod local_time_tests {
    use super::*;

    /// 本地时间与 UTC 的差必须是**整分钟**，且秒数一致。
    ///
    /// 时区偏移只会是 15 分钟的整数倍（印度 :30、尼泊尔 :45），永远不会带秒 ——
    /// 所以这条不变量能挡住"把 FILETIME 的 100ns 当成秒"、"偏移算错一小时"
    /// 这类错误，又不用把测试绑死在某台机器的时区上。
    #[test]
    fn local_time_is_a_whole_minute_away_from_utc() {
        let t = 1_760_000_000i64; // 2025-10-09 08:53:20Z
        let l = local_time(t);
        let u = unix_utc(t);
        assert_eq!(l.len(), 19, "本地时间格式应为 YYYY-MM-DD HH:MM:SS：{l}");
        assert_eq!(&l[4..5], "-", "{l}");
        assert_eq!(&l[10..11], " ", "{l}");
        // 秒必须一致：时区不会偏移"秒"
        assert_eq!(&l[17..19], &u[17..19], "本地 {l} / UTC {u}");
        let mins = |s: &str| -> i32 {
            s[11..13].parse::<i32>().unwrap() * 60 + s[14..16].parse::<i32>().unwrap()
        };
        let diff = (mins(&l) - mins(&u)).rem_euclid(1440);
        assert_eq!(diff % 15, 0, "本地 {l} / UTC {u} 的偏移不是 15 分钟的整数倍");
    }

    #[test]
    fn local_time_handles_zero_and_garbage() {
        assert_eq!(local_time(0), "-");
        assert_eq!(local_time(-1), "-");
        // 极端值不能让 Win32 调用炸掉（返回什么都行，只要不 panic）
        let _ = local_time(i64::MAX / 2);
    }

    #[test]
    fn local_stamp_matches_local_time() {
        let t = 1_760_000_000i64;
        let out = local_stamp(t);
        assert_eq!(out.len(), 15, "{out}");
        assert_eq!(&out[8..9], "-", "{out}");
        let time = local_time(t);
        assert_eq!(&out[..8], format!("{}{}{}", &time[0..4], &time[5..7], &time[8..10]));
        assert_eq!(&out[9..], format!("{}{}{}", &time[11..13], &time[14..16], &time[17..19]));
        assert_eq!(local_stamp(0), "00000000-000000");
    }
}
