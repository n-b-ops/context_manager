use std::path::{Path, PathBuf};
use std::cmp::Ordering;
use std::fs;
use ignore::{WalkBuilder, DirEntry};
use log::{debug, warn};

use crate::error::{AppError, Result};

#[derive(Debug, Clone)]
pub struct FileNode {
    pub name: String,          // Base name of the file/directory
    pub path: PathBuf,         // Walker path (preserves the symlink branch name)
    pub is_dir: bool,
    pub children: Vec<FileNode>, // Sorted: directories first, then files, then alphabetically case-insensitively
}

// Custom sorting for FileNode: directories first, then files, then by name (case-insensitive)
impl PartialEq for FileNode {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for FileNode {}

impl PartialOrd for FileNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FileNode {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.is_dir && !other.is_dir {
            Ordering::Less
        } else if !self.is_dir && other.is_dir {
            Ordering::Greater
        } else {
            self.name.to_lowercase().cmp(&other.name.to_lowercase())
        }
    }
}

pub struct FileHandler {
    directory: PathBuf,
    follow_symlinks: bool,
}

impl FileHandler {
    /// Creates a handler with default options (symlinks followed).
    #[allow(dead_code)]
    pub fn new(directory: PathBuf) -> Result<Self> {
        Self::with_options(directory, true)
    }

    /// Creates a handler that optionally follows symbolic links (and junctions)
    /// encountered during the scan. When disabled, linked directories appear as
    /// plain file entries without children, matching the previous behavior.
    pub fn with_options(directory: PathBuf, follow_symlinks: bool) -> Result<Self> {
        // Validate directory exists and is readable
        if !directory.exists() {
            return Err(AppError::InvalidDirectory(
                format!("Directory does not exist: {:?}", directory)
            ));
        }

        if !directory.is_dir() {
            return Err(AppError::InvalidDirectory(
                format!("Path is not a directory: {:?}", directory)
            ));
        }

        // Test if we can read the directory
        match fs::read_dir(&directory) {
            Ok(_) => {},
            Err(e) => {
                return Err(AppError::PermissionsError {
                    path: directory.clone(),
                    details: format!("Cannot read directory: {}", e),
                });
            }
        }

        Ok(FileHandler {
            directory,
            follow_symlinks,
        })
    }

    pub fn scan_directory(&self, ignore_patterns: Vec<String>) -> Result<FileNode> {
        debug!("Starting directory scan for: {:?}", self.directory);
        
        let mut builder = WalkBuilder::new(&self.directory);
        
        // Configure the walker according to the plan
        builder
            .standard_filters(true)  // respects global gitignore, .git/info/exclude
            .git_global(true)
            .git_ignore(true)
            .git_exclude(true)
            .hidden(false)          // initially include hidden files, let ignore patterns filter them
            .follow_links(self.follow_symlinks);

        // Add additional ignore patterns
        let mut overrides_builder = ignore::overrides::OverrideBuilder::new(&self.directory);
        for pattern_to_ignore in ignore_patterns {
            let blacklist_pattern = format!("!{}", pattern_to_ignore);
            if let Err(e) = overrides_builder.add(&blacklist_pattern) {
                warn!("Failed to add ignore pattern '{}' as blacklist override '{}': {}", pattern_to_ignore, blacklist_pattern, e);
            }
        }
        
        let overrides = overrides_builder.build()
            .map_err(|e| AppError::IgnoreBuild(e))?;
        builder.overrides(overrides);

        let walker = builder.build();
        
        // Build the tree structure
        let root_node = self.build_file_tree(walker)?;
        
        debug!("Directory scan completed");
        Ok(root_node)
    }

