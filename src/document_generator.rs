use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use log::{debug, warn};

use crate::constants::{
    MARKDOWN_HEADER_CONTEXT, MARKDOWN_HEADER_STRUCTURE, MARKDOWN_HEADER_FILES,
    ADOC_SECTION_LEVEL_1, ADOC_SECTION_LEVEL_2, ADOC_SECTION_LEVEL_3, ADOC_SOURCE_BLOCK_DELIMITER,
    OutputFormat
};
use crate::error::{AppError, Result};
use crate::file_handler::FileNode;

pub struct DocumentGenerator {
    directory: PathBuf,
    selected_files: HashSet<PathBuf>,
}

impl DocumentGenerator {
    pub fn new(directory: PathBuf, selected_files: Vec<PathBuf>) -> Self {
        // The generator must NOT resolve paths (canonicalize): doing so collapses
        // symlink/junction branches onto their targets and breaks `strip_prefix` for
        // selections that live on a linked branch. Both the directory and the selected
        // files originate from the same scan and therefore share the same root string;
        // the only normalization applied is stripping Windows' verbatim (`\\?\`)
        // prefix, which never resolves links.
        let directory = strip_verbatim_prefix(directory);

        Self {
            directory,
            selected_files: selected_files
                .into_iter()
                .map(strip_verbatim_prefix)
                .collect(),
        }
    }

    pub fn generate_full_document(&self, root_node: &FileNode, output_path: &Path, format: OutputFormat) -> Result<()> {
        debug!("Generating full document ({:?}) for {} selected files to {:?}", format, self.selected_files.len(), output_path);
        
        let mut content = String::new();
        
        // Context header
        match format {
            OutputFormat::Markdown => content.push_str(&format!("{}\n\n", MARKDOWN_HEADER_CONTEXT)),
            OutputFormat::Adoc => content.push_str(&format!("{} {}\n\n", ADOC_SECTION_LEVEL_1, "Context")),
        }
        
        // Project structure section
        content.push_str(&self.generate_structure_string(root_node, format)?);
        content.push_str("\n\n");
        
        // Files section
        match format {
            OutputFormat::Markdown => content.push_str(&format!("{}\n\n", MARKDOWN_HEADER_FILES)),
            OutputFormat::Adoc => content.push_str(&format!("{} {}\n\n", ADOC_SECTION_LEVEL_2, "Files")),
        }
        content.push_str(&self.generate_files_string(format)?);
        
        self.atomic_write_document(output_path, &content)?;

        Ok(())
    }

    /// Renders the directory tree lines for the current selection — no
    /// section header, no code fence. The CLI `--list` mode prints these
    /// directly; the markdown structure section wraps the same lines in a
    /// fence.
    pub fn generate_tree_lines(&self, root_node: &FileNode) -> Result<String> {
        let mut structure_lines = String::new();
        let mut is_last_child_stack = Vec::new();

        self.build_structure_string_recursive(
            root_node,
            &self.directory,
            &Path::new(""),
            0,
            &mut is_last_child_stack,
            &mut structure_lines,
            OutputFormat::Markdown,
        )?;

        Ok(structure_lines)
    }

    pub fn generate_structure_string(&self, root_node: &FileNode, format: OutputFormat) -> Result<String> {
        let mut structure_content = String::new();

        match format {
            OutputFormat::Markdown => {
                structure_content.push_str(&format!("{}\n", MARKDOWN_HEADER_STRUCTURE));

                let structure_lines = self.generate_tree_lines(root_node)?;

                // Filenames may contain backticks, so size the fence dynamically here
                // too (plain "text" language keeps renderers from guessing).
                let fence_len = markdown_fence_len(&structure_lines);
                let fence = "`".repeat(fence_len);
                structure_content.push_str(&format!("{}text\n", fence));
                structure_content.push_str(&structure_lines);
                structure_content.push_str(&fence);
            },
            OutputFormat::Adoc => {
                structure_content.push_str(&format!("{} {}\n", ADOC_SECTION_LEVEL_2, "Project Structure"));
                structure_content.push_str("[source, text]\n");
                structure_content.push_str(&format!("{}\n", ADOC_SOURCE_BLOCK_DELIMITER));
                
                let mut structure_lines = String::new();
                let mut is_last_child_stack = Vec::new();
                
                self.build_structure_string_recursive(
                    root_node,
                    &self.directory,
                    &Path::new(""),
                    0,
                    &mut is_last_child_stack,
                    &mut structure_lines,
                    format
                )?;
                structure_content.push_str(&structure_lines);
                
                structure_content.push_str(&format!("{}", ADOC_SOURCE_BLOCK_DELIMITER));
            }
        }

        Ok(structure_content)
    }

