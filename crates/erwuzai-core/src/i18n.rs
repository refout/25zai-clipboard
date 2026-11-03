//! 多语言。
//!
//! 设计很简单：中文是“主键”，英文是覆盖项。
//! tr("设置") 在中文模式下原样返回，在英文模式下返回 "Settings"；
//! 表里没有的键（大多是调试断言）在英文模式下也会原样退回中文，不会崩。
//!
//! 设置语言用 set_lang，进程内全局生效——目前只在守护进程 / UI 启动时
//! 读一次配置设好，运行期不切换（切换要重建整棵 UI 树，意义不大）。

use std::sync::OnceLock;
use std::sync::RwLock;

/// 支持的语言。Auto 在启动时按系统区域决定（中文系统→中文，其它→英文）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Zh,
    En,
}

static LANG: OnceLock<RwLock<Lang>> = OnceLock::new();

fn cell() -> &'static RwLock<Lang> {
    LANG.get_or_init(|| RwLock::new(Lang::Zh))
}

/// 设语言。进程级全局；运行期切换（设置页改语言）也走这里，所以必须可重入。
pub fn set_lang(lang: Lang) {
    *cell().write().expect("i18n lock poisoned") = lang;
}

/// 读当前语言。没设过就按中文处理。
pub fn lang() -> Lang {
    *cell().read().expect("i18n lock poisoned")
}

macro_rules! translations {
    ($($zh:literal => $en:literal),* $(,)?) => {
        /// 取翻译。key 是中文原文；英文模式查表，查不到退回中文。
        /// 返回值的生命周期跟 key 走（查到的英文常量是 'static，能自由缩短）。
        pub fn tr(key: &str) -> &str {
            if lang() == Lang::En {
                match key {
                    $($zh => $en,)*
                    _ => key,
                }
            } else {
                key
            }
        }
    };
}

