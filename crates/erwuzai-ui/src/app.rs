//! 弹窗主体：搜索、虚拟列表、键盘导航、设置面板、全局热键。

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui;
use erwuzai_platform::{ClipboardBackend, HotkeySpec, SystemClipboard};
use erwuzai_store::{
    Config, Content, EntryId, EntryView, Kind, Page, PopupPosition, Reply, StoreCommand,
    StoreEvent, StoreHandle, ThemeMode, UiRequest,
};

use crate::listener;

/// 启动后多久才开始响应失焦。刚起来的窗口 `focused` 可能还是 false，
/// 不设这个门槛会自己把自己藏掉。
const BLUR_GRACE: Duration = Duration::from_millis(500);
/// 搜索停顿多久后触发全量兜底检索。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
/// 隐藏期间的轮询间隔。
///
/// 窗口一藏，eframe 就退到 `ControlFlow::Wait`：没有重绘请求就不再跑帧。
/// 而热键、托盘、第二实例全是**通过通道**送进来的，不跑帧等于永远收不到，
/// 表现就是「藏下去之后按快捷键再也唤不出来」。挂一个定时重绘让循环继续转。
const HIDDEN_POLL: Duration = Duration::from_millis(120);
/// 预览最多显示几行。超出部分由 TextWrapping 自动省略。
const PREVIEW_MAX_ROWS: usize = 5;

/// 内容与窗口边框的间距。右边这条留白同时是滚动条的落脚处：
/// 滚动条贴边框，内容退到这里面。
const EDGE_PAD: f32 = 14.0;
const EDGE_PAD_Y: i8 = 12;
/// 行尾「钉住这条」按钮占的宽度。预览正文必须让出这段，
/// 否则长文本会把按钮顶出可视区。
const ROW_ACTION_W: f32 = 18.0;
/// 图片缩略图在列表里的边长（逻辑点）。
const THUMB_SIZE: f32 = 40.0;
/// 缩略图纹理的解码上限（像素）。按 2 倍屏留余量，别把 4K 截图整张塞进显存。
const THUMB_TEX: u32 = 96;
/// 行与行之间的空隙。挨太紧时一眼看过去是一整块文字，找不到行的边界。
const ROW_GAP: f32 = 6.0;

/// 需要右内边距的区块：把可用宽度收窄 `EDGE_PAD`，而不是给外层加边框。
/// 外层一旦有右内边距，滚动条就会跟着缩进去，看起来悬在中间。
fn inset_right<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    ui.scope(|ui| {
        ui.set_max_width((ui.available_width() - EDGE_PAD).max(1.0));
        add(ui);
    });
}
/// 送进排版的字符上限。5 行根本装不下 500 字，截到 240 能显著减少
/// galley 缓存体积和每次排版的工作量。
const PREVIEW_LAYOUT_CHARS: usize = 240;

/// 列表一行的渲染数据。预先拷出来，循环里才好改 `self`。
struct Row {
    id: EntryId,
    preview: String,
    pinned: bool,
    kind: Kind,
    /// 图片条目的 blob 路径；非图片为空。
    blob_path: Option<std::path::PathBuf>,
}

/// 设置页分页。分页只是视图状态，不落盘。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    #[default]
    Appearance,
    Behavior,
    Capacity,
    Trigger,
    About,
}

impl SettingsTab {
    const ALL: [Self; 5] = [
        Self::Appearance,
        Self::Behavior,
        Self::Capacity,
        Self::Trigger,
        Self::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Behavior => "行为",
            Self::Capacity => "容量",
            Self::Trigger => "唤起",
            Self::About => "关于",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum KindFilter {
    #[default]
    All,
    Text,
    Rich,
    Image,
    File,
}

impl KindFilter {
    fn matches(self, kind: Kind) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Text => kind == Kind::Text,
            KindFilter::Rich => kind == Kind::RichText,
            KindFilter::Image => kind == Kind::Image,
            KindFilter::File => kind == Kind::FileList,
        }
    }

    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "全部",
            KindFilter::Text => "文本",
            KindFilter::Rich => "富文本",
            KindFilter::Image => "图片",
            KindFilter::File => "文件",
        }
    }
}

pub struct App {
    handle: StoreHandle,
    events: Receiver<StoreEvent>,
    entries: Vec<EntryView>,
    /// 内存索引之外、由存储层兜底搜索命中的条目。
    search_extra: Vec<EntryView>,
    query: String,
    kind_filter: KindFilter,
    /// `None` 表示不按来源过滤。
    source_filter: Option<String>,
    last_query: String,
    last_edit: Instant,
    query_gen: u64,
    in_flight: Option<(u64, Reply<Vec<EntryView>>)>,
    selected: usize,
    /// 键盘移动后需要确保这一行可见（本帧消费）。
    scroll_to: Option<usize>,
    focus_requested: bool,
    started: Instant,
    /// 本次唤出后是否真的拿到过焦点。没见过焦点就判定"失焦"会把自己立刻藏起来。
    seen_focus: bool,
    hide_on_blur: bool,
    /// 用户手动钉住窗口：失焦不隐藏。每次唤出重置，避免下次意外常驻。
    window_pinned: bool,
    /// 唤起前的前台窗口。点条目时把内容粘回去，就是粘进这个窗口。
    paste_target: isize,
    show_settings: bool,
    /// 首帧也要定位到光标，否则第一次出现还是屏幕中央。
    placed_once: bool,
    config: Config,
    config_dirty: bool,
    /// 主题 / 字号变了，下一帧重设样式。
    appearance_dirty: bool,
    /// 上一帧的深色标记，用来决定要不要去动 uxtheme（见 `set_menu_dark_mode`）。
    /// `None` 表示还没同步过，首帧一定同步一次。
    menu_dark: Option<bool>,
    /// 设置页当前分页。
    settings_tab: SettingsTab,
    /// 关于页的图标，首次打开时解码一次。
    about_icon: Option<egui::TextureHandle>,
    /// 图片条目的缩略图。按条目 id 缓存，`None` 表示这张图读不出来（文件被清掉了），
    /// 记住失败结果，免得每帧都去重试一次解码。
    thumbs: std::collections::HashMap<EntryId, Option<egui::TextureHandle>>,
    /// 本帧已经解码了几张缩略图。整张 PNG 解码不便宜，一帧最多解一张，
    /// 让列表一帧一帧地把图补齐，而不是开窗时卡一下。
    thumb_loads: u8,
    /// 窗口当前是否可见。
    visible: bool,
    /// 启动首帧：先摆好位置再露出来，避免从默认位置跳一下。
    startup: bool,
    /// 期望的窗口位置（物理像素）。摆完之后拿真实位置对一次，
    /// 差得多就补发一次修正——见 `settle_position`。
    pending_pos: Option<(i32, i32)>,
    place_attempts: u8,
    total_visible: usize,
    hotkey_error: Option<String>,
    backend: SystemClipboard,
    /// 守护进程推过来的控制指令：唤起 / 打开设置 / 退出。
    ui_rx: Receiver<UiRequest>,
    /// 录键模式：下一个按下的组合直接生效。
    recording: bool,
}

