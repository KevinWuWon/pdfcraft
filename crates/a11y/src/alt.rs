//! Add alternate text: the document's figures one by one, their alternate text, and marking a
//! figure as decorative (its content becomes an artifact and its tag goes away).

use std::collections::HashSet;

use pdfcraft_content::Matrix;
use pdfcraft_cos::{Dict, Document, ObjRef, Object};

/// A Figure element (after the role map).
#[derive(Clone, Debug, PartialEq)]
pub struct Figure {
    pub obj: ObjRef,
    /// 0-based page of its content.
    pub page: Option<usize>,
    pub alt: Option<String>,
    /// Where its content is drawn, in user space (images, forms and paths it marks).
    pub bbox: Option<[f64; 4]>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AltError {
    #[error("the document has no tags")]
    Untagged,
    #[error("object {0} is not a figure")]
    NotAFigure(u32),
    #[error("{0}")]
    Artifact(#[from] crate::tags::TagError),
    #[error("{0}")]
    Cos(#[from] pdfcraft_cos::CosError),
}

/// Shared checker traversal rejects malformed-present trees rather than silently
/// turning missing/invalid children into an empty list of figures.
fn checked_tree(doc: &Document, pages: &[pdfcraft_model::Page]) -> Result<crate::structure::Tree, AltError> {
    let tree = crate::structure::Tree::read(doc, pages);
    if let Some(issue) = tree.issues.first() {
        return Err(crate::tags::TagError::Invalid(format!("cannot enumerate figures: {issue}")).into());
    }
    Ok(tree)
}

/// The marked-content ids an element owns directly.
fn mcids(d: &Dict) -> Vec<i64> {
    let mut out = Vec::new();
    let mut push = |o: &Object| match o {
        Object::Int(n) => out.push(*n),
        Object::Dict(m) if m.name(b"Type") == Some(b"MCR") => {
            if let Some(n) = m.get(b"MCID").and_then(Object::as_int) {
                out.push(n)
            }
        }
        _ => {}
    };
    match d.get(b"K") {
        Some(Object::Array(a)) => a.iter().for_each(&mut push),
        Some(o) => push(o),
        None => {}
    }
    out
}

const BBOX_DECODE_BUDGET: usize = 32 * 1024 * 1024;
const BBOX_OBJECT_LIMIT: usize = 10_000;

/// The budget covers every stream in the concatenation and every figure in the
/// discovery call. No partial content is used to produce a misleading rectangle.
fn page_content(doc: &Document, page: &Dict, remaining: &mut usize) -> Option<Vec<u8>> {
    fn append(doc: &Document, raw: &Object, out: &mut Vec<u8>, remaining: &mut usize, visits: &mut usize, depth: usize) -> Option<()> {
        if depth > 128 || *visits >= BBOX_OBJECT_LIMIT {
            return None;
        }
        *visits = visits.checked_add(1)?;
        match raw {
            Object::Ref(id) => {
                if doc.generation(id.num) != id.generation {
                    return None;
                }
                let object = doc.try_get(id.num).ok()?;
                append(doc, &object, out, remaining, visits, depth.saturating_add(1))
            }
            Object::Array(values) => {
                if values.len() > BBOX_OBJECT_LIMIT.saturating_sub(*visits) {
                    return None;
                }
                for value in values {
                    append(doc, value, out, remaining, visits, depth.saturating_add(1))?;
                }
                Some(())
            }
            Object::Stream(s) => {
                let allowance = remaining.checked_sub(1)?;
                if s.raw.len() > BBOX_DECODE_BUDGET || s.dict.contains(b"F") {
                    return None;
                }
                for key in [b"Filter".as_slice(), b"DecodeParms"] {
                    if s.dict.get(key).is_some_and(|v| matches!(v, Object::Array(a) if a.len() > 64)) {
                        return None;
                    }
                }
                crate::artifact::validate_decode_parameters(s, &mut crate::artifact::ArtifactBudget::default()).ok()?;
                let chain = s.filters();
                let bytes = if chain.is_empty() {
                    if s.raw.len() > allowance {
                        return None;
                    }
                    s.raw.as_ref().clone()
                } else {
                    pdfcraft_filters::decode(&chain, &s.raw, allowance).ok()?
                };
                let consumed = bytes.len().checked_add(1)?;
                *remaining = remaining.checked_sub(consumed)?;
                out.extend_from_slice(&bytes);
                out.push(b'\n');
                Some(())
            }
            _ => None,
        }
    }
    let mut out = Vec::new();
    if let Some(c) = page.get(b"Contents") {
        append(doc, c, &mut out, remaining, &mut 0, 0)?;
    }
    Some(out)
}

fn union(a: Option<[f64; 4]>, b: [f64; 4]) -> [f64; 4] {
    match a {
        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
        None => b,
    }
}

/// Where the marked content `ids` is drawn on a page (images, forms and paths).
fn content_bbox(doc: &Document, page: &Dict, ids: &HashSet<i64>, remaining: &mut usize) -> Option<[f64; 4]> {
    let res = page.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let xobjects = res.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
    let mut ctm = vec![Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])];
    let mut marks: Vec<bool> = Vec::new();
    let mut bbox = None;
    let mut path: Vec<(f64, f64)> = Vec::new();
    for op in pdfcraft_content::parse(&page_content(doc, page, remaining)?).ops {
        let inside = marks.iter().any(|m| *m);
        let top = ctm.last().copied().unwrap_or(Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]));
        match op.op.as_slice() {
            b"q" => ctm.push(top),
            b"Q" if ctm.len() > 1 => {
                ctm.pop();
            }
            b"cm" => {
                if let Some(m) = Matrix::from_operands(&op.operands)
                    && let Some(t) = ctm.last_mut()
                {
                    *t = m.then(&top);
                }
            }
            b"BMC" => marks.push(false),
            b"BDC" => marks.push(
                matches!(op.operands.get(1), Some(Object::Dict(d)) if d.get(b"MCID").and_then(Object::as_int).is_some_and(|n| ids.contains(&n))),
            ),
            b"EMC" => {
                marks.pop();
            }
            b"m" | b"l" => {
                if let Some([x, y]) = op.nums::<2>() {
                    path.push(top.apply(x, y));
                }
            }
            b"c" => {
                if let Some([_, _, _, _, x, y]) = op.nums::<6>() {
                    path.push(top.apply(x, y));
                }
            }
            b"re" => {
                if let Some([x, y, w, h]) = op.nums::<4>() {
                    let b = top.bbox([x, y, x + w, y + h]);
                    path.push((b[0], b[1]));
                    path.push((b[2], b[3]));
                }
            }
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"S" | b"s" | b"n" => {
                if inside && !matches!(op.op.as_slice(), b"n") {
                    for (x, y) in &path {
                        bbox = Some(union(bbox, [*x, *y, *x, *y]));
                    }
                }
                path.clear();
            }
            b"Do" if inside => {
                let Some(r) = op.name(0).and_then(|n| xobjects.get(n)).and_then(Object::as_ref) else { continue };
                let unit = match &*doc.get(r) {
                    Object::Stream(s) if s.dict.name(b"Subtype") == Some(b"Form") => {
                        let bb: Vec<f64> =
                            s.dict.get(b"BBox").and_then(|b| b.as_array().map(|a| a.iter().filter_map(Object::as_f64).collect())).unwrap_or_default();
                        let m = Matrix::from_operands(s.dict.get(b"Matrix").and_then(|m| m.as_array()).map(Vec::as_slice).unwrap_or(&[]))
                            .unwrap_or(Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]));
                        match bb[..] {
                            [a, b, c, d] => m.then(&top).bbox([a, b, c, d]),
                            _ => continue,
                        }
                    }
                    _ => top.bbox([0.0, 0.0, 1.0, 1.0]),
                };
                bbox = Some(union(bbox, unit));
            }
            b"BI" if inside => bbox = Some(union(bbox, top.bbox([0.0, 0.0, 1.0, 1.0]))),
            _ => {}
        }
    }
    bbox
}

