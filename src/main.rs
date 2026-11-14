use std::path::PathBuf;

fn main() -> eframe::Result {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([700.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Disk Vivarium",
        options,
        Box::new(move |_creation_context| Ok(Box::new(disk_vivarium::gui::Vivarium::new(root)))),
    )
}
