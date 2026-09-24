use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use egui::Context;
use log::{debug, info, warn, error};
use egui_twemoji::EmojiLabel;
use egui::RichText;
use egui_extras;

use crate::constants::{UI_STATUS_MESSAGE_DURATION, OutputFormat, DEFAULT_OUTPUT_FORMAT, DEFAULT_OUTPUT_FILENAME_BASE};
use crate::error::Result;
use crate::events::AppEvent;
use crate::file_handler::{FileHandler, FileNode};
use crate::file_monitor::FileMonitor;
use crate::document_generator::DocumentGenerator;
use crate::ui_tree_handler::UITreeHandler;

// Initial default ignore patterns
const DEFAULT_IGNORE_PATTERNS_ARRAY: &[&str] = &[
    // Common VCS and build artifacts
    ".git/", ".hg/", ".svn/",
    "target/", "build/", "dist/", "pkg/", "node_modules/",
    // Python specific
    "__pycache__/", "*.pyc", "*.pyo", "*.pyd",
    ".env", ".venv", "venv/", "env/",
    // "requirements.txt", // Often useful to see, but can be configured if user wants it ignored
    // Node specific
    "package-lock.json", "yarn.lock",
    // Common OS files
    ".DS_Store", "Thumbs.db",
    // Log files
    "*.log",
    // Temporary files
    "*.tmp", "*.swp", "*.swo",
    // Compiled outputs & binaries from various languages/tools
    "*.o", "*.so", "*.a", "*.dylib",
    "*.exe", "*.dll", "*.lib", "*.exp", "*.obj", "*.def",
    // Archives & compressed files
    "*.zip", "*.tar", "*.gz", "*.rar",
    // Image/Media (usually not context for code)
    "*.ico", "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.tiff", "*.svg",
    "*.mp3", "*.mp4", "*.avi",
    // Database files
    "*.db", "*.sqlite", "*.sqlite3",
    // IDE specific
    ".idea/", ".vscode/", "*.sublime-project", "*.sublime-workspace",
];

pub struct ContextBuilderApp {
    // Core state
    current_directory: Option<PathBuf>,
    root_file_node: Option<FileNode>,
    selected_output_format: OutputFormat,
    output_file_path: Option<PathBuf>,
    
    // UI state
    ui_tree_handler: UITreeHandler,
    ignore_patterns_text: String, // New field for mutable ignore patterns
    follow_symlinks: bool, // whether scans descend into symlinks/junctions
    
    // Communication
    event_sender: mpsc::Sender<AppEvent>,
    event_receiver: mpsc::Receiver<AppEvent>,
    
    // File monitoring
    file_monitor: FileMonitor,
    monitoring_active: bool,
    
    // UI feedback
    status_message: Option<(String, Instant)>,
    error_message: Option<String>,
    
    // Operation states
    is_loading_directory: bool,
    is_generating_document: bool,
}

