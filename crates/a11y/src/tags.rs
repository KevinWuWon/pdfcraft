//! Identity-preserving structure-tree inspection and conservative editing.
//! Content streams are never rewritten by this module.
use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString};
use std::collections::{BTreeMap, BTreeSet};

const LIMIT: usize = 100_000;
const DEPTH: usize = 128;
const DELETE_BATCH_WORK_LIMIT: usize = 1_000_000;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TagError {
    #[error("{0}")]
    Invalid(String),
}
type Result<T> = std::result::Result<T, TagError>;
fn invalid(message: impl Into<String>) -> TagError {
    TagError::Invalid(message.into())
}

#[derive(Clone, Debug, PartialEq)]
pub enum TagKid {
    Element { id: ObjRef },
    Mcid { id: i64, page: Option<ObjRef> },
    Mcr { id: i64, page: Option<ObjRef>, stream: Option<ObjRef> },
    Objr { object: ObjRef, page: Option<ObjRef> },
    Unsupported { description: String },
}
#[derive(Clone, Debug, PartialEq)]
pub struct TagElement {
    pub id: ObjRef,
    /// None denotes the structure-tree root.
    pub parent: Option<ObjRef>,
    pub role: String,
    pub standard_role: String,
    /// Effective page, including inheritance from ancestors.
    pub page: Option<ObjRef>,
    pub page_index: Option<usize>,
    pub expansion: Option<String>,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub alt: Option<String>,
    pub actual_text: Option<String>,
    pub lang: Option<String>,
    pub kids: Vec<TagKid>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagTree {
    pub role_map: BTreeMap<String, String>,
    pub root: Option<ObjRef>,
    pub pages: Vec<ObjRef>,
    pub children: Vec<TagKid>,
    /// Preorder, with persistent indirect object identities.
    pub elements: Vec<TagElement>,
    /// Unsupported/malformed constructs are visible and block edits.
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagProperties {
    pub role: Option<String>,
    /// Outer None leaves unchanged; Some(None) removes the property.
    pub expansion: Option<Option<String>>,
    pub title: Option<Option<String>>,
    pub alt: Option<Option<String>>,
    pub actual_text: Option<Option<String>>,
    pub lang: Option<Option<String>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    Unwrap,
    Subtree,
}

impl TagTree {
    pub fn element(&self, id: ObjRef) -> Option<&TagElement> {
        self.elements.iter().find(|e| e.id == id)
    }
}

fn dictionary(doc: &Document, id: ObjRef) -> Result<Dict> {
    if doc.generation(id.num) != id.generation {
        return Err(invalid(format!("stale object generation: {id}")));
    }
    match &*doc.get(id) {
        Object::Dict(d) => Ok(d.clone()),
        _ => Err(invalid(format!("{id} must be an indirect dictionary"))),
    }
}
fn raw_kids(doc: &Document, d: &Dict) -> Result<Vec<Object>> {
    let Some(k) = d.get(b"K") else { return Ok(Vec::new()) };
    let value = doc.resolve(k);
    match &*value {
        Object::Array(a) if a.len() <= LIMIT => Ok(a.clone()),
        Object::Array(_) => Err(invalid("too many structure children")),
        Object::Null => Err(invalid("null or missing structure child")),
        _ => Ok(vec![k.clone()]),
    }
}
fn text(doc: &Document, d: &Dict, key: &[u8]) -> Option<String> {
    d.get(key).map(|v| doc.resolve(v)).and_then(|v| v.as_string().map(PdfString::to_text))
}
fn resolved_role(doc: &Document, map: &Dict, name: &[u8], issues: &mut Vec<String>) -> String {
    let mut role = name.to_vec();
    let mut seen = BTreeSet::new();
    for _ in 0..DEPTH {
        if !seen.insert(role.clone()) {
            issues.push("cyclic RoleMap".into());
            break;
        }
        if crate::structure::is_standard(&role) {
            return String::from_utf8_lossy(&role).into_owned();
        }
        let Some(next) = map.get(&role) else { return String::from_utf8_lossy(&role).into_owned() };
        let next = doc.resolve(next);
        let Some(next) = next.as_name() else {
            issues.push("RoleMap value must be a name".into());
            break;
        };
        role = next.to_vec();
    }
    String::from_utf8_lossy(&role).into_owned()
}
fn classify(doc: &Document, kid: &Object, page: Option<ObjRef>) -> TagKid {
    if let Some(id) = kid.as_ref()
        && doc.generation(id.num) != id.generation
    {
        return TagKid::Unsupported { description: "stale child object generation".into() };
    }
    let value = doc.resolve(kid);
    if let Object::Dict(d) = &*value {
        for key in [b"Pg".as_slice(), b"Stm"] {
            if d.contains(key) && d.reference(key).is_none() {
                return TagKid::Unsupported { description: "content Pg/Stm must be indirect references".into() };
            }
        }
    }
    match &*value {
        Object::Int(id) if *id >= 0 => TagKid::Mcid { id: *id, page },
        Object::Dict(d) if d.contains(b"S") => match kid.as_ref() {
            Some(id) => TagKid::Element { id },
            None => TagKid::Unsupported { description: "direct StructElem cannot be addressed safely".into() },
        },
        Object::Dict(d) if d.name(b"Type") == Some(b"MCR") => match d.int(b"MCID").filter(|id| *id >= 0) {
            Some(id) => TagKid::Mcr { id, page: d.reference(b"Pg").or(page), stream: d.reference(b"Stm") },
            None => TagKid::Unsupported { description: "MCR needs a nonnegative MCID".into() },
        },
        Object::Dict(d) if d.name(b"Type") == Some(b"OBJR") => match d.reference(b"Obj") {
            Some(object) => TagKid::Objr { object, page: d.reference(b"Pg").or(page) },
            None => TagKid::Unsupported { description: "OBJR needs an indirect Obj".into() },
        },
        _ => TagKid::Unsupported { description: "unsupported or missing structure child".into() },
    }
}

/// Inspect without repairing or renumbering. A malformed graph is reported in `issues`.
pub fn inspect(doc: &Document) -> Result<TagTree> {
    let catalog = dictionary(doc, doc.root().ok_or_else(|| invalid("missing catalog"))?)?;
    let Some(root_obj) = catalog.get(b"StructTreeRoot") else {
        return Ok(TagTree { pages: pdfcraft_model::pages(doc).into_iter().map(|p| p.obj).collect(), ..Default::default() });
    };
    let root = root_obj.as_ref();
    let d = match root {
        Some(root) => dictionary(doc, root)?,
        None => doc.dict(root_obj).ok_or_else(|| invalid("invalid StructTreeRoot"))?,
    };
    let mut tree = TagTree { root, ..Default::default() };
    if root.is_none() {
        tree.issues.push("direct StructTreeRoot is read-only".into());
    }
    let pages = pdfcraft_model::pages(doc);
    tree.pages = pages.iter().map(|p| p.obj).collect();
    let page_indices: BTreeMap<_, _> = pages.iter().enumerate().map(|(i, p)| (p.obj, i)).collect();
    if d.name(b"Type") != Some(b"StructTreeRoot") {
        tree.issues.push("StructTreeRoot has wrong Type".into());
    }
    let role_map = d.get(b"RoleMap").and_then(|v| doc.dict(v)).unwrap_or_default();
    if d.contains(b"RoleMap") && d.get(b"RoleMap").and_then(|v| doc.dict(v)).is_none() {
        tree.issues.push("RoleMap must be a dictionary".into());
    }
    if let Some(id) = d.reference(b"RoleMap")
        && doc.generation(id.num) != id.generation
    {
        tree.issues.push("stale RoleMap generation".into());
    }
    if let Err(error) = validate_role_map(doc, &role_map) {
        tree.issues.push(error.to_string());
    }
    tree.role_map = role_map
        .iter()
        .take(LIMIT)
        .filter_map(|(name, value)| {
            let value = doc.resolve(value);
            value.as_name().map(|target| (String::from_utf8_lossy(name).into_owned(), String::from_utf8_lossy(target).into_owned()))
        })
        .collect();
    let roots = raw_kids(doc, &d)?;
    tree.children = roots.iter().map(|k| classify(doc, k, None)).collect();
    let mut stack: Vec<_> = roots.into_iter().rev().map(|k| (k, None, None, 0usize)).collect();
    let mut seen = BTreeSet::new();
    let mut child_arrays = BTreeSet::new();
    if let Some(array) = d.reference(b"K")
        && matches!(&*doc.get(array), Object::Array(_))
    {
        child_arrays.insert(array);
    }
    let mut content_dictionaries = BTreeSet::new();
    let mut visited = 0usize;
    while let Some((raw, parent, inherited, depth)) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > LIMIT || depth > DEPTH {
            tree.issues.push("structure tree exceeds safe inspection limits".into());
            break;
        }
        if let Some(id) = raw.as_ref()
            && doc.get(id).as_dict().is_some_and(|d| matches!(d.name(b"Type"), Some(b"MCR" | b"OBJR")))
            && !content_dictionaries.insert(id)
        {
            tree.issues.push("shared indirect content dictionary is read-only".into());
        }
        match classify(doc, &raw, inherited) {
            TagKid::Element { id } => {
                if !seen.insert(id) {
                    tree.issues.push(format!("cycle or shared structure element {id}"));
                    continue;
                }
                let d = dictionary(doc, id)?;
                if d.contains(b"NS") {
                    tree.issues.push(format!("{id} uses an unsupported PDF 2.0 namespace"));
                }
                // ISO 32000 structure element Type is optional; when supplied it must
                // identify StructElem. Do not normalize valid omitted-Type elements.
                if d.contains(b"Type") && d.name(b"Type") != Some(b"StructElem") {
                    tree.issues.push(format!("{id} has invalid StructElem Type"));
                }
                if parent.or(root).is_some() && d.reference(b"P") != parent.or(root) {
                    tree.issues.push(format!("{id} has inconsistent parent P"));
                }
                if d.contains(b"Pg") && d.reference(b"Pg").is_none() {
                    tree.issues.push(format!("{id} has invalid Pg"));
                }
                let page = d.reference(b"Pg").or(inherited);
                let name = d.name(b"S").unwrap_or_default();
                if name.is_empty() {
                    tree.issues.push(format!("{id} has invalid structure type S"));
                }
                if let Some(array) = d.reference(b"K")
                    && matches!(&*doc.get(array), Object::Array(_))
                    && !child_arrays.insert(array)
                {
                    tree.issues.push("shared indirect K array".into());
                }
                let kids = raw_kids(doc, &d)?;
                if stack.len().saturating_add(kids.len()).saturating_add(visited) > LIMIT {
                    tree.issues.push("structure tree exceeds safe inspection limits".into());
                    break;
                }
                let element = TagElement {
                    id,
                    parent,
                    role: String::from_utf8_lossy(name).into_owned(),
                    standard_role: resolved_role(doc, &role_map, name, &mut tree.issues),
                    page,
                    page_index: page.and_then(|p| page_indices.get(&p).copied()),
                    expansion: text(doc, &d, b"E"),
                    identifier: text(doc, &d, b"ID"),
                    title: text(doc, &d, b"T"),
                    alt: text(doc, &d, b"Alt"),
                    actual_text: text(doc, &d, b"ActualText"),
                    lang: text(doc, &d, b"Lang"),
                    kids: kids.iter().map(|k| classify(doc, k, page)).collect(),
                };
                stack.extend(kids.into_iter().rev().map(|k| (k, Some(id), page, depth.saturating_add(1))));
                tree.elements.push(element);
            }
            TagKid::Unsupported { description } => tree.issues.push(description),
            _ if parent.is_none() => tree.issues.push("content item cannot be directly under StructTreeRoot".into()),
            _ => {}
        }
    }
    match parent_entries(doc, &d) {
        Ok(entries) => validate_associations(doc, &mut tree, &entries),
        Err(e) => tree.issues.push(e.to_string()),
    }
    Ok(tree)
}
fn editable(doc: &Document) -> Result<TagTree> {
    let tree = inspect(doc)?;
    if let Some(issue) = tree.issues.first() {
        return Err(invalid(format!("cannot edit malformed/unsupported tags: {issue}")));
    }
    Ok(tree)
}
fn element(tree: &TagTree, id: ObjRef) -> Result<&TagElement> {
    tree.elements.iter().find(|e| e.id == id).ok_or_else(|| invalid(format!("{id} is not a reachable structure element")))
}
fn parent_id(tree: &TagTree, parent: Option<ObjRef>) -> Result<ObjRef> {
    match parent {
        Some(id) => {
            element(tree, id)?;
            Ok(id)
        }
        None => tree.root.ok_or_else(|| invalid("document has no structure tree")),
    }
}
fn properties(d: &mut Dict, p: &TagProperties) -> Result<()> {
    if let Some(role) = &p.role {
        if role.is_empty() || role.len() > 127 || !role.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(invalid("tag type must contain 1–127 ASCII letters, digits, underscores or hyphens"));
        }
        d.set(b"S", Object::Name(role.as_bytes().to_vec()));
    }
    for (key, val) in [(b"E".as_slice(), &p.expansion), (b"T", &p.title), (b"Alt", &p.alt), (b"ActualText", &p.actual_text), (b"Lang", &p.lang)] {
        if let Some(val) = val {
            match val {
                Some(value) if value.len() <= 1_048_576 => d.set(key, Object::String(PdfString::text(value))),
                Some(_) => return Err(invalid("tag text exceeds 1 MiB")),
                None => {
                    d.remove(key);
                }
            }
        }
    }
    Ok(())
}
fn set_kids(d: &mut Dict, kids: Vec<Object>) {
    if kids.is_empty() {
        d.remove(b"K");
    } else {
        d.set(b"K", Object::Array(kids));
    }
}

