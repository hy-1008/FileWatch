//! 事件归一化与 debounce 合并（文档 §5.2「事件归一化 + 合并」）。
//!
//! ## 为什么必须要 debounce
//!
//! 一次 save 在 VSCode 类编辑器下会产生 6+ 个 USN 事件
//! （建临时文件 → 写 → 删原件 → 改名 → 改属性 → flush）；
//! `git checkout` 一次能爆发上万个事件。若逐个上报，UI 直接刷屏。
//!
//! ## 合并规则
//!
//! | 先发生 | 后发生 | 合并结果 |
//! |---|---|---|
//! | Created | Modified | Created |
//! | Created | Deleted | **抵消**（什么都没发生） |
//! | Modified | Deleted | Deleted |
//! | Deleted | Created（同 FRN 复用） | Deleted + Created 两条 |
//! | RenameOld | RenameNew（同 FRN） | 一条 Renamed（含新旧名） |
//!
//! 另外按文档 §5.2：
//! * 重命名为 `$R` 前缀 → **转化为删除事件**（Windows 回收站规则）；
//! * `.tmp` / `~$` 等临时文件模式可过滤。

use core_index::arena::INVALID_PARENT as INVALID_RECNO;
use core_index::RECNO_MASK as FRN_RECNO_MASK;
use core_usn::normalize::{self, EventKind};
use std::collections::HashMap;

/// 从 USN 记录直接转换来的「原始事件」。
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    /// Unix 秒
    pub timestamp: i64,
    pub reason: u32,
    pub file_attributes: u32,
    pub name: String,
}

