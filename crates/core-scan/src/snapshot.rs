//! 扫描快照与两次快照的对比（文档 §3.1 的 F11）。
//!
//! 回答的是「这一周哪些文件变大了」这类跨时间的问题 —— 引擎只看得见**此刻**的
//! 盘，要跨时间就必须把某一时刻的清单存下来。
//!
//! # 为什么是 TSV 而不是 JSON / 二进制
//!
//! | 方案 | 问题 |
//! |---|---|
//! | JSON | 每个字段都要配一次名，`{"path":"...","size":123,"mtime":...}` 里路径占不到一半；200 万文件的快照会多出上百 MB |
//! | 二进制（rkyv/bincode） | 引擎层零第三方依赖，自己写一个序列化格式反而要维护版本与字节序 |
//! | **TSV** | 一行一条、路径在前，`sort` / `grep` / Excel 都能直接看；体积接近二进制 |
//!
//! TSV 还有一个实际好处：**快照本身就是可读的文本**，出问题时能直接打开看，
//! 而不是先写一个解析器。
//!
//! # 转义
//!
//! Windows 的文件名里**允许**出现制表符与换行（NTFS 只禁 `\ / : * ? " < > |`），
//! 所以路径不能直接塞进 TSV —— 真出现一次，整个文件的行结构就崩了，而且
//! 症状是「后半段快照全错位」，不比崩溃好查。所以写入时把 `\`、Tab、CR、LF
//! 转义成两字符序列，读取时还原。
//!
//! 反斜杠必须**第一个**转义：否则 `C:\temp` 里的 `\t` 会被后面的规则当成
//! 制表符，路径原地变形。这是这类转义最经典的错法，`escape`/`unescape`
//! 的注释里都点明了。
//!
//! # 一行一条，但**不与操作系统强绑定**
//!
//! 快照里存的路径是「扫描当时显示的路径」——即已经过 [`PathRemap`] 的真实路径。
//! 所以「用 `--root` 扫子目录」与「整卷扫描」得到的快照可以直接对比。

use crate::dedup::DupCandidate;
use core_index::Tree;
use core_io::util::human_size;
use core_monitor::PathRemap;
use std::io::{self, Read, Write};
use std::path::Path;

/// 快照格式的魔数行。读取时先认它，认不出就直接报错 ——
/// 免得把一个随便的文本文件当成快照解析，然后得出一堆"文件全没了"的结论。
pub const MAGIC: &str = "#filewatch-snapshot";
/// 当前格式版本。改字段就进位；读取端只接受自己认识的版本。
pub const VERSION: u32 = 1;

/// 进度回调的**最小步长**。
///
/// 快照这一步的量级是「百万条」：每条都回调一次的话，回调本身就比干活还贵，
/// 而且会把调用方的事件队列冲垮。所以每 `PROGRESS_STRIDE` 条报一次，
/// 收尾时再报一次满值 —— 用固定步长而不是按时间，是为了让测试可复现。
pub const PROGRESS_STRIDE: u64 = 4096;

/// 进度回调：`(已完成, 总量)`。
///
/// 总量为 0 表示这一步算不出总量（调用方应当显示"进行中"而不是百分比）。
/// 只在引擎**内部**循环里调用，不进 CLI 的输出。
pub type Progress<'a> = dyn FnMut(u64, u64) + 'a;

/// 快照里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapEntry {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

/// 某个时刻的文件清单。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// 扫描根（显示路径）
    pub root: String,
    /// 建立时间（Unix 秒）
    pub created_unix: i64,
    /// 条目，**按路径升序**。diff 依赖这个顺序做归并。
    pub entries: Vec<SnapEntry>,
    /// 因为超过上限而被丢掉的文件数（非 0 时 diff 结果会不完整，必须提示）
    pub truncated: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SnapshotOptions {
    /// 最多记录多少个文件。0 = 不限。
    pub max_entries: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions { max_entries: 0 }
    }
}

impl Snapshot {
    /// 从索引树抓一份快照。
    ///
    /// 与 `dedup::collect_candidates` 一样，路径由 `Tree::path_of` 拼出、
    /// 经 [`PathRemap`] 换成真实路径 —— 这样「快照里的路径」「变更流水里的路径」
    /// 「重复文件列表里的路径」三者是同一套，用户不会看到三种写法。
    ///
    /// 与去重不同的一点：**0 字节文件要记**。一个文件被清空也是"变了"，
    /// 去重不关心它，快照对比必须关心。
    pub fn capture(
        tree: &Tree,
        remap: Option<&PathRemap>,
        root: &str,
        created_unix: i64,
        opts: &SnapshotOptions,
    ) -> Snapshot {
        Self::capture_with(tree, remap, root, created_unix, opts, &mut |_, _| {})
    }

    /// 带进度回调的 [`Snapshot::capture`]（`done` 与 `total` 都是**文件条目数**）。
    ///
    /// `total` 需要先把「在用且非目录」的节点数一遍 —— 多一次纯遍历，
    /// 因为进度条上写"第 3 个 / 共 2,000,000 个"比写"第 3 个"有用得多，
    /// 而只报 `done` 的话前端根本不知道该画到哪儿。
    pub fn capture_with(
        tree: &Tree,
        remap: Option<&PathRemap>,
        root: &str,
        created_unix: i64,
        opts: &SnapshotOptions,
        on: &mut Progress<'_>,
    ) -> Snapshot {
        // 根路径先归一到本平台的分隔符：命令行可以写 `--root=C:/a/b`，
        // 而 `Tree::path_of` 拼出来的是反斜杠版本。不归一的话，两份快照里
        // 同一个根会显示成两种写法，对比时被判成"根不同"，归因也切不出目录。
        let root = &core_io::paths::to_native(root);
        let total = tree
            .arena
            .nodes
            .iter()
            .filter(|n| n.in_use() && !n.is_dir())
            .count() as u64;
        let mut entries = Vec::new();
        let mut truncated = 0u64;
        let mut done = 0u64;
        for (i, node) in tree.arena.nodes.iter().enumerate() {
            if !node.in_use() || node.is_dir() {
                continue;
            }
            if opts.max_entries != 0 && entries.len() >= opts.max_entries {
                truncated += 1;
            } else {
                let raw = tree.path_of(i as u32);
                let path = match remap {
                    Some(r) => r.apply(&raw),
                    None => raw,
                };
                entries.push(SnapEntry {
                    path,
                    size: node.size,
                    mtime: node.mtime,
                });
            }
            done += 1;
            if done % PROGRESS_STRIDE == 0 {
                on(done, total);
            }
        }
        on(total, total);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Snapshot {
            root: root.to_string(),
            created_unix,
            entries,
            truncated,
        }
    }