/// Update only explicitly supplied properties, retaining all other dictionary entries.
pub fn update(doc: &mut Document, id: ObjRef, p: &TagProperties) -> Result<()> {
    let tree = editable(doc)?;
    validate_requested_role(doc, &tree, p)?;
    element(&tree, id)?;
    let mut d = dictionary(doc, id)?;
    properties(&mut d, p)?;
    commit_dict(doc, id, d);
    Ok(())
}
/// Create an empty tag at an ordered child slot. Creates a root on untagged documents.
pub fn create(doc: &mut Document, parent: Option<ObjRef>, index: usize, p: &TagProperties) -> Result<ObjRef> {
    let tree = editable(doc)?;
    validate_requested_role(doc, &tree, p)?;
    let mut d = Dict::new();
    d.set(b"Type", Object::Name(b"StructElem".to_vec()));
    d.set(b"S", Object::Name(if tree.root.is_none() { b"Document".to_vec() } else { b"P".to_vec() }));
    properties(&mut d, p)?;
    if tree.root.is_none() {
        if parent.is_some() || index != 0 {
            return Err(invalid("an untagged document requires root insertion at index 0"));
        }
        let cat_id = doc.root().ok_or_else(|| invalid("missing catalog"))?;
        let mut cat = dictionary(doc, cat_id)?;
        let mut mark = match cat.get(b"MarkInfo") {
            Some(o) => doc.dict(o).ok_or_else(|| invalid("invalid MarkInfo"))?,
            None => Dict::new(),
        };
        mark.set(b"Marked", Object::Bool(true));
        let mut root = Dict::new();
        root.set(b"Type", Object::Name(b"StructTreeRoot".to_vec()));
        let root_id = doc.add(root.clone());
        d.set(b"P", Object::Ref(root_id));
        let id = doc.add(d);
        root.set(b"K", Object::Array(vec![Object::Ref(id)]));
        doc.set(root_id, root);
        cat.set(b"StructTreeRoot", Object::Ref(root_id));
        cat.set(b"MarkInfo", Object::Dict(mark));
        doc.set(cat_id, cat);
        return Ok(id);
    }
    let pid = parent_id(&tree, parent)?;
    let mut pd = dictionary(doc, pid)?;
    let mut kids = raw_kids(doc, &pd)?;
    if index > kids.len() {
        return Err(invalid("insertion index exceeds child count"));
    }
    d.set(b"P", Object::Ref(pid));
    let id = doc.add(d);
    kids.insert(index, Object::Ref(id));
    set_kids(&mut pd, kids);
    commit_dict(doc, pid, pd);
    Ok(id)
}

/// Move within or between parents. Index is in the destination list after removing the tag.
pub fn move_tag(doc: &mut Document, id: ObjRef, parent: Option<ObjRef>, index: usize) -> Result<()> {
    let tree = editable(doc)?;
    let e = element(&tree, id)?;
    let new_parent = parent_id(&tree, parent)?;
    let old_parent = parent_id(&tree, e.parent)?;
    let mut ancestor = parent;
    while let Some(a) = ancestor {
        if a == id {
            return Err(invalid("cannot move a tag into itself or its descendant"));
        }
        ancestor = element(&tree, a)?.parent;
    }
    let mut old = dictionary(doc, old_parent)?;
    let mut old_kids = raw_kids(doc, &old)?;
    let pos = old_kids.iter().position(|k| k.as_ref() == Some(id)).ok_or_else(|| invalid("tag is missing from its parent"))?;
    let raw = old_kids.remove(pos);
    let mut new = if old_parent == new_parent { old.clone() } else { dictionary(doc, new_parent)? };
    let mut new_kids = if old_parent == new_parent { old_kids.clone() } else { raw_kids(doc, &new)? };
    if index > new_kids.len() {
        return Err(invalid("destination index exceeds child count after removal"));
    }
    let mut moved = dictionary(doc, id)?;
    // Pin its old effective page before changing ancestry; descendants retain inheritance.
    if let Some(pg) = e.page {
        moved.set(b"Pg", Object::Ref(pg));
    } else if parent.and_then(|p| tree.elements.iter().find(|e| e.id == p)).and_then(|e| e.page).is_some() {
        return Err(invalid("move would give page-less content a new inherited page; set a valid page first"));
    }
    moved.set(b"P", Object::Ref(new_parent));
    new_kids.insert(index, raw);
    set_kids(&mut new, new_kids);
    if old_parent != new_parent {
        set_kids(&mut old, old_kids);
        commit_dict(doc, old_parent, old);
    }
    commit_dict(doc, new_parent, new);
    commit_dict(doc, id, moved);
    Ok(())
}

