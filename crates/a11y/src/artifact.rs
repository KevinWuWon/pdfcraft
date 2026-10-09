//! Conservative, atomic conversion of known marked content to artifacts.
//! Only BDC source spans change; all drawing bytes remain untouched. Page content is
//! copied on write, including shared streams. Forms must have one verified invocation.
use crate::tags::{self, DeleteMode, TagError, TagKid};
use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, TagError>;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const LIMIT: usize = 100_000;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
/// Cumulative decode/copy allowance shared by every artifact conversion in a transaction.
/// The default is 128 MiB. Charges remain consumed after a failed conversion.
#[derive(Debug)]
pub struct ArtifactBudget {
    remaining: usize,
}
impl Default for ArtifactBudget {
    fn default() -> Self {
        Self::new(MAX_TOTAL_BYTES)
    }
}
impl ArtifactBudget {
    /// Set a smaller allowance. Values above the default cap cannot expand it.
    pub fn new(limit: usize) -> Self {
        Self { remaining: limit.min(MAX_TOTAL_BYTES) }
    }
    fn charge(&mut self, size: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(size)
            .ok_or_else(|| invalid("artifact conversion exceeds its aggregate decoded/copy byte budget; select a smaller subtree"))?;
        Ok(())
    }
}
fn invalid(s: impl Into<String>) -> TagError {
    TagError::Invalid(s.into())
}
fn checked(doc: &Document, id: ObjRef) -> Result<std::sync::Arc<Object>> {
    if doc.generation(id.num) != id.generation {
        return Err(invalid("stale content object generation"));
    }
    Ok(doc.get(id))
}
fn decoded(s: &Stream, budget: &mut ArtifactBudget) -> Result<Vec<u8>> {
    if s.raw.len() > MAX_BYTES || s.dict.contains(b"F") {
        return Err(invalid("external or oversized content streams are unsupported"));
    }
    // Reject filter declarations which the tolerant COS filter accessor would ignore.
    if let Some(filter) = s.dict.get(b"Filter") {
        let valid = match filter {
            Object::Name(_) => true,
            Object::Array(a) => a.iter().all(|v| v.as_name().is_some()),
            _ => false,
        };
        if !valid {
            return Err(invalid("indirect or malformed content filter declaration"));
        }
    }
    let max = MAX_BYTES.min(budget.remaining);
    let chain = s.filters();
    let bytes = if chain.is_empty() {
        if s.raw.len() > max {
            return Err(invalid("artifact decoded content exceeds aggregate byte budget; select a smaller subtree"));
        }
        s.raw.as_ref().clone()
    } else {
        pdfcraft_filters::decode(&chain, &s.raw, max)
            .map_err(|e| invalid(format!("artifact stream failed strict bounded decoding (aggregate byte budget); select a smaller subtree: {e}")))?
    };
    budget.charge(bytes.len())?;
    Ok(bytes)
}
fn page_bytes_bounded(doc: &Document, d: &Dict, budget: &mut ArtifactBudget) -> Result<Vec<u8>> {
    let c = d.get(b"Contents").ok_or_else(|| invalid("tagged page has no content"))?;
    if let Some(r) = c.as_ref() {
        checked(doc, r)?;
    }
    let value = doc.resolve(c);
    let items = match &*value {
        Object::Array(a) => a.clone(),
        Object::Stream(_) => vec![c.clone()],
        _ => return Err(invalid("invalid page Contents")),
    };
    if items.len() > LIMIT {
        return Err(invalid("too many page streams"));
    }
    let mut out = Vec::new();
    for item in items {
        if let Some(r) = item.as_ref() {
            checked(doc, r)?;
        }
        let value = doc.resolve(&item);
        let Object::Stream(s) = &*value else { return Err(invalid("page Contents contains a non-stream")) };
        let bytes = decoded(s, budget)?;
        if out.len().saturating_add(bytes.len()).saturating_add(1) > MAX_BYTES {
            return Err(invalid("page content exceeds artifact editing limit"));
        }
        budget.charge(bytes.len().checked_add(1).ok_or_else(|| invalid("page content size overflow"))?)?;
        out.extend_from_slice(&bytes);
        out.push(b'\n');
    }
    Ok(out)
}
fn resource_dictionary(doc: &Document, value: &Object) -> Result<Dict> {
    let value = if let Some(r) = value.as_ref() { checked(doc, r)? } else { std::sync::Arc::new(value.clone()) };
    match &*value {
        Object::Dict(d) => Ok(d.clone()),
        _ => Err(invalid("resource container must be a direct dictionary or an exact dictionary reference")),
    }
}
fn resources(doc: &Document, d: &Dict) -> Result<Dict> {
    match d.get(b"Resources") {
        None => Ok(Dict::new()),
        Some(v) => resource_dictionary(doc, v),
    }
}

// An image cannot execute nested marked content. Its drawing invocation and stream stay
// byte-for-byte unchanged; independently tagged objects must not lose their association.
fn image_invocation(doc: &Document, res: &Dict, op: &pdfcraft_content::Op) -> Result<()> {
    let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed image XObject invocation"))?;
    let xobjects = resource_dictionary(doc, res.get(b"XObject").ok_or_else(|| invalid("missing XObject resource dictionary"))?)?;
    let image = xobjects.get(name).and_then(Object::as_ref).ok_or_else(|| invalid("image XObject must be an exact indirect stream reference"))?;
    let value = checked(doc, image)?;
    let Object::Stream(stream) = &*value else { return Err(invalid("image XObject is not a stream")) };
    let d = &stream.dict;
    if (d.contains(b"Type") && d.name(b"Type") != Some(b"XObject")) || d.name(b"Subtype") != Some(b"Image") {
        return Err(invalid("selected content invokes a Form or unsupported XObject; nested content ownership is not supported"));
    }
    if d.contains(b"StructParent") || d.contains(b"StructParents") {
        return Err(invalid("image XObject has its own structure association; independent ownership must be preserved"));
    }
    if d.int(b"Width").is_none_or(|n| n <= 0) || d.int(b"Height").is_none_or(|n| n <= 0) {
        return Err(invalid("image XObject has invalid dimensions"));
    }
    let mask = match d.get(b"ImageMask") {
        None | Some(Object::Bool(false)) => false,
        Some(Object::Bool(true)) => true,
        _ => return Err(invalid("image XObject has invalid ImageMask")),
    };
    if mask {
        if d.contains(b"ColorSpace") || (d.contains(b"BitsPerComponent") && d.int(b"BitsPerComponent") != Some(1)) {
            return Err(invalid("image mask has invalid color properties"));
        }
    } else if !matches!(d.int(b"BitsPerComponent"), Some(1 | 2 | 4 | 8 | 16)) || !d.contains(b"ColorSpace") {
        return Err(invalid("image XObject requires explicit valid bit depth and color space"));
    }
    Ok(())
}

