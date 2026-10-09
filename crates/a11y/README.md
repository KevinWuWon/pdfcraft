# pdfcraft-a11y

Layer L4: accessibility (execution plan M12.3). Today: the Accessibility Checker's full check.

```rust
let report = check(&doc, &Options::default());          // 31 of 32 rules (colour contrast off)
let html = report_html(&report, "file.pdf", "2026-10-02");
```

- **32 rules in 7 categories**, as in Acrobat's full check: Document (permission flag,
  image-only, tagged, reading order, language, title, bookmarks, colour contrast), Page Content
  (tagged content, tagged annotations, tab order, character encoding, tagged multimedia, and
  four manual checks), Forms (tagged fields, descriptions), Alternate Text (figures, nested,
  associated with content, hides annotation, other elements), Tables (rows, cells, headers,
  regularity with row/column spans, summary), Lists (items, Lbl/LBody) and Headings (nesting).
- Each result is Passed, Failed (with findings and pages), Needs manual check, or Skipped.
- **Content checks** walk the page content streams (and form XObjects) tracking marked
  content: painting outside MCID-marked or `/Artifact` content is untagged.
- **Structure checks** read the structure tree through the role map; table regularity counts
  `ColSpan`/`RowSpan` attributes.
- The report is a self-contained HTML page. Fixes that edit the document (language, title, tab
  order) live in the engine (`Document::accessibility_fix`).

Not yet: autotagging, the Order/Content panels and Reading Order tool (M12.1–M12.2).
The engine/UI integrate the structure editing API described below.

## Structure/tag editor

`tags::inspect` exposes persistent object/generation IDs, preorder elements and ordered
mixed children (StructElem, integer MCID, MCR and OBJR), RoleMap-resolved types, inherited
pages and page indexes, title, alternate/actual/expansion text, language and ID.
`TagTree.pages` maps content-item page references to zero-based document pages. Inspection
reports unsupported structures instead of pretending they are editable.

`tags::{create, update, move_tag, delete}` edit the original object graph. Properties are
tri-state (unchanged, clear, set); unspecified/unknown dictionary keys remain intact.
Moves pin inherited page associations and retain content identities; destination indexes
refer to the child list **after** removing the moved element. Existing indirect child arrays
remain indirect. Create makes empty structure only; it does not tag visual content or certify
accessibility. The first unspecified type is Document; later unspecified types default to P.
New types must be standard or resolve through the existing RoleMap.

Deletion explicitly chooses `DeleteMode::Unwrap` (promote ordered children and reassign their
ParentTree ownership) or `Subtree` (detach the tag subtree and clear its content associations).
Neither mode erases page drawing operators or marks them as artifacts. Nested ParentTree
number trees and indirect MCID arrays are handled without shifting MCID slots; removed OBJR
entries and their object's StructParent are cleared, and number-tree limits are updated.
Removed structure objects remain allocated until ordinary save/garbage collection.

Mutations validate first and return an actionable error without partially changing the document.
Walks are bounded; cycles, shared structure nodes/child arrays, duplicate content ownership,
wrong/orphan ParentTree associations and malformed children block edits. Direct structure roots
are inspectable but read-only; direct StructElem children are reported as unsupported.
PDF 2.0 namespace editing, automatic tagging, repairing broken trees, content association
creation and IDTree/structure-reference/destination-aware deletion are not implemented.
Deletion conservatively rejects IDTree, structure Ref and SD destinations. This is a bounded
editing foundation, not PDF/UA certification or full Acrobat parity.

Additional APIs: `set_role_mapping`, `move_content`, `move_tags`, `delete_tags`, and
`set_identifier`. They preserve exact object generations and either validate before
commit or use a clone-then-commit transaction. RoleMap updates validate all chains and
all tag uses. Content moves preserve page/Form namespaces and repair only the exact
ParentTree owner. Batch moves retain document order; subtree deletion collapses selected
descendants, while unwrap processes all selected tags deepest first. ID changes preserve
raw-byte name ordering and IDTree nodes/unknown keys; reference-sensitive or unsupported
name-tree mutations reject. See the advanced-actions section in
[the editor design](../../docs/accessibility/tag-editor.md#advanced-structure-actions).
`artifact::convert` marks exactly owned supported content as Artifact and detaches the
semantic subtree, using conservative copy-on-write stream changes.