#[derive(Clone)]
struct IdNode {
    original: Object,
    dict: Dict,
    children: Vec<IdNode>,
    entries: Vec<(PdfString, ObjRef)>,
}
fn read_id_node(
    doc: &Document,
    obj: &Object,
    depth: usize,
    seen: &mut BTreeSet<ObjRef>,
    budget: &mut usize,
    string_bytes: &mut usize,
) -> Result<IdNode> {
    *budget = budget.saturating_add(1);
    if depth > DEPTH || *budget > LIMIT {
        return Err(invalid("IDTree exceeds safe limits"));
    }
    if let Some(id) = obj.as_ref()
        && (doc.generation(id.num) != id.generation || !seen.insert(id))
    {
        return Err(invalid("stale, cyclic or shared IDTree node"));
    }
    let dict = doc.dict(obj).ok_or_else(|| invalid("IDTree node must be a dictionary"))?;
    if depth > 0 && !dict.contains(b"Limits") {
        return Err(invalid("non-root IDTree node requires Limits"));
    }
    if dict.contains(b"Kids") == dict.contains(b"Names") {
        return Err(invalid("IDTree node needs exactly one of Kids or Names"));
    }
    let mut node = IdNode { original: obj.clone(), dict, children: Vec::new(), entries: Vec::new() };
    let key = if node.dict.contains(b"Kids") { b"Kids".as_slice() } else { b"Names" };
    let raw = node.dict.get(key).ok_or_else(|| invalid("missing IDTree entries"))?;
    if let Some(id) = raw.as_ref()
        && (doc.generation(id.num) != id.generation || !seen.insert(id))
    {
        return Err(invalid("stale or shared IDTree array"));
    }
    let resolved = doc.resolve(raw);
    let values = resolved.as_array().ok_or_else(|| invalid("IDTree entries must be an array"))?;
    *budget = budget.saturating_add(values.len());
    if *budget > LIMIT {
        return Err(invalid("IDTree exceeds safe limits"));
    }
    if key == b"Kids" {
        if values.is_empty() {
            return Err(invalid("IDTree has empty Kids"));
        }
        for child in values {
            node.children.push(read_id_node(doc, child, depth.saturating_add(1), seen, budget, string_bytes)?);
        }
    } else {
        if values.len() % 2 != 0 {
            return Err(invalid("IDTree Names must contain name/value pairs"));
        }
        for pair in values.as_chunks::<2>().0 {
            let name = pair.first().and_then(Object::as_string).ok_or_else(|| invalid("IDTree key must be a byte string"))?;
            *string_bytes = string_bytes.saturating_add(name.bytes.len());
            if name.bytes.len() > 1_048_576 || *string_bytes > 16_777_216 {
                return Err(invalid("IDTree key exceeds safe limits"));
            }
            let id = pair.get(1).and_then(Object::as_ref).ok_or_else(|| invalid("IDTree value must be a tag reference"))?;
            dictionary(doc, id)?;
            node.entries.push((name.clone(), id));
        }
    }
    Ok(node)
}
fn collect_ids(node: &IdNode, out: &mut Vec<(PdfString, ObjRef)>, leaves: &mut usize) -> Result<()> {
    let first = out.len();
    if node.children.is_empty() {
        *leaves = leaves.saturating_add(1);
        out.extend(node.entries.iter().cloned());
    } else {
        for child in &node.children {
            collect_ids(child, out, leaves)?;
        }
    }
    if let Some(limits) = node.dict.get(b"Limits") {
        let limits = limits.as_array().filter(|a| a.len() == 2).ok_or_else(|| invalid("IDTree Limits must be two direct strings"))?;
        let lo = limits.first().and_then(Object::as_string).ok_or_else(|| invalid("invalid IDTree Limits"))?;
        let hi = limits.get(1).and_then(Object::as_string).ok_or_else(|| invalid("invalid IDTree Limits"))?;
        if out.get(first).map(|p| &p.0.bytes) != Some(&lo.bytes) || out.last().map(|p| &p.0.bytes) != Some(&hi.bytes) {
            return Err(invalid("IDTree Limits do not match names"));
        }
    }
    Ok(())
}
fn rewrite_ids(
    node: &IdNode,
    is_root: bool,
    entries: &[(PdfString, ObjRef)],
    cursor: &mut usize,
    leaves: &mut usize,
    changes: &mut BTreeMap<ObjRef, Dict>,
) -> Result<(Object, Option<(PdfString, PdfString)>)> {
    let mut d = node.dict.clone();
    let start = *cursor;
    if node.children.is_empty() {
        *leaves = leaves.saturating_sub(1);
        let remaining = entries.len().saturating_sub(*cursor);
        let count = if *leaves == 0 { remaining } else { node.entries.len().max(1).min(remaining.saturating_sub(*leaves)) };
        let end = cursor.checked_add(count).ok_or_else(|| invalid("IDTree size overflow"))?;
        let slice = entries.get(*cursor..end).ok_or_else(|| invalid("IDTree distribution failure"))?;
        let values = slice.iter().flat_map(|(name, id)| [Object::String(name.clone()), Object::Ref(*id)]).collect();
        d.set(b"Names", Object::Array(values));
        *cursor = end;
    } else {
        let mut children = Vec::new();
        for child in &node.children {
            children.push(rewrite_ids(child, false, entries, cursor, leaves, changes)?.0);
        }
        d.set(b"Kids", Object::Array(children));
    }
    let range = entries.get(start..*cursor).filter(|s| !s.is_empty()).and_then(|s| Some((s.first()?.0.clone(), s.last()?.0.clone())));
    if !is_root || d.contains(b"Limits") {
        if let Some((lo, hi)) = &range {
            d.set(b"Limits", Object::Array(vec![Object::String(lo.clone()), Object::String(hi.clone())]));
        } else {
            d.remove(b"Limits");
        }
    }
    if let Some(id) = node.original.as_ref() {
        changes.insert(id, d);
        Ok((Object::Ref(id), range))
    } else {
        // Preserve indirect Names/Kids arrays even under direct dictionary nodes.
        for key in [b"Names".as_slice(), b"Kids"] {
            if node.dict.reference(key).is_some() {
                return Err(invalid("direct IDTree nodes with indirect arrays are not editable"));
            }
        }
        Ok((Object::Dict(d), range))
    }
}
/// Set/clear a tag ID and its name-tree entry together, preserving raw-byte ordering and
/// existing name-tree nodes, unknown keys and indirect arrays. ID-sensitive references
/// currently block changes. External fragment links cannot be discovered or rewritten.
pub fn set_identifier(doc: &mut Document, id: ObjRef, identifier: Option<&str>) -> Result<()> {
    let tree = editable(doc)?;
    element(&tree, id)?;
    let root = tree.root.ok_or_else(|| invalid("missing structure root"))?;
    let mut rd = dictionary(doc, root)?;
    let mut td = dictionary(doc, id)?;
    let new = match identifier {
        Some(s) if s.is_empty() || s.len() > 1_048_576 => return Err(invalid("identifier must contain 1–1048576 bytes")),
        Some(s) => Some(PdfString::text(s)),
        None => None,
    };
    // Refuse known ID-sensitive destinations and table-header references until they can
    // be updated alongside the ID. This scan is bounded and does not chase cycles.
    let objects = doc.object_numbers();
    if objects.len() > LIMIT {
        return Err(invalid("document too large to verify ID references"));
    }
    let mut budget = 0usize;
    for num in objects {
        let value = doc.get(ObjRef::new(num, doc.generation(num)));
        let mut stack = vec![(&*value, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            budget = budget.saturating_add(1);
            if budget > LIMIT || depth > DEPTH {
                return Err(invalid("ID reference scan exceeds safe limits"));
            }
            if let Some(d) = value.as_dict() {
                if d.contains(b"Headers") || d.contains(b"SD") {
                    return Err(invalid("ID editing with Headers or structure destinations is not supported"));
                }
                stack.extend(d.iter().map(|(_, v)| (v, depth.saturating_add(1))));
            } else if let Object::Array(a) = value {
                stack.extend(a.iter().map(|v| (v, depth.saturating_add(1))));
            }
            if stack.len().saturating_add(budget) > LIMIT {
                return Err(invalid("ID reference scan exceeds safe limits"));
            }
        }
    }
    let mut entries = Vec::new();
    let mut leaves = 0usize;
    let node = if let Some(obj) = rd.get(b"IDTree") {
        let mut seen: BTreeSet<_> = tree.elements.iter().map(|e| e.id).chain(std::iter::once(root)).collect();
        let node = read_id_node(doc, obj, 0, &mut seen, &mut 0, &mut 0)?;
        collect_ids(&node, &mut entries, &mut leaves)?;
        Some(node)
    } else {
        None
    };
    if entries.windows(2).any(|p| p.first().zip(p.get(1)).is_some_and(|(a, b)| a.0.bytes >= b.0.bytes)) {
        return Err(invalid("IDTree names must be unique and sorted by raw bytes"));
    }
    let mut expected = BTreeMap::new();
    let mut identifier_bytes = 0usize;
    for e in &tree.elements {
        let d = dictionary(doc, e.id)?;
        if let Some(raw) = d.get(b"ID") {
            if let Some(r) = raw.as_ref()
                && doc.generation(r.num) != r.generation
            {
                return Err(invalid("stale tag ID generation"));
            }
            let raw = doc.resolve(raw);
            let value = raw.as_string().ok_or_else(|| invalid("tag ID must be a byte string"))?;
            identifier_bytes = identifier_bytes.saturating_add(value.bytes.len());
            if value.bytes.len() > 1_048_576 || identifier_bytes > 16_777_216 {
                return Err(invalid("tag IDs exceed safe limits"));
            }
            if expected.insert(value.bytes.clone(), e.id).is_some() {
                return Err(invalid("duplicate tag ID"));
            }
        }
    }
    if entries.len() != expected.len() || entries.iter().any(|(name, id)| expected.get(&name.bytes) != Some(id)) {
        return Err(invalid("IDTree does not exactly match reachable tag IDs"));
    }
    if let Some(new) = &new
        && expected.get(&new.bytes).is_some_and(|owner| *owner != id)
    {
        return Err(invalid("identifier already belongs to another tag"));
    }
    entries.retain(|(_, owner)| *owner != id);
    if let Some(new) = &new {
        entries.push((new.clone(), id));
        td.set(b"ID", Object::String(new.clone()));
    } else {
        td.remove(b"ID");
    }
    entries.sort_by(|a, b| a.0.bytes.cmp(&b.0.bytes));
    let mut changes = BTreeMap::new();
    if let Some(node) = node {
        if leaves > 1 && entries.len() < leaves {
            return Err(invalid("ID removal would empty a name-tree leaf; rebalance is not supported"));
        }
        let (rewritten, _) = rewrite_ids(&node, true, &entries, &mut 0, &mut leaves, &mut changes)?;
        rd.set(b"IDTree", rewritten);
    } else if !entries.is_empty() {
        let mut d = Dict::new();
        d.set(b"Names", Object::Array(entries.iter().flat_map(|(name, id)| [Object::String(name.clone()), Object::Ref(*id)]).collect()));
        rd.set(b"IDTree", Object::Dict(d));
    }
    changes.insert(id, td);
    changes.insert(root, rd);
    for (id, d) in changes {
        commit_dict(doc, id, d);
    }
    Ok(())
}

fn selected_roots<'a>(tree: &'a TagTree, ids: &[ObjRef]) -> Result<Vec<&'a TagElement>> {
    if ids.is_empty() || ids.len() > 4096 {
        return Err(invalid("select 1–4096 tags"));
    }
    if tree.elements.len() > LIMIT {
        return Err(invalid("selection tree exceeds safe limits"));
    }
    // Build once: ancestor checks must not repeatedly scan a potentially large tree.
    // Keep output in original document order, independent of selection/input order.
    let parents: BTreeMap<_, _> = tree.elements.iter().map(|e| (e.id, e.parent)).collect();
    if parents.len() != tree.elements.len() {
        return Err(invalid("duplicate structure identity in selection tree"));
    }
    let selected: BTreeSet<_> = ids.iter().copied().collect();
    for id in &selected {
        if !parents.contains_key(id) {
            return Err(invalid(format!("{id} is not a reachable structure element")));
        }
    }
    let mut out = Vec::new();
    for e in &tree.elements {
        if !selected.contains(&e.id) {
            continue;
        }
        let mut ancestor = e.parent;
        let mut covered = false;
        let mut seen = BTreeSet::from([e.id]);
        while let Some(id) = ancestor {
            if !seen.insert(id) {
                return Err(invalid("cyclic parent chain in selection tree"));
            }
            if seen.len() > DEPTH.saturating_add(1) {
                return Err(invalid("selection parent chain exceeds safe limits"));
            }
            covered |= selected.contains(&id);
            ancestor = *parents.get(&id).ok_or_else(|| invalid(format!("{id} is not a reachable structure element")))?;
        }
        if !covered {
            out.push(e);
        }
    }
    Ok(out)
}

/// Move a selection atomically in document order. Selected descendants of selected
/// ancestors are covered by their ancestor. Index is measured after all removals.
pub fn move_tags(doc: &mut Document, ids: &[ObjRef], parent: Option<ObjRef>, index: usize) -> Result<()> {
    let tree = editable(doc)?;
    let selected = selected_roots(&tree, ids)?;
    let selected_ids: BTreeSet<_> = selected.iter().map(|e| e.id).collect();
    let destination = parent_id(&tree, parent)?;
    let by_id: BTreeMap<_, _> = tree.elements.iter().map(|e| (e.id, e)).collect();
    let destination_page = parent.and_then(|id| by_id.get(&id)).and_then(|e| e.page);
    let mut ancestor = parent;
    while let Some(id) = ancestor {
        if selected_ids.contains(&id) {
            return Err(invalid("cannot move selection into itself or its descendants"));
        }
        ancestor = by_id.get(&id).ok_or_else(|| invalid("missing destination ancestor"))?.parent;
    }
    let mut changes = BTreeMap::new();
    // selected_roots already validated every source ancestor against its parent index.
    let parents: BTreeSet<_> =
        selected.iter().map(|e| e.parent.or(tree.root).ok_or_else(|| invalid("missing structure root"))).collect::<Result<_>>()?;
    for id in parents.iter().copied().chain(std::iter::once(destination)) {
        let mut d = dictionary(doc, id)?;
        let kids = raw_kids(doc, &d)?.into_iter().filter(|k| !k.as_ref().is_some_and(|r| selected_ids.contains(&r))).collect();
        set_kids(&mut d, kids);
        changes.insert(id, d);
    }
    let mut dd = changes.get(&destination).cloned().ok_or_else(|| invalid("missing destination"))?;
    let mut dk = raw_kids(doc, &dd)?;
    if index > dk.len() {
        return Err(invalid("destination index exceeds child count after all removals"));
    }
    for e in &selected {
        let mut d = dictionary(doc, e.id)?;
        if let Some(pg) = e.page {
            d.set(b"Pg", Object::Ref(pg));
        } else if destination_page.is_some() {
            return Err(invalid("move would assign an inherited page to a page-less tag"));
        }
        d.set(b"P", Object::Ref(destination));
        changes.insert(e.id, d);
    }
    dk.splice(index..index, selected.iter().map(|e| Object::Ref(e.id)));
    set_kids(&mut dd, dk);
    changes.insert(destination, dd);
    for (id, d) in changes {
        commit_dict(doc, id, d);
    }
    Ok(())
}

/// Delete a selection in one transaction. Subtree deletion collapses descendants; unwrap
/// processes every explicitly selected tag, deepest first.
/// A failed later deletion leaves the original document untouched.
pub fn delete_tags(doc: &mut Document, ids: &[ObjRef], mode: DeleteMode) -> Result<()> {
    let tree = editable(doc)?;
    let roots = selected_roots(&tree, ids)?;
    let selected_ids: BTreeSet<_> = ids.iter().copied().collect();
    let selected = if mode == DeleteMode::Unwrap { tree.elements.iter().rev().filter(|e| selected_ids.contains(&e.id)).collect() } else { roots };
    // Each delete currently revalidates the tree and scans document objects. Bound the
    // repeated work before cloning/mutating instead of freezing on large selections.
    let mut per_delete = tree.elements.len().checked_add(tree.children.len()).and_then(|n| n.checked_add(doc.object_numbers().len()));
    for e in &tree.elements {
        per_delete = per_delete.and_then(|n| n.checked_add(e.kids.len()));
    }
    if per_delete.and_then(|n| n.checked_mul(selected.len())).is_none_or(|n| n > DELETE_BATCH_WORK_LIMIT) {
        return Err(invalid("deletion batch exceeds 1000000 estimated tag/content/object visits; use smaller batches"));
    }
    let mut candidate = doc.clone();
    for e in selected {
        delete(&mut candidate, e.id, mode)?;
    }
    *doc = candidate;
    Ok(())
}