impl App {
    pub fn new(
        handle: StoreHandle,
        events: Receiver<StoreEvent>,
        ui_rx: Receiver<UiRequest>,
        config: Config,
        page: Page,
    ) -> Self {
        // 热键和托盘都在守护进程里，"叫回来"的手段一定存在，
        // 除非调试时显式要求常驻。
        let hide_on_blur = std::env::var("25ZAI_KEEP_OPEN").is_err();

        Self {
            handle,
            events,
            total_visible: page.total_visible,
            entries: page.items,
            search_extra: Vec::new(),
            query: String::new(),
            kind_filter: KindFilter::All,
            source_filter: None,
            last_query: String::new(),
            last_edit: Instant::now(),
            query_gen: 0,
            in_flight: None,
            selected: 0,
            scroll_to: None,
            focus_requested: false,
            started: Instant::now(),
            seen_focus: false,
            hide_on_blur,
            window_pinned: false,
            paste_target: 0,
            show_settings: false,
            placed_once: false,
            config,
            config_dirty: false,
            // 启动时先套用一次，否则用户上次选的主题要等下次改才生效。
            appearance_dirty: true,
            menu_dark: None,
            settings_tab: SettingsTab::Appearance,
            about_icon: None,
            thumbs: std::collections::HashMap::new(),
            thumb_loads: 0,
            visible: false,
            startup: true,
            pending_pos: None,
            place_attempts: 0,
            // 注册结果由守护进程决定，UI 这边只在改键失败时显示错误。
            hotkey_error: None,
            backend: SystemClipboard::new(),
            ui_rx,
            recording: false,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                StoreEvent::Added(items) => {
                    for item in items {
                        self.entries.insert(0, item);
                        self.total_visible += 1;
                    }
                }
                StoreEvent::Moved {
                    id,
                    seq,
                    last_used_at,
                } => {
                    if let Some(pos) = self.entries.iter().position(|e| e.entry.id == id) {
                        let mut entry = self.entries.remove(pos);
                        entry.seq = seq;
                        entry.last_used_at = last_used_at;
                        self.entries.insert(0, entry);
                    }
                }
                StoreEvent::Removed(ids) => {
                    self.entries.retain(|e| !ids.contains(&e.entry.id));
                    self.search_extra.retain(|e| !ids.contains(&e.entry.id));
                    self.total_visible = self.total_visible.saturating_sub(ids.len());
                }
                StoreEvent::ConfigChanged(_) => {}
                StoreEvent::Warning(w) => eprintln!("25zai: {w}"),
            }
        }
    }

    /// 类型 + 来源过滤，与关键词无关。
    fn passes_filters(&self, entry: &EntryView) -> bool {
        if !self.kind_filter.matches(entry.entry.kind) {
            return false;
        }
        if let Some(want) = &self.source_filter {
            if entry.entry.source_app.as_deref() != Some(want.as_str()) {
                return false;
            }
        }
        true
    }

    /// 类型 + 来源 + 关键词。
    fn passes(&self, entry: &EntryView, needle: &str) -> bool {
        self.passes_filters(entry)
            && (needle.is_empty() || entry.entry.preview.to_lowercase().contains(needle))
    }

    fn visible(&self) -> Vec<&EntryView> {
        let needle = self.query.trim().to_lowercase();
        let mut rows: Vec<&EntryView> = self
            .entries
            .iter()
            .filter(|e| self.passes(e, &needle))
            .collect();
        for extra in &self.search_extra {
            // 存储层返回的结果可能只命中正文，预览里没有关键词——
            // 这里不能再按关键词过滤一遍，否则正文命中会被全部丢掉。
            if self.passes_filters(extra) && !rows.iter().any(|r| r.entry.id == extra.entry.id) {
                rows.push(extra);
            }
        }
        rows
    }

    /// 列表里出现过的来源应用，用于过滤下拉。
    fn known_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| e.entry.source_app.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn poll_search(&mut self) {
        if self.query != self.last_query {
            self.last_query = self.query.clone();
            self.last_edit = Instant::now();
            self.query_gen = self.query_gen.wrapping_add(1);
            self.search_extra.clear();
            self.in_flight = None;
        }

        if let Some((gen, rx)) = &self.in_flight {
            match rx.try_recv() {
                Ok(Ok(items)) => {
                    if *gen == self.query_gen {
                        self.search_extra = items;
                    }
                    self.in_flight = None;
                }
                Ok(Err(e)) => {
                    eprintln!("25zai: 搜索失败：{e}");
                    self.in_flight = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.in_flight = None,
            }
        }

        let needle = self.query.trim();
        if needle.is_empty() || self.in_flight.is_some() {
            return;
        }
        if self.last_edit.elapsed() < SEARCH_DEBOUNCE {
            return;
        }
        // 内存索引已经够用就不打扰存储层。
        if self.memory_hits() >= 50 {
            return;
        }

        let (cmd, rx) = StoreCommand::search(needle, 200);
        if self.handle.submit(cmd) {
            self.in_flight = Some((self.query_gen, rx));
        }
    }

    fn memory_hits(&self) -> usize {
        let needle = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| self.passes(e, &needle))
            .count()
    }

    fn hide(&mut self, ctx: &egui::Context) {
        // 钉住窗口时不接受任何"关闭"：失焦、Esc、点条目回放都不关。
        // 要关就先点顶栏图钉解除，或者走设置里的退出。
        if self.window_pinned {
            return;
        }
        // 拆分之后"隐藏"就是退出整个 UI 进程——窗口留着只会继续占着一百多 MB。
        self.visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn show(&mut self, ctx: &egui::Context) {
        // 记下唤起前的前台窗口，点条目时往它里面粘。已经可见时前台就是我们
        // 自己，不能覆盖——否则会把内容粘回剪贴板管理器。
        if !self.visible {
            // 刚藏下去的一瞬间前台可能还是我们自己，那种情况记下来只会粘回自己。
            self.paste_target = if erwuzai_platform::foreground_is_ours() {
                0
            } else {
                erwuzai_platform::foreground_window()
            };
        }
        self.query.clear();
        self.selected = 0;
        self.focus_requested = false;
        self.show_settings = false;
        self.window_pinned = false;
        // 重新开始一轮"先拿到焦点、再谈失焦"的判定。
        self.seen_focus = false;
        self.started = Instant::now();
        self.visible = true;
        self.place_window(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// 按配置把窗口摆到光标处或居中。以**光标所在显示器的工作区**为边界，
    /// 所以多显示器和任务栏都能正确处理。
    fn place_window(&mut self, ctx: &egui::Context) {
        if self.config.popup_position == PopupPosition::Remembered {
            return;
        }
        let Some(placement) = erwuzai_platform::cursor_placement() else {
            return;
        };

        // 全程用物理像素算：光标、工作区、窗口尺寸都来自 Win32，
        // 只有最后发命令时才换成逻辑点。混着用逻辑点算会踩到
        // "第一帧的 pixels_per_point 还是旧显示器"的坑，窗口会偏一个缩放比。
        let Some((_, _, ww, wh)) = erwuzai_platform::window_rect("25zai") else {
            return;
        };
        let pos = place_in_work_area(
            (placement.cursor.0 as f32, placement.cursor.1 as f32),
            (
                placement.work.0 as f32,
                placement.work.1 as f32,
                placement.work.2 as f32,
                placement.work.3 as f32,
            ),
            egui::vec2(ww as f32, wh as f32),
            self.config.popup_position,
        );

        // 记下物理目标：egui 的 OuterPosition 是逻辑点，多显示器 + 混合 DPI 下
        // "调用时的缩放系数"和"真正应用时的"未必一致，整体会偏一个缩放比。
        // 摆完再按真实矩形校准一次，比猜系数可靠。
        self.pending_pos = Some((pos.x.round() as i32, pos.y.round() as i32));
        self.place_attempts = 0;
        let ppp = ctx.pixels_per_point().max(0.5);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos / ppp));
    }

    /// 把窗口真正摆到目标位置：拿 Win32 报的物理矩形和期望值比，
    /// 差得多就按当前缩放补发一次 OuterPosition（最多三次，避免来回抖）。
    fn settle_position(&mut self, ctx: &egui::Context) {
        let Some((tx, ty)) = self.pending_pos else { return };
        let Some((wx, wy, _, _)) = erwuzai_platform::window_rect("25zai") else { return };

        let (dx, dy) = (tx - wx, ty - wy);
        if dx.abs() <= 2 && dy.abs() <= 2 {
            self.pending_pos = None;
            return;
        }
        if self.place_attempts >= 3 {
            self.pending_pos = None;
            return;
        }
        self.place_attempts += 1;

        let ppp = ctx.pixels_per_point().max(0.5);
        let cur = ctx
            .input(|i| i.viewport().outer_rect.map(|r| r.min))
            .unwrap_or(egui::Pos2::ZERO);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
            cur + egui::vec2(dx as f32 / ppp, dy as f32 / ppp),
        ));
    }

    /// 取（必要时解码）一张缩略图。缓存里没有就现读，一帧最多读一张。
    ///
    /// 解码的是原图（可能是 4K 截图），所以先 `thumbnail` 缩到 96px 再上传，
    /// 纹理和显存都只占一点点。读不出来就记一个 `None`，不再重试。
    fn thumbnail(
        &mut self,
        ctx: &egui::Context,
        id: EntryId,
        path: Option<&Path>,
    ) -> Option<egui::TextureHandle> {
        if let Some(hit) = self.thumbs.get(&id) {
            return hit.clone();
        }
        if self.thumb_loads >= 1 {
            // egui 不会自己连续重绘：这一帧没轮到的，把下一帧要过来，
            // 否则整屏只会有一张缩略图。
            ctx.request_repaint();
            return None;
        }
        self.thumb_loads += 1;

        let texture = path.and_then(|path| {
            // blob 文件名是哈希、没有扩展名，image::open 按扩展名判格式会失败，
            // 所以这里显式按内容嗅探。
            let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
            let image = reader.decode().ok()?;
            let rgba = image.thumbnail(THUMB_TEX, THUMB_TEX).to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            Some(ctx.load_texture(
                format!("thumb-{id}"),
                pixels,
                egui::TextureOptions::LINEAR,
            ))
        });

        // 列表最多 500 条，图多了就整批丢掉重来——比维护 LRU 划算。
        if self.thumbs.len() > 400 {
            self.thumbs.clear();
        }
        self.thumbs.insert(id, texture.clone());
        texture
    }

    /// 守护进程推过来的指令：热键、托盘、第二实例都从这条通路进来。
    fn poll_ui(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.ui_rx.try_recv() {
            match cmd {
                UiRequest::Show => self.show(ctx),
                UiRequest::ShowSettings => {
                    self.show(ctx);
                    self.show_settings = true;
                }
                UiRequest::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }

    fn replay(&mut self, id: EntryId, ctx: &egui::Context, as_plain: bool) {
        let (cmd, reply) = StoreCommand::content(id);
        if !self.handle.submit(cmd) {
            return;
        }
        let Ok(Ok(Some(content))) = reply.recv_timeout(Duration::from_millis(300)) else {
            return;
        };
        let Some(mut raw) = content_to_raw(content) else {
            return;
        };
        if as_plain {
            raw = to_plain(raw);
        }

        // 哈希必须基于"最终写进去的字节"，纯文本转换之后算。
        let hash = listener::hash_raw(&raw);
        // 回声抑制的守卫在守护进程里（监听线程也在那边），这里只是通知它布防。
        self.handle.arm_echo(Some(hash));
        if let Err(e) = self.backend.write(&raw) {
            self.handle.arm_echo(None);
            eprintln!("25zai: 写剪贴板失败：{e}");
            return;
        }

        let (cmd, _reply) = StoreCommand::touch(id);
        self.handle.submit(cmd);
        // 钉住时窗口不隐藏、焦点还在自己身上：得先把焦点借给目标窗口，
        // 粘完再还回来，否则整窗就"失活"了，用户还得再点一下。
        let (paste_to, restore) = if self.config.paste_on_click {
            let restore = if self.window_pinned && erwuzai_platform::foreground_is_ours() {
                erwuzai_platform::foreground_window()
            } else {
                0
            };
            (self.paste_target, restore)
        } else {
            (0, 0)
        };
        self.hide(ctx);
        if paste_to != 0 {
            // 等窗口真的藏下去、焦点回落，再敲 Ctrl+V。放后台线程，别卡渲染。
            std::thread::spawn(move || erwuzai_platform::paste_into(paste_to, restore));
        }
    }

    /// 应用一个新组合。失败时保留原来那个，并显示原因。
    fn apply_spec(&mut self, spec: HotkeySpec) {
        // 热键归守护进程所有：注册失败时它会把旧的装回去，这里只负责显示结果。
        let text = spec.to_string();
        match self.handle.set_hotkey(&text) {
            Ok(()) => {
                self.hotkey_error = None;
                self.config.hotkey = text;
                self.config_dirty = true;
            }
            Err(e) => self.hotkey_error = Some(format!("注册失败：{e}")),
        }
    }

    /// 录键模式：截获下一次"非修饰键"的按下，直接组合成快捷键。
    ///
    /// Ctrl / Shift / Alt 本身也会作为 `Event::Key` 出现（ShiftLeft、ControlRight…），
    /// 必须跳过，否则按下 Ctrl 的瞬间就被当成"你选了 Ctrl 这个键"。
    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        if !self.recording {
            return;
        }

        let mut hit: Option<egui::Key> = None;
        let mut modifiers = egui::Modifiers::default();
        let mut cancel = false;

        ctx.input(|i| {
            for event in &i.events {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers: m,
                    ..
                } = event
                else {
                    continue;
                };
                if is_modifier_key(*key) {
                    continue;
                }
                if *key == egui::Key::Escape && !(m.alt || m.ctrl || m.shift || m.command) {
                    cancel = true;
                    return;
                }
                hit = Some(*key);
                modifiers = *m;
                return;
            }
        });

        if cancel {
            self.recording = false;
            return;
        }
        let Some(key) = hit else { return };

        let mut mods = erwuzai_platform::Modifiers::NONE;
        if modifiers.ctrl || modifiers.command {
            mods |= erwuzai_platform::Modifiers::CTRL;
        }
        if modifiers.alt {
            mods |= erwuzai_platform::Modifiers::ALT;
        }
        if modifiers.shift {
            mods |= erwuzai_platform::Modifiers::SHIFT;
        }
        if mods.is_empty() {
            self.hotkey_error = Some("必须带 Ctrl / Alt / Shift".to_string());
            return;
        }

        self.recording = false;
        self.apply_spec(HotkeySpec {
            modifiers: mods,
            key: key_name(key),
        });
    }

    /// 主题、主题色、字号。只在实际变化时调用——`style_mut_of` 会改全局样式，
    /// 每帧重设既浪费又容易和 egui 自己的状态打架。
    fn apply_appearance(&self, ctx: &egui::Context) {
        ctx.set_theme(match self.config.theme {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        });

        let a = self.config.accent;
        let accent = egui::Color32::from_rgb(a[0], a[1], a[2]);
        let scale = self.config.font_scale.clamp(0.8, 1.6);
        // 以 egui 默认字号为基准，避免"改两次字号翻两倍"。
        let base = egui::Style::default().text_styles;


        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.style_mut_of(theme, |style| {
                style.visuals.selection.bg_fill = accent;
                style.visuals.selection.stroke.color = egui::Color32::WHITE;
                style.visuals.hyperlink_color = accent;
                style.visuals.widgets.active.bg_fill = accent;

                // egui 0.37 的默认弱化色是"正文色 × 0.6 的透明度"：
                // 深色下算出来接近 #3F3F3F，压在 #1B1B1B 的面板上基本看不见。
                //
                // 文字一律中性色，不掺主题色：彩色文字看久了会脏，
                // 层级靠字号和亮度区分就够了。
                let dark = style.visuals.dark_mode;
                style.visuals.weak_text_color = Some(if dark {
                    egui::Color32::from_gray(185)
                } else {
                    egui::Color32::from_gray(95)
                });
                // 正文提到接近纯白 / 纯黑，灰色只留给真正次要的位置。
                style.visuals.widgets.noninteractive.fg_stroke.color = if dark {
                    egui::Color32::from_gray(238)
                } else {
                    egui::Color32::from_gray(24)
                };
                for (kind, base_font) in &base {
                    if let Some(font) = style.text_styles.get_mut(kind) {
                        font.size = (base_font.size * scale).round();
                    }
                }
            });
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        // 顶栏固定：标题 + 返回。
        inset_right(ui, |ui| ui.horizontal(|ui| {
            ui.heading("设置");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 和列表页的“设置”按钮同尺寸，来回切换时不会跳高矮。
                if ui
                    .add_sized([48.0, 28.0], egui::Button::new("返回"))
                    .clicked()
                {
                    self.show_settings = false;
                    self.focus_requested = false;
                }
            });
        }));
        // 分页条：四个分组平铺，点击即切，不再让用户在一长串里找。
        ui.add_space(6.0);
        inset_right(ui, |ui| ui.horizontal(|ui| {
            for tab in SettingsTab::ALL {
                let selected = self.settings_tab == tab;
                if ui
                    .add_sized(
                        [64.0, 26.0],
                        egui::Button::new(tab.label()).selected(selected),
                    )
                    .clicked()
                {
                    self.settings_tab = tab;
                }
            }
        }));
        ui.add_space(4.0);
        inset_right(ui, |ui| ui.separator());

        // 中间可滚动，底部动作区固定——否则小窗口里"退出"永远看不到。
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width((ui.available_width() - EDGE_PAD).max(40.0));
                match self.settings_tab {
                    SettingsTab::Appearance => self.appearance_section(ui),
                    SettingsTab::Behavior => self.behavior_section(ui),
                    SettingsTab::Capacity => self.capacity_section(ui),
                    SettingsTab::Trigger => self.trigger_section(ui),
                    SettingsTab::About => self.about_section(ui),
                }
            });

        inset_right(ui, |ui| ui.separator());
        inset_right(ui, |ui| ui.horizontal(|ui| {
            if ui.button("清空历史").clicked() {
                let (cmd, _reply) = StoreCommand::clear_unpinned();
                self.handle.submit(cmd);
                self.show_settings = false;
                self.query.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("退出 25仔").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        }));
    }

    fn about_section(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.about_icon.is_none() {
            self.about_icon = load_about_icon(&ctx);
        }

        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            if let Some(icon) = &self.about_icon {
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::new(
                        icon.id(),
                        egui::vec2(84.0, 103.0),
                    ))
                    .corner_radius(egui::CornerRadius::same(10)),
                );
            }
            ui.add_space(10.0);
            ui.heading("25仔");
            ui.add_space(2.0);
            ui.weak(format!("版本 {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(6.0);
            ui.weak("剪贴板历史，本地存储，随时唤起。");
        });

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        egui::Grid::new("about-facts")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("快捷键");
                ui.weak(self.config.hotkey.clone());
                ui.end_row();

                ui.label("历史条目");
                ui.weak(format!("{} 条", self.total_visible));
                ui.end_row();

                ui.label("数据");
                ui.weak("只存在本机（redb + 图片文件），不上传");
                ui.end_row();

                ui.label("许可证");
                ui.weak("MIT");
                ui.end_row();
            });
    }

    fn appearance_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings-appearance")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label("主题");
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (ThemeMode::System, "跟随系统"),
                        (ThemeMode::Light, "明亮"),
                        (ThemeMode::Dark, "黑暗"),
                    ] {
                        if ui.selectable_label(self.config.theme == mode, label).clicked() {
                            self.config.theme = mode;
                            self.config_dirty = true;
                            self.appearance_dirty = true;
                        }
                    }
                });
                ui.end_row();

                ui.label("主题色");
                ui.horizontal(|ui| {
                    let mut accent = self.config.accent;
                    if ui.color_edit_button_srgb(&mut accent).changed() {
                        self.config.accent = accent;
                        self.config_dirty = true;
                        self.appearance_dirty = true;
                    }
                    ui.weak(format!(
                        "#{:02X}{:02X}{:02X}",
                        self.config.accent[0], self.config.accent[1], self.config.accent[2]
                    ));
                });
                ui.end_row();

                ui.label("字号");
                ui.horizontal(|ui| {
                    for (scale, label) in [(0.9_f32, "小"), (1.0, "标准"), (1.15, "大")] {
                        let selected = (self.config.font_scale - scale).abs() < 0.01;
                        if ui.selectable_label(selected, label).clicked() {
                            self.config.font_scale = scale;
                            self.config_dirty = true;
                            self.appearance_dirty = true;
                        }
                    }
                });
                ui.end_row();
            });
    }

    fn behavior_section(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.config.replay_reorders, "回放后排到第一")
            .changed()
        {
            self.config_dirty = true;
        }
        if ui
            .checkbox(&mut self.config.replay_as_plain, "回放只保留纯文本")
            .changed()
        {
            self.config_dirty = true;
        }
        ui.weak("也可以回放时按 Shift+Enter 临时生效");

        if ui
            .checkbox(&mut self.config.paste_on_click, "点条目后直接粘贴到光标处")
            .changed()
        {
            self.config_dirty = true;
        }
        ui.weak("关闭则只把内容放回剪贴板，等你自己按 Ctrl+V");

        ui.add_space(6.0);
        if ui
            .checkbox(&mut self.config.skip_sensitive, "跳过疑似密钥")
            .changed()
        {
            self.config_dirty = true;
        }

        ui.add_space(8.0);
        ui.label("排除应用（逗号分隔）");
        let mut apps = self.config.excluded_apps.join(", ");
        if ui
            .add_sized(
                [ui.available_width(), 24.0],
                egui::TextEdit::singleline(&mut apps),
            )
            .changed()
        {
            self.config.excluded_apps = apps
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            self.config_dirty = true;
        }
    }

    fn capacity_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings-capacity")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label("条数上限");
                let mut limit = self.config.limit as u32;
                if ui
                    .add(egui::Slider::new(&mut limit, 100..=5000).suffix(" 条"))
                    .changed()
                {
                    self.config.limit = limit as usize;
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("时间清理");
                ui.horizontal(|ui| {
                    let mut retention = self.config.retention_days;
                    let mut enabled = retention.is_some();
                    if ui.checkbox(&mut enabled, "启用").changed() {
                        retention = if enabled { Some(30) } else { None };
                        self.config_dirty = true;
                    }
                    if let Some(days) = retention.as_mut() {
                        if ui
                            .add(egui::Slider::new(days, 1..=365).suffix(" 天"))
                            .changed()
                        {
                            self.config_dirty = true;
                        }
                    }
                    self.config.retention_days = retention;
                });
                ui.end_row();
            });
        ui.weak("固定条目既不计入上限，也不会被清理");
    }

    fn trigger_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings-trigger")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label("快捷键");
                ui.horizontal(|ui| {
                    let label = if self.recording {
                        "按下组合键…".to_string()
                    } else {
                        self.config.hotkey.clone()
                    };
                    if ui
                        .add_sized([130.0, 26.0], egui::Button::new(label))
                        .clicked()
                    {
                        self.recording = !self.recording;
                        self.hotkey_error = None;
                    }
                    ui.weak(if self.recording {
                        "按 Esc 取消"
                    } else {
                        "点击后按下新组合"
                    });
                });
                ui.end_row();

                ui.label("弹出位置");
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (PopupPosition::Cursor, "跟随光标"),
                        (PopupPosition::Center, "居中"),
                    ] {
                        if ui
                            .selectable_label(self.config.popup_position == mode, label)
                            .clicked()
                        {
                            self.config.popup_position = mode;
                            self.config_dirty = true;
                        }
                    }
                });
                ui.end_row();

                ui.label("托盘图标");
                ui.horizontal(|ui| {
                    let current = self
                        .config
                        .tray_icon
                        .as_deref()
                        .and_then(|p| Path::new(p).file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "默认".to_string());
                    ui.weak(current);
                    if ui.button("选择…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("图片", &["png", "jpg", "jpeg", "ico", "bmp"])
                            .pick_file()
                        {
                            self.config.tray_icon = Some(path.to_string_lossy().into_owned());
                            self.config_dirty = true;
                        }
                    }
                    if self.config.tray_icon.is_some() && ui.button("恢复").clicked() {
                        self.config.tray_icon = None;
                        self.config_dirty = true;
                    }
                });
                ui.end_row();
            });

        if let Some(err) = &self.hotkey_error {
            ui.add_space(4.0);
            ui.colored_label(error_color(ui), err);
        }
    }
}