impl ContextBuilderApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (event_sender, event_receiver) = mpsc::channel();
        let file_monitor = FileMonitor::new(event_sender.clone());
        
        // Install image loaders for egui-twemoji (required for rendering SVG and PNG emotes)
        egui_extras::install_image_loaders(&_cc.egui_ctx);
        
        Self {
            current_directory: None,
            root_file_node: None,
            selected_output_format: DEFAULT_OUTPUT_FORMAT,
            output_file_path: None,
            ui_tree_handler: UITreeHandler::new(),
            ignore_patterns_text: DEFAULT_IGNORE_PATTERNS_ARRAY.join("\n"), // Initialize with default patterns
            follow_symlinks: true,
            event_sender,
            event_receiver,
            file_monitor,
            monitoring_active: false,
            status_message: None,
            error_message: None,
            is_loading_directory: false,
            is_generating_document: false,
        }
    }

    fn set_status_message(&mut self, message: String) {
        self.status_message = Some((message, Instant::now()));
        self.error_message = None; // Clear error when showing status
    }

    fn set_error_message(&mut self, message: String) {
        self.error_message = Some(message);
        self.status_message = None; // Clear status when showing error
    }

    fn clear_messages(&mut self) {
        self.status_message = None;
        self.error_message = None;
    }

    fn open_directory_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.open_directory(path, self.ignore_patterns_text.lines().map(|s| s.to_string()).collect());
        }
    }

    fn open_directory(&mut self, directory: PathBuf, ignore_patterns: Vec<String>) {
        info!("Opening directory: {:?}", directory);
        self.is_loading_directory = true;
        self.set_status_message("Scanning directory...".to_string());
        
        // Stop any existing monitoring (for structural changes)
        if let Err(e) = self.file_monitor.stop_monitoring() {
            warn!("Error stopping file monitor: {}", e);
        }
        
        // Start monitoring for structural changes immediately
        let dir_for_monitor = directory.clone();
        if let Err(e) = self.file_monitor.start_monitoring(dir_for_monitor) {
            error!("Failed to start directory monitoring: {}", e);
            self.set_error_message(format!("Failed to start directory monitoring: {}", e));
            // Proceed without monitoring if it fails, but inform the user
        }

        self.monitoring_active = false; // Document monitoring is off by default
        
        // Clear current state
        self.current_directory = Some(directory.clone());
        self.root_file_node = None;
        self.output_file_path = None;
        self.ui_tree_handler = UITreeHandler::new();
        
        // Start directory scan in background thread
        let sender = self.event_sender.clone();
        let follow_symlinks = self.follow_symlinks;
        thread::spawn(move || {
            let result = FileHandler::with_options(directory, follow_symlinks)
                .and_then(|handler| handler.scan_directory(ignore_patterns));
            
            if let Err(e) = sender.send(AppEvent::DirectoryScanComplete(result)) {
                error!("Failed to send directory scan result: {}", e);
            }
        });
    }

    fn handle_directory_scan_complete(&mut self, result: Result<FileNode>) {
        self.is_loading_directory = false;
        
        match result {
            Ok(root_node) => {
                info!("Directory scan completed successfully");
                self.root_file_node = Some(root_node.clone());
                self.ui_tree_handler.build_from_file_node(&root_node);
                self.set_status_message("Directory loaded successfully".to_string());
                
                // Suggest default output path based on directory and default format
                if let Some(dir) = &self.current_directory {
                    self.output_file_path = Some(dir.join(format!("{}.{}", DEFAULT_OUTPUT_FILENAME_BASE, DEFAULT_OUTPUT_FORMAT.extension())));
                }
            }
            Err(e) => {
                error!("Directory scan failed: {}", e);
                self.set_error_message(format!("Failed to scan directory: {}", e));
                self.current_directory = None;
                self.output_file_path = None; // Clear path on scan failure
            }
        }
    }

    fn start_monitoring(&mut self) {
        if self.current_directory.is_some() {
            // First generate the initial document (pass false to suppress completion message here)
            self.generate_document(false);

            // Enable automatic document updates on file modifications
            self.monitoring_active = true;
            self.set_status_message("Monitoring selected files for changes and updating document".to_string());
        } else {
            self.set_error_message("Cannot start monitoring: Current directory not set.".to_string());
        }
    }

    fn stop_monitoring(&mut self) {
        // Only disable automatic document updates
        self.monitoring_active = false;
        self.set_status_message("Document updates stopped".to_string());
        // The underlying file monitor for structural changes remains active
    }

    fn generate_document(&mut self, show_completion_message: bool) {
        if let (Some(directory), Some(root_node), Some(output_path)) = (&self.current_directory, &self.root_file_node, &self.output_file_path) {
            let selected_files = self.ui_tree_handler.get_selected_files();

            if selected_files.is_empty() {
                if show_completion_message {
                    self.set_error_message("Please select at least one file to generate document".to_string());
                }
                return;
            }

            // Clone values before setting status message to avoid borrow issues
            let directory = directory.clone();
            let root_node = root_node.clone();
            let output_path = output_path.clone();
            let output_format = self.selected_output_format;

            self.is_generating_document = true;
            if show_completion_message {
                self.set_status_message("Generating document...".to_string());
            }

            let sender = self.event_sender.clone();

            thread::spawn(move || {
                let generator = DocumentGenerator::new(directory.clone(), selected_files);
                
                let result = generator.generate_full_document(&root_node, &output_path, output_format);

                if let Err(e) = sender.send(AppEvent::DocumentGenerationComplete(result)) {
                    error!("Failed to send document generation result: {}", e);
                }
            });
        } else if self.current_directory.is_none() {
             if show_completion_message {
                 self.set_error_message("Please select a directory first".to_string());
             }
        } else if self.root_file_node.is_none() {
             if show_completion_message {
                 self.set_error_message("Directory scanning not complete".to_string());
             }
        } else if self.output_file_path.is_none() {
             if show_completion_message {
                 self.set_error_message("Please choose an output file path".to_string());
             }
        }
    }

    fn handle_document_generation_complete(&mut self, result: Result<()>) {
        self.is_generating_document = false;

        match result {
            Ok(()) => {
                if let Some(output_path) = &self.output_file_path {
                    self.set_status_message(format!("Document generated: {}", output_path.display()));
                } else {
                    self.set_status_message("Document generated successfully (path unknown)".to_string());
                }
            }
            Err(e) => {
                error!("Document generation failed: {}", e);
                self.set_error_message(format!("Failed to generate document: {}", e));
            }
        }
    }

    fn handle_file_modified(&mut self, file_path: PathBuf) {
        debug!("Handling file modification: {:?}", file_path);

        if let (Some(directory), Some(output_path)) = (&self.current_directory, &self.output_file_path) {
            let selected_files = self.ui_tree_handler.get_selected_files();

            // File events arrive under the path the OS reported (typically the real
            // path), while selections carry the walker path (which may run through a
            // symlink/junction branch). Compare canonicalized forms so a change made
            // through either name matches the selection.
            let canonical_event_path = canonicalize_or_self(&file_path);
            let matches_selection = selected_files
                .iter()
                .any(|selected| canonicalize_or_self(selected) == canonical_event_path);

            if matches_selection {
                let directory = directory.clone();
                let sender = self.event_sender.clone();
                let markdown_path = output_path.clone();

                let output_format = self.selected_output_format;

                thread::spawn(move || {
                    let generator = DocumentGenerator::new(directory.clone(), selected_files);

                    let result = generator.update_file_section_in_document(&markdown_path, &file_path, output_format);

                    if let Err(e) = sender.send(AppEvent::PartialDocumentUpdateComplete(result)) {
                        error!("Failed to send partial document update result: {}", e);
                    }
                });
            } else {
                debug!("Modified file {:?} not in selected files. Skipping partial update.", file_path);
            }
        } else {
             debug!("Modified file {:?} received, but directory or output path not set. Skipping partial update.", file_path);
        }
    }

    fn handle_partial_document_update_complete(&mut self, result: Result<()>) {
        match result {
            Ok(()) => {
                debug!("Partial document update completed successfully");
            }
            Err(e) => {
                warn!("Partial document update failed: {}", e);
            }
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_receiver.try_recv() {
            match event {
                AppEvent::DirectoryScanComplete(result) => {
                    self.handle_directory_scan_complete(result);
                }
                AppEvent::FileModifiedDebounced(file_path) => {
                    self.handle_file_modified(file_path);
                }
                AppEvent::DocumentGenerationComplete(result) => {
                    self.handle_document_generation_complete(result);
                }
                AppEvent::PartialDocumentUpdateComplete(result) => {
                    self.handle_partial_document_update_complete(result);
                }
                AppEvent::DirectoryContentChanged => {
                    info!("Directory content changed, re-scanning...");
                    if let Some(dir) = self.current_directory.clone() {
                        // Re-scan with current ignore patterns
                        self.open_directory(dir, self.ignore_patterns_text.lines().map(|s| s.to_string()).collect());
                    }
                }
                AppEvent::WatcherError(error) => {
                    error!("File watcher error: {}", error);
                    self.set_error_message(format!("File watcher error: {}", error));
                    self.monitoring_active = false;
                }
                AppEvent::StatusMessage(message) => {
                    self.set_status_message(message);
                }
                AppEvent::ErrorMessage(message) => {
                    self.set_error_message(message);
                }
            }
        }
    }

    fn render_directory_selection(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.heading("Project Directory");
                ui.add_space(5.0);
                
                ui.horizontal(|ui| {
                    ui.label("Current directory:");
                    ui.add_space(10.0);
                    
                    if let Some(directory) = &self.current_directory {
                        ui.monospace(directory.display().to_string());
                    } else {
                        ui.weak("No directory selected");
                    }
                });
                
                ui.add_space(8.0);
                
                ui.horizontal(|ui| {
                    if ui.add_sized([120.0, 30.0], egui::Button::new("Browse...")).clicked() {
                        self.open_directory_dialog();
                    }
                    
                    if self.current_directory.is_some() {
                        ui.add_space(10.0);
                        if ui.add_sized([100.0, 30.0], egui::Button::new("🔄 Refresh")).clicked() {
                            if let Some(dir) = self.current_directory.clone() {
                                // Refresh with current ignore patterns
                                self.open_directory(dir, self.ignore_patterns_text.lines().map(|s| s.to_string()).collect());
                            }
                        }
                    }
                });
            });
        });
    }

    fn render_file_tree(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        
        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.heading("File Selection");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.ui_tree_handler.has_selection() {
                            ui.colored_label(
                                egui::Color32::from_rgb(0, 150, 0), 
                                format!("✓ {} files selected", self.ui_tree_handler.get_selected_files().len())
                            );
                        } else if self.current_directory.is_some() {
                            ui.weak("No files selected");
                        }
                    });
                });
                
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(5.0);
                
                if self.is_loading_directory {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.spinner();
                        ui.add_space(10.0);
                        ui.label("🔍 Scanning directory...");
                        ui.add_space(20.0);
                    });
                } else if self.current_directory.is_some() {
                    egui::ScrollArea::vertical()
                        .id_source("file_tree_scroll_area")
                        .max_height(350.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            if self.ui_tree_handler.tree_nodes.is_empty() {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(20.0);
                                    ui.weak("📁 Empty directory or all files filtered");
                                    ui.add_space(20.0);
                                });
                            } else {
                                let selection_changed = self.ui_tree_handler.render_tree(ui);
                                
                                // If automatic document updating is active and selection changed, regenerate document
                                if selection_changed && self.monitoring_active {
                                    self.generate_document(false);
                                }
                            }
                        });
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(30.0);
                        ui.heading("👆");
                        ui.add_space(10.0);
                        ui.weak("Please select a directory above to view its structure");
                        ui.add_space(30.0);
                    });
                }
            });
        });
    }

    fn render_output_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);

        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.heading("Output Settings");
                ui.add_space(5.0);

                // Output Format Selection
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    let old_format = self.selected_output_format;
                    ui.radio_value(&mut self.selected_output_format, OutputFormat::Markdown, OutputFormat::Markdown.name());
                    ui.radio_value(&mut self.selected_output_format, OutputFormat::Adoc, OutputFormat::Adoc.name());
                    
                    // If the format changed and a path is set, update the path extension
                    if old_format != self.selected_output_format {
                        if let Some(path) = &mut self.output_file_path {
                             let new_extension = self.selected_output_format.extension();
                             // Only change the extension if the current path has one, or if it's the default base name
                             if path.extension().is_some() || path.file_name().and_then(|name| name.to_str()).map_or(false, |name| name.starts_with(DEFAULT_OUTPUT_FILENAME_BASE)) {
                                 path.set_extension(new_extension);
                                 debug!("Updated output file extension to {} due to format change.", new_extension);
                             } else {
                                 debug!("Output path has no extension and is not default base name, not auto-updating extension.");
                             }
                        }
                    }
                });
                ui.add_space(8.0);

                // Output File Path Selection
                ui.horizontal(|ui| {
                    ui.label("Save to:");
                    ui.add_space(10.0);

                    if let Some(path) = &self.output_file_path {
                         ui.monospace(path.display().to_string());
                    } else {
                        ui.weak("Click 'Choose File' to select output path");
                    }
                });

                ui.add_space(5.0);

                ui.horizontal(|ui| {
                     if ui.add_enabled(self.current_directory.is_some(), egui::Button::new("Choose File...")).clicked() {
                        self.open_save_file_dialog();
                     }
                     // Optional: Add a button to reset output path to default suggestion
                     if self.current_directory.is_some() && self.output_file_path.is_some() && self.output_file_path.as_ref().map(|p| p.file_name().unwrap_or_default().to_string_lossy().starts_with(DEFAULT_OUTPUT_FILENAME_BASE)).unwrap_or(false) {
                          // This check prevents the 'Reset' button appearing unless a directory is set and the path *looks* like the default
                     } else if self.current_directory.is_some() && (self.output_file_path.is_none() || !self.output_file_path.as_ref().unwrap().file_name().unwrap_or_default().to_string_lossy().starts_with(DEFAULT_OUTPUT_FILENAME_BASE)) {
                         // Add 'Suggest Default' button if directory is set and current path is not the default suggestion
                         if ui.button("Suggest Default").clicked() {
                             if let Some(dir) = &self.current_directory {
                                 self.output_file_path = Some(dir.join(format!("{}.{}", DEFAULT_OUTPUT_FILENAME_BASE, self.selected_output_format.extension())));
                             }
                         }
                     }
                });
            });
        });
    }

    fn open_save_file_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new();

        if let Some(dir) = &self.current_directory {
            dialog = dialog.set_directory(dir);
        }

        // Add filters for both Markdown and AsciiDoc
        dialog = dialog.add_filter(OutputFormat::Markdown.name(), &[OutputFormat::Markdown.extension()]);
        dialog = dialog.add_filter(OutputFormat::Adoc.name(), &[OutputFormat::Adoc.extension()]);

        if let Some(mut path) = dialog.save_file() { // Use mut path to allow modification
            // Check if the path already has a file extension
            if path.extension().is_none() {
                // If not, append the extension of the currently selected format
                if let Some(ext) = self.selected_output_format.extension().strip_prefix('.') { // Get extension without leading dot
                    path.set_extension(ext);
                } else {
                    // Handle cases where extension() might return an empty string or no prefix
                    path.set_extension(self.selected_output_format.extension());
                }
                 debug!("Appended extension to path: {:?}", path);
            } else {
                 debug!("Path already has an extension: {:?}", path);
            }

            self.output_file_path = Some(path.clone()); // Store the potentially modified path
            
            // Determine the format from the selected file's extension (keep existing logic)
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                self.selected_output_format = match ext.to_lowercase().as_str() {
                    "md" => OutputFormat::Markdown,
                    "adoc" => OutputFormat::Adoc,
                    _ => {
                        // If extension is unknown, keep the current selection and maybe warn
                        warn!("Selected file has unknown extension: {}. Keeping current format selection.", ext);
                        self.selected_output_format // Keep current
                    }
                };
            } else {
                 // This case should ideally not be reached if we appended an extension above,
                 // but handle defensively if the selected path had no extension initially.
                 warn!("Selected file has no extension after processing. Keeping current format selection.");
                 // self.selected_output_format // Keep current
            }

            // Note: We don't trigger generation immediately, user clicks 'Generate'
        }
    }

    fn render_ignore_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);

        egui::CollapsingHeader::new("Ignore Patterns")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(5.0);

                ui.label("Enter patterns (one per line) to ignore files/directories (e.g., `.git/`, `target/`, `*.log`):");
                ui.add_space(5.0);

                egui::ScrollArea::vertical()
                    .id_source("ignore_patterns_scroll_area")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut self.ignore_patterns_text)
                            .desired_width(ui.available_width())
                            .frame(true)
                            .hint_text("Enter ignore patterns here..."));
                    });
                
                ui.add_space(8.0);
                
                let follow_changed = ui
                    .add(egui::Checkbox::new(&mut self.follow_symlinks, "Follow symlinks/junctions"))
                    .on_hover_text("When enabled, scanning descends into symbolic links and Windows junctions, so files behind them appear in the tree and the document. Changing this rescans the current directory.")
                    .changed();
                
                let apply_clicked = ui.button("Apply Patterns & Rescan").clicked();
                if follow_changed || apply_clicked {
                    if let Some(dir) = self.current_directory.clone() {
                        self.open_directory(dir, self.ignore_patterns_text.lines().map(|s| s.to_string()).collect());
                    } else if apply_clicked {
                        self.set_error_message("Please select a directory first to apply ignore patterns.".to_string());
                    }
                }
            });
    }

    fn render_control_buttons(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        
        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.heading("Actions");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Monitoring status with better visual indication using EmojiLabel
                        if self.monitoring_active {
                            EmojiLabel::new("🟢 Monitoring Active").show(ui);
                        } else {
                            EmojiLabel::new("⚫ Monitoring Inactive").show(ui);
                        }
                    });
                });
                
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(8.0);
                
                let has_selection = self.ui_tree_handler.has_selection();
                let output_path_set = self.output_file_path.is_some(); // Check if output path is set
                let can_generate = has_selection && output_path_set && !self.is_generating_document && !self.is_loading_directory; // Use renamed field
                let can_start = has_selection && output_path_set && !self.monitoring_active && !self.is_generating_document && !self.is_loading_directory; // Ensure output path is set before monitoring
                let can_stop = self.monitoring_active;
                
                // Action buttons in a more organized layout
                ui.horizontal(|ui| {
                    // Generate Document button (primary action) - Renamed
                    let generate_button = egui::Button::new(RichText::new("📝 Generate Document"))
                        .min_size(egui::vec2(180.0, 35.0));

                    if ui.add_enabled(can_generate, generate_button).clicked() { // Use can_generate
                        self.generate_document(true); // Call renamed method
                    }
                    
                    ui.add_space(10.0);
                    
                    // Start monitoring button
                    let start_button = egui::Button::new("▶️ Start Monitoring")
                        .min_size(egui::vec2(130.0, 35.0));
                    
                    if ui.add_enabled(can_start, start_button).clicked() {
                        self.start_monitoring();
                    }
                    
                    // Stop monitoring button
                    let stop_button = egui::Button::new("⏹️ Stop Monitoring")
                        .min_size(egui::vec2(130.0, 35.0));
                    
                    if ui.add_enabled(can_stop, stop_button).clicked() {
                        self.stop_monitoring();
                    }
                });
                
                ui.add_space(5.0);
                
                // Help text
                if !has_selection && self.current_directory.is_some() {
                    ui.horizontal(|ui| {
                        ui.weak("💡 Select files above to enable actions");
                    });
                } else if self.current_directory.is_none() {
                    ui.horizontal(|ui| {
                        ui.weak("💡 Select a directory first");
                    });
                } else if self.output_file_path.is_none() { // Added check for output path
                     ui.horizontal(|ui| {
                         ui.weak("💡 Choose an output file path");
                     });
                }
                 else if self.is_generating_document { // Use renamed field
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Generating document...");
                    });
                }
            });
        });
    }

    fn render_status_messages(&mut self, ui: &mut egui::Ui) {
        // Clean up expired status messages
        if let Some((_, timestamp)) = &self.status_message {
            if timestamp.elapsed() > UI_STATUS_MESSAGE_DURATION {
                self.status_message = None;
            }
        }
        
        // Show status or error message
        if let Some((message, _)) = &self.status_message {
            let message = message.clone(); // Clone to avoid borrowing issues
            
            ui.add_space(10.0);
            
            egui::Frame::none()
                .fill(egui::Color32::from_rgb(240, 255, 240))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0, 150, 0)))
                .inner_margin(egui::Margin::same(10.0))
                .rounding(egui::Rounding::same(5.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("✅");
                        ui.colored_label(egui::Color32::from_rgb(0, 120, 0), message);
                    });
                });
        } else if let Some(error_message) = &self.error_message {
            let error_message = error_message.clone(); // Clone to avoid borrowing issues
            
            ui.add_space(10.0);
            
            egui::Frame::none()
                .fill(egui::Color32::from_rgb(255, 240, 240))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(200, 0, 0)))
                .inner_margin(egui::Margin::same(10.0))
                .rounding(egui::Rounding::same(5.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("❌");
                        ui.colored_label(egui::Color32::from_rgb(150, 0, 0), &error_message);
                        
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✖").clicked() {
                                self.clear_messages();
                            }
                        });
                    });
                });
        }
    }
}