/// Set or remove a custom RoleMap entry. Existing indirect RoleMap identity is retained.
/// All mappings must terminate at a standard role, and roles still in use cannot be unmapped.
pub fn set_role_mapping(doc: &mut Document, role: &str, target: Option<&str>) -> Result<()> {
    let tree = editable(doc)?;
    let root = tree.root.ok_or_else(|| invalid("document has no structure tree"))?;
    let mut rd = dictionary(doc, root)?;
    let mut map = match rd.get(b"RoleMap") {
        Some(value) => doc.dict(value).ok_or_else(|| invalid("RoleMap must be a dictionary"))?,
        None => Dict::new(),
    };
    let valid_name = |s: &str| !s.is_empty() && s.len() <= 127 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid_name(role) || crate::structure::is_standard(role.as_bytes()) {
        return Err(invalid("RoleMap source must be a custom name of 1–127 ASCII letters, digits, underscores or hyphens"));
    }
    if let Some(target) = target {
        if !valid_name(target) {
            return Err(invalid("invalid RoleMap target name"));
        }
        map.set(role.as_bytes(), Object::name(target));
    } else {
        map.remove(role.as_bytes());
    }
    validate_role_map(doc, &map)?;
    for e in &tree.elements {
        let mut issues = Vec::new();
        let standard = resolved_role(doc, &map, e.role.as_bytes(), &mut issues);
        if !issues.is_empty() || !crate::structure::is_standard(standard.as_bytes()) {
            return Err(invalid("mapping change would leave an existing tag without a standard role"));
        }
    }
    if let Some(id) = rd.reference(b"RoleMap") {
        dictionary(doc, id)?;
        doc.set(id, map);
    } else {
        rd.set(b"RoleMap", Object::Dict(map));
        commit_dict(doc, root, rd);
    }
    Ok(())
}
fn validate_role_map(doc: &Document, map: &Dict) -> Result<()> {
    if map.len() > LIMIT {
        return Err(invalid("RoleMap exceeds safe limits"));
    }
    for (name, value) in map.iter() {
        if crate::structure::is_standard(name) {
            return Err(invalid("RoleMap cannot redefine standard roles"));
        }
        if let Some(id) = value.as_ref()
            && doc.generation(id.num) != id.generation
        {
            return Err(invalid("stale RoleMap value generation"));
        }
        if doc.resolve(value).as_name().is_none() {
            return Err(invalid("RoleMap value must be a name"));
        }
        let mut issues = Vec::new();
        let resolved = resolved_role(doc, map, name, &mut issues);
        if !issues.is_empty() || !crate::structure::is_standard(resolved.as_bytes()) {
            return Err(invalid("RoleMap contains a cycle, excessive depth or a role without a standard target"));
        }
    }
    Ok(())
}

/// Reassign one existing MCID/MCR/OBJR leaf. Both indexes address mixed child lists;
/// destination index is measured after removal. Never changes painting content.
pub fn move_content(doc: &mut Document, source: ObjRef, source_index: usize, destination: ObjRef, index: usize) -> Result<()> {
    let tree = editable(doc)?;
    let src = element(&tree, source)?;
    element(&tree, destination)?;
    let kid = src.kids.get(source_index).ok_or_else(|| invalid("source content index exceeds child count"))?;
    let (object, mcid, page) = match kid {
        TagKid::Mcid { id, page } | TagKid::Mcr { id, page, stream: None } => (page.ok_or_else(|| invalid("content has no page"))?, Some(*id), *page),
        TagKid::Mcr { id, page, stream: Some(stream) } => (*stream, Some(*id), *page),
        TagKid::Objr { object, page } => (*object, None, *page),
        _ => return Err(invalid("source child must be an MCID, MCR or OBJR content leaf")),
    };
    let mut sd = dictionary(doc, source)?;
    let mut sk = raw_kids(doc, &sd)?;
    let raw = sk.get(source_index).cloned().ok_or_else(|| invalid("missing source content"))?;
    sk.remove(source_index);
    let mut dd = if source == destination { sd.clone() } else { dictionary(doc, destination)? };
    let mut dk = if source == destination { sk.clone() } else { raw_kids(doc, &dd)? };
    if index > dk.len() {
        return Err(invalid("destination index exceeds child count after removal"));
    }
    // A page-less stream MCR cannot safely inherit a new page.
    if page.is_none() && element(&tree, destination)?.page.is_some() {
        return Err(invalid("move would assign a page to page-less content"));
    }
    let mut leaf = match kid {
        TagKid::Mcid { id, .. } => {
            let mut d = Dict::new();
            d.set(b"Type", Object::name("MCR"));
            d.set(b"MCID", Object::Int(*id));
            d
        }
        _ => doc.dict(&raw).ok_or_else(|| invalid("invalid content dictionary"))?,
    };
    if let Some(pg) = page {
        leaf.set(b"Pg", Object::Ref(pg));
    }
    let mut changes = BTreeMap::new();
    let moved = if matches!(kid, TagKid::Mcid { .. }) {
        // Indirect integers may be reused across pages. Never mutate that shared integer.
        Object::Dict(leaf)
    } else if let Some(id) = raw.as_ref() {
        let mut uses = 0usize;
        for e in &tree.elements {
            uses = uses.saturating_add(raw_kids(doc, &dictionary(doc, e.id)?)?.iter().filter(|k| k.as_ref() == Some(id)).count());
        }
        if uses != 1 {
            return Err(invalid("shared indirect content dictionary cannot be moved safely"));
        }
        changes.insert(id, leaf);
        raw
    } else {
        Object::Dict(leaf)
    };
    dk.insert(index, moved);
    if source != destination {
        set_kids(&mut sd, sk);
        changes.insert(source, sd);
    }
    set_kids(&mut dd, dk);
    changes.insert(destination, dd);
    let root = tree.root.ok_or_else(|| invalid("missing structure root"))?;
    let mut rd = dictionary(doc, root)?;
    let key = doc
        .get(object)
        .as_dict()
        .and_then(|d| d.int(if mcid.is_some() { b"StructParents" } else { b"StructParent" }))
        .ok_or_else(|| invalid("missing content ParentTree key"))?;
    let entries = parent_entries(doc, &rd)?;
    if let Some(array_id) = entries.iter().find(|e| e.key == key).and_then(|e| e.value.as_ref())
        && mcid.is_some()
        && entries.iter().filter(|e| e.value.as_ref() == Some(array_id)).count() > 1
    {
        return Err(invalid("shared ParentTree value array cannot be reassigned safely"));
    }
    let mut arrays = BTreeMap::new();
    let mut replacements = BTreeMap::new();
    for entry in entries {
        let value = if entry.key != key {
            entry.value
        } else if let Some(mcid) = mcid {
            let resolved = doc.resolve(&entry.value);
            let mut array = resolved.as_array().ok_or_else(|| invalid("MCID ParentTree entry must be an array"))?.to_vec();
            let slot = usize::try_from(mcid).ok().and_then(|n| array.get_mut(n)).ok_or_else(|| invalid("missing MCID ParentTree slot"))?;
            *slot = Object::Ref(destination);
            if let Some(id) = entry.value.as_ref() {
                arrays.insert(id, Object::Array(array));
                entry.value
            } else {
                Object::Array(array)
            }
        } else {
            Object::Ref(destination)
        };
        replacements.insert(entry.key, Some(value));
    }
    let pt = rd.get(b"ParentTree").ok_or_else(|| invalid("missing ParentTree"))?;
    let (rewritten, _) = rewrite_parent(doc, pt, &replacements, &mut changes, 0)?;
    rd.set(b"ParentTree", rewritten);
    changes.insert(root, rd);
    for (id, d) in changes {
        commit_dict(doc, id, d);
    }
    for (id, value) in arrays {
        doc.set(id, value);
    }
    Ok(())
}

// Number-tree leaves are retained in place, including unknown keys and existing Limits.
// Values may themselves be indirect arrays. Cyclic/shared number nodes are rejected.
#[derive(Clone)]
struct ParentEntry {
    key: i64,
    value: Object,
}
fn parent_entries(doc: &Document, root: &Dict) -> Result<Vec<ParentEntry>> {
    let Some(start) = root.get(b"ParentTree") else { return Ok(Vec::new()) };
    let mut stack = vec![(start.clone(), 0usize)];
    let mut seen = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut out = Vec::new();
    let mut count = 0usize;
    let mut previous_key = None;
    while let Some((obj, depth)) = stack.pop() {
        count = count.saturating_add(1);
        if depth > DEPTH || count > LIMIT {
            return Err(invalid("ParentTree exceeds safe inspection limits"));
        }
        if let Some(id) = obj.as_ref() {
            if doc.generation(id.num) != id.generation {
                return Err(invalid("stale ParentTree node generation"));
            }
            if !seen.insert(id) {
                return Err(invalid("cyclic or shared ParentTree node"));
            }
        }
        let node = doc.dict(&obj).ok_or_else(|| invalid("ParentTree node is not a dictionary"))?;
        if node.contains(b"Kids") && node.contains(b"Nums") {
            return Err(invalid("ParentTree node has both Kids and Nums"));
        }
        if let Some(k) = node.get(b"Kids") {
            let kids = doc.resolve(k);
            let kids = kids.as_array().ok_or_else(|| invalid("ParentTree Kids is not an array"))?;
            if stack.len().saturating_add(kids.len()).saturating_add(count) > LIMIT {
                return Err(invalid("ParentTree exceeds safe inspection limits"));
            }
            stack.extend(kids.iter().rev().cloned().map(|k| (k, depth.saturating_add(1))));
        } else if let Some(nums) = node.get(b"Nums") {
            let nums = doc.resolve(nums);
            let nums = nums.as_array().ok_or_else(|| invalid("ParentTree Nums is not an array"))?;
            if nums.len() % 2 != 0 || nums.len() > LIMIT {
                return Err(invalid("invalid ParentTree Nums length"));
            }
            let mut previous = None;
            for pair in nums.as_chunks::<2>().0 {
                let key = pair.first().and_then(Object::as_int).filter(|n| *n >= 0).ok_or_else(|| invalid("invalid ParentTree key"))?;
                if previous.is_some_and(|p| p >= key) || previous_key.is_some_and(|p| p >= key) || !keys.insert(key) {
                    return Err(invalid("unsorted or duplicate ParentTree keys"));
                }
                previous = Some(key);
                previous_key = Some(key);
                let value = pair.get(1).ok_or_else(|| invalid("missing ParentTree value"))?.clone();
                if let Some(id) = value.as_ref()
                    && doc.generation(id.num) != id.generation
                {
                    return Err(invalid("stale ParentTree value generation"));
                }
                let resolved = doc.resolve(&value);
                match &*resolved {
                    Object::Array(a) if a.len() <= LIMIT && a.iter().all(|v| matches!(v, Object::Null | Object::Ref(_))) => {
                        count = count.saturating_add(a.len());
                        if count > LIMIT {
                            return Err(invalid("ParentTree exceeds safe inspection limits"));
                        }
                    }
                    Object::Dict(d) if value.as_ref().is_some() && d.contains(b"S") => {}
                    _ => return Err(invalid("unsupported ParentTree value")),
                }
                out.push(ParentEntry { key, value });
                if out.len() > LIMIT {
                    return Err(invalid("ParentTree exceeds safe inspection limits"));
                }
            }
        }
    }
    Ok(out)
}