/// 关于页的图标：256px 压缩版随二进制内嵌，首次打开时解码一次。
fn load_about_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const PNG: &[u8] = include_bytes!("../assets/about.png");
    let image = image::load_from_memory(PNG).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture("about-icon", pixels, egui::TextureOptions::LINEAR))
}

impl eframe::App for App {
    /// eframe 默认的 `clear_color` 是写死的深灰 `(12,12,12,180)`，不跟随主题。
    /// 不改的话切到明亮主题只有文字变深、底色仍是深灰，等于没效果。
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // 毛玻璃是自己画的一张模糊截图，窗口本身不透明——
        // 不依赖 DWM / 驱动，任何机器上都是一样的观感。
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.thumb_loads = 0;
        self.drain_events();
        self.poll_search();
        let ctx = ui.ctx().clone();
        self.poll_ui(&ctx);

        if self.appearance_dirty {
            self.appearance_dirty = false;
            self.apply_appearance(&ctx);
        }

        // 托盘右键是 Win32 原生菜单，只认进程级的深色标记，不会跟着 egui 的主题走。
        // 主题可能在运行中变（设置里切，或者"跟随系统"时系统自己切），这里每帧
        // 比一下，变了才去调 uxtheme——那两个是未公开接口，能少调就少调。
        let menu_dark = ui.style().visuals.dark_mode;
        if self.menu_dark != Some(menu_dark) {
            self.menu_dark = Some(menu_dark);
            // 托盘菜单在守护进程里，只能让它去调 uxtheme。
            self.handle.set_menu_dark(menu_dark);
        }

