# Accessibility tag editor

## Design and verification plan

The PDF object graph remains the source of truth. Structure editing uses the engine's
transactional edit/undo/save machinery; the UI and automation are thin clients. Unknown
PDF keys and content associations must survive edits. Every traversal is bounded and
cyclic/malformed graphs produce an actionable diagnostic rather than a crash.

### Workflow coverage

| Workflow | Implementation target | Verification |
| --- | --- | --- |
| Inspect document-order structure and mixed content children | Stable indirect identities, role mapping, inherited pages, MCID/MCR/OBJR leaves | Synthetic mixed/cyclic/indirect tree tests |
| Expand/collapse, find a tag by type/title, select and navigate | Native docked tree, separate disclosure/selection, accessible labels | Keyboard and pointer UI tests |
| Edit tag type, title, alternative/actual text and language | Buffered properties, Apply/Cancel, atomic engine edit | Save/reopen and undo/redo tests |
| Create a tag, reorder or reparent | Explicit child/sibling positions; cycle prevention; preserve page association | Engine + automation + real UI tests |
| Remove semantic tags | Explicit unwrap/subtree choice; preserve page artwork; repair association indexes | Nested ParentTree and content-preservation tests |
| Agent access | Same engine APIs via strict automation schemas | Automation end-to-end tests |
| Persistence and safety | Snapshot undo, modification permissions, lossless unknown data | Failed edits leave bytes/history unchanged |

### Boundaries requiring additional content-engine work

Tree editing is not automatic tagging or PDF/UA certification. Page selection to newly
created marked content, new annotation tagging, and the separate Reading Order/Content
tools remain distinct content-engine work. Existing text and Form XObject geometry
highlighting is supported within the explicit selection-envelope limits below. A tag's page association alone
is not precise content geometry. Existing figure bounds are not sufficient for general
tag highlighting. Unsupported malformed structure operations must fail atomically.

### Primary behavior reference

Adobe's public [Edit document structure with the Content and Tags panels](https://helpx.adobe.com/acrobat/using/editing-document-structure-content-tags.html)
describes the ordered tree, property fields, sibling/child moves and content association.
PdfCraft uses its existing theme, Lucide assets and native egui controls; no Adobe assets
are included or recreated. Research screenshots remain private and are never shipped.

## Editing behavior

Open **Accessibility tags** from the side rail, View → Side panels, or the accessibility
workspace. A disclosure control expands a tag independently of selection. Search retains
ancestors so matches keep their context. Content children retain their original order;
MCIDs are page/stream-local identities, never inferred from text strings.

Properties opens on demand from the context menu, double-click or F2, keeping the dock's
height available to the tree. The native egui form buffers changes. Apply creates one undo
step; Cancel leaves the PDF untouched. Reordering and deletion are disabled while a property draft is pending. Each
open document retains its own draft, selection, expansion and search state. A full-save
rewrite or undo/redo invalidates mutation drafts and confirmations. Navigation survives
metadata-only history only when the complete ordered structure identities, namespaces,
pages and content bindings still match; geometry is recomputed. Otherwise stale selection
and its status are cleared.

Ctrl/Cmd-click toggles tags; Shift-click or Shift-arrow selects a visible range. Batch
move and deletion are atomic. Use **Move selected tags…** for a precise destination and
position; individual move shortcuts are disabled for a multi-selection.

Use the context menu or drag a tag or existing content leaf. The upper/lower row edges insert before/after;
the row center reparents as a child. The engine rejects cycles and preserves inherited
page associations. Unwrap promotes children; Delete subtree detaches semantic content
associations. Neither removes page artwork or changes that artwork into an artifact.

Arrow keys traverse the visible tree; Left/Right collapse/expand, Home/End go to its
ends, Enter navigates to content, F2 opens properties, Shift+F10 opens the row action
menu (Tab/Enter operate its actions), and Delete opens a named
confirmation. Text editors retain their own keys. Rows publish native tree-item
accessibility semantics and the tree paints only visible rows.

## Content bounds and reverse lookup

The renderer collects marked-content identity while interpreting the same primitives used
for painting, including named property lists and Form XObject namespaces. Highlighting
joins the tag's page, stream generation and MCID, or its OBJR object identity, to those
records. It does not match extracted strings or use guessed font-size rectangles.

