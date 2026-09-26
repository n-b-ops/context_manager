use std::time::Duration;

pub const MARKDOWN_HEADER_CONTEXT: &str = "# Context";
pub const MARKDOWN_HEADER_STRUCTURE: &str = "## Project Structure";
pub const MARKDOWN_HEADER_FILES: &str = "## Files";

pub const DEBOUNCE_DURATION: Duration = Duration::from_millis(750); // Slightly longer debounce
pub const UI_STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5); 

// Output Formats
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputFormat {
    Markdown,
    Adoc,
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Markdown => "md",
            OutputFormat::Adoc => "adoc",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            OutputFormat::Markdown => "Markdown",
            OutputFormat::Adoc => "AsciiDoc",
        }
    }
}

pub const DEFAULT_OUTPUT_FILENAME_BASE: &str = "project_structure"; // Use base name
pub const DEFAULT_OUTPUT_FORMAT: OutputFormat = OutputFormat::Markdown; // Default format

/// Initial default ignore patterns, shared by the GUI (app.rs) and the CLI
/// (cli.rs) so both frontends filter the same way out of the box.
pub const DEFAULT_IGNORE_PATTERNS_ARRAY: &[&str] = &[
    // Common VCS and build artifacts
    ".git/", ".hg/", ".svn/",
    "target/", "build/", "dist/", "pkg/", "node_modules/",
    // Python specific
    "__pycache__/", "*.pyc", "*.pyo", "*.pyd",
    ".env", ".venv", "venv/", "env/",
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

// AsciiDoc specific constants
pub const ADOC_SECTION_LEVEL_1: &str = "=";
pub const ADOC_SECTION_LEVEL_2: &str = "==";
pub const ADOC_SECTION_LEVEL_3: &str = "==="; // Corrected from "===" to "====" for file sections
pub const ADOC_SOURCE_BLOCK_DELIMITER: &str = "----"; // Typically four hyphens 