    /// 序列化成 TSV 文本。
    ///
    /// 走 [`Snapshot::write_tsv_with`]：正文只有一份实现，这里只是"写进内存"。
    pub fn to_tsv(&self) -> String {
        let mut buf = Vec::with_capacity(self.entries.len() * 72 + 128);
        // 两个 unwrap 都不会触发：写进 `Vec` 不会失败；
        // 正文由 `String` 与 ASCII 转义拼成，必然是合法 UTF-8。
        self.write_tsv_with(&mut buf, &mut |_, _| {}).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// 边序列化边写出去（`done` / `total` 是**条目数**）。
    ///
    /// 与 `to_tsv` 是同一份正文：200 万条的文本有一百多 MB，先拼成一个
    /// `String` 再写盘，等于同时拿着两份。这里直接往 `Write` 里流，
    /// 顺带能把进度报出来 —— 写盘这一步才是存快照真正慢的地方。
    pub fn write_tsv_with(&self, out: &mut dyn Write, on: &mut Progress<'_>) -> io::Result<()> {
        let mut w = io::BufWriter::new(out);
        let total = self.entries.len() as u64;
        write!(w, "{MAGIC}\t{VERSION}\n")?;
        write!(w, "#root\t{}\n", escape(&self.root))?;
        write!(w, "#created\t{}\n", self.created_unix)?;
        write!(w, "#count\t{}\n", total)?;
        if self.truncated > 0 {
            write!(w, "#truncated\t{}\n", self.truncated)?;
        }
        write!(w, "#columns\tpath\tsize\tmtime\n")?;
        // 写一行「转义规则」说明。文件本身是给人看的（这是选 TSV 的主要理由），
        // 而加了转义之后路径里会出现 `\\` —— 不解释一句，读者会以为路径真的有两个反斜杠。
        write!(w, "#note\t路径里的反斜杠写作 \\\\，制表符/回车/换行写作 \\t \\r \\n\n")?;
        let mut done = 0u64;
        for e in &self.entries {
            write!(w, "{}\t{}\t{}\n", escape(&e.path), e.size, e.mtime)?;
            done += 1;
            if done % PROGRESS_STRIDE == 0 {
                on(done, total);
            }
        }
        on(total, total);
        w.flush()
    }

    /// 从 TSV 文本解析。错误一律带行号 —— 快照文件可能有上百 MB，
    /// 只说「格式错误」等于让人自己去找。
    pub fn from_tsv(text: &str) -> Result<Snapshot, String> {
        Self::from_tsv_with(text, &mut |_, _| {})
    }

    /// 带进度回调的 [`Snapshot::from_tsv`]（`done` / `total` 是**条目**）。
    ///
    /// 200 万行的解析要好几秒，期间没有回调的话进度条就是死的 ——
    /// 用户看到的正是"读取快照卡住"。分母优先用 `#count` 表头
    /// （文件头几行就有）；没有表头就先数一遍换行符，这一遍是纯内存
    /// 顺序读，比逐行解析便宜一个量级。
    pub fn from_tsv_with(text: &str, on: &mut Progress<'_>) -> Result<Snapshot, String> {
        let total: u64 = {
            let mut declared = None;
            for line in text.lines().take(16) {
                if let Some(rest) = line.strip_prefix("#count\t") {
                    declared = rest.trim().parse::<u64>().ok();
                    break;
                }
                // 表头区在第一条数据行处结束，再往下找只会白费
                if !line.starts_with('#') {
                    break;
                }
            }
            declared.unwrap_or_else(|| text.bytes().filter(|&b| b == b'\n').count() as u64)
        };
        Self::from_tsv_counted(text, total, on)
    }

    /// [`Snapshot::from_tsv_with`] 的实现体：分母由调用方给。
    fn from_tsv_counted(text: &str, total: u64, on: &mut Progress<'_>) -> Result<Snapshot, String> {
        // **先认魔数再解析**。反过来的话，一个随便的文本文件（比如误选的
        // CSV）会先被当成正文行解析，报出「第 1 行应有 3 个制表符」——
        // 用户看到的是格式细节，而不是"你选错文件了"这个真正的问题。
        match text.lines().find(|l| !l.trim().is_empty()) {
            Some(l) if l.starts_with(MAGIC) => {
                // 版本必须是自己认识的那一版。宁可明确报错，也不要在字段
                // 含义变了之后按老偏移读出一堆"看起来差不多"的数据。
                let ver = l.split('\t').nth(1).unwrap_or("").trim();
                let v: u32 = ver
                    .parse()
                    .map_err(|_| format!("快照版本号「{ver}」不是数字"))?;
                if v != VERSION {
                    return Err(format!(
                        "快照版本 {v} 与当前支持的 {VERSION} 不一致，\
                         请用建立这份快照的那个版本打开"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "这不像是 FileWatch 的快照文件：第一行应当以「{MAGIC}」开头"
                ))
            }
        }

        let mut snap = Snapshot::default();
        let mut declared_count: Option<u64> = None;
        let mut entries: Vec<SnapEntry> = Vec::new();

        for (i, raw) in text.lines().enumerate() {
            let lineno = i + 1;
            if raw.is_empty() {
                continue;
            }
            if let Some(rest) = raw.strip_prefix('#') {
                let mut it = rest.splitn(2, '\t');
                let key = it.next().unwrap_or("");
                let val = it.next().unwrap_or("");
                match key {
                    "root" => snap.root = unescape(val),
                    "created" => {
                        snap.created_unix = val
                            .trim()
                            .parse()
                            .map_err(|_| format!("第 {lineno} 行：时间戳「{val}」不是数字"))?
                    }
                    "count" => declared_count = val.trim().parse().ok(),
                    "truncated" => snap.truncated = val.trim().parse().unwrap_or(0),
                    "columns" => {} // 说明性表头，不参与解析
                    _ => {}         // 未知的 `#` 行：向前兼容，忽略
                }
                continue;
            }
            let mut it = raw.split('\t');
            let path = it.next().unwrap_or("");
            let size = it.next();
            let mtime = it.next();
            let extra = it.next();
            let (size, mtime) = match (size, mtime, extra) {
                (Some(s), Some(m), None) => (s, m),
                _ => {
                    return Err(format!(
                        "第 {lineno} 行：应有 3 个制表符分隔的字段（path/size/mtime），\
                         实际不是 3 个。路径里的制表符本应被转义成 \\t —— \
                         这一行几乎肯定是手工改过的文件"
                    ))
                }
            };
            let size: u64 = size
                .trim()
                .parse()
                .map_err(|_| format!("第 {lineno} 行：大小「{size}」不是非负整数"))?;
            let mtime: i64 = mtime
                .trim()
                .parse()
                .map_err(|_| format!("第 {lineno} 行：修改时间「{mtime}」不是整数"))?;
            entries.push(SnapEntry {
                path: unescape(path),
                size,
                mtime,
            });
            let done = entries.len() as u64;
            if done % PROGRESS_STRIDE == 0 && done <= total {
                on(done, total);
            }
        }
        on(total, total);

        snap.entries = entries;
        // 声明的条数与实际不符 → 说明文件被改过或没写完（截断的文件最危险：
        // 后半段全部"消失"，diff 会报出一大堆假的"已删除"）。
        if let Some(c) = declared_count {
            if c != snap.entries.len() as u64 {
                return Err(format!(
                    "快照声称有 {c} 条记录，实际读到 {} 条 —— 文件很可能被截断或改过，\
                     不能用来对比",
                    snap.entries.len()
                ));
            }
        }
        snap.entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(snap)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        Self::save_with(self, path, &mut |_, _| {})
    }

    /// 带进度回调的 [`Snapshot::save`]（`done` / `total` 是**字节**）。
    ///
    /// 报字节而不是条目：这一步的耗时几乎全在写盘上（200 万条 ≈ 一百多 MB），
    /// 用户看到的"卡住"也发生在这一段。
    pub fn save_with(&self, path: &Path, on: &mut Progress<'_>) -> io::Result<()> {
        // 先写临时文件再改名：对比到一半被杀掉时，不会留下一个
        // 「看起来正常、其实少了一半」的快照当作下次的基准。
        let tmp = path.with_extension("tsv.part");
        {
            let mut f = std::fs::File::create(&tmp)?;
            self.write_tsv_with(&mut f, on)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> Result<Snapshot, String> {
        Self::load_with(path, &mut |_, _| {})
    }

    /// 带进度回调的 [`Snapshot::load`]（`done` / `total` 是**字节**）。
    ///
    /// 分块读而不是 `read_to_string`：几百 MB 的快照要读好几秒，全在内核里
    /// 打转、界面上看不出任何动静。报字节进度后，进度条走的是真实读盘量。
    ///
    /// 注意：这里的进度只覆盖**读文件**；解析这一段没有回调 —— 要把两段
    /// 分开报的调用方（界面）直接用 [`Snapshot::read_to_string_with`] +
    /// [`Snapshot::from_tsv_with`]，别硬塞进一个回调里。
    pub fn load_with(path: &Path, on: &mut Progress<'_>) -> Result<Snapshot, String> {
        let text = Self::read_to_string_with(path, on)?;
        Snapshot::from_tsv(&text).map_err(|e| format!("{}：{e}", path.display()))
    }

    /// 分块把快照读成文本（`done` / `total` 是**字节**），解析交给调用方。
    pub fn read_to_string_with(path: &Path, on: &mut Progress<'_>) -> Result<String, String> {
        let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut f = std::fs::File::open(path)
            .map_err(|e| format!("读不了快照「{}」：{e}", path.display()))?;
        let mut bytes: Vec<u8> = Vec::with_capacity(total.min(1 << 30) as usize);
        let mut buf = [0u8; 1 << 16];
        let mut done = 0u64;
        let mut last = 0u64;
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| format!("读不了快照「{}」：{e}", path.display()))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            done += n as u64;
            // 每 1 MiB 报一次：再密就只是在给事件队列添堵
            if done - last >= (1 << 20) || done >= total {
                last = done;
                on(done, total);
            }
        }
        on(total, total);
        String::from_utf8(bytes)
            .map_err(|_| format!("快照「{}」不是 UTF-8 文本，可能不是快照文件", path.display()))
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 变更的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Grew,
    Shrank,
}

impl DiffKind {
    pub fn label_cn(self) -> &'static str {
        match self {
            DiffKind::Added => "新增",
            DiffKind::Removed => "删除",
            DiffKind::Grew => "变大",
            DiffKind::Shrank => "变小",
        }
    }
    /// 稳定标识（给前端与 CSV 用，不受界面文案改动影响）。
    pub fn key(self) -> &'static str {
        match self {
            DiffKind::Added => "added",
            DiffKind::Removed => "removed",
            DiffKind::Grew => "grew",
            DiffKind::Shrank => "shrank",
        }
    }
}

/// 一行差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub path: String,
    pub kind: DiffKind,
    /// 旧快照里的大小（新增时为 0）
    pub old_size: u64,
    /// 新快照里的大小（删除时为 0）
    pub new_size: u64,
    /// `new_size - old_size`（新增为正、删除为负）
    pub delta: i64,
    pub old_mtime: i64,
    pub new_mtime: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOrder {
    /// 增量降序：直接回答「哪些文件变大了」。增量为负的行排在最后。
    ByGrowth,
    /// 变化幅度降序：变大的和变小的混在一起，找出"动得最厉害"的。
    ByAbsChange,
    /// 按路径升序：与快照文件本身的顺序一致，便于和外部 diff 工具对照。
    ByPath,
}

#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// 增量小于这个字节数的变化不列入 `rows`（仍计入统计）。
    /// 默认 0：不替用户决定什么算"噪声"。
    pub min_delta: u64,
    /// `rows` 最多多少行（0 = 不限）
    pub top: usize,
    pub order: DiffOrder,
    /// 要不要顺带算增长归因（`None` = 不算）。
    ///
    /// 归因必须用**全量**变化行，而全量只在归并结束时存在一次（`rows` 随后会被
    /// `top` 截断），所以它只能在这里开，不能事后补算 —— 见 [`crate::attribution`]。
    pub attr: Option<crate::attribution::AttrOptions>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions {
            min_delta: 0,
            top: 100,
            order: DiffOrder::ByGrowth,
            attr: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffReport {
    /// 参与对比的两个快照各有多少条
    pub old_total: u64,
    pub new_total: u64,
    /// 两侧都有、且信息完全一致
    pub same: u64,
    /// 两侧都有、但大小或时间变了
    pub changed: u64,
    pub added: u64,
    pub removed: u64,
    pub grew: u64,
    pub shrank: u64,
    /// 新增文件带来的字节数
    pub added_bytes: u64,
    /// 被删除文件原先占的字节数
    pub removed_bytes: u64,
    /// 变大那一部分**增加**的字节数（只算增量，不含原体积）
    pub grew_bytes: u64,
    /// 变小那一部分**减少**的字节数
    pub shrank_bytes: u64,
    /// 净变化 = grew + added − shrank − removed
    pub net_delta: i64,
    /// 过滤/截断之后的行；`rows_total` 是符合条件的总行数
    pub rows: Vec<DiffRow>,
    pub rows_total: u64,
    /// 快照本身是否被截断过（截断的快照对比结果不可信，必须提示）
    pub old_truncated: u64,
    pub new_truncated: u64,
    /// 增长归因（`DiffOptions::attr` 为 `Some` 时才有）。
    ///
    /// 与 `rows` 的截断**无关**：它算的是全量变化行，所以"前 300 行"和
    /// "哪些目录在长"是两份互补的答案。
    pub attribution: Option<crate::attribution::Attribution>,
}

/// 对比两份快照。
///
/// 两侧都按路径升序，所以用**归并**一次扫完（O(n+m)），不建哈希表 ——
/// 200 万条的哈希表在 32 位 Rust 上要几百 MB，而归并只要两个游标。
pub fn diff(old: &Snapshot, new: &Snapshot, opts: &DiffOptions) -> DiffReport {
    diff_with(old, new, opts, &mut |_, _| {})
}

/// 带进度回调的 [`diff`]（`done` / `total` 是**归并游标走过的条目数**，
/// 上限为两侧条目数之和）。
pub fn diff_with(
    old: &Snapshot,
    new: &Snapshot,
    opts: &DiffOptions,
    on: &mut Progress<'_>,
) -> DiffReport {
    let total = (old.entries.len() + new.entries.len()) as u64;
    let mut rep = DiffReport {
        old_total: old.entries.len() as u64,
        new_total: new.entries.len() as u64,
        old_truncated: old.truncated,
        new_truncated: new.truncated,
        ..Default::default()
    };
    let mut all: Vec<DiffRow> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut last = 0u64;
    while i < old.entries.len() || j < new.entries.len() {
        let ord = match (old.entries.get(i), new.entries.get(j)) {
            (Some(a), Some(b)) => a.path.cmp(&b.path),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => break,
        };
        match ord {
            std::cmp::Ordering::Less => {
                let a = &old.entries[i];
                rep.removed += 1;
                rep.removed_bytes += a.size;
                push(&mut all, Some(a), None, DiffKind::Removed, opts);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                let b = &new.entries[j];
                rep.added += 1;
                rep.added_bytes += b.size;
                push(&mut all, None, Some(b), DiffKind::Added, opts);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let a = &old.entries[i];
                let b = &new.entries[j];
                if a.size == b.size && a.mtime == b.mtime {
                    rep.same += 1;
                } else {
                    rep.changed += 1;
                    let kind = if b.size >= a.size {
                        // 时间变了但体积没变也归到「变大」会让人误解 ——
                        // 统一按体积判断，体积相等时就是「变大（增量 0）」
                        DiffKind::Grew
                    } else {
                        DiffKind::Shrank
                    };
                    match kind {
                        DiffKind::Grew => {
                            rep.grew += 1;
                            rep.grew_bytes += b.size.saturating_sub(a.size);
                        }
                        _ => {
                            rep.shrank += 1;
                            rep.shrank_bytes += a.size.saturating_sub(b.size);
                        }
                    }
                    push(&mut all, Some(a), Some(b), kind, opts);
                }
                i += 1;
                j += 1;
            }
        }
        // 一次迭代可能推进 1 个也可能 2 个游标，所以按"距上次汇报走了多远"
        // 判断，而不是取模 —— 取模会被跳过，进度条就少一档。
        let done = (i + j) as u64;
        if done - last >= PROGRESS_STRIDE {
            last = done;
            on(done, total);
        }
    }
    on(total, total);

    rep.net_delta = rep.grew_bytes as i64 + rep.added_bytes as i64
        - rep.shrank_bytes as i64
        - rep.removed_bytes as i64;
    rep.rows_total = all.len() as u64;

    // 归因必须在 `all` 被排序/截断**之前**算：它要全量，而全量只在这里存在。
    // 此刻 `all` 还是归并出来的路径顺序（同一目录的行相邻），对归因的
    // HashMap 也更友好。
    if let Some(ao) = opts.attr {
        let root = if !new.root.is_empty() { &new.root } else { &old.root };
        rep.attribution = Some(crate::attribution::build(root, &all, &ao));
    }

    match opts.order {
        DiffOrder::ByGrowth => all.sort_by(|a, b| {
            b.delta
                .cmp(&a.delta)
                .then(a.path.cmp(&b.path))
        }),
        DiffOrder::ByAbsChange => all.sort_by(|a, b| {
            b.delta
                .abs()
                .cmp(&a.delta.abs())
                .then(a.path.cmp(&b.path))
        }),
        DiffOrder::ByPath => all.sort_by(|a, b| a.path.cmp(&b.path)),
    }
    if opts.top != 0 && all.len() > opts.top {
        all.truncate(opts.top);
    }
    rep.rows = all;
    rep
}

/// 造一条差异行：`old` / `new` 至少有一个存在。
///
/// 三个分支（删除 / 新增 / 变化）都要造 [`DiffRow`]，所以收成一个函数 ——
/// 少一处就是一个少犯一次「新增行忘了把 old_size 置 0」这类错的地方。
/// 新增行如果照抄了新快照的体积，会出现「新增 3 KB 的文件，增量却是 0」
/// 这种自相矛盾的行。
fn push(
    out: &mut Vec<DiffRow>,
    old_e: Option<&SnapEntry>,
    new_e: Option<&SnapEntry>,
    kind: DiffKind,
    opts: &DiffOptions,
) {
    let old_size = old_e.map(|e| e.size).unwrap_or(0);
    let new_size = new_e.map(|e| e.size).unwrap_or(0);
    let delta = new_size as i64 - old_size as i64;
    if delta.unsigned_abs() < opts.min_delta {
        return;
    }
    // 路径取"新"的那一侧：新增行没有旧路径；改名在快照里就是「一删一增」，
    // 各自用自己那一侧的路径才对得上。
    let path = new_e
        .or(old_e)
        .map(|e| e.path.clone())
        .unwrap_or_default();
    out.push(DiffRow {
        path,
        kind,
        old_size,
        new_size,
        delta,
        old_mtime: old_e.map(|e| e.mtime).unwrap_or(0),
        new_mtime: new_e.map(|e| e.mtime).unwrap_or(0),
    });
}

/// 一行摘要（CLI 与界面共用）。
pub fn summary_cn(rep: &DiffReport) -> String {
    let mut s = format!(
        "新增 {} 个（+{}）/ 删除 {} 个（−{}）/ 变大 {} 个（+{}）/ 变小 {} 个（−{}），净变化 {}",
        rep.added,
        human_size(rep.added_bytes),
        rep.removed,
        human_size(rep.removed_bytes),
        rep.grew,
        human_size(rep.grew_bytes),
        rep.shrank,
        human_size(rep.shrank_bytes),
        signed_size(rep.net_delta),
    );
    if rep.old_truncated > 0 || rep.new_truncated > 0 {
        s.push_str(&format!(
            "　（注意：快照被截断过 —— 旧 {} / 新 {} 个文件没记进去，对比结果不完整）",
            rep.old_truncated, rep.new_truncated
        ));
    }
    s
}

/// 带正负号的体积（`+1.20 MB` / `−390.62 KB`）。
///
/// 用真正的减号 U+2212 而不是连字符：在等宽字体里它与加号等宽，
/// 一列数字才对得齐。
pub fn signed_size(v: i64) -> String {
    if v < 0 {
        format!("−{}", human_size(v.unsigned_abs()))
    } else {
        format!("+{}", human_size(v as u64))
    }
}

/// 差异行 → CSV（CLI 与界面共用）。
pub fn rows_to_csv(rows: &[DiffRow]) -> String {
    let mut out = String::from("kind,delta,old_size,new_size,path\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            r.kind.key(),
            r.delta,
            r.old_size,
            r.new_size,
            csv_field(&r.path)
        ));
    }
    out
}