        if !self.placed_once {
            self.placed_once = true;
            self.place_window(&ctx);
        }

        self.capture_hotkey(&ctx);

        // 启动首帧：窗口一直是隐藏的，先把位置摆好再露出来，避免闪一下默认位置。
        if self.startup {
            self.startup = false;
            self.show(&ctx);
        }

        if !self.visible {
            // 已经藏起来：本帧只负责轮询通道，不排版也不绘制（隐藏窗口的
            // 帧照样会跑完整 UI，500 行列表排一遍纯属浪费）。
            ctx.request_repaint_after(HIDDEN_POLL);
            return;
        }

        // 关键顺序：`Visible(true)` / `Focus` 是帧末才生效的，唤出后的一两帧里
        // `focused` 仍是 false。必须先确认"曾经拿到过焦点"，否则每次唤出都会闪一下。
        if ctx.input(|i| i.focused) {
            self.seen_focus = true;
        }
        if self.hide_on_blur
            && !self.window_pinned
            && self.seen_focus
            && self.started.elapsed() > BLUR_GRACE
            && !ctx.input(|i| i.focused)
        {
            self.hide(&ctx);
            return;
        }

        // 摆位校准：混合 DPI 下第一次发的坐标可能差一个缩放比，这里补正。
        self.settle_position(&ctx);

