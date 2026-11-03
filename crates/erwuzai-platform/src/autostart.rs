//! 开机自启。
//!
//! Windows 走 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`：
//! 不需要管理员权限，也不往启动目录塞快捷方式（那样卸载时容易漏）。
//! 值是 `"<exe>" --daemon`——只拉守护进程，不会每次登录都弹窗。

use std::path::Path;

use crate::PlatformResult;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "25zai";

/// 打开 / 关闭开机自启。关的时候值不存在也算成功。
pub fn set_enabled(enabled: bool, exe: &Path) -> PlatformResult<()> {
    #[cfg(windows)]
    {
        imp::set(enabled, exe)
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, exe);
        Err(crate::PlatformError::Unsupported("开机自启目前只有 Windows 实现"))
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    use crate::{PlatformError, PlatformResult};

    use super::{RUN_KEY, VALUE_NAME};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn set(enabled: bool, exe: &Path) -> PlatformResult<()> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut hkey: HKEY = std::ptr::null_mut();

        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return Err(PlatformError::Io(std::io::Error::from_raw_os_error(rc as i32)));
        }

        let result = unsafe {
            if enabled {
                // 路径带空格时必须加引号，否则 Windows 会把 "--daemon" 当成 exe 的一部分。
                let command = format!("\"{}\" --daemon", exe.display());
                let data = wide(&command);
                RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr() as *const u8,
                    (data.len() * 2) as u32,
                )
            } else {
                RegDeleteValueW(hkey, name.as_ptr())
            }
        };

        unsafe { RegCloseKey(hkey) };

        // 关自启时值本来就不存在：这不是错误。
        if result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(PlatformError::Io(std::io::Error::from_raw_os_error(result as i32)))
        }
    }

    // windows-sys 的 HKEY 在新版本里是 *mut c_void，这里留个编译期断言，
    // 免得升级之后类型变了还得靠报错才发现。
    const _: fn() = || {
        let _: HKEY = std::ptr::null_mut::<c_void>();
    };
}
