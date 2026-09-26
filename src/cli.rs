//! Headless CLI frontend for Context Builder.
//!
//! The GUI (`app.rs`) and this module drive the same core: `FileHandler`
//! scans a directory into a `FileNode` tree, `DocumentGenerator` renders the
//! context document, and `FileMonitor` watches for changes. Running without
//! egui/eframe lets the tool operate in non-graphical environments such as
//! remote SSH sessions or CI jobs.
//!
//! Selection model: unlike the GUI (where the user ticks boxes in a tree),
//! the CLI selects every file that survives ignore filtering, optionally
//! narrowed by `--include` gitignore-style globs.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use ignore::overrides::OverrideBuilder;
use log::{debug, info, warn};

use crate::constants::{
    DEFAULT_IGNORE_PATTERNS_ARRAY, DEFAULT_OUTPUT_FILENAME_BASE, DEFAULT_OUTPUT_FORMAT, OutputFormat,
};
use crate::document_generator::DocumentGenerator;
use crate::error::{AppError, Result};
use crate::events::AppEvent;
use crate::file_handler::{FileHandler, FileNode};
use crate::file_monitor::FileMonitor;

/// Parsed CLI configuration (everything [`run`] needs).
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub directory: PathBuf,
    /// `None` when `--stdout` (or `--list`) is requested.
    pub output_path: Option<PathBuf>,
    pub format: OutputFormat,
    pub include_patterns: Vec<String>,
    pub extra_ignore_patterns: Vec<String>,
    pub use_default_ignores: bool,
    pub follow_symlinks: bool,
    pub watch: bool,
    pub list_only: bool,
}

const HELP_TEXT: &str = r#"context_builder [OPTIONS] [PATH]

Generate a context document (Markdown or AsciiDoc) from project files,
headless - no display required. Started without arguments, the GUI opens.

Arguments:
  [PATH]                 Project directory to scan [default: .]

Selection (default: every file that survives ignore filtering):
  --include <GLOB>       Gitignore-style glob; select only matches. Repeatable.
                         Prefix with ! to exclude instead (e.g. --include '!*.lock').
  --ignore <PATTERN>     Extra ignore pattern on top of the defaults. Repeatable.
  --no-default-ignores  Use only --ignore patterns (.gitignore rules still apply).
  --no-follow-symlinks   Treat symlinks/junctions as plain files.

Output:
  -o, --output <FILE>    Output file [default: <PATH>/project_structure.<ext>]
      --stdout           Write the document to stdout
  -f, --format <FMT>     md (default) or adoc
      --list             Print the filtered file tree; no document

Modes:
      --watch            Keep running; update the document on changes
                         (file output required; Ctrl-C to stop - writes are atomic)
      --gui              Launch the graphical interface

  -h, --help             Print this help
  -V, --version          Print version

The output document is never included in itself. Set RUST_LOG=debug for
verbose logging.

Examples:
  context_builder --list ~/projects/api
  context_builder ~/projects/api -o /tmp/context.md
  context_builder . --include 'src/**/*.rs' --include '*.toml' --stdout
  context_builder . --watch -f adoc
"#;