/// Delete tags, never page content. Unwrap promotes the exact ordered kids; subtree removes
/// their associations. Objects stay allocated, so unrelated references cannot dangle.
pub fn delete(doc: &mut Document, id: ObjRef, mode: DeleteMode) -> Result<()> {
    let tree = editable(doc)?;
    let e = element(&tree, id)?;
    let root = tree.root.ok_or_else(|| invalid("missing structure root"))?;
    let mut root_dict = dictionary(doc, root)?;
    reject_structure_destinations(doc)?;
    if root_dict.contains(b"IDTree") {
        return Err(invalid("deletion while an IDTree exists is not supported"));
    }
    // Cross-element references need reference-aware rewriting, not a blind tree deletion.
    if tree.elements.iter().any(|e| dictionary(doc, e.id).is_ok_and(|d| d.contains(b"Ref"))) {
        return Err(invalid("deletion with structure Ref associations is not supported"));
    }
    let pid = parent_id(&tree, e.parent)?;
    let mut pd = dictionary(doc, pid)?;
    let mut siblings = raw_kids(doc, &pd)?;
    let pos = siblings.iter().position(|k| k.as_ref() == Some(id)).ok_or_else(|| invalid("tag missing from parent"))?;
    let own = dictionary(doc, id)?;
    let kids = raw_kids(doc, &own)?;
    let mut removed = BTreeSet::from([id]);
    if mode == DeleteMode::Subtree {
        // Preorder guarantees the parent's membership is known first.
        for child in &tree.elements {
            if child.parent.is_some_and(|p| removed.contains(&p)) {
                removed.insert(child.id);
            }
        }
    }
    let mut changes = BTreeMap::new();
    let mut promoted = Vec::new();
    if mode == DeleteMode::Unwrap {
        if e.parent.is_none() && kids.iter().any(|k| !matches!(classify(doc, k, e.page), TagKid::Element { .. })) {
            return Err(invalid("cannot promote content items to StructTreeRoot; move them under another tag first"));
        }
        for kid in kids {
            let mut raw = kid.clone();
            match classify(doc, &kid, e.page) {
                TagKid::Element { id: child } => {
                    let child_info = element(&tree, child)?;
                    let mut d = dictionary(doc, child)?;
                    d.set(b"P", Object::Ref(pid));
                    if let Some(pg) = child_info.page {
                        d.set(b"Pg", Object::Ref(pg));
                    }
                    changes.insert(child, d);
                }
                TagKid::Mcid { id: mcid, page } => {
                    if let Some(pg) = page {
                        let mut m = Dict::new();
                        m.set(b"Type", Object::Name(b"MCR".to_vec()));
                        m.set(b"MCID", Object::Int(mcid));
                        m.set(b"Pg", Object::Ref(pg));
                        raw = Object::Dict(m);
                    }
                }
                TagKid::Mcr { page, .. } | TagKid::Objr { page, .. } => {
                    let mut d = doc.dict(&kid).ok_or_else(|| invalid("invalid content item"))?;
                    if let Some(pg) = page {
                        d.set(b"Pg", Object::Ref(pg));
                    }
                    if let Some(r) = kid.as_ref() {
                        changes.insert(r, d);
                    } else {
                        raw = Object::Dict(d);
                    }
                }
                TagKid::Unsupported { .. } => return Err(invalid("unsupported promoted child")),
            }
            promoted.push(raw);
        }
    }
    siblings.splice(pos..pos.saturating_add(1), promoted);
    set_kids(&mut pd, siblings);
    changes.insert(pid, pd);
    let entries = parent_entries(doc, &root_dict)?;
    // Rewrite only number-tree values. Preserve the tree topology, keys and limits; a
    // deleted singleton maps to null, and a removed MCID slot stays null (never shifts).
    let mut object_changes = BTreeMap::new();
    let replacements: BTreeMap<i64, Option<Object>> = entries
        .into_iter()
        .map(|entry| {
            let resolved = doc.resolve(&entry.value);
            let replace = |v: &Object| {
                if v.as_ref().is_some_and(|r| removed.contains(&r)) {
                    if mode == DeleteMode::Unwrap { Object::Ref(pid) } else { Object::Null }
                } else {
                    v.clone()
                }
            };
            let value = match &*resolved {
                Object::Array(a) => {
                    let updated = Object::Array(a.iter().map(replace).collect());
                    if let Some(array_id) = entry.value.as_ref() {
                        object_changes.insert(array_id, updated);
                        Some(entry.value.clone())
                    } else {
                        Some(updated)
                    }
                }
                _ if mode == DeleteMode::Subtree && entry.value.as_ref().is_some_and(|r| removed.contains(&r)) => None,
                _ => Some(replace(&entry.value)),
            };
            (entry.key, value)
        })
        .collect();
    if let Some(pt) = root_dict.get(b"ParentTree").cloned() {
        let (rewritten, _) = rewrite_parent(doc, &pt, &replacements, &mut changes, 0)?;
        root_dict.set(b"ParentTree", rewritten);
        // Root may also be the affected tag's parent.
        if let Some(changed) = changes.get(&root) {
            let k = changed.get(b"K").cloned();
            root_dict.remove(b"K");
            if let Some(k) = k {
                root_dict.set(b"K", k);
            }
        }
        changes.insert(root, root_dict);
    }
    if mode == DeleteMode::Subtree {
        for e in &tree.elements {
            if removed.contains(&e.id) {
                for kid in &e.kids {
                    if let TagKid::Objr { object, .. } = kid {
                        let mut value = (*doc.get(*object)).clone();
                        if let Some(d) = value.as_dict_mut() {
                            d.remove(b"StructParent");
                        }
                        object_changes.insert(*object, value);
                    }
                }
            }
        }
    }
    for (r, d) in changes {
        commit_dict(doc, r, d);
    }
    for (r, value) in object_changes {
        doc.set(r, value);
    }
    Ok(())
}
fn rewrite_parent(
    doc: &Document,
    obj: &Object,
    replacements: &BTreeMap<i64, Option<Object>>,
    changes: &mut BTreeMap<ObjRef, Dict>,
    depth: usize,
) -> Result<(Object, Option<(i64, i64)>)> {
    if depth > DEPTH {
        return Err(invalid("ParentTree nesting limit"));
    }
    let mut d = doc.dict(obj).ok_or_else(|| invalid("invalid ParentTree node"))?;
    let mut range: Option<(i64, i64)> = None;
    if let Some(nums) = d.get(b"Nums") {
        let nums = doc.resolve(nums);
        let nums = nums.as_array().ok_or_else(|| invalid("invalid ParentTree Nums"))?;
        let mut out = Vec::new();
        for pair in nums.as_chunks::<2>().0 {
            let key = pair.first().and_then(Object::as_int).ok_or_else(|| invalid("invalid ParentTree key"))?;
            if let Some(value) = replacements.get(&key).ok_or_else(|| invalid("ParentTree key missing"))? {
                out.push(Object::Int(key));
                out.push(value.clone());
                range = Some((range.map_or(key, |r| r.0.min(key)), range.map_or(key, |r| r.1.max(key))));
            }
        }
        d.set(b"Nums", Object::Array(out));
    }
    if let Some(kids) = d.get(b"Kids") {
        let kids = doc.resolve(kids);
        let kids = kids.as_array().ok_or_else(|| invalid("invalid ParentTree Kids"))?;
        let mut out = Vec::new();
        for kid in kids {
            let (kid, bounds) = rewrite_parent(doc, kid, replacements, changes, depth.saturating_add(1))?;
            if let Some((lo, hi)) = bounds {
                out.push(kid);
                range = Some((range.map_or(lo, |r| r.0.min(lo)), range.map_or(hi, |r| r.1.max(hi))));
            }
        }
        if out.is_empty() {
            d.remove(b"Kids");
            d.set(b"Nums", Object::Array(Vec::new()));
        } else {
            d.set(b"Kids", Object::Array(out));
        }
    }
    if let Some((lo, hi)) = range {
        if depth > 0 || d.contains(b"Limits") {
            d.set(b"Limits", Object::Array(vec![Object::Int(lo), Object::Int(hi)]));
        }
    } else {
        d.remove(b"Limits");
    }
    let result = if let Some(r) = obj.as_ref() {
        changes.insert(r, d);
        Object::Ref(r)
    } else {
        Object::Dict(d)
    };
    Ok((result, range))
}

