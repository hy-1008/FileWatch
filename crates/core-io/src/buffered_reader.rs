//! 大块顺序预读：固定大小缓冲 + 循环复用，避免每条 MFT 记录一次分配。
//!
//! 对应文档 §5.1 性能要点 2：「大块顺序读（16 MB/次）+ 生产者-消费者双线程」。

use crate::volume::Volume;
use std::io;

/// 在 `[start, end)` 这段**连续物理区间**上按固定大小分块读取。
///
/// 缓冲只分配一次，之后每块就地覆盖；调用方拿到的 `&[u8]` 在下次
/// `next_chunk()` 之前有效（不复制块内容，零拷贝）。
pub struct ChunkReader<'a> {
    vol: &'a Volume,
    pos: u64,
    end: u64,
    chunk: usize,
    buf: Vec<u8>,
    last_base: u64,
    last_len: usize,
}

impl<'a> ChunkReader<'a> {
    /// `chunk` 会被向上对齐到扇区大小的整数倍。
    pub fn new(vol: &'a Volume, start: u64, end: u64, chunk: usize) -> ChunkReader<'a> {
        let ss = vol.sector_size() as usize;
        let mut chunk = chunk.max(ss);
        if chunk % ss != 0 {
            chunk = (chunk / ss + 1) * ss;
        }
        let buf = vec![0u8; chunk];
        ChunkReader {
            vol,
            pos: start,
            end,
            chunk,
            buf,
            last_base: start,
            last_len: 0,
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk
    }

    /// 下一块将要读取的物理偏移。
    pub fn current_pos(&self) -> u64 {
        self.pos
    }

    /// 取下一块。返回 `Ok(None)` 表示这一段读完。
    pub fn next_chunk(&mut self) -> io::Result<Option<&[u8]>> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let want = core::cmp::min(self.chunk as u64, self.end - self.pos) as usize;
        self.vol.read_raw(self.pos, &mut self.buf[..want])?;
        self.last_base = self.pos;
        self.last_len = want;
        self.pos += want as u64;
        Ok(Some(&self.buf[..want]))
    }

    /// 取下一块（可变版本）。
    ///
    /// 与 `next_chunk` 是同一套读取逻辑，只是返回的借用可变性不同。
    /// 之所以必须有可变版本：**MFT 记录解析必须就地应用 fixup**
    /// （用 Update Sequence Array 覆盖每个扇区尾部的序列号），
    /// 这一步会改写缓冲内容，`&[u8]` 做不到。
    pub fn next_chunk_mut(&mut self) -> io::Result<Option<&mut [u8]>> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let want = core::cmp::min(self.chunk as u64, self.end - self.pos) as usize;
        let base = self.pos;
        self.vol.read_raw(base, &mut self.buf[..want])?;
        self.last_base = base;
        self.last_len = want;
        self.pos = base + want as u64;
        Ok(Some(&mut self.buf[..want]))
    }

    /// 上一块的物理起始偏移（与 `next_chunk` 返回的切片一一对应）。
    pub fn last_chunk_base(&self) -> u64 {
        self.last_base
    }

    /// 上一块的长度。
    pub fn last_chunk_len(&self) -> usize {
        self.last_len
    }
}