        // 整窗拖动：先铺一个覆盖全窗口的 drag 区域。之后注册的输入框、
        // 按钮、列表行在命中测试里都排在它上面，所以「点控件照常操作」
        // 和「拖空白处移动窗口」互不打架。
        let drag = ui.interact(
            ui.max_rect(),
            ui.scope_id().with("window-drag"),
            egui::Sense::drag(),
        );
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // 左、上、下留 14/12；右边故意留 0——滚动条要贴着窗口右边框画，
        // 内容再各自退 EDGE_PAD，这样条和文字之间才有间隙。
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: EDGE_PAD as i8,
                right: 0,
                top: EDGE_PAD_Y,
                bottom: EDGE_PAD_Y,
            })
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                if self.show_settings {
                    self.settings_panel(ui);
                } else {
                    self.list_view(ui, &ctx);
                }
            });

        if self.config_dirty {
            self.config_dirty = false;
            let (cmd, _reply) = StoreCommand::update_config(self.config.clone());
            self.handle.submit(cmd);
        }
    }
}

impl App {
    fn list_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (down, up, enter, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });

        if esc {
            self.hide(ctx);
            return;
        }

        let rows: Vec<Row> = self
            .visible()
            .iter()
            .map(|e| {
                // 保留原文的换行：多行内容要按原样折行显示，只靠 max_rows 截断。
                let preview: String = e
                    .entry
                    .preview
                    .chars()
                    .take(PREVIEW_LAYOUT_CHARS)
                    .collect();
                Row {
                    id: e.entry.id,
                    preview,
                    pinned: e.entry.pinned,
                    kind: e.entry.kind,
                    blob_path: e.blob_path.clone(),
                }
            })
            .collect();

        if rows.is_empty() {
            self.selected = 0;
        } else {
            let before = self.selected;
            if down {
                self.selected = (self.selected + 1).min(rows.len() - 1);
            }
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if self.selected >= rows.len() {
                self.selected = rows.len() - 1;
            }
            if self.selected != before {
                self.scroll_to = Some(self.selected);
            }
            if enter {
                let id = rows[self.selected].id;
                let as_plain = self.config.replay_as_plain || ctx.input(|i| i.modifiers.shift);
                self.replay(id, ctx, as_plain);
                return;
            }
        }

        let mut replay: Option<EntryId> = None;
        let mut toggle_pin: Option<(EntryId, bool)> = None;

        ui.vertical(|ui| {
            inset_right(ui, |ui| ui.horizontal(|ui| {
                // 右侧常驻两个 28/48 宽的按钮，再各留 8px 间距。
                let width = (ui.available_width() - 92.0).max(80.0);
                // 默认文字贴顶：显式给 LEFT_CENTER，再加一点内边距，
                // 否则 28 px 高的输入框里光标和 placeholder 都偏上。
                let response = ui.add_sized(
                    [width, 28.0],
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("搜索剪贴板历史")
                        .align(egui::Align2::LEFT_CENTER)
                        .margin(egui::Margin::symmetric(8, 5)),
                );
                if !self.focus_requested {
                    response.request_focus();
                    self.focus_requested = true;
                }
                // 钉住窗口：开着的时候切到别的应用也不隐藏，方便对照着往回粘。
                if pin_toggle_button(ui, self.window_pinned).clicked() {
                    self.window_pinned = !self.window_pinned;
                }
                // 和搜索框同高（28），否则默认按钮高度会让整行看起来高低不齐。
                if ui
                    .add_sized([48.0, 28.0], egui::Button::new("设置"))
                    .clicked()
                {
                    self.show_settings = true;
                }
            }));

            ui.add_space(4.0);
            inset_right(ui, |ui| ui.horizontal_wrapped(|ui| {
                for filter in [
                    KindFilter::All,
                    KindFilter::Text,
                    KindFilter::Rich,
                    KindFilter::Image,
                    KindFilter::File,
                ] {
                    if ui
                        .selectable_label(self.kind_filter == filter, filter.label())
                        .clicked()
                    {
                        self.kind_filter = filter;
                        self.selected = 0;
                    }
                }

                let sources = self.known_sources();
                if !sources.is_empty() {
                    let current = self
                        .source_filter
                        .clone()
                        .unwrap_or_else(|| "全部来源".to_string());
                    egui::ComboBox::from_id_salt("source-filter")
                        .selected_text(current)
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(self.source_filter.is_none(), "全部来源")
                                .clicked()
                            {
                                self.source_filter = None;
                                self.selected = 0;
                            }
                            for source in sources {
                                let selected = self.source_filter.as_deref() == Some(source.as_str());
                                if ui.selectable_label(selected, &source).clicked() {
                                    self.source_filter = Some(source.clone());
                                    self.selected = 0;
                                }
                            }
                        });
                }
            }));

            if let Some(err) = &self.hotkey_error {
                ui.add_space(4.0);
                inset_right(ui, |ui| {
                    ui.colored_label(error_color(ui), err);
                });
            }

            ui.add_space(4.0);
            inset_right(ui, |ui| ui.separator());

            if rows.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.weak(if self.entries.is_empty() {
                        "还没有历史。复制点什么试试。"
                    } else {
                        "没有匹配的条目"
                    });
                });
                return;
            }

            // 底部那行统计要留位置，否则会被自动撑满的 ScrollArea 挤没。
            let show_footer = self.total_visible > rows.len();
            let reserved = if show_footer { 22.0 } else { 0.0 };
            let area_h = (ui.available_height() - reserved).max(60.0);

            // 行高随内容变化，所以不能用 show_rows（它要求等高）。
            // 改为全量渲染 + 选中行 scroll_to_me。内存索引上限 500 条，
            // 这个量级的布局开销可以接受。
            let scroll_target = self.scroll_to.take();

            // 滚动区不再收窄：悬浮滚动条画在它的右边缘，也就是窗口右边框上。
            // 内容自己退 EDGE_PAD，条与文字之间才有间隙。
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(area_h)
                .show(ui, |ui| {
                    ui.set_max_width((ui.available_width() - EDGE_PAD).max(40.0));
                    // 行与行之间留空隙：紧挨着时整列文字糊成一片，分不清一条到哪结束。
                    ui.spacing_mut().item_spacing.y = ROW_GAP;
                    for (row, item) in rows.iter().enumerate() {
                        let (id, preview, pinned) = (item.id, &item.preview, item.pinned);
                        let selected = row == self.selected;
                        // 行尾星标的矩形，用来把行的点击区让出来。
                        let mut pin_rect = egui::Rect::NOTHING;
                        // 图片行左边放一张缩略图。读不出来（blob 被清掉）就留空，
                        // 只显示文字预览，不让一行坏数据把整个列表卡住。
                        let thumb = if item.kind == Kind::Image {
                            self.thumbnail(ui.ctx(), item.id, item.blob_path.as_deref())
                        } else {
                            None
                        };

                        // 先占一个绘制槽位，等布局完成拿到矩形后再把底色填进去，
                        // 这样底色画在内容下面，且能用真实的悬停状态。
                        let bg_slot = ui.painter().add(egui::Shape::Noop);
                        let response = egui::Frame::NONE
                            .corner_radius(egui::CornerRadius::same(4))
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                // 多行内容时星标要对齐顶部，不是垂直居中。
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    if let Some(texture) = &thumb {
                                        paint_thumbnail(ui, texture);
                                    }

                                    // 预览只吃「剩余宽度 - 按钮宽」：先把列宽定死，
                                    // 星标才不会被长文本顶出可视区。
                                    let gap = ui.spacing().item_spacing.x;
                                    let preview_w =
                                        (ui.available_width() - ROW_ACTION_W - gap).max(24.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(preview_w, 0.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_min_width(preview_w);
                                            ui.set_max_width(preview_w);
                                            let mut job = egui::text::LayoutJob::default();
                                            // 横向布局里 wrap_mode 是 Extend，Label 会把
                                            // max_width 改成 INFINITY——长串 URL 会一路顶穿
                                            // 右边界，把外边距挤掉。必须显式要求 Wrap。
                                            job.wrap.max_width = preview_w;
                                            job.wrap.max_rows = PREVIEW_MAX_ROWS;
                                            job.wrap.break_anywhere = true;
                                            job.append(
                                                preview,
                                                0.0,
                                                egui::TextFormat {
                                                    font_id: egui::FontId::proportional(13.0),
                                                    color: if selected {
                                                        ui.visuals().strong_text_color()
                                                    } else {
                                                        ui.visuals().text_color()
                                                    },
                                                    ..Default::default()
                                                },
                                            );
                                            ui.add(
                                                egui::Label::new(job)
                                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                            );
                                        },
                                    );

                                    let pin = entry_pin_button(ui, pinned, selected);
                                    pin_rect = pin.rect;
                                    if pin.clicked() {
                                        toggle_pin = Some((id, !pinned));
                                    }
                                });
                            })
                        .response;

                        let rect = response.rect;
                        let hovered = ui
                            .ctx()
                            .pointer_hover_pos()
                            .is_some_and(|p| rect.contains(p));
                        let fill = if selected {
                            ui.visuals().selection.bg_fill
                        } else if hovered {
                            ui.visuals().widgets.hovered.weak_bg_fill
                        } else {
                            row_fill(ui.visuals().dark_mode)
                        };
                        ui.painter()
                            .set(bg_slot, egui::Shape::rect_filled(rect, 4.0, fill));

                        // 点击区缩到星标左边：按钮自己吃掉落在它身上的点击，
                        // 否则点「钉住这条」会顺带回放内容。
                        let click_rect = egui::Rect::from_min_max(
                            rect.min,
                            egui::pos2((pin_rect.left() - 2.0).max(rect.left()), rect.max.y),
                        );
                        let response = ui.interact(click_rect, response.id, egui::Sense::click());
                        if response.clicked() {
                            self.selected = row;
                            replay = Some(id);
                        }
                        // 右键切换钉住，作为快捷方式保留。
                        if response.secondary_clicked() {
                            toggle_pin = Some((id, !pinned));
                        }
                        if scroll_target == Some(row) {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                    }
                });

            if show_footer {
                ui.add_space(2.0);
                inset_right(ui, |ui| {
                    ui.weak(format!(
                        "共 {} 条，当前显示 {} 条（继续输入可全量搜索）",
                        self.total_visible,
                        rows.len()
                    ));
                });
            }
        });

        if let Some((id, pinned)) = toggle_pin {
            // 存储层只写库、不回推事件，本地必须先改一份：否则右键钉住之后
            // 星标不出现、顺序也不动，看起来就像"这个功能根本没生效"。
            if let Some(view) = self.entries.iter_mut().find(|e| e.entry.id == id) {
                view.entry.pinned = pinned;
            }
            // 与 page() 保持同一套顺序：固定的在前，各自按最近使用倒序。
            self.entries.sort_by(|a, b| {
                b.entry
                    .pinned
                    .cmp(&a.entry.pinned)
                    .then(b.seq.cmp(&a.seq))
            });
            // 重排后光标跟着同一条走，不然会突然指向别的内容。
            if let Some(idx) = self.visible().iter().position(|e| e.entry.id == id) {
                self.selected = idx;
                self.scroll_to = Some(idx);
            }
            let (cmd, _reply) = StoreCommand::set_pinned(id, pinned);
            self.handle.submit(cmd);
        }
        if let Some(id) = replay {
            let as_plain = self.config.replay_as_plain || ctx.input(|i| i.modifiers.shift);
            self.replay(id, ctx, as_plain);
        }
    }
}