// Preserve existing indirect array identities as well as each structure/content reference.
fn commit_dict(doc: &mut Document, id: ObjRef, mut new: Dict) {
    if let Some(old) = doc.get(id).as_dict().cloned() {
        for key in [b"K".as_slice(), b"Nums", b"Kids", b"Names"] {
            if let Some(array_id) = old.reference(key)
                && matches!(&*doc.get(array_id), Object::Array(_))
            {
                match new.get(key).cloned() {
                    Some(Object::Array(a)) => {
                        doc.set(array_id, Object::Array(a));
                        new.set(key, Object::Ref(array_id));
                    }
                    None if key == b"K" => {
                        doc.set(array_id, Object::Array(Vec::new()));
                        new.set(key, Object::Ref(array_id));
                    }
                    _ => {}
                }
            }
        }
    }
    doc.set(id, new);
}
fn validate_associations(doc: &Document, tree: &mut TagTree, entries: &[ParentEntry]) {
    let by_key: BTreeMap<_, _> = entries.iter().map(|e| (e.key, &e.value)).collect();
    let page_ids: BTreeSet<_> = tree.pages.iter().copied().collect();
    let ids: BTreeSet<_> = tree.elements.iter().map(|e| e.id).collect();
    for entry in entries {
        let value = doc.resolve(&entry.value);
        let owners: Vec<_> = match &*value {
            Object::Array(a) => a.iter().filter_map(Object::as_ref).collect(),
            Object::Dict(_) => entry.value.as_ref().into_iter().collect(),
            _ => Vec::new(),
        };
        for owner in owners {
            if !ids.contains(&owner) {
                tree.issues.push(format!("ParentTree key {} refers to unreachable tag {owner}", entry.key));
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut expected = BTreeMap::new();
    let mut key_objects = BTreeMap::new();
    for e in &tree.elements {
        for kid in &e.kids {
            let association = match kid {
                TagKid::Mcid { id, page } | TagKid::Mcr { id, page, stream: None } => Some((*page, Some(*id))),
                TagKid::Mcr { id, stream: Some(stream), .. } => Some((Some(*stream), Some(*id))),
                TagKid::Objr { object, .. } => Some((Some(*object), None)),
                _ => None,
            };
            let Some((object, mcid)) = association else { continue };
            let Some(object) = object else {
                tree.issues.push(format!("{} content has no effective page or stream", e.id));
                continue;
            };
            if !seen.insert((object, mcid)) {
                tree.issues.push(format!("duplicate content ownership on {object}"));
            }
            let key_name = if mcid.is_some() { b"StructParents".as_slice() } else { b"StructParent" };
            let object_value = doc.get(object);
            if doc.generation(object.num) != object.generation {
                tree.issues.push(format!("stale content object generation {object}"));
            }
            match kid {
                TagKid::Mcid { .. } | TagKid::Mcr { stream: None, .. } if !page_ids.contains(&object) => {
                    tree.issues.push(format!("{object} is not a document page"))
                }
                TagKid::Mcr { stream: Some(_), .. } if !matches!(&*object_value, Object::Stream(_)) => {
                    tree.issues.push(format!("{object} is not a content stream"))
                }
                _ => {}
            }
            let key = object_value.as_dict().and_then(|d| d.int(key_name));
            let Some(value) = key.and_then(|k| by_key.get(&k)) else {
                tree.issues.push(format!("{} content lacks its ParentTree mapping", e.id));
                continue;
            };
            if let Some(key) = key
                && key_objects.insert(key, object).is_some_and(|old| old != object)
            {
                tree.issues.push(format!("ParentTree key {key} is shared by different content objects"));
            }
            if let Some(key) = key
                && expected.insert((key, mcid), e.id).is_some()
            {
                tree.issues.push(format!("multiple content owners use ParentTree key {key}"));
            }
            let value = doc.resolve(value);
            let owner = if let Some(mcid) = mcid {
                usize::try_from(mcid).ok().and_then(|n| value.as_array().and_then(|a| a.get(n))).and_then(Object::as_ref)
            } else {
                by_key.get(&key.unwrap_or(-1)).and_then(|v| v.as_ref())
            };
            if owner != Some(e.id) {
                tree.issues.push(format!("{} content has inconsistent ParentTree owner", e.id));
            }
        }
    }
    for entry in entries {
        let resolved = doc.resolve(&entry.value);
        match &*resolved {
            Object::Array(a) => {
                for (index, owner) in a.iter().enumerate() {
                    if let Some(owner) = owner.as_ref()
                        && i64::try_from(index).ok().and_then(|n| expected.get(&(entry.key, Some(n)))) != Some(&owner)
                    {
                        tree.issues.push(format!("ParentTree key {} contains an orphan content association", entry.key));
                    }
                }
            }
            _ => {
                if let Some(owner) = entry.value.as_ref()
                    && expected.get(&(entry.key, None)) != Some(&owner)
                {
                    tree.issues.push(format!("ParentTree key {} contains an orphan object association", entry.key));
                }
            }
        }
    }
}

fn reject_structure_destinations(doc: &Document) -> Result<()> {
    let objects = doc.object_numbers();
    if objects.len() > LIMIT {
        return Err(invalid("document too large to verify structure destinations before deletion"));
    }
    let mut count = 0usize;
    for num in objects {
        let value = doc.get(ObjRef::new(num, doc.generation(num)));
        let mut stack = vec![(&*value, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            count = count.saturating_add(1);
            if count > LIMIT || depth > DEPTH {
                return Err(invalid("structure destination check exceeds safe limits"));
            }
            if let Some(d) = value.as_dict() {
                if d.contains(b"SD") {
                    return Err(invalid("deletion with structure destinations (SD) is not supported"));
                }
                stack.extend(d.iter().map(|(_, v)| (v, depth.saturating_add(1))));
            } else if let Object::Array(a) = value {
                stack.extend(a.iter().map(|v| (v, depth.saturating_add(1))));
            }
            if stack.len().saturating_add(count) > LIMIT {
                return Err(invalid("structure destination check exceeds safe limits"));
            }
        }
    }
    Ok(())
}

fn validate_requested_role(doc: &Document, tree: &TagTree, p: &TagProperties) -> Result<()> {
    let Some(role) = &p.role else { return Ok(()) };
    let map = tree.root.and_then(|r| doc.get(r).as_dict().and_then(|d| d.get(b"RoleMap")).and_then(|v| doc.dict(v))).unwrap_or_default();
    let mut issues = Vec::new();
    let standard = resolved_role(doc, &map, role.as_bytes(), &mut issues);
    if !issues.is_empty() || !crate::structure::is_standard(standard.as_bytes()) {
        return Err(invalid("tag type must be standard or resolve to a standard type through RoleMap"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn props(role: &str) -> TagProperties {
        TagProperties { role: Some(role.into()), ..Default::default() }
    }
    fn fixture() -> (Document, ObjRef, ObjRef, ObjRef, ObjRef, ObjRef) {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let group = create(&mut doc, Some(top), 0, &props("Sect")).unwrap();
        let child = create(&mut doc, Some(group), 0, &props("P")).unwrap();
        let mut page = Dict::new();
        page.set(b"Type", Object::name("Page"));
        page.set(b"StructParents", Object::Int(0));
        let page = doc.add(page);
        let cat = doc.get(doc.root().unwrap());
        let pages = cat.as_dict().unwrap().reference(b"Pages").unwrap();
        doc.update_dict(pages, |d| {
            d.set(b"Kids", Object::Array(vec![Object::Ref(page)]));
            d.set(b"Count", Object::Int(1));
        })
        .unwrap();
        doc.update_dict(group, |d| {
            d.set(b"Pg", Object::Ref(page));
            d.set(b"Private", Object::Int(99));
        })
        .unwrap();
        doc.update_dict(child, |d| {
            d.set(b"K", Object::Array(vec![Object::Int(0)]));
        })
        .unwrap();
        let mut pt = Dict::new();
        pt.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(child)])]));
        let pt = doc.add(pt);
        let root = inspect(&doc).unwrap().root.unwrap();
        doc.update_dict(root, |d| d.set(b"ParentTree", Object::Ref(pt))).unwrap();
        (doc, top, group, child, page, pt)
    }
    #[test]
    fn reads_inherited_pages_properties_roles_and_order() {
        let (mut doc, _, group, child, page, _) = fixture();
        let alt = doc.add(Object::String(PdfString::text("こんにちは")));
        doc.update_dict(child, |d| {
            d.set(b"S", Object::name("Custom"));
            d.set(b"Alt", Object::Ref(alt));
            d.set(b"E", Object::String(PdfString::text("expanded")));
        })
        .unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let mut roles = Dict::new();
        roles.set(b"Custom", Object::name("P"));
        doc.update_dict(root, |d| d.set(b"RoleMap", roles)).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        let e = tree.element(child).unwrap();
        assert_eq!(e.parent, Some(group));
        assert_eq!(e.page, Some(page));
        assert_eq!(e.page_index, Some(0));
        assert_eq!(e.standard_role, "P");
        assert_eq!(e.alt.as_deref(), Some("こんにちは"));
        assert_eq!(e.expansion.as_deref(), Some("expanded"));
        assert_eq!(e.kids, vec![TagKid::Mcid { id: 0, page: Some(page) }]);
    }
    #[test]
    fn edits_reorders_reparents_without_losing_identity_or_unknown_keys() {
        let (mut doc, top, group, child, page, _) = fixture();
        let other = create(&mut doc, Some(top), 1, &props("Sect")).unwrap();
        update(
            &mut doc,
            group,
            &TagProperties {
                title: Some(Some("Title".into())),
                actual_text: Some(Some("Actual".into())),
                lang: Some(Some("fr".into())),
                ..Default::default()
            },
        )
        .unwrap();
        move_tag(&mut doc, group, Some(other), 0).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        assert_eq!(tree.element(child).unwrap().page, Some(page));
        assert_eq!(tree.element(group).unwrap().parent, Some(other));
        assert_eq!(doc.get(group).as_dict().unwrap().int(b"Private"), Some(99));
        update(&mut doc, group, &TagProperties { title: Some(None), ..Default::default() }).unwrap();
        assert!(doc.get(group).as_dict().unwrap().get(b"T").is_none());
    }
    #[test]
    fn unwrap_pins_pages_and_remaps_mixed_content_owners() {
        let (mut doc, top, group, child, page, pt) = fixture();
        let mut annotation = Dict::new();
        annotation.set(b"StructParent", Object::Int(1));
        let annotation = doc.add(annotation);
        let mut objr = Dict::new();
        objr.set(b"Type", Object::name("OBJR"));
        objr.set(b"Obj", Object::Ref(annotation));
        let objr = doc.add(objr);
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", Object::Int(1));
        mcr.set(b"Private", Object::Int(11));
        let mcr = doc.add(mcr);
        doc.update_dict(group, |d| d.set(b"K", Object::Array(vec![Object::Int(2), Object::Ref(child), Object::Ref(mcr), Object::Ref(objr)])))
            .unwrap();
        doc.update_dict(pt, |d| {
            d.set(
                b"Nums",
                Object::Array(vec![
                    Object::Int(0),
                    Object::Array(vec![Object::Ref(child), Object::Ref(group), Object::Ref(group)]),
                    Object::Int(1),
                    Object::Ref(group),
                ]),
            )
        })
        .unwrap();
        assert!(inspect(&doc).unwrap().issues.is_empty());
        delete(&mut doc, group, DeleteMode::Unwrap).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        let top = tree.element(top).unwrap();
        assert_eq!(top.kids.len(), 4);
        assert!(matches!(top.kids.first(),Some(TagKid::Mcr{id:2,page:Some(p),..}) if *p==page));
        assert_eq!(tree.element(child).unwrap().page, Some(page));
        assert_eq!(doc.get(mcr).as_dict().unwrap().int(b"Private"), Some(11));
        assert_eq!(doc.get(objr).as_dict().unwrap().reference(b"Pg"), Some(page));
    }
    #[test]
    fn subtree_deletion_cleans_nested_parent_tree_and_annotation_key() {
        let (mut doc, _, group, child, _, pt) = fixture();
        let mut annotation = Dict::new();
        annotation.set(b"StructParent", Object::Int(3));
        let annotation = doc.add(annotation);
        let mut objr = Dict::new();
        objr.set(b"Type", Object::name("OBJR"));
        objr.set(b"Obj", Object::Ref(annotation));
        doc.update_dict(child, |d| d.set(b"K", Object::Array(vec![Object::Int(0), Object::Dict(objr)]))).unwrap();
        let array = doc.add(Object::Array(vec![Object::Ref(child)]));
        let mut leaf = Dict::new();
        leaf.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Ref(array), Object::Int(3), Object::Ref(child)]));
        leaf.set(b"Limits", Object::Array(vec![Object::Int(0), Object::Int(3)]));
        leaf.set(b"Private", Object::Int(12));
        let leaf = doc.add(leaf);
        let mut node = Dict::new();
        node.set(b"Kids", Object::Array(vec![Object::Ref(leaf)]));
        doc.set(pt, node);
        delete(&mut doc, group, DeleteMode::Subtree).unwrap();
        assert!(inspect(&doc).unwrap().issues.is_empty());
        assert_eq!(&*doc.get(array), &Object::Array(vec![Object::Null]));
        let leaf = doc.get(leaf);
        let leaf = leaf.as_dict().unwrap();
        assert_eq!(leaf.int(b"Private"), Some(12));
        assert_eq!(leaf.get(b"Limits"), Some(&Object::Array(vec![Object::Int(0), Object::Int(0)])));
        assert!(!doc.get(annotation).as_dict().unwrap().contains(b"StructParent"));
    }
    #[test]
    fn cycle_bad_indices_and_invalid_edits_are_atomic() {
        let (mut doc, _, group, child, _, _) = fixture();
        let before = doc.get(group);
        assert!(move_tag(&mut doc, group, Some(child), 0).is_err());
        assert_eq!(doc.get(group), before);
        assert!(move_tag(&mut doc, child, Some(group), 99).is_err());
        assert_eq!(doc.get(group), before);
        assert!(
            update(&mut doc, group, &TagProperties { role: Some("bad/name".into()), title: Some(Some("lost".into())), ..Default::default() })
                .is_err()
        );
        assert_eq!(doc.get(group), before);
        doc.update_dict(child, |d| d.set(b"K", Object::Ref(group))).unwrap();
        let before = doc.get(group);
        assert!(!inspect(&doc).unwrap().issues.is_empty());
        assert!(update(&mut doc, group, &props("P")).is_err());
        assert_eq!(doc.get(group), before);
    }
    #[test]
    fn malformed_number_trees_and_direct_elements_block_mutation() {
        let (mut doc, _, group, _, _, pt) = fixture();
        doc.update_dict(pt, |d| d.set(b"Nums", Object::Array(vec![Object::Int(0)]))).unwrap();
        assert!(inspect(&doc).unwrap().issues.iter().any(|i| i.contains("Nums length")));
        assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
        let mut d = Dict::new();
        d.set(b"Kids", Object::Array(vec![Object::Ref(pt)]));
        doc.set(pt, d);
        assert!(inspect(&doc).unwrap().issues.iter().any(|i| i.contains("cyclic")));
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let mut direct = Dict::new();
        direct.set(b"S", Object::name("P"));
        doc.update_dict(top, |d| d.set(b"K", direct)).unwrap();
        assert!(update(&mut doc, top, &props("Sect")).is_err());
    }
    #[test]
    fn indirect_k_array_identity_survives_reordering() {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let a = create(&mut doc, Some(top), 0, &props("P")).unwrap();
        let b = create(&mut doc, Some(top), 1, &props("P")).unwrap();
        let array = doc.add(Object::Array(vec![Object::Ref(a), Object::Ref(b)]));
        doc.update_dict(top, |d| d.set(b"K", Object::Ref(array))).unwrap();
        move_tag(&mut doc, a, Some(top), 1).unwrap();
        assert_eq!(doc.get(top).as_dict().unwrap().reference(b"K"), Some(array));
        assert_eq!(&*doc.get(array), &Object::Array(vec![Object::Ref(b), Object::Ref(a)]));
    }
    #[test]
    fn rejects_wrong_parent_tree_owner_negative_and_huge_mcids() {
        for mcid in [-1, i64::MAX] {
            let (mut doc, _, group, child, _, _) = fixture();
            doc.update_dict(child, |d| d.set(b"K", Object::Int(mcid))).unwrap();
            assert!(update(&mut doc, group, &props("Div")).is_err());
        }
        let (mut doc, top, group, _, _, pt) = fixture();
        doc.update_dict(pt, |d| d.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(top)])]))).unwrap();
        assert!(update(&mut doc, group, &props("Div")).is_err());
    }
    #[test]
    fn reject_root_unwrap_content_and_external_structure_references() {
        let (mut doc, _, group, _, _, _) = fixture();
        let root = inspect(&doc).unwrap().root.unwrap();
        doc.update_dict(root, |d| d.set(b"IDTree", Dict::new())).unwrap();
        assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
    }
    #[test]
    fn shared_empty_k_arrays_and_namespace_edits_are_rejected_atomically() {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let a = create(&mut doc, Some(top), 0, &props("P")).unwrap();
        let b = create(&mut doc, Some(top), 1, &props("P")).unwrap();
        let array = doc.add(Object::Array(Vec::new()));
        for id in [a, b] {
            doc.update_dict(id, |d| d.set(b"K", Object::Ref(array))).unwrap();
        }
        let before = doc.get(array);
        assert!(create(&mut doc, Some(a), 0, &props("P")).is_err());
        assert_eq!(doc.get(array), before);
        doc.update_dict(b, |d| {
            d.remove(b"K");
            d.set(b"NS", Dict::new());
        })
        .unwrap();
        assert!(update(&mut doc, b, &props("P")).is_err());
    }
    #[test]
    fn direct_root_remains_inspectable_and_read_only() {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let direct = (*doc.get(root)).clone();
        doc.update_dict(doc.root().unwrap(), |d| d.set(b"StructTreeRoot", direct)).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.element(top).is_some());
        assert!(tree.root.is_none());
        assert!(!tree.issues.is_empty());
        assert!(update(&mut doc, top, &props("Sect")).is_err());
    }
    #[test]
    fn root_content_unwrap_and_structure_destinations_reject_without_writes() {
        let (mut doc, top, group, child, _, _) = fixture();
        move_tag(&mut doc, child, None, 1).unwrap();
        let before = doc.get(child);
        assert!(delete(&mut doc, child, DeleteMode::Unwrap).is_err());
        assert_eq!(doc.get(child), before);
        let mut action = Dict::new();
        action.set(b"S", Object::name("GoTo"));
        action.set(b"SD", Object::Ref(group));
        doc.add(action);
        let before = doc.get(top);
        assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
        assert_eq!(doc.get(top), before);
    }
    #[test]
    fn form_stream_mcr_preserves_identity_and_parent_tree_on_unwrap() {
        let (mut doc, top, group, child, page, pt) = fixture();
        let mut stream = Dict::new();
        stream.set(b"StructParents", Object::Int(2));
        let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(stream, b"q Q".to_vec())));
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"Stm", Object::Ref(stream));
        mcr.set(b"MCID", Object::Int(0));
        let mcr = doc.add(mcr);
        doc.update_dict(group, |d| d.set(b"K", Object::Array(vec![Object::Ref(child), Object::Ref(mcr)]))).unwrap();
        doc.update_dict(pt, |d| {
            d.set(
                b"Nums",
                Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(child)]), Object::Int(2), Object::Array(vec![Object::Ref(group)])]),
            )
        })
        .unwrap();
        let raw = doc.get(stream);
        delete(&mut doc, group, DeleteMode::Unwrap).unwrap();
        assert_eq!(doc.get(stream), raw);
        assert_eq!(doc.get(mcr).as_dict().unwrap().reference(b"Pg"), Some(page));
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        assert!(matches!(tree.element(top).unwrap().kids.get(1),Some(TagKid::Mcr{id:0,stream:Some(s),..}) if *s==stream));
        let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        let reopened = Document::open(std::sync::Arc::new(bytes)).unwrap();
        assert!(inspect(&reopened).unwrap().issues.is_empty());
    }
    #[test]
    fn bounded_deep_walk_and_unmapped_custom_role_reject() {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        assert!(create(&mut doc, Some(top), 0, &props("UnmappedCustomRole")).is_err());
        let mut parent = top;
        for _ in 0..DEPTH + 2 {
            let mut d = Dict::new();
            d.set(b"Type", Object::name("StructElem"));
            d.set(b"S", Object::name("Sect"));
            d.set(b"P", Object::Ref(parent));
            let id = doc.add(d);
            doc.update_dict(parent, |d| d.set(b"K", Object::Ref(id))).unwrap();
            parent = id;
        }
        assert!(inspect(&doc).unwrap().issues.iter().any(|i| i.contains("safe inspection limits")));
        assert!(update(&mut doc, top, &props("Div")).is_err());
    }
    #[test]
    fn role_map_edits_validate_all_mappings_and_keep_indirect_identity() {
        let (mut doc, _, _, child, _, _) = fixture();
        let root = inspect(&doc).unwrap().root.unwrap();
        let map = doc.add(Dict::new());
        doc.update_dict(root, |d| d.set(b"RoleMap", Object::Ref(map))).unwrap();
        set_role_mapping(&mut doc, "Custom", Some("P")).unwrap();
        update(&mut doc, child, &props("Custom")).unwrap();
        set_role_mapping(&mut doc, "Alias", Some("Custom")).unwrap();
        assert_eq!(inspect(&doc).unwrap().role_map.get("Alias").map(String::as_str), Some("Custom"));
        let before = doc.get(map);
        for (role, target) in [("Custom", None), ("Custom", Some("Alias")), ("P", Some("H1")), ("Broken", Some("Missing"))] {
            assert!(set_role_mapping(&mut doc, role, target).is_err());
            assert_eq!(doc.get(map), before);
        }
        set_role_mapping(&mut doc, "Custom", Some("H2")).unwrap();
        assert_eq!(inspect(&doc).unwrap().element(child).unwrap().standard_role, "H2");
        set_role_mapping(&mut doc, "Alias", None).unwrap();
        assert_eq!(doc.get(root).as_dict().unwrap().reference(b"RoleMap"), Some(map));
        let mut malformed = Dict::new();
        malformed.set(b"A", Object::name("A"));
        doc.set(map, malformed);
        assert!(!inspect(&doc).unwrap().issues.is_empty());
        assert!(set_role_mapping(&mut doc, "X", Some("P")).is_err());
    }
    #[test]
    fn content_move_preserves_page_and_form_namespace_and_unknown_keys() {
        let (mut doc, top, group, child, page, pt) = fixture();
        let other = create(&mut doc, Some(top), 1, &props("Sect")).unwrap();
        move_content(&mut doc, child, 0, other, 0).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        assert!(matches!(tree.element(other).unwrap().kids.first(), Some(TagKid::Mcr { id:0,page:Some(p),stream:None }) if *p==page));
        let mut stream = Dict::new();
        stream.set(b"StructParents", Object::Int(2));
        let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(stream, b"q Q".to_vec())));
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", Object::Int(0));
        mcr.set(b"Stm", Object::Ref(stream));
        mcr.set(b"Private", Object::Int(9));
        let mcr = doc.add(mcr);
        doc.update_dict(child, |d| d.set(b"K", Object::Ref(mcr))).unwrap();
        doc.update_dict(pt, |d| {
            d.set(
                b"Nums",
                Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(other)]), Object::Int(2), Object::Array(vec![Object::Ref(child)])]),
            )
        })
        .unwrap();
        let stream_before = doc.get(stream);
        move_content(&mut doc, child, 0, other, 1).unwrap();
        assert_eq!(doc.get(stream), stream_before);
        assert_eq!(doc.get(mcr).as_dict().unwrap().reference(b"Pg"), Some(page));
        assert_eq!(doc.get(mcr).as_dict().unwrap().int(b"Private"), Some(9));
        assert_eq!(doc.get(other).as_dict().unwrap().get(b"K").unwrap().as_array().unwrap()[1].as_ref(), Some(mcr));
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        let before = doc.get(other);
        assert!(move_content(&mut doc, other, 1, group, 999).is_err());
        assert_eq!(doc.get(other), before);
        assert!(move_content(&mut doc, top, 0, other, 0).is_err());
        move_content(&mut doc, other, 1, other, 0).unwrap();
        assert!(inspect(&doc).unwrap().issues.is_empty());
        let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(inspect(&Document::open(std::sync::Arc::new(bytes)).unwrap()).unwrap().issues.is_empty());
    }
    #[test]
    fn object_reference_move_preserves_identity_and_scalar_parent_tree() {
        let (mut doc, top, group, child, page, pt) = fixture();
        let mut annotation = Dict::new();
        annotation.set(b"StructParent", Object::Int(3));
        let annotation = doc.add(annotation);
        let mut objr = Dict::new();
        objr.set(b"Type", Object::name("OBJR"));
        objr.set(b"Obj", Object::Ref(annotation));
        let objr = doc.add(objr);
        doc.update_dict(group, |d| d.set(b"K", Object::Array(vec![Object::Ref(child), Object::Ref(objr)]))).unwrap();
        doc.update_dict(pt, |d| {
            d.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(child)]), Object::Int(3), Object::Ref(group)]))
        })
        .unwrap();
        let original = doc.get(annotation);
        move_content(&mut doc, group, 1, top, 1).unwrap();
        assert_eq!(doc.get(annotation), original);
        assert_eq!(doc.get(objr).as_dict().unwrap().reference(b"Pg"), Some(page));
        assert!(inspect(&doc).unwrap().issues.is_empty());
    }
    #[test]
    fn batch_moves_use_document_order_collapse_descendants_and_reject_atomically() {
        let (mut doc, top, group, child, _, _) = fixture();
        let second = create(&mut doc, Some(top), 1, &props("Sect")).unwrap();
        let third = create(&mut doc, Some(top), 2, &props("Sect")).unwrap();
        move_tags(&mut doc, &[second, child, group], Some(top), 1).unwrap();
        assert_eq!(
            inspect(&doc).unwrap().element(top).unwrap().kids,
            vec![TagKid::Element { id: third }, TagKid::Element { id: group }, TagKid::Element { id: second }]
        );
        let before = inspect(&doc).unwrap();
        assert!(move_tags(&mut doc, &[group, second], Some(child), 0).is_err());
        assert!(move_tags(&mut doc, &[group, ObjRef::new(99999, 0)], None, 0).is_err());
        assert_eq!(inspect(&doc).unwrap(), before);
        delete_tags(&mut doc, &[child, group, second], DeleteMode::Subtree).unwrap();
        assert_eq!(inspect(&doc).unwrap().element(top).unwrap().kids, vec![TagKid::Element { id: third }]);
        assert!(inspect(&doc).unwrap().issues.is_empty());
    }
    #[test]
    fn batch_unwrap_late_failure_preserves_entire_document() {
        let (mut doc, top, group, child, _, _) = fixture();
        move_tag(&mut doc, child, None, 1).unwrap();
        // The empty group can unwrap; the root child with content cannot.
        let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(delete_tags(&mut doc, &[group, child], DeleteMode::Unwrap).is_err());
        assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
        assert!(inspect(&doc).unwrap().element(top).is_some());
    }

    #[test]
    fn identifiers_preserve_nested_name_tree_raw_sorting_and_indirect_nodes() {
        let (mut doc, top, group, child, _, _) = fixture();
        set_identifier(&mut doc, top, Some("a")).unwrap();
        set_identifier(&mut doc, group, Some("m")).unwrap();
        set_identifier(&mut doc, child, Some("日本語")).unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let idtext = |id| doc.get(id).as_dict().unwrap().get(b"ID").unwrap().clone();
        let names1 = doc.add(Object::Array(vec![idtext(top), Object::Ref(top)]));
        let names2 = doc.add(Object::Array(vec![
            doc.get(group).as_dict().unwrap().get(b"ID").unwrap().clone(),
            Object::Ref(group),
            doc.get(child).as_dict().unwrap().get(b"ID").unwrap().clone(),
            Object::Ref(child),
        ]));
        let mut leaf1 = Dict::new();
        leaf1.set(b"Names", Object::Ref(names1));
        leaf1.set(b"Private", Object::Int(42));
        leaf1.set(b"Limits", Object::Array(vec![Object::String(PdfString::text("a")), Object::String(PdfString::text("a"))]));
        let leaf1 = doc.add(leaf1);
        let mut leaf2 = Dict::new();
        leaf2.set(b"Names", Object::Ref(names2));
        leaf2.set(b"Limits", Object::Array(vec![Object::String(PdfString::text("m")), Object::String(PdfString::text("日本語"))]));
        let leaf2 = doc.add(leaf2);
        let kids = doc.add(Object::Array(vec![Object::Ref(leaf1), Object::Ref(leaf2)]));
        let mut node = Dict::new();
        node.set(b"Kids", Object::Ref(kids));
        let node = doc.add(node);
        doc.update_dict(root, |d| d.set(b"IDTree", Object::Ref(node))).unwrap();
        let unicode_bytes = doc.get(child).as_dict().unwrap().get(b"ID").unwrap().as_string().unwrap().bytes.clone();
        set_identifier(&mut doc, top, Some("zzz")).unwrap();
        assert_eq!(doc.get(root).as_dict().unwrap().reference(b"IDTree"), Some(node));
        assert_eq!(doc.get(node).as_dict().unwrap().reference(b"Kids"), Some(kids));
        assert_eq!(doc.get(leaf1).as_dict().unwrap().reference(b"Names"), Some(names1));
        assert_eq!(doc.get(leaf1).as_dict().unwrap().int(b"Private"), Some(42));
        assert_eq!(doc.get(names1).as_array().unwrap()[0].as_string().unwrap().bytes, b"m");
        assert_eq!(doc.get(names2).as_array().unwrap()[2].as_string().unwrap().bytes, unicode_bytes);
        let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(set_identifier(&mut doc, top, Some("m")).is_err());
        assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
        set_identifier(&mut doc, top, None).unwrap();
        assert_eq!(inspect(&doc).unwrap().element(top).unwrap().identifier, None);
        assert!(set_identifier(&mut doc, group, None).is_err(), "would empty a multi-leaf tree");
        let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        let mut reopened = Document::open(std::sync::Arc::new(bytes)).unwrap();
        let tree = inspect(&reopened).unwrap();
        let group = tree.elements.iter().find(|e| e.identifier.as_deref() == Some("m")).unwrap().id;
        set_identifier(&mut reopened, group, Some("b")).unwrap();
        assert!(inspect(&reopened).unwrap().issues.is_empty());
    }
    #[test]
    fn identifier_references_and_malformed_trees_reject_atomically() {
        for key in [b"Headers".as_slice(), b"SD"] {
            let (mut doc, top, group, _, _, _) = fixture();
            set_identifier(&mut doc, top, Some("a")).unwrap();
            doc.update_dict(group, |d| d.set(key, Object::String(PdfString::text("a")))).unwrap();
            let before = doc.get(top);
            assert!(set_identifier(&mut doc, top, Some("b")).is_err());
            assert_eq!(doc.get(top), before);
        }
        let (mut doc, top, _, _, _, _) = fixture();
        set_identifier(&mut doc, top, Some("a")).unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let mut cycle = Dict::new();
        cycle.set(b"Names", Object::Array(Vec::new()));
        let cycle = doc.add(cycle);
        doc.update_dict(cycle, |d| {
            d.remove(b"Names");
            d.set(b"Kids", Object::Array(vec![Object::Ref(cycle)]));
        })
        .unwrap();
        doc.update_dict(root, |d| d.set(b"IDTree", Object::Ref(cycle))).unwrap();
        let before = doc.get(top);
        assert!(set_identifier(&mut doc, top, None).is_err());
        assert_eq!(doc.get(top), before);
    }
    #[test]
    fn content_moves_refuse_shared_parent_arrays_and_shared_mcr_dictionaries() {
        let (mut doc, top, group, child, page, pt) = fixture();
        let mut second = dictionary(&doc, page).unwrap();
        second.set(b"StructParents", Object::Int(1));
        let second = doc.add(second);
        let cat = dictionary(&doc, doc.root().unwrap()).unwrap();
        let pages = cat.reference(b"Pages").unwrap();
        doc.update_dict(pages, |d| {
            d.set(b"Kids", Object::Array(vec![Object::Ref(page), Object::Ref(second)]));
            d.set(b"Count", Object::Int(2));
        })
        .unwrap();
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", Object::Int(0));
        mcr.set(b"Pg", Object::Ref(second));
        doc.update_dict(child, |d| d.set(b"K", Object::Array(vec![Object::Int(0), Object::Dict(mcr)]))).unwrap();
        let array = doc.add(Object::Array(vec![Object::Ref(child)]));
        doc.update_dict(pt, |d| d.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Ref(array), Object::Int(1), Object::Ref(array)]))).unwrap();
        assert!(inspect(&doc).unwrap().issues.is_empty());
        let before = doc.get(array);
        assert!(move_content(&mut doc, child, 0, top, 1).is_err());
        assert_eq!(doc.get(array), before);
        // A shared MCR without Pg inherits two different pages. Refuse rather than pinning
        // the shared dictionary and silently changing the other association.
        let other = create(&mut doc, Some(top), 1, &props("P")).unwrap();
        let mut shared = Dict::new();
        shared.set(b"Type", Object::name("MCR"));
        shared.set(b"MCID", Object::Int(0));
        let shared = doc.add(shared);
        doc.update_dict(child, |d| d.set(b"K", Object::Ref(shared))).unwrap();
        doc.update_dict(other, |d| {
            d.set(b"Pg", Object::Ref(second));
            d.set(b"K", Object::Ref(shared));
        })
        .unwrap();
        doc.update_dict(pt, |d| {
            d.set(
                b"Nums",
                Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(child)]), Object::Int(1), Object::Array(vec![Object::Ref(other)])]),
            )
        })
        .unwrap();
        assert!(inspect(&doc).unwrap().issues.iter().any(|e| e.contains("shared indirect content")));
        assert!(delete(&mut doc, group, DeleteMode::Unwrap).is_err());
        // Shared integer objects are harmless if converted by copy, never in place.
        let integer = doc.add(Object::Int(0));
        for id in [child, other] {
            doc.update_dict(id, |d| d.set(b"K", Object::Ref(integer))).unwrap();
        }
        assert!(inspect(&doc).unwrap().issues.is_empty());
        move_content(&mut doc, child, 0, top, 2).unwrap();
        assert_eq!(&*doc.get(integer), &Object::Int(0));
        assert!(inspect(&doc).unwrap().issues.is_empty());
    }
    #[test]
    fn batch_unwrap_removes_explicitly_selected_descendants_too() {
        let (mut doc, top, group, child, page, _) = fixture();
        delete_tags(&mut doc, &[group, child, child], DeleteMode::Unwrap).unwrap();
        let tree = inspect(&doc).unwrap();
        assert_eq!(tree.elements.len(), 1);
        assert_eq!(tree.elements[0].id, top);
        assert!(matches!(tree.elements[0].kids.first(),Some(TagKid::Mcr{id:0,page:Some(p),..}) if *p==page));
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
    }
    #[test]
    fn large_deep_selection_uses_indexed_parents_and_keeps_document_order() {
        let (doc, _, _, _, _, _) = fixture();
        let template = inspect(&doc).unwrap().elements[0].clone();
        let mut tree = TagTree::default();
        let mut leaves = Vec::new();
        let mut roots = Vec::new();
        // 64,000 tags and 1,000 deep selections: the old repeated linear lookup
        // performed billions of identity comparisons for this ordinary bounded input.
        for branch in 0..1000u32 {
            let mut parent = None;
            for depth in 0..64u32 {
                let id = ObjRef::new(branch * 64 + depth + 1, 3);
                let mut e = template.clone();
                e.id = id;
                e.parent = parent;
                e.kids.clear();
                tree.elements.push(e);
                if depth == 0 {
                    roots.push(id);
                }
                if depth == 63 {
                    leaves.push(id);
                }
                parent = Some(id);
            }
        }
        let mut reversed = leaves.clone();
        reversed.reverse();
        assert_eq!(selected_roots(&tree, &reversed).unwrap().iter().map(|e| e.id).collect::<Vec<_>>(), leaves);
        reversed.extend(roots.iter().rev().copied());
        assert_eq!(selected_roots(&tree, &reversed).unwrap().iter().map(|e| e.id).collect::<Vec<_>>(), roots);
    }
    #[test]
    fn indexed_selection_rejects_cyclic_missing_duplicate_and_overdeep_parents() {
        let (doc, _, _, _, _, _) = fixture();
        let template = inspect(&doc).unwrap().elements[0].clone();
        let mut tree = TagTree::default();
        let mut parent = None;
        for num in 1..=DEPTH.saturating_add(2) {
            let mut e = template.clone();
            e.id = ObjRef::new(u32::try_from(num).unwrap(), 0);
            e.parent = parent;
            parent = Some(e.id);
            tree.elements.push(e);
        }
        assert!(selected_roots(&tree, &[parent.unwrap()]).is_err());
        tree.elements.truncate(2);
        let first = tree.elements[0].id;
        let second = tree.elements[1].id;
        tree.elements[0].parent = Some(second);
        assert!(selected_roots(&tree, &[first, second]).is_err());
        tree.elements[0].parent = Some(ObjRef::new(99999, 0));
        assert!(selected_roots(&tree, &[first]).is_err());
        tree.elements[0].parent = None;
        tree.elements[1].id = first;
        assert!(selected_roots(&tree, &[first]).is_err());
    }
    #[test]
    fn omitted_structure_type_remains_editable_without_normalizing_original_nodes() {
        let (mut doc, top, group, child, page, _) = fixture();
        for id in [top, group, child] {
            doc.update_dict(id, |d| {
                d.remove(b"Type");
            })
            .unwrap();
        }
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        update(&mut doc, child, &TagProperties { title: Some(Some("Updated".into())), ..Default::default() }).unwrap();
        let created = create(&mut doc, Some(group), 1, &props("Span")).unwrap();
        move_tag(&mut doc, child, Some(top), 1).unwrap();
        move_tags(&mut doc, &[created], Some(top), 2).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.issues.is_empty(), "{:?}", tree.issues);
        assert_eq!(tree.element(child).unwrap().parent, Some(top));
        assert_eq!(tree.element(child).unwrap().page, Some(page));
        assert_eq!(tree.element(child).unwrap().title.as_deref(), Some("Updated"));
        for id in [top, group, child] {
            assert!(!doc.get(id).as_dict().unwrap().contains(b"Type"));
        }
        assert_eq!(doc.get(created).as_dict().unwrap().name(b"Type"), Some(b"StructElem".as_slice()));
        let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        let reopened = Document::open(std::sync::Arc::new(bytes)).unwrap();
        assert!(inspect(&reopened).unwrap().issues.is_empty());
    }
    #[test]
    fn explicit_wrong_structure_type_refuses_all_edits_atomically() {
        for wrong in [Object::name("Wrong"), Object::Int(7), Object::Null] {
            let (mut doc, top, group, child, _, _) = fixture();
            doc.update_dict(child, |d| d.set(b"Type", wrong)).unwrap();
            assert!(inspect(&doc).unwrap().issues.iter().any(|issue| issue.contains("invalid StructElem Type")));
            let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
            assert!(update(&mut doc, child, &props("H1")).is_err());
            assert!(create(&mut doc, Some(group), 1, &props("Span")).is_err());
            assert!(move_tag(&mut doc, child, Some(top), 1).is_err());
            assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
        }
    }
    #[test]
    fn oversized_deletion_batch_rejects_before_mutation() {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let mut ids = Vec::new();
        for _ in 0..600 {
            let mut d = Dict::new();
            d.set(b"S", Object::name("P"));
            d.set(b"P", Object::Ref(top));
            ids.push(doc.add(d));
        }
        doc.update_dict(top, |d| d.set(b"K", Object::Array(ids.iter().copied().map(Object::Ref).collect()))).unwrap();
        let tree = inspect(&doc).unwrap();
        let per_delete =
            tree.elements.len() + tree.children.len() + tree.elements.iter().map(|e| e.kids.len()).sum::<usize>() + doc.object_numbers().len();
        let count = DELETE_BATCH_WORK_LIMIT / per_delete + 1;
        assert!(count <= ids.len());
        let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        for mode in [DeleteMode::Subtree, DeleteMode::Unwrap] {
            let error = delete_tags(&mut doc, &ids[..count], mode).unwrap_err();
            assert!(error.to_string().contains("use smaller batches"));
            assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
        }
        // Subtree selection with an ancestor is one actual deletion, not 601 rescans.
        ids.push(top);
        delete_tags(&mut doc, &ids, DeleteMode::Subtree).unwrap();
        assert!(inspect(&doc).unwrap().elements.is_empty());
    }
}
