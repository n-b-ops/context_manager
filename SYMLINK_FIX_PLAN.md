# Fix: context manager skips docs reachable via symlinks/junctions

> **STATUS: IMPLEMENTED** (see §10 for what shipped). All 7 tests pass, including an
> env-driven e2e check against a real Windows junction fixture.

## 1. Problem statement

Selecting a project directory that reaches documentation through a **symbolic link or
Windows junction** produces a context document that silently omits the linked directory's
contents. Today the linked directory appears in the tree (as a *file*-type node) but its
children are never scanned, never selectable, and selecting the link node itself makes
document generation fail outright.

## 2. Root causes (verified empirically)

Reproduced on this machine with a junction fixture (`project\src_link -> ..\src`) and a
probe binary using the exact same `ignore` walker configuration as
`FileHandler::scan_directory`:

### Cause 1 — walker never descends into links: `src/file_handler.rs:89`

```rust
builder.follow_links(false);   // crucial: do not follow symlinks
```

With `follow_links(false)` the walker yields the junction entry itself but nothing under
it, and the entry's `file_type()` is the *link* (`is_dir() == false`), so the tree shows
`src_link` as a childless file node:

```
depth=1 "project\src_link" [dir=false symlink=true]
```

### Cause 2 — per-entry `canonicalize()` makes the naive fix corrupt the tree: `src/file_handler.rs:166`

`process_dir_entry` keys `path_to_node` by `path.canonicalize()`. Canonicalizing resolves
links, so after enabling `follow_links(true)` the same physical file reached via two names
collapses to one key:

```
depth=2 "project\src_link\main.rs" canon=\\?\...\src\main.rs
depth=2 "project\src\main.rs"       canon=\\?\...\src\main.rs   // same key!
```

The later insert overwrites the earlier node; `src` loses its child and the tree silently
corrupts (duplicate/missing branches). **Any fix that follows links must stop using the
canonical path as node identity.**

### Cause 3 — selecting a link node aborts the whole generation

Because the link scans as a file node, the UI lets the user select it.
`DocumentGenerator::generate_file_string` then does `fs::read` on a directory → error →
`generate_files_string` propagates with `?` → the **entire document** fails, not just
that entry.

## 3. Additional related defect found while tracing

**Partial-update matching is brittle:** `app.rs::handle_file_modified` compares raw
`notify` event paths against selected paths with `Vec::contains`. After the fix,
selected paths preserve the *link* branch name while events arrive under the *real* path,
so live section updates for linked files would never match. Fix: compare canonicalized
paths (best-effort, fallback to raw).

## 4. What already works (no work needed)

- **Cycle/loop protection**: the `ignore` walker detects link cycles itself
  (`File system loop found: ... points to an ancestor`) and continues — verified with
  `project\src\loop -> project`. No hang, no extra code required.
- Ignore rules (`.gitignore`, custom patterns) keep applying to paths reached through
  links, matching `rg --follow` / `git` semantics.

## 5. Proposed fix (recommended option)

**Follow links at scan time, keyed by walker path identity, with a UI toggle.**

### 5.1 `file_handler.rs`

1. Add `follow_symlinks: bool` to `FileHandler` (constructor arg, exposed to app).
2. `builder.follow_links(self.follow_symlinks)` — default **true** (the requested
   behavior), overridable from the UI.
3. **Remove per-entry `canonicalize()`** in `process_dir_entry`; use `entry.path()`
   (the walker path, preserving the link branch name) as both the node path and the map
   key. Parent linkage is then correct by construction
   (`project\src_link\main.rs` → parent `project\src_link`).
4. Root of the tree: use `self.directory` as given (walker's depth-0 entry is exactly
   that path) instead of `self.directory.canonicalize()`.
