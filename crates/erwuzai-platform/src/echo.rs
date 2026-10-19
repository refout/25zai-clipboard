//! 回声抑制。
//!
//! 回放要写剪贴板，写剪贴板会触发自己的监听。不处理的话，普通回放会
//! 空转一次去重，**纯文本粘贴会生成一条新历史**。
//!
//! 判定分两级：
//!
//! 1. 平台有序列号时，序列号相等才是"我们写的那一次"。序列号不等时
//!    **不再回退到哈希**——那说明是另一次写入（哪怕内容相同），
//!    用户真的又复制了一遍同样的东西，应该照常采集。
//! 2. 没有序列号时（或事件早于 `note_sequence` 到达），退回哈希 + 超时窗口。
//!
//! 注意 guard 存的是**我们写进去的字节**的哈希，不是条目的哈希。
//! 纯文本粘贴写进去的内容与存储内容不同，只有前者能匹配上。

use std::time::{Duration, Instant};

/// 默认窗口。太短会漏判（平台事件延迟），太长会把用户紧接着的
/// 真实复制吞掉。
pub const DEFAULT_TTL: Duration = Duration::from_millis(300);

pub struct EchoGuard {
    armed: Option<Armed>,
    ttl: Duration,
}

struct Armed {
    hash: [u8; 32],
    expected_seq: Option<u64>,
    deadline: Instant,
}

impl Default for EchoGuard {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl EchoGuard {
    pub fn new(ttl: Duration) -> Self {
        Self { armed: None, ttl }
    }

    /// 写剪贴板**之前**调用。
    pub fn arm(&mut self, hash: [u8; 32]) {
        self.armed = Some(Armed {
            hash,
            expected_seq: None,
            deadline: Instant::now() + self.ttl,
        });
    }

    /// 写成功**之后**调用，用精确序列号替换哈希兜底。
    ///
    /// 如果事件已经先到并被哈希判掉，这里什么也不做。
    pub fn note_sequence(&mut self, seq: u64) {
        if let Some(armed) = self.armed.as_mut() {
            armed.expected_seq = Some(seq);
        }
    }

    /// 写入失败时立即调用，否则窗口内用户真实的复制会被误吞。
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// 监听线程收到变更后调用。返回 `true` 表示这是自己写的那次，丢弃。
    pub fn should_suppress(&mut self, hash: &[u8; 32], seq: Option<u64>) -> bool {
        let Some(armed) = self.armed.as_ref() else {
            return false;
        };
        let expired = Instant::now() > armed.deadline;
        let expected = armed.expected_seq;
        let hash_match = armed.hash == *hash;

        if expired {
            self.armed = None;
            return false;
        }

        let hit = match (expected, seq) {
            (Some(expected), Some(observed)) => expected == observed,
            _ => hash_match,
        };
        if hit {
            self.armed = None;
        }
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn suppresses_matching_hash_once() {
        let mut g = EchoGuard::default();
        g.arm(h(1));
        assert!(g.should_suppress(&h(1), None));
        assert!(!g.is_armed(), "命中后必须解除，避免吞掉后续真实复制");
        assert!(!g.should_suppress(&h(1), None));
    }

    #[test]
    fn keeps_armed_on_other_content() {
        let mut g = EchoGuard::default();
        g.arm(h(1));
        assert!(!g.should_suppress(&h(2), None));
        assert!(g.is_armed());
        assert!(g.should_suppress(&h(1), None));
    }

    #[test]
    fn exact_sequence_wins_over_hash() {
        let mut g = EchoGuard::default();
        g.arm(h(1));
        g.note_sequence(42);

        // 序列号不同 ⇒ 是另一次写入，即使内容相同也要采集
        assert!(!g.should_suppress(&h(1), Some(7)));
        assert!(g.is_armed());

        assert!(g.should_suppress(&h(1), Some(42)));
    }

    #[test]
    fn expires() {
        let mut g = EchoGuard::new(Duration::from_millis(1));
        g.arm(h(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!g.should_suppress(&h(1), None));
        assert!(!g.is_armed());
    }

    #[test]
    fn disarm_prevents_suppression() {
        let mut g = EchoGuard::default();
        g.arm(h(1));
        g.disarm();
        assert!(!g.should_suppress(&h(1), None));
    }

    #[test]
    fn idle_guard_never_suppresses() {
        let mut g = EchoGuard::default();
        assert!(!g.should_suppress(&h(1), Some(1)));
    }
}