    fn build_file_tree(&self, walker: ignore::Walk) -> Result<FileNode> {
        let mut path_to_node: std::collections::HashMap<PathBuf, FileNode> = std::collections::HashMap::new();
        let mut parent_child_map: std::collections::HashMap<PathBuf, Vec<PathBuf>> = std::collections::HashMap::new();

        let mut total_entries = 0;
        let mut processed_entries = 0;

        // First pass: collect all entries and build node relationships
        for result in walker {
            total_entries += 1;
            match result {
                Ok(entry) => {
                    debug!("Processing entry: {:?}", entry.path());
                    if let Err(e) = self.process_dir_entry(entry, &mut path_to_node, &mut parent_child_map) {
                        warn!("Error processing directory entry: {}", e);
                    } else {
                        processed_entries += 1;
                    }
                }
                Err(e) => {
                    warn!("Error walking directory: {}", e);
                }
            }
        }

        debug!("Processed {} out of {} entries", processed_entries, total_entries);
        debug!("Created {} nodes", path_to_node.len());

        // Second pass: build the tree structure.
        // The walker's depth-0 entry is exactly `self.directory` as given, so using it
        // directly (instead of a canonicalized variant) keeps node identity consistent
        // with the per-entry paths below.
        let root_path = self.directory.clone();

        self.build_tree_recursive(&root_path, &mut path_to_node, &parent_child_map)
    }

    fn process_dir_entry(
        &self,
        entry: DirEntry,
        path_to_node: &mut std::collections::HashMap<PathBuf, FileNode>,
        parent_child_map: &mut std::collections::HashMap<PathBuf, Vec<PathBuf>>,
    ) -> Result<()> {
        let path = entry.path();
        // When following links, resolve directory-ness through the link: the raw
        // file type of a linked directory is the link itself, not a directory.
        // When not following, keep the walker's raw type so links behave like the
        // legacy file-like nodes.
        let is_dir = if self.follow_symlinks
            && entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false)
        {
            fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
        } else {
            entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
        };

