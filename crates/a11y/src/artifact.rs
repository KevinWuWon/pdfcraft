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
/// Cumulative decode/output allowance shared by every artifact conversion in a transaction.
/// The default is 128 MiB, with one million aggregate graph/operator work units.
/// Charges remain consumed after a failed conversion.
#[derive(Debug)]
pub struct ArtifactBudget {
    remaining: usize,
    work_remaining: usize,
}
impl Default for ArtifactBudget {
    fn default() -> Self {
        Self::new(MAX_TOTAL_BYTES)
    }
}
impl ArtifactBudget {
    /// Set a smaller allowance. Values above the default cap cannot expand it.
    pub fn new(limit: usize) -> Self {
        Self { remaining: limit.min(MAX_TOTAL_BYTES), work_remaining: 1_000_000 }
    }
    fn work(&mut self, count: usize) -> Result<()> {
        self.work_remaining = self
            .work_remaining
            .checked_sub(count)
            .ok_or_else(|| invalid("artifact conversion exceeds its aggregate graph/operator work budget; select a smaller subtree"))?;
        Ok(())
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
    doc.try_get(id.num).map_err(|error| invalid(format!("cannot read exact content object {id}: {error}")))
}
fn exact_value(doc: &Document, value: &Object) -> Result<std::sync::Arc<Object>> {
    match value {
        Object::Ref(id) => checked(doc, *id),
        _ => Ok(std::sync::Arc::new(value.clone())),
    }
}
pub(crate) fn validate_decode_parameters(stream: &Stream, budget: &mut ArtifactBudget) -> Result<()> {
    budget.work(match stream.dict.get(b"Filter") {
        Some(Object::Array(items)) => items.len(),
        Some(_) => 1,
        None => 0,
    })?;
    let filters: Vec<&[u8]> = match stream.dict.get(b"Filter") {
        None => Vec::new(),
        Some(Object::Name(name)) => vec![name],
        Some(Object::Array(items)) if items.iter().all(|value| value.as_name().is_some()) => items.iter().filter_map(Object::as_name).collect(),
        _ => return Err(invalid("indirect or malformed content filter declaration")),
    };
    // COS applies Crypt while reading objects; this editor cannot prove a separately
    // named crypt filter's parameters, so it must not silently drop that declaration.
    if filters.contains(&b"Crypt".as_slice()) {
        return Err(invalid("explicit Crypt content filters require security-handler-aware rewriting"));
    }
    let parameters: Vec<Option<&Dict>> = match stream.dict.get(b"DecodeParms") {
        None | Some(Object::Null) => return Ok(()),
        Some(Object::Dict(dict)) if filters.len() == 1 && matches!(stream.dict.get(b"Filter"), Some(Object::Name(_))) => vec![Some(dict)],
        Some(Object::Array(items)) if items.len() == filters.len() && matches!(stream.dict.get(b"Filter"), Some(Object::Array(_))) => {
            budget.work(items.len())?;
            let mut result = Vec::new();
            for item in items {
                match item {
                    Object::Null => result.push(None),
                    Object::Dict(dict) => result.push(Some(dict)),
                    _ => return Err(invalid("indirect or malformed DecodeParms array entry")),
                }
            }
            result
        }
        _ => return Err(invalid("indirect, malformed or mismatched content DecodeParms")),
    };
    for (filter, parameters) in filters.iter().zip(parameters) {
        let Some(parameters) = parameters else { continue };
        budget.work(parameters.len())?;
        for (key, value) in parameters.iter() {
            let allowed = match *filter {
                b"FlateDecode" | b"Fl" => matches!(key.as_slice(), b"Predictor" | b"Colors" | b"BitsPerComponent" | b"Columns"),
                b"LZWDecode" | b"LZW" => matches!(key.as_slice(), b"Predictor" | b"Colors" | b"BitsPerComponent" | b"Columns" | b"EarlyChange"),
                _ => false,
            };
            let valid = match (key.as_slice(), value) {
                (b"Predictor", Object::Int(1 | 2 | 10..=15)) => true,
                (b"Colors", Object::Int(value)) => (1..=255).contains(value),
                (b"Columns", Object::Int(value)) => *value > 0,
                (b"BitsPerComponent", Object::Int(1 | 2 | 4 | 8 | 16)) => true,
                (b"EarlyChange", Object::Int(0 | 1)) => true,
                _ => false,
            };
            if !allowed || !valid {
                return Err(invalid("unsupported, malformed or non-integer content decode parameter"));
            }
        }
    }
    Ok(())
}
fn decoded(s: &Stream, budget: &mut ArtifactBudget) -> Result<Vec<u8>> {
    if s.raw.len() > MAX_BYTES || s.dict.contains(b"F") {
        return Err(invalid("external or oversized content streams are unsupported"));
    }
    validate_decode_parameters(s, budget)?;
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
    let value = exact_value(doc, c)?;
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
        let value = exact_value(doc, &item)?;
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
            let props = resource_dictionary(doc, res.get(b"Properties").ok_or_else(|| invalid("missing named content Properties"))?)?;
            let value = props.get(name).ok_or_else(|| invalid("missing named marked-content property"))?;
            resource_dictionary(doc, value)
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
// Only empty P/Span wrappers are transparent to another selected ownership scope.
// Semantic Span properties are tolerated *inside* a selected scope, where their exact
// bytes are preserved. They are opaque above a selected mark or a selected Form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Selected,
    Transparent,
    Opaque,
}
fn empty_wrapper(name: &[u8], props: &Dict) -> bool {
    matches!(name, b"P" | b"Span") && props.is_empty()
}
fn semantic_span(name: &[u8], props: &Dict) -> bool {
    name == b"Span"
        && props.iter().all(|(key, value)| matches!(key.as_slice(), b"Lang" | b"ActualText" | b"Alt" | b"E") && matches!(value, Object::String(_)))
}
fn marked_properties(doc: &Document, bytes: &[u8], res: &Dict, op: &pdfcraft_content::Op) -> Result<Dict> {
    if op.operands.len() != 2 || op.name(0).is_none() {
        return Err(invalid("malformed BDC"));
    }
    let raw = bytes.get(op.span.clone()).ok_or_else(|| invalid("invalid BDC span"))?;
    let mut lx = pdfcraft_cos::Lexer::new(raw, 0);
    strict_operand(&mut lx, raw, 0)?;
    strict_operand(&mut lx, raw, 0)?;
    let props = properties(doc, res, op.operands.get(1).ok_or_else(|| invalid("missing BDC properties"))?)?;
    if props.get(b"MCID").is_some_and(|value| !matches!(value, Object::Int(n) if *n >= 0)) {
        return Err(invalid("invalid BDC MCID"));
    }
    Ok(props)
}
fn mask_state(doc: &Document, res: &Dict, op: &pdfcraft_content::Op, active: bool) -> Result<bool> {
    let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed ExtGState invocation"))?;
    let states = resource_dictionary(doc, res.get(b"ExtGState").ok_or_else(|| invalid("missing ExtGState resources"))?)?;
    let state = resource_dictionary(doc, states.get(name).ok_or_else(|| invalid("missing ExtGState resource"))?)?;
    match state.get(b"SMask") {
        None => Ok(active),
        Some(Object::Name(name)) if name == b"None" => Ok(false),
        Some(Object::Dict(_)) | Some(Object::Ref(_)) => Ok(true),
        _ => Err(invalid("malformed graphics-state soft mask")),
    }
}
// Resource-backed property lists are copied inline. Reserve a conservative serialized
// size before constructing that output, and reject reference operands which would become
// invalid content syntax (and therefore cannot simply be copied from a named dictionary).
fn reserve_inline_properties(props: &Dict, budget: &mut ArtifactBudget) -> Result<usize> {
    let mut size = 4usize;
    let mut stack = Vec::new();
    budget.work(props.len())?;
    for (name, value) in props.iter() {
        size = size
            .checked_add(name.len().checked_mul(3).and_then(|n| n.checked_add(2)).ok_or_else(|| invalid("artifact property size overflow"))?)
            .ok_or_else(|| invalid("artifact property size overflow"))?;
        stack.push((value, 0usize));
    }
    while let Some((value, depth)) = stack.pop() {
        budget.work(1)?;
        if depth > 128 {
            return Err(invalid("inline artifact property nesting exceeds safe limit"));
        }
        let extra = match value {
            Object::Null | Object::Bool(_) => 5,
            Object::Int(_) => 21,
            Object::Real(number) if number.is_finite() => 400,
            Object::String(text) => {
                text.bytes.len().checked_mul(4).and_then(|n| n.checked_add(2)).ok_or_else(|| invalid("artifact property size overflow"))?
            }
            Object::Name(name) => {
                name.len().checked_mul(3).and_then(|n| n.checked_add(1)).ok_or_else(|| invalid("artifact property size overflow"))?
            }
            Object::Array(items) => {
                budget.work(items.len())?;
                for value in items {
                    stack.push((value, depth + 1));
                }
                items.len().checked_add(2).ok_or_else(|| invalid("artifact property size overflow"))?
            }
            Object::Dict(dict) => {
                budget.work(dict.len())?;
                let mut extra = 4usize;
                for (name, value) in dict.iter() {
                    extra = extra
                        .checked_add(
                            name.len().checked_mul(3).and_then(|n| n.checked_add(2)).ok_or_else(|| invalid("artifact property size overflow"))?,
                        )
                        .ok_or_else(|| invalid("artifact property size overflow"))?;
                    stack.push((value, depth + 1));
                }
                extra
            }
            _ => return Err(invalid("named artifact properties contain an indirect, nonfinite or stream value which cannot be safely inlined")),
        };
        size = size.checked_add(extra).ok_or_else(|| invalid("artifact property size overflow"))?;
        if size > MAX_BYTES {
            return Err(invalid("artifact property output exceeds safe byte budget"));
        }
    }
    let reserved = size.checked_add(14).ok_or_else(|| invalid("artifact property size overflow"))?;
    budget.charge(reserved)?;
    Ok(reserved)
}
type Replacement = (std::ops::Range<usize>, Vec<u8>);
fn rewrite(doc: &Document, bytes: &[u8], res: &Dict, wanted: &BTreeSet<i64>, budget: &mut ArtifactBudget) -> Result<Vec<Replacement>> {
    let parsed = parse(bytes)?;
    budget.work(parsed.ops.len())?;
    let mut marks = Vec::new();
    let mut seen = BTreeSet::new();
    let mut edits = Vec::new();
    let mut masks = Vec::new();
    let mut mask = false;
    for op in parsed.ops {
        let inside = marks.contains(&Scope::Selected);
        match op.op.as_slice() {
            b"BDC" => {
                let mut props = marked_properties(doc, bytes, res, &op)?;
                let name = op.name(0).ok_or_else(|| invalid("missing BDC tag"))?;
                let mcid = props.int(b"MCID");
                let target = mcid.is_some_and(|n| wanted.contains(&n));
                let scope = if target {
                    if inside || marks.contains(&Scope::Opaque) {
                        return Err(invalid("overlapping claimed marks or opaque ancestor intersects selected content"));
                    }
                    if matches!(name, b"OC" | b"Artifact") || mask {
                        return Err(invalid("optional/artifact content or active soft mask cannot be converted"));
                    }
                    if !seen.insert(mcid.ok_or_else(|| invalid("missing target MCID"))?) {
                        return Err(invalid("duplicate selected MCID in content namespace"));
                    }
                    props.remove(b"MCID");
                    let mut replacement = Vec::with_capacity(reserve_inline_properties(&props, budget)?);
                    replacement.extend_from_slice(b"/Artifact ");
                    pdfcraft_cos::serialize(&Object::Dict(props), &mut replacement);
                    replacement.extend_from_slice(b" BDC");
                    edits.push((op.span, replacement));
                    Scope::Selected
                } else if empty_wrapper(name, &props) || (inside && semantic_span(name, &props)) {
                    Scope::Transparent
                } else {
                    if inside {
                        return Err(invalid("unsupported semantic descendant or overlapping claimed mark in selected content"));
                    }
                    Scope::Opaque
                };
                marks.push(scope);
            }
            b"BMC" => {
                let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed BMC"))?;
                let scope = if matches!(name, b"P" | b"Span") { Scope::Transparent } else { Scope::Opaque };
                if inside && scope == Scope::Opaque {
                    return Err(invalid("opaque BMC intersects selected content"));
                }
                marks.push(scope);
            }
            b"EMC" => {
                if !op.operands.is_empty() || marks.pop().is_none() {
                    return Err(invalid("unbalanced or malformed EMC"));
                }
            }
            b"q" => {
                if !op.operands.is_empty() || masks.len() >= 128 {
                    return Err(invalid("invalid or excessive graphics-state nesting"));
                }
                masks.push(mask);
            }
            b"Q" => {
                if !op.operands.is_empty() {
                    return Err(invalid("malformed graphics-state restore"));
                }
                mask = masks.pop().ok_or_else(|| invalid("unbalanced graphics-state restore"))?;
            }
            b"gs" => {
                mask = mask_state(doc, res, &op, mask)?;
                if inside && mask {
                    return Err(invalid("selected content uses an active soft mask"));
                }
            }
            b"Do" if inside => image_invocation(doc, res, &op)?,
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
    let object_count = doc.object_numbers().len();
    if object_count > LIMIT {
        return Err(invalid("too many objects to verify artifact ownership"));
    }
    byte_budget.work(object_count)?;
    let tree = tags::inspect(doc)?;
    if !tree.edit_blockers.is_empty() {
        return Err(invalid(format!("structure is not safely editable: {}", tree.edit_blockers.join("; "))));
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
    byte_budget.work(tree.elements.len())?;
    for element in &tree.elements {
        byte_budget.work(element.kids.len())?;
    }
    let affected = groups.iter().flat_map(|((page, stream), ids)| ids.iter().map(move |id| (stream.unwrap_or(*page), *id))).collect();
    tags::reject_orphan_content_overlap(doc, &tree, &affected)?;
    // The model intentionally tolerates stale page-tree references while viewing.
    // Editing must prove the exact tree before using its inherited page resources.
    page_resource_owners(doc, byte_budget)?;
    let pages = pdfcraft_model::pages(doc);
    let forms: BTreeSet<_> = groups.keys().filter_map(|(_, s)| *s).collect();
    let form_resources = if forms.is_empty() { BTreeMap::new() } else { verify_forms(doc, &pages, &forms, &groups, byte_budget)? };
    let mut candidate = doc.clone();
    for ((page, stream), ids) in groups {
        let page = pages.iter().find(|p| p.obj == page).ok_or_else(|| invalid("content page not found"))?;
        checked(doc, page.obj)?;
        if let Some(stream) = stream {
            let value = checked(doc, stream)?;
            let Object::Stream(s) = &*value else { return Err(invalid("MCR Stm is not a stream")) };
            let res = form_resources.get(&stream).ok_or_else(|| invalid("missing verified Form caller resources"))?;
            let source = decoded(s, byte_budget)?;
            let bytes = replace(&source, rewrite(doc, &source, res, &ids, byte_budget)?, byte_budget)?;
            let mut d = s.dict.clone();
            for k in [b"Filter".as_slice(), b"DecodeParms", b"Length"] {
                d.remove(k);
            }
            candidate.set(stream, Object::Stream(Stream::from_raw(d, bytes)));
        } else {
            let source = page_bytes_bounded(doc, &page.dict, byte_budget)?;
            let edits = rewrite(doc, &source, &resources(doc, &page.dict)?, &ids, byte_budget)?;
            let original = page.dict.get(b"Contents").ok_or_else(|| invalid("missing Contents"))?;
            let resolved = exact_value(doc, original)?;
            let array = matches!(&*resolved, Object::Array(_));
            let items = if let Object::Array(a) = &*resolved { a.clone() } else { vec![original.clone()] };
            let mut contents = Vec::new();
            let mut offset = 0usize;
            let mut applied = 0usize;
            for item in items {
                let value = exact_value(doc, &item)?;
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
    tags::validate_preserved_associations(doc, &candidate)?;
    *doc = candidate;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PathPart {
    Key(Vec<u8>),
    Index(usize),
}
type Location = (ObjRef, Vec<PathPart>);
type Edge = (Location, ObjRef);
fn key_location(location: &Location, key: &[u8]) -> Location {
    let mut path = location.1.clone();
    path.push(PathPart::Key(key.to_vec()));
    (location.0, path)
}
fn located_dictionary(doc: &Document, value: &Object, location: Location, chain: &mut Vec<Edge>) -> Result<(Dict, Location)> {
    if let Some(id) = value.as_ref() {
        chain.push((location, id));
        Ok((resource_dictionary(doc, value)?, (id, Vec::new())))
    } else {
        Ok((resource_dictionary(doc, value)?, location))
    }
}
fn accept_chain(chain: &[Edge], allowed: &mut BTreeSet<Edge>, sensitive: &mut BTreeSet<ObjRef>) {
    for edge in chain {
        allowed.insert(edge.clone());
        sensitive.insert(edge.1);
    }
}
// Resource ownership is established from the catalog's actual page tree. A detached
// dictionary claiming /Type /Page is not evidence that its Resources alias is harmless.
fn page_resource_owners(doc: &Document, budget: &mut ArtifactBudget) -> Result<BTreeSet<ObjRef>> {
    budget.work(1)?;
    let catalog = checked(doc, doc.root().ok_or_else(|| invalid("missing catalog"))?)?;
    let Object::Dict(catalog) = &*catalog else { return Err(invalid("catalog must be an exact dictionary")) };
    let root = catalog.reference(b"Pages").ok_or_else(|| invalid("missing exact page-tree root"))?;
    let mut owners = BTreeSet::new();
    let mut stack = vec![(root, None, 0usize)];
    while let Some((id, parent, depth)) = stack.pop() {
        budget.work(1)?;
        if depth > 128 || owners.len() >= LIMIT || !owners.insert(id) {
            return Err(invalid("cyclic, shared or oversized page resource graph"));
        }
        let value = checked(doc, id)?;
        let Object::Dict(dict) = &*value else { return Err(invalid("page-tree node must be an exact dictionary")) };
        if parent.is_some_and(|parent| dict.reference(b"Parent") != Some(parent)) {
            return Err(invalid("page-tree Parent does not match exact traversal ownership"));
        }
        match dict.name(b"Type") {
            Some(b"Page") if parent.is_some() => {}
            Some(b"Pages") => {
                let kids = exact_value(doc, dict.get(b"Kids").ok_or_else(|| invalid("missing page-tree Kids"))?)?;
                let Object::Array(kids) = &*kids else { return Err(invalid("page-tree Kids must be an exact array")) };
                budget.work(kids.len())?;
                for kid in kids {
                    let child = kid.as_ref().ok_or_else(|| invalid("page-tree children must be exact references"))?;
                    stack.push((child, Some(id), depth + 1));
                }
            }
            _ => return Err(invalid("invalid page-tree node type")),
        }
    }
    Ok(owners)
}
// Prove every physical reference to each edited Form and its resource containers. A
// reference under a private alias, appearance, mask or pattern is never a page invocation.
// Indirect dictionaries are supported without conflating their identity with the caller.
fn verify_form_references(doc: &Document, forms: &BTreeSet<ObjRef>, budget: &mut ArtifactBudget) -> Result<()> {
    let page_owners = page_resource_owners(doc, budget)?;
    let nums = doc.object_numbers();
    if nums.len() > LIMIT {
        return Err(invalid("too many objects to verify Form usage"));
    }
    let mut allowed = BTreeSet::<Edge>::new();
    let mut sensitive = forms.clone();
    let mut resource_edges = BTreeMap::<ObjRef, BTreeSet<Location>>::new();
    for &num in &nums {
        budget.work(1)?;
        let id = ObjRef::new(num, doc.generation(num));
        let value = checked(doc, id)?;
        let Some(d) = value.as_dict() else { continue };
        let resource_owner =
            page_owners.contains(&id) || forms.contains(&id) && matches!(&*value, Object::Stream(_)) && d.name(b"Subtype") == Some(b"Form");
        if !resource_owner {
            continue;
        }
        let Some(res) = d.get(b"Resources") else { continue };
        let mut chain = Vec::new();
        let (res, location) = located_dictionary(doc, res, key_location(&(id, Vec::new()), b"Resources"), &mut chain)?;
        let Some(xobjects) = res.get(b"XObject") else { continue };
        let (xobjects, location) = located_dictionary(doc, xobjects, key_location(&location, b"XObject"), &mut chain)?;
        budget.work(xobjects.len())?;
        for (name, value) in xobjects.iter() {
            let Some(target) = value.as_ref().filter(|r| forms.contains(r)) else { continue };
            let edge = (key_location(&location, name), target);
            resource_edges.entry(target).or_default().insert(edge.0.clone());
            allowed.insert(edge);
            accept_chain(&chain, &mut allowed, &mut sensitive);
        }
    }
    if forms.iter().any(|form| resource_edges.get(form).is_none_or(|edges| edges.len() != 1)) {
        return Err(invalid("Form must have exactly one verified resource entry; aliases require instance isolation"));
    }
    // Only MCRs actually reached through the inspected structure tree may point at a
    // target stream. Standalone dictionaries with a plausible /Type are not authority.
    let tree = tags::inspect(doc)?;
    for element in &tree.elements {
        let value = checked(doc, element.id)?;
        let d = value.as_dict().ok_or_else(|| invalid("invalid structure element"))?;
        let Some(kids) = d.get(b"K") else { continue };
        let mut stack = vec![(kids.clone(), key_location(&(element.id, Vec::new()), b"K"), Vec::<Edge>::new(), 0usize)];
        while let Some((value, mut location, mut chain, depth)) = stack.pop() {
            budget.work(1)?;
            if depth > 128 {
                return Err(invalid("MCR reference graph exceeds safe depth"));
            }
            let value = if let Some(id) = value.as_ref() {
                chain.push((location, id));
                location = (id, Vec::new());
                checked(doc, id)?
            } else {
                std::sync::Arc::new(value)
            };
            match &*value {
                Object::Array(items) => {
                    budget.work(items.len())?;
                    for (index, item) in items.iter().enumerate() {
                        let mut child = location.clone();
                        child.1.push(PathPart::Index(index));
                        stack.push((item.clone(), child, chain.clone(), depth + 1));
                    }
                }
                Object::Dict(d) if d.name(b"Type") == Some(b"MCR") => {
                    if let Some(target) = d.reference(b"Stm").filter(|r| forms.contains(r)) {
                        if d.contains(b"StmOwn") {
                            return Err(invalid("Form MCR StmOwn needs instance-aware ownership"));
                        }
                        allowed.insert((key_location(&location, b"Stm"), target));
                        accept_chain(&chain, &mut allowed, &mut sensitive);
                    }
                }
                _ => {}
            }
        }
    }
    let sensitive_numbers: BTreeSet<_> = sensitive.iter().map(|r| r.num).collect();
    let trailer = Object::Dict(doc.trailer().clone());
    for id in nums.into_iter().map(|num| ObjRef::new(num, doc.generation(num))).chain(std::iter::once(ObjRef::new(0, 0))) {
        let value = if id.num == 0 { std::sync::Arc::new(trailer.clone()) } else { checked(doc, id)? };
        let mut stack = vec![(&*value, (id, Vec::new()), 0usize)];
        while let Some((value, location, depth)) = stack.pop() {
            budget.work(1)?;
            if depth > 128 {
                return Err(invalid("Form reference graph exceeds safe depth"));
            }
            match value {
                Object::Ref(target) if sensitive_numbers.contains(&target.num) => {
                    if !allowed.contains(&(location, *target)) {
                        return Err(invalid("Form or its resource container has a private, stale, shared or unsupported reference"));
                    }
                }
                Object::Array(items) => {
                    budget.work(items.len())?;
                    for (index, item) in items.iter().enumerate() {
                        let mut child = location.clone();
                        child.1.push(PathPart::Index(index));
                        stack.push((item, child, depth + 1));
                    }
                }
                _ => {
                    if let Some(d) = value.as_dict() {
                        budget.work(d.len())?;
                        for (key, value) in d.iter() {
                            stack.push((value, key_location(&location, key), depth + 1));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn verify_forms(
    doc: &Document,
    pages: &[pdfcraft_model::Page],
    forms: &BTreeSet<ObjRef>,
    groups: &BTreeMap<(ObjRef, Option<ObjRef>), BTreeSet<i64>>,
    byte_budget: &mut ArtifactBudget,
) -> Result<BTreeMap<ObjRef, Dict>> {
    let mut invoked_ancestors = forms.clone();
    let mut usages = BTreeMap::<ObjRef, Vec<ObjRef>>::new();
    let mut verified_resources = BTreeMap::new();
    for page in pages {
        if !page.dict.contains(b"Contents") {
            continue;
        }
        let mut stack = vec![(page_bytes_bounded(doc, &page.dict, byte_budget)?, resources(doc, &page.dict)?, BTreeSet::new(), false, false)];
        while let Some((bytes, res, ancestors, inherited_opaque, inherited_mask)) = stack.pop() {
            let parsed = parse(&bytes)?;
            byte_budget.work(parsed.ops.len())?;
            let mut marks = Vec::new();
            let mut mask = inherited_mask;
            let mut masks = Vec::new();
            for op in parsed.ops {
                match op.op.as_slice() {
                    b"BDC" => {
                        let props = marked_properties(doc, &bytes, &res, &op)?;
                        let name = op.name(0).ok_or_else(|| invalid("missing BDC tag"))?;
                        marks.push(if empty_wrapper(name, &props) { Scope::Transparent } else { Scope::Opaque });
                    }
                    b"BMC" => {
                        let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed BMC"))?;
                        marks.push(if matches!(name, b"P" | b"Span") { Scope::Transparent } else { Scope::Opaque });
                    }
                    b"EMC" => {
                        if !op.operands.is_empty() || marks.pop().is_none() {
                            return Err(invalid("unbalanced or malformed marked content while checking Form usage"));
                        }
                    }
                    b"q" => {
                        if !op.operands.is_empty() || masks.len() >= 128 {
                            return Err(invalid("invalid or excessive graphics-state nesting"));
                        }
                        masks.push(mask);
                    }
                    b"Q" => {
                        if !op.operands.is_empty() {
                            return Err(invalid("malformed graphics-state restore"));
                        }
                        mask = masks.pop().ok_or_else(|| invalid("unbalanced graphics-state restore"))?;
                    }
                    b"gs" => mask = mask_state(doc, &res, &op, mask)?,
                    b"Do" => {
                        let name = op.name(0).filter(|_| op.operands.len() == 1).ok_or_else(|| invalid("malformed XObject invocation"))?;
                        let xobjects = resource_dictionary(doc, res.get(b"XObject").ok_or_else(|| invalid("missing XObject resources"))?)?;
                        let id = xobjects.get(name).and_then(Object::as_ref).ok_or_else(|| invalid("XObject must be an exact indirect stream"))?;
                        let value = checked(doc, id)?;
                        let Object::Stream(stream) = &*value else { return Err(invalid("XObject is not a stream")) };
                        if stream.dict.contains(b"Type") && stream.dict.name(b"Type") != Some(b"XObject") {
                            return Err(invalid("invalid XObject type"));
                        }
                        match stream.dict.name(b"Subtype") {
                            Some(b"Image") => continue,
                            Some(b"Form") => {}
                            _ => return Err(invalid("unsupported XObject subtype")),
                        }
                        if ancestors.contains(&id) || ancestors.len() >= 128 {
                            return Err(invalid("cyclic or deeply nested Form invocation"));
                        }
                        let opaque = inherited_opaque || marks.contains(&Scope::Opaque);
                        let child_res = if stream.dict.contains(b"Resources") { resources(doc, &stream.dict)? } else { res.clone() };
                        if forms.contains(&id) {
                            if opaque || mask {
                                return Err(invalid("selected Form is inside opaque marked content or an active soft mask"));
                            }
                            let uses = usages.entry(id).or_default();
                            uses.push(page.obj);
                            if uses.len() > 1 {
                                return Err(invalid("Form must be invoked exactly once; reused Forms require instance isolation"));
                            }
                            invoked_ancestors.extend(ancestors.iter().copied());
                            verified_resources.insert(id, child_res.clone());
                        }
                        let mut ancestors = ancestors.clone();
                        ancestors.insert(id);
                        stack.push((decoded(stream, byte_budget)?, child_res, ancestors, opaque, mask));
                    }
                    _ => {}
                }
                if marks.len() > 128 {
                    return Err(invalid("marked-content nesting exceeds safe limit"));
                }
            }
            if !marks.is_empty() {
                return Err(invalid("unbalanced marked content while checking Form usage"));
            }
        }
    }
    for (page, stream) in groups.keys() {
        if let Some(stream) = stream
            && usages.get(stream).is_none_or(|u| u.as_slice() != [*page])
        {
            return Err(invalid("Form must be invoked exactly once on its associated page; reused or unassociated Forms require instance isolation"));
        }
    }
    verify_form_references(doc, &invoked_ancestors, byte_budget)?;
    Ok(verified_resources)
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
            b"/P <</MCID 0>> BDC /Figure BMC EMC EMC",
            b"/Figure BMC /P <</MCID 0>> BDC EMC EMC",
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
    fn form_nested_in_opaque_page_mark_and_split_bdc_are_rejected_atomically() {
        let (mut doc, a, page, _, _) = form_fixture();
        let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/Figure BMC /F Do EMC".to_vec())));
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
    #[test]
    fn empty_paragraph_and_span_wrappers_are_transparent_and_byte_preserved() {
        for content in [
            b"/Span BMC /P <</MCID 0>> BDC /Span BMC 1 2 3 4 re f EMC EMC EMC".as_slice(),
            b"/P <<>> BDC /Span <<>> BDC /P <</MCID 0>> BDC 1 2 3 4 re f EMC EMC EMC",
            b"/P <</MCID 0>> BDC /P <<>> BDC /Span <<>> BDC 1 2 3 4 re f EMC EMC EMC",
        ] {
            let (mut doc, tag, _, page, other) = fixture(content);
            let before = page_content(&doc, other);
            convert(&mut doc, tag, false).unwrap();
            assert_eq!(page_content(&doc, other), before);
            let expected = String::from_utf8_lossy(content).replace("/P <</MCID 0>> BDC", "/Artifact <<>> BDC");
            assert_eq!(String::from_utf8_lossy(&page_content(&doc, page)).trim(), expected);
        }
    }
    #[test]
    fn semantic_span_descendants_preserve_exact_bytes_but_are_opaque_ancestors() {
        for props in [
            "/Lang (en-US)",
            "/ActualText (actual)",
            "/Alt (alternate)",
            "/E (expanded)",
            "/Lang (en) /ActualText (actual) /Alt (alternate) /E (expanded)",
        ] {
            let descendant = format!("/Span <<{props}>> BDC 1 2 3 4 re f EMC");
            let input = format!("/P <</MCID 0>> BDC {descendant} EMC");
            let (mut doc, tag, _, page, _) = fixture(input.as_bytes());
            convert(&mut doc, tag, false).unwrap();
            assert!(String::from_utf8_lossy(&page_content(&doc, page)).contains(&descendant));
            let input = format!("/Span <<{props}>> BDC /P <</MCID 0>> BDC 1 2 3 4 re f EMC EMC");
            let (mut doc, tag, _, _, _) = fixture(input.as_bytes());
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn unsupported_descendant_semantics_and_overlapping_claims_are_atomic() {
        for descendant in [
            "/Span <</Lang /en>> BDC",
            "/Span <</Lang 42>> BDC",
            "/Span <</Lang [(en)]>> BDC",
            "/Span <</Lang (en) /Private true>> BDC",
            "/Span <</MCID 1>> BDC",
            "/Span <</MCID 0>> BDC",
            "/P <</Lang (en)>> BDC",
            "/Custom <<>> BDC",
            "/Artifact BMC",
            "/OC <<>> BDC",
        ] {
            let input = format!("/P <</MCID 0>> BDC {descendant} 1 2 3 4 re f EMC EMC");
            let (mut doc, tag, _, _, _) = fixture(input.as_bytes());
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err(), "{descendant}");
            assert_eq!(bytes(&doc), before);
        }
        let (mut doc, tag, _, _, _) = fixture(b"/P <</MCID 0.0>> BDC EMC");
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn named_empty_and_semantic_properties_remain_shared_and_unchanged() {
        let (mut doc, tag, _, page, _) = fixture(b"/Span /Empty BDC /P <</MCID 0>> BDC /Span /Words BDC 1 2 3 4 re f EMC EMC EMC");
        let empty = doc.add(Dict::new());
        let mut words = Dict::new();
        words.set(b"ActualText", Object::String(pdfcraft_cos::PdfString::text("replacement")));
        let words = doc.add(words);
        let mut props = Dict::new();
        props.set(b"Empty", Object::Ref(empty));
        props.set(b"Words", Object::Ref(words));
        let props = doc.add(props);
        let mut res = Dict::new();
        res.set(b"Properties", Object::Ref(props));
        let res = doc.add(res);
        doc.update_dict(page, |d| d.set(b"Resources", Object::Ref(res))).unwrap();
        let resources_before = doc.get(res);
        let words_before = doc.get(words);
        convert(&mut doc, tag, false).unwrap();
        assert_eq!(doc.get(res), resources_before);
        assert_eq!(doc.get(words), words_before);
        assert!(String::from_utf8_lossy(&page_content(&doc, page)).contains("/Span /Words BDC"));
    }
    fn indirect_form_resources(doc: &mut Document, page: ObjRef) -> (ObjRef, ObjRef) {
        let mut res = doc.get(page).as_dict().unwrap().get(b"Resources").unwrap().as_dict().unwrap().clone();
        let xobjects = doc.add(res.get(b"XObject").unwrap().clone());
        res.set(b"XObject", Object::Ref(xobjects));
        let res = doc.add(res);
        doc.update_dict(page, |d| d.set(b"Resources", Object::Ref(res))).unwrap();
        (res, xobjects)
    }
    #[test]
    fn indirect_form_resources_and_empty_caller_wrappers_preserve_identity() {
        for wrapper in ["/Span BMC", "/P BMC", "/Span <<>> BDC", "/P <<>> BDC"] {
            let (mut doc, tag, page, _, form) = form_fixture();
            let (res, xobjects) = indirect_form_resources(&mut doc, page);
            let content = doc.add(Object::Stream(Stream::from_raw(Dict::new(), format!("{wrapper} /F Do EMC").into_bytes())));
            doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(content))).unwrap();
            let res_before = doc.get(res);
            let xobjects_before = doc.get(xobjects);
            let content_before = doc.get(content);
            convert(&mut doc, tag, false).unwrap();
            assert_eq!(doc.get(res), res_before);
            assert_eq!(doc.get(xobjects), xobjects_before);
            assert_eq!(doc.get(content), content_before);
            let value = doc.get(form);
            let Object::Stream(stream) = &*value else { panic!() };
            assert!(stream.decoded().unwrap().starts_with(b"/Artifact"));
        }
    }
    #[test]
    fn selected_form_and_resource_container_private_aliases_reject() {
        for alias in 0..7 {
            let (mut doc, tag, page, _, form) = form_fixture();
            let (res, xobjects) = indirect_form_resources(&mut doc, page);
            let target = match alias {
                0 | 3 => form,
                1 | 4..=6 => res,
                _ => xobjects,
            };
            if alias == 3 {
                doc.trailer_mut().set(b"PrivateAlias", Object::Ref(target));
            } else if alias >= 4 {
                let mut private = Dict::new();
                private.set(b"Resources", Object::Ref(target));
                if alias == 6 {
                    private.set(b"Type", Object::name("XObject"));
                    private.set(b"Subtype", Object::name("Form"));
                    doc.add(Object::Stream(Stream::from_raw(private, b"q Q".to_vec())));
                } else {
                    private.set(b"Type", Object::name(if alias == 4 { "Page" } else { "Pages" }));
                    doc.add(private);
                }
            } else {
                let mut private = Dict::new();
                private.set(b"PrivateAlias", Object::Ref(target));
                doc.add(private);
            }
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err(), "alias {alias}");
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn duplicate_form_resource_alias_and_unreachable_mcr_refuse_atomically() {
        let (mut doc, tag, page, _, form) = form_fixture();
        let (_, xobjects) = indirect_form_resources(&mut doc, page);
        doc.update_dict(xobjects, |d| d.set(b"Alias", Object::Ref(form))).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
        let (mut doc, tag, _, _, form) = form_fixture();
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", 99);
        mcr.set(b"Stm", Object::Ref(form));
        doc.add(mcr);
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    fn nested_form_fixture() -> (Document, ObjRef, ObjRef, ObjRef, ObjRef) {
        let (mut doc, tag, page, _, form) = form_fixture();
        let value = doc.get(form);
        let Object::Stream(stream) = &*value else { panic!() };
        doc.set(form, Object::Stream(Stream::from_raw(stream.dict.clone(), b"/P /Caller BDC 1 2 3 4 re f EMC".to_vec())));
        let mut property = Dict::new();
        property.set(b"MCID", 0);
        property.set(b"Private", Object::name("caller"));
        let mut props = Dict::new();
        props.set(b"Caller", property);
        let mut xobjects = Dict::new();
        xobjects.set(b"F", Object::Ref(form));
        let mut caller_res = Dict::new();
        caller_res.set(b"Properties", props);
        caller_res.set(b"XObject", xobjects);
        let mut caller_dict = Dict::new();
        caller_dict.set(b"Type", Object::name("XObject"));
        caller_dict.set(b"Subtype", Object::name("Form"));
        caller_dict.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]));
        caller_dict.set(b"Resources", caller_res);
        let caller = doc.add(Object::Stream(Stream::from_raw(caller_dict, b"/Span BMC /F Do EMC".to_vec())));
        let mut page_xobjects = Dict::new();
        page_xobjects.set(b"Outer", Object::Ref(caller));
        let mut wrong = Dict::new();
        wrong.set(b"MCID", 9);
        let mut page_props = Dict::new();
        page_props.set(b"Caller", wrong);
        let mut page_res = Dict::new();
        page_res.set(b"XObject", page_xobjects);
        page_res.set(b"Properties", page_props);
        let contents = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/Outer Do".to_vec())));
        doc.update_dict(page, |d| {
            d.set(b"Resources", page_res);
            d.set(b"Contents", Object::Ref(contents));
        })
        .unwrap();
        (doc, tag, page, form, caller)
    }
    #[test]
    fn resource_less_form_uses_exact_intermediate_caller_not_page_fallback() {
        let (mut doc, tag, page, form, caller) = nested_form_fixture();
        let caller_before = doc.get(caller);
        let page_before = doc.get(page);
        convert(&mut doc, tag, false).unwrap();
        assert_eq!(doc.get(caller), caller_before);
        assert_eq!(doc.get(page), page_before);
        let value = doc.get(form);
        let Object::Stream(stream) = &*value else { panic!() };
        let decoded = stream.decoded().unwrap();
        assert!(String::from_utf8_lossy(&decoded).contains("/Private /caller"));
        assert!(!String::from_utf8_lossy(&decoded).contains("/MCID"));
    }
    #[test]
    fn private_alias_to_an_intermediate_form_rejects() {
        let (mut doc, tag, _, _, caller) = nested_form_fixture();
        let mut appearance = Dict::new();
        appearance.set(b"N", Object::Ref(caller));
        let mut annotation = Dict::new();
        annotation.set(b"AP", appearance);
        doc.add(annotation);
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    fn add_mask_states(doc: &mut Document, page: ObjRef) {
        let mut res = resources(doc, doc.get(page).as_dict().unwrap()).unwrap();
        let mut active = Dict::new();
        active.set(b"SMask", Dict::new());
        let mut none = Dict::new();
        none.set(b"SMask", Object::name("None"));
        let mut states = Dict::new();
        states.set(b"Mask", active);
        states.set(b"None", none);
        res.set(b"ExtGState", states);
        doc.update_dict(page, |d| d.set(b"Resources", res)).unwrap();
    }
    #[test]
    fn active_soft_masks_refuse_and_none_or_restored_masks_remain_supported() {
        for (content, allowed) in [
            ("/Mask gs /P <</MCID 0>> BDC 1 2 3 4 re f EMC", false),
            ("/P <</MCID 0>> BDC /Mask gs 1 2 3 4 re f EMC", false),
            ("/Mask gs /None gs /P <</MCID 0>> BDC 1 2 3 4 re f EMC", true),
            ("q /Mask gs Q /P <</MCID 0>> BDC 1 2 3 4 re f EMC", true),
        ] {
            let (mut doc, tag, _, page, _) = fixture(content.as_bytes());
            add_mask_states(&mut doc, page);
            let before = bytes(&doc);
            assert_eq!(convert(&mut doc, tag, false).is_ok(), allowed, "{content}");
            if !allowed {
                assert_eq!(bytes(&doc), before);
            }
        }
        let (mut doc, tag, page, _, _) = form_fixture();
        add_mask_states(&mut doc, page);
        let contents = doc.add(Object::Stream(Stream::from_raw(Dict::new(), b"/Mask gs /F Do".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(contents))).unwrap();
        let before = bytes(&doc);
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn aggregate_graph_budget_refuses_without_reset_or_document_mutation() {
        let (mut doc, tag, _, _, _) = form_fixture();
        let before = bytes(&doc);
        let mut budget = ArtifactBudget { work_remaining: 1, ..Default::default() };
        let err = convert_with_budget(&mut doc, tag, false, &mut budget).unwrap_err().to_string();
        assert!(err.contains("work budget"), "{err}");
        assert_eq!(bytes(&doc), before);
        assert_eq!(budget.work_remaining, 1);
        let mut budget = ArtifactBudget { work_remaining: 2, ..Default::default() };
        assert!(verify_form_references(&doc, &BTreeSet::new(), &mut budget).is_err());
        assert_eq!(budget.work_remaining, 0);
    }
    /// Optional external-input harness. The caller verifies pinned source hashes before
    /// invoking this test and independently validates every output. No corpus is bundled.
    #[test]
    #[ignore = "requires explicit PDFCRAFT_ARTIFACT_CASES TSV with source and output paths"]
    fn external_pinned_artifact_conversion_targets() {
        let cases = std::env::var("PDFCRAFT_ARTIFACT_CASES").expect("explicit source/output TSV");
        let cases = std::fs::read_to_string(cases).unwrap();
        for line in cases.lines().filter(|line| !line.is_empty()) {
            let (source, output) = line.split_once('\t').expect("source/output TSV row");
            let raw = std::fs::read(source).unwrap();
            let mut doc = Document::open(std::sync::Arc::new(raw)).unwrap();
            let tree = tags::inspect(&doc).unwrap();
            assert!(tree.edit_blockers.is_empty(), "{source}: {:?}", tree.edit_blockers);
            let tag = tree
                .elements
                .iter()
                .find(|element| !element.kids.is_empty() && element.kids.iter().all(|kid| matches!(kid, TagKid::Mcid { .. } | TagKid::Mcr { .. })))
                .expect("first exact marked-content leaf")
                .id;
            convert(&mut doc, tag, false).unwrap_or_else(|error| panic!("{source}: {error}"));
            let output_bytes = pdfcraft_cos::write_incremental(&doc, &Default::default()).unwrap();
            let reopened = Document::open(std::sync::Arc::new(output_bytes.clone())).unwrap();
            assert!(tags::inspect(&reopened).unwrap().edit_blockers.is_empty(), "{source}: reopened blockers");
            std::fs::write(output, output_bytes).unwrap();
            eprintln!("artifact target: {source}; selected {tag}; output {output}");
        }
    }
    #[test]
    fn named_inline_output_is_prebudgeted_and_rejects_indirect_values() {
        let mut large = Dict::new();
        large.set(b"Private", Object::String(pdfcraft_cos::PdfString::text(&"x".repeat(4096))));
        let mut budget = ArtifactBudget::new(1024);
        assert!(reserve_inline_properties(&large, &mut budget).unwrap_err().to_string().contains("budget"));
        assert_eq!(budget.remaining, 1024);
        let (mut doc, tag, _, page, _) = fixture(b"/P /Named BDC 1 2 3 4 re f EMC");
        let text = doc.add(Object::String(pdfcraft_cos::PdfString::text("private")));
        let mut property = Dict::new();
        property.set(b"MCID", 0);
        property.set(b"Private", Object::Ref(text));
        let mut props = Dict::new();
        props.set(b"Named", property);
        let mut res = Dict::new();
        res.set(b"Properties", props);
        doc.update_dict(page, |d| d.set(b"Resources", res)).unwrap();
        let before = bytes(&doc);
        let err = convert(&mut doc, tag, false).unwrap_err().to_string();
        assert!(err.contains("inlined"), "{err}");
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn indirect_contents_chains_and_stale_array_members_refuse_atomically() {
        for case in 0..3 {
            let (mut doc, tag, _, page, _) = fixture(b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC");
            let stream = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
            let stale = ObjRef::new(stream.num, stream.generation + 1);
            let contents = match case {
                0 => Object::Ref(doc.add(Object::Ref(stream))),
                1 => Object::Ref(doc.add(Object::Ref(stale))),
                _ => Object::Ref(doc.add(Object::Array(vec![Object::Ref(stale)]))),
            };
            doc.update_dict(page, |d| d.set(b"Contents", contents)).unwrap();
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn malformed_or_indirect_decode_parameters_refuse_atomically() {
        for case in 0..8 {
            let content = b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC";
            let (mut doc, tag, _, page, _) = fixture(content);
            let id = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
            let mut stream = Stream::flate(Dict::new(), content);
            let mut params = Dict::new();
            params.set(b"Predictor", 1);
            let indirect = doc.add(params.clone());
            match case {
                0 => stream.dict.set(b"DecodeParms", Object::Ref(indirect)),
                1 => stream.dict.set(b"DecodeParms", Object::name("invalid")),
                2 => {
                    params.set(b"Predictor", Object::Ref(indirect));
                    stream.dict.set(b"DecodeParms", params);
                }
                3 => {
                    stream.dict.set(b"Filter", Object::Array(vec![Object::name("FlateDecode")]));
                    stream.dict.set(b"DecodeParms", Object::Array(vec![Object::Ref(indirect)]));
                }
                4 => {
                    stream.dict.set(b"Filter", Object::Array(vec![Object::name("FlateDecode")]));
                    stream.dict.set(b"DecodeParms", Object::Array(Vec::new()));
                }
                5 => {
                    params.set(b"Columns", 0);
                    stream.dict.set(b"DecodeParms", params);
                }
                6 => {
                    params.set(b"Predictor", Object::Real(1.0));
                    stream.dict.set(b"DecodeParms", params);
                }
                _ => {
                    params.set(b"Private", true);
                    stream.dict.set(b"DecodeParms", params);
                }
            }
            doc.set(id, Object::Stream(stream));
            let before = bytes(&doc);
            assert!(convert(&mut doc, tag, false).is_err(), "case {case}");
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn direct_decode_parameter_dictionary_and_matching_null_array_are_supported() {
        for array in [false, true] {
            let content = b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC";
            let (mut doc, tag, _, page, _) = fixture(content);
            let id = doc.get(page).as_dict().unwrap().reference(b"Contents").unwrap();
            let mut stream = Stream::flate(Dict::new(), content);
            if array {
                stream.dict.set(b"Filter", Object::Array(vec![Object::name("FlateDecode")]));
                stream.dict.set(b"DecodeParms", Object::Array(vec![Object::Null]));
            } else {
                let mut params = Dict::new();
                params.set(b"Predictor", 1);
                stream.dict.set(b"DecodeParms", params);
            }
            doc.set(id, Object::Stream(stream));
            convert(&mut doc, tag, false).unwrap();
            assert!(page_content(&doc, page).starts_with(b"/Artifact"));
        }
    }
    #[test]
    fn unreadable_live_object_cannot_be_skipped_when_proving_form_references() {
        let (doc, _, _, _, _) = form_fixture();
        let base = bytes(&doc);
        let reopened = Document::open(std::sync::Arc::new(base.clone())).unwrap();
        let next = reopened.object_numbers().into_iter().max().unwrap() + 1;
        let root = reopened.root().unwrap();
        let text = String::from_utf8_lossy(&base);
        let prev: usize = text.rsplit_once("startxref\n").unwrap().1.lines().next().unwrap().parse().unwrap();
        let mut corrupted = base;
        let object_offset = corrupted.len();
        corrupted.extend_from_slice(format!("\n{next} 0 obj\ninvalid invalid\nendobj\n").as_bytes());
        let xref_offset = corrupted.len();
        corrupted.extend_from_slice(
            format!(
                "xref\n{next} 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size {} /Root {root} /Prev {prev} >>\nstartxref\n{xref_offset}\n%%EOF\n",
                next + 1
            )
            .as_bytes(),
        );
        let mut doc = Document::open(std::sync::Arc::new(corrupted.clone())).unwrap();
        assert!(doc.object_numbers().contains(&next));
        assert!(doc.try_get(next).is_err());
        let tree = tags::inspect(&doc).unwrap();
        let tag = tree.elements.iter().find(|element| element.kids.iter().any(|kid| matches!(kid, TagKid::Mcr { stream: Some(_), .. }))).unwrap().id;
        let forms = tree
            .element(tag)
            .unwrap()
            .kids
            .iter()
            .filter_map(|kid| match kid {
                TagKid::Mcr { stream, .. } => *stream,
                _ => None,
            })
            .collect();
        let err = verify_form_references(&doc, &forms, &mut ArtifactBudget::default()).unwrap_err().to_string();
        assert!(err.contains("cannot read exact content object"), "{err}");
        assert!(convert(&mut doc, tag, false).is_err());
        assert_eq!(&**doc.bytes(), &corrupted);
        assert!(!doc.is_modified());
        assert!(doc.modified_objects().is_empty());
    }
    #[test]
    fn inherited_actual_pages_resources_support_single_invocation_form() {
        let (mut doc, tag, page, _, form) = form_fixture();
        let page_dict = doc.get(page);
        let page_dict = page_dict.as_dict().unwrap();
        let parent = page_dict.reference(b"Parent").unwrap();
        let res = page_dict.get(b"Resources").unwrap().clone();
        doc.update_dict(parent, |dict| dict.set(b"Resources", res)).unwrap();
        doc.update_dict(page, |dict| {
            dict.remove(b"Resources");
        })
        .unwrap();
        let before = doc.get(parent);
        convert(&mut doc, tag, false).unwrap();
        assert_eq!(doc.get(parent), before);
        let value = doc.get(form);
        let Object::Stream(stream) = &*value else { panic!() };
        assert!(stream.decoded().unwrap().starts_with(b"/Artifact"));
    }
    #[test]
    fn stream_catalog_cannot_establish_resource_ownership() {
        let (mut doc, _, _, _, _) = form_fixture();
        let root = doc.root().unwrap();
        let dict = doc.get(root).as_dict().unwrap().clone();
        doc.set(root, Object::Stream(Stream::from_raw(dict, Vec::new())));
        let before = bytes(&doc);
        assert!(page_resource_owners(&doc, &mut ArtifactBudget::default()).unwrap_err().to_string().contains("exact dictionary"));
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn page_only_conversion_rejects_stale_ancestor_and_indirect_kids_references() {
        for stale_ancestor in [true, false] {
            let (mut doc, tag, _, page, _) = fixture(b"/P <</MCID 0>> BDC 1 2 3 4 re f EMC");
            let parent = doc.get(page).as_dict().unwrap().reference(b"Parent").unwrap();
            if stale_ancestor {
                doc.set(ObjRef::new(parent.num, parent.generation + 1), (*doc.get(parent)).clone());
            } else {
                let kids = doc.add(doc.get(parent).as_dict().unwrap().get(b"Kids").unwrap().clone());
                doc.update_dict(parent, |dict| dict.set(b"Kids", Object::Ref(kids))).unwrap();
                doc.set(ObjRef::new(kids.num, kids.generation + 1), (*doc.get(kids)).clone());
            }
            let before = bytes(&doc);
            let err = convert(&mut doc, tag, false).unwrap_err().to_string();
            assert!(err.contains("stale"), "{err}");
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn page_only_inherited_indirect_resources_require_exact_generation() {
        for stale in [false, true] {
            let (mut doc, tag, _, page, _) = fixture(b"/P /Named BDC 1 2 3 4 re f EMC");
            let parent = doc.get(page).as_dict().unwrap().reference(b"Parent").unwrap();
            let mut named = Dict::new();
            named.set(b"MCID", 0);
            let mut props = Dict::new();
            props.set(b"Named", named);
            let mut res = Dict::new();
            res.set(b"Properties", props);
            let res = doc.add(res);
            doc.update_dict(parent, |dict| dict.set(b"Resources", Object::Ref(res))).unwrap();
            if stale {
                doc.set(ObjRef::new(res.num, res.generation + 1), (*doc.get(res)).clone());
            }
            let before = bytes(&doc);
            assert_eq!(convert(&mut doc, tag, false).is_ok(), !stale);
            if stale {
                assert_eq!(bytes(&doc), before);
            }
        }
    }
}