fn parse(bytes: &[u8]) -> Result<pdfcraft_content::Parsed> {
    let parsed = pdfcraft_content::parse(bytes);
    if parsed.skipped != 0 || parsed.ops.len() > LIMIT || parsed.ops.iter().any(|op| op.is("BI")) {
        return Err(invalid("malformed, oversized, or inline-image content cannot be safely rewritten"));
    }
    Ok(parsed)
}
fn properties(doc: &Document, res: &Dict, value: &Object) -> Result<Dict> {
    match value {
        Object::Dict(d) => Ok(d.clone()),
        Object::Name(name) => {
            let props = res.get(b"Properties").and_then(|v| doc.dict(v)).ok_or_else(|| invalid("missing named content Properties"))?;
            let value = props.get(name).ok_or_else(|| invalid("missing named marked-content property"))?;
            if let Some(r) = value.as_ref() {
                checked(doc, r)?;
            }
            doc.dict(value).ok_or_else(|| invalid("named marked-content property must be a dictionary"))
        }
        _ => Err(invalid("BDC property must be a dictionary or resource name")),
    }
}
// COS intentionally repairs damaged dictionaries. Editing must reject those repairs,
// duplicate keys and indirect operands rather than guessing the selected marking.
fn strict_operand(lx: &mut pdfcraft_cos::Lexer<'_>, bytes: &[u8], depth: usize) -> Result<()> {
    if depth > 128 {
        return Err(invalid("marked-content property nesting exceeds safe limit"));
    }
    lx.skip_ws();
    if bytes.get(lx.pos..lx.pos.saturating_add(2)) == Some(b"<<") {
        lx.pos += 2;
        let mut keys = BTreeSet::new();
        loop {
            lx.skip_ws();
            if bytes.get(lx.pos..lx.pos.saturating_add(2)) == Some(b">>") {
                lx.pos += 2;
                break;
            }
            let key = lx.object().map_err(|e| invalid(e.to_string()))?;
            let Object::Name(key) = key else { return Err(invalid("malformed marked-content property key")) };
            if keys.len() >= LIMIT || !keys.insert(key) {
                return Err(invalid("duplicate or excessive marked-content property keys"));
            }
            strict_operand(lx, bytes, depth + 1)?;
        }
    } else if bytes.get(lx.pos) == Some(&b'[') {
        lx.pos += 1;
        let mut count = 0;
        loop {
            lx.skip_ws();
            if bytes.get(lx.pos) == Some(&b']') {
                lx.pos += 1;
                break;
            }
            count += 1;
            if count > LIMIT {
                return Err(invalid("oversized property array"));
            }
            strict_operand(lx, bytes, depth + 1)?;
        }
    } else {
        let v = lx.object().map_err(|e| invalid(e.to_string()))?;
        if matches!(v, Object::Ref(_)) {
            return Err(invalid("indirect content operands are unsupported"));
        }
    }
    Ok(())
}
type Replacement = (std::ops::Range<usize>, Vec<u8>);
fn rewrite(doc: &Document, bytes: &[u8], res: &Dict, wanted: &BTreeSet<i64>) -> Result<Vec<Replacement>> {
    let parsed = parse(bytes)?;
    let mut marks = Vec::new();
    let mut seen = BTreeSet::new();
    let mut edits = Vec::new();
    for op in parsed.ops {
        match op.op.as_slice() {
            b"BDC" => {
                if op.operands.len() != 2 || op.name(0).is_none() {
                    return Err(invalid("malformed BDC"));
                }
                let raw = bytes.get(op.span.clone()).ok_or_else(|| invalid("invalid BDC span"))?;
                let mut lx = pdfcraft_cos::Lexer::new(raw, 0);
                strict_operand(&mut lx, raw, 0)?;
                strict_operand(&mut lx, raw, 0)?;
                let mut props = properties(doc, res, op.operands.get(1).ok_or_else(|| invalid("missing BDC properties"))?)?;
                let mcid = props.int(b"MCID");
                if props.contains(b"MCID") && mcid.is_none_or(|n| n < 0) {
                    return Err(invalid("invalid BDC MCID"));
                }
                let target = mcid.is_some_and(|n| wanted.contains(&n));
                if marks.iter().any(|v| *v) || (target && !marks.is_empty()) {
                    return Err(invalid("nested marked content intersects the selected content"));
                }
                if target {
                    if op.name(0) == Some(b"OC") {
                        return Err(invalid("optional-content BDC cannot be changed to an artifact"));
                    }
                    if !seen.insert(mcid.ok_or_else(|| invalid("missing target MCID"))?) {
                        return Err(invalid("duplicate selected MCID in content namespace"));
                    }
                    props.remove(b"MCID");
                    let mut replacement = b"/Artifact ".to_vec();
                    pdfcraft_cos::serialize(&Object::Dict(props), &mut replacement);
                    replacement.extend_from_slice(b" BDC");
                    edits.push((op.span, replacement));
                }
                marks.push(target);
            }
            b"BMC" => {
                if op.operands.len() != 1 || op.name(0).is_none() || marks.iter().any(|v| *v) {
                    return Err(invalid("malformed or nested BMC intersects selected content"));
                }
                marks.push(false);
            }
            b"EMC" => {
                if !op.operands.is_empty() || marks.pop().is_none() {
                    return Err(invalid("unbalanced or malformed EMC"));
                }
            }
            b"Do" if marks.iter().any(|v| *v) => {
                image_invocation(doc, res, &op)?;
            }
            _ => {}
        }
        if marks.len() > 128 {
            return Err(invalid("marked-content nesting exceeds safe limit"));
        }
    }
    if !marks.is_empty() || &seen != wanted {
        return Err(invalid("unbalanced marks or selected MCID missing from exact content namespace"));
    }
    Ok(edits)
}
fn replace(bytes: &[u8], edits: Vec<Replacement>, budget: &mut ArtifactBudget) -> Result<Vec<u8>> {
    let mut size = bytes.len();
    let mut end = 0usize;
    for (span, replacement) in &edits {
        if span.start < end || span.end < span.start || span.end > bytes.len() {
            return Err(invalid("invalid replacement span"));
        }
        size = size
            .checked_sub(span.end - span.start)
            .and_then(|n| n.checked_add(replacement.len()))
            .ok_or_else(|| invalid("artifact output size overflow"))?;
        end = span.end;
    }
    budget.charge(size)?;
    let mut out = Vec::with_capacity(size);
    let mut cursor = 0;
    for (span, replacement) in edits {
        out.extend_from_slice(bytes.get(cursor..span.start).ok_or_else(|| invalid("invalid operator span"))?);
        out.extend_from_slice(&replacement);
        cursor = span.end;
    }
    out.extend_from_slice(bytes.get(cursor..).ok_or_else(|| invalid("invalid content tail"))?);
    Ok(out)
}