        // Node identity is the walker path, NOT a canonicalized one: canonicalizing
        // resolves symlinks, which would collapse `a_link/x` and `real/x` onto the
        // same key and silently corrupt the tree once links are followed. Walker
        // paths preserve the branch the scan actually took.
        let node_path = path.to_path_buf();

        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => {
                // This is likely the root directory
                match path.file_name() {
                    Some(name) => name.to_string_lossy().to_string(),
                    None => self.directory.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "root".to_string()),
                }
            }
        };

        let node = FileNode {
            name,
            path: node_path.clone(),
            is_dir,
            children: Vec::new(),
        };

        path_to_node.insert(node_path.clone(), node);

        // Track parent-child relationships
        if let Some(parent_path) = node_path.parent() {
            parent_child_map
                .entry(parent_path.to_path_buf())
                .or_insert_with(Vec::new)
                .push(node_path);
        }

        Ok(())
    }

    fn build_tree_recursive(
        &self,
        current_path: &Path,
        path_to_node: &mut std::collections::HashMap<PathBuf, FileNode>,
        parent_child_map: &std::collections::HashMap<PathBuf, Vec<PathBuf>>,
    ) -> Result<FileNode> {
        let mut node = path_to_node.remove(current_path)
            .ok_or_else(|| AppError::PathNotFound(current_path.to_path_buf()))?;

        if let Some(children_paths) = parent_child_map.get(current_path) {
            let mut children = Vec::new();
            for child_path in children_paths {
                if let Ok(child_node) = self.build_tree_recursive(child_path, path_to_node, parent_child_map) {
                    children.push(child_node);
                }
            }
            
            // Sort children according to FileNode's Ord implementation
            children.sort();
            node.children = children;
        }

        Ok(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Test fixture: a temp directory with a real subtree and a link to it.
    /// Returns (root, real_dir, link_dir) or None when the platform denies
    /// creation of directory links (e.g. Windows without Developer Mode/admin).
    fn fixture_with_dir_link() -> Option<(PathBuf, PathBuf, PathBuf)> {
        let root = std::env::temp_dir().join(format!(
            "cb_fixture_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let real = root.join("real");
        let link = root.join("real_link");
        fs::create_dir_all(&real).ok()?;
        fs::write(real.join("main.rs"), "fn main() {}").ok()?;

        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(&real, &link)
            .or_else(|_| {
                // Fall back to a junction, which requires no special privileges.
                junction(&real, &link)
            })
            .is_ok();

        if created {
            Some((root, real, link))
        } else {
            let _ = fs::remove_dir_all(&root);
            None
        }
    }

    #[cfg(windows)]
    fn junction(target: &Path, link: &Path) -> std::io::Result<()> {
        // Junctions need no special privileges on Windows, unlike symlinks;
        // `mklink /J` is the standard way to create one without extra crates.
        let output = std::process::Command::new("cmd")
            .args([
                "/c", "mklink", "/J",
                &link.display().to_string(),
                &target.display().to_string(),
            ])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "mklink /J failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    fn scan(root: &Path, follow: bool) -> FileNode {
        FileHandler::with_options(root.to_path_buf(), follow)
            .expect("handler")
            .scan_directory(vec![])
            .expect("scan")
    }

    fn find_child<'a>(node: &'a FileNode, name: &str) -> Option<&'a FileNode> {
        node.children.iter().find(|c| c.name == name)
    }

    /// End-to-end check against a user-provided fixture: set CB_E2E_ROOT to a
    /// directory that already contains links (e.g. created by hand with mklink)
    /// and this scan asserts every junction/symlink child is followed.
    #[test]
    fn e2e_env_fixture_links_are_followed() {
        let Ok(root) = std::env::var("CB_E2E_ROOT") else {
            eprintln!("skipping: CB_E2E_ROOT not set");
            return;
        };
        let root = PathBuf::from(root);
        let tree = scan(&root, true);
        let mut followed = 0;
        for child in &tree.children {
            if child.is_dir && child.children.is_empty() {
                // empty dirs are legal; only flag when the target has content
                let target_has_content = fs::read_dir(&child.path)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(false);
                if target_has_content {
                    panic!(
                        "directory {} scanned empty but its target has content",
                        child.path.display()
                    );
                }
            } else if child.is_dir {
                followed += 1;
            }
        }
        assert!(followed > 0, "expected at least one followed directory branch");
    }

    #[test]
    fn follows_dir_links_and_keeps_both_branches() {
        let Some((root, real, link)) = fixture_with_dir_link() else {
            eprintln!("skipping: no permission to create directory links");
            return;
        };
        let tree = scan(&root, true);

        let link_node = find_child(&tree, "real_link").expect("link node present");
        assert!(link_node.is_dir, "followed link must scan as a directory");
        assert!(
            find_child(link_node, "main.rs").is_some(),
            "contents behind the link must be scanned"
        );

        let real_node = find_child(&tree, "real").expect("real node present");
        assert!(
            find_child(real_node, "main.rs").is_some(),
            "real branch must keep its children (no canonical collision)"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = (&real, &link);
    }

    #[test]
    fn no_follow_keeps_link_as_file_like_node() {
        let Some((root, _real, _link)) = fixture_with_dir_link() else {
            eprintln!("skipping: no permission to create directory links");
            return;
        };
        let tree = scan(&root, false);

        let link_node = find_child(&tree, "real_link").expect("link node present");
        assert!(!link_node.is_dir, "unfollowed link stays a file-like node");
        assert!(link_node.children.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cycle_through_link_does_not_hang_or_corrupt() {
        let Some((root, _real, _link)) = fixture_with_dir_link() else {
            eprintln!("skipping: no permission to create directory links");
            return;
        };
        // real/loop -> root : reachable cycle
        #[cfg(unix)]
        let ok = std::os::unix::fs::symlink(&root, root.join("real").join("loop")).is_ok();
        #[cfg(windows)]
        let ok = junction(&root, &root.join("real").join("loop")).is_ok();
        if !ok {
            eprintln!("skipping: could not create cycle link");
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let tree = scan(&root, true); // must terminate
        let real_node = find_child(&tree, "real").expect("real branch survives");
        assert!(find_child(real_node, "main.rs").is_some());
        let _ = fs::remove_dir_all(&root);
    }
}
 