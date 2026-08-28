use eframe::egui::{
    self, FontData, FontFamily,
    epaint::text::{FontInsert, FontPriority, InsertFontFamily},
};

const NOTO_SANS_CJK_SC: &[u8] = include_bytes!("../assets/fonts/NotoSansCJKsc-Regular.otf");

/// Adds pan-CJK glyph coverage while retaining egui's built-in Latin, symbol,
/// emoji, and monospace fonts as the preferred faces.
pub fn install(context: &egui::Context) {
    context.add_font(FontInsert::new(
        "Noto Sans CJK SC",
        FontData::from_static(NOTO_SANS_CJK_SC),
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ],
    ));
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Context, FontId, RawInput};

    use super::{NOTO_SANS_CJK_SC, install};

    #[test]
    fn bundled_font_is_an_opentype_font() {
        assert!(
            NOTO_SANS_CJK_SC.starts_with(b"OTTO"),
            "the bundled Unicode font must be a valid OpenType asset"
        );
        assert!(
            NOTO_SANS_CJK_SC.len() > 1_000_000,
            "the bundled Unicode font appears truncated"
        );
    }

    #[test]
    fn gui_font_chain_contains_common_unicode_glyphs() {
        let context = Context::default();
        install(&context);
        let mut output = context.run_ui(RawInput::default(), |_| {});
        output.textures_delta.clear();

        assert!(context.fonts_mut(|fonts| fonts.has_glyphs(
            &FontId::proportional(14.0),
            "中文文件名 · 繁體中文 · 日本語 · 한국어 · Русский · Ελληνικά · ✓"
        )));
        assert!(context.fonts_mut(|fonts| {
            fonts.has_glyphs(&FontId::monospace(14.0), "影片家庭视频第集")
        }));
    }
}