/// Convert a leaf tag, or explicitly its complete subtree, into artifacts. OBJR, ambiguous
/// nesting, inline images, and reused Forms are rejected. Failure leaves `doc` unchanged.
pub fn convert(doc: &mut Document, tag: ObjRef, include_descendants: bool) -> Result<()> {
    convert_with_budget(doc, tag, include_descendants, &mut ArtifactBudget::default())
}
/// Convert using the caller's transaction-wide allowance. A failure leaves `doc`
/// unchanged, but does not refund work already charged to `byte_budget`. Batch callers
/// must keep their own document snapshot and commit only after every edit succeeds.
pub fn convert_with_budget(doc: &mut Document, tag: ObjRef, include_descendants: bool, byte_budget: &mut ArtifactBudget) -> Result<()> {
    let tree = tags::inspect(doc)?;
    if !tree.issues.is_empty() {
        return Err(invalid(format!("structure is not safely editable: {}", tree.issues.join("; "))));
    }
    let selected = tree.element(tag).ok_or_else(|| invalid("selected tag does not exist"))?;
    if !include_descendants && selected.kids.iter().any(|k| matches!(k, TagKid::Element { .. })) {
        return Err(invalid("tag contains descendants; explicitly include its subtree"));
    }
    let mut selected_ids = BTreeSet::from([tag]);
    for e in &tree.elements {
        if include_descendants && e.parent.is_some_and(|p| selected_ids.contains(&p)) {
            selected_ids.insert(e.id);
        }
    }
    let mut groups: BTreeMap<(ObjRef, Option<ObjRef>), BTreeSet<i64>> = BTreeMap::new();
    for e in &tree.elements {
        if !selected_ids.contains(&e.id) {
            continue;
        }
        for kid in &e.kids {
            let (id, page, stream) = match kid {
                TagKid::Element { .. } => continue,
                TagKid::Mcid { id, page } => (*id, *page, None),
                TagKid::Mcr { id, page, stream } => (*id, *page, *stream),
                _ => return Err(invalid("artifact conversion supports only known MCID content, not OBJR or unsupported children")),
            };
            let page = page.ok_or_else(|| invalid("marked content has no known page"))?;
            if !groups.entry((page, stream)).or_default().insert(id) {
                return Err(invalid("duplicate selected content association"));
            }
        }
    }
    if groups.is_empty() {
        return Err(invalid("selected tag has no known marked content"));
    }
    let pages = pdfcraft_model::pages(doc);
    let forms: BTreeSet<_> = groups.keys().filter_map(|(_, s)| *s).collect();
    if !forms.is_empty() {
        verify_forms(doc, &pages, &forms, &groups, byte_budget)?;
    }
    let mut candidate = doc.clone();
    for ((page, stream), ids) in groups {
        let page = pages.iter().find(|p| p.obj == page).ok_or_else(|| invalid("content page not found"))?;
        if let Some(stream) = stream {
            let value = checked(doc, stream)?;
            let Object::Stream(s) = &*value else { return Err(invalid("MCR Stm is not a stream")) };
            let res = if s.dict.contains(b"Resources") { resources(doc, &s.dict)? } else { resources(doc, &page.dict)? };
            let source = decoded(s, byte_budget)?;
            let bytes = replace(&source, rewrite(doc, &source, &res, &ids)?, byte_budget)?;
            let mut d = s.dict.clone();
            for k in [b"Filter".as_slice(), b"DecodeParms", b"Length"] {
                d.remove(k);
            }
            candidate.set(stream, Object::Stream(Stream::from_raw(d, bytes)));
        } else {
            let source = page_bytes_bounded(doc, &page.dict, byte_budget)?;
            let edits = rewrite(doc, &source, &resources(doc, &page.dict)?, &ids)?;
            let original = page.dict.get(b"Contents").ok_or_else(|| invalid("missing Contents"))?;
            let resolved = doc.resolve(original);
            let array = matches!(&*resolved, Object::Array(_));
            let items = if let Object::Array(a) = &*resolved { a.clone() } else { vec![original.clone()] };
            let mut contents = Vec::new();
            let mut offset = 0usize;
            let mut applied = 0usize;
            for item in items {
                let value = doc.resolve(&item);
                let Object::Stream(s) = &*value else { return Err(invalid("invalid page stream")) };
                let data = decoded(s, byte_budget)?;
                let end = offset.saturating_add(data.len());
                let local: Vec<_> = edits
                    .iter()
                    .filter(|(span, _)| span.start >= offset && span.end <= end)
                    .map(|(span, value)| (span.start - offset..span.end - offset, value.clone()))
                    .collect();
                applied += local.len();
                if local.is_empty() {
                    contents.push(item);
                } else {
                    if s.dict.contains(b"StructParents")
                        || item.as_ref().is_some_and(|r| {
                            tree.elements.iter().any(|e| e.kids.iter().any(|k| matches!(k, TagKid::Mcr { stream: Some(stm), .. } if *stm == r)))
                        })
                    {
                        return Err(invalid("page stream has an explicit structure stream association; instance-aware remapping is required"));
                    }
                    let bytes = replace(&data, local, byte_budget)?;
                    let mut d = s.dict.clone();
                    for k in [b"Filter".as_slice(), b"DecodeParms", b"Length"] {
                        d.remove(k);
                    }
                    let content = candidate.add(Object::Stream(Stream::from_raw(d, bytes)));
                    contents.push(Object::Ref(content));
                }
                offset = end.saturating_add(1);
            }
            if applied != edits.len() {
                return Err(invalid("a selected BDC crosses a content-stream boundary; safe per-stream rewrite is unavailable"));
            }
            let contents = if array {
                if original.as_ref().is_some() { Object::Ref(candidate.add(Object::Array(contents))) } else { Object::Array(contents) }
            } else {
                contents.into_iter().next().ok_or_else(|| invalid("empty Contents"))?
            };
            candidate.update_dict(page.obj, |d| d.set(b"Contents", contents)).map_err(|e| invalid(e.to_string()))?;
        }
    }
    tags::delete(&mut candidate, tag, DeleteMode::Subtree)?;
    *doc = candidate;
    Ok(())
}