translations! {
    // 通用
    "设置" => "Settings",
    "返回" => "Back",
    "打开设置" => "Open settings",
    "退出" => "Quit",
    "退出 25仔" => "Quit 25zai",
    "清空历史" => "Clear history",
    "显示历史" => "Show history",
    "25仔 — 剪贴板历史" => "25zai — Clipboard history",
    "隐藏" => "Hide",
    "关闭" => "Close",
    "恢复" => "Reset",
    "选择…" => "Choose…",
    "默认" => "Default",
    "关于" => "About",
    "许可证" => "License",
    "数据" => "Data",
    "版本 {}" => "Version {}",

    // 标签页
    "外观" => "Appearance",
    "行为" => "Behavior",
    "容量" => "Capacity",
    "唤起" => "Trigger",

    // 外观
    "主题" => "Theme",
    "跟随系统" => "Follow system",
    "明亮" => "Light",
    "黑暗" => "Dark",
    "主题色" => "Accent color",
    "字号" => "Font size",
    "语言" => "Language",
    "小" => "Small",
    "标准" => "Standard",
    "大" => "Large",
    "改两次字号翻两倍" => "Changing twice doubles the scale",
    "托盘图标" => "Tray icon",

    // 行为
    "回放后排到第一" => "Move to top after replay",
    "回放只保留纯文本" => "Replay as plain text only",
    "也可以回放时按 Shift+Enter 临时生效" => "Or hold Shift+Enter while replaying to apply once",
    "点条目后直接粘贴到光标处" => "Paste at cursor on click",
    "关闭则只把内容放回剪贴板，等你自己按 Ctrl+V" => "Off: only copy back to clipboard, press Ctrl+V yourself",
    "开机自动启动（后台常驻）" => "Launch at startup (background)",
    "用 --daemon 拉起守护进程，不弹窗；改动会同步到系统的启动项" => "Starts the daemon with --daemon (no window); changes sync to the system startup entry",
    "跳过疑似密钥" => "Skip likely secrets",
    "排除应用（逗号分隔）" => "Excluded apps (comma-separated)",

    // 容量
    "条数上限" => "Max entries",
    "时间清理" => "Time cleanup",
    "启用" => "Enable",
    " 天" => " days",
    " 条" => " entries",
    "固定条目既不计入上限，也不会被清理" => "Pinned entries are exempt from both the limit and cleanup",

    // 唤起
    "快捷键" => "Hotkey",
    "按下组合键…" => "Press a key combo…",
    "点击后按下新组合" => "Click then press a new combo",
    "按 Esc 取消" => "Press Esc to cancel",
    "必须带 Ctrl / Alt / Shift" => "Must include Ctrl / Alt / Shift",
    "注册失败：{}" => "Registration failed: {}",
    "弹出位置" => "Popup position",
    "跟随光标" => "Follow cursor",
    "居中" => "Centered",

    // 关于
    "25仔" => "25zai",
    "剪贴板历史，本地存储，随时唤起。" => "Clipboard history, stored locally, summon anytime.",
    "只存在本机（redb + 图片文件），不上传" => "Lives only on this machine (redb + image files), never uploaded",
    "曾经拿到过焦点" => "Has held focus before",
    "这个功能根本没生效" => "This feature never actually worked",
    "先拿到焦点、再谈失焦" => "Focus first, then talk about losing it",

    // 主列表
    "搜索剪贴板历史" => "Search clipboard history",
    "全部" => "All",
    "全部来源" => "All sources",
    "文本" => "Text",
    "图片" => "Image",
    "文件" => "Files",
    "富文本" => "Rich text",
    "历史条目" => "History",
    "还没有历史。复制点什么试试。" => "No history yet. Copy something to begin.",
    "没有匹配的条目" => "No matching entries",
    "共 {} 条，当前显示 {} 条（继续输入可全量搜索）" => "{} total, {} shown (keep typing for a full search)",
    "{} 条" => "{} entries",
    "钉住这条：不会被清理，并排在最前" => "Pin: exempt from cleanup, always on top",
    "取消钉住" => "Unpin",
    "钉住窗口：失焦、点条目、Esc 都不会关闭" => "Pin window: blur, click, or Esc won't close it",
    "取消钉住，恢复失焦自动隐藏" => "Unpin to restore auto-hide on blur",
    "带格式的句子" => "Formatted text",
    "<b>带格式的句子</b>" => "<b>Formatted text</b>",
    "框 / 放大后的图" => "Box / enlarged image",
    "正文色 × 0.6 的透明度" => "Body text color at 0.6 opacity",

    // 写入 / 错误
    "25zai: 写剪贴板失败：{}" => "25zai: failed to write clipboard: {}",
    "25zai: 搜索失败：{}" => "25zai: search failed: {}",

    // 调试断言（保留中文即可，普通用户看不到）
    "左边界不应越过副屏原点" => "Left edge must not cross the secondary monitor origin",
    "星标 {pin:?} 越出了行右边界 {row_right}" => "Star {pin:?} overflowed row right {row_right}",
    "行右边界 {right} 越过了内边距 {content_right}" => "Row right {right} passed padding {content_right}",
    "行底框只有 {right}，没铺到 {content_right}" => "Row bottom only {right}, not reaching {content_right}",
    "最终写进去的字节" => "Final written bytes",
    "第一帧的 pixels_per_point 还是旧显示器" => "First frame pixels_per_point still on old monitor",
    "调用时的缩放系数" => "Scale factor at call time",
    "真正应用时的" => "Actually applied",
    "用户选的键" => "User-selected key",
    "非修饰键" => "Non-modifier key",
    "失活" => "Inactive",
    "失焦" => "Blurred",
    "你选了 Ctrl 这个键" => "You picked Ctrl as the key",
}

/// 带参数的翻译。format! 的首参必须是字面量，没法直接塞 tr(...)，
/// 这里按顺序把每个 {} 替换成对应的参数。占位符多于参数时剩下的原样保留。
pub fn tr_fmt(key: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = tr(key).to_string();
    for arg in args {
        match out.find("{}") {
            Some(pos) => {
                let text = arg.to_string();
                out.replace_range(pos..pos + 2, &text);
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 全局语言是进程级状态，集中在一个测试里跑，避免和其它测试并发互相踩。
    #[test]
    fn switches_between_languages() {
        set_lang(Lang::En);
        assert_eq!(tr("设置"), "Settings");
        assert_eq!(tr("没有这条"), "没有这条"); // 查不到时退回中文原文
        assert_eq!(tr_fmt("版本 {}", &[&"1.2.3"]), "Version 1.2.3");

        set_lang(Lang::Zh);
        assert_eq!(tr("设置"), "设置");
        assert_eq!(tr_fmt("版本 {}", &[&"1.2.3"]), "版本 1.2.3");
    }
}