/// 纯文本模式：富文本降级成 plain，其他类型原样返回。
fn to_plain(raw: erwuzai_platform::RawClipboard) -> erwuzai_platform::RawClipboard {
    use erwuzai_platform::RawClipboard;
    match raw {
        RawClipboard::Rich { plain, .. } => RawClipboard::Text(plain),
        other => other,
    }
}

fn content_to_raw(content: Content) -> Option<erwuzai_platform::RawClipboard> {
    use erwuzai_platform::RawClipboard;
    Some(match content {
        Content::Text(text) => RawClipboard::Text(text),
        Content::RichText { plain, html } => RawClipboard::Rich { plain, html },
        Content::FileList(paths) => RawClipboard::Files(paths),
        Content::Image { path, width, height } => RawClipboard::Image {
            png: std::fs::read(path).ok()?,
            width,
            height,
        },
    })
}

/// 类型名，中文，不带任何补齐空格——现在它是标签块内的文字。
/// 纯计算：把窗口摆到工作区里的合适位置，并夹在工作区内。
///
/// `cursor` 与 `work` 都是逻辑点；`work` 是 (x, y, w, h)。
/// 靠近右下角时光标定位会把窗口推出屏幕，所以必须夹。
fn place_in_work_area(
    cursor: (f32, f32),
    work: (f32, f32, f32, f32),
    size: egui::Vec2,
    mode: PopupPosition,
) -> egui::Pos2 {
    let (wx, wy, ww, wh) = work;
    let (left, top) = (wx, wy);
    let (right, bottom) = (wx + ww, wy + wh);

    let (x, y) = match mode {
        PopupPosition::Cursor => (cursor.0 + 8.0, cursor.1 + 12.0),
        _ => (
            left + (right - left - size.x) / 2.0,
            top + (bottom - top - size.y) / 2.0,
        ),
    };

    egui::pos2(
        x.clamp(left, (right - size.x).max(left)),
        y.clamp(top, (bottom - size.y).max(top)),
    )
}