fn verify_forms(
    doc: &Document,
    pages: &[pdfcraft_model::Page],
    forms: &BTreeSet<ObjRef>,
    groups: &BTreeMap<(ObjRef, Option<ObjRef>), BTreeSet<i64>>,
    byte_budget: &mut ArtifactBudget,
) -> Result<()> {
    // For now require every reference to a target Form to be a structure MCR or a direct
    // page/Pages Resources/XObject entry. Indirect/shared resource containers, appearances,
    // patterns and soft masks need instance-aware copy-on-write, not stream replacement.
    let nums = doc.object_numbers();
    if nums.len() > LIMIT {
        return Err(invalid("too many objects to verify Form usage"));
    }
    let mut resource_refs = BTreeMap::<ObjRef, usize>::new();
    for num in nums {
        let value = doc.get(ObjRef::new(num, doc.generation(num)));
        let mut stack = vec![(&*value, Vec::<Vec<u8>>::new(), 0usize)];
        let root_type = value.as_dict().and_then(|d| d.name(b"Type"));
        let mut count = 0usize;
        while let Some((value, path, depth)) = stack.pop() {
            count += 1;
            if depth > 128 || count > LIMIT {
                return Err(invalid("Form reference graph exceeds safe limits"));
            }
            if let Object::Ref(r) = value {
                if forms.contains(r) {
                    let is_resource = matches!(root_type, Some(b"Page" | b"Pages"))
                        && path.len() == 3
                        && path.first().is_some_and(|k| k == b"Resources")
                        && path.get(1).is_some_and(|k| k == b"XObject");
                    let is_mcr = path.last().is_some_and(|k| k == b"Stm") && matches!(root_type, Some(b"MCR" | b"StructElem"));
                    if is_resource {
                        *resource_refs.entry(*r).or_default() += 1;
                    } else if !is_mcr {
                        return Err(invalid("Form has an unsupported or shared resource reference"));
                    }
                }
            } else if let Some(d) = value.as_dict() {
                if d.contains(b"AP") || d.contains(b"SMask") || d.contains(b"Pattern") || d.contains(b"StmOwn") {
                    return Err(invalid("Form conversion with appearances, soft masks or patterns is not supported"));
                }
                for (k, v) in d.iter() {
                    let mut next = path.clone();
                    next.push(k.clone());
                    stack.push((v, next, depth + 1));
                }
            } else if let Object::Array(a) = value {
                for v in a {
                    stack.push((v, path.clone(), depth + 1));
                }
            }
        }
    }
    if forms.iter().any(|s| resource_refs.get(s) != Some(&1)) {
        return Err(invalid("Form must have exactly one direct page resource reference"));
    }
    let mut usages = BTreeMap::<ObjRef, Vec<ObjRef>>::new();
    let mut budget = LIMIT;
    for page in pages {
        if !page.dict.contains(b"Contents") {
            continue;
        }
        let mut stack = vec![(page_bytes_bounded(doc, &page.dict, byte_budget)?, resources(doc, &page.dict)?, BTreeSet::new(), false)];
        while let Some((bytes, res, ancestors, inherited_mark)) = stack.pop() {
            let mut mark_depth = 0usize;
            for op in parse(&bytes)?.ops {
                budget = budget.checked_sub(1).ok_or_else(|| invalid("Form invocation scan exceeds safe limits"))?;
                if op.is("BDC") || op.is("BMC") {
                    mark_depth = mark_depth.saturating_add(1);
                    if mark_depth > 128 {
                        return Err(invalid("marked-content nesting exceeds safe limit"));
                    }
                    continue;
                }
                if op.is("EMC") {
                    mark_depth = mark_depth.checked_sub(1).ok_or_else(|| invalid("unbalanced marked content while checking Form usage"))?;
                    continue;
                }
                if !op.is("Do") {
                    continue;
                }
                let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed XObject invocation"))?;
                let xobjects = res.get(b"XObject").and_then(|v| doc.dict(v)).ok_or_else(|| invalid("missing XObject resources"))?;
                let r = xobjects.get(name).and_then(Object::as_ref).ok_or_else(|| invalid("XObject must be an indirect stream"))?;
                let value = checked(doc, r)?;
                let Object::Stream(s) = &*value else { return Err(invalid("XObject is not a stream")) };
                if s.dict.name(b"Subtype") != Some(b"Form") {
                    continue;
                }
                if ancestors.contains(&r) || ancestors.len() >= 128 {
                    return Err(invalid("cyclic or deeply nested Form invocation"));
                }
                if forms.contains(&r) {
                    if inherited_mark || mark_depth > 0 {
                        return Err(invalid("selected Form is nested in another marked-content scope"));
                    }
                    usages.entry(r).or_default().push(page.obj);
                }
                let mut ancestors = ancestors.clone();
                ancestors.insert(r);
                let child_res = if s.dict.contains(b"Resources") { resources(doc, &s.dict)? } else { res.clone() };
                stack.push((decoded(s, byte_budget)?, child_res, ancestors, inherited_mark || mark_depth > 0));
            }
            if mark_depth != 0 {
                return Err(invalid("unbalanced marked content while checking Form usage"));
            }
        }
    }
    for (page, stream) in groups.keys() {
        if let Some(s) = stream
            && usages.get(s).is_none_or(|u| u.as_slice() != [*page])
        {
            return Err(invalid("Form must be invoked exactly once on its associated page; reused Forms require instance isolation"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn convert_bounded(doc: &mut Document, tag: ObjRef, descendants: bool, max_bytes: usize) -> Result<()> {
        convert_with_budget(doc, tag, descendants, &mut ArtifactBudget::new(max_bytes))
    }
    fn page_bytes(doc: &Document, d: &Dict) -> Result<Vec<u8>> {
        page_bytes_bounded(doc, d, &mut ArtifactBudget::new(MAX_TOTAL_BYTES))
    }
    fn fixture(content: &[u8]) -> (Document, ObjRef, ObjRef, ObjRef, ObjRef) {
        let mut doc = Document::new_empty();
        let top = tags::create(&mut doc, None, 0, &tags::TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
        let a = tags::create(&mut doc, Some(top), 0, &tags::TagProperties { role: Some("P".into()), ..Default::default() }).unwrap();
        let b = tags::create(&mut doc, Some(top), 1, &tags::TagProperties { role: Some("P".into()), ..Default::default() }).unwrap();
        let stream = doc.add(Object::Stream(Stream::from_raw(Dict::new(), content.to_vec())));
        let pages = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
        let mut refs = Vec::new();
        for (i, tag) in [a, b].into_iter().enumerate() {
            let mut d = Dict::new();
            d.set(b"Type", Object::name("Page"));
            d.set(b"Parent", Object::Ref(pages));
            d.set(b"MediaBox", Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]));
            d.set(b"StructParents", Object::Int(i as i64));
            d.set(b"Contents", Object::Ref(stream));
            let page = doc.add(d);
            doc.update_dict(tag, |d| {
                d.set(b"Pg", Object::Ref(page));
                d.set(b"K", Object::Int(0));
            })
            .unwrap();
            refs.push(page);
        }
        doc.update_dict(pages, |d| {
            d.set(b"Kids", Object::Array(refs.iter().copied().map(Object::Ref).collect()));
            d.set(b"Count", 2);
        })
        .unwrap();
        let root = tags::inspect(&doc).unwrap().root.unwrap();
        let mut pt = Dict::new();
        pt.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(a)]), 1.into(), Object::Array(vec![Object::Ref(b)])]));
        doc.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(pt))).unwrap();
        (doc, a, b, refs[0], refs[1])
    }
    fn bytes(doc: &Document) -> Vec<u8> {
        pdfcraft_cos::write_full(doc, &Default::default()).unwrap()
    }
    fn page_content(doc: &Document, page: ObjRef) -> Vec<u8> {
        page_bytes(doc, doc.get(page).as_dict().unwrap()).unwrap()
    }
    #[test]
    fn exact_page_copy_on_write_preserves_other_page_and_roundtrips() {
        let input = b"% keep this\n/P <</MCID 0 /Private (keep)>> BDC\n0 0 10 10 re f\nEMC\n% tail";
        let (mut doc, a, b, page, other) = fixture(input);
        let original_other = page_content(&doc, other);
        convert(&mut doc, a, false).unwrap();
        assert_eq!(page_content(&doc, other), original_other);
        assert!(tags::inspect(&doc).unwrap().element(b).is_some());
        assert!(tags::inspect(&doc).unwrap().element(a).is_none());
        let rewritten = page_content(&doc, page);
        assert!(rewritten.windows(9).any(|w| w == b"/Artifact"));
        assert!(rewritten.windows(b"\n0 0 10 10 re f\nEMC\n% tail".len()).any(|w| w == b"\n0 0 10 10 re f\nEMC\n% tail"));
        assert!(tags::inspect(&doc).unwrap().issues.is_empty());
        let reopened = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
        assert!(tags::inspect(&reopened).unwrap().issues.is_empty());
        assert_eq!(
            pdfcraft_content::parse(&page_content(&reopened, pdfcraft_model::pages(&reopened)[0].obj)).ops[0].name(0),
            Some(b"Artifact".as_slice())
        );
    }
    #[test]
    fn named_shared_properties_are_copied_inline_and_inherited_resources_resolved() {
        let (mut doc, a, _, page, other) = fixture(b"/P /Shared BDC 1 2 3 4 re f EMC");
        let mut property = Dict::new();
        property.set(b"MCID", 0);
        property.set(b"Private", Object::name("preserved"));
        let property = doc.add(property);
        let mut props = Dict::new();
        props.set(b"Shared", Object::Ref(property));
        let mut res = Dict::new();
        res.set(b"Properties", props);
        let pages = doc.get(page).as_dict().unwrap().reference(b"Parent").unwrap();
        doc.update_dict(pages, |d| d.set(b"Resources", res)).unwrap();
        let before = doc.get(property);
        let other_before = page_content(&doc, other);
        convert(&mut doc, a, false).unwrap();
        assert_eq!(doc.get(property), before);
        assert_eq!(page_content(&doc, other), other_before);
        let content = page_content(&doc, page);
        let parsed = pdfcraft_content::parse(&content);
        let d = parsed.ops[0].operands[1].as_dict().unwrap();
        assert!(!d.contains(b"MCID"));
        assert_eq!(d.name(b"Private"), Some(b"preserved".as_slice()));
    }
    #[test]
    fn content_arrays_are_one_namespace_even_when_marks_cross_stream_boundary() {
        let (mut doc, a, _, page, _) = fixture(b"unused");
        let s1 = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/P <</MCID 0>> BDC 0 0".to_vec())));
        let s2 = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"10 10 re f EMC".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Array(vec![Object::Ref(s1), Object::Ref(s2)]))).unwrap();
        let original = doc.get(s1);
        convert(&mut doc, a, false).unwrap();
        assert_eq!(doc.get(s1), original);
        assert!(page_content(&doc, page).windows(9).any(|w| w == b"/Artifact"));
    }
    #[test]
    fn malformed_missing_duplicate_nested_and_xobject_cases_are_atomic() {
        for content in [
            b"/P <</MCID 0>> BDC 0 0 1 1 re f".as_slice(),
            b"/P <</MCID 1>> BDC EMC",
            b"/P <</MCID 0>> BDC EMC /P <</MCID 0>> BDC EMC",
            b"/P <</MCID 0>> BDC /Span BMC EMC EMC",
            b"/Span BMC /P <</MCID 0>> BDC EMC EMC",
            b"/P <</MCID 0>> BDC /X Do EMC",
            b"EMC /P <</MCID 0>> BDC EMC",
            b"/OC <</MCID 0>> BDC EMC",
            b"/P <</MCID 0>> BDC EMC )",
            b"/P /Missing BDC EMC",
            b"/P <</MCID 9 /MCID 0>> BDC EMC",
            b"/P <</MCID 0 /Bad >> BDC EMC",
            b"/P <</MCID 0 garbage>> BDC EMC",
            b"/P <</MCID 0>> BDC BI ID x EI EMC",
        ] {
            let (mut doc, a, _, _, _) = fixture(content);
            let before = bytes(&doc);
            assert!(convert(&mut doc, a, false).is_err(), "{:?}", String::from_utf8_lossy(content));
            assert_eq!(bytes(&doc), before);
        }
    }
    fn form_fixture() -> (Document, ObjRef, ObjRef, ObjRef, ObjRef) {
        let (mut doc, a, _, page, other) = fixture(b"/P <</MCID 0>> BDC 4 5 6 7 re f EMC");
        let mut fd = Dict::new();
        fd.set(b"Type", Object::name("XObject"));
        fd.set(b"Subtype", Object::name("Form"));
        fd.set(b"StructParents", 2);
        fd.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]));
        let form = doc.add(Object::Stream(Stream::from_raw(fd, b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC".to_vec())));
        let mut xobj = Dict::new();
        xobj.set(b"F", Object::Ref(form));
        let mut res = Dict::new();
        res.set(b"XObject", xobj);
        let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/F Do".to_vec())));
        doc.update_dict(page, |d| {
            d.set(b"Resources", res);
            d.set(b"Contents", Object::Ref(content));
        })
        .unwrap();
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", 0);
        mcr.set(b"Stm", Object::Ref(form));
        doc.update_dict(a, |d| d.set(b"K", mcr)).unwrap();
        let root = tags::inspect(&doc).unwrap().root.unwrap();
        let mut pt = doc.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
        let mut nums = pt.get(b"Nums").unwrap().as_array().unwrap().to_vec();
        nums[1] = Object::Array(vec![Object::Null]);
        nums.extend([2.into(), Object::Array(vec![Object::Ref(a)])]);
        pt.set(b"Nums", Object::Array(nums));
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        assert!(tags::inspect(&doc).unwrap().issues.is_empty());
        (doc, a, page, other, form)
    }
    #[test]
    fn exact_form_namespace_does_not_touch_same_mcid_on_another_page() {
        let (mut doc, a, page, other, form) = form_fixture();
        let page_before = page_content(&doc, page);
        let other_before = page_content(&doc, other);
        convert(&mut doc, a, false).unwrap();
        assert_eq!(page_content(&doc, page), page_before);
        assert_eq!(page_content(&doc, other), other_before);
        let value = doc.get(form);
        let Object::Stream(s) = &*value else { panic!() };
        assert!(s.decoded().unwrap().starts_with(b"/Artifact"));
        let reopened = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
        assert!(tags::inspect(&reopened).unwrap().issues.is_empty());
    }
    #[test]
    fn reused_forms_reject_without_mutation() {
        let (mut doc, a, page, _, _) = form_fixture();
        let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/F Do /F Do".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(content))).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn subtree_requires_explicit_opt_in_and_late_delete_failure_is_atomic() {
        let (mut doc, a, _, _, _) = fixture(b"/P <</MCID 0>> BDC EMC");
        let child = tags::create(&mut doc, Some(a), 0, &tags::TagProperties { role: Some("Span".into()), ..Default::default() }).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
        convert(&mut doc, a, true).unwrap();
        assert!(tags::inspect(&doc).unwrap().element(child).is_none());
        let (mut doc, a, _, _, _) = fixture(b"/P <</MCID 0>> BDC EMC");
        let root = tags::inspect(&doc).unwrap().root.unwrap();
        doc.update_dict(root, |d| d.set(b"IDTree", Dict::new())).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn array_copy_preserves_stream_metadata_and_untouched_stream_identity() {
        let (mut doc, a, _, page, _) = fixture(b"unused");
        let mut d = Dict::new();
        d.set(b"Private", Object::name("keep"));
        let first = doc.add(Object::Stream(Stream::from_raw(d, b"/P <</MCID 0>> BDC 0 0 1 1 re f".to_vec())));
        let last = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"EMC".to_vec())));
        let array = doc.add(Object::Array(vec![Object::Ref(first), Object::Ref(last)]));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(array))).unwrap();
        convert(&mut doc, a, false).unwrap();
        let new_array = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
        assert_ne!(new_array, array);
        let value = doc.get(new_array);
        let refs = value.as_array().unwrap();
        assert_eq!(refs[1].as_ref(), Some(last));
        assert_ne!(refs[0].as_ref(), Some(first));
        let value = doc.resolve(&refs[0]);
        let Object::Stream(s) = &*value else { panic!() };
        assert_eq!(s.dict.name(b"Private"), Some(b"keep".as_slice()));
    }
    #[test]
    fn explicit_page_stream_association_rejects_copy_without_retargeting() {
        let (mut doc, a, b, page, _) = fixture(b"/P <</MCID 0>> BDC EMC /P <</MCID 1>> BDC EMC");
        let stream = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
        let mut value = (*doc.get(stream)).clone();
        value.as_dict_mut().unwrap().set(b"StructParents", 2);
        doc.set(stream, value);
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", 1);
        mcr.set(b"Stm", Object::Ref(stream));
        mcr.set(b"Pg", Object::Ref(page));
        doc.update_dict(b, |d| d.set(b"K", mcr)).unwrap();
        let root = tags::inspect(&doc).unwrap().root.unwrap();
        let mut pt = Dict::new();
        pt.set(
            b"Nums",
            Object::Array(vec![
                0.into(),
                Object::Array(vec![Object::Ref(a)]),
                1.into(),
                Object::Array(vec![Object::Null]),
                2.into(),
                Object::Array(vec![Object::Null, Object::Ref(b)]),
            ]),
        );
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        assert!(tags::inspect(&doc).unwrap().issues.is_empty());
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn form_generation_is_exact_and_stale_reference_rejects() {
        let (mut doc, a, page, _, form) = form_fixture();
        let generation = ObjRef::new(form.num, 7);
        doc.set(generation, (*doc.get(form)).clone());
        doc.update_dict(a, |d| {
            let mut k = d.get(b"K").unwrap().as_dict().unwrap().clone();
            k.set(b"Stm", Object::Ref(generation));
            d.set(b"K", k);
        })
        .unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
        let mut x = Dict::new();
        x.set(b"F", Object::Ref(generation));
        let mut res = Dict::new();
        res.set(b"XObject", x);
        doc.update_dict(page, |d| d.set(b"Resources", res)).unwrap();
        convert(&mut doc, a, false).unwrap();
        assert_eq!(doc.generation(form.num), 7);
        let value = doc.get(generation);
        let Object::Stream(s) = &*value else { panic!() };
        assert!(s.decoded().unwrap().starts_with(b"/Artifact"));
    }
    #[test]
    fn form_nested_in_page_mark_and_split_bdc_are_rejected_atomically() {
        let (mut doc, a, page, _, _) = form_fixture();
        let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/Span BMC /F Do EMC".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(content))).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
        let (mut doc, a, _, page, _) = fixture(b"unused");
        let s1 = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/P <</MCID 0>>".to_vec())));
        let s2 = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"BDC EMC".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Array(vec![Object::Ref(s1), Object::Ref(s2)]))).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, a, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn identical_mcid_in_different_forms_remains_isolated() {
        let (mut doc, a, _, other, _) = form_fixture();
        let tree = tags::inspect(&doc).unwrap();
        let b = tree.elements.iter().find(|e| e.id != a && e.page == Some(other)).unwrap().id;
        let mut fd = Dict::new();
        fd.set(b"Type", Object::name("XObject"));
        fd.set(b"Subtype", Object::name("Form"));
        fd.set(b"StructParents", 3);
        let second = doc.add(Object::Stream(Stream::from_raw(fd, b"/P <</MCID 0>> BDC 7 8 9 10 re f EMC".to_vec())));
        let mut x = Dict::new();
        x.set(b"F", Object::Ref(second));
        let mut res = Dict::new();
        res.set(b"XObject", x);
        let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/F Do".to_vec())));
        doc.update_dict(other, |d| {
            d.set(b"Resources", res);
            d.set(b"Contents", Object::Ref(content));
        })
        .unwrap();
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", 0);
        mcr.set(b"Stm", Object::Ref(second));
        doc.update_dict(b, |d| d.set(b"K", mcr)).unwrap();
        let root = tree.root.unwrap();
        let mut pt = doc.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
        let mut nums = pt.get(b"Nums").unwrap().as_array().unwrap().to_vec();
        nums[3] = Object::Array(vec![Object::Null]);
        nums.extend([3.into(), Object::Array(vec![Object::Ref(b)])]);
        pt.set(b"Nums", Object::Array(nums));
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        assert!(tags::inspect(&doc).unwrap().issues.is_empty());
        let before = doc.get(second);
        convert(&mut doc, a, false).unwrap();
        assert_eq!(doc.get(second), before);
        assert!(tags::inspect(&doc).unwrap().issues.is_empty());
    }
    fn image_fixture() -> (Document, ObjRef, ObjRef, ObjRef) {
        let (mut doc, tag, _, page, _) = fixture(b"/Figure <</MCID 0>> BDC q 20 0 0 20 3 4 cm /Im Do Q EMC");
        let mut d = Dict::new();
        d.set(b"Type", Object::name("XObject"));
        d.set(b"Subtype", Object::name("Image"));
        d.set(b"Width", 1);
        d.set(b"Height", 1);
        d.set(b"BitsPerComponent", 8);
        d.set(b"ColorSpace", Object::name("DeviceRGB"));
        let image = doc.add(Object::Stream(Stream::from_raw(d, vec![230, 40, 10])));
        let mut x = Dict::new();
        x.set(b"Im", Object::Ref(image));
        let x = doc.add(x);
        let mut resources = Dict::new();
        resources.set(b"XObject", Object::Ref(x));
        let resources = doc.add(resources);
        doc.update_dict(page, |d| d.set(b"Resources", Object::Ref(resources))).unwrap();
        (doc, tag, page, image)
    }
    #[test]
    fn image_do_preserves_drawing_operators_raw_pixels_and_image_object() {
        let (mut doc, tag, page, image) = image_fixture();
        let pixels = doc.get(image);
        let before = pdfcraft_content::parse(&page_content(&doc, page))
            .ops
            .into_iter()
            .filter(|op| !op.is("BDC"))
            .map(|op| (op.op, op.operands))
            .collect::<Vec<_>>();
        convert(&mut doc, tag, false).unwrap();
        let after = pdfcraft_content::parse(&page_content(&doc, page))
            .ops
            .into_iter()
            .filter(|op| !op.is("BDC"))
            .map(|op| (op.op, op.operands))
            .collect::<Vec<_>>();
        assert_eq!(after, before);
        assert_eq!(doc.get(image), pixels);
        let reopened = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
        assert!(tags::inspect(&reopened).unwrap().issues.is_empty());
        let page = &pdfcraft_model::pages(&reopened)[0];
        let res = resources(&reopened, &page.dict).unwrap();
        let ops = pdfcraft_content::parse(&page_content(&reopened, page.obj));
        image_invocation(&reopened, &res, ops.ops.iter().find(|op| op.is("Do")).unwrap()).unwrap();
    }
    #[test]
    fn selected_image_do_rejects_form_stale_generation_invalid_dimensions_and_own_tags() {
        for case in 0..4 {
            let (mut doc, tag, _, image) = image_fixture();
            let mut value = (*doc.get(image)).clone();
            let d = value.as_dict_mut().unwrap();
            match case {
                0 => d.set(b"Subtype", Object::name("Form")),
                1 => d.set(b"StructParent", 8),
                2 => d.set(b"Width", 0),
                _ => {}
            }
            doc.set(if case == 3 { ObjRef::new(image.num, 2) } else { image }, value);
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn image_type_is_optional_but_present_wrong_type_rejects_atomically() {
        let (mut doc, tag, _, image) = image_fixture();
        let mut value = (*doc.get(image)).clone();
        value.as_dict_mut().unwrap().remove(b"Type");
        doc.set(image, value);
        let before_image = doc.get(image);
        convert(&mut doc, tag, false).unwrap();
        assert_eq!(doc.get(image), before_image);
        let (mut doc, tag, _, image) = image_fixture();
        let mut value = (*doc.get(image)).clone();
        value.as_dict_mut().unwrap().set(b"Type", Object::name("Page"));
        doc.set(image, value);
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn aggregate_compressed_subtree_budget_rejects_without_partial_commit() {
        let mut content = b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC".to_vec();
        content.resize(4096, b' ');
        let (mut doc, tag, _, page, _) = fixture(&content);
        let stream = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
        doc.set(stream, Object::Stream(Stream::flate(Dict::new(), &content)));
        let top = tags::inspect(&doc).unwrap().element(tag).unwrap().parent.unwrap();
        let before = bytes(&doc);
        // One page fits (including decode, concatenation, second pass and rewritten output),
        // but the second selected namespace exhausts the same transaction-wide allowance.
        let mut single = doc.clone();
        convert_bounded(&mut single, tag, false, 20_000).unwrap();
        let error = convert_bounded(&mut doc, top, true, 20_000).unwrap_err().to_string();
        assert!(error.contains("budget"), "{error}");
        assert_eq!(bytes(&doc), before);
        convert_bounded(&mut doc, top, true, 40_000).unwrap();
    }
    #[test]
    fn form_scan_bounds_total_pending_decoded_streams() {
        let (mut doc, _, page, _, target) = form_fixture();
        let target_value = doc.get(target);
        let Object::Stream(target_stream) = &*target_value else { panic!() };
        let mut content = b"/P <</MCID 0>> BDC EMC".to_vec();
        content.resize(4096, b' ');
        doc.set(target, Object::Stream(Stream::flate(target_stream.dict.clone(), &content)));
        let mut other_dict = target_stream.dict.clone();
        other_dict.remove(b"StructParents");
        let second = doc.add(Object::Stream(Stream::flate(other_dict.clone(), &content)));
        let third = doc.add(Object::Stream(Stream::flate(other_dict, &content)));
        let mut x = Dict::new();
        x.set(b"F", Object::Ref(target));
        x.set(b"G", Object::Ref(second));
        x.set(b"H", Object::Ref(third));
        let mut res = Dict::new();
        res.set(b"XObject", x);
        let contents = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/F Do /G Do /H Do".to_vec())));
        doc.update_dict(page, |d| {
            d.set(b"Resources", res);
            d.set(b"Contents", Object::Ref(contents));
        })
        .unwrap();
        let groups = BTreeMap::from([((page, Some(target)), BTreeSet::from([0]))]);
        let forms = BTreeSet::from([target]);
        let before = bytes(&doc);
        let mut budget = ArtifactBudget::new(9000);
        let error = verify_forms(&doc, &pdfcraft_model::pages(&doc), &forms, &groups, &mut budget).unwrap_err().to_string();
        assert!(error.contains("budget"), "{error}");
        assert!(budget.remaining < 1000);
        assert_eq!(bytes(&doc), before);
        verify_forms(&doc, &pdfcraft_model::pages(&doc), &forms, &groups, &mut ArtifactBudget::new(20_000)).unwrap();
    }
    #[test]
    fn replacement_reserves_output_before_allocation_and_checks_size_overflow() {
        let mut budget = ArtifactBudget::new(4);
        assert!(replace(b"abcd", vec![(0..1, b"lengthy".to_vec())], &mut budget).is_err());
        assert_eq!(budget.remaining, 4);
        assert!(budget.charge(usize::MAX).is_err());
        assert_eq!(budget.remaining, 4);
    }
    #[test]
    fn shared_transaction_budget_exhausts_across_independent_conversions() {
        let mut content = b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC".to_vec();
        content.resize(4096, b' ');
        let (doc, first, second, page, _) = fixture(&content);
        let mut doc = doc;
        let stream = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
        doc.set(stream, Object::Stream(Stream::flate(Dict::new(), &content)));
        let before = bytes(&doc);
        let mut candidate = doc.clone();
        let mut shared = ArtifactBudget::new(20_000);
        convert_with_budget(&mut candidate, first, false, &mut shared).unwrap();
        let first_result = bytes(&candidate);
        let error = convert_with_budget(&mut candidate, second, false, &mut shared).unwrap_err().to_string();
        assert!(error.contains("budget"), "{error}");
        assert_eq!(bytes(&candidate), first_result);
        assert_eq!(bytes(&doc), before);
        // Each operation would fit a fresh allowance, proving the second refusal is cumulative.
        convert_with_budget(&mut candidate, second, false, &mut ArtifactBudget::new(20_000)).unwrap();
        assert_eq!(ArtifactBudget::new(usize::MAX).remaining, MAX_TOTAL_BYTES);
        assert_eq!(ArtifactBudget::default().remaining, MAX_TOTAL_BYTES);
    }
}
