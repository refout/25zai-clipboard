//! 系统实现：clipboard-rs 与 global-hotkey。
//!
//! 这一层只负责"把平台能力翻译成 crate 内部的类型"，不含业务判断。
//! 回声判定、敏感检测、入库都在上游做。

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use clipboard_rs::common::RustImage;
use clipboard_rs::{
    Clipboard, ClipboardContent, ClipboardContext, ClipboardHandler, ClipboardWatcher,
    ClipboardWatcherContext, ContentFormat, RustImageData, WatcherShutdown,
};
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::clipboard::{ClipboardBackend, RawClipboard, Watcher};
use crate::hotkey::{HotkeyBackend, HotkeySpec, Modifiers};
use crate::{PlatformError, PlatformResult};

// ---------------------------------------------------------------------------
// 剪贴板
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClipboard;

impl SystemClipboard {
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardBackend for SystemClipboard {
    fn read(&self) -> PlatformResult<Option<RawClipboard>> {
        let ctx = ClipboardContext::new().map_err(to_clip_err)?;
        Ok(read_from(&ctx))
    }

    fn write(&self, content: &RawClipboard) -> PlatformResult<()> {
        let ctx = ClipboardContext::new().map_err(to_clip_err)?;
        write_to(&ctx, content)
    }

    /// Windows 用 `GetClipboardSequenceNumber`，是全局单调计数器，
    /// 任何进程写剪贴板都会 +1。回声判定靠它做精确匹配。
    fn sequence(&self) -> Option<u64> {
        #[cfg(windows)]
        {
            win::sequence()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn watch(
        &self,
        sink: Arc<dyn Fn(RawClipboard) + Send + Sync>,
    ) -> PlatformResult<Box<dyn Watcher>> {
        spawn_watcher(sink)
    }
}

fn to_clip_err(e: Box<dyn std::error::Error + Send + Sync>) -> PlatformError {
    PlatformError::Clipboard(e.to_string())
}

/// 逐格式读取，优先级：文件 > 图片 > 富文本 > 纯文本。
///
/// **不要改成 `ctx.get(&[...])`**：clipboard-rs 0.3.5 的 Windows 实现里，
/// `get` 自己先 `OpenClipboard`，然后对每个格式再调 `get_files` / `get_image` /
/// `get_text`，那些函数会再次打开剪贴板。嵌套打开全部失败，函数返回空数组
/// 而不是报错——表现是"监听有事件但一条都采集不到"，极难排查。
fn read_from(ctx: &ClipboardContext) -> Option<RawClipboard> {
    // 复制文件时系统通常同时提供文本形式，先取文件才不会被降级成路径串。
    if let Ok(files) = ctx.get_files() {
        if !files.is_empty() {
            return Some(RawClipboard::Files(
                files.into_iter().map(PathBuf::from).collect(),
            ));
        }
    }

    // 快路径：源应用已经以 PNG 格式放在剪贴板上（浏览器、部分截图工具），
    // 直接取字节，完全不解码、不重编码。4K 图从 ~340 ms 降到几毫秒。
    if let Ok(png) = ctx.get_buffer("PNG") {
        if let Some((width, height)) = png_size(&png) {
            return Some(RawClipboard::Image { png, width, height });
        }
    }

    // 慢路径：只有 DIB/DIBV5。get_image 的 DIB→RGBA 解码占了绝大部分耗时
    // （实测 3840×2400：get_image 253 ms，to_png 84 ms）。
    if ctx.has(ContentFormat::Image) {
        if let Ok(image) = ctx.get_image() {
            let (width, height) = image.get_size();
            if let Ok(png) = image.to_png() {
                return Some(RawClipboard::Image {
                    png: png.get_bytes().to_vec(),
                    width,
                    height,
                });
            }
        }
    }

    if ctx.has(ContentFormat::Html) {
        if let Ok(html) = ctx.get_html() {
            if !html.is_empty() {
                return Some(RawClipboard::Rich {
                    plain: ctx.get_text().unwrap_or_default(),
                    html: Some(html),
                });
            }
        }
    }

    match ctx.get_text() {
        Ok(text) if !text.is_empty() => Some(RawClipboard::Text(text)),
        _ => None,
    }
}

fn write_to(ctx: &ClipboardContext, content: &RawClipboard) -> PlatformResult<()> {
    let result = match content {
        RawClipboard::Text(text) => ctx.set_text(text.clone()),
        // 富文本必须同时写 plain 和 html：只写 CF_HTML 的话，记事本这类
        // 只认 CF_UNICODETEXT 的应用粘出来是空的。
        RawClipboard::Rich { plain, html } => match html {
            Some(html) => {
                let mut contents = Vec::with_capacity(2);
                if !plain.is_empty() {
                    contents.push(ClipboardContent::Text(plain.clone()));
                }
                contents.push(ClipboardContent::Html(html.clone()));
                ctx.set(contents)
            }
            None => ctx.set_text(plain.clone()),
        },
        RawClipboard::Files(paths) => ctx.set_files(
            paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        ),
        RawClipboard::Image { png, .. } => {
            let image = RustImageData::from_bytes(png).map_err(to_clip_err)?;
            ctx.set_image(image)
        }
    };
    result.map_err(to_clip_err)
}

/// 从 PNG 头里读尺寸，避免为了拿宽高再去解码一遍。
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != SIG {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

struct Handler {
    ctx: ClipboardContext,
    sink: Arc<dyn Fn(RawClipboard) + Send + Sync>,
}

impl ClipboardHandler for Handler {
    fn on_clipboard_change(&mut self) {
        // 这里跑在 clipboard-rs 自己的线程上：不做哈希、不解码、不写库。
        if let Some(raw) = read_from(&self.ctx) {
            (self.sink)(raw);
        }
    }
}

/// 起一个独占线程跑监听循环。`start_watch` 是阻塞的，用 shutdown 通道停下。
pub fn spawn_watcher(
    sink: Arc<dyn Fn(RawClipboard) + Send + Sync>,
) -> PlatformResult<Box<dyn Watcher>> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<WatcherShutdown, String>>();

    let handle = std::thread::Builder::new()
        .name("25zai-clipboard".into())
        .spawn(move || {
            let ctx = match ClipboardContext::new() {
                Ok(ctx) => ctx,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let mut watcher = match ClipboardWatcherContext::new() {
                Ok(watcher) => watcher,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let shutdown = watcher
                .add_handler(Handler { ctx, sink })
                .get_shutdown_channel();
            let _ = ready_tx.send(Ok(shutdown));
            watcher.start_watch();
        })
        .map_err(PlatformError::Io)?;

    let shutdown = ready_rx
        .recv()
        .map_err(|_| PlatformError::Clipboard("监听线程启动失败".into()))?
        .map_err(PlatformError::Clipboard)?;

    Ok(Box::new(ThreadWatcher {
        shutdown: Some(shutdown),
        handle: Some(handle),
    }))
}

struct ThreadWatcher {
    shutdown: Option<WatcherShutdown>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watcher for ThreadWatcher {
    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.stop();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// 弹窗定位
// ---------------------------------------------------------------------------

/// 光标位置与它所在显示器的工作区（物理像素，已排除任务栏）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPlacement {
    /// 屏幕坐标，物理像素。
    pub cursor: (i32, i32),
    /// 工作区：x, y, width, height，物理像素。
    pub work: (i32, i32, i32, i32),
}

/// 取光标位置和所在显示器工作区。
///
/// 用 `MonitorFromPoint` 而不是主显示器尺寸：多显示器时主屏尺寸会让窗口
/// 被错误地拽回主屏，或者夹在屏幕外。
pub fn cursor_placement() -> Option<CursorPlacement> {
    #[cfg(windows)]
    {
        win::cursor_placement()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// 来源应用
// ---------------------------------------------------------------------------

/// Windows：`GetClipboardOwner` → HWND → PID → exe 文件名。
///
/// 已知局限：
/// - Electron / 浏览器常用隐藏的 clipboard helper 窗口，PID 是那个 helper 的，
///   但通常仍属于同一个进程，文件名可用。
/// - 若来源进程已退出或权限不足，`OpenProcess` 失败，返回 `None`。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSourceApp;

impl crate::source::SourceAppResolver for SystemSourceApp {
    fn resolve(&self) -> Option<String> {
        #[cfg(windows)]
        {
            win::owner_process_name()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, POINT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows_sys::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    pub fn sequence() -> Option<u64> {
        // 返回 0 表示调用失败。绝不能让 0 参与比较：
        // 两边都是 Some(0) 会被判成"同一次写入"，把用户真实的复制吞掉。
        let seq = unsafe { GetClipboardSequenceNumber() };
        (seq != 0).then_some(seq as u64)
    }

    pub fn cursor_placement() -> Option<super::CursorPlacement> {
        unsafe {
            let mut point = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut point) == 0 {
                return None;
            }

            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }

            let work = info.rcWork;
            Some(super::CursorPlacement {
                cursor: (point.x, point.y),
                work: (
                    work.left,
                    work.top,
                    work.right - work.left,
                    work.bottom - work.top,
                ),
            })
        }
    }

    pub fn owner_process_name() -> Option<String> {
        unsafe {
            let hwnd = GetClipboardOwner();
            if hwnd.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return None;
            }

            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }

            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            if ok == 0 {
                return None;
            }

            let path = OsString::from_wide(&buf[..len as usize]);
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        }
    }
}

#[cfg(all(test, windows))]
mod placement_tests {
    /// `GetCursorPos` 在锁屏 / 非交互会话下会失败，所以这里只做"取到就必须合理"的检查。
    /// 真正的定位数学在 UI 层用纯函数测试，不依赖桌面状态。
    #[test]
    fn cursor_placement_is_sane_when_available() {
        if let Some(p) = super::cursor_placement() {
            assert!(p.work.2 > 0 && p.work.3 > 0, "工作区尺寸应为正：{:?}", p.work);
            assert!(p.cursor.0.abs() < 40000 && p.cursor.1.abs() < 40000);
        }
    }
}

// ---------------------------------------------------------------------------
// 全局快捷键
// ---------------------------------------------------------------------------

pub struct SystemHotkey {
    manager: GlobalHotKeyManager,
    /// 当前注册的组合，改键时需要它来注销。
    current: std::cell::RefCell<Option<HotKey>>,
}

impl SystemHotkey {
    /// 必须在跑消息循环的线程上创建（Windows 要求）。
    /// 在 eframe 里就是 `App::new`，它在 winit 事件循环启动后、主线程上执行。
    pub fn new() -> PlatformResult<Self> {
        let manager =
            GlobalHotKeyManager::new().map_err(|e| PlatformError::Hotkey(e.to_string()))?;
        Ok(Self {
            manager,
            current: std::cell::RefCell::new(None),
        })
    }

    /// 换成新的组合。新组合注册失败时恢复旧的，不会把用户锁在外面。
    pub fn rebind(&self, spec: &HotkeySpec) -> PlatformResult<()> {
        let next = HotKey::new(Some(to_gh_modifiers(spec.modifiers)), parse_code(&spec.key)?);
        let previous = self.current.borrow_mut().take();

        if let Some(old) = previous {
            if old == next {
                *self.current.borrow_mut() = Some(old);
                return Ok(());
            }
            let _ = self.manager.unregister(old);
        }

        match self.manager.register(next) {
            Ok(()) => {
                *self.current.borrow_mut() = Some(next);
                Ok(())
            }
            Err(e) => {
                if let Some(old) = previous {
                    let _ = self.manager.register(old);
                    *self.current.borrow_mut() = Some(old);
                }
                Err(PlatformError::Hotkey(e.to_string()))
            }
        }
    }

    /// 返回本次是否有按键按下。会排空事件队列。
    pub fn poll_pressed(&self) -> bool {
        let mut pressed = false;
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state == HotKeyState::Pressed {
                pressed = true;
            }
        }
        pressed
    }
}

impl HotkeyBackend for SystemHotkey {
    fn register(&self, spec: &HotkeySpec) -> PlatformResult<()> {
        self.rebind(spec)
    }

    fn unregister(&self) -> PlatformResult<()> {
        // 进程退出时随 manager 一起释放。
        Ok(())
    }
}

/// `"V"` → `KeyV`，`"1"` → `Digit1`，`"Space"` 原样交给 FromStr。
fn parse_code(key: &str) -> PlatformResult<Code> {
    let normalized = {
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                format!("Key{}", c.to_ascii_uppercase())
            }
            (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
            _ => key.to_string(),
        }
    };
    normalized
        .parse::<Code>()
        .map_err(|_| PlatformError::Malformed(format!("无法识别的按键：{key}")))
}

fn to_gh_modifiers(mods: Modifiers) -> global_hotkey::hotkey::Modifiers {
    use global_hotkey::hotkey::Modifiers as Gh;
    let mut out = Gh::empty();
    if mods.contains(Modifiers::CTRL) {
        out |= Gh::CONTROL;
    }
    if mods.contains(Modifiers::ALT) {
        out |= Gh::ALT;
    }
    if mods.contains(Modifiers::SHIFT) {
        out |= Gh::SHIFT;
    }
    if mods.contains(Modifiers::SUPER) {
        out |= Gh::META;
    }
    out
}
