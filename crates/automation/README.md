# pdfcraft-automation

Agent control for PdfCraft: a headless tool table over the engine, and an opt-in MCP server.

- **Layer:** L7 (architecture §13), but headless. It depends on `engine`, `render` and `organize`, never on a UI toolkit.
- **Status:** over a hundred tools; `pdfcraft-cli tools` lists the current set with their schemas (the count isn't repeated here, so it can't go stale). They cover inspection, rendering, text, page edits and organizing, page labels, bookmarks, comments and their review, form filling and authoring (fields, properties, scripts, data exchange, detection), redaction and sanitizing, password protection and signatures, calibrated 2D measurements and CSV export, adding text, images, links and stamps, optimizing, OCR, printing, export, metadata, undo/redo, save, combine, extract and split. Comment tools take geometry in the same top-left-origin points as everything else, and `comment_add` can mark text by searching for it (`find`). The MCP server also serves the open documents as resources, page images included. The running app's UI is driven separately, through the `ui.*` control channel in `pdfcraft-ui-egui`.

## API

```rust
let mut a = Automation::new().with_root("/work")?;      // optional: confine all paths
let doc = a.call("doc_open", &json!({ "path": "in.pdf" }))?;   // Vec<Content>
a.write_output("page1.png", &png)?;                      // save a result yourself, under the same root rules
tools() -> Vec<ToolDef>                                  // name, title, description, input_schema, read_only, destructive, command
mcp::McpServer::new(a).serve(stdin, stdout)?             // newline-delimited JSON-RPC 2.0
```

