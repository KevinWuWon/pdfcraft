//! Public-check regressions: uncertainty must never appear as a clean empty result.
use crate::{Options, Rule, Status, check, tags, try_figures};
use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};

const AFFECTED: [Rule; 9] = [
    Rule::FiguresAltText,
    Rule::HeadingNesting,
    Rule::TableRows,
    Rule::ListItems,
    Rule::TaggedPdf,
    Rule::TaggedContent,
    Rule::TaggedAnnotations,
    Rule::TaggedMultimedia,
    Rule::TaggedFormFields,
];
fn options() -> Options {
    Options { rules: AFFECTED.into(), pages: None }
}
fn marked() -> Document {
    let mut doc = Document::new_empty();
    let mut mark = Dict::new();
    mark.set(b"Marked", Object::Bool(true));
    doc.update_dict(doc.root().unwrap(), |d| d.set(b"MarkInfo", mark)).unwrap();
    doc
}
fn fixture() -> (Document, ObjRef, ObjRef) {
    let mut doc = marked();
    let top = tags::create(&mut doc, None, 0, &tags::TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
    let root = tags::inspect(&doc).unwrap().root.unwrap();
    (doc, top, root)
}
fn uncertain(doc: &Document, text: &str) {
    let report = check(doc, &options());
    for rule in AFFECTED {
        let result = report.result(rule).unwrap();
        assert_eq!(result.status, Status::Manual, "{rule:?}: {:?}", result.findings);
        assert!(result.findings.iter().any(|f| f.message.contains(text)), "{rule:?}: {:?}", result.findings);
    }
    assert!(try_figures(doc).is_err());
}
#[test]
fn malformed_present_roots_are_uncertain_and_figure_errors() {
    for mode in 0..8 {
        let mut doc = marked();
        let root = match mode {
            0 => Object::Null,
            1 => Object::Bool(false),
            2 => Object::name("StructTreeRoot"),
            3 => Object::Stream(Stream::from_raw(Dict::new(), Vec::new())),
            4 => {
                let mut d = Dict::new();
                d.set(b"Type", Object::name("Wrong"));
                Object::Dict(d)
            }
            5 => {
                let id = doc.add(Dict::new());
                Object::Ref(ObjRef::new(id.num, id.generation + 1))
            }
            6 => Object::Ref(ObjRef::new(999_999, 0)),
            _ => {
                let mut d = Dict::new();
                d.set(b"Type", Object::name("StructTreeRoot"));
                Object::Ref(doc.add(Object::Stream(Stream::from_raw(d, vec![]))))
            }
        };
        doc.update_dict(doc.root().unwrap(), |d| d.set(b"StructTreeRoot", root)).unwrap();
        uncertain(&doc, "Structure resolution incomplete");
    }
    assert!(try_figures(&Document::new_empty()).unwrap().is_empty());
}
#[test]
fn malformed_children_are_explicit_not_silently_ignored() {
    for kid in [
        Object::Null,
        Object::Bool(true),
        Object::name("P"),
        Object::Real(0.5),
        Object::String(pdfcraft_cos::PdfString::text("P")),
        Object::Stream(Stream::from_raw(Dict::new(), vec![])),
    ] {
        let (mut doc, top, _) = fixture();
        doc.update_dict(top, |d| d.set(b"K", kid)).unwrap();
        uncertain(&doc, "structure child");
    }
}
#[test]
fn missing_type_name_and_negative_mcids_are_diagnostics() {
    let (mut doc, top, _) = fixture();
    let mut missing = Dict::new();
    missing.set(b"Type", Object::name("StructElem"));
    doc.update_dict(top, |d| d.set(b"K", missing)).unwrap();
    uncertain(&doc, "missing S");
    doc.update_dict(top, |d| d.set(b"K", Object::Int(-1))).unwrap();
    uncertain(&doc, "nonnegative");
}
#[test]
fn stale_or_missing_child_references_are_diagnostics() {
    for stale in [true, false] {
        let (mut doc, top, _) = fixture();
        let kid = if stale { ObjRef::new(top.num, top.generation + 1) } else { ObjRef::new(999_999, 0) };
        doc.update_dict(top, |d| d.set(b"K", Object::Ref(kid))).unwrap();
        uncertain(&doc, "Structure resolution incomplete");
    }
}
#[test]
fn huge_non_element_arrays_and_aggregate_visits_are_bounded() {
    for aggregate in [false, true] {
        let (mut doc, top, _) = fixture();
        let kids = if aggregate {
            Object::Array(vec![Object::Array(vec![Object::Null; 50_001]), Object::Array(vec![Object::Null; 50_001])])
        } else {
            Object::Array(vec![Object::Null; 100_001])
        };
        doc.update_dict(top, |d| d.set(b"K", kids)).unwrap();
        uncertain(&doc, "budget");
        let tree = crate::structure::Tree::read(&doc, &[]);
        assert!(tree.elems.len() <= 1);
        assert!(tree.issues.len() <= 200);
    }
}
#[test]
fn mcr_objr_and_page_references_are_checked_exactly() {
    for mode in 0..8 {
        let (mut doc, top, _) = fixture();
        let mut kid = Dict::new();
        if mode <= 3 {
            kid.set(b"Type", Object::name("MCR"));
            match mode {
                0 => {}
                1 => kid.set(b"MCID", Object::Int(-1)),
                2 => kid.set(b"MCID", Object::Real(0.0)),
                _ => {
                    kid.set(b"MCID", Object::Int(0));
                    kid.set(b"Stm", Object::Int(10));
                }
            }
        } else {
            kid.set(b"Type", Object::name("OBJR"));
            match mode {
                4 => {}
                5 => kid.set(b"Obj", Object::Int(3)),
                6 => kid.set(b"Obj", Object::Ref(ObjRef::new(top.num, top.generation + 1))),
                _ => {
                    kid.set(b"Obj", Object::Ref(top));
                    kid.set(b"Pg", Object::Dict(Dict::new()));
                }
            }
        }
        doc.update_dict(top, |d| d.set(b"K", kid)).unwrap();
        uncertain(&doc, "Structure resolution incomplete");
    }
}
#[test]
fn definite_missing_alt_is_failed_with_traversal_uncertainty_appended() {
    let (mut doc, top, _) = fixture();
    let figure = tags::create(&mut doc, Some(top), 0, &tags::TagProperties { role: Some("Figure".into()), ..Default::default() }).unwrap();
    doc.update_dict(top, |d| d.set(b"K", Object::Array(vec![Object::Ref(figure), Object::Null]))).unwrap();
    let result = check(&doc, &options()).result(Rule::FiguresAltText).unwrap().clone();
    assert_eq!(result.status, Status::Failed);
    assert!(result.findings.iter().any(|f| f.message.contains("no alternate text")));
    assert!(result.findings.iter().any(|f| f.message.contains("Structure resolution incomplete")));
    assert!(try_figures(&doc).is_err());
}

#[test]
fn direct_figure_is_checked_but_discovery_reports_unaddressable_identity() {
    let (mut doc, top, _) = fixture();
    let mut figure = Dict::new();
    figure.set(b"S", Object::name("Figure"));
    doc.update_dict(top, |d| d.set(b"K", figure)).unwrap();
    let report = check(&doc, &options());
    assert_eq!(report.result(Rule::FiguresAltText).unwrap().status, Status::Failed);
    assert!(try_figures(&doc).unwrap_err().to_string().contains("direct Figure"));
}

#[test]
fn figure_alt_text_uses_the_same_exact_reference_chain_as_checker() {
    let (mut doc, top, _) = fixture();
    let figure = tags::create(&mut doc, Some(top), 0, &tags::TagProperties { role: Some("Figure".into()), ..Default::default() }).unwrap();
    let text = doc.add(Object::String(pdfcraft_cos::PdfString::text("Through two references")));
    let alias = doc.add(Object::Ref(text));
    doc.update_dict(figure, |d| d.set(b"Alt", Object::Ref(alias))).unwrap();
    assert_eq!(try_figures(&doc).unwrap()[0].alt.as_deref(), Some("Through two references"));
    doc.set(alias, Object::Ref(ObjRef::new(text.num, text.generation + 1)));
    assert!(try_figures(&doc).is_err());
    let report = check(&doc, &options());
    assert_eq!(report.result(Rule::FiguresAltText).unwrap().status, Status::Failed);
    assert!(report.result(Rule::FiguresAltText).unwrap().findings.iter().any(|f| f.message.contains("incomplete")));
}

fn table_document(colspan: i64, cells: usize, rows: usize) -> Document {
    let (mut doc, top, _) = fixture();
    let mut attrs = Dict::new();
    attrs.set(b"ColSpan", Object::Int(colspan));
    let mut cell = Dict::new();
    cell.set(b"S", Object::name("TD"));
    cell.set(b"A", attrs);
    let mut row = Dict::new();
    row.set(b"S", Object::name("TR"));
    row.set(b"K", Object::Array(vec![Object::Dict(cell); cells]));
    let mut table = Dict::new();
    table.set(b"S", Object::name("Table"));
    table.set(b"K", Object::Array(vec![Object::Dict(row); rows]));
    doc.update_dict(top, |d| d.set(b"K", table)).unwrap();
    doc
}
#[test]
fn huge_spans_and_wide_rows_are_uncertain_without_grid_allocation() {
    for doc in [table_document(i64::MAX, 1, 1), table_document(10_000, 2, 2), table_document(1, 10_001, 1)] {
        let report = check(&doc, &Options { rules: [Rule::TableRegularity].into(), pages: None });
        let result = report.result(Rule::TableRegularity).unwrap();
        assert_eq!(result.status, Status::Manual, "{:?}", result.findings);
        assert!(result.findings.iter().any(|f| f.message.contains("safe regularity limits")));
        // A regularity-only bound doesn't prevent confirmed Figure discovery.
        assert!(try_figures(&doc).unwrap().is_empty());
    }
}
#[test]
fn repeated_wide_rows_share_one_table_work_budget() {
    let doc = table_document(10_000, 1, 200);
    let report = check(&doc, &Options { rules: [Rule::TableRegularity].into(), pages: None });
    let result = report.result(Rule::TableRegularity).unwrap();
    assert_eq!(result.status, Status::Manual);
    assert!(result.findings.iter().any(|f| f.message.contains("aggregate table regularity work budget")));
}
