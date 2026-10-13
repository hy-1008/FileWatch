//! USN `Reason` 位掩码 → 语义事件归一化（文档 §5.2 映射表）。

// ── Reason 位（winioctl.h） ─────────────────────────────────────────────────
pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const USN_REASON_NAMED_DATA_OVERWRITE: u32 = 0x0000_0010;
pub const USN_REASON_NAMED_DATA_EXTEND: u32 = 0x0000_0020;
pub const USN_REASON_NAMED_DATA_TRUNCATION: u32 = 0x0000_0040;
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_EA_CHANGE: u32 = 0x0000_0400;
pub const USN_REASON_SECURITY_CHANGE: u32 = 0x0000_0800;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_INDEXABLE_CHANGE: u32 = 0x0000_4000;
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
pub const USN_REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
pub const USN_REASON_COMPRESSION_CHANGE: u32 = 0x0002_0000;
pub const USN_REASON_ENCRYPTION_CHANGE: u32 = 0x0004_0000;
pub const USN_REASON_OBJECT_ID_CHANGE: u32 = 0x0008_0000;
pub const USN_REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
pub const USN_REASON_STREAM_CHANGE: u32 = 0x0020_0000;
pub const USN_REASON_TRANSACTED_CHANGE: u32 = 0x0040_0000;
pub const USN_REASON_INTEGRITY_CHANGE: u32 = 0x0080_0000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// 「数据被写过」的位组合——大小可能变化，需要回读 MFT 拿新大小。
pub const REASON_DATA_MASK: u32 =
    USN_REASON_DATA_OVERWRITE | USN_REASON_DATA_EXTEND | USN_REASON_DATA_TRUNCATION;

/// 命名数据流（ADS）的写入——文档 §3.2：只统计主流。
pub const REASON_NAMED_DATA_MASK: u32 = USN_REASON_NAMED_DATA_OVERWRITE
    | USN_REASON_NAMED_DATA_EXTEND
    | USN_REASON_NAMED_DATA_TRUNCATION;

/// 会影响「占用大小」语义的位（压缩/加密/稀疏）。
pub const REASON_SIZE_SEMANTICS_MASK: u32 =
    USN_REASON_COMPRESSION_CHANGE | USN_REASON_ENCRYPTION_CHANGE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Created,
    Deleted,
    Renamed,
    Modified,
    AttrChanged,
    HardLinkChanged,
    SizeSemanticsChanged,
    Unknown,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Deleted => "deleted",
            EventKind::Renamed => "renamed",
            EventKind::Modified => "modified",
            EventKind::AttrChanged => "attr_changed",
            EventKind::HardLinkChanged => "hardlink_changed",
            EventKind::SizeSemanticsChanged => "size_semantics_changed",
            EventKind::Unknown => "unknown",
        }
    }

    /// 中文短标签（UI / 流水表用）。
    pub fn label_cn(self) -> &'static str {
        match self {
            EventKind::Created => "新增",
            EventKind::Deleted => "删除",
            EventKind::Renamed => "重命名",
            EventKind::Modified => "修改",
            EventKind::AttrChanged => "属性变更",
            EventKind::HardLinkChanged => "硬链接变更",
            EventKind::SizeSemanticsChanged => "压缩/加密变更",
            EventKind::Unknown => "其他",
        }
    }

    /// 该事件是否需要回读 MFT 重新取大小（文档 §5.3）。
    pub fn needs_size_reread(self) -> bool {
        matches!(
            self,
            EventKind::Created | EventKind::Modified | EventKind::SizeSemanticsChanged
        )
    }

    /// 解析 `--filter` 参数。
    pub fn parse(s: &str) -> Option<EventKind> {
        match s.to_ascii_lowercase().as_str() {
            "created" | "create" | "新增" => Some(EventKind::Created),
            "deleted" | "delete" | "删除" => Some(EventKind::Deleted),
            "renamed" | "rename" | "重命名" => Some(EventKind::Renamed),
            "modified" | "modify" | "修改" => Some(EventKind::Modified),
            "attr" | "attr_changed" | "属性" => Some(EventKind::AttrChanged),
            "hardlink" | "硬链接" => Some(EventKind::HardLinkChanged),
            "semantics" | "压缩" => Some(EventKind::SizeSemanticsChanged),
            _ => None,
        }
    }
}

/// 位掩码 → 单一语义事件。
///
/// 优先级按「语义最强的先判」排列，与文档 §5.2 表格一致。
pub fn classify(reason: u32) -> EventKind {
    if reason & USN_REASON_FILE_DELETE != 0 {
        return EventKind::Deleted;
    }
    if reason & USN_REASON_FILE_CREATE != 0 {
        return EventKind::Created;
    }
    if reason & (USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME) != 0 {
        return EventKind::Renamed;
    }
    if reason & REASON_DATA_MASK != 0 {
        return EventKind::Modified;
    }
    if reason & REASON_SIZE_SEMANTICS_MASK != 0 {
        return EventKind::SizeSemanticsChanged;
    }
    if reason & USN_REASON_HARD_LINK_CHANGE != 0 {
        return EventKind::HardLinkChanged;
    }
    if reason & (USN_REASON_BASIC_INFO_CHANGE
        | USN_REASON_SECURITY_CHANGE
        | USN_REASON_EA_CHANGE
        | USN_REASON_OBJECT_ID_CHANGE
        | USN_REASON_INDEXABLE_CHANGE
        | USN_REASON_REPARSE_POINT_CHANGE
        | USN_REASON_INTEGRITY_CHANGE)
        != 0
    {
        return EventKind::AttrChanged;
    }
    // 只有 CLOSE 或只有 ADS 变更 → 不产生独立业务事件
    if reason & (USN_REASON_CLOSE | REASON_NAMED_DATA_MASK | USN_REASON_STREAM_CHANGE) != 0 {
        return EventKind::Unknown;
    }
    EventKind::Unknown
}

