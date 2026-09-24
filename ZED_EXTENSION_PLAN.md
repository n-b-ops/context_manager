# Turning Context Manager into a Zed Extension — Investigation & Plan

Investigated against: **Zed 1.19.2** (installed locally), `zed_extension_api` **0.7.0**, `rmcp` **3.3.0**, Zed docs @ `main` (Sept 2026).

---

## 1. Executive summary

Zed extensions **cannot host a GUI** (no custom panels, no egui) and **extension slash
commands were removed** from Zed entirely. The one supported way to add capabilities like
"generate a project context bundle" into Zed is an **MCP (Model Context Protocol) server**,
surfaced in Zed's Agent Panel either as:

1. a **Zed extension** that downloads/launches a native MCP server binary (recommended), or
2. a plain **custom server entry in `settings.json`** (zero-extension fallback), or
3. an entry in the **official MCP registry** (where Zed says extensions are heading).

The good news: Context Manager's core (`file_handler.rs`, `document_generator.rs`,
`constants.rs`, `error.rs`) is already cleanly separated from the egui UI and can be
reused almost verbatim as a library behind a new headless MCP-server binary. The GUI app
remains valuable for standalone use; the extension covers the in-editor/AI workflow.

**Recommended architecture — a Cargo workspace:**

```
context_manager/
├── Cargo.toml                 # [workspace] members = crates/*
├── crates/
│   ├── core/                  # lib: FileHandler, DocumentGenerator, AppError  (extracted from current src/)
│   ├── gui/                   # bin: current egui app (unchanged behavior)
│   └── mcp/                   # bin: context-manager-mcp — stdio MCP server (rmcp)
└── zed-extension/             # separate crate, compiled to wasm32-wasip2
    ├── extension.toml
    ├── Cargo.toml
    ├── src/lib.rs             # downloads server binary from GitHub Releases, returns Command
    └── configuration/         # optional settings UI files
```

---

## 2. What the research found (Zed's extension surface today)

### 2.1 Extension slash commands — REMOVED

From `docs/src/extensions/slash-commands.md`:

> Extension-provided slash commands have been removed from Zed.
> To extend the Agent Panel with custom tools and context, use MCP Servers instead.

The `run_slash_command` method still exists on the `Extension` trait but is dead surface.
A `/context`-style command is **not** an option anymore.

### 2.2 The supported path: MCP server extensions

An extension declares a context server in `extension.toml`:

```toml
[context_servers.context-manager]
```

and implements `context_server_command` on the `Extension` trait, returning the command
line Zed should spawn. Zed then speaks MCP (JSON-RPC 2.0 over stdio) to that process.

**Zed's MCP feature coverage** (from `docs/src/ai/mcp.md`): only **Tools** and **Prompts**.
No resources, no subscriptions, no roots, no sampling/elicitation.
Zed does honor `notifications/tools/list_changed`.

### 2.3 Deprecation warning (plan for it)

From `docs/src/extensions/mcp-extensions.md`:

> We plan to deprecate MCP server extensions in favor of the official MCP registry.
> To keep your MCP server available in Zed, publish it to the official registry as well.

Implication: build the MCP server so it works **standalone** (any MCP client: Claude Code,
Cursor, Zed manual config) and treat the Zed extension as a distribution convenience.

### 2.4 What the extension (wasm) side can and cannot do

- Extension Rust compiles to `wasm32-wasip2`. `std::env::var` and `cfg()` don't behave
  normally there; use `zed::current_platform()`, `Worktree::shell_env()`, `Worktree::which()`.
- `context_server_command(&mut self, id, project)` receives a `Project` whose **only**
  method is `worktree_ids() -> Vec<u64>` — there is **no way to resolve a worktree root**
  from it. So the extension cannot tell the server "the project is at C:\...\my-project".
  → The server's tools must take an explicit `root_path` argument (the agent knows its
  worktree root and passes it). This is the single most important design constraint.
- Extension capabilities are user-restricted (`granted_extension_capabilities`). Our design
  needs only `download_file` from `github.com` — minimal permission footprint; we do not
  need `process:exec` (that governs in-wasm process spawning, which we avoid).

### 2.5 Canonical distribution pattern (verified from a real extension)

`LoamStudios/zed-mcp-server-github` does exactly what we need:

1. `zed::latest_github_release("owner/repo", GithubReleaseOptions { require_assets: true, pre_release: false })`
2. `zed::current_platform()` → asset name like `context-manager_Windows_x86_64.zip`
3. `zed::download_file(url, version_dir, DownloadedFileType::Zip)` into the extension's
   working directory (relative paths — Zed sets cwd to the extension work dir)
