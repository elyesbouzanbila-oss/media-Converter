//! dconvert — desktop GUI front end for the docconvert engine.
//!
//! M0 builds the full shell against a demo engine (`engine::spawn_demo_engine`)
//! so the interface can be finished and reviewed before the real facade in
//! `dconv-engine` lands. Swapping the real engine in is a one-line change at
//! the call below, since both speak the same command/event protocol.

mod app;
mod engine;
mod log_ring;
mod state;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "dconvert — document converter",
        options,
        Box::new(|cc| {
            // The log ring is created before the engine so tracing output from
            // either side lands in the same ring from the very first frame.
            let log = log_ring::install_tracing();
            let engine = engine::spawn_demo_engine();
            Ok(Box::new(app::App::new(cc, log, engine)))
        }),
    )
}
