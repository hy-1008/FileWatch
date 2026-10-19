//! 文件内容哈希与逐字节比对（**零依赖**，只用来给重复文件去重做筛子）。
//!
//! # 为什么可以不用加密哈希
//!
//! 重复文件的最后一步是**逐字节复核**（见 `core_scan::dedup`）。也就是说哈希只
//! 负责「把不可能相同的文件快速扔掉」，正确性由字节比对兜底。既然如此，
//! 引入 SHA-1 / BLAKE3 这类密码学哈希没有收益：它们为了抗碰撞要跑十几轮混合，
//! 而在本场景里「碰撞」的最坏后果只是多比一次字节 —— 反正也要比。
//!
//! 所以这里用 FNV-1a 64：一条 `xor` + 一条乘法，纯顺序、没有查表、没有 SIMD
//! 分派。它是**唯一**一种可以十行写完、且能逐字节对照公开测试向量的哈希。
//!
//! # 三个入口的分工
//!
//! | 函数 | 用途 | 代价 |
//! |---|---|---|
//! | [`Hasher64`] | 流式累加，给「边读边算」用 | — |
//! | [`hash_head_tail`] | 只读文件首尾各 N 字节，快速筛掉大部分候选 | O(块大小) |
//! | [`hash_file`] | 读完整文件 | O(文件大小) |
//! | [`files_equal`] | 逐字节复核（分块流式，内存恒定） | 最坏 O(文件大小) |
//!
//! **长度一律先折进哈希**：文件长度相同才可能内容相同，把长度当作前缀写进去，
//! 可以让「长度不同」的候选在第一轮就被分开，不必等读到内容。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// FNV-1a 64 的偏移基数（公开常量，见 <http://www.isthe.com/chongo/tech/comp/fnv/>）。
pub const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 的质数。
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 部分哈希与逐字节比对的分块大小（64 KiB）。
///
/// 与磁盘的典型预读粒度同量级：再小会退化成小块随机读，再大对顺序读没有帮助
/// 却增加内存峰值。
pub const CHUNK: usize = 64 * 1024;

/// 「部分哈希」默认取首尾各多少字节。
///
/// 4 KiB 是 NTFS 的默认簇大小，也是绝大多数文件系统的块大小 ——
/// 取一个块的首尾，刚好能覆盖「文件头 + 文件尾」这类最常见的差异位置。
pub const PARTIAL_BLOCK: usize = 4096;

/// 纯 FNV-1a 64。一次算完的便捷入口，同时是 [`Hasher64`] 的正确性参照。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET_BASIS;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(FNV_PRIME);
    }
    h
}

/// 流式 FNV-1a 64。可以「读一块写一块」，不必把整个文件读进内存。
///
/// [`Hasher64::finish`] 会把**总字节数**折进去，因此本哈希等于
/// `fnv1a64(全部已写入字节 ‖ 总长度的小端 8 字节)`。
///
/// 为什么不在每次 `write` 里带上长度：那样 `write(a)` + `write(b)` 就不等于
/// `write(a‖b)` 了，而流式读取恰恰是按块调用的。把长度留到最后一次折，
/// 才能保证「同一段字节流，无论分几块读，结果都一样」——这是流式哈希的
/// 最低要求，也是这条链路上最容易写错的地方。
#[derive(Debug, Clone)]
pub struct Hasher64 {
    state: u64,
    written: u64,
}

impl Default for Hasher64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher64 {
    pub const fn new() -> Hasher64 {
        Hasher64 {
            state: FNV_OFFSET_BASIS,
            written: 0,
        }
    }

    /// 把 `bytes` 追加进哈希。可重复调用，结果与一次性写入等价。
    pub fn write(&mut self, bytes: &[u8]) {
        let mut h = self.state;
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(FNV_PRIME);
        }
        self.state = h;
        self.written = self.written.saturating_add(bytes.len() as u64);
    }

    /// 小端 8 字节整数入哈希（用来把长度、偏移这类元信息也折进去）。
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    /// 已写入的字节数。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 收尾。可重复调用（不改内部状态），所以「先算个中间值看看」是安全的。
    pub fn finish(&self) -> u64 {
        let mut h = self.state;
        for &b in self.written.to_le_bytes().iter() {
            h = (h ^ b as u64).wrapping_mul(FNV_PRIME);
        }
        h
    }
}

