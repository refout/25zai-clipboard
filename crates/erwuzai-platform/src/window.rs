//! 窗口几何。
//!
//! 为什么不直接用 egui 的 `inner_rect` / `outer_rect`：那是"按窗口所在显示器
//! 的缩放换算过的逻辑点"，在多显示器 + 混合 DPI 下再乘回 `pixels_per_point`
//! 未必等于真实物理坐标。摆位要拿它和 Win32 报的矩形对齐，必须两边同一套坐标。

/// 按标题找本程序的窗口，返回它在屏幕上的物理像素矩形 `(x, y, w, h)`。
#[cfg(windows)]
pub fn window_rect(title: &str) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), wide.as_ptr());
        if hwnd.is_null() {
            return None;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        (w > 0 && h > 0).then_some((rect.left, rect.top, w, h))
    }
}

/// 抽掉 eframe 之后，托盘和全局热键的消息没人再泵了——守护进程要自己跑一轮。
///
/// 只处理**已经排队**的消息（PeekMessageW + PM_REMOVE），拿到就分发，
/// 没消息立刻返回，调用方自己决定睡多久。
pub fn pump_messages() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
pub fn window_rect(_title: &str) -> Option<(i32, i32, i32, i32)> {
    None
}
