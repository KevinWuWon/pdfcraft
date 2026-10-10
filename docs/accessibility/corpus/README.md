# Accessibility corpus evidence

Start with the [support checklist](support-checklist.md) and the
[fresh 526-file acceptance results](results/README.md).

Original public PDFs stay outside this repository. The corpus manifest pins each
source, revision, path, SHA-256, profile and upstream target. Test compatibility,
refusals, independent machine validation and human accessibility review are separate.

## Additional evidence

- [artifact-evidence.json](artifact-evidence.json): independent checks of 23 exact
  artifact-conversion outputs from the fresh acceptance run
- [artifact-binding.json](artifact-binding.json): source/output/run binding for that check
- [geometry-binding.json](geometry-binding.json): fresh source-hash and exact identity/owner
  reconciliation for all 212 missing-bounds observations across 19 inputs
- [geometry-source-analysis.historical.json](geometry-source-analysis.historical.json):
  unchanged retained analysis of those original source bytes; older renderer
  observations in that file are historical and do not certify reconstructed code

The geometry receipt grants no support waiver. Empty/invisible/outside-crop content,
missing structural MCIDs and dynamic-XFA regeneration remain distinct explanations;
coverage gaps are retained in the new report.

Corpus/profile metadata and adaptations are [CC BY 4.0](LICENSE-CC-BY-4.0.txt).
Authors: veraPDF Consortium and PDF Association; analysis and adaptations by PdfCraft
contributors. Exact source and licence links are in the manifest. No corpus PDF,
embedded font, artwork, renderer output or validator executable is redistributed here.
