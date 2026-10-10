# Accessibility support and public-corpus checklist

This checklist separates authoring, compatibility regression tests, automated PDF/UA
validation, and human accessibility review. A PDF that opens, or has no findings from
PdfCraft's screening checks, is **not thereby PDF/UA conformant**.

## Reproducible external test set

`xtask/baselines/accessibility-corpus.json` pins every input by source revision,
relative path, byte count, SHA-256, licensing source, and upstream purpose. Original
PDFs are external test inputs, not redistributed assets. Their embedded fonts and
artwork are not approved for product documentation or published screenshots.

| Source | Pinned population | Upstream purpose |
| --- | ---: | --- |
| [veraPDF corpus](https://github.com/veraPDF/veraPDF-corpus) | 435 (297 PDF/UA-1; 138 PDF/UA-2) | Atomic positive/negative cases for particular requirements |
| [PDF Association techniques](https://github.com/pdf-association/techniques-for-accessible-pdf) | 82 | Technique-specific positive/negative examples, with YAML and human tests |
| [PDF/UA Reference Suite 1.1, February 2024](https://pdfa.org/resource/pdfua-reference-suite/) | 9 | Complex reference-positive documents |
| **Total** | **526** | Every manifest member must be accounted for |

The reference ZIP's `__MACOSX/._*.pdf` entries are AppleDouble metadata, not PDFs.
All three sources state CC BY 4.0. Attribution and exact source links are in the
manifest. A corpus filename's `pass` or `fail` is a **target requirement** expectation;
it is not an assertion of overall conformance. Historic file and outline rule numbers
can differ from current validator profiles. Unverified rule mappings remain unknown,
never an inferred successful check.

The 82 technique examples include nine machine-detectable primary negative cases,
36 compound positive techniques with selected machine subchecks, and 37 cases
whose primary requirement needs human semantic review. The manifest preserves the
upstream YAML and `tests.md` hashes and records this distinction. The runner's
selected primary check does not exhaust a compound technique. Manual checks remain
unperformed unless an explicit review record says otherwise.

### Test layers

1. Require the exact manifest population and matching input hashes. Missing files,
   stale reports, crashes, timeouts, and malformed output fail the run.
2. Open every PDF and inspect pages, structure, content identities, and screening
   findings. Preserve warnings and missing geometry explicitly.
3. Exercise editing operations on independent copies of each applicable input.
   Verify the intended change, unrelated object/tree preservation, atomic undo/redo,
   save/reopen persistence, and unchanged artwork for semantic-only edits.
4. Render every page at a bounded 512-pixel long side before and after edits using
   the same renderer. Pixel equality is regression evidence, not an independent
   proof of accurate rendering at every scale.
5. Run the separately installed, pinned veraPDF validator with explicit `ua1` or
   `ua2`, before and after the persisted title-property edit. Compare evaluated target rules and
   unintended new findings. Missing/unevaluated targets are incomplete evidence.
6. Keep successful edits, expected safe refusals, not-applicable operations,
   untested capabilities, and unexpected failures in separate counts. A refusal
   cannot count as successful feature support.

The engine example `a11y_corpus_probe` is isolated per file by
`cargo xtask a11y-corpus`. The runner uses a fresh output directory by default and records
per-file reports, timeouts, logs, source integrity, operation outcomes, and validator
identity. `--require-full-support` is stricter than accepting documented safe refusals.

### Per-file operation coverage

The probe records 14 separate operation slots. It does not count a slot as exercised
when the source lacks the required kind of content:

- Create a root/first tag for an empty structure. This does not tag page content.
- Edit an existing tag's title.
- Edit and reopen its alternate text, actual text, language and expansion text.
- Reorder an existing tag and reparent an existing tag, on separate copies.
- Move each available leaf kind separately: integer MCID, page MCR, Form MCR, OBJR.
- Add a custom role mapping and add/change a tag ID, on separate copies.
- Unwrap a tag, delete a subtree, and convert supported marked content to an artifact.

These are representative operations, not exhaustive combinations of every tag,
property value or destination. In particular, a successful OBJR move is evidence
for the selected association, not every annotation subtype in that PDF. Empty-tree
creation has a separate COS-graph proof and leaves the existing-tag title/oracle
comparison explicitly unexercised. Expected refusals are exact assertions: a changed
reason, missing operation, unexpected acceptance or unexpected “not applicable”
result fails until the source condition and expectation are reviewed again.

Handling categories describe the particular edit and its source condition, not the
whole document's conformance. `safe_invalid_input` means the independently checked
association/map condition makes that edit unsafe; `permission_restricted` means
the document forbids the edit; `unsupported_valid_input` identifies a compatibility
or authoring gap without treating that condition as malformed. A target-positive
fixture can still contain an unrelated malformed association. None of these labels
replaces the independent validator's result or human review.

## Authoring and annotation support

The table describes intended product capabilities; the accompanying run report is
the evidence for each external file. Synthetic tests cover hostile and unusual graph
shapes that published samples do not necessarily contain.

| Area | Authoring/inspection support | Required evidence or remaining boundary |
| --- | --- | --- |
| Structure hierarchy and reading order | Ordered tag/content tree; create, reorder, reparent, unwrap, delete; multiselect atomic operations | Mixed child order, parent links, page inheritance, cycle prevention, undo/save; meaningful reading order requires human review |
| Standard/custom roles | Type edits and root RoleMap editor | Resolve role chains; reject cycles/invalid mappings; preserve unknown dictionaries |
| PDF 2 namespaces | Namespace-aware role resolution, inspection and preservation | Explicit element namespaces, cross-namespace mappings, default namespace semantics; namespace-map authoring is not offered |
| Text properties | Title, Alt, ActualText, Lang, abbreviation expansion E | Observe each applied value after save/reopen; human review of meaningful replacements and language |
| Tag IDs | ID/IDTree-aware edits | Preserve byte-string ordering; reference-sensitive changes may be refused, including Headers and ContinuedFrom; external references cannot be repaired |
| Headings | Heading roles and hierarchy editing; screening checks | Heading levels and meaningful section structure; PDF 2 role differences |
| Lists | L/LI/Lbl/LBody roles and structure | Nesting and label/body semantics; list attribute authoring is not a dedicated UI |
| Tables | Table/TR/TH/TD and grouping roles; inspect/preserve attributes | Scope, Headers, spans, ID references, and ClassMap must survive; dedicated table-attribute/class editor is not implemented |
| Page marked content | MCID integer and MCR leaves, exact page-local identity, move/retag existing leaves | ParentTree ownership and generations; repeated MCIDs on different pages must not collide |
| Form marked content | Stream-qualified MCR and named Properties inspection/geometry | Preserve stream identity; reused/nested Form artifact ambiguities can require refusal |
| Object references | OBJR inspection and movement with exact object identity | Annotation and XObject association indexes preserved; creating new associations is a separate unsupported workflow |
| Links | Link tags and existing link OBJR inspection | Link purpose, destination validity and keyboard behavior require review; generic tag operations do not create annotations |
| Form widgets | Existing Widget OBJR/tag relationships and form descriptions screened | TU labels, form interaction and tab order need separate form tooling/manual verification |
| Dynamic XFA | Form filling remains available; tag mutations are refused after dynamic page regeneration | Existing tags are not remapped to regenerated pages; source-to-open changes are reported separately and are not a preservation pass. Static XFA without regeneration is unaffected |
| Other annotations | Existing OBJR structure retained | Text notes, markup, stamps, attachments, multimedia and other annotation subtypes are not all accessible merely because they have StructParent |
| Artifacts | Exact supported marked-content conversion, with actual operator rewriting | Resource/stream ambiguity and bounded batch budgets can cause atomic refusal; deletion alone is never artifact conversion |
| Untagged content | Inventory limitations disclosed; existing tagged content can be moved | Creating marked-content sequences for arbitrary unmarked text/images and automatic tagging are not implemented |
| Content navigation | Tree-to-page bounds and page-to-tree matching | Exact MCID/stream/OBJR identity; bounding envelopes, not pixel masks; unavailable geometry remains explicit |
| Persistence | Incremental Session save, reopen, snapshot undo/redo | Corpus probe identifies save mode; full-rewrite save needs separate evidence |
| Malformed/encrypted/signed input | Bounded inspection and permission-aware editing | Failed edit must preserve bytes, dirty state and history; metadata-only edits may preserve unrelated structural warnings |
| Keyboard and assistive UI | Tree navigation, focus, contextual actions, accessible tree semantics | Native pointer/keyboard QA and automated UI tests; actual screen-reader interoperability is a separate manual test |

### Annotation subtype inventory

“Accessibility annotations” can mean semantic tags or PDF annotation objects. They
are separate. This editor inspects and preserves existing OBJR relationships using
object identity; it does not infer accessibility from an annotation's appearance.
The corpus report must identify which actual subtypes were exercised:

- Link and Widget: tag association, alternate/form description, destination or field
  behavior, and keyboard/tab order
- Text, Popup, FreeText, Caret and Ink: readable contents and appropriate association
- Highlight, Underline, Squiggly and StrikeOut: meaning beyond color/markup appearance
- Line, Square, Circle, Polygon, PolyLine and Stamp: useful textual alternatives
- FileAttachment: attachment description and accessible embedded content
- Sound, Movie, Screen, RichMedia and 3D: accessible alternatives and interaction
- Redact, Watermark, PrinterMark and TrapNet: distinguish user-facing information
  from production or decorative content

Subtype presence in a source PDF is inventory evidence only. Generic OBJR move
success does not establish that all of these annotations have been individually
tested, correctly described, or made keyboard-operable. Unsupported authoring and
manual checks stay explicit in per-feature results.

The independent source inventory contains these exact subtype names:

| Raw subtype | PDFs containing it | Annotation objects |
| --- | ---: | ---: |
| Link | 43 | 172 |
| Widget | 25 | 48 |
| Highlight | 9 | 9 |
| Popup | 9 | 9 |
| FileAttachment | 6 | 6 |
| FREETEXT | 5 | 5 |
| Screen | 5 | 5 |
| PrinterMark | 1 | 1 |
| TrapNet | 1 | 1 |

Names are case-sensitive: the fixtures' `FREETEXT` is not silently normalized to
the standard `FreeText`. Subtypes absent from this population need synthetic or
additional external fixtures before claiming external-corpus coverage.

## Validation checklist beyond tag authoring

Use the appropriate independent PDF/UA profile and the
[Matterhorn Protocol](https://pdfa.org/resource/the-matterhorn-protocol/) for the full
requirement inventory. PdfCraft's built-in checker is screening, not a complete
replacement for that inventory.

- Document identification, PDF version, XMP, title/display preference and language
- Font embedding, glyph metrics, Unicode mapping and meaningful text extraction
- Security permissions and assistive-technology access
- Complete content tagging, artifacts, semantic roles and logical reading order
- Headings, lists, tables, formulas, notes, figures and alternative descriptions
- Links, widget labels, annotation associations and tab/navigation order
- Optional content, embedded files, multimedia, scripts, timing and flicker
- Human judgment: useful alternative text, correct language, meaningful reading
  order, color contrast, comprehension and operable controls

An automated rule that was not implemented, not evaluated, or needs human judgment
must remain visibly unverified. The public-corpus report must not turn those cases
into an aggregate accessibility pass.

## Verification status — 2026-10-10

The fresh reconstructed-source acceptance run completed **all 526 PDFs**, with
**zero unexpected failures and zero infrastructure errors**. Every input has a
durable record. This verifies the declared operational-compatibility contract;
it is not full authoring support, a full-support-mode pass or accessibility certification.
The older 2026-10-09 run is not used to certify the reconstructed code.

- **7,364 operation slots:** 4,734 successful edits, 217 exact expected edit refusals
  and 2,413 not-applicable slots. Successful edits include the applicable effect,
  preservation, undo/redo, save/reopen and all-page render checks.
- The **217 edit refusals** comprise 164 independently justified invalid-input safety
  conditions, 10 permission restrictions and 43 unsupported workflows. Refusals do
  not count as successful edits.
- **16 separate Figure-discovery stage refusals:** 14 invalid-input conditions and
  two unsupported cases. The source-bound diagnostic and checker uncertainty remain
  visible. These are not successful Figure enumeration or parity checks, and they
  are not added to the edit-refusal total.
- File outcomes: 468 operational passes, 49 with expected refusals, eight with
  source-proven unevaluated target checks and one unsupported dynamic-XFA case.
- Source-target evidence: 472 verified targets, eight explicitly incomplete
  zero-instance checks, 37 manual/unmapped primary requirements and nine reference
  documents assessed at machine-profile level. **73 technique reviews remain
  unperformed.**
- All **521 source-to-title-edit pairs** have identical complete validator rule
  vectors, check counts and boolean machine-profile compliance. Three empty trees
  have no existing-tag title comparison; the permission-restricted and regenerated-XFA
  cases likewise have no verified title-edit pair. Other saved edits do not inherit
  this title-only validator result.
- All **nine reference PDFs** pass their **102 applicable edit operations**. Each
  reference is machine-compliant before and after its persisted title edit.
- A fresh independent check of **23 exact acceptance-run artifact outputs** found
  unchanged all-page 72-dpi Poppler pixels, only the intended marked-content rewrite,
  unchanged resources and unrelated original streams, and no new or increased
  veraPDF failures. All ten previously machine-compliant inputs remained compliant.
- Coverage gaps stay explicit: 212 owned identities lack bounds across 19 PDFs;
  273 rendered identity observations lack a reachable owner across 41 PDFs; 38 PDFs
  report geometry issues. These populations overlap and are not a new failure total.
  Original-source analysis was rebound to all 212 exact missing identity/owner tuples;
  its older renderer corroboration remains historical, not a new rendering pass.

Read the [per-file summary and reproducible archive](results/README.md),
[23-output artifact evidence](artifact-evidence.json), and
[geometry source-analysis binding](geometry-binding.json). The complete portable
JSON is stored as standard deterministic gzip with compressed and uncompressed
hashes. Independent reconciliation checked all 7,364 operation records, all 1,048
recorded oracle vectors, and both non-null page-inheritance proofs with zero
reconciliation issues. The stricter full-support criterion is not met.

### Code and interface verification

Fresh checks passed **1,586 workspace tests**, including **163 accessibility-crate
tests**, with **five skips**. Separately, all **30 corpus-probe tests** and **39 private
exporter tests** passed. Workspace formatting, strict clippy, asset and dependency-layer
checks, all **30 WASM crates**, and the minimal CLI and production native build passed.
The parity consistency check also completed; it does not establish complete feature parity.

Actual pointer/keyboard verification on the cloud Linux desktop covered the tree,
properties and draft-loss guard, title Apply/Save/Undo/Redo, search, context menus,
reorder/undo, RoleMap persistence, highlights and compact/light/dark layouts. Final
regression retesting confirmed cohesive bold-heading outlines and continuously
preserved property-edit selection/highlights. Structural moves intentionally clear
stale selection. Only a contributor-original fixture was displayed.

Runtime QA used an isolated egui Glow/GLX shell because the cloud desktop's default
EGL path could not paint. The production WGPU desktop binary compiled; no production
renderer runtime result is inferred. macOS, Windows and real screen-reader
interoperability were not runtime-tested here.

### Commands

Fetch the two Git repositories from the source URLs in the manifest and check out
exactly `bb75f4f0073d9350dfd058c0162a367e6fadf25e` (veraPDF) and
`3e31095d8aeea941eccbbb766a23369374c2e9a6` (techniques). Extract the reference
archive into a separate directory and verify its archive hash from the manifest.
Use the official veraPDF **1.30.3** CLI; installer provenance and signature key
fingerprint are recorded in the manifest's `oracle` object. Keep these external
inputs outside the repository. For example, prepare a new external directory:

```sh
mkdir pdfcraft-public-corpus
cd pdfcraft-public-corpus
git clone https://github.com/veraPDF/veraPDF-corpus.git verapdf
git -C verapdf checkout bb75f4f0073d9350dfd058c0162a367e6fadf25e
git clone https://github.com/pdf-association/techniques-for-accessible-pdf.git techniques
git -C techniques checkout 3e31095d8aeea941eccbbb766a23369374c2e9a6
curl --fail --location --output reference.zip \
  https://pdfa.org/wp-content/uploads/2024/02/PDFUA-Reference-Files_1-1_2024_02.zip
printf '%s  %s\n' \
  5ff554857b2dac0623aaee340a5df2f444f683ee8d197148bc64ac83a7832cd8 \
  reference.zip | sha256sum --check
unzip reference.zip -d reference
```

The runner selects only the nine manifest PDF paths, excluding AppleDouble files.
Then, from the repository root:

```sh
cargo build -p pdfcraft-engine --example a11y_corpus_probe
cargo xtask a11y-corpus \
  --probe target/debug/examples/a11y_corpus_probe \
  --corpus-root verapdf=/absolute/path/to/veraPDF-corpus \
  --corpus-root techniques=/absolute/path/to/techniques-for-accessible-pdf \
  --corpus-root reference=/absolute/path/to/reference-suite \
  --verapdf /absolute/path/to/verapdf \
  --timeout 600 \
  --jobs 3 \
  --output /absolute/path/to/new-results-directory
```

Use a new output directory. Explicit `--resume` is only for interrupted work with
identical runner/probe/manifest/options/oracle hashes; it rechecks sources and durable
records and is not permission to reuse another implementation's evidence. The command writes `report.json` and per-file
records even when the overall result fails. Inspect those records before altering
any expectation: an observed refusal is not automatically an approved baseline.
The runner freezes and hashes the probe executable before starting. `--jobs`
accepts 1–4 isolated file workers; the final report retains manifest order. Every
file gets a durable result, including timeouts and worker failures. The default
is one worker. Keep enough free disk space for each edited PDF and oracle report.
The upstream technique's adjacent `technique-info.yaml` and `tests.md` describe
human checks; those remain unperformed unless separately documented.

Relevant synthetic and interactive regression suites live in:

- `crates/a11y/src/tags.rs`: graph mutations, namespace/role resolution, ID references,
  malformed/shared objects and text-only preservation boundaries; the split
  `crates/a11y/src/tags/` tests cover namespaces and preserved associations
- `crates/a11y/src/artifact.rs`: exact content conversion and resource budgets
- `crates/a11y/src/alt.rs` and `structure.rs`: figure and screening traversal
- `crates/render/src/tagged.rs`: geometry identities, transforms, text, images and OBJR
- `crates/engine/src/tag_selection.rs` and engine tag tests: hit matching and transactions
- `crates/automation/tests/automation.rs`: strict tool arguments and real save/reopen
- `crates/ui-egui/tests/tags.rs` and `tag_dialogs.rs`: native toolkit interaction/focus
- `crates/engine/examples/a11y_corpus_probe.rs`: preservation-oracle self-tests
- `xtask/src/accessibility_corpus.rs`: missing inputs, tampering, false passes,
  incomplete oracle output, crashes and timeout handling

### Portable report reproduction

The runner output is the authoritative raw evidence. Keep it privately with its
frozen executable and edited outputs. The publication helper is separate private
Python tooling, not part of PdfCraft's Rust product or build. Its source and tests
must accompany the evidence handoff; the compact report records the helper hash.
Run the supplied `export_accessibility_report.py` with:

```sh
python3 export_accessibility_report.py \
  --report /absolute/path/to/new-results-directory/report.json \
  --manifest xtask/baselines/accessibility-corpus.json \
  --runner-source xtask/src/accessibility_corpus.rs \
  --probe /absolute/path/to/new-results-directory/tools/a11y_corpus_probe \
  --output /absolute/path/to/new-portable-report-directory \
  --include-details --require-success
```

The exporter requires matching manifest bytes, runner source and frozen probe
hashes; it never upgrades a historical portable report into new evidence. It keeps
all operation fields, stage diagnostics, refusal reasons/categories and geometry
coverage. Full validator rule vectors are reduced to canonical SHA-256 hashes,
exact rule/check counts, machine-profile compliance, target rules and failing-rule
details. A same-as-before reference is emitted only after comparing complete vectors
and an explicit true preservation assertion. Recompute those hashes from the retained
raw report using the encoding contract in the portable report; a compact hash alone
cannot reconstruct the omitted vector.

Generated `files.csv` and `operations.csv` are convenience views. The JSON retains
complete portable evidence and exact texts. `checksums.sha256` binds exported files.
The report is operational evidence only; source artifacts and executable binaries
are not included, and external artifact/geometry investigations stay separately scoped.

### Representative synthetic evidence locations

These are reproducible test identifiers, not a claim that a fresh test run passed.
The final verification log must identify the exact source that ran them.

| Boundary | Source and named regression |
| --- | --- |
| Role/namespace cycles and atomic failure | `crates/a11y/src/tags/namespace_tests.rs`: `cyclic_unknown_stale_and_undeclared_namespaces_are_explicit_and_atomic` |
| Figure traversal through indirect arrays | Same file: `figure_discovery_and_checker_follow_indirect_child_arrays` |
| Preserved orphan ParentTree entries | `crates/a11y/src/tags/orphan_tests.rs`: `preserved_same_key_orphans_allow_safe_structural_edits_and_roundtrip` |
| Scalar owner identity versus mutable contents | Same file: `scalar_entry_tracks_owner_reference_instead_of_resolved_mutable_dictionary` |
| Exact Form/page content namespaces | `crates/render/src/tagged.rs`: `forms_scope_duplicate_mcids_and_preserve_transformed_invocations` and `same_mcid_on_distinct_pages_stays_page_scoped` |
| Honest unavailable geometry | Same file: `invisible_text_keeps_identity_without_fabricated_geometry`, `cropped_out_text_keeps_identity_and_reappears_only_when_crop_expands` and `hidden_noview_empty_and_offpage_annotations_have_no_selection_regions` |
| Reverse lookup preserves exact generations | `crates/engine/src/tag_selection.rs`: `tag_selection_returns_all_owners_and_matches_exact_stream_generation` |
| Headless artifact/ID/XFA transactions | `crates/automation/tests/automation.rs`: `tag_artifact_tool_keeps_pixels_and_roundtrips_history`, `fresh_identifier_with_unrelated_references_roundtrips_and_rejects_activation` and `regenerated_xfa_tag_tools_refuse_without_disabling_form_tools` |
| Native property draft/modal safety | `crates/ui-egui/tests/tags.rs`: `tree_find_shortcut_and_dirty_close_keep_target_and_draft` and `app_modal_suspends_tag_properties_and_preserves_draft` |
| Namespace-aware type suggestions | Same file: `pdf2_type_suggestions_offer_em_and_strong_without_legacy_quote_or_note` and `custom_and_mathml_namespaces_keep_manual_type_entry_without_pdf_suggestions` |