/// Entry point for the CLI frontend. `args` excludes the program name.
pub fn run(args: Vec<String>) -> Result<()> {
    if args.iter().any(|a| a == "-h" || a == "help" || a == "--help") {
        print!("{}", HELP_TEXT);
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("context_builder {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cfg = parse_args(&args)?;

    let mut ignore_patterns = effective_ignore_patterns(&cfg);
    // Feedback-loop guard: keep the output document itself out of the scan when
    // it lives inside the scanned directory (the canonicalized exclusion in
    // [`build_selection`] covers the remaining path spellings).
    if let Some(out) = &cfg.output_path {
        if let Ok(rel) = out.strip_prefix(&cfg.directory) {
            ignore_patterns.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    let handler = FileHandler::with_options(cfg.directory.clone(), cfg.follow_symlinks)?;
    let tree = handler.scan_directory(ignore_patterns.clone())?;

    if cfg.list_only {
        let all_files = build_selection(&tree, &cfg.directory, &[], None)?;
        let generator = DocumentGenerator::new(cfg.directory.clone(), all_files);
        let tree_lines = generator.generate_tree_lines(&tree)?;
        if tree_lines.lines().count() <= 1 {
            eprintln!("(nothing visible after ignore filtering)");
        } else {
            print!("{}", tree_lines);
        }
        return Ok(());
    }

    let selected = build_selection(
        &tree,
        &cfg.directory,
        &cfg.include_patterns,
        cfg.output_path.as_deref(),
    )?;
    if selected.is_empty() {
        return Err(AppError::OperationFailed(
            "no files selected after applying ignore/include patterns".to_string(),
        ));
    }

    match &cfg.output_path {
        None => {
            let document =
                render_document_to_string(&cfg.directory, &tree, &selected, cfg.format)?;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle
                .write_all(document.as_bytes())
                .and_then(|()| handle.write_all(b"\n"))
                .and_then(|()| handle.flush())
                .map_err(|e| {
                    AppError::new_io_error(e, None, "failed to write document to stdout".to_string())
                })?;
        }
        Some(output) => {
            DocumentGenerator::new(cfg.directory.clone(), selected.clone())
                .generate_full_document(&tree, output, cfg.format)?;
            eprintln!(
                "Wrote {} ({} files, {})",
                output.display(),
                selected.len(),
                cfg.format.name()
            );
        }
    }

    if cfg.watch {
        return run_watch(cfg, selected, ignore_patterns);
    }

    Ok(())
}

/// Parses CLI arguments (excluding the program name) into a [`CliConfig`].
pub fn parse_args(args: &[String]) -> Result<CliConfig> {
    let mut output: Option<PathBuf> = None;
    let mut stdout = false;
    let mut format = DEFAULT_OUTPUT_FORMAT;
    let mut include_patterns: Vec<String> = Vec::new();
    let mut extra_ignore_patterns: Vec<String> = Vec::new();
    let mut use_default_ignores = true;
    let mut follow_symlinks = true;
    let mut watch = false;
    let mut list_only = false;

    let mut positional: Vec<PathBuf> = Vec::new();
    let mut no_more_flags = false;
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        if no_more_flags {
            positional.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        match arg {
            "-o" | "--output" => {
                let value = value_of(args, i, "--output")?;
                output = Some(PathBuf::from(&value));
                i += 2;
            }
            "-f" | "--format" => {
                let value = value_of(args, i, "--format")?;
                format = parse_format(&value)?;
                i += 2;
            }
            "--include" => {
                let value = value_of(args, i, "--include")?;
                include_patterns.push(value);
                i += 2;
            }
            "--ignore" => {
                let value = value_of(args, i, "--ignore")?;
                extra_ignore_patterns.push(value);
                i += 2;
            }
            "--stdout" => {
                stdout = true;
                i += 1;
            }
            "--watch" => {
                watch = true;
                i += 1;
            }
            "--list" => {
                list_only = true;
                i += 1;
            }
            "--no-default-ignores" => {
                use_default_ignores = false;
                i += 1;
            }
            "--no-follow-symlinks" => {
                follow_symlinks = false;
                i += 1;
            }
            "--" => {
                no_more_flags = true;
                i += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(AppError::OperationFailed(format!(
                    "unknown flag '{}'; pass --help for usage",
                    other
                )));
            }
            other => {
                positional.push(PathBuf::from(other));
                i += 1;
            }
        }
    }

    if positional.len() > 1 {
        return Err(AppError::OperationFailed(
            "expected at most one PATH argument".to_string(),
        ));
    }
    let directory = positional.into_iter().next().unwrap_or_else(|| PathBuf::from("."));

    if stdout && output.is_some() {
        return Err(AppError::OperationFailed(
            "--stdout and --output are mutually exclusive".to_string(),
        ));
    }
    if stdout && watch {
        return Err(AppError::OperationFailed(
            "--stdout cannot be combined with --watch".to_string(),
        ));
    }
    if list_only && watch {
        return Err(AppError::OperationFailed(
            "--list cannot be combined with --watch".to_string(),
        ));
    }
    if list_only && (stdout || output.is_some()) {
        return Err(AppError::OperationFailed(
            "--list prints the tree; it does not write a document".to_string(),
        ));
    }

    let output_path = if stdout || list_only {
        None
    } else {
        Some(output.unwrap_or_else(|| {
            directory.join(format!(
                "{}.{}",
                DEFAULT_OUTPUT_FILENAME_BASE,
                format.extension()
            ))
        }))
    };

    Ok(CliConfig {
        directory,
        output_path,
        format,
        include_patterns,
        extra_ignore_patterns,
        use_default_ignores,
        follow_symlinks,
        watch,
        list_only,
    })
}

/// Default ignore patterns plus the user's extras.
fn effective_ignore_patterns(cfg: &CliConfig) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    if cfg.use_default_ignores {
        patterns.extend(DEFAULT_IGNORE_PATTERNS_ARRAY.iter().map(|p| p.to_string()));
    }
    patterns.extend(cfg.extra_ignore_patterns.iter().cloned());
    patterns
}

/// Collects every regular file in the scanned tree (walk order is the
/// tree's sorted order: directories first, then files, case-insensitive).
fn collect_files(node: &FileNode, out: &mut Vec<PathBuf>) {
    if !node.is_dir {
        out.push(node.path.clone());
    }
    for child in &node.children {
        collect_files(child, out);
    }
}

/// Applies the CLI selection model to a scanned tree:
/// - no `--include` patterns: every file is selected;
/// - whitelist patterns present: only matching files stay;
/// - `!`-prefixed blacklist patterns drop matches in either mode.
/// The output document (`exclude_output`) is always removed so the
/// document can never contain itself.
pub fn build_selection(
    root: &FileNode,
    directory: &Path,
    include_patterns: &[String],
    exclude_output: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(root, &mut files);

    let mut selected: Vec<PathBuf> = if include_patterns.is_empty() {
        files
    } else {
        let mut builder = OverrideBuilder::new(directory);
        for pattern in include_patterns {
            builder.add(pattern).map_err(|e| {
                AppError::OperationFailed(format!(
                    "invalid --include pattern '{}': {}",
                    pattern, e
                ))
            })?;
        }
        let overrides = builder.build().map_err(AppError::IgnoreBuild)?;
        let has_whitelist = include_patterns.iter().any(|p| !p.starts_with('!'));

        files
            .into_iter()
            .filter(|file| {
                let rel = match file.strip_prefix(directory) {
                    Ok(rel) => rel.to_path_buf(),
                    Err(_) => return false, // outside the scanned root: skip
                };
                match overrides.matched(&rel, false) {
                    ignore::Match::Whitelist(_) => true,
                    ignore::Match::Ignore(_) => false,
                    ignore::Match::None => !has_whitelist,
                }
            })
            .collect()
    };

    if let Some(out) = exclude_output {
        let out_canon = canonicalize_or_self(out);
        selected.retain(|p| canonicalize_or_self(p) != out_canon);
    }

    Ok(selected)
}

/// Renders the full document into a string (used by `--stdout`).
fn render_document_to_string(
    directory: &Path,
    tree: &FileNode,
    selected: &[PathBuf],
    format: OutputFormat,
) -> Result<String> {
    let generator = DocumentGenerator::new(directory.to_path_buf(), selected.to_vec());
    let mut document = generator.generate_structure_string(tree, format)?;
    document.push_str("\n\n");
    document.push_str(&generator.generate_files_string(format)?);
    Ok(document)
}

/// Watch loop: mirrors the GUI's event handling without a repaint loop.
///
/// - a modified selected file triggers a section update
///   ([`DocumentGenerator::update_file_section_in_document`]);
/// - a structural change triggers a rescan, and the document is regenerated
///   only when the selected set actually changed. The atomic document write
///   itself fires watcher events, so regenerating unconditionally here would
///   oscillate forever.
fn run_watch(cfg: CliConfig, mut selected: Vec<PathBuf>, ignore_patterns: Vec<String>) -> Result<()> {
    let output_path = cfg
        .output_path
        .clone()
        .expect("parse_args rejects --watch without a file output");

    let (event_sender, event_receiver) = mpsc::channel();
    let mut monitor = FileMonitor::new(event_sender);
    monitor.start_monitoring(cfg.directory.clone())?;

    eprintln!(
        "Watching {} - press Ctrl-C to stop (writes are atomic)",
        cfg.directory.display()
    );

    let mut current_selection: HashSet<PathBuf> = selected.iter().cloned().collect();

    while let Ok(event) = event_receiver.recv() {
        match event {
            AppEvent::FileModifiedDebounced(path) => {
                // Watcher events arrive under OS-reported absolute paths (notify
                // joins the cwd when the watch root is relative), while
                // selections and the generator use the directory exactly as
                // given. Translate the event back into walker coordinates.
                let path = to_walker_path(&path, &cfg.directory);
                // Selections may additionally run through a symlink branch;
                // compare canonicalized forms, same as the GUI does.
                let changed = canonicalize_or_self(&path);
                let is_selected = current_selection
                    .iter()
                    .any(|s| canonicalize_or_self(s) == changed);
                if !is_selected {
                    debug!("modified path {:?} is not selected; skipping", path);
                    continue;
                }
                let generator = DocumentGenerator::new(cfg.directory.clone(), selected.clone());
                match generator.update_file_section_in_document(&output_path, &path, cfg.format) {
                    Ok(()) => info!("updated section for {:?}", path),
                    Err(e) => warn!("section update failed for {:?}: {}", path, e),
                }
            }
            AppEvent::DirectoryContentChanged => {
                let handler = match FileHandler::with_options(
                    cfg.directory.clone(),
                    cfg.follow_symlinks,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("rescan failed: {}", e);
                        continue;
                    }
                };
                let tree = match handler.scan_directory(ignore_patterns.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("rescan failed: {}", e);
                        continue;
                    }
                };
                let new_selected = match build_selection(
                    &tree,
                    &cfg.directory,
                    &cfg.include_patterns,
                    cfg.output_path.as_deref(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("rebuilding selection failed: {}", e);
                        continue;
                    }
                };
                let new_selection: HashSet<PathBuf> = new_selected.iter().cloned().collect();
                if new_selection == current_selection {
                    continue; // e.g. only the output document or temp files changed
                }
                current_selection = new_selection;
                selected = new_selected;
                if selected.is_empty() {
                    eprintln!("selection is empty; waiting for matching files to appear");
                    continue;
                }
                let generator = DocumentGenerator::new(cfg.directory.clone(), selected.clone());
                match generator.generate_full_document(&tree, &output_path, cfg.format) {
                    Ok(()) => info!("selection changed; document regenerated"),
                    Err(e) => warn!("regeneration failed: {}", e),
                }
            }
            AppEvent::WatcherError(e) => {
                eprintln!("file watcher error: {}", e);
            }
            _ => {}
        }
    }

    Ok(())
}

fn value_of(args: &[String], i: usize, flag: &str) -> Result<String> {
    args.get(i + 1).cloned().ok_or_else(|| {
        AppError::OperationFailed(format!("{} requires a value", flag))
    })
}

fn parse_format(value: &str) -> Result<OutputFormat> {
    match value.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Ok(OutputFormat::Markdown),
        "adoc" | "asciidoc" => Ok(OutputFormat::Adoc),
        other => Err(AppError::OperationFailed(format!(
            "unknown format '{}': expected 'md' or 'adoc'",
            other
        ))),
    }
}