/// 读满 `buf` 或读到文件尾为止，返回实际读到的字节数。
///
/// 不能直接用 `read_exact`：正常文件的大小会在两次读之间被改小，那时
/// `read_exact` 会返回 `UnexpectedEof` 并让整个去重流程报错 —— 而我们要的是
/// 「按实际读到的内容算哈希」，不是失败。
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0usize;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// 「部分哈希」：读文件**首尾各 `block` 字节**（外加长度）。
///
/// 比只读开头强得多：大量不同格式的文件共享同样的文件头（`PK\x03\x04`、
/// `\x89PNG`……），只看开头会让它们全落进同一个桶里。
///
/// 文件不超过 `2 * block` 时**整文件参与判据**（反正读得完，多读没有代价，
/// 结论还更准），顺带天然避开了「首尾区间重叠、同一段字节被算两遍」的坑。
///
/// 注意它与 [`hash_file`] 的判据**不相等**，也不需要相等：这里把长度写在内容
/// 前面，[`hash_file`] 依赖 [`Hasher64::finish`] 把长度缀在后面。两者是两级筛选
/// 各自的判据，只要各自稳定、且同一级内可比即可。
///
/// 判据本身在 [`hash_head_tail_bytes`] 里，这里只负责「少读一点」。
pub fn hash_head_tail(path: &Path, block: usize) -> io::Result<u64> {
    let block = block.max(1);
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut h = Hasher64::new();
    h.write_u64(len);

    let mut buf = vec![0u8; block];
    if len <= 2 * block as u64 {
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let n = read_up_to(&mut f, &mut chunk)?;
            if n == 0 {
                break;
            }
            h.write(&chunk[..n]);
        }
    } else {
        let n = read_up_to(&mut f, &mut buf)?;
        h.write(&buf[..n]);
        // 尾部区间与首部区间不重叠（上面已按 2*block 分流）
        f.seek(SeekFrom::Start(len - block as u64))?;
        let m = read_up_to(&mut f, &mut buf)?;
        h.write(&buf[..m]);
    }
    Ok(h.finish())
}

/// 「首尾块」判据的**唯一实现**：对一段完整内容做部分哈希。
///
/// 单独抽出来是为了让「不读盘」的调用方（以及测试里的内存内容源）能用**同一个**
/// 判据。否则内存实现与磁盘实现各写一份，两边对「小文件算不算整读」的理解
/// 一旦不同，测试就会在真实文件上得出与内存里不同的结论 —— 而那种不一致
/// 只表现为「某些文件莫名不成组」，极难查。
pub fn hash_head_tail_bytes(data: &[u8], block: usize) -> u64 {
    let block = block.max(1);
    let mut h = Hasher64::new();
    h.write_u64(data.len() as u64);
    if data.len() <= 2 * block {
        h.write(data);
    } else {
        h.write(&data[..block]);
        h.write(&data[data.len() - block..]);
    }
    h.finish()
}

/// 整个文件的流式哈希（内存恒定 64 KiB）。
pub fn hash_file(path: &Path) -> io::Result<u64> {
    let mut f = File::open(path)?;
    Ok(hash_reader(&mut f)?)
}

/// 从任意 `Read` 上算整体哈希。给测试与非文件来源（内存、管道）用。
pub fn hash_reader<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut h = Hasher64::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let n = read_up_to(r, &mut chunk)?;
        if n == 0 {
            break;
        }
        h.write(&chunk[..n]);
    }
    Ok(h.finish())
}