/// 修饰键本身不该被当成"用户选的键"。
fn is_modifier_key(key: egui::Key) -> bool {
    use egui::Key;
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// egui 的 Key 转成我们解析器认识的写法：`A` / `0` / `Space` / `F1`。
fn key_name(key: egui::Key) -> String {
    let name = format!("{key:?}");
    match name.strip_prefix("Num") {
        Some(digit) if digit.len() == 1 => digit.to_string(),
        _ => name,
    }
}

/// 错误提示色：明亮主题下要压暗，否则橙红在浅底上发飘。
fn error_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(230, 130, 110)
    } else {
        egui::Color32::from_rgb(175, 60, 45)
    }
}

/// 顶栏的「钉住窗口」开关：钉住后切到别的窗口也不自动关闭。
///
/// 图钉是自己画的——egui 没有图标字体，中文界面里写「钉」字又占宽。
/// 钉住时按钮底色用主题色、针变白，和旁边「设置」的默认按钮一眼能分开。
fn pin_toggle_button(ui: &mut egui::Ui, pinned: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let hovered = response.hovered();
    let visuals = ui.visuals();
    let fill = if pinned {
        visuals.selection.bg_fill
    } else if hovered {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 4.0, fill);
    }

    let color = if pinned {
        visuals.selection.stroke.color
    } else if hovered {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    paint_pin(ui.painter(), rect.center(), color, 1.25);
    response.on_hover_text(if pinned {
        "取消钉住，恢复失焦自动隐藏"
    } else {
        "钉住窗口：失焦、点条目、Esc 都不会关闭"
    })
}

/// 行的静止底色：比面板略亮一档，整行看着像一张卡片。
///
/// 直接用 `inactive.weak_bg_fill`（深色是 50）在几百行里太跳，
/// 压到面板与悬停色之间。深色 38 / 浅色 243，和 6px 的行间隙配合，
/// 一眼就能分出行的边界。
fn row_fill(dark: bool) -> egui::Color32 {
    egui::Color32::from_gray(if dark { 38 } else { 238 })
}

/// 图片行左侧的缩略图：固定方框，等比放大后**居中裁切**填满。
///
/// 不按比例缩进框里（contain）的话，长截图会变成一条几像素高的细线，
/// 看不出是什么；裁切至少能看清画面主体。
fn paint_thumbnail(ui: &mut egui::Ui, texture: &egui::TextureHandle) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(THUMB_SIZE, THUMB_SIZE), egui::Sense::hover());
    let tex = texture.size_vec2();
    let scale = (rect.width() / tex.x).max(rect.height() / tex.y);
    let fitted = tex * scale;
    // 只取中间那一块，UV 宽度就是"框 / 放大后的图"。
    let uv = egui::Rect::from_center_size(
        egui::pos2(0.5, 0.5),
        egui::vec2(
            (rect.width() / fitted.x).min(1.0),
            (rect.height() / fitted.y).min(1.0),
        ),
    );
    ui.painter().image(texture.id(), rect, uv, egui::Color32::WHITE);
}

