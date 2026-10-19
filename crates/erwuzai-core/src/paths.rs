//! 数据目录解析。
//!
//! 不引入 `dirs` 之类的依赖：路径规则就三条，写清楚比多一个 crate 划算。
//!
//! | 平台 | 默认目录 |
//! |---|---|
//! | Windows | `%APPDATA%\25zai` |
//! | macOS | `$HOME/Library/Application Support/25zai` |
//! | 其他 Unix | `$XDG_DATA_HOME/25zai`，缺省 `$HOME/.local/share/25zai` |
//!
//! `25ZAI_DATA_DIR` 可以整体覆盖，用于便携模式和测试。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const APP_DIR: &str = "25zai";
pub const ENV_OVERRIDE: &str = "25ZAI_DATA_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// 从真实环境变量解析。
    pub fn resolve() -> PathBuf {
        Self::resolve_with(|k| std::env::var(k).ok())
    }

    /// 注入式解析，便于测试而不用改进程环境。
    pub fn resolve_with(get: impl Fn(&str) -> Option<String>) -> PathBuf {
        if let Some(dir) = get(ENV_OVERRIDE).filter(|s| !s.trim().is_empty()) {
            return PathBuf::from(dir);
        }

        if cfg!(target_os = "windows") {
            if let Some(appdata) = get("APPDATA").filter(|s| !s.is_empty()) {
                return PathBuf::from(appdata).join(APP_DIR);
            }
            if let Some(profile) = get("USERPROFILE").filter(|s| !s.is_empty()) {
                return PathBuf::from(profile).join("AppData/Roaming").join(APP_DIR);
            }
            return PathBuf::from(".").join(APP_DIR);
        }

        if cfg!(target_os = "macos") {
            if let Some(home) = get("HOME").filter(|s| !s.is_empty()) {
                return PathBuf::from(home)
                    .join("Library/Application Support")
                    .join(APP_DIR);
            }
            return PathBuf::from(".").join(APP_DIR);
        }

        if let Some(data) = get("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            return PathBuf::from(data).join(APP_DIR);
        }
        if let Some(home) = get("HOME").filter(|s| !s.is_empty()) {
            return PathBuf::from(home).join(".local/share").join(APP_DIR);
        }
        PathBuf::from(".").join(APP_DIR)
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("25zai.db")
    }

    pub fn blobs(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// 单实例锁文件。文件本身不需要内容，锁由操作系统持有。
    pub fn lock(&self) -> PathBuf {
        self.root.join("instance.lock")
    }

    /// 首实例监听的回环端口。第二实例读它来发"唤出"请求。
    pub fn port(&self) -> PathBuf {
        self.root.join("instance.port")
    }

    /// 建目录。Unix 上把权限收紧到 0700。
    ///
    /// Windows 不需要显式处理：`%APPDATA%` 的默认 ACL 已经只允许当前用户。
    pub fn ensure(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.blobs())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn override_wins() {
        let got = Paths::resolve_with(env(&[
            (ENV_OVERRIDE, "D:/portable/25zai"),
            ("APPDATA", "C:/Users/x/AppData/Roaming"),
        ]));
        assert_eq!(got, PathBuf::from("D:/portable/25zai"));
    }

    #[test]
    fn blank_override_is_ignored() {
        let got = Paths::resolve_with(env(&[(ENV_OVERRIDE, "   "), ("HOME", "/home/x")]));
        assert_ne!(got, PathBuf::from("   "));
        assert!(got.ends_with(APP_DIR));
    }

    #[test]
    fn falls_back_to_relative_dir_without_env() {
        let got = Paths::resolve_with(|_| None);
        assert_eq!(got, PathBuf::from(".").join(APP_DIR));
    }

    #[test]
    fn derived_paths_live_under_root() {
        let p = Paths::new("/tmp/25zai");
        assert_eq!(p.db(), PathBuf::from("/tmp/25zai/25zai.db"));
        assert_eq!(p.blobs(), PathBuf::from("/tmp/25zai/blobs"));
        assert_eq!(p.lock(), PathBuf::from("/tmp/25zai/instance.lock"));
    }
}
