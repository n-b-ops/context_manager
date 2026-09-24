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

// AsciiDoc specific constants
pub const ADOC_SECTION_LEVEL_1: &str = "=";
pub const ADOC_SECTION_LEVEL_2: &str = "==";
pub const ADOC_SECTION_LEVEL_3: &str = "==="; // Corrected from "===" to "====" for file sections
pub const ADOC_SOURCE_BLOCK_DELIMITER: &str = "----"; // Typically four hyphens 