impl eframe::App for ContextBuilderApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Process background events
        self.process_events();
        
        // Main UI with better layout
        egui::CentralPanel::default().show(ctx, |ui| {
            // Title bar using RichText for emojis
            ui.vertical_centered(|ui| {
                ui.add_space(10.0);
                // Use RichText with heading style for the main title including emoji
                ui.label(RichText::new("🦀 Context Builder - Rust Edition").heading());
                ui.weak("Generate markdown documentation from your project files");
                ui.add_space(10.0);
            });
            
            ui.separator();
            ui.add_space(8.0);
            
            // Main content with proper spacing
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    self.render_directory_selection(ui);
                    self.render_file_tree(ui);
                    self.render_ignore_settings(ui);
                    self.render_output_settings(ui); 
                    self.render_control_buttons(ui);
                    self.render_status_messages(ui);
                    
                    ui.add_space(20.0); // Bottom padding
                });
        });
        
        // Request repaint for animations (spinner, etc.)
        if self.is_loading_directory || self.is_generating_document {
            ctx.request_repaint();
        }
    }
}

/// Canonicalizes a path for comparison purposes, falling back to the input when
/// canonicalization fails (e.g. the file was just deleted). Used to match file
/// watcher events against selected paths that may run through different
/// symlink/junction branches.
fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
