#[cfg(feature = "gui")]
use std::sync::Arc;

#[cfg(feature = "gui")]
pub mod app;
pub mod command;
#[cfg(feature = "gui")]
pub mod loader_versions;
pub mod mod_list;
pub mod overrides;
pub mod settings;
#[cfg(feature = "gui")]
pub mod sync_tree;

#[cfg(feature = "gui")]
pub fn run_gui() -> Result<(), GuiError> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("mcmpu")
            .with_inner_size([960.0, 660.0])
            .with_min_inner_size([820.0, 540.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mcmpu",
        options,
        Box::new(|cc| {
            load_cjk_font(&cc.egui_ctx);
            app::install_accent_styles(&cc.egui_ctx);
            Ok(Box::new(app::McmpuGuiApp::default()))
        }),
    )
    .map_err(|source| GuiError::Native(source.to_string()))
}

#[cfg(not(feature = "gui"))]
pub fn run_gui() -> Result<(), GuiError> {
    Err(GuiError::Disabled)
}

#[cfg(feature = "gui")]
fn load_cjk_font(ctx: &eframe::egui::Context) {
    const FONT_CANDIDATES: &[&str] = &["C:/Windows/Fonts/msyh.ttc", "C:/Windows/Fonts/simhei.ttf"];

    let mut fonts = eframe::egui::FontDefinitions::default();
    let mut loaded = false;

    for path in FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                Arc::new(eframe::egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(eframe::egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".to_owned());
            fonts
                .families
                .entry(eframe::egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            loaded = true;
            break;
        }
    }

    if loaded {
        ctx.set_fonts(fonts);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GuiError {
    #[cfg(feature = "gui")]
    #[error("failed to start GUI: {0}")]
    Native(String),
    #[cfg(not(feature = "gui"))]
    #[error("GUI support is disabled in this build")]
    Disabled,
}
