//! Win32 弹出菜单的深色适配。
//!
//! 托盘右键菜单是 `TrackPopupMenu` 画出来的，属于系统菜单：它只跟随
//! Windows 自己的主题设置，不会跟着应用选的浅色/深色走。Win11 至今
//! 也没有给这件事提供公开接口，唯一可行的办法是 uxtheme.dll 里两个
//! 按序号导出的未公开函数——135 `SetPreferredAppMode`、136
//! `FlushMenuThemes`。这里按序号取地址调用，取不到就安静地跳过：
//! 老系统上菜单继续跟随系统主题，功能不受影响。

/// 把本进程的弹出菜单切到深色 / 浅色。
///
/// 只管"系统菜单长什么样"，和窗口内容无关；窗口内容仍然是 egui 自己画的。
#[cfg(windows)]
pub fn set_menu_dark_mode(dark: bool) {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

    /// `PreferredAppMode`：0 默认、1 允许深色、2 强制深色、3 强制浅色。
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;
    const SET_PREFERRED_APP_MODE: usize = 135;
    const FLUSH_MENU_THEMES: usize = 136;

    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();

    let name: Vec<u16> = "uxtheme.dll\0".encode_utf16().collect();
    // 先问已经加载的，省一次引用计数；没有就自己加载——egui 不一定会碰
    // uxtheme，进程里没加载时 GetModuleHandle 会返回空，那样整个函数就白跑了。
    let mut module = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() {
        module = unsafe { LoadLibraryW(name.as_ptr()) };
    }
    if module.is_null() {
        return;
    }

    // 序号导出：把序号本身当成 MAKEINTRESOURCE 传给 GetProcAddress。
    let proc = |ordinal: usize| unsafe { GetProcAddress(module, ordinal as *const u8) };

    unsafe {
        if let Some(f) = proc(SET_PREFERRED_APP_MODE) {
            let set: SetPreferredAppMode = std::mem::transmute(f);
            set(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        // 已经建好的菜单不会自己重画，得让 uxtheme 把主题缓存刷掉。
        if let Some(f) = proc(FLUSH_MENU_THEMES) {
            let flush: FlushMenuThemes = std::mem::transmute(f);
            flush();
        }
    }
}

#[cfg(not(windows))]
pub fn set_menu_dark_mode(_dark: bool) {}