#[inline]
pub fn has_close(reason: u32) -> bool {
    reason & USN_REASON_CLOSE != 0
}

#[inline]
pub fn is_data_change(reason: u32) -> bool {
    reason & REASON_DATA_MASK != 0
}

/// Windows 回收站命名规则：`$R...` 是真实内容，`$I...` 是元数据。
/// 文档 §5.2 要求把 `$R` 前缀的重命名**转化为删除事件**。
#[inline]
pub fn is_recycle_bin_content(name: &str) -> bool {
    name.len() >= 2 && name.starts_with("$R")
}

#[inline]
pub fn is_recycle_bin_metadata(name: &str) -> bool {
    name.len() >= 2 && name.starts_with("$I")
}

/// 编辑器/编译器的临时文件模式（文档 §5.2：可在 debounce 阶段过滤掉这些噪声）。
pub fn is_temp_pattern(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tmp")
        || lower.ends_with(".temp")
        || lower.ends_with(".crdownload")
        || lower.ends_with(".part")
        || lower.ends_with(".partial")
        || lower.ends_with(".download")
        || lower.ends_with('~')
        || lower.starts_with("~$")
        || lower.starts_with(".~")
        // VSCode / JetBrains 的原子保存临时名
        || (lower.starts_with('.') && lower.ends_with(".swp"))
        || lower.ends_with(".swx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_priority() {
        // 删除优先于一切
        assert_eq!(
            classify(USN_REASON_FILE_DELETE | USN_REASON_CLOSE),
            EventKind::Deleted
        );
        assert_eq!(
            classify(USN_REASON_FILE_CREATE | USN_REASON_CLOSE),
            EventKind::Created
        );
        assert_eq!(
            classify(USN_REASON_DATA_EXTEND | USN_REASON_CLOSE),
            EventKind::Modified
        );
    }

    #[test]
    fn classify_rename_pairing_bits() {
        assert_eq!(classify(USN_REASON_RENAME_OLD_NAME), EventKind::Renamed);
        assert_eq!(classify(USN_REASON_RENAME_NEW_NAME), EventKind::Renamed);
    }

    #[test]
    fn classify_close_only_is_noise() {
        assert_eq!(classify(USN_REASON_CLOSE), EventKind::Unknown);
        assert_eq!(classify(0), EventKind::Unknown);
    }

    #[test]
    fn classify_attr_and_semantics() {
        assert_eq!(
            classify(USN_REASON_BASIC_INFO_CHANGE),
            EventKind::AttrChanged
        );
        assert_eq!(
            classify(USN_REASON_COMPRESSION_CHANGE),
            EventKind::SizeSemanticsChanged
        );
        assert_eq!(
            classify(USN_REASON_HARD_LINK_CHANGE),
            EventKind::HardLinkChanged
        );
    }

    #[test]
    fn only_ads_change_is_noise() {
        assert_eq!(
            classify(USN_REASON_NAMED_DATA_EXTEND | USN_REASON_CLOSE),
            EventKind::Unknown
        );
    }

    #[test]
    fn size_reread_rules() {
        assert!(EventKind::Created.needs_size_reread());
        assert!(EventKind::Modified.needs_size_reread());
        assert!(!EventKind::Deleted.needs_size_reread());
        assert!(!EventKind::Renamed.needs_size_reread());
    }

    #[test]
    fn recycle_bin_detection() {
        assert!(is_recycle_bin_content("$R1ABCDEF.txt"));
        assert!(!is_recycle_bin_content("$I1ABCDEF.txt"));
        assert!(is_recycle_bin_metadata("$I1ABCDEF.txt"));
        assert!(!is_recycle_bin_content("normal.txt"));
    }

    #[test]
    fn temp_patterns() {
        assert!(is_temp_pattern("foo.tmp"));
        assert!(is_temp_pattern("foo.TMP"));
        assert!(is_temp_pattern("~$report.docx"));
        assert!(is_temp_pattern(".~lock.odt"));
        assert!(is_temp_pattern("file.crdownload"));
        assert!(!is_temp_pattern("report.docx"));
        assert!(!is_temp_pattern("data.csv"));
    }

    #[test]
    fn kind_parsing_roundtrip() {
        assert_eq!(EventKind::parse("created"), Some(EventKind::Created));
        assert_eq!(EventKind::parse("删除"), Some(EventKind::Deleted));
        assert_eq!(EventKind::parse("nonsense"), None);
    }
}