/// 两个文件是否**逐字节**相同。
///
/// 先比长度（不同即不同，省掉一次全读），再分块流式比较，内存恒定。
/// 哈希撞车在这里被挡住 —— 这是三级去重里唯一给出「确定」结论的一步。
pub fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let la = fa.metadata()?.len();
    let lb = fb.metadata()?.len();
    if la != lb {
        return Ok(false);
    }
    let mut ba = vec![0u8; CHUNK];
    let mut bb = vec![0u8; CHUNK];
    let mut left = la;
    while left > 0 {
        let want = (left as usize).min(CHUNK);
        let na = read_up_to(&mut fa, &mut ba[..want])?;
        let nb = read_up_to(&mut fb, &mut bb[..want])?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            // 长度声明是 la，却读到 EOF：文件在比较过程中被截断。
            // 保守地当作「不相同」，让上层把它归到"读不完整"，而不是误判为重复。
            return Ok(false);
        }
        left -= na as u64;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// FNV-1a 64 的公开测试向量。
    ///
    /// 这不是「跑通就行」的断言：脱敏算法一旦写错（比如把 `xor` 与 `mul` 的
    /// 顺序颠倒，那就成了 FNV-1），分布会变差但**不会报错**，只有对向量才看得出来。
    #[test]
    fn fnv1a64_matches_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// 流式拆分与一次性写入必须完全等价 —— 这是 [`Hasher64::finish`] 把长度
    /// 留到最后才折进去的全部理由。
    #[test]
    fn streaming_equals_one_shot() {
        let all = b"the quick brown fox jumps over the lazy dog";
        let one = {
            let mut h = Hasher64::new();
            h.write(all);
            h.finish()
        };
        for cut in 1..all.len() {
            let mut h = Hasher64::new();
            h.write(&all[..cut]);
            h.write(&all[cut..]);
            assert_eq!(h.finish(), one, "在 {cut} 处切开后结果不一致");
        }
        // 再加一条：三块也对
        let mut h = Hasher64::new();
        h.write(&all[..3]);
        h.write(&all[3..7]);
        h.write(&all[7..]);
        assert_eq!(h.finish(), one);
    }

    /// `finish()` 可重复调用，且不改状态（诊断时常常先看一眼再继续写）。
    #[test]
    fn finish_is_pure() {
        let mut h = Hasher64::new();
        h.write(b"abc");
        let a = h.finish();
        let b = h.finish();
        assert_eq!(a, b);
        h.write(b"def");
        assert_ne!(h.finish(), a);
        assert_eq!(h.written(), 6);
    }

    /// 长度必须参与哈希：否则「前缀相同的两个不同长度文件」会被判为同一桶。
    #[test]
    fn length_is_folded_in() {
        let short = {
            let mut h = Hasher64::new();
            h.write(b"abc");
            h.finish()
        };
        let long = {
            let mut h = Hasher64::new();
            h.write(b"abc");
            h.write(b"");
            h.finish()
        };
        assert_eq!(short, long, "长度相同，末次写入为空串不应改变结果");
        let other = {
            let mut h = Hasher64::new();
            h.write(b"abcdef");
            h.finish()
        };
        assert_ne!(short, other, "长度不同必须得到不同哈希");
    }

    fn write_file(path: &Path, data: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(data).unwrap();
        f.flush().unwrap();
    }