    pub fn generate_files_string(&self, format: OutputFormat) -> Result<String> {
        let mut content = String::new();
        
        // Sort selected files for consistent output
        let mut sorted_files: Vec<_> = self.selected_files.iter().collect();
        sorted_files.sort();
        
        for (i, file_path) in sorted_files.iter().enumerate() {
            // A directory can end up in the selection when symlinks are disabled and
            // a linked directory scans as a file-like node, or when a selected path
            // was replaced by a directory after scanning. Reading it would fail and
            // abort the whole document, so skip such entries instead.
            if file_path.is_dir() {
                warn!("Skipping selected path that is a directory: {:?}", file_path);
                continue;
            }
            if i > 0 {
                content.push_str("\n\n");
            }
            content.push_str(&self.generate_file_string(file_path, format)?);
        }
        
        Ok(content)
    }

    pub fn generate_file_string(&self, file_path: &Path, format: OutputFormat) -> Result<String> {
        let relative_path = file_path.strip_prefix(&self.directory)
            .map_err(|_| AppError::StripPrefixError {
                prefix: self.directory.clone(),
                path: file_path.to_path_buf(),
            })?;
        
        // Use forward slashes for cross-platform consistency
        let display_path = relative_path.to_string_lossy().replace('\\', "/");
        let extension = self.get_file_extension(file_path);
        let content = self.read_file_content(file_path, format)?;
        
        match format {
            OutputFormat::Markdown => {
                // The fence must be strictly longer than any backtick run inside the
                // content, otherwise an inner fence would close the wrapper early.
                let fence_len = markdown_fence_len(&content);
                let fence = "`".repeat(fence_len);
                let language = fence_language(file_path);
                Ok(format!(
                    "### `{}`\n\n<!-- file: {} -->\n\n{}{}\n{}\n{}",
                    display_path,
                    display_path,
                    fence,
                    language,
                    content,
                    fence
                ))
            },
            OutputFormat::Adoc => {
                Ok(format!(
                    "{} {}\n\n{}[source, {}]\n{}\n{}\n{}",
                    ADOC_SECTION_LEVEL_3,
                    display_path,
                    "",
                    extension,
                    ADOC_SOURCE_BLOCK_DELIMITER,
                    content,
                    ADOC_SOURCE_BLOCK_DELIMITER
                ))
            }
        }
    }

    fn build_structure_string_recursive(
        &self,
        node: &FileNode,
        base_dir_path: &Path,
        _current_relative_path: &Path,
        depth: usize,
        is_last_child_stack: &mut Vec<bool>,
        output: &mut String,
        _format: OutputFormat
    ) -> Result<()> {
        if depth == 0 {
            // Root directory
            let root_name = base_dir_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "root".to_string());

            output.push_str(&format!("{}\n", root_name));
        } else {
            let prefix = self.get_branch_prefix(depth, is_last_child_stack);
            let is_last = is_last_child_stack.last().copied().unwrap_or(false);
            let connector = if is_last { "└── " } else { "├── " };
            
            output.push_str(&format!("{}{}{}", prefix, connector, node.name));
            if node.is_dir {
                output.push('/');
            }
            output.push('\n');
        }

        if node.is_dir {
            // Filter children: only include directories that contain selected files, or selected files themselves
            let children_to_render: Vec<&FileNode> = node.children.iter()
                .filter(|child_node| {
                    self.selected_files.contains(&child_node.path) ||
                    (child_node.is_dir && self.directory_contains_selected_file(child_node))
                })
                .collect();

            let num_children_to_render = children_to_render.len();
            for (i, child) in children_to_render.iter().enumerate() {
                is_last_child_stack.push(i == num_children_to_render - 1);
                let child_relative_path = _current_relative_path.join(&child.name);
                self.build_structure_string_recursive(
                    child,
                    base_dir_path,
                    &child_relative_path,
                    depth + 1,
                    is_last_child_stack,
                    output,
                    _format
                )?;
                is_last_child_stack.pop();
            }
        }

