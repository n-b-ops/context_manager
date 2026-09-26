use std::process::ExitCode;

use eframe::NativeOptions;
use log::info;

use ctxpack::app::ContextBuilderApp;
use ctxpack::cli;

fn main() -> ExitCode {
    // Initialize logging
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Frontend dispatch: a bare invocation (or an explicit --gui) starts the
    // desktop interface as before; any other argument runs the headless CLI,
    // which works over SSH and other non-graphical environments.
    let want_gui = args.is_empty() || args.iter().any(|a| a == "--gui");
    if want_gui {
        if let Err(e) = run_gui() {
            eprintln!("GUI error: {}", e);
            return ExitCode::FAILURE;
        }
    } else if let Err(e) = cli::run(args) {
        eprintln!("error: {}", e);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run_gui() -> Result<(), eframe::Error> {
    info!("Starting CtxPack (GUI)");

    let mut options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_title("CtxPack"),
        ..Default::default()
    };

    // Allow overriding the renderer through an environment variable.
    //   EF_RENDERER=wgpu  -> use the default wgpu backend (preferred when a GPU or WARP is available)
    //   EF_RENDERER=glow  -> force OpenGL backend (works on systems with at least OpenGL 2.0)
    //   EF_RENDERER=auto  -> let eframe pick (default behaviour)
    match std::env::var("EF_RENDERER").as_deref() {
        Ok("glow") => {
            options.renderer = eframe::Renderer::Glow;
            info!("Renderer forced to Glow via EF_RENDERER env var");
        }
        Ok("wgpu") => {
            options.renderer = eframe::Renderer::Wgpu;
            info!("Renderer forced to WGPU via EF_RENDERER env var");
        }
        _ => {
            // Leave at default (Auto). eframe will decide (prefers WGPU)
            info!("Renderer auto-selected (override via EF_RENDERER)");
        }
    }

    eframe::run_native(
        "CtxPack",
        options,
        Box::new(|cc| Box::new(ContextBuilderApp::new(cc))),
    )
}
