//! 把图标和版本信息嵌进 exe 的资源段。
//!
//! 不用 winresource：它靠注册表 / vswhere 找 rc.exe，在这台机器上直接
//! "Could not compile resource file"，而且不给原因。自己找、自己调，
//! 失败时至少能打出一条能看懂的警告。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // build.rs 的工作目录是 crate 根，图标放在仓库根的 assets/ 下。
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    #[cfg(windows)]
    if let Err(e) = embed_windows_resources() {
        // 缺 SDK 时退化成"没有图标的 exe"，但不该让整个构建失败。
        println!("cargo:warning=嵌入 exe 资源失败：{e}");
    }
}

#[cfg(windows)]
fn embed_windows_resources() -> Result<(), String> {
    let rc = find_rc().ok_or("找不到 rc.exe（需要 Windows SDK）")?;
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let (v0, v1, v2) = parse_version(&version);

    // canonicalize 会给出 \\?\ 前缀的扩展路径，rc.exe 不认，得剥掉。
    let mut ico = Path::new("../../assets/icon.ico")
        .canonicalize()
        .map_err(|e| format!("找不到 assets/icon.ico：{e}"))?
        .display()
        .to_string();
    if let Some(rest) = ico.strip_prefix(r"\\?\") {
        ico = rest.to_string();
    }
    // rc 的字符串里反斜杠是转义符，路径要换成正斜杠，否则 C:\Users 会被
    // 当成 \U 转义，报 "file not found: C:\Users"。
    ico = ico.replace('\\', "/");
    let rc_path = out.join("25zai.rc");
    let res_path = out.join("25zai.res");

    // code_page 必须写：文件描述里有中文，rc 默认按系统 ANSI 读会乱码。
    let script = format!(
        r#"#pragma code_page(65001)
1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {v0},{v1},{v2},0
PRODUCTVERSION {v0},{v1},{v2},0
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "refout"
            VALUE "FileDescription", "25仔 — 剪贴板历史"
            VALUE "FileVersion", "{version}"
            VALUE "ProductName", "25zai"
            VALUE "ProductVersion", "{version}"
            VALUE "LegalCopyright", "MIT"
        END
    END
END
"#
    );
    std::fs::write(&rc_path, script).map_err(|e| format!("写 rc 失败：{e}"))?;

    let output = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_path.display()))
        .arg(&rc_path)
        .output()
        .map_err(|e| format!("调用 rc 失败：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "rc 退出码 {:?}：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // .res 直接交给 link.exe；只作用于 bin，不污染单测。
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
    Ok(())
}

/// 先看 PATH，再扫 SDK 安装目录取版本号最大的那个。
#[cfg(windows)]
fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let kits = PathBuf::from(std::env::var("ProgramFiles(x86)").ok()?).join("Windows Kits/10/bin");
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for entry in std::fs::read_dir(kits).ok()? {
        let entry = entry.ok()?;
        let candidate = entry.path().join("x64/rc.exe");
        if !candidate.is_file() {
            continue;
        }
        let key = parse_version(&entry.file_name().to_string_lossy());
        let key = vec![key.0, key.1, key.2];
        if best.as_ref().is_none_or(|(current, _)| key > *current) {
            best = Some((key, candidate));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(windows)]
fn parse_version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.trim_start_matches('v').split('.');
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next(), next())
}