4. cache path in extension state; return `zed::Command { command, args, env }`

---

## 3. Feature mapping: GUI app → MCP server

| Context Manager feature | In the Zed extension world |
|---|---|
| Pick project directory | `root_path` tool argument supplied by the agent (Zed agent knows the worktree root) |
| File-tree scan, .gitignore + custom ignores | **Reuses as-is** → tool `scan_project` returns the tree as text |
| Select files via checkboxes | `files` array argument on `generate_context`; the agent picks files |
| Markdown / AsciiDoc generation | **Reuses as-is** → tool returns the document **as the tool result** (better UX than writing a file: the content lands directly in the agent's context; optionally also `output_path` to save) |
| File monitoring (`notify`) w/ auto-regeneration | **Does not map.** MCP in Zed is request/response only (no resources/subscriptions). Drop it; the agent re-invokes the tool when it wants fresh context. Keep monitoring in the GUI app. |
| Status UI / error toasts | Tool result errors surface inline in the Agent Panel |
| Output format choice | `format: "markdown" \| "adoc"` argument |

---

## 4. Proposed MCP tool set

Three small, composable tools (MCP tool selection by the model works best with few,
well-described tools):

```ts
scan_project(root_path, extra_ignores?: string[])
  → "src/\n  app.rs\n  ..." (tree text, dirs first, gitignore-respected)

generate_context(root_path, files: string[], format?: "markdown" | "adoc",
                 output_path?: string, extra_ignores?: string[])
  → full document text; if output_path given, also written atomically (tempfile rename)

get_file(root_path, path)          // convenience: single file w/ language-tagged fence
```

Descriptions must explicitly say "use the project root as root_path" — the model reads them.

---

## 5. Implementation sketches

### 5.1 Extract the core library (mechanical)

`file_handler.rs`, `document_generator.rs`, `constants.rs`, `error.rs` move to
`crates/core/src/` unchanged; `gui` and `mcp` depend on `context-manager-core`.
The only edits: drop the `crate::`-internal coupling to `app.rs`/`events.rs`, and make
`DocumentGenerator` able to render **to a String** (it already has
`generate_structure_string`/`generate_files_string`; add `render_full_document(...) -> String`
next to the file-writing variant).

### 5.2 MCP server (`crates/mcp`)

```toml
[dependencies]
context-manager-core = { path = "../core" }
rmcp = { version = "3", features = ["server", "transport-io"] }
tokio = { version = "1", features = ["full"] }
schemars = "1"
serde = { version = "1", features = ["derive"] }
```

```rust
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router, ServiceExt};
use rmcp::transport::stdio;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct GenerateContextParams {
    /// Absolute path of the project root to bundle.
    root_path: String,
    /// Relative paths of files to include.
    files: Vec<String>,
    /// Output document format. Default: markdown.
    format: Option<String>,
}

#[derive(Clone)]
struct ContextManager;

#[tool_router(server_handler)]
impl ContextManager {
    #[tool(description = "Generate a project context document (directory tree + file         contents) for the given project root_path and file list.")]
    async fn generate_context(
        &self,
        Parameters(p): Parameters<GenerateContextParams>,
    ) -> Result<String, rmcp::Error> {
        let root = std::path::PathBuf::from(&p.root_path);
        let handler = context_manager_core::FileHandler::new(root.clone())
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let tree = handler.scan_directory(vec![])
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let sel: Vec<_> = p.files.iter().map(|f| root.join(f)).collect();
        let gen = context_manager_core::DocumentGenerator::new(root, sel);
        let fmt = match p.format.as_deref() {
            Some("adoc") => context_manager_core::OutputFormat::Adoc,
            _ => context_manager_core::OutputFormat::Markdown,
        };
        gen.render_full_document(&tree, fmt)           // new String-returning API
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))
    }

    // scan_project, get_file: same pattern
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = ContextManager.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

### 5.3 The extension wrapper (`zed-extension/`, wasm32-wasip2)

`extension.toml`:

```toml
id = "context-manager"
name = "Context Manager"
version = "0.1.0"
schema_version = 1
authors = ["Nikolay Bobovnikov"]
description = "Generate project context documents (tree + file contents) for the Agent Panel"
repository = "https://github.com/<you>/context_manager"

[context_servers.context-manager]
```

`src/lib.rs` — download-once logic per §2.5:

```rust
use std::fs;
use zed_extension_api::{self as zed, Command, ContextServerId, Project, Result};

const REPO: &str = "<you>/context_manager";

struct ContextManagerExtension { cached: Option<String> }

impl zed::Extension for ContextManagerExtension {
    fn new() -> Self { Self { cached: None } }

    fn context_server_command(
        &mut self, _id: &ContextServerId, _project: &Project,
    ) -> Result<Command> {
        let release = zed::latest_github_release(REPO,
            zed::GithubReleaseOptions { require_assets: true, pre_release: false })?;
        let (os, arch) = zed::current_platform();
        let asset = format!("context-manager-mcp_{}_{}{}",
            match os   { zed::Os::Mac => "macos", zed::Os::Linux => "linux",
                         zed::Os::Windows => "windows" },
            match arch { zed::Architecture::Aarch64 => "aarch64",
                         zed::Architecture::X8664 => "x86_64", zed::Architecture::X86 => "i686" },
            match os   { zed::Os::Windows => ".zip", _ => ".tar.gz" });
        // ...find asset, download into versioned dir, cache path (see §2.5)...
        Ok(Command { command: binary_path, args: vec![], env: vec![] })
    }
}
zed::register_extension!(ContextManagerExtension);
```

`Cargo.toml`: `crate-type = ["cdylib"]`, `zed_extension_api = "0.7"`,
plus `schemars`/`serde` only if you add a `context_server_configuration` settings UI
(pattern: `configuration/{installation_instructions.md,default_settings.jsonc}` + JSON
schema — e.g. for default ignore patterns).

### 5.4 Release pipeline (GitHub Actions)

Build `context-manager-mcp` for: `x86_64-pc-windows-msvc` (zip),
`x86_64-unknown-linux-gnu` + `aarch64-unknown-linux-gnu` (tar.gz),
`x86_64-apple-darwin` + `aarch64-apple-darwin` (tar.gz). Attach with the exact asset
names from §5.3 and tag `v*` — `latest_github_release` picks them up automatically.
The headless server binary will be far smaller than the 22 MB GUI exe (no eframe/wgpu).

---

## 6. Install / test / publish

- **Dev install**: Zed → Extensions page → `Install Dev Extension` → select `zed-extension/`.
  Logs: `zed::OpenLog`; run `zed --foreground` from a terminal for verbose output.
- **Zero-extension testing** (works today, before any extension exists): add to
  `settings.json`:
  ```json
  "context_servers": {
    "context-manager": {
      "command": "C:\\path\\to\\context-manager-mcp.exe", "args": [], "env": {}
    }
  }
  ```
  then check the indicator dot in Settings → AI → MCP Servers.
- **Publishing**: push `zed-extension` as its own Git repo/tag, install the `zed` CLI
  extension publisher (`cargo install zed-extension`), `zed extension publish` (requires
  a zed.dev account + API key). Also list the server in the official MCP registry given
  the deprecation note (§2.3).
- **Tool permissions**: users approve tool calls by default (`agent.tool_permissions.default:
  "confirm"`); document a profile snippet that disables built-in `read_file`/`list_directory`
  when they conflict.

---

## 7. Risks / open questions

| Risk | Mitigation |
|---|---|
| MCP-extension deprecation in Zed | Server is generic MCP: also register in the MCP registry + document manual config |
| No workspace root passed to servers (§2.4) | Explicit `root_path` arg; optionally honor `CONTEXT_MANAGER_ROOT` env as default |
| Model picks tools unreliably | Few tools, verbose descriptions, mention server name in prompt; agent-profile snippet |
| `rmcp` targets MCP spec 2026-07-28, Zed implements 2025-11-25 | Protocol version is negotiated at initialize; pin rmcp and test against Zed early |
| Windows path quirks (verbatim \\?\ prefixes) | Already handled in `DocumentGenerator::new` (canonicalize fallback) |
| Binary download blocked by user capability settings | Document the required `download_file` grant; offer manual binary path setting |

---

## 8. Effort estimate

| Step | Estimate |
|---|---|
| Workspace split, extract `core` (no behavior change, GUI still builds) | 0.5–1 day |
| `render_full_document -> String` API + `mcp` crate with 3 tools | 0.5–1 day |
| Test via manual `context_servers` config in Zed | hours |
| Extension wrapper + GitHub Actions release matrix | 0.5 day |
| Polish (settings UI, docs, publish to registry) | 0.5–1 day |

Total: **~2–4 focused days** to a published extension.
```
