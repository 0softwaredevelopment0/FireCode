use std::path::PathBuf;
use std::sync::Arc;

use egui::FontFamily;

/// Installs Windows system fonts: Segoe UI (regular/bold/italic) as the
/// proportional family and Consolas (regular/bold) as monospace, keeping the
/// egui defaults as glyph fallbacks (emoji etc.).
///
/// Real bold/italic glyphs are exposed through named families ("bold",
/// "italic", "mono-bold") for markdown rendering - egui has no font weight
/// selection, a named family per style is the supported way.
pub fn install(ctx: &egui::Context) {
    let fonts_dir = PathBuf::from("C:\\Windows\\Fonts");
    let mut defs = egui::FontDefinitions::default();

    let load = |defs: &mut egui::FontDefinitions, file: &str, name: &str| -> bool {
        match std::fs::read(fonts_dir.join(file)) {
            Ok(data) => {
                defs.font_data
                    .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(data)));
                true
            }
            Err(_) => false,
        }
    };

    let segoe = load(&mut defs, "segoeui.ttf", "fc-segoe");
    let segoe_bold = load(&mut defs, "segoeuib.ttf", "fc-segoe-bold");
    let segoe_italic = load(&mut defs, "segoeuii.ttf", "fc-segoe-italic");
    let consolas = load(&mut defs, "consola.ttf", "fc-consolas");
    let consolas_bold = load(&mut defs, "consolab.ttf", "fc-consolas-bold");

    if segoe {
        defs.families
            .entry(FontFamily::Proportional)
            .and_modify(|list| list.insert(0, "fc-segoe".to_owned()));
    }
    if consolas {
        defs.families
            .entry(FontFamily::Monospace)
            .and_modify(|list| list.insert(0, "fc-consolas".to_owned()));
    }

    let proportional_fallback = |defs: &egui::FontDefinitions| -> Vec<String> {
        defs.families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default()
    };
    let monospace_fallback = |defs: &egui::FontDefinitions| -> Vec<String> {
        defs.families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default()
    };

    if segoe_bold {
        let mut list = vec!["fc-segoe-bold".to_owned()];
        list.extend(proportional_fallback(&defs));
        defs.families
            .insert(FontFamily::Name("bold".into()), list);
    }
    if segoe_italic {
        let mut list = vec!["fc-segoe-italic".to_owned()];
        list.extend(proportional_fallback(&defs));
        defs.families
            .insert(FontFamily::Name("italic".into()), list);
    }
    if consolas_bold {
        let mut list = vec!["fc-consolas-bold".to_owned()];
        list.extend(monospace_fallback(&defs));
        defs.families
            .insert(FontFamily::Name("mono-bold".into()), list);
    }

    ctx.set_fonts(defs);
}
