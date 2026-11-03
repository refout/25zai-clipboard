//! 开机自启。
//!
//! 两种分发形态，走两条完全不同的路：
//!
//! - 绿色版 / 安装版（普通 exe）：写 HKCU 的 Run 键，值是 "<exe>" --daemon。
//!   不需要管理员权限，也不往启动目录塞快捷方式（卸载时容易漏）。
//! - Microsoft Store（MSIX 全信任）：注册表会被虚拟化，Run 键写了也不生效，
//!   必须用清单里声明的 desktop:StartupTask，运行时调 WinRT StartupTask 开关。
//!
//! 判断是不是包内：StartupTask 能取到就是包内，取不到就退回注册表。

use std::path::Path;

use crate::PlatformResult;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "25zai";
/// 必须和 AppxManifest.xml 里 desktop:StartupTask 的 TaskId 一致。
#[cfg(windows)]
const STARTUP_TASK_ID: &str = "25zaiDaemon";

/// 打开 / 关闭开机自启。关的时候值不存在也算成功。
pub fn set_enabled(enabled: bool, exe: &Path) -> PlatformResult<()> {
    #[cfg(windows)]
    {
        // 包内优先走 StartupTask；不是包（取不到任务）才写注册表。
        if let Some(result) = imp::startup_task(enabled) {
            return result;
        }
        imp::set_registry(enabled, exe)
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

    use super::{RUN_KEY, STARTUP_TASK_ID, VALUE_NAME};

    /// 包内返回 Some(结果)，不是包返回 None（据此退回注册表）。
    pub fn startup_task(enabled: bool) -> Option<PlatformResult<()>> {
        use windows::core::HSTRING;
        use windows::ApplicationModel::{StartupTask, StartupTaskState};

        // WinRT 激活要求线程已初始化；MTA 对后台线程最省事。已经初始化过
        // （比如托盘先建了）会返回 RPC_E_CHANGED_MODE，忽略即可。
        unsafe {
            windows_sys::Win32::System::Com::CoInitializeEx(
                std::ptr::null(),
                windows_sys::Win32::System::Com::COINIT_MULTITHREADED as u32,
            );
        }

        // 不是包时这里会失败（无包标识 / 找不到任务），于是返回 None。
        let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID))
            .ok()?
            .get()
            .ok()?;

        Some(if enabled {
            match task
                .RequestEnableAsync()
                .ok()
                .and_then(|op| op.get().ok())
            {
                Some(StartupTaskState::Enabled) => Ok(()),
                Some(state) => Err(PlatformError::Autostart(format!(
                    "系统没有允许开机自启（{state:?}），可能被用户或组策略关掉了"
                ))),
                None => Err(PlatformError::Autostart("启用开机自启失败".into())),
            }
        } else {
            match task.Disable() {
                Ok(()) => Ok(()),
                Err(e) => Err(PlatformError::Autostart(format!("关闭开机自启失败：{e}"))),
            }
        })
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 非 MSIX 分发：写 HKCU 的 Run 键。
    pub fn set_registry(enabled: bool, exe: &Path) -> PlatformResult<()> {
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