/// Canonicalizes a path for comparison purposes, falling back to the input
/// when canonicalization fails (e.g. the file was just deleted). Same helper
/// as `app.rs`; duplicated to keep the CLI free of GUI-module imports.
fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Translates an OS-reported (absolute) watcher path into the coordinate
/// system of the scan: rooted at the directory exactly as given on the
/// command line. Falls back to the raw event path when the file lives
/// outside the watched directory.
fn to_walker_path(event_path: &Path, directory: &Path) -> PathBuf {
    let canon_event = canonicalize_or_self(event_path);
    let canon_dir = canonicalize_or_self(directory);
    if let Ok(rel) = canon_event.strip_prefix(&canon_dir) {
        directory.join(rel)
    } else {
        event_path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cb_cli_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src").join("lib.rs"), "pub fn f() {}").unwrap();
        std::fs::write(root.join("docs").join("adr.md"), "# ADR").unwrap();
        std::fs::write(root.join("README.md"), "# Project").unwrap();
        std::fs::write(root.join("yarn.lock"), "# lock").unwrap();
        root
    }

    fn scan(root: &Path) -> FileNode {
        FileHandler::with_options(root.to_path_buf(), true)
            .unwrap()
            .scan_directory(vec![])
            .unwrap()
    }

    fn selection(root: &Path, includes: &[&str]) -> Vec<String> {
        let tree = scan(root);
        let patterns: Vec<String> = includes.iter().map(|s| s.to_string()).collect();
        build_selection(&tree, root, &patterns, None)
            .unwrap()
            .into_iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn parse_defaults() {
        let cfg = parse_args(&args(&[])).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("."));
        assert_eq!(
            cfg.output_path,
            Some(PathBuf::from("./project_structure.md"))
        );
        assert_eq!(cfg.format, OutputFormat::Markdown);
        assert!(cfg.use_default_ignores);
        assert!(cfg.follow_symlinks);
        assert!(!cfg.watch);
        assert!(!cfg.list_only);
        assert!(cfg.output_path.is_some());
    }

    #[test]
    fn parse_flags() {
        let cfg = parse_args(&args(&[
            "/tmp/proj", "-o", "out.adoc", "-f", "adoc",
            "--include", "src/**/*.rs", "--ignore", "vendor/",
            "--no-default-ignores", "--no-follow-symlinks", "--watch",
        ]))
        .unwrap();
        assert_eq!(cfg.directory, PathBuf::from("/tmp/proj"));
        assert_eq!(cfg.output_path, Some(PathBuf::from("out.adoc")));
        assert_eq!(cfg.format, OutputFormat::Adoc);
        assert_eq!(cfg.include_patterns, vec!["src/**/*.rs".to_string()]);
        assert_eq!(cfg.extra_ignore_patterns, vec!["vendor/".to_string()]);
        assert!(!cfg.use_default_ignores);
        assert!(!cfg.follow_symlinks);
        assert!(cfg.watch);
    }

    #[test]
    fn parse_rejects_conflicts() {
        assert!(parse_args(&args(&["--watch", "--stdout"])).is_err());
        assert!(parse_args(&args(&["--stdout", "-o", "x.md"])).is_err());
        assert!(parse_args(&args(&["--list", "--watch"])).is_err());
        assert!(parse_args(&args(&["--list", "--stdout"])).is_err());
        assert!(parse_args(&args(&["--frobnicate"])).is_err());
        assert!(parse_args(&args(&["-o"])).is_err());
        assert!(parse_args(&args(&["-f", "pdf"])).is_err());
        assert!(parse_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn selection_defaults_to_all_files() {
        let root = fixture();
        assert_eq!(
            selection(&root, &[]),
            vec![
                "docs/adr.md".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
                "README.md".to_string(),
                "yarn.lock".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_whitelist_narrows_selection() {
        let root = fixture();
        assert_eq!(
            selection(&root, &["src/**/*.rs"]),
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_blacklist_excludes() {
        let root = fixture();
        let sel = selection(&root, &["!*.md", "!yarn.lock"]);
        assert!(sel.contains(&"src/main.rs".to_string()));
        assert!(!sel.iter().any(|p| p.ends_with(".md")));
        assert!(!sel.contains(&"yarn.lock".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mixed_whitelist_and_blacklist() {
        let root = fixture();
        assert_eq!(
            selection(&root, &["src/**", "!**/lib.rs"]),
            vec!["src/main.rs".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_document_is_excluded_from_selection() {
        let root = fixture();
        let tree = scan(&root);
        let out = root.join("project_structure.md");
        std::fs::write(&out, "previous run").unwrap();
        let sel = build_selection(&tree, &root, &[], Some(&out)).unwrap();
        assert!(!sel.contains(&out));
        assert!(sel.contains(&root.join("src").join("main.rs")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rendered_document_contains_structure_and_files() {
        let root = fixture();
        let tree = scan(&root);
        let selected = build_selection(
            &tree,
            &root,
            &["src/**".to_string()],
            None,
        )
        .unwrap();
        let doc = render_document_to_string(&root, &tree, &selected, OutputFormat::Markdown).unwrap();
        assert!(doc.contains("## Project Structure"));
        assert!(doc.contains("### `src/main.rs`"));
        assert!(doc.contains("fn main() {}"));
        assert!(!doc.contains("### `README.md`"), "non-selected files must not appear");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_tree_lines_show_everything_scanned() {
        let root = fixture();
        let tree = scan(&root);
        let all = build_selection(&tree, &root, &[], None).unwrap();
        let generator = DocumentGenerator::new(root.clone(), all);
        let lines = generator.generate_tree_lines(&tree).unwrap();
        assert!(lines.contains("docs/"));
        assert!(lines.contains("adr.md"));
        assert!(lines.contains("yarn.lock"));
        assert!(lines.starts_with(
            root.file_name().unwrap().to_string_lossy().as_ref()
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