/// 首尾块哈希的三条契约：
/// ① 小文件（≤ 2×block）**整文件参与判据**，改任意一个字节都会变；
/// ② 大文件**只看首尾**，改中段不变 —— 这正是它只能当筛子的原因；
/// ③ 与 [`hash_file`] 的判据不同（长度位置不同），不要求相等。
#[test]
fn head_tail_block_hash_behaves_as_documented() {
        let owned = crate::test_dir::TestDir::new("hash-head-tail-block-hash-behaves-as-documented").unwrap();
    // ① 小文件：任意位置改一个字节都必须变
    let small = owned.path().join("small_ctr.bin");
    let data = b"hello world".to_vec();
    let base = hash_head_tail_bytes(&data, 64);
    write_file(&small, &data);
    assert_eq!(hash_head_tail(&small, 64).unwrap(), base);
    for i in 0..data.len() {
        let mut d = data.clone();
        d[i] ^= 0xFF;
        assert_ne!(
            hash_head_tail_bytes(&d, 64),
            base,
            "第 {i} 字节改动后小文件的判据必须变化（说明整文件都参与了）"
        );
    }
    // 内容相同的两个小文件必须同判据
    let small2 = owned.path().join("small_ctr2.bin");
    write_file(&small2, &data);
    assert_eq!(
        hash_head_tail(&small, 64).unwrap(),
        hash_head_tail(&small2, 64).unwrap()
    );

    // ② 大文件：中段不同 → 部分哈希相同；整文件哈希必须看出来
    let big1 = owned.path().join("big1.bin");
    let big2 = owned.path().join("big2.bin");
    let mut d1 = vec![0u8; 1024 * 1024];
    let mut d2 = d1.clone();
    d2[500_000] = 1;
    d1[0] = 7;
    d2[0] = 7;
    write_file(&big1, &d1);
    write_file(&big2, &d2);
    assert_eq!(
        hash_head_tail(&big1, PARTIAL_BLOCK).unwrap(),
        hash_head_tail(&big2, PARTIAL_BLOCK).unwrap(),
        "中段不同、首尾相同 → 部分哈希必然相同（否则它就不叫「部分」哈希了）"
    );
    assert_ne!(
        hash_file(&big1).unwrap(),
        hash_file(&big2).unwrap(),
        "整文件哈希必须把中段差异看出来"
    );
    assert!(!files_equal(&big1, &big2).unwrap());
}

    /// 首尾区间不得重叠：`block < len <= 2*block` 时如果两边都写，
    /// 同一段字节会被算两遍，于是「内容相同的两个文件」有可能算出不同哈希 ——
    /// 这类错误不会把不同的东西判成相同，只会**漏掉**真正的重复，更难发现。
    #[test]
    fn overlapping_head_tail_does_not_double_count() {
        let owned = crate::test_dir::TestDir::new("hash-overlapping-head-tail-does-not-double-count").unwrap();
        for len in [
            PARTIAL_BLOCK,
            PARTIAL_BLOCK + 1,
            2 * PARTIAL_BLOCK - 1,
            2 * PARTIAL_BLOCK,
            2 * PARTIAL_BLOCK + 1,
        ] {
            let p1 = owned.path().join(&format!("ovl_a_{len}.bin"));
            let p2 = owned.path().join(&format!("ovl_b_{len}.bin"));
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            write_file(&p1, &data);
            write_file(&p2, &data);
            assert_eq!(
                hash_head_tail(&p1, PARTIAL_BLOCK).unwrap(),
                hash_head_tail(&p2, PARTIAL_BLOCK).unwrap(),
                "长度 {len}：内容相同必须得到相同的部分哈希"
            );
        }
    }

    /// 「读盘版」与「内存版」的部分哈希必须**逐字节同一判据**。
    ///
    /// 两者一旦分叉，测试里用内存造的例子都会过，但真实文件的行为不一样 ——
    /// 症状只是「有些真正重复的文件不成组」，而没有任何报错。所以这条要
    /// 跨长度边界全跑一遍。
    #[test]
    fn disk_and_memory_partial_hash_agree() {
        let owned = crate::test_dir::TestDir::new("hash-disk-and-memory-partial-hash-agree").unwrap();
        for len in [
            0usize,
            1,
            100,
            PARTIAL_BLOCK - 1,
            PARTIAL_BLOCK,
            2 * PARTIAL_BLOCK,
            2 * PARTIAL_BLOCK + 1,
            100_000,
        ] {
            let p = owned.path().join(&format!("agree_{len}.bin"));
            let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            write_file(&p, &data);
            assert_eq!(
                hash_head_tail(&p, PARTIAL_BLOCK).unwrap(),
                hash_head_tail_bytes(&data, PARTIAL_BLOCK),
                "长度 {len} 时两种入口的结果必须一致"
            );
        }
    }

    /// 逐字节比对：长度不同立刻为假；内容相同为真；只在最后一字节不同也为假。
    #[test]
    fn byte_compare_is_exact() {
        let owned = crate::test_dir::TestDir::new("hash-byte-compare-is-exact").unwrap();
        let a = owned.path().join("eq_a.bin");
        let b = owned.path().join("eq_b.bin");
        let c = owned.path().join("eq_c.bin");
        write_file(&a, b"abcdefgh");
        write_file(&b, b"abcdefgh");
        write_file(&c, b"abcdefgX");
        assert!(files_equal(&a, &b).unwrap());
        assert!(!files_equal(&a, &c).unwrap());
        // 长度不同
        write_file(&c, b"abc");
        assert!(!files_equal(&a, &c).unwrap());
        // 大文件、只有中间一个字节不同
        let big1 = owned.path().join("eq_big1.bin");
        let big2 = owned.path().join("eq_big2.bin");
        let mut d = vec![0u8; 300_000];
        write_file(&big1, &d);
        d[150_000] = 9;
        write_file(&big2, &d);
        assert!(!files_equal(&big1, &big2).unwrap());
    }

    /// 打不开的文件必须**报错**而不是给一个"看起来正常"的哈希 ——
    /// 上层要靠这个 `Err` 把文件记成"读不到"，绝不能让它参与成组。
    #[test]
    fn missing_file_is_an_error() {
        let owned = crate::test_dir::TestDir::new("hash-missing-file-is-an-error").unwrap();
        let missing = owned.path().join("does_not_exist_12345.bin");
        assert!(hash_file(&missing).is_err());
        assert!(hash_head_tail(&missing, 64).is_err());
        assert!(files_equal(&missing, &missing).is_err());
    }
}
