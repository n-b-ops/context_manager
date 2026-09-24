use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use log::{debug, warn};

use crate::constants::{
    MARKDOWN_HEADER_CONTEXT, MARKDOWN_HEADER_STRUCTURE, MARKDOWN_HEADER_FILES, MARKDOWN_CODE_BLOCK,
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

    pub fn generate_structure_string(&self, root_node: &FileNode, format: OutputFormat) -> Result<String> {
        let mut structure_content = String::new();

        match format {
            OutputFormat::Markdown => {
                structure_content.push_str(&format!("{}\n", MARKDOWN_HEADER_STRUCTURE));
                structure_content.push_str(&format!("{}\n", MARKDOWN_CODE_BLOCK));
                
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
                structure_content.push_str(&format!("{}", MARKDOWN_CODE_BLOCK));
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
                Ok(format!(
                    "### {}\n\n{}{}\n{}\n{}",
                    display_path,
                    MARKDOWN_CODE_BLOCK,
                    extension,
                    content,
                    MARKDOWN_CODE_BLOCK
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
                // Sanitize content to prevent markdown issues
                let sanitized = match format {
                    OutputFormat::Markdown => content.replace("```", r"\`\`\`"),
                    OutputFormat::Adoc => content.replace("----", "\\----"),
                };
                Ok(sanitized.trim().to_string())
            }
            Err(e) => {
                warn!("File {:?} contains non-UTF8 content, using lossy conversion", file_path);
                let bytes = e.into_bytes();
                let content = String::from_utf8_lossy(&bytes);
                let sanitized = match format {
                    OutputFormat::Markdown => content.replace("```", r"\`\`\`"),
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
        file_path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_string()
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

        // Determine the section header based on format
        let section_header_prefix = match format {
            OutputFormat::Markdown => format!("### {}", display_path),
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
            files.contains("### src_link/main.rs"),
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
        assert!(!files.contains("### src/"), "directories must not be rendered as file sections");

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
            files.contains("### README.md"),
            "verbatim-prefixed selection must still resolve relative to the directory; got:\n{}",
            files
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
