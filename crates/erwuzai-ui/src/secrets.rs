//! 敏感内容检测的生产实现。
//!
//! `erwuzai-store` 里只有一个占位启发式，误报率明显偏高。这里换成
//! `secretshape` 的布尔层——它专门为剪贴板这类"留不留"的热路径设计：
//! 宁可误报，也不要把密钥留在历史里。单次判定是微秒级。
//!
//! 之所以放在 UI 层而不是存储层：存储层不应该为了一个策略依赖具体实现，
//! 它只认 `SecretDetector` 这个 trait。

use erwuzai_store::SecretDetector;

#[derive(Debug, Default, Clone, Copy)]
pub struct SecretshapeDetector;

impl SecretDetector for SecretshapeDetector {
    fn looks_sensitive(&self, text: &str) -> bool {
        secretshape::is_probably_secret(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensitive(text: &str) -> bool {
        SecretshapeDetector.looks_sensitive(text)
    }

    #[test]
    fn catches_vendor_tokens() {
        assert!(sensitive("ghp_1234567890abcdefghijklmnopqrstuvwxyz"));
        assert!(sensitive("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn catches_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert!(sensitive(jwt), "JWT 应被识别");
    }

    #[test]
    fn passes_ordinary_prose() {
        assert!(!sensitive("今天下午三点在会议室讨论一下下周的排期"));
        assert!(!sensitive("The quick brown fox jumps over the lazy dog."));
    }
}
