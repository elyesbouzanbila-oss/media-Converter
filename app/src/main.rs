//! dconvert — desktop GUI front end for the docconvert engine.
//!
//! The real engine (`engine::spawn_real_engine`) is a pool of worker
//! processes around the facade in `dconv-engine`; see `app/src/engine.rs` for
//! the protocol. To review UI behaviour without a PDFium dependency, swap the
//! one engine call below for `engine::demo::spawn_demo_engine()`.

mod app;
mod engine;
mod log_ring;
mod state;
mod ui;

fn main() -> eframe::Result {
    // Worker process: this executable is re-invoked as a conversion worker by
    // the engine's process pool. Handle it before the first line of GUI code
    // so a child does no windowing work at all, and exit the process with the
    // worker's status (never returns).
    if std::env::args().any(|arg| arg == engine::WORKER_FLAG) {
        std::process::exit(engine::worker_main());
    }

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
            let engine = engine::spawn_real_engine();
            Ok(Box::new(app::App::new(cc, log, engine)))
        }),
    )
}