Outlines are selection envelopes around painted primitives, not pixel masks. Nearby glyph
bounds within one exact content identity are grouped for display using bounded baseline
and spacing checks; original geometry remains unchanged for picking. Nonrectangular
clipping and other incomplete geometry are disclosed. Select content on page picks a
point; Find tag from selection uses selected glyph rectangles. Ambiguous hits list every
matching owning tag rather than silently picking one. All lookup is also available through
the engine and `accessibility_tags_find` automation tool.

## Explicit limits

- Creating an empty tag does not wrap previously untagged page content.
- Automatic tagging, arbitrary content-to-artifact conversion, table repair, ClassMap
  authoring and new annotation bindings remain separate work.
- PDF 1.7, PDF 2.0 and supported MathML namespace roles are resolved using each element's
  own namespace. Existing RoleMap/RoleMapNS data is preserved; namespace-map authoring is
  not implemented. Unknown or contradictory namespaces remain explicit compatibility limits.
- Tag IDs and validated IDTree deletion are supported. Changes that would break known
  ID references, structure destinations or protected associations are refused atomically.
- Unsupported direct structure dictionaries are reported read-only; malformed graphs are
  never silently normalized. The inspector has traversal and allocation limits.
- This editor does not certify reading order, alt-text quality, WCAG or PDF/UA conformance.

## Advanced structure actions

The Tag options menu opens the role-map editor. Existing content leaves offer **Move
content to…** and drag/reorder controls. Tag properties include abbreviation expansion
and an ID field. **Convert to artifact…** requires an explicit named confirmation and
refuses unsupported content without changing the document.

Role mappings are exposed by `TagTree.role_map` and `accessibility_tags.tree.role_map`.
`SetTagRoleMapping` / `accessibility_role_map_set` sets a custom role's target or removes
it with `target: null`. Changes affect all tags using that role. The whole map must
resolve to standard roles: cycles, excessive chains, overriding standard roles and
removing a mapping still in use are rejected. Existing indirect map identity is retained.

`MoveTagContent` / `accessibility_tag_content_move` moves an existing MCID, MCR or OBJR
leaf between tags; it does not move artwork. Source and destination indexes address all
mixed kids, with destination position measured after removal. The Rust API uses zero-based
indexes and automation uses one-based positions. Integer MCIDs become explicit MCRs when
moved, pinning their original page. Existing MCR/OBJR dictionaries retain unknown keys and
indirect identities; `/Stm` and object generations are preserved exactly. The exact
ParentTree owner is reassigned without shifting slots. Shared content dictionaries and
shared ParentTree value arrays that cannot be reassigned independently are refused.

`MoveTags` / `accessibility_tags_move` moves selected roots in document order in one undo
step. Descendants selected with an ancestor travel with that ancestor. The destination
index is measured after all selected roots are removed. `DeleteTags` /
`accessibility_tags_delete` is atomic: subtree deletion collapses selected descendants;
unwrap processes every explicitly selected tag deepest first. Any unsafe member rejects
the entire operation, including errors discovered after another member was processed.

Moving a page-less tag under a paged parent requires a bounded proof of every changed
inheritance branch. Empty page-less branches and unbound integer/MCR/OBJR content are refused.
Explicit valid page bindings on descendant tags shield their unchanged subtrees;
explicit-page content leaves also keep their exact page, stream and object identities.
Every newly page-bound container must have no class, bounding-box or Ref attributes;
only the already-supported Layout/Placement-only attribute dictionary is accepted.
Incoming structure destinations to a changed container are refused. IDTree validation
is needed for ID-based destinations, not unrelated moves with no such destination.
This exception is unavailable when surplus ParentTree associations are being preserved:
their unreachable owners may inherit through an affected container outside its K graph.
Already-paged moves still pin the original page. No descendant page binding is rewritten.

`SetTagIdentifier` / `accessibility_tag_identifier_set` changes an ID and its IDTree entry
together. Existing byte strings remain unchanged, keys are sorted by raw bytes, and
validated name-tree nodes, indirect arrays and unknown keys survive. Duplicate IDs, stale
or malformed indexes, mismatched tag IDs and aliased storage are rejected. A fresh unrelated
ID can be added without rewriting existing table or structure-destination references.
Renaming/removing an ID remains conservative around `/Headers`, `/ContinuedFrom`, `/Ref`
and `/SD` references. Deletion prunes a validated IDTree while preserving unrelated nodes
and references; a targeted structure destination prevents deletion. Unknown properties on
pruned direct index nodes cannot be silently discarded. External fragment links cannot be
discovered or repaired; changing an ID can break them. New IDs use PDF text-string encoding;
clearing uses `identifier: null`.