5. Update the `FileNode.path` doc comment ("full, canonicalized path" → "walker path,
   preserving symlink-branch names").
6. Optional hardening: `builder.max_depth(N)` (e.g. 128) as a cheap guard against
   pathological link chains; loop detection is already handled by the walker.

With links followed, the junction entry correctly reports `dir=true`, children appear,
and Cause 3 disappears (link nodes are directories, so they are never `fs::read`).

### 5.2 `document_generator.rs`

- Stop canonicalizing `directory` in `DocumentGenerator::new`. The invariant changes to:
  *"both the generator's directory and selected file paths are walker paths derived from
  the same root string"* → `strip_prefix` works without resolution.
  Keep a normalization that only strips the Windows verbatim prefix (`\\?\`) if ever
  present — do **not** resolve links.
- Replace the existing explanatory comment (it documents the old canonicalize-based
  invariant and must not survive as-is).
- Keep `atomic_write_document` and section update logic unchanged.

### 5.3 `app.rs`

1. Add a "Follow symlinks" checkbox (next to the ignore-patterns input); toggling
   triggers a rescan with the new flag; persist in UI state only (no config file needed
   for now).
2. `handle_file_modified`: build `HashSet<PathBuf>` of
   `selected.iter().map(canon_or_self)` once per event, then check
   `set.contains(&canon_or_self(&file_path))` (`canon_or_self` = canonicalize, fall back
   to the raw path on error, e.g. file already deleted).

### 5.4 `file_monitor.rs`

No structural change: watching stays recursive on the real base directory; changes made
*through* a link still surface as events on the real path, which the canonical comparison
in 5.3 now matches.

## 6. Alternatives considered

| Option | Verdict |
|---|---|
| **A (recommended): follow at scan + walker-path identity + toggle** | Direct, matches user expectation ("docs reachable via symlinks" simply appear in the tree and doc). Cost: scan may traverse large external trees reached by links — mitigated by ignore rules + toggle. |
| **B: keep `follow_links(false)`, resolve link targets lazily at generation** | Safer scans, but link contents are invisible/unselectable in the tree and the structure section can't reflect them without extra synthesis. More code, worse UX. |
| **C: follow only links whose target stays inside the root** | Excludes exactly the common case that motivated this fix (docs living *outside* the project dir). Rejected. |

## 7. Behavior notes / accepted trade-offs (A)

- **Duplicate content**: selecting both `src/` and `src_link/` yields the same file twice
  under two headings. Deemed acceptable (explicit user choice); a future enhancement can
  dedup by canonical path with a "(`via <link>`)" annotation.
- **Scan scope**: following a link into a huge tree (e.g. a link to `~`) scans it.
  Mitigations: ignore rules still apply, the toggle turns it off, optional depth cap.
- **Broken links**: walker emits an error entry, already skipped with `warn!` — unchanged.
- **Selection state across rescans**: paths are stable strings; existing rescan flow
  rebuilds the tree from scratch anyway.

## 8. Test plan

Automated (unit/integration in `file_handler` + `document_generator`; fixture built in
`std::env::temp_dir()`, junctions via `std::os::windows::fs::symlink_dir` falling back
to skipping on privilege errors):

1. Scan with junction `src_link -> src`: assert `src_link` node `is_dir` and contains
   `main.rs`; assert `src` still contains `main.rs` (no collision/corruption).
2. Cycle `src/loop -> <root>`: scan completes (walker errors skipped), no hang, both
   branches intact.
3. Generate document with only `src_link` branch selected: output contains the file
   section for `src_link\main.rs` and correct tree lines.
4. Generate with a directory-link node selected (Cause 3 regression): succeeds, no
   `fs::read` on a directory.
5. Partial update matching: canonical set contains walker-path selection; a simulated
   event under the real path matches.

Manual: the GUI fixture above — select `docs_link`, generate, confirm `guide.md` and
`api.md` sections appear; toggle "Follow symlinks" off → rescan → link collapses to a
file node again.

## 9. Implementation order & estimate

1. `file_handler.rs` identity change + flag (~1h, includes walker-root change)
2. `document_generator.rs` invariant change + comment (~30min)
3. `app.rs` toggle + canonical matching in `handle_file_modified` (~1h)
4. Tests (5 cases) + fixture helper (~2h)
5. README note + manual GUI pass (~30min)

**Total: ~0.5–1 day.** No new dependencies; `dunce`-style verbatim stripping is 5 lines
of manual code.

---

## 10. What actually shipped

| Plan item | Status |
|---|---|
| §5.1 `FileHandler::with_options(dir, follow_symlinks)` + walker-path identity | ✅ `file_handler.rs` — nodes keyed by `entry.path()`, root = given directory, no canonicalize anywhere |
| §5.1 directory-ness through links | ✅ conditional `fs::metadata` resolution, only when following is enabled (keeps legacy file-like nodes when disabled) |
| §5.2 generator invariant change | ✅ `DocumentGenerator::new` strips the Windows verbatim prefix only (`strip_verbatim_prefix`), never resolves links |
| §5.2 defensive directory skip | ✅ `generate_files_string` skips selected paths that are directories (warn + continue) instead of aborting the document |
| §5.3 UI toggle | ✅ "Follow symlinks/junctions" checkbox in Ignore Patterns panel; toggling triggers rescan |
| §5.3 canonical event matching | ✅ `handle_file_modified` compares `canonicalize_or_self` on both sides |
| §8 tests | ✅ 7 tests: follow-keeps-both-branches, no-follow-legacy, cycle-termination, generate-behind-link (files + structure sections), directory-selection skip, verbatim strip, env-driven e2e (`CB_E2E_ROOT`) |

### Notes from implementation

- **Legacy behavior preserved**: with the toggle off, links scan exactly as before
  (file-like, childless) — verified by `no_follow_keeps_link_as_file_like_node`.
- **Cycle safety** comes free from the `ignore` walker (loop detection), confirmed by
  `cycle_through_link_does_not_hang_or_corrupt` with `real/loop -> root`.
- **Junction fallback in tests**: `std::os::windows::fs::symlink_dir` needs privileges;
  tests fall back to `cmd /c mklink /J` (no privileges required) and skip gracefully
  when neither works.
- Default behavior is now **follow = on**: `FileHandler::new` and the GUI checkbox
  default to true, so docs behind links appear out of the box.
