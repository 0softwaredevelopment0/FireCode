#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FireCode")
            .with_inner_size([1000.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FireCode",
        options,
        Box::new(|_cc| Ok(Box::new(FireCodeApp::default()))),
    )
}

#[derive(Default)]
struct FireCodeApp {}

impl eframe::App for FireCodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("FireCode");
            ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
            ui.separator();
            ui.label("Skeleton build. Browserless AI chat client: phases pending.");
        });
    }
}