`ArtifactTag` / `accessibility_tag_artifact` converts exactly owned marked-content
sequences to `/Artifact` and removes the semantic subtree. Unlike ordinary deletion,
this explicitly changes the content marking while preserving the painting operators.
`include_descendants: false` requires a leaf tag; true includes descendant tags. OBJR,
ambiguous or reused Form invocations, unsupported stream layouts and malformed marks are
rejected. A supported Form must have one proven invocation and exact caller resources;
validated transparent wrappers and selected Span descendants preserve their original
operators and resource identities. Active masks and private aliases
remain conservative refusal boundaries. This operation does not automatically repair reading order or certify PDF/UA.

Evidence: backend alias/namespace/IDTree tests; engine transaction/history/render tests;
automation role-map/batch/content/identifier/artifact tests exercise save, reopen, undo,
redo, negative arguments, and unchanged rendering for supported artifact conversion.

The existing alternate-text “mark decorative” action now uses the same exact, atomic
artifact conversion, after confirming the selected tag is a Figure. It supports validated
Image XObject drawing inside the figure, nested ParentTrees and validated IDTree pruning.
Reference-sensitive figures, figure subtrees requiring broader selection and ambiguous
content are refused without detaching tags or changing streams. This closes the older flat-ParentTree-only removal path.

## Workload limits for large selections

Deletion rejects a batch before its working clone when repeated deletion work exceeds
1,000,000 estimated visits. The estimate includes structure elements, all mixed children,
preserved-association work, and up to five whole-document scans measured across direct
container values as well as indirect objects. Subtree mode counts unique selected roots;
unwrap counts every unique selected tag. The rejection is atomic and advises smaller
batches. Individual traversals also have depth, object and allocation bounds.

Every engine Edit::Batch is checked before the document snapshot clone and recursive
permission/script handling: at most 4,096 edit nodes and 64 nesting levels. This structural
traversal cap includes non-artifact batches; ordinary single edits are unchanged. Only
batches containing multiple ArtifactTag or legacy MarkDecorative actions receive the
following additional workload checks. For artifact action count N, the estimated workload
must satisfy N × (document objects + encoded KiB rounded up) ≤ 100,000 and N × encoded bytes
≤ 128 MiB. Encoded bytes use the larger of the current serialized document size and the sum
of current COS stream raw lengths, including unsaved inserted/rewritten streams. Ordinary
single artifact actions keep their converter limits. No selection is silently truncated.

The batch encoded-work estimate is not a decompression bound. Independently, an engine edit transaction shares one 128 MiB cumulative decoded/copied-byte
budget across every artifact action, including legacy decorative actions and nested
batches, all selected namespaces, and Form scans. Standalone conversion starts its own
128 MiB allowance. The existing 64 MiB per stream/page limit also applies.
Repeated decoding, page concatenation and replacement output consume the cumulative
budget before allocation. A refusal leaves content, associations and undo history intact;
choose a smaller selection or subtree.

## Public-corpus verification

The [support checklist](corpus/support-checklist.md) distinguishes authoring, per-PDF
regressions, independent validator results and unperformed manual accessibility checks.
The pinned manifest covers 526 external PDFs; expected-invalid examples retain their
upstream intent. Strict Figure discovery reports incomplete traversal explicitly rather
than treating unresolved roles or page associations as an empty successful result.

Preserved unreachable ParentTree owners and globally unclaimed scalar entries are shown
as warnings. Safe unrelated edits require exact index/owner preservation, bounded global
claimant checks and a matching post-edit proof. Protected owner/content changes still
refuse. Shared K or ParentTree storage, stale references and unknown direct-node properties
are never silently rewritten for another consumer.

Dynamic XFA layout can regenerate pages without remapping existing tags. Tag edits on
that regenerated representation are disabled with an explicit explanation; form filling
remains available. This pipeline limitation is reported separately from ordinary roundtrip
preservation and from PDF/UA conformance.