pub fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 一条快照记录 → 去重的候选（两者形状相同，转一下就能复用同一套判据）。
pub fn entry_as_candidate(e: &SnapEntry) -> DupCandidate {
    DupCandidate {
        path: e.path.clone(),
        size: e.size,
        mtime: e.mtime,
    }
}

// ── 转义 ────────────────────────────────────────────────────────────────────

/// 把路径里的 `\`、Tab、CR、LF 转成两字符序列。
///
/// **顺序要紧**：反斜杠必须第一个处理。先处理 `\t` 的话，`C:\temp` 会变成
/// `C:<TAB>emp`，路径原地变形，而且它读回来还是「合法」的一行 —— 只是内容错了。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// [`escape`] 的逆。无法识别的转义**原样保留**（连反斜杠一起），
/// 这样「转义规则将来加了新字符」也不会把老文件读坏。
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 从快照重建一棵可查询的索引树（`--from-snapshot`）。
///
/// 快照只记 `路径 / 体积 / 修改时间`，**没有父子关系**，所以这里按路径分量重新
/// 搭一棵树：目录节点的体积交给 [`core_index::aggregate::aggregate`] 聚合，
/// 与真实扫描出来的是同一个口径。
///
/// 重建出来的树**以盘符（卷键）为根**，不是以 `snapshot.root` 为根：快照里的
/// 路径本来就是绝对路径，按卷根挂回去之后 `Tree::path_of` 拼出来的就是原路径，
/// 调用方**不需要**再走一次 `PathRemap`（再换一次会拼错）。
///
/// 两个已知取舍（都要在输出里说，不能假装一样）：
///
/// * 快照不存 `alloc`（占用大小），这里按逻辑大小填 —— 稀疏/压缩文件的"占用"
///   在快照模式下与真实扫描有差异；
/// * 快照只记文件，**空目录不会出现**在重建结果里（真实扫描看得见它们）。
///
/// 返回 `(树, 快照根的节点下标)`。第二个值不能省：重建出来的树以**卷根**为根，
/// 而"扫描根"可能深好几层（`C://projects//a`）。`rules` 的样本是"扫描根 + 它的一级
/// 子目录"，拿卷根当样本会得出完全不同的结论（实测：变成 `C://Users` 与 `C://`）。
pub fn tree_from_snapshot(snap: &Snapshot, drive: &str) -> Result<(Tree, u32), String> {
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};
    use std::collections::HashMap;

    const ROOT: u32 = 5;
    let prefix = core_io::paths::root_prefix(drive);
    let mut tree = Tree::new(drive, snap.entries.len().saturating_mul(2).max(64));
    tree.arena.ensure(ROOT);
    tree.dir_file_counts.resize(ROOT as usize + 1, 0);
    {
        let off = tree.names.intern_str(".");
        tree.arena.nodes[ROOT as usize] = Node {
            parent: ROOT,
            seq: 0,
            flags: NODE_IN_USE | NODE_DIR,
            size: 0,
            alloc: 0,
            mtime: 0,
            name: off,
        };
    }
    tree.root = ROOT;

    // 快照根在树里对应的键（`/users/hy/proj` 这种小写形式）
    let root_key = {
        let rel = core_io::paths::strip_root_prefix(&snap.root, &prefix).unwrap_or("");
        let mut k = String::new();
        for p in rel.split(['\\', '/']).filter(|s| !s.is_empty()) {
            k.push('/');
            k.push_str(&p.to_lowercase());
        }
        k
    };
    let mut sample_root: u32 = ROOT;

    let mut next: u32 = ROOT + 1;
    // 键 = 小写相对路径（`/a/b`）。NTFS 大小写不敏感，两行 `C:\A\x` 与 `C:\a\y`
    // 必须挂到同一个目录节点上，否则目录会被拆成两份、体积各算一半。
    let mut dirs: HashMap<String, u32> = HashMap::new();
    let mut files = 0u64;

    for e in &snap.entries {
        // 快照里的路径是绝对路径；剥掉卷根就是相对部分。
        // `strip_root_prefix` 容忍分隔符混用（`C:/a` 与 `C:\a` 是同一条）。
        let rel = match core_io::paths::strip_root_prefix(&e.path, &prefix) {
            Some(r) => r,
            // 不在这个卷下的行：快照被手工改过，或者卷键传错了。跳过而不是
            // 挂到根上 —— 挂上去会凭空多出一堆"卷根下的怪目录"。
            None => continue,
        };
        let parts: Vec<&str> = rel
            .split(['\\', '/'])
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }

        let mut parent = ROOT;
        let mut acc = String::new();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            acc.push('/');
            acc.push_str(&part.to_lowercase());
            if last {
                if next == u32::MAX {
                    break;
                }
                let idx = next;
                next += 1;
                tree.arena.ensure(idx);
                if idx as usize >= tree.dir_file_counts.len() {
                    tree.dir_file_counts.resize(idx as usize + 1, 0);
                }
                let off = tree.names.intern_str(part);
                tree.arena.nodes[idx as usize] = Node {
                    parent,
                    seq: 0,
                    flags: NODE_IN_USE,
                    size: e.size,
                    // 快照没有占用大小，按逻辑大小填（见文档）
                    alloc: e.size,
                    mtime: e.mtime,
                    name: off,
                };
                files += 1;
            } else {
                parent = match dirs.get(&acc) {
                    Some(&idx) => idx,
                    None => {
                        if next == u32::MAX {
                            break;
                        }
                        let idx = next;
                        next += 1;
                        tree.arena.ensure(idx);
                        if idx as usize >= tree.dir_file_counts.len() {
                            tree.dir_file_counts.resize(idx as usize + 1, 0);
                        }
                        let off = tree.names.intern_str(part);
                        tree.arena.nodes[idx as usize] = Node {
                            parent,
                            seq: 0,
                            flags: NODE_IN_USE | NODE_DIR,
                            size: 0,
                            alloc: 0,
                            mtime: 0,
                            name: off,
                        };
                        if !root_key.is_empty() && acc == root_key {
                            sample_root = idx;
                        }
                        dirs.insert(acc.clone(), idx);
                        idx
                    }
                };
            }
        }
    }

    core_index::aggregate::aggregate(&mut tree);
    tree.file_count = files;
    Ok((tree, sample_root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_index::arena::{Node, NODE_DIR, NODE_IN_USE};

    fn entry(path: &str, size: u64, mtime: i64) -> SnapEntry {
        SnapEntry {
            path: path.to_string(),
            size,
            mtime,
        }
    }

    fn snap(entries: Vec<SnapEntry>) -> Snapshot {
        let mut entries = entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Snapshot {
            root: r"C:\".to_string(),
            created_unix: 1_700_000_000,
            entries,
            truncated: 0,
        }
    }

    // ── 快照 → 树（`--from-snapshot`）──────────────────────────────────────

    /// 路径必须能原样拼回来，目录体积按子项聚合，大小写不同的同一目录要挂到一起。
    #[test]
    fn tree_from_snapshot_round_trips_paths() {
        let s = snap(vec![
            entry(r"C:\data\a\x.txt", 100, 111),
            entry(r"C:\data\a\y.txt", 200, 222),
            entry(r"C:\data\b.txt", 50, 333),
            // 大小写不同 = NTFS 上的同一个目录：不能拆成两份，否则体积各算一半
            entry(r"C:\DATA\A\z.txt", 7, 444),
        ]);
        let (t, sample_root) = tree_from_snapshot(&s, "C:").unwrap();
        assert_eq!(t.file_count, 4);
        assert_eq!(t.root, 5, "树以卷根为根");
        assert!(t.path_of(sample_root).eq_ignore_ascii_case("C:\\"),
                "快照根是卷根时，样本根就是卷根：{}", t.path_of(sample_root));

        let paths: Vec<String> = (0..t.arena.len() as u32)
            .filter(|&i| t.node(i).map(|n| n.is_file()).unwrap_or(false))
            .map(|i| t.path_of(i))
            .collect();
        // 大小写以**先出现的写法**为准（NTFS 不敏感）：这里只断言路径能拼回来
        assert!(paths.iter().any(|p| p.eq_ignore_ascii_case(r"C:\data\b.txt")), "{paths:?}");
        assert_eq!(paths.len(), 4, "{paths:?}");

        let dir_of = |want: &str| {
            (0..t.arena.len() as u32).find(|&i| {
                t.node(i).map(|n| n.is_dir()).unwrap_or(false)
                    && t.path_of(i).eq_ignore_ascii_case(want)
            })
        };
        let a = dir_of(r"C:\data\a").expect("应当有 data\\a 这个目录");
        assert_eq!(t.node(a).unwrap().size, 307, "100 + 200 + 7");
        let data = dir_of(r"C:\data").expect("应当有 data");
        assert_eq!(t.node(data).unwrap().size, 357);
        let roots = (0..t.arena.len() as u32)
            .filter(|&i| {
                t.node(i).map(|n| n.is_dir()).unwrap_or(false)
                    && t.path_of(i).eq_ignore_ascii_case(r"C:\data\a")
            })
            .count();
        assert_eq!(roots, 1, "大小写不同的同一目录只能有一个节点");
    }

    /// 不在这个卷下的行要**跳过**：挂到根上会凭空多出一堆"卷根下的怪目录"。
    #[test]
    fn tree_from_snapshot_skips_foreign_paths() {
        let s = snap(vec![
            entry(r"D:\other\x.txt", 10, 1),
            entry(r"C:\ok\y.txt", 20, 2),
        ]);
        let (t, _) = tree_from_snapshot(&s, "C:").unwrap();
        assert_eq!(t.file_count, 1, "D: 那行必须被跳过");
    }

    fn opts(f: impl FnOnce(&mut DiffOptions)) -> DiffOptions {
        let mut o = DiffOptions {
            top: 0,
            ..Default::default()
        };
        f(&mut o);
        o
    }

    // ── 转义 ────────────────────────────────────────────────────────────────

    /// 反斜杠必须先转义。写反了的话 `C:\temp` 会变成 `C:<TAB>emp`，
    /// 而读回来仍然"合法" —— 只是路径错了。所以这条要逐字符断言。
    #[test]
    fn escape_handles_backslash_before_tab() {
        assert_eq!(escape(r"C:\temp"), r"C:\\temp");
        assert_eq!(escape("a\tb"), "a\\tb");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\rb"), "a\\rb");
        // 关键用例：一个**真的**制表符，与一个"反斜杠 + 字母 t"
        assert_eq!(escape("x\ty"), "x\\ty");
        assert_eq!(escape(r"x\ty"), r"x\\ty");
        assert_ne!(escape("x\ty"), escape(r"x\ty"), "两者绝不能转成同一个东西");
    }

    #[test]
    fn escape_round_trips_everything() {
        for s in [
            r"C:\Windows\System32",
            "tab\there",
            "line\nbreak",
            "carriage\rreturn",
            r"\\server\share\file.txt",
            "混合\\路径\t带\n怪字符\r",
            "尾部反斜杠\\",
            "",
        ] {
            assert_eq!(unescape(&escape(s)), s, "往返失败：{s:?}");
        }
    }

    /// 未知转义原样保留：将来加了新转义字符，老文件也不会被读坏。
    #[test]
    fn unknown_escape_is_kept_literally() {
        assert_eq!(unescape(r"a\qb"), r"a\qb");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    // ── 快照往返 ────────────────────────────────────────────────────────────

    #[test]
    fn tsv_round_trip_preserves_entries() {
        let s = snap(vec![
            entry(r"C:\a.txt", 100, 1),
            entry("C:\\奇怪\t名.bin", 200, 2),
            entry(r"D:\deep\nest\ed\file.dat", 300, 3),
        ]);
        let text = s.to_tsv();
        let back = Snapshot::from_tsv(&text).unwrap();
        assert_eq!(back.entries, s.entries);
        assert_eq!(back.root, s.root);
        assert_eq!(back.created_unix, s.created_unix);
        // 每一行必须是 3 个字段 —— 转义漏了的话这里会多出字段
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert_eq!(line.matches('\t').count(), 2, "行字段数不对：{line:?}");
        }
    }

    #[test]
    fn tsv_has_magic_and_columns_header() {
        let text = snap(vec![entry(r"C:\a.txt", 1, 1)]).to_tsv();
        let first = text.lines().next().unwrap();
        assert_eq!(first, format!("{MAGIC}\t{VERSION}"));
        assert!(text.contains("#columns\tpath\tsize\tmtime"));
    }

    #[test]
    fn parsing_rejects_a_foreign_file() {
        let e = Snapshot::from_tsv("hello world\nthis is not a snapshot\n").unwrap_err();
        assert!(e.contains("不像是 FileWatch 的快照"), "{e}");
    }

    #[test]
    fn parsing_rejects_version_mismatch() {
        let text = format!("{MAGIC}\t99\n#count\t0\n");
        let e = Snapshot::from_tsv(&text).unwrap_err();
        assert!(e.contains("版本"), "{e}");
    }

    /// 声明条数与实际不符 = 文件被截断过。**必须报错**：截断的快照会让
    /// 后半段文件全部被判成"已删除"，用户可能据此去清理并不存在的垃圾。
    #[test]
    fn parsing_rejects_truncated_snapshot() {
        let text = format!(
            "{MAGIC}\t{VERSION}\n#count\t3\n{}",
            "C:\\a.txt\t1\t1\nC:\\b.txt\t2\t2\n"
        );
        let e = Snapshot::from_tsv(&text).unwrap_err();
        assert!(e.contains("截断"), "{e}");
    }

    #[test]
    fn parsing_reports_line_number_for_bad_field_count() {
        let text = format!("{MAGIC}\t{VERSION}\n#count\t1\nC:\\a.txt\t1\n");
        let e = Snapshot::from_tsv(&text).unwrap_err();
        assert!(e.contains("第 3 行"), "{e}");
        assert!(e.contains("3 个制表符"), "{e}");
    }

    #[test]
    fn parsing_reports_line_number_for_bad_number() {
        let text = format!("{MAGIC}\t{VERSION}\n#count\t1\nC:\\a.txt\tabc\t1\n");
        let e = Snapshot::from_tsv(&text).unwrap_err();
        assert!(e.contains("第 3 行"), "{e}");
        assert!(e.contains("abc"), "{e}");
    }

    #[test]
    fn parsing_tolerates_blank_lines_and_unknown_headers() {
        let text = format!(
            "{MAGIC}\t{VERSION}\n#root\tC:\\\n\n#future\twhatever\n#count\t1\nC:\\a.txt\t9\t7\n"
        );
        let s = Snapshot::from_tsv(&text).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].size, 9);
    }

    #[test]
    fn save_then_load_on_disk() {
        let owned = core_io::test_dir::TestDir::new("snapshot-roundtrip").unwrap();
        let p = owned.path().join("snapshot.tsv");
        let s = snap(vec![entry(r"C:\x\y.txt", 42, 5)]);
        s.save(&p).unwrap();
        assert!(p.exists());
        let back = Snapshot::load(&p).unwrap();
        assert_eq!(back.entries, s.entries);
        // 临时 `.part` 必须已经被改名，不能留在旁边
        assert!(!p.with_extension("tsv.part").exists());
    }

    /// 流式写出的正文必须与 `to_tsv` **一字不差**。
    ///
    /// 这两条路一旦分叉，界面存出来的快照和 CLI 存出来的就不一样 ——
    /// 而差异只会在某些字符（转义过的路径）上出现，很难靠肉眼发现。
    #[test]
    fn streaming_write_matches_to_tsv() {
        let s = snap(vec![
            entry(r"C:\a\b.txt", 42, 5),
            entry("C:\tc\td.txt", 7, 6),
            entry(r"C:\e\\f", 0, 7),
        ]);
        let mut buf = Vec::new();
        s.write_tsv_with(&mut buf, &mut |_, _| {}).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), s.to_tsv());
    }

    /// 进度回调必须**单调不减**、且以满值收尾。
    ///
    /// 少了收尾那一次，界面上的进度条会停在 99%（最后不足一个 stride 的部分
    /// 永远报不出来），用户就只能猜"是不是完了"。
    #[test]
    fn write_save_and_load_report_monotonic_progress_ending_at_total() {
        let owned = core_io::test_dir::TestDir::new("snapshot-progress").unwrap();
        let p = owned.path().join("snapshot.tsv");
        let s = snap((0..40).map(|i| entry(&format!(r"C:\f{i}.bin"), i, i as i64)).collect());

        let mut seen: Vec<(u64, u64)> = Vec::new();
        s.save_with(&p, &mut |d, t| seen.push((d, t))).unwrap();
        assert!(!seen.is_empty(), "写盘必须报进度");
        assert_eq!(seen.last().unwrap().0, seen.last().unwrap().1, "收尾要报满值");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "进度不能倒退：{seen:?}");

        let mut rd: Vec<(u64, u64)> = Vec::new();
        let back = Snapshot::load_with(&p, &mut |d, t| rd.push((d, t))).unwrap();
        assert_eq!(back.entries, s.entries);
        assert!(!rd.is_empty(), "读盘必须报进度");
        assert_eq!(rd.last().unwrap().0, rd.last().unwrap().1, "收尾要报满值");
        assert!(rd.last().unwrap().1 > 0, "总量应当是文件字节数");
        assert!(rd.windows(2).all(|w| w[0].0 <= w[1].0), "进度不能倒退：{rd:?}");
    }

    #[test]
    fn load_reports_the_path_on_failure() {
        let owned = core_io::test_dir::TestDir::new("snapshot-missing").unwrap();
        let p = owned.path().join("fw_snap_definitely_missing_12345.tsv");
        let e = Snapshot::load(&p).unwrap_err();
        assert!(e.contains("12345"), "错误里要带路径，否则用户不知道是哪个文件：{e}");
    }

    #[test]
    fn from_tsv_with_reports_progress_and_ends_at_declared_total() {
        // 1 万条，超过一个 PROGRESS_STRIDE，回调必然触发多次
        let entries: Vec<SnapEntry> =
            (0..10_000).map(|i| entry(&format!("C:\\f{i:05}"), i, 1_700_000_000)).collect();
        let s = snap(entries);
        let text = s.to_tsv();
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let got = Snapshot::from_tsv_with(&text, &mut |d, t| {
            assert!(seen.last().map(|last| d > last.0).unwrap_or(true), "进度不能原地踏步或倒退");
            seen.push((d, t));
        })
        .unwrap();
        assert_eq!(got.len(), s.len());
        let total = seen.last().unwrap().1;
        assert_eq!(total, 10_000, "分母应是 #count 表头声明的条数");
        assert_eq!(seen.last().unwrap().0, total, "收尾必须报满值，条不能停在 99%");
        assert!(seen.len() >= 2, "1 万条只报一次 = 没起作用：{seen:?}");
        // 解析结果必须与不带进度的版本完全一致
        assert_eq!(got.entries, Snapshot::from_tsv(&text).unwrap().entries);
    }

    #[test]
    fn from_tsv_with_falls_back_to_line_count_without_declared_total() {
        let entries: Vec<SnapEntry> =
            (0..300).map(|i| entry(&format!("C:\\f{i:03}"), i, 1_700_000_000)).collect();
        let text = snap(entries).to_tsv();
        // 抹掉 #count 行：分母退回换行符计数，进度照样要走完
        let text: String =
            text.lines().filter(|l| !l.starts_with("#count\t")).map(|l| format!("{l}\n")).collect();
        let mut last = (0u64, 0u64);
        let got = Snapshot::from_tsv_with(&text, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(got.len(), 300);
        assert_eq!(last.0, last.1, "收尾必须报满值");
        assert!(last.1 >= 300, "兜底分母不能小于实际条数：{}", last.1);
    }

    // ── 从索引树抓取 ────────────────────────────────────────────────────────

    fn tree() -> Tree {
        let mut t = Tree::new("C:", 32);
        t.arena.ensure(20);
        let set = |t: &mut Tree, idx: u32, parent: u32, name: &str, dir: bool, size: u64| {
            let off = t.names.intern_str(name);
            t.arena.nodes[idx as usize] = Node {
                parent,
                seq: 1,
                flags: NODE_IN_USE | if dir { NODE_DIR } else { 0 },
                size,
                alloc: size,
                mtime: idx as i64 * 10,
                name: off,
            };
        };
        set(&mut t, 5, 5, ".", true, 0);
        set(&mut t, 10, 5, "docs", true, 0);
        set(&mut t, 11, 10, "a.bin", false, 5000);
        set(&mut t, 12, 10, "zero.dat", false, 0);
        t.root = 5;
        t
    }

    /// 与去重相反：**0 字节文件要记进来**。文件被清空也是一种变化。
    #[test]
    fn capture_includes_zero_size_files_but_not_dirs() {
        let t = tree();
        let s = Snapshot::capture(&t, None, "C:", 100, &SnapshotOptions::default());
        let paths: Vec<&str> = s.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&r"C:\docs\a.bin"));
        assert!(paths.contains(&r"C:\docs\zero.dat"), "0 字节文件必须记");
        assert!(!paths.iter().any(|p| p.ends_with("docs")));
        // 必须按路径升序：diff 的归并依赖这一点
        let mut sorted = s.entries.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(s.entries, sorted);
    }

    #[test]
    fn capture_honours_max_entries() {
        let t = tree();
        let o = SnapshotOptions { max_entries: 1 };
        let s = Snapshot::capture(&t, None, "C:", 100, &o);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.truncated, 1);
        assert!(s.to_tsv().contains("#truncated\t1"));
    }

    #[test]
    fn capture_applies_remap() {
        let t = tree();
        let r = PathRemap::new("C:", r"D:\projects").unwrap();
        let s = Snapshot::capture(&t, Some(&r), r"D:\projects", 100, &SnapshotOptions::default());
        assert!(s
            .entries
            .iter()
            .any(|e| e.path == r"D:\projects\docs\a.bin"));
        assert_eq!(s.root, r"D:\projects");
    }

    /// `capture_with` 的 `total` 必须排除目录与未使用的槽位，否则进度条永远走不满。
    #[test]
    fn capture_with_reports_files_only_and_ends_at_total() {
        let t = tree();
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let s = Snapshot::capture_with(
            &t,
            None,
            "C:",
            100,
            &SnapshotOptions::default(),
            &mut |d, tt| seen.push((d, tt)),
        );
        // 树里只有 2 个文件（a.bin / zero.dat），目录不算
        assert_eq!(seen.last().unwrap().1, 2, "total 只数文件：{seen:?}");
        assert_eq!(seen.last().unwrap().0, 2, "收尾要报满值：{seen:?}");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "进度不能倒退：{seen:?}");
        assert_eq!(s.entries.len(), 2);
    }

    // ── diff ────────────────────────────────────────────────────────────────

    #[test]
    fn diff_classifies_all_four_kinds() {
        let old = snap(vec![
            entry("a", 100, 1),  // 变大
            entry("b", 200, 1),  // 变小
            entry("c", 300, 1),  // 删除
            entry("d", 400, 1),  // 不变
        ]);
        let new = snap(vec![
            entry("a", 150, 2),
            entry("b", 120, 2),
            entry("d", 400, 1),
            entry("e", 50, 3), // 新增
        ]);
        let r = diff(&old, &new, &opts(|_| {}));
        assert_eq!(r.grew, 1);
        assert_eq!(r.shrank, 1);
        assert_eq!(r.removed, 1);
        assert_eq!(r.added, 1);
        assert_eq!(r.same, 1);
        assert_eq!(r.changed, 2);
        assert_eq!(r.grew_bytes, 50);
        assert_eq!(r.shrank_bytes, 80);
        assert_eq!(r.added_bytes, 50);
        assert_eq!(r.removed_bytes, 300);
        // 净变化 = 50 + 50 − 80 − 300
        assert_eq!(r.net_delta, -280);
        assert_eq!(r.rows.len(), 4, "不变的那条不进差异行");
    }

    /// `diff_with` 与 `diff` 必须是同一份结果，只多一个进度回调。
    ///
    /// 两条路分叉的话，界面上会看到和终端里不一样的"新增 / 删除"数量 ——
    /// 而用户拿它去删文件，错一个数就是误删。
    #[test]
    fn diff_with_matches_diff_and_reports_progress() {
        let old = snap(vec![entry("a", 100, 1), entry("c", 300, 1)]);
        let new = snap(vec![entry("a", 150, 2), entry("e", 50, 3)]);
        let plain = diff(&old, &new, &opts(|_| {}));

        let mut seen: Vec<(u64, u64)> = Vec::new();
        let got = diff_with(&old, &new, &opts(|_| {}), &mut |d, t| seen.push((d, t)));
        assert_eq!(got.added, plain.added);
        assert_eq!(got.removed, plain.removed);
        assert_eq!(got.rows, plain.rows);
        // 两侧各 2 条 → 总量 4；收尾必须正好落在满值
        assert_eq!(seen.last().unwrap(), &(4, 4), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "进度不能倒退：{seen:?}");
    }

    #[test]
    fn diff_rows_are_sorted_by_growth_then_path() {
        let old = snap(vec![entry("a", 1, 1), entry("b", 1, 1), entry("c", 1, 1)]);
        let new = snap(vec![entry("a", 11, 2), entry("b", 11, 2), entry("c", 100, 2)]);
        let r = diff(&old, &new, &opts(|_| {}));
        let order: Vec<&str> = r.rows.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(order, vec!["c", "a", "b"], "先按增量降序，增量相同按路径");
        assert_eq!(r.rows[0].delta, 99);
    }

    #[test]
    fn diff_can_order_by_absolute_change() {
        let old = snap(vec![entry("grow", 0, 1), entry("drop", 100, 1)]);
        let new = snap(vec![entry("grow", 10, 2), entry("drop", 0, 2)]);
        let r = diff(&old, &new, &opts(|o| o.order = DiffOrder::ByAbsChange));
        assert_eq!(r.rows.len(), 2);
        // 默认顺序（按增量）会把 +10 放在最前；按幅度则是 −100 那个最前
        assert_eq!(r.rows[0].path, "drop");
        assert_eq!(r.rows[0].delta, -100);
        let by_growth = diff(&old, &new, &opts(|_| {}));
        assert_eq!(by_growth.rows[0].path, "grow");
    }

    #[test]
    fn diff_min_delta_filters_rows_but_not_totals() {
        let old = snap(vec![entry("a", 100, 1), entry("b", 100, 1)]);
        let new = snap(vec![entry("a", 101, 2), entry("b", 500, 2)]);
        let r = diff(&old, &new, &opts(|o| o.min_delta = 50));
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].path, "b");
        // 统计不受过滤影响：两条都算了
        assert_eq!(r.changed, 2);
        assert_eq!(r.grew_bytes, 401);
        assert_eq!(r.rows_total, 1);
    }

    #[test]
    fn diff_top_limits_rows_but_keeps_rows_total() {
        let old: Vec<SnapEntry> = (0..10).map(|i| entry(&format!("f{i:02}"), 0, 1)).collect();
        let new: Vec<SnapEntry> = (0..10)
            .map(|i| entry(&format!("f{i:02}"), (i + 1) * 10, 2))
            .collect();
        let r = diff(&snap(old), &snap(new), &opts(|o| o.top = 3));
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows_total, 10);
        assert_eq!(r.rows[0].path, "f09", "最大的增量在最前");
    }

    /// 只在 mtime 上变化、体积不变的文件：必须被算作"变了"（否则改了内容又
    /// 改回同样大小的文件会被漏掉），但不能把净变化带偏。
    #[test]
    fn mtime_only_change_counts_as_changed_with_zero_delta() {
        let old = snap(vec![entry("a", 100, 1)]);
        let new = snap(vec![entry("a", 100, 2)]);
        let r = diff(&old, &new, &opts(|_| {}));
        assert_eq!(r.changed, 1);
        assert_eq!(r.same, 0);
        assert_eq!(r.net_delta, 0);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].delta, 0);
    }

    /// 空对空、空对有内容、有内容对空：三个边界都不能 panic。
    #[test]
    fn diff_handles_empty_sides() {
        let empty = snap(vec![]);
        let full = snap(vec![entry("a", 1, 1), entry("b", 2, 2)]);
        let r = diff(&empty, &full, &opts(|_| {}));
        assert_eq!(r.added, 2);
        assert_eq!(r.rows.len(), 2);
        let r = diff(&full, &empty, &opts(|_| {}));
        assert_eq!(r.removed, 2);
        assert_eq!(r.net_delta, -3);
        let r = diff(&empty, &empty, &opts(|_| {}));
        assert_eq!(r.rows.len(), 0);
        assert_eq!(r.net_delta, 0);
    }

    #[test]
    fn diff_keeps_only_the_added_side_path_when_paths_differ() {
        // 改名 = 一条删除 + 一条新增（快照里没有 FRN，无法分辨"改名"）
        let old = snap(vec![entry(r"C:\old.txt", 10, 1)]);
        let new = snap(vec![entry(r"C:\new.txt", 10, 1)]);
        let r = diff(&old, &new, &opts(|_| {}));
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
        let kinds: Vec<&str> = r.rows.iter().map(|x| x.kind.key()).collect();
        assert!(kinds.contains(&"added"));
        assert!(kinds.contains(&"removed"));
        let added = r.rows.iter().find(|x| x.kind == DiffKind::Added).unwrap();
        assert_eq!(added.path, r"C:\new.txt", "新增行要用新快照的路径");
        let removed = r.rows.iter().find(|x| x.kind == DiffKind::Removed).unwrap();
        assert_eq!(removed.path, r"C:\old.txt");
    }

    #[test]
    fn summary_warns_about_truncated_snapshots() {
        let mut old = snap(vec![entry("a", 1, 1)]);
        old.truncated = 7;
        let new = snap(vec![entry("a", 1, 1)]);
        let r = diff(&old, &new, &opts(|_| {}));
        let s = summary_cn(&r);
        assert!(s.contains("截断"), "{s}");
        assert!(s.contains('7'), "{s}");
    }

    #[test]
    fn signed_size_uses_a_real_minus_sign() {
        assert_eq!(signed_size(0), "+0 B");
        assert!(signed_size(-1024).starts_with('−'), "必须用 U+2212，不是连字符");
        assert!(signed_size(-1024).contains("1.00 KB"));
    }

    #[test]
    fn rows_to_csv_has_header_and_escapes() {
        let rows = vec![DiffRow {
            path: "C:\\a,b\\c.txt".to_string(),
            kind: DiffKind::Grew,
            old_size: 1,
            new_size: 2,
            delta: 1,
            old_mtime: 1,
            new_mtime: 2,
        }];
        let csv = rows_to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "kind,delta,old_size,new_size,path");
        assert_eq!(lines[1], "grew,1,1,2,\"C:\\a,b\\c.txt\"");
    }

    #[test]
    fn full_pipeline_capture_diff_round_trip() {
        let t = tree();
        let before = Snapshot::capture(&t, None, "C:", 1, &SnapshotOptions::default());
        let text = before.to_tsv();
        let loaded = Snapshot::from_tsv(&text).unwrap();

        // 模拟"一周后"：a.bin 变大、zero.dat 被删、多了一个大文件
        let mut after = loaded.clone();
        after.entries.retain(|e| !e.path.ends_with("zero.dat"));
        for e in after.entries.iter_mut() {
            if e.path.ends_with("a.bin") {
                e.size = 9_000;
                e.mtime += 86_400;
            }
        }
        after.entries.push(entry(r"C:\docs\new.bin", 1_000, 999));
        after.entries.sort_by(|a, b| a.path.cmp(&b.path));

        let r = diff(&loaded, &after, &opts(|_| {}));
        assert_eq!(r.grew, 1);
        assert_eq!(r.removed, 1);
        assert_eq!(r.added, 1);
        assert_eq!(r.grew_bytes, 4_000);
        assert_eq!(r.removed_bytes, 0, "zero.dat 是 0 字节");
        assert_eq!(r.added_bytes, 1_000);
        assert_eq!(r.net_delta, 5_000);
        // 变大 4 000 排在新增 1 000 之前
        assert_eq!(r.rows[0].path, r"C:\docs\a.bin");
        assert_eq!(r.rows[0].kind, DiffKind::Grew);
    }
}
