//! dconvert — desktop GUI front end for the docconvert engine.
//!
//! The real engine (`engine::spawn_real_engine`) is a pool of worker
//! processes around the facade in `dconv-engine`; see `app/src/engine.rs` for
//! the protocol. To review UI behaviour without a PDFium dependency, swap the
//! one engine call below for `engine::demo::spawn_demo_engine()`.

// A windows-subsystem binary opens no console window of its own. Release
// builds only: a debug build keeps the console subsystem so tracing and
// panics stay visible under `cargo run`. The worker children spoken to over
// pipes (engine::WORKER_FLAG) do not need one either — they print nothing to
// a console by design.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod log_ring;
mod state;
mod theme;
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

    // Files named on the command line queue on launch: "Open with
    // dconvert" and dropping a selection onto the executable both arrive
    // this way. Unreadable ones are filtered by `App::add_files`, which
    // logs why rather than refusing to start. The worker flag never reaches
    // here (it returns above).
    let initial_files: Vec<std::path::PathBuf> =
        std::env::args_os().skip(1).map(std::path::PathBuf::from).collect();

    eframe::run_native(
        // The pre-frame title; the app rewrites it from its content every
        // frame after (app.rs › update_window_title). The tagline stayed
        // here only until the first frame in earlier builds.
        "dconvert",
        options,
        Box::new(move |cc| {
            // The log ring is created before the engine so tracing output from
            // either side lands in the same ring from the very first frame.
            let log = log_ring::install_tracing();
            let engine = engine::spawn_real_engine();
            let mut app = app::App::new(cc, log, engine);
            app.add_files(initial_files);
            Ok(Box::new(app))
        }),
    )
}
