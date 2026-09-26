//! End-to-end tests: drive the real CLI entry (`ctxpack::cli::run`) over a
//! mock project that combines tricky markdown content with the full set of
//! symlink cases (followed directory link, file link, cycle through the
//! root). These tests pin the CURRENT doc-tree behavior:
//!
//! - links are followed by default; both the real and the link branch of a
//!   followed directory appear in the document under their own names;
//! - a file link appears under the link's name with the target's content;
//! - cycles are pruned by the walker: no `loop/`-prefixed content leaks;
//! - `--no-follow-symlinks` keeps directory links out of the document while
//!   real branches and readable file links still work;
//! - the AsciiDoc backend escapes `----` delimiters found in file content.

use std::fs;
use std::path::{Path, PathBuf};

use ctxpack::cli;

/// Which of the fixture's links could be created on this platform.
#[derive(Default)]
struct Links {
    dir_link: bool,
    file_link: bool,
    cycle: bool,
}

fn mock_project() -> (PathBuf, Links) {
    let root = std::env::temp_dir().join(format!(
        "ctxpack_e2e_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    // Nested fences: pins dynamic fence sizing end to end.
    fs::write(
        root.join("README.md"),
        "# Mock Project\n\n```rust\nfn nested() {}\n```\n",
    )
    .unwrap();
    // Contains an AsciiDoc source delimiter and stray backtick runs.
    fs::write(
        root.join("docs").join("guide.md"),
        "Guide\n\n```yaml\na: 1\n```\ntext ```inline``` ticks\n----\n",
    )
    .unwrap();
    fs::write(root.join("docs").join("adr.md"), "# ADR\nDecision: follow links.\n").unwrap();
    fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    let mut links = Links::default();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("docs"), root.join("linked_docs")).unwrap();
        std::os::unix::fs::symlink(root.join("README.md"), root.join("linked_file.md")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        links.dir_link = true;
        links.file_link = true;
        links.cycle = true;
    }

    #[cfg(windows)]
    {
        // Junctions need no privileges; file symlinks may require Developer
        // Mode. Each behavior is asserted only when its link exists.
        if junction(&root.join("docs"), &root.join("linked_docs")).is_ok() {
            links.dir_link = true;
        }
        if std::os::windows::fs::symlink_file(root.join("README.md"), root.join("linked_file.md")).is_ok() {
            links.file_link = true;
        }
        if junction(&root, root.join("loop")).is_ok() {
            links.cycle = true;
        }
    }

    (root, links)
}

#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> std::io::Result<()> {
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
        Err(std::io::Error::other("mklink /J failed"))
    }
}

/// Runs the CLI over the mock project and returns the generated document.
/// The output lives inside the scanned root, exercising self-exclusion too.
fn generate(root: &Path, extra: &[&str]) -> String {
    let out = root.join("ctx.md");
    let mut args: Vec<String> = vec![
        root.display().to_string(),
        "-o".into(),
        out.display().to_string(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    cli::run(args).expect("cli run must succeed");
    fs::read_to_string(&out).expect("generated document must be readable")
}

#[test]
fn followed_links_keep_both_branches_in_document() {
    let (root, links) = mock_project();
    let doc = generate(&root, &[]);

    if links.dir_link {
        assert!(doc.contains("### `docs/guide.md`"), "real branch missing:\n{}", doc);
        assert!(
            doc.contains("### `linked_docs/guide.md`"),
            "link branch missing:\n{}",
            doc
        );
        assert!(doc.contains("<!-- file: linked_docs/guide.md -->"));
    }
    if links.file_link {
        assert!(doc.contains("### `README.md`"));
        assert!(
            doc.contains("### `linked_file.md`"),
            "file link must appear under its own name:\n{}",
            doc
        );
    }

    // Verbatim content and dynamic fence sizing survive the whole pipeline.
    assert!(doc.contains("fn main() {}"));
    assert!(
        doc.contains("text ```inline``` ticks"),
        "inline tick runs must stay verbatim"
    );
    assert!(
        doc.contains("````markdown\nGuide"),
        "outer fence must outsize the inner ``` runs"
    );

    if links.cycle {
        // The walker prunes cycles: `loop` mirrors the already-visited
        // root, so no loop/-prefixed sections may leak into the document.
        assert!(!doc.contains("`loop/"), "cycle branch must be pruned:\n{}", doc);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn no_follow_keeps_dir_links_out_but_reads_file_links() {
    let (root, links) = mock_project();
    let doc = generate(&root, &["--no-follow-symlinks"]);

    if links.dir_link {
        assert!(
            !doc.contains("### `linked_docs/guide.md`"),
            "content behind an unfollowed dir link must not appear"
        );
        assert!(
            !doc.contains("### `linked_docs`"),
            "a dir link reads as a directory through the link and is skipped"
        );
    }
    if links.file_link {
        assert!(
            doc.contains("### `linked_file.md`"),
            "a file link stays a readable plain entry:\n{}",
            doc
        );
    }
    assert!(doc.contains("### `docs/guide.md`"), "real branch must be unaffected");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn adoc_backend_escapes_source_delimiters() {
    let (root, _links) = mock_project();
    let out = root.join("ctx.adoc");
    cli::run(vec![
        root.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        "adoc".into(),
    ])
    .expect("cli run must succeed");
    let doc = fs::read_to_string(&out).unwrap();
    assert!(doc.contains("=== docs/guide.md"), "adoc file section header:\n{}", doc);
    assert!(doc.contains("\\----"), "adoc delimiters in content must be escaped");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn include_globs_filter_the_e2e_document() {
    let (root, links) = mock_project();
    let doc = generate(&root, &["--include", "src/**"]);
    assert!(doc.contains("### `src/main.rs`"));
    assert!(!doc.contains("### `README.md`"), "non-matching files must be absent");
    if links.cycle {
        assert!(!doc.contains("`loop/"));
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn output_document_is_never_part_of_the_document() {
    let (root, _links) = mock_project();
    let doc = generate(&root, &[]);
    assert!(!doc.contains("### `ctx.md`"), "the output must not include itself");
    assert!(!doc.contains("<!-- file: ctx.md -->"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_directory_fails_cleanly() {
    assert!(cli::run(vec!["/definitely/not/here".into()]).is_err());
}
