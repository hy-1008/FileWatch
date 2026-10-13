//! 有界环形事件队列 + 背压（文档风险 R5）。
//!
//! 事件风暴（解压、`git checkout`）会在瞬间产生上万个事件。
//! 直接塞进 `Vec` 会无界增长直到 OOM。这里用**固定容量的环形缓冲**：
//! 满时丢弃**最旧**的元素并累加 `dropped` 计数——调用方据此
//! 降级为「标记子树脏、稍后局部重扫」（文档 R5 的对策）。

#[derive(Debug)]
pub struct RingQueue<T> {
    buf: Vec<Option<T>>,
    head: usize,
    len: usize,
    dropped: u64,
    capacity: usize,
}

impl<T> RingQueue<T> {
    pub fn new(capacity: usize) -> RingQueue<T> {
        let cap = capacity.max(1);
        let mut buf = Vec::with_capacity(cap);
        buf.resize_with(cap, || None);
        RingQueue {
            buf,
            head: 0,
            len: 0,
            dropped: 0,
            capacity: cap,
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len == self.capacity
    }

    /// 因溢出而被丢弃的元素总数（**对账用**，文档 §9「事件零丢失」指标）。
    #[inline]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 入队。返回 `false` 表示队列已满、最旧元素被挤掉。
    pub fn push(&mut self, v: T) -> bool {
        if self.len == self.capacity {
            // 覆盖最旧：head 前进，最旧槽位被新值替换
            let idx = self.head;
            self.buf[idx] = Some(v);
            self.head = (self.head + 1) % self.capacity;
            self.dropped += 1;
            return false;
        }
        let idx = (self.head + self.len) % self.capacity;
        self.buf[idx] = Some(v);
        self.len += 1;
        true
    }

    /// 出队（FIFO）。
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let idx = self.head;
        let v = self.buf[idx].take();
        self.head = (self.head + 1) % self.capacity;
        self.len -= 1;
        v
    }

    /// 批量出队，最多 `max` 个。返回实际取出的数量。
    pub fn drain_into(&mut self, out: &mut Vec<T>, max: usize) -> usize {
        let mut n = 0;
        while n < max {
            match self.pop() {
                Some(v) => {
                    out.push(v);
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// 清空（保留 dropped 统计）。
    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order() {
        let mut q = RingQueue::new(4);
        for i in 0..3 {
            assert!(q.push(i));
        }
        assert_eq!(q.pop(), Some(0));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn drops_oldest_when_full() {
        let mut q = RingQueue::new(3);
        q.push(1);
        q.push(2);
        q.push(3);
        assert!(q.is_full());
        assert!(!q.push(4), "满队列 push 应返回 false");
        assert_eq!(q.dropped(), 1);
        // 最旧的 1 被挤掉
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), Some(4));
    }

    #[test]
    fn wraps_around_many_times() {
        let mut q = RingQueue::new(3);
        for i in 0..100 {
            q.push(i);
        }
        // 保留最后 3 个
        assert_eq!(q.pop(), Some(97));
        assert_eq!(q.pop(), Some(98));
        assert_eq!(q.pop(), Some(99));
        assert_eq!(q.dropped(), 97);
    }

    #[test]
    fn drain_into_respects_max() {
        let mut q = RingQueue::new(10);
        for i in 0..8 {
            q.push(i);
        }
        let mut out = Vec::new();
        assert_eq!(q.drain_into(&mut out, 3), 3);
        assert_eq!(out, vec![0, 1, 2]);
        assert_eq!(q.len(), 5);
    }

    #[test]
    fn capacity_is_at_least_one() {
        let q: RingQueue<u8> = RingQueue::new(0);
        assert_eq!(q.capacity(), 1);
    }

    #[test]
    fn dropped_counter_survives_clear() {
        let mut q = RingQueue::new(2);
        q.push(1);
        q.push(2);
        q.push(3); // 挤掉 1
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.dropped(), 1);
    }

    #[test]
    fn works_with_non_copy_types() {
        let mut q = RingQueue::new(2);
        q.push(String::from("a"));
        q.push(String::from("b"));
        assert_eq!(q.pop(), Some(String::from("a")));
    }
}