/// The figures, in document order.
pub fn figures(doc: &Document) -> Vec<Figure> {
    // Legacy convenience API; interactive/automation callers use try_figures to expose
    // unresolved namespaces instead of treating this fallback as proof of no figures.
    try_figures(doc).unwrap_or_default()
}

fn exact_text(doc: &Document, raw: &Object, depth: usize) -> Result<String, AltError> {
    if depth > 128 {
        return Err(crate::tags::TagError::Invalid("alternate text reference depth exceeds safe limits".into()).into());
    }
    match raw {
        Object::String(s) if s.bytes.len() <= 1_048_576 => Ok(s.to_text()),
        Object::Ref(id) if doc.generation(id.num) == id.generation => exact_text(doc, &*doc.try_get(id.num)?, depth.saturating_add(1)),
        _ => Err(crate::tags::TagError::Invalid("alternate text is stale or not a bounded string".into()).into()),
    }
}

/// Figure discovery with explicit namespace and traversal diagnostics.
pub fn try_figures(doc: &Document) -> Result<Vec<Figure>, AltError> {
    let pages = pdfcraft_model::pages(doc);
    let tree = checked_tree(doc, &pages)?;
    let mut figures = Vec::new();
    let mut remaining = BBOX_DECODE_BUDGET;
    for e in tree.elems.iter().filter(|e| e.ty == b"Figure") {
        let obj = e.obj.ok_or_else(|| crate::tags::TagError::Invalid("direct Figure cannot be addressed safely".into()))?;
        let value = doc.try_get(obj.num)?;
        let Object::Dict(d) = &*value else {
            return Err(AltError::NotAFigure(obj.num));
        };
        let alt = d.get(b"Alt").map(|raw| exact_text(doc, raw, 0)).transpose()?;
        let ids: HashSet<i64> = mcids(d).into_iter().collect();
        let bbox = e.page.and_then(|p| pages.get(p)).and_then(|p| content_bbox(doc, &p.dict, &ids, &mut remaining));
        figures.push(Figure { obj, page: e.page, alt, bbox });
    }
    Ok(figures)
}