        Ok(())
    }

    fn get_branch_prefix(&self, depth: usize, is_last_child_stack: &[bool]) -> String {
        let mut prefix = String::new();
        if depth > 1 {
            // We look at the ancestors, which are at depths 1 to depth-1.
            // The is_last_child_stack has `depth-1` relevant items for a node at `depth`.
            // The loop goes from 0 to depth-2.
            for i in 0..depth.saturating_sub(1) {
                prefix.push_str(if is_last_child_stack.get(i).copied().unwrap_or(false) { 
                    "    " // Ancestor was the last child, so no vertical line.
                } else { 
                    "│   " // Ancestor was not the last child, so add a vertical line.
                });
            }
        }
        prefix
    }

    fn directory_contains_selected_file(&self, dir_node: &FileNode) -> bool {
        if !dir_node.is_dir {
            return false;
        }
        
        for child in &dir_node.children {
            if self.selected_files.contains(&child.path) ||
               (child.is_dir && self.directory_contains_selected_file(child)) {
                return true;
            }
        }
        false
    }

    fn read_file_content(&self, file_path: &Path, format: OutputFormat) -> Result<String> {
        let bytes = fs::read(file_path)
            .map_err(|e| AppError::new_io_error(
                e,
                Some(file_path.to_path_buf()),
                "Failed to read file".to_string(),
            ))?;

        match String::from_utf8(bytes) {
            Ok(content) => {
                // Markdown output preserves the file byte-for-byte: fence safety comes
                // from choosing a long enough outer fence (see `markdown_fence_len`),
                // not from escaping, which would corrupt the content. AsciiDoc source
                // blocks cannot be length-escaped, so the delimiter is escaped there.
                let sanitized = match format {
                    OutputFormat::Markdown => content.to_string(),
                    OutputFormat::Adoc => content.replace("----", "\\----"),
                };
                Ok(sanitized.trim().to_string())
            }
            Err(e) => {
                warn!("File {:?} contains non-UTF8 content, using lossy conversion", file_path);
                let bytes = e.into_bytes();
                let content = String::from_utf8_lossy(&bytes);
                let sanitized = match format {
                    OutputFormat::Markdown => content.to_string(),
                    OutputFormat::Adoc => content.replace("----", "\\----"),
                };
                Ok(format!(
                    "[WARNING: This file contained non-UTF8 content and was converted with potential data loss]\n\n{}",
                    sanitized.trim()
                ))
            }
        }
    }

    fn get_file_extension(&self, file_path: &Path) -> String {
        // AsciiDoc [source, lang] blocks: reuse the markdown language map so both
        // formats agree and unknown extensions get a safe text fallback.
        fence_language(file_path).to_string()
    }

    pub fn atomic_write_document(&self, output_path: &Path, content: &str) -> Result<()> {
        let parent_dir = output_path.parent().ok_or_else(|| AppError::AtomicWriteError {
            path: output_path.to_path_buf(),
            details: "Could not get parent directory for temp file.".to_string(),
        })?;

        let mut temp_file = NamedTempFile::new_in(parent_dir)
            .map_err(|e| AppError::new_io_error(
                e,
                None,
                "Failed to create temp file for atomic write.".to_string(),
            ))?;

        temp_file.write_all(content.as_bytes())
            .map_err(|e| AppError::new_io_error(
                e,
                Some(temp_file.path().to_path_buf()),
                "Failed to write to temp file.".to_string(),
            ))?;

        temp_file.persist(output_path)
            .map_err(|e| AppError::AtomicWriteError {
                path: output_path.to_path_buf(),
                details: format!("Failed to persist temp file to target path: {}", e.error),
            })?;

        debug!("Successfully wrote document to {:?}", output_path);
        Ok(())
    }

    pub fn update_file_section_in_document(
        &self,
        document_path: &Path,
        updated_file_path: &Path,
        format: OutputFormat
    ) -> Result<()> {
        debug!("Updating document section ({:?}) for file: {:?}", format, updated_file_path);
        
        // Read current document content
        let current_content = fs::read_to_string(document_path)
            .map_err(|e| AppError::new_io_error(
                e,
                Some(document_path.to_path_buf()),
                "Failed to read existing document file".to_string(),
            ))?;

        let relative_path = updated_file_path.strip_prefix(&self.directory)
            .map_err(|_| AppError::StripPrefixError {
                prefix: self.directory.clone(),
                path: updated_file_path.to_path_buf(),
            })?;

        let display_path = relative_path.to_string_lossy().replace('\\', "/");

        // Determine the section header based on format. Must stay in sync with
        // the headings produced by `generate_file_string`.
        let section_header_prefix = match format {
            OutputFormat::Markdown => format!("### `{}`", display_path),
            OutputFormat::Adoc => format!("{} {}", ADOC_SECTION_LEVEL_3, display_path),
        };

        // Find the section to replace
        if let Some(start_index) = current_content.find(&section_header_prefix) {
            // Find the end of this section (next header of same or higher level, or end of file)
            let search_start = start_index + section_header_prefix.len();
            let end_index = current_content[search_start..]
                .find("\n### ")
                .or_else(|| {
                    if format == OutputFormat::Adoc {
                        current_content[search_start..].find(&format!("\n{} ", ADOC_SECTION_LEVEL_3))
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    if format == OutputFormat::Adoc {
                        current_content[search_start..].find(&format!("\n{} ", ADOC_SECTION_LEVEL_2))
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    if format == OutputFormat::Adoc {
                        current_content[search_start..].find(&format!("\n{} ", ADOC_SECTION_LEVEL_1))
                    } else {
                        None
                    }
                })
                .map(|pos| search_start + pos)
                .unwrap_or(current_content.len());

            // Generate new section for this file
            let new_section = self.generate_file_string(updated_file_path, format)?;
            
            // Replace the section
            let updated_content = format!(
                "{}{}{}",
                &current_content[..start_index],
                new_section,
                &current_content[end_index..]
            );
            
            self.atomic_write_document(document_path, &updated_content)?;
            debug!("Successfully updated document section for: {}", display_path);
        } else {
            warn!("Could not find section for file {} in document", display_path);
            // This might happen if the document was edited manually and the header changed.
            // Re-generating the full document might be a fallback, or just log a warning.
            return Err(AppError::DocumentGenerationError(
                format!("Could not find section for file {} in document. Consider regenerating the full document.", display_path)
            ));
        }

        Ok(())
    }
}

/// Maps a file extension to a fence language identifier for markdown code
/// blocks. Unknown extensions fall back to `text` so no file is ever
/// mislabelled as a language it is not.
fn fence_language(file_path: &Path) -> &'static str {
    match file_path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("md" | "markdown") => "markdown",
        Some("py" | "pyi") => "python",
        Some("rs") => "rust",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("json") => "json",
        Some("sh" | "bash") => "bash",
        Some("ps1") => "powershell",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts" | "tsx" | "mts" | "cts") => "typescript",
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "hpp" | "hh") => "cpp",
        Some("cs") => "csharp",
        Some("go") => "go",
        Some("java") => "java",
        Some("kt" | "kts") => "kotlin",
        Some("rb") => "ruby",
        Some("php") => "php",
        Some("swift") => "swift",
        Some("sql") => "sql",
        Some("html" | "htm") => "html",
        Some("css") => "css",
        Some("xml") => "xml",
        Some("ini" | "cfg" | "conf") => "ini",
        Some("proto") => "protobuf",
        Some("dockerfile") => "dockerfile",
        _ => "text",
    }
}

