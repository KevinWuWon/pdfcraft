//! Add alternate text: the document's figures one by one, their alternate text, and marking a
//! figure as decorative (its content becomes an artifact and its tag goes away).

use std::collections::{HashMap, HashSet};

use pdfcraft_content::Matrix;
use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString};

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

fn tree_root(doc: &Document) -> Option<Dict> {
    let cat = doc.root().and_then(|r| doc.get(r).as_dict().cloned())?;
    cat.get(b"StructTreeRoot").map(|o| doc.resolve(o)).and_then(|o| o.as_dict().cloned())
}

/// The standard type of a structure type through the role map.
fn standard(doc: &Document, role_map: &Dict, s: &[u8]) -> Vec<u8> {
    let mut t = s.to_vec();
    for _ in 0..12 {
        if crate::structure::is_standard(&t) {
            break;
        }
        match role_map.get(&t).map(|o| doc.resolve(o)).and_then(|o| o.as_name().map(<[u8]>::to_vec)) {
            Some(n) if n != t => t = n,
            _ => break,
        }
    }
    t
}

/// Indirect structure elements in document order, with their page.
fn elements(doc: &Document) -> Vec<(ObjRef, Dict, Option<ObjRef>)> {
    let Some(root) = tree_root(doc) else { return Vec::new() };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(Object, Option<ObjRef>)> = root.get(b"K").map(|k| vec![(k.clone(), None)]).unwrap_or_default();
    while let Some((k, pg)) = stack.pop() {
        if out.len() > 1_000_000 {
            break;
        }
        match k {
            Object::Array(a) => stack.extend(a.into_iter().rev().map(|x| (x, pg))),
            Object::Ref(r) => {
                if !seen.insert(r) {
                    continue;
                }
                let Some(d) = doc.get(r).as_dict().cloned() else { continue };
                if d.contains(b"S") {
                    let pg = d.get(b"Pg").and_then(Object::as_ref).or(pg);
                    if let Some(kids) = d.get(b"K") {
                        stack.push((kids.clone(), pg));
                    }
                    out.push((r, d, pg));
                }
            }
            Object::Dict(d) if d.contains(b"S") => {
                // Direct elements can't be addressed; their indirect children still can.
                let pg = d.get(b"Pg").and_then(Object::as_ref).or(pg);
                if let Some(kids) = d.get(b"K") {
                    stack.push((kids.clone(), pg));
                }
            }
            _ => {}
        }
    }
    out
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

fn page_content(doc: &Document, page: &Dict) -> Vec<u8> {
    let Some(c) = page.get(b"Contents") else { return Vec::new() };
    match &*doc.resolve(c) {
        Object::Stream(s) => s.decoded().unwrap_or_default(),
        Object::Array(a) => a
            .iter()
            .flat_map(|x| match &*doc.resolve(x) {
                Object::Stream(s) => {
                    let mut v = s.decoded().unwrap_or_default();
                    v.push(b'\n');
                    v
                }
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn union(a: Option<[f64; 4]>, b: [f64; 4]) -> [f64; 4] {
    match a {
        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
        None => b,
    }
}

/// Where the marked content `ids` is drawn on a page (images, forms and paths).
fn content_bbox(doc: &Document, page: &Dict, ids: &HashSet<i64>) -> Option<[f64; 4]> {
    let res = page.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let xobjects = res.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
    let mut ctm = vec![Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])];
    let mut marks: Vec<bool> = Vec::new();
    let mut bbox = None;
    let mut path: Vec<(f64, f64)> = Vec::new();
    for op in pdfcraft_content::parse(&page_content(doc, page)).ops {
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
    let Some(root) = tree_root(doc) else { return Vec::new() };
    let role_map = root.get(b"RoleMap").map(|o| doc.resolve(o)).and_then(|o| o.as_dict().cloned()).unwrap_or_default();
    let pages = pdfcraft_model::pages(doc);
    let index: HashMap<ObjRef, usize> = pages.iter().enumerate().map(|(i, p)| (p.obj, i)).collect();
    elements(doc)
        .into_iter()
        .filter(|(_, d, _)| d.name(b"S").is_some_and(|s| standard(doc, &role_map, s) == b"Figure"))
        .map(|(obj, d, pg)| {
            let page = pg.and_then(|p| index.get(&p).copied());
            let alt = d.get(b"Alt").map(|o| doc.resolve(o)).and_then(|o| o.as_string().map(|s| s.to_text()));
            let ids: HashSet<i64> = mcids(&d).into_iter().collect();
            let bbox = page.and_then(|p| content_bbox(doc, &pages[p].dict, &ids));
            Figure { obj, page, alt, bbox }
        })
        .collect()
}

fn check_figure(doc: &Document, obj: ObjRef) -> Result<Dict, AltError> {
    let root = tree_root(doc).ok_or(AltError::Untagged)?;
    let role_map = root.get(b"RoleMap").map(|o| doc.resolve(o)).and_then(|o| o.as_dict().cloned()).unwrap_or_default();
    let d = doc.get(obj).as_dict().cloned().ok_or(AltError::NotAFigure(obj.num))?;
    match d.name(b"S") {
        Some(s) if standard(doc, &role_map, s) == b"Figure" => Ok(d),
        _ => Err(AltError::NotAFigure(obj.num)),
    }
}

/// Set (or, with `None` or blank text, remove) a figure's alternate text.
pub fn set_alt(doc: &mut Document, obj: ObjRef, alt: Option<&str>) -> Result<(), AltError> {
    check_figure(doc, obj)?;
    let alt = alt.map(str::trim).filter(|a| !a.is_empty());
    doc.update_dict(obj, |d| match alt {
        Some(a) => d.set(b"Alt".to_vec(), Object::String(PdfString::text(a))),
        None => {
            d.remove(b"Alt");
        }
    })?;
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
