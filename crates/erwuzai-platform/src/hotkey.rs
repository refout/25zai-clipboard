//! 全局快捷键的边界与解析。
//!
//! 解析是纯逻辑，放在这里而不是各平台实现里，这样三个平台对
//! `"Ctrl+Shift+V"` 的理解不会漂移。

use std::fmt;

use crate::{PlatformError, PlatformResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const CTRL: Self = Self(1 << 0);
    pub const ALT: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeySpec {
    pub modifiers: Modifiers,
    /// 主键，保持用户书写的大小写（`V` 与 `v` 在实现层等价）。
    pub key: String,
}

impl HotkeySpec {
    /// 解析 `"Ctrl+Shift+V"` 这类写法。
    ///
    /// 拒绝不带修饰键的组合：单键全局热键会吞掉正常输入，
    /// 这在剪贴板工具上是灾难性的。
    pub fn parse(input: &str) -> PlatformResult<Self> {
        let mut modifiers = Modifiers::NONE;
        let mut key: Option<String> = None;

        for raw in input.split('+') {
            let part = raw.trim();
            if part.is_empty() {
                return Err(PlatformError::Malformed(format!("热键写法无效：{input}")));
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= Modifiers::CTRL,
                "alt" | "option" => modifiers |= Modifiers::ALT,
                "shift" => modifiers |= Modifiers::SHIFT,
                "super" | "cmd" | "command" | "win" | "meta" => modifiers |= Modifiers::SUPER,
                _ => {
                    if key.is_some() {
                        return Err(PlatformError::Malformed(format!(
                            "热键只能有一个主键：{input}"
                        )));
                    }
                    key = Some(part.to_string());
                }
            }
        }

        let key = key.ok_or_else(|| PlatformError::Malformed(format!("热键缺少主键：{input}")))?;
        if modifiers.is_empty() {
            return Err(PlatformError::Malformed(format!(
                "热键必须带修饰键：{input}"
            )));
        }
        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for HotkeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.modifiers.contains(Modifiers::CTRL) {
            parts.push("Ctrl");
        }
        if self.modifiers.contains(Modifiers::ALT) {
            parts.push("Alt");
        }
        if self.modifiers.contains(Modifiers::SHIFT) {
            parts.push("Shift");
        }
        if self.modifiers.contains(Modifiers::SUPER) {
            parts.push("Super");
        }
        parts.push(&self.key);
        f.write_str(&parts.join("+"))
    }
}

/// 注意：**不加 `Send + Sync`**。全局热键管理器在 Windows 上必须和消息循环
/// 同线程创建（`GlobalHotKeyManager` 内部持有 `*mut c_void`），
/// 强行加约束会让实现无法存在。这个 trait 只允许在 UI 主线程上使用。
pub trait HotkeyBackend {
    /// 注册失败必须上报，不能静默失效——用户需要知道快捷键被占了。
    fn register(&self, spec: &HotkeySpec) -> PlatformResult<()>;
    fn unregister(&self) -> PlatformResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let spec = HotkeySpec::parse("Ctrl+Shift+V").unwrap();
        assert!(spec.modifiers.contains(Modifiers::CTRL));
        assert!(spec.modifiers.contains(Modifiers::SHIFT));
        assert_eq!(spec.key, "V");
        assert_eq!(spec.to_string(), "Ctrl+Shift+V");
    }

    #[test]
    fn accepts_platform_aliases() {
        assert_eq!(
            HotkeySpec::parse("Cmd+Space").unwrap().modifiers,
            Modifiers::SUPER
        );
        assert_eq!(
            HotkeySpec::parse("Win+V").unwrap().modifiers,
            Modifiers::SUPER
        );
    }

    #[test]
    fn rejects_bare_key() {
        assert!(HotkeySpec::parse("V").is_err());
    }

    #[test]
    fn rejects_two_main_keys() {
        assert!(HotkeySpec::parse("Ctrl+V+B").is_err());
    }

    #[test]
    fn rejects_empty_parts() {
        assert!(HotkeySpec::parse("Ctrl++V").is_err());
    }
}