fn check_figure(doc: &Document, obj: ObjRef) -> Result<(), AltError> {
    let pages = pdfcraft_model::pages(doc);
    let tree = checked_tree(doc, &pages)?;
    if !tree.exists {
        return Err(AltError::Untagged);
    }
    if tree.elems.iter().any(|e| e.obj == Some(obj) && e.ty == b"Figure") { Ok(()) } else { Err(AltError::NotAFigure(obj.num)) }
}

/// Set (or, with `None` or blank text, remove) a figure's alternate text.
pub fn set_alt(doc: &mut Document, obj: ObjRef, alt: Option<&str>) -> Result<(), AltError> {
    check_figure(doc, obj)?;
    let alt = alt.map(str::trim).filter(|a| !a.is_empty());
    crate::tags::update(doc, obj, &crate::tags::TagProperties { alt: Some(alt.map(str::to_owned)), ..Default::default() })?;
    Ok(())
}

/// Mark a leaf figure's exact content as Artifact and detach its semantic association.
/// Uses the same atomic namespace-aware operation as the tag editor. Unsupported or
/// reference-sensitive structures are rejected without touching streams or ParentTree.
pub fn mark_decorative(doc: &mut Document, obj: ObjRef) -> Result<(), AltError> {
    mark_decorative_with_budget(doc, obj, &mut crate::artifact::ArtifactBudget::default())
}

/// The same figure-validated conversion with a transaction-wide consumptive allowance.
pub fn mark_decorative_with_budget(doc: &mut Document, obj: ObjRef, budget: &mut crate::artifact::ArtifactBudget) -> Result<(), AltError> {
    check_figure(doc, obj)?;
    crate::artifact::convert_with_budget(doc, obj, false, budget)?;
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn geometry_decode_budget_is_aggregate_and_never_returns_partial_content() {
        let mut doc = Document::new_empty();
        let a = doc.add(Object::Stream(pdfcraft_cos::Stream::flate(Dict::new(), b"1234567890")));
        let b = doc.add(Object::Stream(pdfcraft_cos::Stream::flate(Dict::new(), b"abcdefghij")));
        let mut page = Dict::new();
        page.set(b"Contents", Object::Array(vec![Object::Ref(a), Object::Ref(b)]));
        let mut enough = 22;
        assert_eq!(page_content(&doc, &page, &mut enough).unwrap(), b"1234567890\nabcdefghij\n");
        assert_eq!(enough, 0);
        let mut short = 21;
        assert!(page_content(&doc, &page, &mut short).is_none());
        assert_eq!(short, 10, "only first stream was decoded successfully");
        assert!(content_bbox(&doc, &page, &HashSet::new(), &mut 21).is_none());
    }
    #[test]
    fn geometry_structure_budget_counts_non_stream_containers_and_cycles() {
        let mut doc = Document::new_empty();
        let empty = doc.add(Object::Array(Vec::new()));
        let mut page = Dict::new();
        page.set(b"Contents", Object::Array(vec![Object::Ref(empty); BBOX_OBJECT_LIMIT]));
        assert!(page_content(&doc, &page, &mut { BBOX_DECODE_BUDGET }).is_none());
        doc.set(empty, Object::Array(vec![Object::Ref(empty)]));
        page.set(b"Contents", Object::Ref(empty));
        assert!(page_content(&doc, &page, &mut { BBOX_DECODE_BUDGET }).is_none());
    }
    #[test]
    fn geometry_refuses_malformed_filters_and_raw_over_budget_before_copying() {
        let mut doc = Document::new_empty();
        let mut dict = Dict::new();
        dict.set(b"Filter", Object::name("ASCIIHexDecode"));
        let corrupt = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(dict, b"71GG>".to_vec())));
        let mut page = Dict::new();
        page.set(b"Contents", Object::Ref(corrupt));
        assert!(page_content(&doc, &page, &mut 100).is_none());
        doc.set(corrupt, Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), vec![b'x'; 32])));
        assert!(page_content(&doc, &page, &mut 16).is_none());
        let mut malformed = Dict::new();
        malformed.set(b"Filter", Object::Bool(true));
        doc.set(corrupt, Object::Stream(pdfcraft_cos::Stream::from_raw(malformed, b"q Q".to_vec())));
        assert!(page_content(&doc, &page, &mut 100).is_none());
    }
}
