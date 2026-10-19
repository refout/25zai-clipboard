//! 敏感内容检测的边界。
//!
//! 检测是策略，不是存储；但检测必须发生在入库之前、且在存储线程上。
//! 这里只留一个 trait，把第三方检测器（`secretshape`）隔离在边界外：
//! 它的 API 与性能特征尚未在本项目里验证，不适合直接写死进存储层。
//!
//! 默认实现是保守的启发式，只用来保证"不装 secretshape 也不会把
//! 明显凭据原样落盘"。生产环境应注入 secretshape 实现。

/// `looks_sensitive` 会在每次文本采集时被调用，必须廉价。
pub trait SecretDetector: Send + Sync {
    fn looks_sensitive(&self, text: &str) -> bool;
}

/// 占位实现。命中以下任一条件即视为敏感：
///
/// - 长度 ≥ 16 且不含空白（密钥通常不分词）
/// - 已知 token 前缀
/// - 字母数字占比高且同时出现大小写与数字
///
/// 误报率明显高于 secretshape，**不要把它当成最终方案**。
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicDetector;

const TOKEN_PREFIXES: &[&str] = &[
    "sk-", "sk_live_", "ghp_", "gho_", "github_pat_", "xoxb-", "xoxp-", "AKIA", "ASIA",
    "AIza", "eyJ", "-----BEGIN",
];

impl SecretDetector for HeuristicDetector {
    fn looks_sensitive(&self, text: &str) -> bool {
        let t = text.trim();
        if t.len() < 16 {
            return false;
        }
        if TOKEN_PREFIXES.iter().any(|p| t.starts_with(p)) {
            return true;
        }
        if t.contains(char::is_whitespace) {
            return false;
        }
        let mut has_lower = false;
        let mut has_upper = false;
        let mut has_digit = false;
        let mut alnum = 0usize;
        for c in t.chars() {
            if c.is_ascii_lowercase() {
                has_lower = true;
                alnum += 1;
            } else if c.is_ascii_uppercase() {
                has_upper = true;
                alnum += 1;
            } else if c.is_ascii_digit() {
                has_digit = true;
                alnum += 1;
            }
        }
        alnum * 10 >= t.len() * 8 && has_lower && has_upper && has_digit
    }
}