impl RawEvent {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.file_attributes & core_io::winapi::FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// debounce 之后的「确定事件」，可以直接驱动索引更新与预警。
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub frn: u64,
    pub parent_frn: u64,
    pub kind: EventKind,
    pub name: String,
    /// 重命名时的旧名
    pub old_name: Option<String>,
    pub is_dir: bool,
    pub first_ms: u64,
    pub last_ms: u64,
    /// 合并进来的原始事件数
    pub merged: u32,
    /// 最新一次 USN（游标推进用）
    pub usn: i64,
    pub reason: u32,
    /// 大小变化量（回读 MFT 后由引擎填入；正=增长）
    pub size_delta: i64,
    pub alloc_delta: i64,
    /// 新的大小（回读后填入）
    pub new_size: u64,
    pub new_alloc: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct DebounceConfig {
    /// 合并窗口（文档 §5.2：300~500 ms）
    pub window_ms: u64,
    /// 过滤 `.tmp` / `~$` / `.crdownload` 等临时文件噪声
    pub filter_temp_files: bool,
    /// 把 `$R` 前缀的重命名转成删除事件
    pub recycle_bin_as_delete: bool,
}

impl Default for DebounceConfig {
    fn default() -> Self {
        DebounceConfig {
            window_ms: 400,
            filter_temp_files: false,
            recycle_bin_as_delete: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DebounceStats {
    pub offered: u64,
    pub emitted: u64,
    /// 因「创建后又被删除」而抵消掉的事件数
    pub cancelled: u64,
    pub filtered_temp: u64,
    pub rename_pairs: u64,
    /// 未配对的 rename（另一条可能在窗口之外或已丢失）
    pub unpaired_renames: u64,
}

struct Slot {
    ev: PendingEvent,
    /// 该 FRN 最近一次 rename 旧名（等待配对新名）
    pending_old_name: Option<String>,
}

/// 事件合并器。按 `frn` 聚合（**同一个文件在窗口内的多次变更合并为一条**）。
pub struct Debouncer {
    cfg: DebounceConfig,
    slots: Vec<Slot>,
    index: HashMap<u64, usize>,
    stats: DebounceStats,
}

impl Debouncer {
    pub fn new(cfg: DebounceConfig) -> Debouncer {
        Debouncer {
            cfg,
            slots: Vec::with_capacity(1024),
            index: HashMap::with_capacity(1024),
            stats: DebounceStats::default(),
        }
    }

    pub fn stats(&self) -> DebounceStats {
        self.stats
    }

    pub fn pending_len(&self) -> usize {
        self.slots.len()
    }

    pub fn has_pending(&self, frn: u64) -> bool {
        self.index.contains_key(&frn)
    }

    /// 投递一个原始事件。
    pub fn offer(&mut self, raw: RawEvent, now_ms: u64) {
        self.stats.offered += 1;

        // ① 临时文件过滤
        if self.cfg.filter_temp_files && normalize::is_temp_pattern(&raw.name) {
            self.stats.filtered_temp += 1;
            return;
        }

        let mut kind = normalize::classify(raw.reason);

        // ② 回收站：重命名为 $Rxxxx → 当作删除
        if self.cfg.recycle_bin_as_delete
            && kind == EventKind::Renamed
            && raw.reason & normalize::USN_REASON_RENAME_NEW_NAME != 0
            && normalize::is_recycle_bin_content(&raw.name)
        {
            kind = EventKind::Deleted;
        }
        // $Ixxxx 是回收站元数据，本身无意义
        if self.cfg.recycle_bin_as_delete && normalize::is_recycle_bin_metadata(&raw.name) {
            self.stats.filtered_temp += 1;
            return;
        }

        // ③ 纯 CLOSE / 纯 ADS 变更不产生业务事件
        if kind == EventKind::Unknown {
            return;
        }

        let is_old_name = raw.reason & normalize::USN_REASON_RENAME_OLD_NAME != 0
            && raw.reason & normalize::USN_REASON_RENAME_NEW_NAME == 0;

        match self.index.get(&raw.frn).copied() {
            None => {
                let slot = Slot {
                    ev: PendingEvent {
                        frn: raw.frn,
                        parent_frn: raw.parent_frn,
                        kind,
                        name: if is_old_name { String::new() } else { raw.name.clone() },
                        old_name: if is_old_name { Some(raw.name.clone()) } else { None },
                        is_dir: raw.is_dir(),
                        first_ms: now_ms,
                        last_ms: now_ms,
                        merged: 1,
                        usn: raw.usn,
                        reason: raw.reason,
                        size_delta: 0,
                        alloc_delta: 0,
                        new_size: 0,
                        new_alloc: 0,
                    },
                    pending_old_name: if is_old_name { Some(raw.name.clone()) } else { None },
                };
                self.index.insert(raw.frn, self.slots.len());
                self.slots.push(slot);
            }
            Some(i) => {
                self.merge_at(i, raw, kind, is_old_name, now_ms);
            }
        }
    }

    fn merge_at(&mut self, i: usize, raw: RawEvent, kind: EventKind, is_old_name: bool, now_ms: u64) {
        let prev_kind = self.slots[i].ev.kind;
        let slot = &mut self.slots[i];

        slot.ev.last_ms = now_ms;
        slot.ev.merged += 1;
        slot.ev.usn = raw.usn;
        slot.ev.reason |= raw.reason;
        slot.ev.is_dir |= raw.is_dir();
        // 父指针只允许被**有效**值覆盖。
        //
        // FRN 低 48 位是 MFT 记录号，`u32::MAX` 是「无父」哨兵
        // （core-index 的 INVALID_PARENT；core-monitor 不依赖 core-index，
        //  所以这里只按位判断，不引用那个常量）。
        //
        // 为什么必须防：降级模式下节点是懒物化的，「同一窗口内先新增、再修改」
        // 时后一条事件可能带着无效父指针。若让它盖掉前一条已经正确的父，
        // 合并出的事件就再也传不上 delta —— 症状是「文件大小对，目录总量不变」，
        // 属于最难查的静默错误。宁可保守：无效值一律不覆盖。
        if (raw.parent_frn & FRN_RECNO_MASK) as u32 != INVALID_RECNO
            || (slot.ev.parent_frn & FRN_RECNO_MASK) as u32 == INVALID_RECNO
        {
            slot.ev.parent_frn = raw.parent_frn;
        }

        if is_old_name {
            // 记录旧名，等配对
            slot.pending_old_name = Some(raw.name.clone());
            if slot.ev.old_name.is_none() {
                slot.ev.old_name = Some(raw.name.clone());
            }
            // 旧名不是一次新的状态转换，尤其不能让尚未入树的 Created 丢失。
            return;
        } else {
            slot.ev.name = raw.name.clone();
            // ④ rename 配对：旧名 + 新名 → 合成一条 Renamed
            if raw.reason & normalize::USN_REASON_RENAME_NEW_NAME != 0 {
                if let Some(old) = slot.pending_old_name.take() {
                    if slot.ev.old_name.is_none() {
                        slot.ev.old_name = Some(old);
                    }
                    slot.ev.kind = match (prev_kind, kind) {
                        (EventKind::Created, EventKind::Deleted) => EventKind::Unknown,
                        (EventKind::Created, _) => EventKind::Created,
                        (EventKind::Deleted, _) => EventKind::Deleted,
                        (_, k) => k,
                    };
                    self.stats.rename_pairs += 1;
                    return;
                }
            }
        }

        // ⑤ 状态机合并
        slot.ev.kind = match (prev_kind, kind) {
            // 创建后又被删除 → 完全抵消（等价于没发生）
            (EventKind::Created, EventKind::Deleted) => EventKind::Unknown,
            // 创建后修改 → 仍是创建
            (EventKind::Created, EventKind::Modified) => EventKind::Created,
            (EventKind::Created, _) => EventKind::Created,
            // 删除是终态
            (EventKind::Deleted, _) => EventKind::Deleted,
            // 重命名后修改 → 仍报重命名
            (EventKind::Renamed, EventKind::Modified) => EventKind::Renamed,
            // 重命名后删除 → 删除
            (EventKind::Renamed, EventKind::Deleted) => EventKind::Deleted,
            (_, k) => k,
        };
    }

    /// 取出已「成熟」（距最近一次变更超过窗口）的事件。
    pub fn drain_matured(&mut self, now_ms: u64, out: &mut Vec<PendingEvent>) {
        self.drain_matured_limited(now_ms, usize::MAX, out);
    }

    /// 达到本轮处理额度后保留其余事件，下一轮继续；不能截断已取出的事件。
    pub fn drain_matured_limited(
        &mut self,
        now_ms: u64,
        limit: usize,
        out: &mut Vec<PendingEvent>,
    ) {
        let window = self.cfg.window_ms;
        let mut keep: Vec<Slot> = Vec::with_capacity(self.slots.len());
        let mut emitted = 0;

        for slot in self.slots.drain(..) {
            let matured = now_ms.saturating_sub(slot.ev.last_ms) >= window;
            if matured && (emitted < limit || slot.ev.kind == EventKind::Unknown) {
                if slot.ev.kind == EventKind::Unknown {
                    self.stats.cancelled += 1;
                } else {
                    if slot.ev.kind == EventKind::Renamed && slot.ev.old_name.is_none() {
                        self.stats.unpaired_renames += 1;
                    }
                    self.stats.emitted += 1;
                    emitted += 1;
                    out.push(slot.ev);
                }
            } else {
                keep.push(slot);
            }
        }

        self.slots = keep;
        self.reindex();
    }

    /// 无视窗口，强制吐出全部（进程退出 / 手动 flush）。
    pub fn flush_all(&mut self, out: &mut Vec<PendingEvent>) {
        for slot in self.slots.drain(..) {
            if slot.ev.kind == EventKind::Unknown {
                self.stats.cancelled += 1;
            } else {
                if slot.ev.kind == EventKind::Renamed && slot.ev.old_name.is_none() {
                    self.stats.unpaired_renames += 1;
                }
                self.stats.emitted += 1;
                out.push(slot.ev);
            }
        }
        self.index.clear();
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, s) in self.slots.iter().enumerate() {
            self.index.insert(s.ev.frn, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_usn::normalize::*;

    fn raw(frn: u64, reason: u32, name: &str) -> RawEvent {
        RawEvent {
            frn,
            parent_frn: 5,
            usn: 1,
            timestamp: 1_700_000_000,
            reason,
            file_attributes: core_io::winapi::FILE_ATTRIBUTE_ARCHIVE,
            name: name.to_string(),
        }
    }

    fn drain(d: &mut Debouncer, now: u64) -> Vec<PendingEvent> {
        let mut v = Vec::new();
        d.drain_matured(now, &mut v);
        v
    }

    /// 回归守卫：降级模式下节点懒物化，「同一窗口内先新增、再修改」时
    /// 后一条事件可能带 INVALID_PARENT。若让它盖掉前一条已正确的父，
    /// 合并出的事件就传不上 delta（症状：文件大小对、目录总量不变）。
    #[test]
    fn invalid_parent_never_overwrites_valid_one() {
        let mut d = Debouncer::new(DebounceConfig {
            window_ms: 100,
            filter_temp_files: false,
            recycle_bin_as_delete: true,
        });
        let valid_parent = 0x0000_0000_0000_000Au64; // 记录号 10
        let invalid_parent = u32::MAX as u64; // 记录号 u32::MAX = INVALID_PARENT

        let mut e1 = raw(0x300, USN_REASON_FILE_CREATE, "a.bin");
        e1.parent_frn = valid_parent;
        d.offer(e1, 1000);

        let mut e2 = raw(0x300, USN_REASON_DATA_EXTEND, "a.bin");
        e2.parent_frn = invalid_parent;
        d.offer(e2, 1050);

        let out = drain(&mut d, 1200);
        assert_eq!(out.len(), 1, "两条事件应合并为一条");
        assert_eq!(out[0].kind, EventKind::Created, "创建后修改仍是创建");
        assert_eq!(
            (out[0].parent_frn & FRN_RECNO_MASK) as u32, 10,
            "有效父指针不得被 INVALID_PARENT 覆盖"
        );

        // 反向：先无效后有效 → 必须被有效值修正
        let mut d = Debouncer::new(DebounceConfig {
            window_ms: 100,
            filter_temp_files: false,
            recycle_bin_as_delete: true,
        });
        let mut e1 = raw(0x400, USN_REASON_FILE_CREATE, "b.bin");
        e1.parent_frn = invalid_parent;
        d.offer(e1, 1000);
        let mut e2 = raw(0x400, USN_REASON_DATA_EXTEND, "b.bin");
        e2.parent_frn = valid_parent;
        d.offer(e2, 1050);
        let out = drain(&mut d, 1200);
        assert_eq!((out[0].parent_frn & FRN_RECNO_MASK) as u32, 10);
    }

    #[test]
    fn single_create_emits_one_event() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE | USN_REASON_CLOSE, "a.txt"), 1000);
        // 未到窗口
        assert!(drain(&mut d, 1100).is_empty());
        // 窗口过后
        let out = drain(&mut d, 1600);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Created);
        assert_eq!(out[0].name, "a.txt");
    }

    #[test]
    fn create_then_delete_cancels_out() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "tmp.exe"), 1000);
        d.offer(raw(1, USN_REASON_FILE_DELETE, "tmp.exe"), 1050);
        let out = drain(&mut d, 2000);
        assert!(out.is_empty(), "创建后立即删除应被完全抵消");
        assert_eq!(d.stats().cancelled, 1);
    }

    #[test]
    fn create_then_modify_stays_created() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "a"), 1000);
        d.offer(raw(1, USN_REASON_DATA_EXTEND, "a"), 1100);
        d.offer(raw(1, USN_REASON_DATA_OVERWRITE, "a"), 1200);
        let out = drain(&mut d, 2000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Created);
        assert_eq!(out[0].merged, 3);
    }

    #[test]
    fn modify_then_delete_becomes_delete() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_DATA_EXTEND, "a"), 1000);
        d.offer(raw(1, USN_REASON_FILE_DELETE, "a"), 1100);
        let out = drain(&mut d, 2000);
        assert_eq!(out[0].kind, EventKind::Deleted);
    }

    #[test]
    fn rename_pairing_produces_single_event() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_RENAME_OLD_NAME, "old.txt"), 1000);
        d.offer(raw(7, USN_REASON_RENAME_NEW_NAME, "new.txt"), 1010);
        let out = drain(&mut d, 2000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Renamed);
        assert_eq!(out[0].old_name.as_deref(), Some("old.txt"));
        assert_eq!(out[0].name, "new.txt");
        assert_eq!(d.stats().rename_pairs, 1);
    }

    #[test]
    fn create_then_rename_keeps_create_and_final_name() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_FILE_CREATE, "draft.txt"), 1000);
        d.offer(raw(7, USN_REASON_RENAME_OLD_NAME, "draft.txt"), 1010);
        d.offer(raw(7, USN_REASON_RENAME_NEW_NAME, "saved.txt"), 1020);
        let out = drain(&mut d, 2000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Created);
        assert_eq!(out[0].name, "saved.txt");
    }

    #[test]
    fn paired_recycle_rename_remains_deleted() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_RENAME_OLD_NAME, "old.txt"), 1000);
        d.offer(raw(7, USN_REASON_RENAME_NEW_NAME, "$RABCDEF.txt"), 1010);
        let out = drain(&mut d, 2000);
        assert_eq!(out[0].kind, EventKind::Deleted);
    }

    #[test]
    fn chained_renames_preserve_original_name() {
        let mut d = Debouncer::new(DebounceConfig::default());
        for (offset, reason, name) in [
            (0, USN_REASON_RENAME_OLD_NAME, "a.txt"),
            (10, USN_REASON_RENAME_NEW_NAME, "b.txt"),
            (20, USN_REASON_RENAME_OLD_NAME, "b.txt"),
            (30, USN_REASON_RENAME_NEW_NAME, "c.txt"),
        ] {
            d.offer(raw(7, reason, name), 1000 + offset);
        }
        let out = drain(&mut d, 2000);
        assert_eq!(out[0].old_name.as_deref(), Some("a.txt"));
        assert_eq!(out[0].name, "c.txt");
    }

    #[test]
    fn bounded_drain_retains_mature_events() {
        let mut d = Debouncer::new(DebounceConfig::default());
        for frn in 1..=3 {
            d.offer(raw(frn, USN_REASON_FILE_CREATE, "file.txt"), 1000);
        }
        let mut out = Vec::new();
        d.drain_matured_limited(2000, 1, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(d.pending_len(), 2);
        assert!(d.has_pending(2));
        d.drain_matured_limited(2000, 1, &mut out);
        d.drain_matured_limited(2000, 1, &mut out);
        assert_eq!(out.iter().map(|e| e.frn).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(d.pending_len(), 0);
    }

    #[test]
    fn rename_to_recycle_bin_becomes_delete() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_RENAME_NEW_NAME, "$RABCDEF.txt"), 1000);
        let out = drain(&mut d, 2000);
        assert_eq!(out[0].kind, EventKind::Deleted);
    }

    #[test]
    fn recycle_bin_metadata_is_filtered() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_FILE_CREATE, "$IABCDEF.txt"), 1000);
        assert!(drain(&mut d, 2000).is_empty());
    }

    #[test]
    fn temp_files_filtered_when_enabled() {
        let cfg = DebounceConfig {
            filter_temp_files: true,
            ..Default::default()
        };
        let mut d = Debouncer::new(cfg);
        d.offer(raw(1, USN_REASON_FILE_CREATE, "x.tmp"), 1000);
        d.offer(raw(2, USN_REASON_FILE_CREATE, "~$doc.docx"), 1000);
        d.offer(raw(3, USN_REASON_FILE_CREATE, "keep.txt"), 1000);
        let out = drain(&mut d, 2000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "keep.txt");
        assert_eq!(d.stats().filtered_temp, 2);
    }

    #[test]
    fn temp_files_kept_when_disabled() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "x.tmp"), 1000);
        assert_eq!(drain(&mut d, 2000).len(), 1);
    }

    #[test]
    fn close_only_is_ignored() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_CLOSE, "a"), 1000);
        assert!(drain(&mut d, 2000).is_empty());
    }

    #[test]
    fn different_files_are_separate_slots() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "a"), 1000);
        d.offer(raw(2, USN_REASON_FILE_CREATE, "b"), 1000);
        assert_eq!(d.pending_len(), 2);
        assert_eq!(drain(&mut d, 2000).len(), 2);
    }

    #[test]
    fn flush_all_bypasses_window() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "a"), 1000);
        let mut out = Vec::new();
        d.flush_all(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(d.pending_len(), 0);
    }

    #[test]
    fn sliding_window_extends() {
        let cfg = DebounceConfig {
            window_ms: 400,
            ..Default::default()
        };
        let mut d = Debouncer::new(cfg);
        d.offer(raw(1, USN_REASON_FILE_CREATE, "a"), 1000);
        d.offer(raw(1, USN_REASON_DATA_EXTEND, "a"), 1200);
        d.offer(raw(1, USN_REASON_DATA_EXTEND, "a"), 1400);
        // 距最后事件 300ms，不应成熟
        assert!(drain(&mut d, 1700).is_empty());
        // 距最后事件 400ms，成熟
        assert_eq!(drain(&mut d, 1800).len(), 1);
    }

    #[test]
    fn unpaired_rename_counted() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(7, USN_REASON_RENAME_NEW_NAME, "new.txt"), 1000);
        let out = drain(&mut d, 2000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Renamed);
        assert_eq!(d.stats().unpaired_renames, 1);
    }

    #[test]
    fn reindex_after_partial_drain() {
        let mut d = Debouncer::new(DebounceConfig::default());
        d.offer(raw(1, USN_REASON_FILE_CREATE, "a"), 1000);
        d.offer(raw(2, USN_REASON_FILE_CREATE, "b"), 1600);
        // 只有 1 成熟
        let out = drain(&mut d, 1600);
        assert_eq!(out.len(), 1);
        assert_eq!(d.pending_len(), 1);
        // 新事件仍能正确合并进剩余槽位
        d.offer(raw(2, USN_REASON_DATA_EXTEND, "b"), 1700);
        let out2 = drain(&mut d, 2500);
        assert_eq!(out2.len(), 1);
        assert_eq!(out2[0].merged, 2);
    }

    #[test]
    fn directory_flag_propagates() {
        let mut d = Debouncer::new(DebounceConfig::default());
        let mut r = raw(1, USN_REASON_FILE_CREATE, "newdir");
        r.file_attributes = core_io::winapi::FILE_ATTRIBUTE_DIRECTORY;
        d.offer(r, 1000);
        let out = drain(&mut d, 2000);
        assert!(out[0].is_dir);
    }
}
