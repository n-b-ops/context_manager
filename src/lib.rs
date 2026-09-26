//! CtxPack core: directory scanning, context-document generation, file
//! watching, and the headless CLI frontend. The binary (`src/main.rs`) only
//! wires argv dispatch and the GUI entry point; integration tests in
//! `tests/` drive `cli::run` through this library.

pub mod app;
pub mod cli;
pub mod constants;
pub mod document_generator;
pub mod error;
pub mod events;
pub mod file_handler;
pub mod file_monitor;
pub mod ui_tree_handler;
