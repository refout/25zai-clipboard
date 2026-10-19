//! 中文字形。
//!
//! egui 自带的 `default_fonts`（Ubuntu-Light / NotoEmoji / Hack）不含 CJK，
//! 不挂系统字体的话中文全是方框。这里按平台试几个候选路径，取第一个能读到的，
//! 作为 fallback 追加到 Proportional 和 Monospace 的**末尾**——
//! ASCII 仍然用 egui 默认字体（更好看），只有缺字形时才落到中文字体。

use std::sync::Arc;

use eframe::egui;

#[cfg(windows)]
const CANDIDATES: &[&str] = &[
    "C:/Windows/Fonts/simhei.ttf",
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simsun.ttc",
];

#[cfg(target_os = "macos")]
const CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
];

#[cfg(not(any(windows, target_os = "macos")))]
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
];

/// 挂载中文字体。任何候选都读不到时静默返回——界面还能用，只是中文显示为方框。
pub fn install_cjk(ctx: &egui::Context) {
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        println!("cjk font: {path}");
        return;
    }
    eprintln!("25zai: 没找到可用的中文字体，中文会显示为方框");
}