/// Computes the backtick fence length needed to safely wrap `content` in a
/// fenced code block: one more than the longest backtick run anywhere in the
/// content (three at minimum, per CommonMark). Scanning anywhere — not just
/// line starts — is deliberately conservative: it also protects lax parsers
/// and plain substring consumers (e.g. LLM prompt splitters) that do not
/// honour the line-start rule. Tildes cannot close a backtick fence, so only
/// backtick runs matter.
fn markdown_fence_len(content: &str) -> usize {
    let max_run = content
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    max_run.max(3 - 1) + 1
}

/// Strips Windows' extended-length ("verbatim") prefix from a path without
/// resolving symlinks. No-op on paths without the prefix and on non-Windows
/// platforms where the prefix cannot occur.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        if let Some(parsed) = Path::new(stripped).to_str() {
            return PathBuf::from(parsed);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cb_gen_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "# Project").unwrap();
        root
    }

    fn scan(root: &Path) -> crate::file_handler::FileNode {
        crate::file_handler::FileHandler::with_options(root.to_path_buf(), true)
            .unwrap()
            .scan_directory(vec![])
            .unwrap()
    }

    #[test]
    fn generates_document_for_selection_behind_dir_link() {
        let root = write_fixture();
        let link = root.join("src_link");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(root.join("src"), &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(root.join("src"), &link)
            .map(|_| ())
            .or_else(|_| {
                let output = std::process::Command::new("cmd")
                    .args([
                        "/c", "mklink", "/J",
                        &link.display().to_string(),
                        &root.join("src").display().to_string(),
                    ])
                    .output()
                    .map(|o| o.status.success())?;
                if output { Ok(()) } else { Err(std::io::Error::other("mklink failed")) }
            })
            .is_ok();
        if !linked {
            eprintln!("skipping: no permission to create directory links");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let tree = scan(&root);
        let generator = DocumentGenerator::new(
            root.clone(),
            vec![link.join("main.rs")],
        );

        let files = generator.generate_files_string(OutputFormat::Markdown).unwrap();
        assert!(
            files.contains("### `src_link/main.rs`"),
            "file behind the link must be rendered under the link-relative path; got:\n{}",
            files
        );

        let structure = generator.generate_structure_string(&tree, OutputFormat::Markdown).unwrap();
        assert!(
            structure.contains("src_link/"),
            "structure section must show the followed link directory; got:\n{}",
            structure
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_directory_selections_instead_of_failing() {
        let root = write_fixture();
        let generator = DocumentGenerator::new(
            root.clone(),
            vec![root.join("src")], // a directory, not a file
        );

        let files = generator.generate_files_string(OutputFormat::Markdown).unwrap();
        assert!(!files.contains("### `src/`"), "directories must not be rendered as file sections");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_fences_get_longer_outer_fence_and_content_stays_verbatim() {
        let root = write_fixture();
        let nested = root.join("docs");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("adr.md"),
            "# ADR\n```yaml\nprovisioning:\n  type: object\n```\n## Configuration\n```python\nx = 1\n```",
        ).unwrap();

        let generator = DocumentGenerator::new(root.clone(), vec![nested.join("adr.md")]);
        let files = generator.generate_files_string(OutputFormat::Markdown).unwrap();

        // Content must appear byte-for-byte: no escape sequences introduced
        assert!(files.contains("```yaml\nprovisioning:"), "inner fence must survive unescaped");
        assert!(!files.contains("\\`"), "no backslash-escaped backticks allowed");
        // Outer fence is longer than any inner run (3) and uses the markdown tag
        assert!(files.contains("\n````markdown\n"), "outer fence must be 4+ backticks");
        assert!(files.ends_with("````"), "outer fence must close the section");
        // File-boundary marker for grep-based extraction survives rendering
        assert!(files.contains("<!-- file: docs/adr.md -->"), "marker must precede each file block");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fence_language_maps_extensions_and_falls_back_to_text() {
        let dir = Path::new("x");
        assert_eq!(fence_language(&dir.join("a.md")), "markdown");
        assert_eq!(fence_language(&dir.join("a.py")), "python");
        assert_eq!(fence_language(&dir.join("a.RS")), "rust", "case-insensitive");
        assert_eq!(fence_language(&dir.join("a.yaml")), "yaml");
        assert_eq!(fence_language(&dir.join("a.robot")), "text", "unknown ext falls back");
        assert_eq!(fence_language(&dir.join("noext")), "text", "no extension falls back");
    }

    #[test]
    fn python_file_gets_python_tag_not_markdown() {
        let root = write_fixture();
        let generator = DocumentGenerator::new(root.clone(), vec![root.join("src").join("main.rs")]);
        let files = generator.generate_files_string(OutputFormat::Markdown).unwrap();
        assert!(
            files.contains("\n```rust\nfn main() {}"),
            "rust files must be tagged rust, not markdown; got:\n{}",
            files
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fence_length_scales_with_longest_backtick_run() {
        assert_eq!(markdown_fence_len("plain text"), 3);
        assert_eq!(markdown_fence_len("```yaml\na: 1\n```"), 4);
        assert_eq!(markdown_fence_len("`````md\nx\n`````"), 6);
        assert_eq!(markdown_fence_len("  \t````rust\nfn x() {}\n````"), 5);
        // Runs anywhere count, even mid-line (conservative for lax consumers)
        assert_eq!(markdown_fence_len("text ```inline``` text"), 4);
        // Tildes cannot close a backtick fence
        assert_eq!(markdown_fence_len("~~~\nnot a closer\n~~~"), 3);
    }

    #[test]
    fn structure_block_uses_dynamic_fence_too() {
        let root = write_fixture();
        let tricky = root.join("we```ird.md");
        std::fs::write(&tricky, "hello").unwrap();
        let tree = crate::file_handler::FileHandler::with_options(root.clone(), true)
            .unwrap()
            .scan_directory(vec![])
            .unwrap();
        let generator = DocumentGenerator::new(root.clone(), vec![tricky]);
        let structure = generator
            .generate_structure_string(&tree, OutputFormat::Markdown)
            .unwrap();
        assert!(
            structure.contains("\n````text\n"),
            "structure fence must outsize the backticks in filenames; got:\n{}",
            structure
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Full-pipeline check: generate a document containing nested fences and
    /// nested-backtick filenames, then parse it with pulldown-cmark to prove
    /// the structure holds: one code block per file, no content leakage into
    /// the document outline.
    /// Renders a fixture with several languages through the real pipeline;
    /// CB_LANG_ROOT drives it for manual inspection of the final document.
    #[test]
    fn dump_language_document_for_review() {
        let Ok(root) = std::env::var("CB_LANG_ROOT") else {
            eprintln!("skipping: CB_LANG_ROOT not set");
            return;
        };
        let root = PathBuf::from(root);
        let tree = crate::file_handler::FileHandler::with_options(root.clone(), true)
            .unwrap()
            .scan_directory(vec![])
            .unwrap();
        let mut selected = Vec::new();
        collect_files(&tree, &mut selected);
        let generator = DocumentGenerator::new(root.clone(), selected);
        println!("{}", generator.generate_files_string(OutputFormat::Markdown).unwrap());
    }

    fn collect_files(node: &crate::file_handler::FileNode, out: &mut Vec<PathBuf>) {
        if !node.is_dir {
            out.push(node.path.clone());
        }
        for child in &node.children {
            collect_files(child, out);
        }
    }

    #[test]
    fn full_document_parses_with_commonmark_and_keeps_files_contained() {
        use pulldown_cmark::{Parser, Event, Tag};

        let root = write_fixture();
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("adr.md"),
            "# ADR\n```yaml\nprovisioning:\n  type: object\n```\n## Configuration\ntext ```inline``` ticks",
        ).unwrap();
        std::fs::write(root.join("we```ird.md"), "odd name").unwrap();
        let tree = crate::file_handler::FileHandler::with_options(root.clone(), true)
            .unwrap()
            .scan_directory(vec![])
            .unwrap();
        let generator = DocumentGenerator::new(
            root.clone(),
            vec![root.join("README.md"), docs.join("adr.md"), root.join("we```ird.md")],
        );
        let doc = generator
            .generate_structure_string(&tree, OutputFormat::Markdown)
            .unwrap()
            + "\n\n"
            + &generator.generate_files_string(OutputFormat::Markdown).unwrap();

        let mut code_blocks = 0usize;
        let mut headings = Vec::new();
        let mut in_code = false;
        for event in Parser::new(&doc) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => { in_code = true; code_blocks += 1; }
                Event::End(Tag::CodeBlock(_)) => in_code = false,
                Event::Start(Tag::Heading(level, _, _)) => {
                    assert!(!in_code, "heading opened while inside a code block");
                    headings.push(level as u8);
                }
                Event::Text(t) if !in_code => {
                    assert!(
                        !t.contains("provisioning:"),
                        "file content leaked outside code blocks: {}", t
                    );
                }
                _ => {}
            }
        }
        // 1 structure block + 3 file blocks
        assert_eq!(code_blocks, 4, "exactly one fenced block per file plus structure");
        assert_eq!(
            headings.iter().filter(|&&l| l == 3).count(),
            3,
            "three file-path headings at level 3; got {:?}",
            headings
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verbatim_prefix_is_stripped_not_resolved() {
        let root = write_fixture();
        // Simulate a verbatim-prefixed selection (as canonicalize would produce)
        // without any real link: strip_verbatim_prefix must normalize it away.
        let verbatim = if cfg!(windows) {
            PathBuf::from(format!("\\\\?\\{}", root.join("README.md").display()))
        } else {
            root.join("README.md")
        };
        let generator = DocumentGenerator::new(root.clone(), vec![verbatim]);
        let files = generator.generate_files_string(OutputFormat::Markdown).unwrap();
        assert!(
            files.contains("### `README.md`"),
            "verbatim-prefixed selection must still resolve relative to the directory; got:\n{}",
            files
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