`Content` is `Json(Value)` or `Png { data, width, height }`. Errors are `UnknownTool`, `InvalidArgs` (the call didn't match the schema) or `Failed` (a readable message for the agent).

## Conventions

- Tool names are `snake_case` (`page_rotate`): MCP clients reject dots. `ToolDef::command` links a tool to the registry id it automates (`page.rotate`), and `command_list` reports the link.
- Pages and positions are **1-based**. Rectangles are PDF points with the origin at the top-left of the displayed page.
- Unknown arguments are rejected, so typos fail loudly.
- Tools that change a document return its summary (`doc`, `pages`, `dirty`, `undo`, `redo`, …).
- `comment_image_preview` returns JSON and a PNG: `layer: "background"` (the default) gives the page without the selected image signature/initials; `layer: "image"` gives its embedded image with alpha. The metadata supplies its displayed rectangle, document rotation and annotation opacity. Each result can be saved with the CLI's `--out`. Cache the background and transform the image for live movement/resizing; `comment_edit` commits the final geometry in one undo step. Previewing never changes the document.
- For proportional image signature resizing, use the embedded image layer's width/height ratio rather than a previously stretched annotation rectangle. The GUI preserves this ratio at corners with the opposite corner anchored; edge midpoint handles change only their own axis. `comment_edit` accepts the resulting rectangle.
- `doc_close` refuses to drop unsaved changes unless `discard_changes: true`. `doc_save` writes atomically, incrementally in place, and in full for a new path.
- With a root set, every read and write path must resolve inside it (symlinks and `..` included). Relative paths resolve inside the root, and `..` is resolved by name before the check. Every path outside the root gets the same refusal (`<path> is outside the allowed directory <root>`), whether or not it exists, so a confined agent can't probe the rest of the disk; on Windows another network share or device path (`\\host\share`, `\\?\UNC\…`, `\\.\…`) is refused without being contacted. "Not found" and other filesystem errors are reported only for paths inside the root. Tools that write several files into a folder name them after the document or input file, with separators, colons and control characters replaced by `_`, so a name can't lead them out of the folder. The root itself must be a folder.

## MCP server

**Opt-in only.** Nothing starts it automatically, and it opens no port. It runs while `pdfcraft-cli mcp [--root DIR]` runs, normally launched by an agent from its MCP configuration. It stops when stdin closes. The CLI's `mcp` Cargo feature (on by default) compiles it out entirely when disabled.

`pdfcraft-cli mcp --compact` (or `McpServer::with_compact(true)`) keeps `tools/list` short for agents with small context budgets. It lists only the core tools (`mcp::COMPACT_CORE_TOOLS`: open, info, save, close, render, text extract and find, combine, split, undo) plus two meta tools: `tool_search` (optional `query` and `category`, the name prefix; or `name` for one tool's full `input_schema`) and `tool_call` (`name` and `arguments`, run through the same `Automation::call`). `tools/call` by name keeps working for every tool, and the server instructions mention the meta tools. Without the flag nothing changes.

Implemented: `initialize` (protocol 2025-06-18, 2025-03-26, 2024-11-05), `ping`, `tools/list` (with `readOnlyHint`/`destructiveHint` annotations), `tools/call` (JSON results also returned as `structuredContent`; images as `image/png`), and `resources/list`, `resources/templates/list` and `resources/read`. The resources expose the open documents read-only: `pdfcraft://doc/{doc}/info` (JSON), `…/text`, `…/page/{page}/text` and `…/page/{page}/image{?dpi}` (PNG, 1–600 dpi). Tool failures come back as `isError: true` results, so the agent can read and recover from them.

## Adding a tool

1. Add the engine capability first, with its tests (the tool is a thin adapter).
2. Add a `ToolDef` in `src/tools.rs`: a precise description, the schema, `ro()`/`destructive()`, and `cmd()` if a registry command exists.
3. Handle it in `Automation::call`, validating pages and positions with the existing helpers.
4. Add an end-to-end test in `tests/automation.rs`. `tool_table_is_well_formed` checks names, schemas and command links.

## Document tags

`accessibility_tags` inspects the structure tree (including diagnostics and ordered mixed kids).
The create/update/move/delete tools use persistent `{ "number": N, "generation": G }`
identities from that inspection, never transient row numbers. Edits and incremental saves
retain them; full rewrites (including default Save As) can renumber objects. Re-inspect
after full save/reopen before addressing tags again. Document summaries expose
`object_identity_generation`; discard cached identities when it changes (also on
undo, redo and revert). `position` is 1-based among
all kids, including MCID/MCR/OBJR content references; moves count after removing the
source. Omit `parent` for the root. Text properties omitted from an update are preserved,
`null` clears them, and strings set them. `mode: "unwrap"` replaces a tag with its kids;
`mode: "subtree"` removes the tag and descendants and their structure associations.
Neither deletion mode deletes page drawing content. Every mutation is undoable and
returns the document summary and new tree; creation also returns `created`.

This is manual structure editing, not automatic content tagging or PDF/UA certification.
Malformed or ambiguous structures are inspected with diagnostics and conservatively
refused for editing rather than silently repaired.

`accessibility_tag_content` reads one page's marked-content geometry and text without
changing the PDF. Regions use displayed-page points (top-left origin, rotation and crop
applied), and are primitive envelopes rather than exact painted-pixel masks. Match by
page object, MCID and stream identity, not text or reading order. A null stream is the
page content sequence; an object identity is a Form XObject stream. Explicit MCR stream
references without an exact returned match are unavailable, never a reason to fall back
to a page MCID. OBJR annotation regions are declared rectangles; XObject regions are
painted envelopes. Inspect `issues` for incomplete coverage and never invent missing
regions.

`accessibility_tags_find` accepts a 1-based `page` and 1–4096 `regions` in the same
page-view coordinates (`[left, top, right, bottom]`; zero-size point rectangles allowed).
It returns every direct tag owner hit by the selection, plus coverage/structure `issues`
even when no tags match. Full page, stream and object identities are matched exactly.
It never chooses an arbitrary owner for an ambiguous selection.

Advanced tools: `accessibility_role_map_set` edits custom role mappings (null removes);
`accessibility_tag_content_move` reassigns existing content leaves using `source`,
`source_position`, `destination`, and destination `position`; `accessibility_tags_move`
and `accessibility_tags_delete` apply atomic multi-tag operations. `accessibility_tag_identifier_set`
updates IDTree-aware IDs (null clears); external fragment links may break, and unresolved
Headers/structure destinations are rejected. `accessibility_tag_artifact` converts exact
owned marks and removes the tag subtree, with explicit `include_descendants`.
See [advanced structure actions](../../docs/accessibility/tag-editor.md#advanced-structure-actions)
for bounds, identity preservation and supported-tree restrictions.
