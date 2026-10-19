//! 系统托盘。
//!
//! 用 `tray-icon` + `muda`（Tauri 生态）。它们内部自己建隐藏窗口，
//! 依赖当前线程的消息循环——和 global-hotkey 一样，必须在 UI 主线程创建。
//!
//! 这里只暴露一个中立的事件枚举，UI 层不需要认识 tray-icon 的类型。

use std::path::Path;

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::{PlatformError, PlatformResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Show,
    Settings,
    Quit,
}

pub struct SystemTray {
    /// 必须持有：drop 掉托盘图标就从任务栏消失了。
    _icon: TrayIcon,
    show_id: muda::MenuId,
    settings_id: muda::MenuId,
    quit_id: muda::MenuId,
}

impl SystemTray {
    /// `icon_path` 为空或读取失败时用内置图标。
    pub fn new(tooltip: &str, icon_path: Option<&Path>) -> PlatformResult<Self> {
        let menu = Menu::new();
        let show = MenuItem::new("显示历史", true, None);
        let settings = MenuItem::new("设置", true, None);
        let quit = MenuItem::new("退出 25仔", true, None);
        let separator = PredefinedMenuItem::separator();

        menu.append(&show).map_err(to_err)?;
        menu.append(&settings).map_err(to_err)?;
        menu.append(&separator).map_err(to_err)?;
        menu.append(&quit).map_err(to_err)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(load_icon(icon_path).unwrap_or(default_icon()?))
            .build()
            .map_err(|e| PlatformError::Tray(e.to_string()))?;

        Ok(Self {
            _icon: icon,
            show_id: show.id().clone(),
            settings_id: settings.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// 替换托盘图标；`None` 或读取失败时回到内置图标。
    pub fn apply_icon(&self, path: Option<&Path>) {
        let icon = load_icon(path).or_else(|| default_icon().ok());
        if let Err(e) = self._icon.set_icon(icon) {
            eprintln!("25zai: 更新托盘图标失败：{e}");
        }
    }

    /// 排空事件队列，返回最先命中的一个动作。
    pub fn poll(&self) -> Option<TrayEvent> {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show_id {
                return Some(TrayEvent::Show);
            }
            if event.id == self.settings_id {
                return Some(TrayEvent::Settings);
            }
            if event.id == self.quit_id {
                return Some(TrayEvent::Quit);
            }
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            // 左键单击直接唤出窗口，右键交给菜单。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                return Some(TrayEvent::Show);
            }
        }

        None
    }
}

/// 读任意常见图片格式并缩放到 32×32。
///
/// 不用 `tray_icon::Icon::from_path`：那个只在 Windows 下可用且只认 .ico，
/// 用户选 png 会直接失败。
fn load_icon(path: Option<&Path>) -> Option<tray_icon::Icon> {
    let path = path?;
    let started = std::time::Instant::now();
    let image = match image::open(path) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("25zai: 读取托盘图标失败（{}）：{e}", path.display());
            return None;
        }
    };
    println!("tray icon: {} ({}ms)", path.display(), started.elapsed().as_millis());
    let rgba = image.thumbnail(32, 32).to_rgba8();
    let (width, height) = rgba.dimensions();
    match tray_icon::Icon::from_rgba(rgba.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(e) => {
            eprintln!("25zai: 托盘图标格式无效：{e}");
            None
        }
    }
}

fn to_err(e: muda::Error) -> PlatformError {
    PlatformError::Tray(e.to_string())
}

/// 内嵌的默认图标（64×64 PNG，8 KB）。由 `--pack-icon` 从原图压出来。
const EMBEDDED_ICON: &[u8] = include_bytes!("../assets/icon.png");

fn default_icon() -> PlatformResult<tray_icon::Icon> {
    if let Ok(image) = image::load_from_memory(EMBEDDED_ICON) {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        if let Ok(icon) = tray_icon::Icon::from_rgba(rgba.into_raw(), width, height) {
            return Ok(icon);
        }
    }
    // 内嵌图损坏时退回程序生成的，保证托盘一定能建起来。
    generated_icon()
}

/// 32×32 的兜底图标：蓝底圆角方块 + 白色内页。
fn generated_icon() -> PlatformResult<tray_icon::Icon> {
    const S: i32 = 32;
    let mut rgba = vec![0u8; (S * S * 4) as usize];

    for y in 0..S {
        for x in 0..S {
            let inside_shell = rounded(x, y, 1, 30, 7);
            if !inside_shell {
                continue;
            }
            let inside_page = rounded(x, y, 9, 22, 2);
            let color = if inside_page {
                [246, 247, 249, 255]
            } else {
                [58, 110, 165, 255]
            };
            let i = ((y * S + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }

    tray_icon::Icon::from_rgba(rgba, S as u32, S as u32)
        .map_err(|e| PlatformError::Tray(e.to_string()))
}

/// 圆角矩形判定：`lo..=hi` 是矩形范围，`r` 是圆角半径。
fn rounded(x: i32, y: i32, lo: i32, hi: i32, r: i32) -> bool {
    if x < lo || x > hi || y < lo || y > hi {
        return false;
    }
    let dx = (lo + r - x).max(x - (hi - r)).max(0);
    let dy = (lo + r - y).max(y - (hi - r)).max(0);
    dx * dx + dy * dy <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_png(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("25zai-tray-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("icon.png");
        let img = image::RgbaImage::from_pixel(128, 128, image::Rgba([200, 40, 40, 255]));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn large_image_is_accepted_and_scaled() {
        let path = temp_png("big");
        assert!(load_icon(Some(&path)).is_some(), "128×128 应能缩到 32×32");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_or_absent_path_falls_back() {
        assert!(load_icon(None).is_none());
        assert!(load_icon(Some(Path::new("no/such/icon.png"))).is_none());
    }
}
