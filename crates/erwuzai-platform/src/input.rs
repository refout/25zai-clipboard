//! 把回放的内容直接送进光标所在的窗口：焦点还回去，再补一次 Ctrl+V。
//!
//! 顺序不能反——先切前台再敲键。窗口刚 Visible(false) 的时候进程还握着
//! 前台权，此时 SetForegroundWindow 是被允许的；等系统把前台判给别人之后
//! 再切就会被前台锁挡掉，按键也就掉到桌面上了。
//!
//! 非 Windows 平台是空操作，调用方不用分平台。

/// 当前前台窗口的句柄；拿不到返回 0。
pub fn foreground_window() -> isize {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as isize
    }
    #[cfg(not(windows))]
    0
}

/// 前台窗口是不是本进程的。
pub fn foreground_is_ours() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid == std::process::id()
    }
    #[cfg(not(windows))]
    false
}

/// 激活 `window` 并发送 Ctrl+V；`restore` 非 0 时粘完把焦点还给它。
///
/// 窗口钉住时它一直开着、焦点也在我们这边，不先切走的话 Ctrl+V 只会粘给自己，
/// 所以那种情况要把焦点借给目标窗口，粘完再还回来。
///
/// 里面的等待是给三件事留时间：窗口藏下去、焦点切过去、粘完焦点再回来。
/// 调用方应当把它放到后台线程里，别阻塞渲染。
pub fn paste_into(window: isize, restore: isize) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_V,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

        std::thread::sleep(std::time::Duration::from_millis(80));
        unsafe {
            if window == 0 {
                return;
            }
            // 已经是前台就不用再切（切一次会把它顶到最上层）。
            if windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() != window as _ {
                SetForegroundWindow(window as _);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe {
            // 焦点没真的搬过去就别敲键——否则 Ctrl+V 会落回我们自己窗口，
            // 把内容粘进搜索框。
            if windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() != window as _ {
                return;
            }
        }

        let key = |vk: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let keys = [
            key(VK_CONTROL, false),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_CONTROL, true),
        ];
        unsafe {
            SendInput(
                keys.len() as u32,
                keys.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }

        if restore != 0 {
            std::thread::sleep(std::time::Duration::from_millis(80));
            unsafe {
                SetForegroundWindow(restore as _);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (window, restore);
}