/// 用五角星而不是图钉：顶栏那个图钉是**窗口**钉住，这里钉的是**条目**
/// （不被清理、置顶）。钉住时常亮实心，未钉住只在悬停 / 选中行里淡显，
/// 免得整列都是星号干扰扫读。
fn entry_pin_button(ui: &mut egui::Ui, pinned: bool, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ROW_ACTION_W, 18.0), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let hovered = response.hovered();
    let color = if pinned {
        ui.visuals().strong_text_color()
    } else if hovered {
        ui.visuals().weak_text_color()
    } else if selected {
        ui.visuals().strong_text_color().gamma_multiply(0.5)
    } else {
        egui::Color32::TRANSPARENT
    };

    if hovered {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if color != egui::Color32::TRANSPARENT {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if pinned { "★" } else { "☆" },
            egui::FontId::proportional(13.0),
            color,
        );
    }

    response.on_hover_text(if pinned {
        "取消钉住"
    } else {
        "钉住这条：不会被清理，并排在最前"
    })
}

/// 图钉：一个 45° 的方形钉帽 + 一根斜向的针。
fn paint_pin(painter: &egui::Painter, center: egui::Pos2, color: egui::Color32, scale: f32) {
    let at = |x: f32, y: f32| egui::pos2(center.x + x * scale, center.y + y * scale);
    painter.add(egui::Shape::convex_polygon(
        vec![
            at(-0.6, -5.0),
            at(3.4, -1.0),
            at(-0.6, 3.0),
            at(-4.6, -1.0),
        ],
        color,
        egui::Stroke::NONE,
    ));
    painter.line_segment(
        [at(-0.6, 2.4), at(3.2, 6.0)],
        egui::Stroke::new(1.3, color),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use erwuzai_platform::RawClipboard;

    #[test]
    fn rich_downgrades_to_plain() {
        let raw = RawClipboard::Rich {
            plain: "hello".into(),
            html: Some("<b>hello</b>".into()),
        };
        match to_plain(raw) {
            RawClipboard::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn rich_content_can_be_downgraded_end_to_end() {
        let content = Content::RichText {
            plain: "带格式的句子".into(),
            html: Some("<b>带格式的句子</b>".into()),
        };
        let raw = content_to_raw(content).expect("rich content converts");
        assert!(matches!(raw, RawClipboard::Rich { .. }));

        match to_plain(raw) {
            RawClipboard::Text(t) => assert_eq!(t, "带格式的句子"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn cursor_placement_clamps_inside_work_area() {
        let work = (0.0, 0.0, 1920.0, 1080.0);
        let size = egui::vec2(480.0, 420.0);

        // 屏幕正中间：光标右下偏一点
        let p = place_in_work_area((900.0, 500.0), work, size, PopupPosition::Cursor);
        assert_eq!(p, egui::pos2(908.0, 512.0));

        // 右下角：不能把窗口推出屏幕
        let p = place_in_work_area((1910.0, 1070.0), work, size, PopupPosition::Cursor);
        assert_eq!(p, egui::pos2(1440.0, 660.0));

        // 居中模式：忽略光标
        let p = place_in_work_area((10.0, 10.0), work, size, PopupPosition::Center);
        assert_eq!(p, egui::pos2(720.0, 330.0));

        // 副屏（工作区原点不为 0）：夹在副屏范围内，不能跑回主屏
        let work2 = (1920.0, 0.0, 2560.0, 1400.0);
        let p = place_in_work_area((1900.0, 700.0), work2, size, PopupPosition::Cursor);
        assert_eq!(p.x, 1920.0, "左边界不应越过副屏原点");
    }

    #[test]
    fn key_names_match_parser_expectations() {
        assert_eq!(key_name(egui::Key::V), "V");
        assert_eq!(key_name(egui::Key::Num5), "5");
        assert_eq!(key_name(egui::Key::Space), "Space");
        assert_eq!(key_name(egui::Key::F5), "F5");
        assert_eq!(key_name(egui::Key::ArrowUp), "ArrowUp");
    }

    #[test]
    fn modifier_keys_are_skipped() {
        assert!(is_modifier_key(egui::Key::ControlLeft));
        assert!(is_modifier_key(egui::Key::ShiftRight));
        assert!(is_modifier_key(egui::Key::SuperLeft));
        assert!(!is_modifier_key(egui::Key::A));
        assert!(!is_modifier_key(egui::Key::ArrowLeft));
    }

    #[test]
    fn captured_combo_formats_as_display_string() {
        let spec = HotkeySpec {
            modifiers: erwuzai_platform::Modifiers::CTRL | erwuzai_platform::Modifiers::SHIFT,
            key: key_name(egui::Key::V),
        };
        assert_eq!(spec.to_string(), "Ctrl+Shift+V");
        // 生成的字符串必须能被解析器读回来（回填配置时会用到）
        assert!(HotkeySpec::parse(&spec.to_string()).is_ok());
    }

    #[test]
    fn other_kinds_pass_through() {
        let text = to_plain(RawClipboard::Text("x".into()));
        assert!(matches!(text, RawClipboard::Text(_)));

        let files = to_plain(RawClipboard::Files(vec!["a.txt".into()]));
        assert!(matches!(files, RawClipboard::Files(_)));

        let image = to_plain(RawClipboard::Image {
            png: vec![1, 2, 3],
            width: 1,
            height: 1,
        });
        assert!(matches!(image, RawClipboard::Image { .. }));
    }
/// 长串（URL / 令牌）以前会被 Label 的 Extend 模式排成一行 800px 宽，
    /// 把 14px 的右外边距整个顶掉。锁定：预览必须折行，行宽不能越过内边距。
    #[test]
    fn long_preview_stays_inside_padding() {
        let ctx = egui::Context::default();
        let mut content_right = 0.0;
        let mut row_rights = Vec::new();
        let mut pin_rects = Vec::new();

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(480.0, 420.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    content_right = ui.max_rect().right();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let response = egui::Frame::NONE
                                .inner_margin(egui::Margin::symmetric(6, 3))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;

                                        let gap = ui.spacing().item_spacing.x;
                                        let preview_w =
                                            (ui.available_width() - ROW_ACTION_W - gap).max(24.0);
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(preview_w, 0.0),
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                ui.set_min_width(preview_w);
                                                ui.set_max_width(preview_w);
                                                let mut job = egui::text::LayoutJob::default();
                                                job.wrap.max_width = preview_w;
                                                job.wrap.max_rows = PREVIEW_MAX_ROWS;
                                                job.wrap.break_anywhere = true;
                                                job.append(
                                                    &"https://example.com/".repeat(12),
                                                    0.0,
                                                    egui::TextFormat::default(),
                                                );
                                                ui.add(
                                                    egui::Label::new(job)
                                                        .wrap_mode(egui::TextWrapMode::Wrap),
                                                );
                                            },
                                        );
                                        pin_rects.push(entry_pin_button(ui, false, false).rect);
                                    });
                                })
                                .response;
                            row_rights.push(response.rect.right());
                        });
                });
        });
        output.textures_delta.clear();

        assert!(!row_rights.is_empty());
        for right in &row_rights {
            assert!(
                *right <= content_right + 0.5,
                "行右边界 {right} 越过了内边距 {content_right}"
            );
            // 行的底框要铺满整行，否则选中色块只盖住前几个字。
            assert!(
                *right >= content_right - 1.0,
                "行底框只有 {right}，没铺到 {content_right}"
            );
        }
        // 长文本不能把星标顶出右边界：它是行的最后一个元素。
        for (row_right, pin) in row_rights.iter().zip(&pin_rects) {
            assert!(
                pin.right() <= row_right + 0.5,
                "星标 {pin:?} 越出了行右边界 {row_right}"
            );
            assert_eq!(pin.width(), ROW_ACTION_W);
        }
    }
}
