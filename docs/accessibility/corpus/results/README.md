# Accessibility corpus acceptance results

Fresh reconstructed-source run, 2026-10-10: **526/526 PDFs completed and durable,
zero unexpected failures and zero infrastructure errors**. This is operational
compatibility evidence, not PDF/UA, WCAG or assistive-technology certification.

## Results

| Evidence | Count |
| --- | ---: |
| Successful edits | 4,734 |
| Exact expected edit refusals | 217 |
| Not-applicable operation slots | 2,413 |
| Total operation slots | 7,364 |
| Separately counted Figure-discovery stage refusals | 16 |
| Verified source-to-title oracle pairs | 521 |
| Required technique reviews still unperformed | 73 |

The 217 edit refusals comprise 164 invalid-input safety conditions, 10 permission
restrictions and 43 unsupported workflows. The 16 Figure-stage refusals comprise
14 invalid-input conditions and two unsupported cases; they are not successful
Figure enumeration. Refusals never count as successful edits.

File outcomes: 468 operational passes, 49 expected-refusal cases, eight incomplete
target-rule checks and one unsupported dynamic-XFA case. Nine reference PDFs pass
all 102 applicable edits and are machine-profile compliant before/after their title
edits. Source-target verification remains separate from whole-document conformance.

## Per-file evidence

- [files.csv](files.csv): browsable outcome, source identity, target and manual-review index
- [report.json.gz](report.json.gz): complete portable schema-3 JSON, losslessly compressed
- [report-archive.json](report-archive.json): compressed/uncompressed lengths and SHA-256
- [reconciliation.json](reconciliation.json): independent raw-to-portable verification receipt
- [Support checklist](../support-checklist.md): authoring/annotation boundaries, tests and reproduction
- [Artifact evidence](../artifact-evidence.json): fresh independent checks on 23 exact saved outputs
- [Geometry binding](../geometry-binding.json): current identity/source binding of retained source analysis

The report preserves every operation and its invariants, exact refusal texts and
categories, all portable probe/assessment fields, Figure diagnostics, per-page
geometry counts and omissions, and complete page-inheritance/structure-destination
proofs. Whole validator vectors are represented by canonical hashes, exact counts,
matching target rules, failing-rule details and boolean compliance. Full vectors
remain in the separately retained private raw audit trail.

Extract using any standard gzip implementation:

```sh
gzip -dc report.json.gz > report.json
```

The archive receipt's `uncompressed_sha256` must match the extracted bytes. No
custom decoder is needed. The downloadable evidence ZIP also contains uncompressed
report.json, detailed-report.json, all 7,364 operations in CSV and the private
publication helper's source/tests. PDFs, rendered assets and executables are excluded.

The independent reconciliation compared all 526 files, all 7,364 operation records,
all 1,048 oracle vectors, both non-null inherited-page proofs and all aggregate counts.
It found no reconciliation issues. A preserved oracle pair applies only to the
source and persisted title edit, not automatically to other edited outputs.

## Scope and provenance

The pinned population is 435 veraPDF cases, 82 PDF Association techniques and nine
PDF/UA Reference Suite files. Revisions, per-source paths and SHA-256 are embedded
in the report and pinned in the repository's accessibility-corpus manifest.

- Raw acceptance report SHA-256: `ccbd7240d9073e60a472f9938f675df02b6f46942c62a418c90c8a2cfc8f38f9`
- Manifest SHA-256: `11a5946e7282db563f42c40eb308c1c891fce4b56e184f89f6c3e2357f13e25d`
- Frozen probe SHA-256: `1182b3c688b3d58bda44eb9caffa73c74fb56ed23e2a623ed3c87cfa13120a26`
- Runner source SHA-256: `16faa9eb09d138a804da04a8d498dc46616efd0be0602e2f0a81d8eb37be8228`

Full-support mode was not requested and its stricter criterion is not met. Missing
bounds, unowned geometry, unexercised leaf kinds, unsupported authoring and manual
semantics remain visible. Earlier runs, including the failed reconstructed diagnostic
run, are not substituted for this acceptance run. Historical geometry renderer
corroboration remains historical; its current binding concerns immutable source
bytes and exact missing identity/owner sets only.
