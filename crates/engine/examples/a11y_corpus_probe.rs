//! Isolated per-input operational probe. This does not establish PDF/UA conformance.
//! Run under an external timeout and memory limit; one process per input PDF.
//! Exit 0 includes explicitly refused/inapplicable edits; exit 1 means an invariant
//! or engine failure, and exit 2 means invocation/report-writing failure.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
use pdfcraft_cos::{Dict, Document as CosDocument, ObjRef, Object};
use pdfcraft_engine::a11y::tags::{DeleteMode, ParentAssociationKind, TagElement, TagKid, TagProperties, TagTree};
use pdfcraft_engine::{DocId, Document, Edit, EditError, Session, a11y};
use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as FmtWrite,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
type ProbeResult<T> = Result<T, String>;
const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PAGES: usize = 1024;
const RENDER_SIDE: f32 = 512.0;
const MAX_OBJECTS: usize = 100_000;
const MAX_GRAPH_NODES: usize = 2_000_000;
const MAX_DEPTH: usize = 128;
const MAX_DIAGNOSTICS: usize = 200;
const REVERSE_SAMPLES: usize = 16;
const FIXED_CLOCK: i64 = 1_700_000_000;
const TITLE_MARKER: &str = "PdfCraft corpus probe title";
const OPERATIONS: [&str; 14] = [
    "create_root",
    "title_property",
    "metadata_properties",
    "reorder",
    "reparent",
    "leaf_move_integer_mcid",
    "leaf_move_page_mcr",
    "leaf_move_form_mcr",
    "leaf_move_objr",
    "role_map",
    "identifier",
    "unwrap",
    "delete",
    "artifact",
];
fn fixed_clock() -> i64 {
    FIXED_CLOCK
}
fn check(ok: bool, message: impl Into<String>) -> ProbeResult<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    } // String formatting cannot fail.
    out
}
fn digest(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_ref())
}
fn debug_digest(value: &impl std::fmt::Debug) -> String {
    digest(format!("{value:?}").as_bytes())
}
fn reference(id: ObjRef) -> String {
    format!("{} {} R", id.num, id.generation)
}
fn result(status: &str, detail: impl Into<String>) -> Value {
    json!({"status":status,"detail":detail.into()})
}
fn diagnostics(values: &[String]) -> Value {
    json!({"total":values.len(),"items":values.iter().take(MAX_DIAGNOSTICS).collect::<Vec<_>>(),"omitted":values.len().saturating_sub(MAX_DIAGNOSTICS)})
}
fn doc(s: &Session, id: DocId) -> ProbeResult<&Document> {
    s.get(id).ok_or_else(|| "document missing from session".into())
}
fn tags(s: &Session, id: DocId) -> ProbeResult<TagTree> {
    doc(s, id)?.tags().map_err(|e| e.to_string())
}
fn open_copy(bytes: Arc<Vec<u8>>) -> ProbeResult<(Session, DocId)> {
    let mut s = Session::new().with_clock(fixed_clock);
    let id = s.open("corpus probe", None, bytes, None).map_err(|e| e.to_string())?;
    Ok((s, id))
}
fn exact_object(cos: &CosDocument, id: ObjRef) -> ProbeResult<Arc<Object>> {
    check(cos.generation(id.num) == id.generation, format!("stale object reference {id}"))?;
    let value = cos.try_get(id.num).map_err(|e| e.to_string())?;
    check(!matches!(&*value, Object::Null), format!("missing object {id}"))?;
    Ok(value)
}
fn exact_dictionary(cos: &CosDocument, id: ObjRef) -> ProbeResult<Dict> {
    match &*exact_object(cos, id)? {
        Object::Dict(d) => Ok(d.clone()),
        _ => Err(format!("{id} is not an exact dictionary")),
    }
}
fn exact_value(cos: &CosDocument, value: &Object) -> ProbeResult<Object> {
    match value {
        Object::Ref(id) => Ok((*exact_object(cos, *id)?).clone()),
        _ => Ok(value.clone()),
    }
}
#[derive(Clone, Debug, PartialEq)]
struct State {
    bytes: String,
    display: String,
    tree: TagTree,
    dirty: bool,
    generation: u64,
    identity: u64,
    undo: Option<String>,
    redo: Option<String>,
}
fn state(s: &Session, id: DocId) -> ProbeResult<State> {
    let d = doc(s, id)?;
    Ok(State {
        bytes: digest(&d.bytes),
        display: digest(&d.display),
        tree: d.tags().map_err(|e| e.to_string())?,
        dirty: d.dirty,
        generation: d.edit_generation(),
        identity: d.object_identity_generation(),
        undo: d.can_undo().map(str::to_owned),
        redo: d.can_redo().map(str::to_owned),
    })
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ContentKey {
    Mcid { page: Option<ObjRef>, stream: Option<ObjRef>, mcid: i64 },
    Objr { page: Option<ObjRef>, object: ObjRef },
}
fn content_key(kid: &TagKid) -> Option<ContentKey> {
    match kid {
        TagKid::Mcid { id, page } => Some(ContentKey::Mcid { page: *page, stream: None, mcid: *id }),
        TagKid::Mcr { id, page, stream } => Some(ContentKey::Mcid { page: *page, stream: *stream, mcid: *id }),
        TagKid::Objr { object, page } => Some(ContentKey::Objr { page: *page, object: *object }),
        _ => None,
    }
}
fn ownership(tree: &TagTree) -> BTreeMap<ContentKey, Vec<ObjRef>> {
    let mut out: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for e in &tree.elements {
        for kid in &e.kids {
            if let Some(key) = content_key(kid) {
                out.entry(key).or_default().push(e.id);
            }
        }
    }
    for owners in out.values_mut() {
        owners.sort();
    }
    out
}
fn kind(kid: &TagKid) -> Option<&'static str> {
    match kid {
        TagKid::Mcid { .. } => Some("integer_mcid"),
        TagKid::Mcr { stream: None, .. } => Some("page_mcr"),
        TagKid::Mcr { stream: Some(_), .. } => Some("form_mcr"),
        TagKid::Objr { .. } => Some("objr"),
        _ => None,
    }
}
fn hash_part(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}
fn hash_object(h: &mut Sha256, o: &Object, depth: usize, budget: &mut usize) -> ProbeResult<()> {
    check(depth <= MAX_DEPTH, "COS value exceeds graph depth limit")?;
    *budget = budget.checked_sub(1).ok_or_else(|| "COS value graph exceeds work limit".to_owned())?;
    match o {
        Object::Null => hash_part(h, b"null"),
        Object::Bool(v) => hash_part(h, if *v { b"true" } else { b"false" }),
        Object::Int(v) => {
            hash_part(h, b"number");
            hash_part(h, v.to_string().as_bytes());
        }
        Object::Real(v) => {
            check(v.is_finite(), "nonfinite COS number")?;
            hash_part(h, b"number");
            hash_part(h, v.to_string().as_bytes());
        }
        Object::Name(v) => {
            hash_part(h, b"name");
            hash_part(h, v);
        }
        Object::String(v) => {
            hash_part(h, b"string");
            hash_part(h, &v.bytes);
        }
        Object::Ref(r) => {
            hash_part(h, b"reference");
            hash_part(h, &r.num.to_be_bytes());
            hash_part(h, &r.generation.to_be_bytes());
        }
        Object::Array(a) => {
            hash_part(h, b"array");
            hash_part(h, &(a.len() as u64).to_be_bytes());
            for v in a {
                hash_object(h, v, depth + 1, budget)?;
            }
        }
        Object::Dict(d) => hash_dict(h, d, depth, budget)?,
        Object::Stream(s) => {
            hash_part(h, b"stream");
            hash_dict(h, &s.dict, depth, budget)?;
            hash_part(h, &s.raw);
        }
    }
    Ok(())
}
#[derive(Debug)]
struct Graph {
    objects: BTreeMap<ObjRef, String>,
    trailer: BTreeMap<Vec<u8>, String>,
    info: Option<ObjRef>,
    xref_streams: BTreeSet<ObjRef>,
}
fn hash_dict(h: &mut Sha256, d: &Dict, depth: usize, budget: &mut usize) -> ProbeResult<()> {
    hash_part(h, b"dictionary");
    hash_part(h, &(d.len() as u64).to_be_bytes());
    let sorted: BTreeMap<_, _> = d.iter().collect();
    for (key, value) in sorted {
        hash_part(h, key);
        hash_object(h, value, depth + 1, budget)?;
    }
    Ok(())
}
fn object_digest(object: &Object, budget: &mut usize) -> ProbeResult<String> {
    let mut h = Sha256::new();
    hash_object(&mut h, object, 0, budget)?;
    Ok(hex(h.finalize().as_ref()))
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum PropertyMask {
    None,
    Title(ObjRef),
    Suite(ObjRef),
    CreateRoot(ObjRef),
    CreateFirstTag { root: ObjRef, kid_array: Option<ObjRef> },
}
fn graph(bytes: Arc<Vec<u8>>, mask: PropertyMask) -> ProbeResult<Graph> {
    let cos = CosDocument::open(bytes).map_err(|e| format!("COS preservation read: {e}"))?;
    let ids = cos.object_numbers();
    check(ids.len() <= MAX_OBJECTS, format!("resource limit: {} COS objects exceeds {MAX_OBJECTS}", ids.len()))?;
    let info = cos.trailer().reference(b"Info");
    let mut objects = BTreeMap::new();
    let mut xref_streams = BTreeSet::new();
    let mut budget = MAX_GRAPH_NODES;
    for num in ids {
        let id = ObjRef::new(num, cos.generation(num));
        let mut value = (*cos.try_get(num).map_err(|e| format!("COS object {id}: {e}"))?).clone();
        if let PropertyMask::CreateFirstTag { kid_array: Some(array), .. } = mask
            && array == id
        {
            check(matches!(value, Object::Array(_)), "first-tag child array changed type")?;
            value = Object::Array(Vec::new());
        }
        if matches!(&value,Object::Stream(stream) if stream.dict.name(b"Type") == Some(b"XRef")) {
            xref_streams.insert(id);
        }
        if let Object::Dict(d) = &mut value {
            match mask {
                PropertyMask::Title(target) if target == id => {
                    d.remove(b"T");
                }
                PropertyMask::Suite(target) if target == id => {
                    for key in [b"Alt".as_slice(), b"ActualText", b"Lang", b"E"] {
                        d.remove(key);
                    }
                }
                PropertyMask::CreateRoot(target) if target == id => {
                    d.remove(b"StructTreeRoot");
                    d.remove(b"MarkInfo");
                }
                PropertyMask::CreateFirstTag { root, .. } if root == id => {
                    d.remove(b"K");
                }
                _ => {}
            }
            if Some(id) == info {
                d.remove(b"ModDate");
            }
        }
        objects.insert(id, object_digest(&value, &mut budget)?);
    }
    let mut trailer = BTreeMap::new();
    for (key, value) in cos.trailer().iter() {
        // PDF revision/xref bookkeeping changes on save. Existing ID and Info,
        // encryption, catalog, and every unknown trailer key are still checked.
        if matches!(key.as_slice(), b"Size" | b"Prev" | b"XRefStm" | b"Type" | b"W" | b"Index" | b"Length" | b"Filter" | b"DecodeParms") {
            continue;
        }
        trailer.insert(key.clone(), object_digest(value, &mut budget)?);
    }
    Ok(Graph { objects, trailer, info, xref_streams })
}
fn compare_graph(before: &Graph, after: &Graph) -> ProbeResult<()> {
    for (id, hash) in &before.objects {
        check(after.objects.get(id) == Some(hash), format!("original COS object {id} changed outside explicit edit fields/Info ModDate"))?;
    }
    for (key, hash) in &before.trailer {
        check(after.trailer.get(key) == Some(hash), format!("original trailer key {} changed", String::from_utf8_lossy(key)))?;
    }
    for key in after.trailer.keys() {
        check(before.trailer.contains_key(key) || key == b"Info", "unexpected new trailer data")?;
    }
    Ok(())
}
fn preview_xref_replacements(bytes: Arc<Vec<u8>>, ids: &[ObjRef], original: &Graph) -> ProbeResult<Vec<Value>> {
    let saved = CosDocument::open(bytes).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for id in ids {
        check(!original.objects.contains_key(id), "attempted to exempt an original object")?;
        let value = exact_object(&saved, *id)?;
        let classification = match &*value {
            Object::Stream(s) if s.dict.name(b"Type") == Some(b"XRef") => {
                check(
                    matches!(saved.xref_entry(id.num),Some(pdfcraft_cos::XrefEntry::InFile{offset,..}) if saved.revisions().last().is_some_and(|r|r.is_stream&&r.xref_offset==offset)),
                    "preview replacement is not actual current XRef",
                )?;
                "current_xref_stream"
            }
            Object::Dict(d)
                if saved.trailer().reference(b"Info") == Some(*id)
                    && d.len() == 1
                    && d.get(b"ModDate").and_then(Object::as_string).is_some_and(|s| s.to_text() == pdfcraft_cos::pdf_date(FIXED_CLOCK)) =>
            {
                "new_info_moddate"
            }
            _ => return Err(format!("preview-only XRef slot {id} became unexpected semantic data")),
        };
        out.push(json!({"object_ref":reference(*id),"classification":classification,"absent_from_original_input":true}));
    }
    Ok(out)
}

// Empty roots with separately classified orphan associations remain editable,
// but arbitrary diagnostics never qualify. The raw index/owner proof below is
// independent of the inspector's classification and repeats after serialization.
fn empty_root_creation_allowed(tree: &TagTree) -> bool {
    if !tree.elements.is_empty() || !tree.children.is_empty() || !tree.edit_blockers.is_empty() {
        return false;
    }
    if tree.preserved_associations.is_empty() {
        return tree.issues.is_empty();
    }
    if tree.root.is_none() || tree.preserved_associations.iter().any(|a| a.kind != ParentAssociationKind::UnreachableOwner) {
        return false;
    }
    let slots: BTreeSet<_> = tree.preserved_associations.iter().map(|a| (a.key, a.mcid)).collect();
    let mut warnings: Vec<_> = tree
        .preserved_associations
        .iter()
        .map(|a| format!("ParentTree key {} refers to unreachable tag {} (preserved association)", a.key, a.owner))
        .collect();
    let mut actual = tree.issues.clone();
    warnings.sort();
    actual.sort();
    slots.len() == tree.preserved_associations.len() && warnings == actual
}

#[derive(Debug, PartialEq, Eq)]
struct PreservedRootSnapshot {
    slots: BTreeMap<(i64, Option<i64>), ObjRef>,
    index_link: String,
    index_objects: BTreeMap<ObjRef, String>,
    owner_objects: BTreeMap<ObjRef, String>,
}
fn snapshot_value(cos: &CosDocument, value: &Object, objects: &mut BTreeMap<ObjRef, String>, budget: &mut usize) -> ProbeResult<Object> {
    if let Object::Ref(id) = value {
        let object = exact_object(cos, *id)?;
        objects.insert(*id, object_digest(object.as_ref(), budget)?);
        Ok((*object).clone())
    } else {
        Ok(value.clone())
    }
}
fn preserved_root_snapshot(cos: &CosDocument, tree: &TagTree) -> ProbeResult<PreservedRootSnapshot> {
    let root = tree.root.ok_or_else(|| "preserved association proof requires an existing root".to_owned())?;
    let root_dict = exact_dictionary(cos, root)?;
    let link = root_dict.get(b"ParentTree").ok_or_else(|| "preserved association proof: ParentTree missing".to_owned())?;
    let mut budget = MAX_GRAPH_NODES;
    let mut snapshot = PreservedRootSnapshot {
        slots: BTreeMap::new(),
        index_link: object_digest(link, &mut budget)?,
        index_objects: BTreeMap::new(),
        owner_objects: BTreeMap::new(),
    };
    let mut index_seen = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut pending = vec![(link.clone(), 0usize)];
    let mut visits = 0usize;
    while let Some((node, depth)) = pending.pop() {
        visits = visits.saturating_add(1);
        check(depth <= MAX_DEPTH && visits.saturating_add(pending.len()) <= MAX_OBJECTS, "preserved ParentTree exceeds proof limits")?;
        if let Object::Ref(id) = &node {
            check(index_seen.insert(*id), "preserved ParentTree has a cycle or aliased number-tree node")?;
        }
        let value = snapshot_value(cos, &node, &mut snapshot.index_objects, &mut budget)?;
        let Object::Dict(d) = value else { return Err("preserved ParentTree node must be an exact dictionary".into()) };
        match (d.get(b"Nums"), d.get(b"Kids")) {
            (Some(nums), None) => {
                let nums = snapshot_value(cos, nums, &mut snapshot.index_objects, &mut budget)?;
                let Object::Array(nums) = nums else { return Err("preserved ParentTree Nums is not an array".into()) };
                check(nums.len() % 2 == 0 && nums.len() <= MAX_OBJECTS, "preserved ParentTree Nums has invalid length")?;
                for pair in nums.as_chunks::<2>().0.iter() {
                    let key = pair.first().and_then(Object::as_int).ok_or_else(|| "preserved ParentTree key is not an integer".to_owned())?;
                    check(key >= 0 && keys.insert(key), "preserved ParentTree has a negative or duplicate key")?;
                    let raw = pair.get(1).ok_or_else(|| "preserved ParentTree value missing".to_owned())?;
                    let value = snapshot_value(cos, raw, &mut snapshot.index_objects, &mut budget)?;
                    match value {
                        Object::Array(owners) => {
                            check(owners.len() <= MAX_OBJECTS, "preserved ParentTree owner array exceeds limits")?;
                            for (index, owner) in owners.iter().enumerate() {
                                if matches!(owner, Object::Null) {
                                    continue;
                                }
                                let owner = owner.as_ref().ok_or_else(|| "preserved ParentTree MCID owner is not an exact reference".to_owned())?;
                                let mcid = i64::try_from(index).map_err(|_| "preserved MCID exceeds integer limits")?;
                                snapshot.slots.insert((key, Some(mcid)), owner);
                            }
                        }
                        Object::Dict(_) => {
                            let owner = raw.as_ref().ok_or_else(|| "preserved scalar owner is not an exact reference".to_owned())?;
                            snapshot.slots.insert((key, None), owner);
                        }
                        _ => return Err("preserved ParentTree entry is neither an owner array nor a scalar dictionary reference".into()),
                    }
                }
            }
            (None, Some(kids)) => {
                let kids = snapshot_value(cos, kids, &mut snapshot.index_objects, &mut budget)?;
                let Object::Array(kids) = kids else { return Err("preserved ParentTree Kids is not an array".into()) };
                check(
                    !kids.is_empty() && visits.saturating_add(pending.len()).saturating_add(kids.len()) <= MAX_OBJECTS,
                    "preserved ParentTree Kids exceeds limits or is empty",
                )?;
                pending.extend(kids.into_iter().map(|kid| (kid, depth.saturating_add(1))));
            }
            _ => return Err("preserved ParentTree node needs exactly one of Nums or Kids".into()),
        }
    }
    let expected: BTreeMap<_, _> = tree.preserved_associations.iter().map(|a| ((a.key, a.mcid), a.owner)).collect();
    check(
        snapshot.slots == expected && expected.len() == tree.preserved_associations.len(),
        "raw ParentTree slots differ from the complete classified preserved associations",
    )?;
    let forbidden_array = root_dict.reference(b"K");
    for owner in snapshot.slots.values().copied().collect::<BTreeSet<_>>() {
        let d = exact_dictionary(cos, owner)?;
        check(tree.element(owner).is_none() && owner != root, "preserved owner aliases reachable structure or root")?;
        check(
            d.name(b"S").is_some_and(|role| !role.is_empty()) && (!d.contains(b"Type") || d.name(b"Type") == Some(b"StructElem")),
            "preserved owner is not an exact StructElem dictionary",
        )?;
        pending.push((Object::Ref(owner), 0));
    }
    let mut seen = BTreeSet::new();
    while let Some((raw, depth)) = pending.pop() {
        visits = visits.saturating_add(1);
        check(depth <= MAX_DEPTH && visits.saturating_add(pending.len()) <= MAX_OBJECTS, "preserved owner graph exceeds proof limits")?;
        if let Object::Ref(id) = &raw {
            check(*id != root && Some(*id) != forbidden_array, "preserved owner/child graph aliases the edited root or root child array")?;
            if !seen.insert(*id) {
                continue;
            }
        }
        let value = snapshot_value(cos, &raw, &mut snapshot.owner_objects, &mut budget)?;
        match value {
            Object::Array(kids) => {
                check(visits.saturating_add(pending.len()).saturating_add(kids.len()) <= MAX_OBJECTS, "preserved child array exceeds proof limits")?;
                pending.extend(kids.into_iter().map(|kid| (kid, depth.saturating_add(1))));
            }
            Object::Dict(d) => {
                if let Some(kids) = d.get(b"K") {
                    pending.push((kids.clone(), depth.saturating_add(1)));
                }
                let content_reference = match d.name(b"Type") {
                    Some(b"OBJR") => Some(d.reference(b"Obj").ok_or_else(|| "preserved OBJR target must be an exact reference".to_owned())?),
                    Some(b"MCR") if d.contains(b"Stm") => {
                        Some(d.reference(b"Stm").ok_or_else(|| "preserved MCR stream must be an exact reference".to_owned())?)
                    }
                    _ => None,
                };
                if let Some(id) = content_reference {
                    pending.push((Object::Ref(id), depth.saturating_add(1)));
                }
            }
            _ => {}
        }
    }
    Ok(snapshot)
}
fn preserved_creation_proof(before: &CosDocument, after: &CosDocument, before_tree: &TagTree, after_tree: &TagTree) -> ProbeResult<Value> {
    check(empty_root_creation_allowed(before_tree), "create_root input has unclassified warnings, edit blockers, or existing reachable tags")?;
    check(
        before_tree.preserved_associations == after_tree.preserved_associations
            && before_tree.issues == after_tree.issues
            && after_tree.edit_blockers.is_empty(),
        "create_root changed preserved warnings or association classifications",
    )?;
    let count = before_tree.preserved_associations.len();
    if count == 0 {
        return Ok(json!({"status":"not_applicable","count":0,"edit_blockers":0,"warnings_exactly_classified":true}));
    }
    let inventory: Vec<_> = before_tree.preserved_associations.iter().map(|a| (a.kind.as_str(), a.key, a.mcid, reference(a.owner))).collect();
    let inventory_sha256 = digest(&serde_json::to_vec(&inventory).map_err(|e| e.to_string())?);
    let original = preserved_root_snapshot(before, before_tree)?;
    let candidate = preserved_root_snapshot(after, after_tree)?;
    check(original == candidate, "create_root changed preserved ParentTree slots, index graph, or owner/child graph")?;
    Ok(
        json!({"status":"passed","count":count,"classification":"unreachable_owner","association_inventory_sha256":inventory_sha256,"warnings_exactly_classified":true,"edit_blockers":0,
        "index_slots_sha256_before":debug_digest(&original.slots),"index_slots_sha256_after":debug_digest(&candidate.slots),
        "index_graph_sha256_before":debug_digest(&(&original.index_link,&original.index_objects)),"index_graph_sha256_after":debug_digest(&(&candidate.index_link,&candidate.index_objects)),
        "owner_graph_sha256_before":debug_digest(&original.owner_objects),"owner_graph_sha256_after":debug_digest(&candidate.owner_objects),
        "index_objects_checked":original.index_objects.len().saturating_add(1),"owner_objects_checked":original.owner_objects.len(),
        "verified_snapshots":["after_edit","after_redo","after_save_reopen"]}),
    )
}

fn observe_created_root(before: &TagTree, after: &TagTree) -> ProbeResult<()> {
    check(empty_root_creation_allowed(before), "create_root requires an absent or valid empty structure tree")?;
    let root = after.root.ok_or_else(|| "create_root did not create an indirect StructTreeRoot".to_owned())?;
    check(after.elements.len() == 1, "create_root must create exactly one empty Document tag")?;
    check(before.root.is_none_or(|old| old == root), "creating the first tag replaced an existing structure root")?;
    let tag = after.elements.first().ok_or_else(|| "new Document tag missing".to_owned())?;
    check(
        tag.id != root && tag.role == "Document" && tag.standard_role == "Document" && tag.parent.is_none() && tag.kids.is_empty(),
        "create_root produced the wrong tag, parentage or content associations",
    )?;
    check(
        after.children == vec![TagKid::Element { id: tag.id }] && ownership(after).is_empty(),
        "create_root did not produce exactly one empty root child",
    )?;
    let mut restored = after.clone();
    restored.root = before.root;
    restored.children.clear();
    restored.elements.clear();
    check(&restored == before, "create_root changed unrelated page, role-map, namespace or issue state")
}
#[cfg(test)]
fn root_absence(bytes: Arc<Vec<u8>>) -> ProbeResult<(ObjRef, bool)> {
    let cos = CosDocument::open(bytes).map_err(|e| e.to_string())?;
    let catalog = cos.root().ok_or_else(|| "create_root: document catalog missing".to_owned())?;
    let value = cos.get(catalog);
    let dictionary = value.as_dict().ok_or_else(|| "create_root: catalog is not a dictionary".to_owned())?;
    Ok((catalog, !dictionary.contains(b"StructTreeRoot")))
}
fn root_creation_mask(bytes: Arc<Vec<u8>>, tree: &TagTree) -> ProbeResult<PropertyMask> {
    let cos = CosDocument::open(bytes).map_err(|e| e.to_string())?;
    let catalog = cos.root().ok_or_else(|| "create_root: catalog missing".to_owned())?;
    let value = cos.get(catalog);
    let catalog_dict = value.as_dict().ok_or_else(|| "create_root: catalog is not a dictionary".to_owned())?;
    let Some(existing) = catalog_dict.get(b"StructTreeRoot") else {
        return Ok(PropertyMask::CreateRoot(catalog));
    };
    let root = existing.as_ref().ok_or_else(|| "existing direct, null or malformed StructTreeRoot must not be replaced".to_owned())?;
    check(tree.root == Some(root) && empty_root_creation_allowed(tree), "existing root is not an inspectable empty structure tree")?;
    check(cos.generation(root.num) == root.generation, "existing structure root has stale generation")?;
    let value = cos.get(root);
    let dictionary = value.as_dict().ok_or_else(|| "existing structure root is not a dictionary".to_owned())?;
    check(dictionary.name(b"Type") == Some(b"StructTreeRoot"), "existing structure root has wrong Type")?;
    let kid_array = match dictionary.get(b"K") {
        None => None,
        Some(Object::Array(kids)) if kids.is_empty() => None,
        Some(Object::Ref(id)) if cos.generation(id.num) == id.generation && cos.get(*id).as_array().is_some_and(Vec::is_empty) => Some(*id),
        _ => return Err("existing root K is not absent or a valid empty child array".into()),
    };
    if !tree.preserved_associations.is_empty() {
        preserved_root_snapshot(&cos, tree)?;
    }
    Ok(PropertyMask::CreateFirstTag { root, kid_array })
}

fn original_mark_info(cos: &CosDocument, catalog: &Dict) -> ProbeResult<Option<Dict>> {
    match catalog.get(b"MarkInfo") {
        None => Ok(None),
        Some(Object::Dict(d)) => Ok(Some(d.clone())),
        Some(Object::Ref(id)) => {
            check(cos.generation(id.num) == id.generation, "original MarkInfo has a stale indirect generation")?;
            let value = cos.try_get(id.num).map_err(|e| format!("original MarkInfo could not be read: {e}"))?;
            match &*value {
                Object::Dict(d) => Ok(Some(d.clone())),
                _ => Err("original indirect MarkInfo is not a dictionary (streams are not dictionaries here)".into()),
            }
        }
        Some(_) => Err("original MarkInfo is not a direct dictionary or exact-generation indirect dictionary".into()),
    }
}

fn created_root_proof(before_bytes: Arc<Vec<u8>>, after_bytes: Arc<Vec<u8>>, after_tree: &TagTree) -> ProbeResult<Value> {
    let before = CosDocument::open(before_bytes).map_err(|e| e.to_string())?;
    let after = CosDocument::open(after_bytes).map_err(|e| e.to_string())?;
    let before_tree = a11y::tags::inspect(&before).map_err(|e| e.to_string())?;
    let preserved_associations = preserved_creation_proof(&before, &after, &before_tree, after_tree)?;
    let catalog = before.root().ok_or_else(|| "create_root: original catalog missing".to_owned())?;
    check(after.root() == Some(catalog), "create_root changed the document catalog identity")?;
    let original_catalog_object = before.get(catalog);
    let original_catalog = original_catalog_object.as_dict().ok_or_else(|| "create_root: original catalog is not a dictionary".to_owned())?;
    let existing_root = match original_catalog.get(b"StructTreeRoot") {
        None => None,
        Some(Object::Ref(id)) => Some(*id),
        Some(_) => return Err("create_root must not replace direct, null or malformed existing StructTreeRoot values".into()),
    };
    let original_ids: BTreeSet<_> = before.object_numbers().into_iter().map(|num| ObjRef::new(num, before.generation(num))).collect();
    let root = after_tree.root.ok_or_else(|| "create_root: no indirect structure root".to_owned())?;
    let tag = after_tree.elements.first().ok_or_else(|| "create_root: no new Document tag".to_owned())?.id;
    check(!original_ids.contains(&tag), "create_root reused an original object identity for the Document tag")?;
    check(
        existing_root.map_or_else(|| !original_ids.contains(&root), |old| old == root),
        "create_root replaced an existing root or reused an original object for a new root",
    )?;
    check(
        after.generation(root.num) == root.generation && after.generation(tag.num) == tag.generation,
        "create_root generated stale object references",
    )?;
    let root_object = after.get(root);
    let root_dict = root_object.as_dict().ok_or_else(|| "structure root is not an indirect dictionary".to_owned())?;
    let mut expected_root = if let Some(old) = existing_root {
        before.get(old).as_dict().cloned().ok_or_else(|| "existing root is not a dictionary".to_owned())?
    } else {
        let mut d = Dict::new();
        d.set(b"Type", Object::name("StructTreeRoot"));
        d
    };
    let mut budget = MAX_GRAPH_NODES;
    let child = Object::Array(vec![Object::Ref(tag)]);
    if let Some(array) = expected_root.reference(b"K") {
        check(before.get(array).as_array().is_some_and(Vec::is_empty), "existing root child array was not empty")?;
        check(
            after.generation(array.num) == array.generation && object_digest(&after.get(array), &mut budget)? == object_digest(&child, &mut budget)?,
            "first-tag insertion did not preserve and update the exact indirect child array",
        )?;
    } else {
        check(expected_root.get(b"K").is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)), "existing root has malformed or nonempty K")?;
        expected_root.set(b"K", child);
    }
    let tag_object = after.get(tag);
    let tag_dict = tag_object.as_dict().ok_or_else(|| "new Document tag is not an indirect dictionary".to_owned())?;
    let mut expected_tag = Dict::new();
    expected_tag.set(b"Type", Object::name("StructElem"));
    expected_tag.set(b"S", Object::name("Document"));
    expected_tag.set(b"P", Object::Ref(root));
    check(
        object_digest(&Object::Dict(root_dict.clone()), &mut budget)? == object_digest(&Object::Dict(expected_root), &mut budget)?,
        "structure root changed beyond its intended first-child insertion",
    )?;
    check(
        object_digest(&Object::Dict(tag_dict.clone()), &mut budget)? == object_digest(&Object::Dict(expected_tag), &mut budget)?,
        "new Document tag has unexpected keys, role, properties or parent link",
    )?;
    // New-root creation may set Marked only in an actual dictionary; stale
    // references and stream dictionaries must not be silently normalized.
    // Existing-root insertion leaves opaque MarkInfo untouched instead.
    let original_mark = if existing_root.is_none() {
        original_mark_info(&before, original_catalog)?
    } else {
        original_mark_info(&before, original_catalog).ok().flatten()
    };
    let previous_marked = original_mark.as_ref().and_then(|d| d.get(b"Marked")).and_then(|v| match v {
        Object::Bool(v) => Some(*v),
        _ => None,
    });
    let mut expected_catalog = original_catalog.clone();
    if existing_root.is_none() {
        let mut mark = match original_catalog.get(b"MarkInfo") {
            Some(_) => original_mark.ok_or_else(|| "original MarkInfo was not safely interpretable".to_owned())?,
            None => Dict::new(),
        };
        mark.set(b"Marked", Object::Bool(true));
        expected_catalog.set(b"StructTreeRoot", Object::Ref(root));
        expected_catalog.set(b"MarkInfo", Object::Dict(mark));
    }
    check(
        object_digest(&after.get(catalog), &mut budget)? == object_digest(&Object::Dict(expected_catalog), &mut budget)?,
        "create_root changed catalog/MarkInfo data beyond the new root link and Marked flag",
    )?;
    Ok(json!({"status":"passed","mode":if existing_root.is_some() { "create_first_tag_existing_root" } else { "create_structure_root" },
        "original_struct_tree_root_absent":existing_root.is_none(),"original_tag_count":0,"new_indirect_root":existing_root.is_none(),"existing_root_identity_preserved":existing_root.is_some(),"new_document_tag":true,
        "catalog_ref":reference(catalog),"struct_tree_root_ref":reference(root),"document_tag_ref":reference(tag),"catalog_parent_links":"passed",
        "preserved_associations":preserved_associations,"mark_info_change":"passed","mark_info_action":if existing_root.is_some() { "preserved" } else { "marked_true" },"marked_before":previous_marked,"marked_after":if existing_root.is_some() { previous_marked } else { Some(true) },"content_associations_created":0,
        "scope":"Creates an empty Document tag only, retaining an existing empty indirect root when present. Existing drawing content is not tagged, repaired, or certified accessible."}))
}

#[derive(Clone, Debug)]
enum Expected {
    CreateRoot,
    Title { id: ObjRef, value: String },
    Properties { id: ObjRef, alt: String, actual_text: String, lang: String, expansion: String },
    Move { id: ObjRef, parent: Option<ObjRef>, index: usize },
    Leaf { source: ObjRef, source_index: usize, destination: ObjRef, index: usize },
    Role { role: String, target: String },
    Identifier { id: ObjRef, value: String },
    Remove { id: ObjRef, unwrap: bool },
}
struct Plan {
    name: &'static str,
    edit: Edit,
    expected: Expected,
    artwork: bool,
}
fn children(tree: &TagTree, parent: Option<ObjRef>) -> Option<&[TagKid]> {
    match parent {
        Some(id) => tree.element(id).map(|e| e.kids.as_slice()),
        None => Some(&tree.children),
    }
}
fn children_mut(tree: &mut TagTree, parent: Option<ObjRef>) -> ProbeResult<&mut Vec<TagKid>> {
    match parent {
        Some(id) => tree.elements.iter_mut().find(|e| e.id == id).map(|e| &mut e.kids).ok_or_else(|| "missing expected parent".into()),
        None => Ok(&mut tree.children),
    }
}
fn element_mut(tree: &mut TagTree, id: ObjRef) -> ProbeResult<&mut TagElement> {
    tree.elements.iter_mut().find(|e| e.id == id).ok_or_else(|| format!("missing expected tag {id}"))
}
fn subtree(tree: &TagTree, id: ObjRef) -> BTreeSet<ObjRef> {
    let mut out = BTreeSet::from([id]);
    for e in &tree.elements {
        if e.parent.is_some_and(|p| out.contains(&p)) {
            out.insert(e.id);
        }
    }
    out
}
fn nonempty_marker(old: Option<&str>, marker: &str) -> String {
    if old == Some(marker) { format!("{marker} 2") } else { marker.to_owned() }
}
fn plans(tree: &TagTree) -> Vec<(&'static str, Option<Plan>, &'static str)> {
    let mut out = Vec::new();
    out.push((
        "create_root",
        empty_root_creation_allowed(tree).then(|| Plan {
            name: "create_root",
            edit: Edit::CreateTag { parent: None, index: 0, properties: TagProperties { role: Some("Document".into()), ..Default::default() } },
            expected: Expected::CreateRoot,
            artwork: true,
        }),
        "structure already has tags or inspection is not an unambiguous empty tree; existing roots are never replaced",
    ));
    let first = tree.elements.first();
    out.push((
        "title_property",
        first.map(|e| {
            let value = nonempty_marker(e.title.as_deref(), TITLE_MARKER);
            Plan {
                name: "title_property",
                edit: Edit::UpdateTag { id: e.id, properties: TagProperties { title: Some(Some(value.clone())), ..Default::default() } },
                expected: Expected::Title { id: e.id, value },
                artwork: true,
            }
        }),
        "no addressable existing tag",
    ));
    out.push((
        "metadata_properties",
        first.map(|e| {
            let alt = nonempty_marker(e.alt.as_deref(), "PdfCraft corpus probe alternative text");
            let actual_text = nonempty_marker(e.actual_text.as_deref(), "PdfCraft corpus probe actual text");
            let lang = if e.lang.as_deref() == Some("en-US") { "en-GB" } else { "en-US" }.to_owned();
            let expansion = nonempty_marker(e.expansion.as_deref(), "PdfCraft corpus probe expansion");
            Plan {
                name: "metadata_properties",
                edit: Edit::UpdateTag {
                    id: e.id,
                    properties: TagProperties {
                        alt: Some(Some(alt.clone())),
                        actual_text: Some(Some(actual_text.clone())),
                        lang: Some(Some(lang.clone())),
                        expansion: Some(Some(expansion.clone())),
                        ..Default::default()
                    },
                },
                expected: Expected::Properties { id: e.id, alt, actual_text, lang, expansion },
                artwork: true,
            }
        }),
        "no addressable existing tag",
    ));
    let reorder = tree.elements.iter().find_map(|e| {
        let siblings = children(tree, e.parent)?;
        let position = siblings.iter().position(|k| *k == TagKid::Element { id: e.id })?;
        if siblings.len() < 2 {
            return None;
        }
        let index = if position == 0 { siblings.len() - 1 } else { 0 };
        Some(Plan {
            name: "reorder",
            edit: Edit::MoveTag { id: e.id, parent: e.parent, index },
            expected: Expected::Move { id: e.id, parent: e.parent, index },
            artwork: false,
        })
    });
    out.push(("reorder", reorder, "no existing tag with another mixed sibling"));
    let reparent = tree.elements.iter().find_map(|e| {
        let descendants = subtree(tree, e.id);
        let destination = tree.elements.iter().find(|d| !descendants.contains(&d.id) && e.parent != Some(d.id))?;
        Some(Plan {
            name: "reparent",
            edit: Edit::MoveTag { id: e.id, parent: Some(destination.id), index: 0 },
            expected: Expected::Move { id: e.id, parent: Some(destination.id), index: 0 },
            artwork: false,
        })
    });
    out.push(("reparent", reparent, "no distinct non-descendant destination tag"));
    for (name, wanted) in [
        ("leaf_move_integer_mcid", "integer_mcid"),
        ("leaf_move_page_mcr", "page_mcr"),
        ("leaf_move_form_mcr", "form_mcr"),
        ("leaf_move_objr", "objr"),
    ] {
        let plan = tree.elements.iter().find_map(|e| {
            let source_index = e.kids.iter().position(|kid| kind(kid) == Some(wanted))?;
            let destination = tree.elements.iter().find(|d| d.id != e.id)?;
            Some(Plan {
                name,
                edit: Edit::MoveTagContent { source: e.id, source_index, destination: destination.id, index: 0 },
                expected: Expected::Leaf { source: e.id, source_index, destination: destination.id, index: 0 },
                artwork: false,
            })
        });
        out.push((name, plan, "no content leaf of this exact kind with a distinct destination tag"));
    }
    let role =
        (0..1000).map(|n| format!("PdfCraftCorpusProbe{n}")).find(|v| !tree.role_map.contains_key(v) && tree.elements.iter().all(|e| e.role != *v));
    out.push((
        "role_map",
        tree.root.and(role).map(|role| Plan {
            name: "role_map",
            edit: Edit::SetTagRoleMapping { role: role.clone(), target: Some("Span".into()) },
            expected: Expected::Role { role, target: "Span".into() },
            artwork: false,
        }),
        "no existing structure root or unused custom role",
    ));
    let identifier =
        (0..1000).map(|n| format!("PdfCraftCorpusProbeID{n}")).find(|v| tree.elements.iter().all(|e| e.identifier.as_deref() != Some(v)));
    out.push((
        "identifier",
        first.zip(identifier).map(|(e, value)| Plan {
            name: "identifier",
            edit: Edit::SetTagIdentifier { id: e.id, identifier: Some(value.clone()) },
            expected: Expected::Identifier { id: e.id, value },
            artwork: false,
        }),
        "no existing tag or unused identifier",
    ));
    let removable = tree.elements.iter().find(|e| e.parent.is_some()).or(first);
    out.push((
        "unwrap",
        removable.map(|e| Plan {
            name: "unwrap",
            edit: Edit::DeleteTag { id: e.id, mode: DeleteMode::Unwrap },
            expected: Expected::Remove { id: e.id, unwrap: true },
            artwork: false,
        }),
        "no existing tag",
    ));
    out.push((
        "delete",
        removable.map(|e| Plan {
            name: "delete",
            edit: Edit::DeleteTag { id: e.id, mode: DeleteMode::Subtree },
            expected: Expected::Remove { id: e.id, unwrap: false },
            artwork: false,
        }),
        "no existing tag",
    ));
    let artifact = tree.elements.iter().find(|e| {
        e.kids.iter().all(|k| !matches!(k, TagKid::Element { .. })) && e.kids.iter().any(|k| matches!(k, TagKid::Mcid { .. } | TagKid::Mcr { .. }))
    });
    out.push((
        "artifact",
        artifact.map(|e| Plan {
            name: "artifact",
            edit: Edit::ArtifactTag { id: e.id, include_descendants: false },
            expected: Expected::Remove { id: e.id, unwrap: false },
            artwork: true,
        }),
        "no existing terminal tag owning MCID/MCR content",
    ));
    out
}
fn leaf_coverage(tree: &TagTree, expected: &Expected) -> Value {
    let present: BTreeSet<_> = tree.elements.iter().flat_map(|e| e.kids.iter()).filter_map(kind).collect();
    let selected = match expected {
        Expected::Leaf { source, source_index, .. } => tree.element(*source).and_then(|e| e.kids.get(*source_index)).and_then(kind),
        _ => None,
    };
    json!({"present_kinds":present,"selected_kind":selected,"untested_present_kinds":present.iter().filter(|k|Some(**k)!=selected).collect::<Vec<_>>(),"scope":"Each typed leaf operation tests one representative of precisely its selected kind."})
}
fn preorder(tree: &mut TagTree) -> ProbeResult<()> {
    let elements: BTreeMap<_, _> = tree.elements.iter().cloned().map(|e| (e.id, e)).collect();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = tree.children.clone();
    pending.reverse();
    while let Some(kid) = pending.pop() {
        if let TagKid::Element { id } = kid {
            check(seen.insert(id), "expected topology has a cycle/shared child")?;
            let e = elements.get(&id).ok_or_else(|| "expected topology child missing".to_owned())?;
            pending.extend(e.kids.iter().rev().cloned());
            out.push(e.clone());
            check(out.len() + pending.len() <= MAX_OBJECTS, "expected topology exceeds bounds")?;
        }
    }
    check(out.len() == elements.len(), "expected topology lost an element")?;
    tree.elements = out;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct MovePageProof {
    tag: ObjRef,
    page: ObjRef,
    page_index: usize,
    explicit_leaves: usize,
    affected_containers: Vec<ObjRef>,
    page_boundaries: Vec<ObjRef>,
    structure_destinations_checked: usize,
}
fn promoted(kid: &TagKid) -> TagKid {
    match kid {
        TagKid::Mcid { id, page: Some(page) } => TagKid::Mcr { id: *id, page: Some(*page), stream: None },
        _ => kid.clone(),
    }
}
fn expected_topology(before: &TagTree, expected: &Expected, page_change: Option<&MovePageProof>) -> ProbeResult<TagTree> {
    let mut tree = before.clone();
    match expected {
        Expected::CreateRoot => return Err("root creation has a separate proof".into()),
        Expected::Title { id, value } => element_mut(&mut tree, *id)?.title = Some(value.clone()),
        Expected::Properties { id, alt, actual_text, lang, expansion } => {
            let e = element_mut(&mut tree, *id)?;
            e.alt = Some(alt.clone());
            e.actual_text = Some(actual_text.clone());
            e.lang = Some(lang.clone());
            e.expansion = Some(expansion.clone());
        }
        Expected::Role { role, target } => {
            tree.role_map.insert(role.clone(), target.clone());
        }
        Expected::Identifier { id, value } => element_mut(&mut tree, *id)?.identifier = Some(value.clone()),
        Expected::Move { id, parent, index } => {
            let old = before.element(*id).ok_or_else(|| "move target missing".to_owned())?;
            let siblings = children_mut(&mut tree, old.parent)?;
            let pos = siblings.iter().position(|k| *k == TagKid::Element { id: *id }).ok_or_else(|| "move target not under old parent".to_owned())?;
            let kid = siblings.remove(pos);
            let destination = children_mut(&mut tree, *parent)?;
            check(*index <= destination.len(), "move insertion index invalid")?;
            destination.insert(*index, kid);
            let e = element_mut(&mut tree, *id)?;
            e.parent = *parent;
            if let Some(proof) = page_change {
                check(proof.tag == *id, "page proof target mismatch")?;
                check(proof.affected_containers.contains(id), "page proof omitted moved container")?;
                for affected in &proof.affected_containers {
                    let e = element_mut(&mut tree, *affected)?;
                    check(e.page.is_none() && e.page_index.is_none(), "page proof adjusts an already bound container")?;
                    e.page = Some(proof.page);
                    e.page_index = Some(proof.page_index);
                }
            }
        }
        Expected::Leaf { source, source_index, destination, index } => {
            let e = element_mut(&mut tree, *source)?;
            check(*source_index < e.kids.len(), "leaf index missing")?;
            let kid = promoted(&e.kids.remove(*source_index));
            let target = element_mut(&mut tree, *destination)?;
            check(*index <= target.kids.len(), "leaf destination index invalid")?;
            target.kids.insert(*index, kid);
        }
        Expected::Remove { id, unwrap } => {
            let old = before.element(*id).ok_or_else(|| "removed tag missing".to_owned())?;
            let removed = if *unwrap { BTreeSet::from([*id]) } else { subtree(before, *id) };
            let siblings = children_mut(&mut tree, old.parent)?;
            let pos = siblings.iter().position(|k| *k == TagKid::Element { id: *id }).ok_or_else(|| "removed tag not under parent".to_owned())?;
            siblings.splice(pos..pos + 1, if *unwrap { old.kids.iter().map(promoted).collect::<Vec<_>>() } else { Vec::new() });
            if *unwrap {
                for kid in &old.kids {
                    if let TagKid::Element { id } = kid {
                        element_mut(&mut tree, *id)?.parent = old.parent;
                    }
                }
            }
            tree.elements.retain(|e| !removed.contains(&e.id));
        }
    }
    preorder(&mut tree)?;
    Ok(tree)
}
fn observe(before: &TagTree, after: &TagTree, expected: &Expected, page_change: Option<&MovePageProof>) -> ProbeResult<()> {
    if matches!(expected, Expected::CreateRoot) {
        return observe_created_root(before, after);
    }
    let wanted = expected_topology(before, expected, page_change)?;
    check(&wanted == after, "observed semantic tree differs from independently constructed intended topology/properties/pages/namespaces/issues")?;
    check(before != after, "edit succeeded without the intended observable change")?;
    check(ownership(&wanted) == ownership(after), "exact page/stream/MCID/OBJR ownership mismatch")
}
fn raw_kids(cos: &CosDocument, d: &Dict) -> ProbeResult<Vec<Object>> {
    let Some(raw) = d.get(b"K") else { return Ok(Vec::new()) };
    match exact_value(cos, raw)? {
        Object::Array(a) => {
            check(a.len() <= MAX_OBJECTS, "raw children exceed limit")?;
            Ok(a)
        }
        Object::Null => Err("null raw child".into()),
        _ => Ok(vec![raw.clone()]),
    }
}
fn raw_inherited_page(cos: &CosDocument, id: ObjRef, root: ObjRef) -> ProbeResult<Option<ObjRef>> {
    let mut seen = BTreeSet::new();
    let mut at = id;
    for _ in 0..MAX_DEPTH {
        if at == root {
            return Ok(None);
        }
        check(seen.insert(at), "cycle in raw page ancestry")?;
        let d = exact_dictionary(cos, at)?;
        if let Some(pg) = d.get(b"Pg") {
            let pg = pg.as_ref().ok_or_else(|| "raw Pg is not an exact reference".to_owned())?;
            exact_dictionary(cos, pg)?;
            return Ok(Some(pg));
        }
        at = d.reference(b"P").ok_or_else(|| "raw ancestry missing exact parent".to_owned())?;
    }
    Err("raw page ancestry exceeds depth limit".into())
}
fn unchanged_reference(before: &CosDocument, after: &CosDocument, id: ObjRef, budget: &mut usize) -> ProbeResult<()> {
    check(
        object_digest(exact_object(before, id)?.as_ref(), budget)? == object_digest(exact_object(after, id)?.as_ref(), budget)?,
        format!("referenced object {id} changed"),
    )
}
fn bounded_destination_clone(raw: &Object, bytes_left: &mut usize) -> ProbeResult<()> {
    let scalar = |value: &Object, bytes_left: &mut usize| -> ProbeResult<()> {
        let bytes = match value {
            Object::String(value) => value.bytes.len(),
            Object::Name(value) => value.len(),
            Object::Null | Object::Bool(_) | Object::Int(_) | Object::Real(_) | Object::Ref(_) => 0,
            _ => return Err("nested/compound destination operand cannot be copied".into()),
        };
        check(bytes <= 1_048_576, "destination token exceeds string limit")?;
        *bytes_left = bytes_left.checked_sub(bytes).ok_or_else(|| "destination inventory exceeds byte limit".to_owned())?;
        Ok(())
    };
    if let Object::Array(values) = raw {
        check((2..=6).contains(&values.len()), "destination array exceeds operand limit")?;
        for value in values {
            scalar(value, bytes_left)?;
        }
        Ok(())
    } else {
        scalar(raw, bytes_left)
    }
}
fn proof_reference_chain(cos: &CosDocument, raw: &Object, budget: &mut usize) -> ProbeResult<Arc<Object>> {
    bounded_destination_clone(raw, &mut 16_777_216)?;
    let mut value = Arc::new(raw.clone());
    let mut seen = BTreeSet::new();
    for _ in 0..MAX_DEPTH {
        *budget = budget.checked_sub(1).ok_or_else(|| "destination reference proof exceeds work limit".to_owned())?;
        let Object::Ref(id) = value.as_ref() else { return Ok(value) };
        check(seen.insert(*id), "cyclic destination reference")?;
        value = exact_object(cos, *id)?;
    }
    Err("destination reference proof exceeds depth limit".into())
}
fn proof_identifier_targets(cos: &CosDocument, tree: &TagTree, budget: &mut usize) -> ProbeResult<BTreeMap<Vec<u8>, ObjRef>> {
    let mut string_bytes = 0usize;
    let mut identifiers = BTreeMap::new();
    for element in &tree.elements {
        if let Some(raw) = exact_dictionary(cos, element.id)?.get(b"ID") {
            let resolved = proof_reference_chain(cos, raw, budget)?;
            let Object::String(id) = resolved.as_ref() else { return Err("destination ID is not a string".into()) };
            string_bytes = string_bytes.saturating_add(id.bytes.len());
            check(id.bytes.len() <= 1_048_576 && string_bytes <= 16_777_216, "destination IDs exceed string limit")?;
            check(identifiers.insert(id.bytes.clone(), element.id).is_none(), "duplicate destination ID")?;
        }
    }
    let root = exact_dictionary(cos, tree.root.ok_or_else(|| "destination proof requires root".to_owned())?)?;
    let index = read_index(cos, &root, b"IDTree", true)?;
    let expected: BTreeMap<_, _> = identifiers.iter().map(|(name, id)| (IndexKey::Name(name.clone()), Object::Ref(*id))).collect();
    check(index.entries == expected, "destination IDTree is not an exact tag-ID bijection")?;
    Ok(identifiers)
}
fn prove_destinations_avoid_affected(cos: &CosDocument, tree: &TagTree, affected: &BTreeSet<ObjRef>) -> ProbeResult<usize> {
    let mut budget = MAX_GRAPH_NODES;
    let numbers = cos.object_numbers();
    check(numbers.len() <= MAX_OBJECTS, "destination object scan exceeds limit")?;
    let mut destinations = Vec::new();
    let mut inventory_bytes_left = 16_777_216;
    for number in numbers {
        let object = cos.try_get(number).map_err(|e| format!("destination scan unreadable object: {e}"))?;
        let mut stack = vec![((&*object), 0usize)];
        while let Some((value, depth)) = stack.pop() {
            budget = budget.checked_sub(1).ok_or_else(|| "destination object proof exceeds work limit".to_owned())?;
            check(depth <= MAX_DEPTH, "destination object proof exceeds depth limit")?;
            if let Some(dict) = value.as_dict() {
                if let Some(raw) = dict.get(b"SD") {
                    check(dict.name(b"S") == Some(b"GoTo"), "unsupported or remote destination context in page proof")?;
                    bounded_destination_clone(raw, &mut inventory_bytes_left)?;
                    destinations.push(raw.clone());
                }
                check(stack.len().saturating_add(dict.len()) <= budget, "destination pending dictionary limit")?;
                stack.extend(dict.iter().map(|(_, value)| (value, depth + 1)));
            } else if let Object::Array(values) = value {
                check(stack.len().saturating_add(values.len()) <= budget, "destination pending array limit")?;
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
        }
    }
    // No destination means this operation does not interpret or repair an unrelated
    // malformed IDTree. Absence was proved across every original COS object.
    if destinations.is_empty() {
        return Ok(0);
    }
    let mut string_bytes = 0usize;
    let mut identifiers = None;
    for raw in &destinations {
        let resolved_destination = proof_reference_chain(cos, raw, &mut budget)?;
        let Object::Array(destination) = resolved_destination.as_ref() else { return Err("destination must be an array".into()) };
        check((2..=6).contains(&destination.len()), "destination operand count")?;
        let mode = proof_reference_chain(cos, &destination[1], &mut budget)?;
        let size = match mode.as_name() {
            Some(b"Fit" | b"FitB") => 2,
            Some(b"FitH" | b"FitV" | b"FitBH" | b"FitBV") => 3,
            Some(b"XYZ") => 5,
            Some(b"FitR") => 6,
            _ => return Err("unsupported destination mode in page proof".into()),
        };
        check(destination.len() == size, "destination mode operand count")?;
        for raw in &destination[2..] {
            let operand = proof_reference_chain(cos, raw, &mut budget)?;
            check(
                operand.as_f64().is_some_and(f64::is_finite) || (mode.as_name() != Some(b"FitR") && matches!(operand.as_ref(), Object::Null)),
                "invalid destination coordinate",
            )?;
        }
        let resolved = proof_reference_chain(cos, &destination[0], &mut budget)?;
        let target = match resolved.as_ref() {
            Object::String(id) => {
                string_bytes = string_bytes.saturating_add(id.bytes.len());
                check(id.bytes.len() <= 1_048_576 && string_bytes <= 16_777_216, "destination targets exceed string limit")?;
                if identifiers.is_none() {
                    identifiers = Some(proof_identifier_targets(cos, tree, &mut budget)?);
                }
                *identifiers.as_ref().and_then(|ids| ids.get(&id.bytes)).ok_or_else(|| "destination ID does not name a reachable tag".to_owned())?
            }
            Object::Dict(_) => destination[0]
                .as_ref()
                .filter(|id| tree.element(*id).is_some())
                .ok_or_else(|| "destination does not name an exact reachable tag".to_owned())?,
            _ => return Err("unsupported destination target in page proof".into()),
        };
        check(!affected.contains(&target), "incoming structure destination targets newly page-bound container")?;
    }
    Ok(destinations.len())
}
fn prove_move_page_change(
    before_bytes: Arc<Vec<u8>>,
    after_bytes: Arc<Vec<u8>>,
    before: &TagTree,
    after: &TagTree,
    expected: &Expected,
) -> ProbeResult<Option<MovePageProof>> {
    let Expected::Move { id, parent: Some(parent), .. } = expected else { return Ok(None) };
    let old = before.element(*id).ok_or_else(|| "move source missing".to_owned())?;
    let new = after.element(*id).ok_or_else(|| "move result missing".to_owned())?;
    if old.page == new.page && old.page_index == new.page_index {
        return Ok(None);
    }
    check(old.page.is_none() && old.page_index.is_none(), "move changed an already assigned page")?;
    check(
        before.preserved_associations.is_empty() && after.preserved_associations.is_empty(),
        "new page binding with preserved ParentTree associations is not independently established",
    )?;
    let root = before.root.ok_or_else(|| "move root missing".to_owned())?;
    let a = CosDocument::open(before_bytes).map_err(|e| e.to_string())?;
    let b = CosDocument::open(after_bytes).map_err(|e| e.to_string())?;
    let page = raw_inherited_page(&a, *parent, root)?.ok_or_else(|| "new parent has no independently established inherited page".to_owned())?;
    let page_index = before.pages.iter().position(|p| *p == page).ok_or_else(|| "new inherited page is not an original document page".to_owned())?;
    check(
        before.pages == after.pages && new.page == Some(page) && new.page_index == Some(page_index),
        "new inherited page differs from original destination ancestry",
    )?;
    let mut budget = MAX_GRAPH_NODES;
    let mut seen = BTreeSet::new();
    let mut kid_arrays = BTreeSet::new();
    let mut pending = vec![(*id, 0usize)];
    let mut affected = Vec::new();
    let mut boundaries = Vec::new();
    let mut explicit_leaves = 0usize;
    while let Some((at, depth)) = pending.pop() {
        check(
            depth <= MAX_DEPTH && seen.insert(at) && seen.len() + pending.len() <= MAX_OBJECTS,
            "page binding proof has cyclic/shared/deep/excessive containers",
        )?;
        let old = before.element(at).ok_or_else(|| "page binding source container missing".to_owned())?;
        let new = after.element(at).ok_or_else(|| "page binding result container missing".to_owned())?;
        let d = exact_dictionary(&a, at)?;
        let actual = exact_dictionary(&b, at)?;
        check(
            !d.contains(b"Pg") && !actual.contains(b"Pg") && !d.contains(b"C") && !d.contains(b"BBox") && !d.contains(b"Ref"),
            "page-less move has explicit Pg, class, reference or page-dependent BBox",
        )?;
        check(
            old.page.is_none() && old.page_index.is_none() && new.page == Some(page) && new.page_index == Some(page_index),
            "affected container page differs from independently derived binding",
        )?;
        check(
            raw_inherited_page(&a, at, root)?.is_none() && raw_inherited_page(&b, at, root)? == Some(page),
            "raw affected ancestry has unexpected page",
        )?;
        if let Some(attrs) = d.get(b"A") {
            let Object::Dict(attrs_dict) = exact_value(&a, attrs)? else { return Err("page-less move attributes must be exact dictionaries".into()) };
            check(
                attrs_dict.len() == 2
                    && attrs_dict.name(b"O") == Some(b"Layout")
                    && matches!(attrs_dict.name(b"Placement"), Some(b"Block" | b"Inline")),
                "page-less move attributes are not placement-only Layout",
            )?;
            if let Object::Ref(id) = attrs {
                unchanged_reference(&a, &b, *id, &mut budget)?;
            }
        }
        let mut wanted = d.clone();
        if at == *id {
            wanted.set(b"P", Object::Ref(*parent));
        }
        check(
            object_digest(&Object::Dict(wanted), &mut budget)? == object_digest(&Object::Dict(actual.clone()), &mut budget)?,
            "page-less moved dictionary changed beyond selected P",
        )?;
        if let Some(array) = d.reference(b"K")
            && matches!(&*exact_object(&a, array)?, Object::Array(_))
        {
            check(kid_arrays.insert(array), "shared indirect K array in page binding proof")?;
            unchanged_reference(&a, &b, array, &mut budget)?;
        }
        let kids = raw_kids(&a, &d)?;
        check(
            !kids.is_empty() && kids.len() == old.kids.len() && kids == raw_kids(&b, &actual)? && old.kids == new.kids,
            "page-less move changed raw/content children or has empty branch",
        )?;
        affected.push(at);
        for (raw, kid) in kids.iter().zip(&old.kids) {
            let Object::Dict(leaf) = exact_value(&a, raw)? else {
                return Err("unbound integer or non-dictionary child in page binding proof".into());
            };
            if let Object::Ref(child) = raw {
                unchanged_reference(&a, &b, *child, &mut budget)?;
            }
            if let TagKid::Element { id: child } = kid {
                check(
                    raw.as_ref() == Some(*child) && leaf.reference(b"P") == Some(at) && before.element(*child).is_some_and(|e| e.parent == Some(at)),
                    "page binding child or parent identity mismatch",
                )?;
                if leaf.contains(b"Pg") {
                    let pg = leaf.reference(b"Pg").ok_or_else(|| "structural page boundary lacks exact Pg".to_owned())?;
                    check(before.pages.contains(&pg), "structural page boundary is not an original document page")?;
                    unchanged_reference(&a, &b, pg, &mut budget)?;
                    check(before.element(*child) == after.element(*child), "explicit-page boundary properties/content changed")?;
                    check(seen.insert(*child), "shared explicit-page boundary")?;
                    boundaries.push(*child);
                } else {
                    pending.push((*child, depth + 1));
                }
                continue;
            }
            let pg = leaf.reference(b"Pg").ok_or_else(|| "every unbound content leaf needs its own exact Pg".to_owned())?;
            check(before.pages.contains(&pg), "explicit leaf Pg is not an original document page")?;
            unchanged_reference(&a, &b, pg, &mut budget)?;
            match leaf.name(b"Type") {
                Some(b"MCR") => {
                    let mcid = leaf.int(b"MCID").filter(|id| *id >= 0).ok_or_else(|| "explicit MCR lacks nonnegative MCID".to_owned())?;
                    let stream = if leaf.contains(b"Stm") {
                        let stream = leaf.reference(b"Stm").ok_or_else(|| "MCR Stm lacks exact reference".to_owned())?;
                        check(matches!(&*exact_object(&a, stream)?, Object::Stream(_)), "MCR Stm is not a stream")?;
                        unchanged_reference(&a, &b, stream, &mut budget)?;
                        Some(stream)
                    } else {
                        None
                    };
                    check(content_key(kid) == Some(ContentKey::Mcid { page: Some(pg), stream, mcid }), "raw MCR identity differs from tree")?;
                }
                Some(b"OBJR") => {
                    let object = leaf.reference(b"Obj").ok_or_else(|| "OBJR missing exact object".to_owned())?;
                    unchanged_reference(&a, &b, object, &mut budget)?;
                    check(content_key(kid) == Some(ContentKey::Objr { page: Some(pg), object }), "raw OBJR identity differs from tree")?;
                }
                _ => return Err("page-less move has unsupported content child".into()),
            }
            explicit_leaves += 1;
        }
    }
    check(ownership(before) == ownership(after), "page-less move changed global content ownership")?;
    affected.sort();
    boundaries.sort();
    let targets: BTreeSet<_> = affected.iter().copied().collect();
    let structure_destinations_checked = prove_destinations_avoid_affected(&a, before, &targets)?;
    check(prove_destinations_avoid_affected(&b, after, &targets)? == structure_destinations_checked, "structure destination inventory changed")?;
    Ok(Some(MovePageProof {
        tag: *id,
        page,
        page_index,
        explicit_leaves,
        affected_containers: affected,
        page_boundaries: boundaries,
        structure_destinations_checked,
    }))
}
fn tree_report(tree: &TagTree) -> Value {
    let mut roles = BTreeMap::<String, usize>::new();
    let mut standard = BTreeMap::<String, usize>::new();
    let mut properties = BTreeMap::<&str, usize>::new();
    let mut mixed = BTreeMap::<&str, usize>::new();
    let mut mixed_elements = 0;
    for e in &tree.elements {
        *roles.entry(e.role.clone()).or_default() += 1;
        *standard.entry(e.standard_role.clone()).or_default() += 1;
        for (name, value) in [
            ("title", &e.title),
            ("alt", &e.alt),
            ("actual_text", &e.actual_text),
            ("lang", &e.lang),
            ("expansion", &e.expansion),
            ("identifier", &e.identifier),
        ] {
            if value.is_some() {
                *properties.entry(name).or_default() += 1;
            }
        }
        let mut kinds = BTreeSet::new();
        for kid in &e.kids {
            let name = kind(kid).unwrap_or(if matches!(kid, TagKid::Element { .. }) { "element" } else { "unsupported" });
            *mixed.entry(name).or_default() += 1;
            kinds.insert(name);
        }
        if kinds.len() > 1 {
            mixed_elements += 1;
        }
    }
    let owners = ownership(tree);
    let preserved: Vec<_> = tree
        .preserved_associations
        .iter()
        .take(MAX_DIAGNOSTICS)
        .map(|a| json!({"kind":a.kind.as_str(),"key":a.key,"mcid":a.mcid,"owner_ref":reference(a.owner)}))
        .collect();
    json!({"status":"passed","elements":tree.elements.len(),"root_children":tree.children.len(),"root":tree.root.map(reference),"pages":tree.pages.len(),"roles":roles,"standard_roles":standard,"properties_present":properties,"mixed_children":mixed,"elements_with_mixed_child_types":mixed_elements,"role_map":tree.role_map,"content_identities":owners.len(),"duplicate_content_ownership":owners.values().filter(|v|v.len()>1).count(),"semantic_sha256":debug_digest(tree),"ownership_sha256":debug_digest(&owners),"issues":diagnostics(&tree.issues),"edit_blockers":diagnostics(&tree.edit_blockers),"preserved_associations":{"total":tree.preserved_associations.len(),"items":preserved,"omitted":tree.preserved_associations.len().saturating_sub(MAX_DIAGNOSTICS)},"interpretation":"Inspection describes the input; no conformance is inferred from absence of diagnostics."})
}
fn render(d: &Document) -> ProbeResult<Value> {
    check(d.info.pages.len() <= MAX_PAGES, format!("resource limit: page count exceeds {MAX_PAGES}"))?;
    let mut renderer =
        PageRenderer::new(d.display.clone(), RenderConfig { password: d.password.as_deref().map(Arc::<str>::from), ..Default::default() });
    let mut pages = Vec::new();
    for (page, info) in d.info.pages.iter().enumerate() {
        let side = info.width.max(info.height);
        check(side.is_finite() && side > 0.0, "invalid page dimensions")?;
        let image = renderer.render(RenderRequest { page, scale: RENDER_SIDE / side, ..Default::default() });
        if let Some(e) = image.error {
            return Err(format!("page {} render: {e}", page + 1));
        }
        check(image.width > 0 && image.height > 0 && !image.rgba.is_empty(), "renderer returned empty pixels")?;
        check(image.width <= 513 && image.height <= 513, "renderer exceeded bounded target")?;
        pages.push(json!({"page":page,"width":image.width,"height":image.height,"rgba_sha256":digest(&image.rgba)}));
    }
    Ok(
        json!({"status":"passed","pages":pages,"pages_rendered":d.info.pages.len(),"longest_side_pixels":RENDER_SIDE,"scope":"Same-renderer pixel equality at bounded resolution proves preservation, not external rendering correctness."}),
    )
}
fn same_render(a: &Value, b: &Value) -> ProbeResult<()> {
    check(a == b, "all-page renderer pixels/dimensions changed")
}
fn checker(d: &Document) -> ProbeResult<Value> {
    let Some(report) = d.accessibility_check(&a11y::Options::default()) else {
        return Ok(result("refused", "accessibility checker unavailable for read-only document"));
    };
    let results:Vec<_>=report.results.iter().map(|r|json!({"rule":format!("{:?}",r.rule),"status":match r.status{a11y::Status::Passed=>"passed",a11y::Status::Failed=>"failed",a11y::Status::Manual=>"manual",a11y::Status::Skipped=>"skipped"},"findings":r.findings.iter().map(|f|json!({"page":f.page,"message":f.message})).collect::<Vec<_>>()})).collect();
    Ok(
        json!({"status":"passed","outcomes":{"passed":report.count(a11y::Status::Passed),"failed":report.count(a11y::Status::Failed),"manual":report.count(a11y::Status::Manual),"skipped":report.count(a11y::Status::Skipped)},"rules":results,"scope":"Checker execution and rule outcomes are separate; manual/skipped checks do not pass automatically."}),
    )
}
fn figure_diagnostic(error: &EditError, checked: &Value, tree: &TagTree, expected: &BTreeSet<ObjRef>) -> ProbeResult<Value> {
    let EditError::Accessibility(message) = error else {
        return Err(format!("unexpected Figure API error: {error}"));
    };
    let issue = message
        .strip_prefix("cannot enumerate figures: ")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("unclassified Figure API error: {error}"))?;
    check(checked["status"] == "passed", "Figure diagnostic lacks completed checker evidence")?;
    let rule = checked["rules"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["rule"] == "FiguresAltText")
        .ok_or_else(|| "Figure diagnostic lacks FiguresAltText rule".to_owned())?;
    check(matches!(rule["status"].as_str(), Some("manual" | "failed")), "Figure diagnostic checker did not report uncertainty")?;
    let finding = format!("Structure resolution incomplete: {issue}");
    check(
        rule["findings"].as_array().into_iter().flatten().any(|f| f["page"].is_null() && f["message"] == finding),
        "Figure diagnostic does not match exact checker uncertainty",
    )?;
    Ok(json!({"status":"passed","classification":"structure_resolution_incomplete","strict_error":message,
        "checker_rule":"FiguresAltText","checker_status":rule["status"],"checker_finding":finding,
        "legacy_empty_fallback_verified":true,"inspector_issue_count":tree.issues.len(),"inspector_edit_blocker_count":tree.edit_blockers.len(),
        "tag_tree_known_figure_count":expected.len(),"tag_tree_known_figure_ids":expected.iter().copied().map(reference).collect::<Vec<_>>(),
        "scope":"Diagnostic consistency only. Checker and strict Figure API share traversal; this is not independent source proof or successful Figure discovery. Acceptance requires external source-bound review."}))
}
fn figures(d: &Document, tree: &TagTree, checked: &Value) -> ProbeResult<Value> {
    let legacy = d.figures();
    let actual: BTreeSet<_> = legacy.iter().map(|f| f.obj).collect();
    let expected: BTreeSet<_> = tree
        .elements
        .iter()
        .filter(|e| {
            e.standard_role == "Figure"
                && matches!(e.standard_namespace_uri.as_deref(), Some(a11y::tags::PDF_17_NAMESPACE | a11y::tags::PDF_20_NAMESPACE))
        })
        .map(|e| e.id)
        .collect();
    let strict = match d.try_figures() {
        Ok(v) => v,
        Err(error) => {
            check(legacy.is_empty(), "legacy Figure API returned partial results after strict discovery refusal")?;
            let mut diagnostic = figure_diagnostic(&error, checked, tree, &expected)?;
            diagnostic["post_open_sha256"] = json!(digest(&d.bytes));
            return Ok(json!({"status":"refused","reason_code":"structure_resolution_incomplete","reason":error.to_string(),
                "legacy_figures":actual.len(),"tag_tree_figures":expected.len(),"known_unenumerated_figure_ids":expected.iter().copied().map(reference).collect::<Vec<_>>(),
                "diagnostic":diagnostic}));
        }
    };
    let strict: BTreeSet<_> = strict.iter().map(|f| f.obj).collect();
    check(actual == strict && actual == expected, "Figure API identity parity mismatch")?;
    Ok(
        json!({"status":"passed","legacy_figures":actual.len(),"tag_tree_figures":expected.len(),"missing_in_figures_api":[],"unexpected_in_figures_api":[]}),
    )
}
fn overlaps(a: &[f32; 4], b: &[f32; 4]) -> bool {
    a.iter().all(|x| x.is_finite()) && a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}
fn expected_hits(tree: &TagTree, regions: &BTreeMap<ContentKey, Vec<[f32; 4]>>, selection: &[f32; 4]) -> BTreeSet<ObjRef> {
    tree.elements
        .iter()
        .filter(|e| {
            e.kids.iter().filter_map(content_key).any(|key| regions.get(&key).is_some_and(|bounds| bounds.iter().any(|r| overlaps(r, selection))))
        })
        .map(|e| e.id)
        .collect()
}
fn unowned_geometry(tree: &TagTree, owners: &BTreeMap<ContentKey, Vec<ObjRef>>, regions: &BTreeMap<ContentKey, Vec<[f32; 4]>>) -> Vec<Value> {
    regions
        .iter()
        .filter(|(key, bounds)| !bounds.is_empty() && !owners.contains_key(*key))
        .map(|(key, bounds)| {
            let (kind, page, stream, mcid, object) = match key {
                ContentKey::Mcid { page, stream, mcid } => ("mcid", *page, *stream, Some(*mcid), None),
                ContentKey::Objr { page, object } => ("objr", *page, None, None, Some(*object)),
            };
            json!({"identity":format!("{key:?}"),"kind":kind,"page_ref":page.map(reference),
            "page_index":page.and_then(|p| tree.pages.iter().position(|q| *q == p)),"stream_ref":stream.map(reference),
            "mcid":mcid,"object_ref":object.map(reference),"region_count":bounds.len(),"owner_refs":[]})
        })
        .collect()
}
fn bounded_geometry_examples(items: &[Value]) -> Value {
    json!({"total":items.len(),"items":items.iter().take(MAX_DIAGNOSTICS).collect::<Vec<_>>(),
        "omitted":items.len().saturating_sub(MAX_DIAGNOSTICS)})
}
fn geometry(d: &Document, tree: &TagTree) -> ProbeResult<Value> {
    let mut all = BTreeMap::<ContentKey, Vec<[f32; 4]>>::new();
    let owners = ownership(tree);
    let mut unowned = Vec::new();
    let mut pages = Vec::new();
    let mut samples = 0usize;
    let mut issue_count = 0usize;
    for (page, info) in d.info.pages.iter().enumerate() {
        let content = d.tag_content(page).map_err(|e| format!("page {} geometry: {e}", page + 1))?;
        let pg = tree.pages.get(page).copied().ok_or_else(|| "geometry page missing from tree page list".to_owned())?;
        let mut regions = BTreeMap::<ContentKey, Vec<[f32; 4]>>::new();
        for item in &content.items {
            regions.insert(
                ContentKey::Mcid { page: Some(pg), stream: item.stream.map(|(n, g)| ObjRef::new(n, g)), mcid: item.mcid },
                item.regions.clone(),
            );
        }
        for item in &content.objects {
            regions.insert(ContentKey::Objr { page: Some(pg), object: ObjRef::new(item.object.0, item.object.1) }, item.regions.clone());
        }
        for bounds in regions.values() {
            for r in bounds {
                check(r.iter().all(|v| v.is_finite()) && r[0] <= r[2] && r[1] <= r[3], "geometry contains nonfinite/reversed region")?;
            }
        }
        let whole = [0.0, 0.0, info.width, info.height];
        let actual: BTreeSet<_> = d.tags_at_regions(page, &[whole]).map_err(|e| e.to_string())?.into_iter().collect();
        check(actual == expected_hits(tree, &regions, &whole), "whole-page reverse lookup differs from exact content ownership")?;
        let mut count = 0;
        for rect in regions.values().flat_map(|v| v.iter()).take(REVERSE_SAMPLES) {
            let actual: BTreeSet<_> = d.tags_at_regions(page, &[*rect]).map_err(|e| e.to_string())?.into_iter().collect();
            check(actual == expected_hits(tree, &regions, rect), "region reverse lookup differs from exact content ownership")?;
            count += 1;
        }
        issue_count += content.issues.len();
        samples += count;
        let page_unowned = unowned_geometry(tree, &owners, &regions);
        pages.push(json!({"page":page,"status":"passed","geometry_items":content.items.len(),"geometry_objects":content.objects.len(),
            "geometry_identities_with_bounds":regions.values().filter(|v| !v.is_empty()).count(),
            "geometry_identities_without_owner":page_unowned.len(),"unowned_geometry":bounded_geometry_examples(&page_unowned),
            "sampled_reverse_lookups":count,"whole_page_reverse_lookup":"passed","issues":diagnostics(&content.issues)}));
        unowned.extend(page_unowned);
        all.extend(regions);
    }
    let mut missing = Vec::new();
    for (key, owners) in owners {
        if all.get(&key).is_none_or(Vec::is_empty) {
            let (page, stream, mcid, object) = match &key {
                ContentKey::Mcid { page, stream, mcid } => (*page, *stream, Some(*mcid), None),
                ContentKey::Objr { page, object } => (*page, None, None, Some(*object)),
            };
            missing.push(json!({"identity":format!("{key:?}"),"page_ref":page.map(reference),"page_index":page.and_then(|p|tree.pages.iter().position(|q|*q==p)),"stream_ref":stream.map(reference),"mcid":mcid,"object_ref":object.map(reference),"owner_refs":owners.into_iter().map(reference).collect::<Vec<_>>(),"reason":if all.contains_key(&key){"empty_regions"}else{"no_geometry_entry"}}));
        }
    }
    Ok(
        json!({"status":"passed","pages_inspected":pages.len(),"pages_total":d.info.pages.len(),"pages":pages,"geometry_issue_count":issue_count,"geometry_identities_without_owner":unowned.len(),"unowned_geometry":bounded_geometry_examples(&unowned),"coverage_scope":"Unowned geometry counts only identities with nonempty regions and no exact page/stream/object/MCID reachable owner; correct empty reverse lookup is not tagging coverage.","owned_identities_without_geometry":missing.len(),"missing_geometry":{"total":missing.len(),"items":missing.iter().take(MAX_DIAGNOSTICS).collect::<Vec<_>>(),"omitted":missing.len().saturating_sub(MAX_DIAGNOSTICS),"interpretation":"Observed missing bounds require independent diagnosis; they are not inferred rendering errors."},"sample_limit":REVERSE_SAMPLES,"sampled_reverse_lookups":samples,"whole_page_reverse_lookups":d.info.pages.len()}),
    )
}
fn decoded_stream(cos: &CosDocument, raw: &Object, remaining: &mut usize) -> ProbeResult<Vec<u8>> {
    let value = exact_value(cos, raw)?;
    let Object::Stream(stream) = value else { return Err("content is not a stream".into()) };
    check(!stream.dict.contains(b"F") && stream.raw.len() <= 64 * 1024 * 1024, "external/oversized content stream")?;
    let limit = (*remaining).min(64 * 1024 * 1024);
    let filters = stream.filters();
    let bytes = if filters.is_empty() {
        check(stream.raw.len() <= limit, "content byte budget exceeded")?;
        stream.raw.as_ref().clone()
    } else {
        pdfcraft_filters::decode(&filters, &stream.raw, limit).map_err(|e| e.to_string())?
    };
    *remaining = remaining.checked_sub(bytes.len()).ok_or_else(|| "aggregate decoded stream byte limit".to_owned())?;
    Ok(bytes)
}
fn page_stream_bytes(cos: &CosDocument, page: ObjRef, remaining: &mut usize) -> ProbeResult<Vec<u8>> {
    let d = exact_dictionary(cos, page)?;
    let Some(raw) = d.get(b"Contents") else { return Ok(Vec::new()) };
    let value = exact_value(cos, raw)?;
    let list = if let Object::Array(items) = value { items } else { vec![raw.clone()] };
    check(list.len() <= MAX_OBJECTS, "too many page content streams")?;
    let mut bytes = Vec::new();
    for raw in list {
        let part = decoded_stream(cos, &raw, remaining)?;
        check(bytes.len().saturating_add(part.len()).saturating_add(1) <= MAX_INPUT_BYTES as usize, "joined page content exceeds limit")?;
        bytes.extend_from_slice(&part);
        bytes.push(b'\n');
    }
    Ok(bytes)
}
fn resource_dict(cos: &CosDocument, raw: Option<&Object>) -> ProbeResult<Dict> {
    match raw {
        None => Ok(Dict::new()),
        Some(raw) => match exact_value(cos, raw)? {
            Object::Dict(d) => Ok(d),
            _ => Err("resource value is not an exact dictionary".into()),
        },
    }
}
fn marked_properties(cos: &CosDocument, resources: &Dict, op: &pdfcraft_content::Op) -> ProbeResult<Dict> {
    check(op.operands.len() == 2, "BDC operand count differs")?;
    match op.operands.get(1) {
        Some(Object::Dict(d)) => Ok(d.clone()),
        Some(Object::Name(name)) => {
            let properties = resource_dict(cos, resources.get(b"Properties"))?;
            resource_dict(cos, Some(properties.get(name).ok_or_else(|| "named BDC property missing".to_owned())?))
        }
        _ => Err("BDC property is not direct dictionary or resource name".into()),
    }
}
fn artifact_proof(before_bytes: Arc<Vec<u8>>, after_bytes: Arc<Vec<u8>>, tree: &TagTree, target: ObjRef) -> ProbeResult<Value> {
    let old = CosDocument::open(before_bytes).map_err(|e| e.to_string())?;
    let new = CosDocument::open(after_bytes).map_err(|e| e.to_string())?;
    let selected = subtree(tree, target);
    let mut groups = BTreeMap::<(Option<ObjRef>, Option<ObjRef>), BTreeSet<i64>>::new();
    for e in tree.elements.iter().filter(|e| selected.contains(&e.id)) {
        for kid in &e.kids {
            match kid {
                TagKid::Mcid { id, page } => {
                    groups.entry((*page, None)).or_default().insert(*id);
                }
                TagKid::Mcr { id, page, stream } => {
                    groups.entry((*page, *stream)).or_default().insert(*id);
                }
                TagKid::Element { .. } => {}
                _ => return Err("artifact proof cannot accept OBJR/unsupported content".into()),
            }
        }
    }
    check(!groups.is_empty(), "artifact conversion has no marked-content identities")?;
    let old_pages = pdfcraft_model::pages(&old);
    let new_pages = pdfcraft_model::pages(&new);
    check(old_pages.iter().map(|p| p.obj).eq(new_pages.iter().map(|p| p.obj)), "artifact changed document pages")?;
    let mut remaining = MAX_INPUT_BYTES as usize;
    let mut converted = 0usize;
    let mut untouched = 0usize;
    let forms: BTreeSet<_> = groups.keys().filter_map(|(_, stream)| *stream).collect();
    let pages: BTreeSet<_> = groups.keys().filter_map(|(page, stream)| if stream.is_none() { *page } else { None }).collect();
    for ((page, stream), wanted) in &groups {
        let (a, b, resources) = if let Some(stream) = stream {
            let source = exact_object(&old, *stream)?;
            let Object::Stream(source) = &*source else { return Err("artifact Form target is not a stream".into()) };
            (
                decoded_stream(&old, &Object::Ref(*stream), &mut remaining)?,
                decoded_stream(&new, &Object::Ref(*stream), &mut remaining)?,
                resource_dict(&old, source.dict.get(b"Resources"))?,
            )
        } else {
            let page = page.ok_or_else(|| "artifact page namespace missing".to_owned())?;
            let p = old_pages.iter().find(|p| p.obj == page).ok_or_else(|| "artifact page not in original page tree".to_owned())?;
            (
                page_stream_bytes(&old, page, &mut remaining)?,
                page_stream_bytes(&new, page, &mut remaining)?,
                resource_dict(&old, p.dict.get(b"Resources"))?,
            )
        };
        let a = pdfcraft_content::parse(&a);
        let b = pdfcraft_content::parse(&b);
        check(
            a.skipped == 0 && b.skipped == 0 && a.ops.len() == b.ops.len() && a.ops.len() <= MAX_OBJECTS,
            "artifact parse/operator count differs or is incomplete",
        )?;
        let mut seen = BTreeSet::new();
        let mut budget = MAX_GRAPH_NODES;
        for (old_op, new_op) in a.ops.iter().zip(&b.ops) {
            let mut expected = old_op.clone();
            expected.span = 0..0;
            let mut observed = new_op.clone();
            observed.span = 0..0;
            let props = if old_op.is("BDC") { Some(marked_properties(&old, &resources, old_op)?) } else { None };
            if let Some(mut props) = props.filter(|d| d.int(b"MCID").is_some_and(|mcid| wanted.contains(&mcid))) {
                let mcid = props.int(b"MCID").ok_or_else(|| "selected BDC lost MCID".to_owned())?;
                check(seen.insert(mcid), "selected MCID occurs more than once in namespace")?;
                props.remove(b"MCID");
                expected.operands = vec![Object::name("Artifact"), Object::Dict(props)];
                check(new_op.is("BDC") && new_op.name(0) == Some(b"Artifact"), "selected BDC was not rewritten as Artifact")?;
                converted += 1;
            } else {
                untouched += 1;
            }
            check(
                expected.op == observed.op
                    && expected.inline == observed.inline
                    && object_digest(&Object::Array(expected.operands), &mut budget)?
                        == object_digest(&Object::Array(observed.operands), &mut budget)?,
                "artifact edit changed unrelated operator, marking, or property",
            )?;
        }
        check(&seen == wanted, "not every selected MCID was converted to Artifact")?;
    }
    let mut untouched_streams = 0;
    let mut budget = MAX_GRAPH_NODES;
    for number in old.object_numbers() {
        let id = ObjRef::new(number, old.generation(number));
        if matches!(&*old.try_get(number).map_err(|e| e.to_string())?, Object::Stream(_)) && !forms.contains(&id) {
            unchanged_reference(&old, &new, id, &mut budget)?;
            untouched_streams += 1;
        }
    }
    for page in &old_pages {
        let actual = exact_dictionary(&new, page.obj)?;
        let original = exact_dictionary(&old, page.obj)?;
        check(original.get(b"Resources") == actual.get(b"Resources"), "artifact changed a page Resources association")?;
        if !pages.contains(&page.obj) {
            check(original.get(b"Contents") == actual.get(b"Contents"), "artifact changed unrelated page Contents")?;
        }
    }
    Ok(
        json!({"status":"passed","namespaces_checked":groups.len(),"mcid_sequences_converted":converted,"unrelated_operators_preserved":untouched,"untouched_original_streams_checked":untouched_streams,"untouched_page_associations_checked":old_pages.len().saturating_sub(pages.len()),"scope":"Exact selected namespace operator rewrite, original non-target-Form stream bytes, all-page Resources, and unrelated page Contents associations."}),
    )
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IndexKey {
    Number(i64),
    Name(Vec<u8>),
}
#[derive(Default)]
struct IndexRead {
    entries: BTreeMap<IndexKey, Object>,
    nodes: BTreeSet<ObjRef>,
    arrays: BTreeSet<ObjRef>,
}
fn read_index(cos: &CosDocument, root: &Dict, key: &[u8], names: bool) -> ProbeResult<IndexRead> {
    let mut out = IndexRead::default();
    let Some(start) = root.get(key) else { return Ok(out) };
    let mut pending = vec![(start.clone(), 0usize)];
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    while let Some((raw, depth)) = pending.pop() {
        count += 1;
        check(depth <= MAX_DEPTH && count + pending.len() <= MAX_OBJECTS, "index proof exceeds limits")?;
        if let Object::Ref(id) = &raw {
            check(seen.insert(*id), "index proof has shared/cyclic node")?;
            out.nodes.insert(*id);
        }
        let Object::Dict(d) = exact_value(cos, &raw)? else { return Err("index proof node is not a dictionary".into()) };
        let entry_key = if names { b"Names".as_slice() } else { b"Nums" };
        check(!(d.contains(entry_key) && d.contains(b"Kids")), "index node has both entries and children")?;
        if let Some(raw) = d.get(b"Kids") {
            if let Object::Ref(id) = raw {
                out.arrays.insert(*id);
            }
            let Object::Array(kids) = exact_value(cos, raw)? else { return Err("index Kids is not an array".into()) };
            check(count + pending.len() + kids.len() <= MAX_OBJECTS, "too many index nodes")?;
            pending.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        } else if let Some(raw) = d.get(entry_key) {
            if let Object::Ref(id) = raw {
                out.arrays.insert(*id);
            }
            let Object::Array(entries) = exact_value(cos, raw)? else { return Err("index entries are not an array".into()) };
            check(entries.len() % 2 == 0 && entries.len() <= MAX_OBJECTS, "bad index entry count")?;
            for pair in entries.as_chunks::<2>().0.iter() {
                let rawkey = pair.first().ok_or_else(|| "missing index key".to_owned())?;
                let value = pair.get(1).ok_or_else(|| "missing index value".to_owned())?;
                let key = if names {
                    IndexKey::Name(rawkey.as_string().ok_or_else(|| "name-tree key is not string".to_owned())?.bytes.clone())
                } else {
                    IndexKey::Number(rawkey.as_int().filter(|n| *n >= 0).ok_or_else(|| "number-tree key invalid".to_owned())?)
                };
                let stored = if names {
                    value.clone()
                } else {
                    let resolved = exact_value(cos, value)?;
                    if matches!(resolved, Object::Array(_)) {
                        if let Object::Ref(id) = value {
                            out.arrays.insert(*id);
                        }
                        resolved
                    } else {
                        value.clone()
                    }
                };
                check(out.entries.insert(key, stored).is_none(), "duplicate index key")?;
            }
        } else {
            return Err("index node has neither entries nor children".into());
        }
    }
    Ok(out)
}
fn associated_parent_slot(cos: &CosDocument, key: &ContentKey) -> ProbeResult<(IndexKey, Option<usize>)> {
    let (object, mcid) = match key {
        ContentKey::Mcid { page, stream, mcid } => (stream.or(*page).ok_or_else(|| "MCID has no page/stream".to_owned())?, Some(*mcid)),
        ContentKey::Objr { object, .. } => (*object, None),
    };
    let object = exact_object(cos, object)?;
    let d = object.as_dict().ok_or_else(|| "content association object is not dictionary/stream".to_owned())?;
    let index = d.int(if mcid.is_some() { b"StructParents" } else { b"StructParent" }).ok_or_else(|| "missing content ParentTree key".to_owned())?;
    Ok((IndexKey::Number(index), mcid.map(|id| usize::try_from(id).map_err(|_| "negative MCID".to_owned())).transpose()?))
}
fn update_parent_slot(entries: &mut BTreeMap<IndexKey, Object>, key: IndexKey, slot: Option<usize>, owner: Option<ObjRef>) -> ProbeResult<()> {
    if let Some(slot) = slot {
        let value = entries.get_mut(&key).ok_or_else(|| "ParentTree array entry absent".to_owned())?;
        let Object::Array(a) = value else { return Err("ParentTree expected array".into()) };
        let value = a.get_mut(slot).ok_or_else(|| "ParentTree slot absent".to_owned())?;
        *value = owner.map(Object::Ref).unwrap_or(Object::Null);
    } else if let Some(owner) = owner {
        check(entries.contains_key(&key), "ParentTree scalar absent")?;
        entries.insert(key, Object::Ref(owner));
    } else {
        check(entries.remove(&key).is_some(), "ParentTree scalar absent")?;
    }
    Ok(())
}
fn raw_leaf_inventory(cos: &CosDocument, tree: &TagTree) -> ProbeResult<BTreeMap<ContentKey, (Object, Object)>> {
    let mut out = BTreeMap::new();
    for e in &tree.elements {
        let raw = raw_kids(cos, &exact_dictionary(cos, e.id)?)?;
        check(raw.len() == e.kids.len(), "raw/model mixed child count differs")?;
        for (kid, raw) in e.kids.iter().zip(raw) {
            if let Some(key) = content_key(kid) {
                let resolved = exact_value(cos, &raw)?;
                check(out.insert(key, (raw, resolved)).is_none(), "raw content inventory has duplicate ownership")?;
            }
        }
    }
    Ok(out)
}
fn mask_dictionary(mut value: Object, keys: &BTreeSet<Vec<u8>>) -> Object {
    if let Some(d) = value.as_dict_mut() {
        for key in keys {
            d.remove(key);
        }
    }
    value
}
fn raw_structural_proof(
    before_bytes: Arc<Vec<u8>>,
    after_bytes: Arc<Vec<u8>>,
    tree: &TagTree,
    after_tree: &TagTree,
    expected: &Expected,
    is_artifact: bool,
) -> ProbeResult<()> {
    if matches!(expected, Expected::CreateRoot | Expected::Title { .. } | Expected::Properties { .. }) {
        return Ok(());
    }
    let a = CosDocument::open(before_bytes).map_err(|e| e.to_string())?;
    let b = CosDocument::open(after_bytes).map_err(|e| e.to_string())?;
    let root = tree.root.ok_or_else(|| "structural proof root absent".to_owned())?;
    let ar = exact_dictionary(&a, root)?;
    let br = exact_dictionary(&b, root)?;
    let mut masks = BTreeMap::<ObjRef, BTreeSet<Vec<u8>>>::new();
    let mut arrays = BTreeSet::new();
    let mut allow = |id: ObjRef, keys: &[&[u8]]| {
        masks.entry(id).or_default().extend(keys.iter().map(|k| k.to_vec()));
    };
    // Only exact intended parents/containers may rewrite K. Removed and unrelated
    // structure dictionaries and indirect child arrays must remain raw-identical.
    let mut child_parents = BTreeSet::new();
    match expected {
        Expected::Move { id, parent, .. } => {
            let e = tree.element(*id).ok_or_else(|| "missing raw move target".to_owned())?;
            child_parents.insert(e.parent.unwrap_or(root));
            child_parents.insert(parent.unwrap_or(root));
            allow(*id, &[b"P", b"Pg"]);
            let moved = exact_dictionary(&b, *id)?;
            check(moved.reference(b"P") == Some(parent.unwrap_or(root)), "raw moved parent is not requested destination")?;
            if let Some(pg) = e.page {
                check(moved.reference(b"Pg") == Some(pg), "move did not pin its exact old page")?;
            } else {
                check(!moved.contains(b"Pg"), "page-less move unexpectedly gained explicit Pg")?;
            }
        }
        Expected::Leaf { source, destination, .. } => {
            child_parents.extend([*source, *destination]);
        }
        Expected::Remove { id, unwrap } => {
            let e = tree.element(*id).ok_or_else(|| "missing raw removal target".to_owned())?;
            child_parents.insert(e.parent.unwrap_or(root));
            if *unwrap {
                for kid in &e.kids {
                    if let TagKid::Element { id } = kid {
                        allow(*id, &[b"P", b"Pg"]);
                        let promoted = exact_dictionary(&b, *id)?;
                        check(promoted.reference(b"P") == Some(e.parent.unwrap_or(root)), "unwrap promoted raw parent incorrectly")?;
                        let old = tree.element(*id).ok_or_else(|| "promoted tag absent".to_owned())?;
                        if let Some(pg) = old.page {
                            check(promoted.reference(b"Pg") == Some(pg), "unwrap did not pin promoted exact page")?;
                        } else {
                            check(!promoted.contains(b"Pg"), "unwrap assigned page-less promoted tag an explicit Pg")?;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    for parent in child_parents {
        allow(parent, &[b"K"]);
        let original = exact_dictionary(&a, parent)?;
        let current = exact_dictionary(&b, parent)?;
        if let Some(id) = original.reference(b"K")
            && matches!(&*exact_object(&a, id)?, Object::Array(_))
        {
            check(current.reference(b"K") == Some(id), "edited indirect K array identity changed")?;
            arrays.insert(id);
        }
    }
    let parent_change = matches!(expected, Expected::Leaf { .. } | Expected::Remove { .. });
    if parent_change {
        let old = read_index(&a, &ar, b"ParentTree", false)?;
        let new = read_index(&b, &br, b"ParentTree", false)?;
        let mut wanted = old.entries.clone();
        match expected {
            Expected::Leaf { source, source_index, destination, .. } => {
                let kid = tree
                    .element(*source)
                    .and_then(|e| e.kids.get(*source_index))
                    .and_then(content_key)
                    .ok_or_else(|| "moved raw leaf missing".to_owned())?;
                let (key, slot) = associated_parent_slot(&a, &kid)?;
                update_parent_slot(&mut wanted, key, slot, Some(*destination))?;
            }
            Expected::Remove { id, unwrap } => {
                let removed = if *unwrap { BTreeSet::from([*id]) } else { subtree(tree, *id) };
                let parent = tree.element(*id).and_then(|e| e.parent).or(tree.root);
                for e in tree.elements.iter().filter(|e| removed.contains(&e.id)) {
                    for kid in &e.kids {
                        if let Some(key) = content_key(kid) {
                            let (index, slot) = associated_parent_slot(&a, &key)?;
                            update_parent_slot(&mut wanted, index, slot, if *unwrap { parent } else { None })?;
                            if !*unwrap && let TagKid::Objr { object, .. } = kid {
                                check(
                                    !exact_object(&b, *object)?
                                        .as_dict()
                                        .ok_or_else(|| "deleted OBJR target is no longer dictionary/stream".to_owned())?
                                        .contains(b"StructParent"),
                                    "deleted OBJR target retained StructParent backlink",
                                )?;
                                allow(*object, &[b"StructParent"]);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        check(wanted == new.entries, "ParentTree key/slot/owner mapping differs from intended edit")?;
        allow(root, &[b"ParentTree"]);
        for id in old.nodes {
            allow(id, &[b"Nums", b"Kids", b"Limits"]);
        }
        arrays.extend(index_preservation(&a, &b, ar.get(b"ParentTree"), br.get(b"ParentTree"), false, &wanted)?);
    }
    if matches!(expected, Expected::Identifier { .. } | Expected::Remove { .. }) {
        let old = read_index(&a, &ar, b"IDTree", true)?;
        let new = read_index(&b, &br, b"IDTree", true)?;
        let mut wanted = old.entries.clone();
        match expected {
            Expected::Identifier { id, value } => {
                wanted.retain(|_, v| v.as_ref() != Some(*id));
                wanted.insert(IndexKey::Name(pdfcraft_cos::PdfString::text(value).bytes), Object::Ref(*id));
                let tag = exact_dictionary(&b, *id)?;
                check(
                    matches!(tag.get(b"ID"),Some(Object::String(s)) if s.bytes==pdfcraft_cos::PdfString::text(value).bytes),
                    "raw tag ID bytes do not match exact IDTree key",
                )?;
                allow(*id, &[b"ID"]);
            }
            Expected::Remove { id, unwrap } => {
                let removed = if *unwrap { BTreeSet::from([*id]) } else { subtree(tree, *id) };
                wanted.retain(|_, v| !v.as_ref().is_some_and(|r| removed.contains(&r)));
            }
            _ => {}
        }
        check(wanted == new.entries, "IDTree entries differ from intended identifier/deletion edit")?;
        allow(root, &[b"IDTree"]);
        for id in old.nodes {
            allow(id, &[b"Names", b"Kids", b"Limits"]);
        }
        arrays.extend(index_preservation(&a, &b, ar.get(b"IDTree"), br.get(b"IDTree"), true, &wanted)?);
    }
    if let Expected::Role { role, target } = expected {
        let mut wanted = resource_dict(&a, ar.get(b"RoleMap"))?;
        wanted.set(role.as_bytes(), Object::name(target));
        let actual = resource_dict(&b, br.get(b"RoleMap"))?;
        check(wanted == actual, "raw RoleMap changed outside requested mapping")?;
        if let Some(id) = ar.reference(b"RoleMap") {
            check(br.reference(b"RoleMap") == Some(id), "RoleMap indirect identity changed")?;
            allow(id, &[role.as_bytes()]);
        } else {
            allow(root, &[b"RoleMap"]);
        }
    }
    let old_leaves = raw_leaf_inventory(&a, tree)?;
    let new_leaves = raw_leaf_inventory(&b, after_tree)?;
    let old_owners = ownership(tree);
    let new_owners = ownership(after_tree);
    for (key, (raw, resolved)) in &old_leaves {
        if let Some((new_raw, new_resolved)) = new_leaves.get(key) {
            let moved = old_owners.get(key) != new_owners.get(key);
            let mut wanted = resolved.clone();
            if moved {
                let page = match key {
                    ContentKey::Mcid { page, .. } | ContentKey::Objr { page, .. } => *page,
                };
                if let Object::Int(mcid) = wanted {
                    let mut d = Dict::new();
                    d.set(b"Type", Object::name("MCR"));
                    d.set(b"MCID", mcid);
                    if let Some(page) = page {
                        d.set(b"Pg", Object::Ref(page));
                    }
                    wanted = Object::Dict(d);
                } else if let Some(page) = page {
                    let d = wanted.as_dict_mut().ok_or_else(|| "moved leaf is not dictionary".to_owned())?;
                    d.set(b"Pg", Object::Ref(page));
                }
            }
            check(&wanted == new_resolved, "raw retained content dictionary lost or changed properties")?;
            if matches!(resolved, Object::Int(_)) && !moved {
                check(raw == new_raw, "unmoved integer MCID raw identity changed")?;
            }
            if !matches!(resolved, Object::Int(_)) {
                check(raw.as_ref() == new_raw.as_ref(), "indirect content dictionary identity changed")?;
                if moved && let Some(id) = raw.as_ref() {
                    allow(id, &[b"Pg"]);
                }
            }
        }
    }
    let mut forms = BTreeSet::new();
    if is_artifact && let Expected::Remove { id, .. } = expected {
        for e in tree.elements.iter().filter(|e| subtree(tree, *id).contains(&e.id)) {
            for kid in &e.kids {
                match kid {
                    TagKid::Mcid { page: Some(p), .. } | TagKid::Mcr { page: Some(p), stream: None, .. } => allow(*p, &[b"Contents"]),
                    TagKid::Mcr { stream: Some(stream), .. } => {
                        forms.insert(*stream);
                    }
                    _ => {}
                }
            }
        }
    }
    let info = a.trailer().reference(b"Info");
    if let Some(info) = info {
        allow(info, &[b"ModDate"]);
    }
    let mut budget = MAX_GRAPH_NODES;
    for number in a.object_numbers() {
        let id = ObjRef::new(number, a.generation(number));
        let old = a.try_get(number).map_err(|e| e.to_string())?;
        check(b.generation(number) == id.generation, "structural edit changed original object generation")?;
        let new = b.try_get(number).map_err(|e| e.to_string())?;
        if arrays.contains(&id) {
            check(matches!(&*old, Object::Array(_)) && matches!(&*new, Object::Array(_)), "edited structural/index array changed type")?;
            continue;
        }
        if forms.contains(&id) {
            let (Object::Stream(old), Object::Stream(new)) = (&*old, &*new) else { return Err("artifact Form changed type".into()) };
            same_form_dictionary(&old.dict, &new.dict, &mut budget)?;
            continue;
        }
        let keys = masks.get(&id).cloned().unwrap_or_default();
        let x = mask_dictionary((*old).clone(), &keys);
        let y = mask_dictionary((*new).clone(), &keys);
        check(
            object_digest(&x, &mut budget)? == object_digest(&y, &mut budget)?,
            format!("structural edit altered unrepresented data in original object {id}"),
        )?;
    }
    Ok(())
}
fn same_form_dictionary(old: &Dict, new: &Dict, budget: &mut usize) -> ProbeResult<()> {
    let mut x = old.clone();
    let mut y = new.clone();
    for key in [b"Length".as_slice(), b"Filter", b"DecodeParms"] {
        x.remove(key);
        y.remove(key);
    }
    check(object_digest(&Object::Dict(x), budget)? == object_digest(&Object::Dict(y), budget)?, "artifact changed Form dictionary properties")
}
fn operation_error(e: &EditError) -> (&'static str, &'static str) {
    match e {
        EditError::Accessibility(_) => ("refused", "structure_edit_rejected"),
        EditError::NotPermitted(_) => ("refused", "permission_denied"),
        EditError::ReadOnly(_) => ("refused", "read_only"),
        EditError::Signed | EditError::SignedRewrite(_) => ("refused", "signed_document"),
        _ => ("error", "engine_error"),
    }
}
fn same_undo_objects(before: Arc<Vec<u8>>, after: Arc<Vec<u8>>) -> ProbeResult<()> {
    let a = CosDocument::open(before).map_err(|e| e.to_string())?;
    let b = CosDocument::open(after).map_err(|e| e.to_string())?;
    let mut budget = MAX_GRAPH_NODES;
    for n in a.object_numbers() {
        let id = ObjRef::new(n, a.generation(n));
        check(b.generation(n) == id.generation, "undo changed original generation")?;
        check(
            object_digest(a.try_get(n).map_err(|e| e.to_string())?.as_ref(), &mut budget)?
                == object_digest(b.try_get(n).map_err(|e| e.to_string())?.as_ref(), &mut budget)?,
            format!("undo changed original object {id}"),
        )?;
    }
    Ok(())
}
fn run_edit(bytes: Arc<Vec<u8>>, original: &TagTree, original_render: &Value, plan: Plan, artifact_dir: Option<&Path>) -> ProbeResult<Value> {
    let (mut s, id) = open_copy(bytes)?;
    let before = state(&s, id)?;
    check(&before.tree == original, "independent copy opened with different semantic tree")?;
    let before_bytes = doc(&s, id)?.bytes.clone();
    let coverage = leaf_coverage(original, &plan.expected);
    let start = Instant::now();
    let mask = match &plan.expected {
        Expected::CreateRoot => match root_creation_mask(before_bytes.clone(), original) {
            Ok(mask) => mask,
            Err(reason) => {
                return Ok(
                    json!({"operation":plan.name,"status":"refused","reason_code":"existing_structure_root","reason":reason,"unchanged_on_failure":state(&s,id)?==before}),
                );
            }
        },
        Expected::Title { id, .. } => PropertyMask::Title(*id),
        Expected::Properties { id, .. } => PropertyMask::Suite(*id),
        _ => PropertyMask::None,
    };
    let title = matches!(plan.expected, Expected::Title { .. });
    let properties = matches!(plan.expected, Expected::Properties { .. });
    let create = matches!(plan.expected, Expected::CreateRoot);
    let before_graph = graph(before_bytes.clone(), mask)?;
    let before_checker = if title { Some(checker(doc(&s, id)?)?) } else { None };
    if let Err(e) = s.apply(id, plan.edit) {
        check(state(&s, id)? == before, format!("{} failed but changed document bytes/display/tree/generations/history", plan.name))?;
        let (status, code) = operation_error(&e);
        return Ok(
            json!({"operation":plan.name,"status":status,"reason_code":code,"reason":e.to_string(),"unchanged_on_failure":true,"target":format!("{:?}",plan.expected),"leaf_move_coverage":coverage,"elapsed_ms":start.elapsed().as_millis()}),
        );
    }
    let after = tags(&s, id)?;
    let after_bytes = doc(&s, id)?.bytes.clone();
    let after_state = state(&s, id)?;
    let page_change = prove_move_page_change(before_bytes.clone(), after_bytes.clone(), original, &after, &plan.expected)?;
    observe(original, &after, &plan.expected, page_change.as_ref())?;
    let root_proof = if create {
        created_root_proof(before_bytes.clone(), after_bytes.clone(), &after)?
    } else {
        result("not_applicable", "operation does not create structure")
    };
    let artifact = if plan.name == "artifact" {
        let Expected::Remove { id, .. } = plan.expected else { return Err("artifact target missing".into()) };
        artifact_proof(before_bytes.clone(), after_bytes.clone(), original, id)?
    } else {
        result("not_applicable", "operation does not convert marked content")
    };
    raw_structural_proof(before_bytes.clone(), after_bytes.clone(), original, &after, &plan.expected, plan.name == "artifact")?;
    let after_graph = graph(after_bytes.clone(), mask)?;
    if mask != PropertyMask::None {
        compare_graph(&before_graph, &after_graph)?;
    }
    same_render(original_render, &render(doc(&s, id)?)?)?;
    if let Some(checks) = &before_checker {
        check(*checks == checker(doc(&s, id)?)?, "harmless title changed checker findings")?;
    }
    s.undo(id).map_err(|e| e.to_string())?;
    check(tags(&s, id)? == *original, "undo did not restore exact original tree")?;
    same_undo_objects(before_bytes.clone(), doc(&s, id)?.bytes.clone())?;
    let undone = doc(&s, id)?;
    check(
        undone.dirty && undone.can_undo().map(str::to_owned) == before.undo && undone.can_redo().map(str::to_owned) == after_state.undo,
        "undo dirty/history state mismatch",
    )?;
    same_render(original_render, &render(doc(&s, id)?)?)?;
    s.redo(id).map_err(|e| e.to_string())?;
    check(tags(&s, id)? == after, "redo did not restore exact edited tree")?;
    same_undo_objects(after_bytes.clone(), doc(&s, id)?.bytes.clone())?;
    let redone = state(&s, id)?;
    check(
        redone.display == after_state.display
            && redone.dirty == after_state.dirty
            && redone.undo == after_state.undo
            && redone.redo == after_state.redo,
        "redo did not restore display/dirty/history state",
    )?;
    same_render(original_render, &render(doc(&s, id)?)?)?;
    check(
        prove_move_page_change(before_bytes.clone(), doc(&s, id)?.bytes.clone(), original, &after, &plan.expected)? == page_change,
        "redo changed explicit-page proof",
    )?;
    if create {
        check(created_root_proof(before_bytes.clone(), doc(&s, id)?.bytes.clone(), &after)? == root_proof, "redo changed create-root proof")?;
    }
    if plan.name == "artifact" {
        let Expected::Remove { id: target, .. } = plan.expected else { return Err("artifact target missing".into()) };
        artifact_proof(before_bytes.clone(), doc(&s, id)?.bytes.clone(), original, target)?;
    }
    let mut saved_graph = graph(doc(&s, id)?.bytes.clone(), PropertyMask::None)?;
    let preview: Vec<_> = saved_graph.xref_streams.iter().copied().filter(|id| !before_graph.objects.contains_key(id)).collect();
    for id in &preview {
        saved_graph.objects.remove(id);
    }
    let saved = s.save_bytes(id).map_err(|e| e.to_string())?;
    let replacements = preview_xref_replacements(saved.clone(), &preview, &before_graph)?;
    let (reopened, rid) = open_copy(saved.clone())?;
    let reopened_tree = tags(&reopened, rid)?;
    check(reopened_tree == after, "save/reopen changed exact semantic tree")?;
    compare_graph(&saved_graph, &graph(doc(&reopened, rid)?.bytes.clone(), PropertyMask::None)?)?;
    if mask != PropertyMask::None {
        compare_graph(&before_graph, &graph(doc(&reopened, rid)?.bytes.clone(), mask)?)?;
    }
    raw_structural_proof(
        before_bytes.clone(),
        doc(&reopened, rid)?.bytes.clone(),
        original,
        &reopened_tree,
        &plan.expected,
        plan.name == "artifact",
    )?;
    check(
        prove_move_page_change(before_bytes.clone(), doc(&reopened, rid)?.bytes.clone(), original, &reopened_tree, &plan.expected)? == page_change,
        "reopen changed explicit-page proof",
    )?;
    if create {
        check(
            created_root_proof(before_bytes.clone(), doc(&reopened, rid)?.bytes.clone(), &reopened_tree)? == root_proof,
            "reopen changed root proof",
        )?;
    }
    if plan.name == "artifact" {
        let Expected::Remove { id: target, .. } = plan.expected else { return Err("artifact target missing".into()) };
        artifact_proof(before_bytes.clone(), doc(&reopened, rid)?.bytes.clone(), original, target)?;
    }
    same_render(original_render, &render(doc(&reopened, rid)?)?)?;
    if let Some(checks) = &before_checker {
        check(*checks == checker(doc(&reopened, rid)?)?, "reopened harmless title changed checker findings")?;
    }
    let artifact_path = if let Some(dir) = artifact_dir {
        let path = dir.join(format!("{}.pdf", plan.name));
        fs::write(&path, &*saved).map_err(|e| e.to_string())?;
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    };
    let mut out = json!({"operation":plan.name,"status":"passed","observed_edit":true,"target":format!("{:?}",plan.expected),"undo":"passed","redo":"passed","save_reopen":"passed","exact_ownership":"passed","all_page_artwork":"passed","unknown_data_save_reopen":"passed","unknown_data_before_after":"passed","save_api":"Session::save_bytes","requested_save_mode":"incremental (engine may rebuild repaired inputs)","saved_has_original_prefix":saved.starts_with(&before_bytes),"full_save_tested":false,"rendered_pages_per_snapshot":doc(&s,id)?.info.pages.len(),"artwork_snapshots":["original","after_edit","after_undo","after_redo","after_save_reopen"],"saved_sha256":digest(&saved),"artifact_path":artifact_path,"elapsed_ms":start.elapsed().as_millis()});
    out["successful_undo_dirty_state"] = json!("dirty=true by Session undo contract; original content/tree/pixels restored");
    out["unknown_data_harmless_edit"] = json!(if title { "passed" } else { "not_applicable" });
    out["checker_findings_preserved"] = json!(if title { "passed" } else { "not_applicable" });
    out["unknown_data_property_edit"] = json!(if properties { "passed" } else { "not_applicable" });
    out["changed_properties"] = json!(if properties {
        vec!["alt", "actual_text", "lang", "expansion"]
    } else if title {
        vec!["title"]
    } else {
        vec![]
    });
    out["properties_observed"] = json!(if properties {
        vec!["Alt", "ActualText", "Lang", "E"]
    } else if title {
        vec!["T"]
    } else {
        vec![]
    });
    out["unknown_data_create_root"] = json!(if create { "passed" } else { "not_applicable" });
    out["root_creation_evidence"] = root_proof;
    out["semantic_artifact_rewrite"] = artifact["status"].clone();
    out["artifact_rewrite_evidence"] = artifact;
    out["leaf_move_coverage"] = coverage;
    out["content_identities_before"] = json!(ownership(original).len());
    out["content_identities_after"] = json!(ownership(&after).len());
    out["ownership_sha256_before"] = json!(debug_digest(&ownership(original)));
    out["ownership_sha256_after"] = json!(debug_digest(&ownership(&after)));
    out["semantic_sha256_before"] = json!(debug_digest(original));
    out["semantic_sha256_after"] = json!(debug_digest(&after));
    out["original_info_object"] = json!(before_graph.info.map(reference));
    out["original_objects_checked"] = json!(before_graph.objects.len());
    out["saved_objects_checked"] = json!(saved_graph.objects.len());
    out["generated_preview_xref_objects_excluded"] = json!(preview.into_iter().map(reference).collect::<Vec<_>>());
    out["generated_preview_xref_replacements"] = json!(replacements);
    out["representative_artwork_case"] = json!(plan.artwork);
    out["inherited_container_page_change"]=page_change.map(|p|json!({"status":"passed","tag_ref":reference(p.tag),"from_page":null,"to_page_ref":reference(p.page),"to_page_index":p.page_index,"explicit_content_leaves":p.explicit_leaves,"affected_container_refs":p.affected_containers.into_iter().map(reference).collect::<Vec<_>>(),"explicit_page_boundary_refs":p.page_boundaries.into_iter().map(reference).collect::<Vec<_>>(),"structure_destinations_checked":p.structure_destinations_checked,"incoming_destinations_to_affected":"absent","raw_Pg_remains_absent":true,"content_identities_preserved":true,"scope":"Only independently proved container inheritance changed; all explicit leaf pages, objects, streams and properties stayed exact."})).unwrap_or(Value::Null);
    Ok(out)
}
fn guarded<T>(f: impl FnOnce() -> ProbeResult<T>) -> ProbeResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        format!(
            "engine/probe panic: {}",
            payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("non-string panic")
        )
    })?
}
fn checkpoint(path: &Path, report: &Value) -> ProbeResult<()> {
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    fs::write(path, bytes).map_err(|e| format!("write report: {e}"))
}
fn stage(report: &mut Value, path: &Path, name: &str, f: impl FnOnce() -> ProbeResult<Value>) -> ProbeResult<Value> {
    report["active_stage"] = json!(name);
    checkpoint(path, report)?;
    let value = match guarded(f) {
        Ok(v) => v,
        Err(e) => json!({"status":"error","detail":e}),
    };
    report["stages"][name] = value.clone();
    checkpoint(path, report)?;
    Ok(value)
}
fn summarize(report: &mut Value) -> bool {
    let mut counts = BTreeMap::<String, usize>::new();
    for edit in report["edits"].as_array().into_iter().flatten() {
        if let Some(status) = edit["status"].as_str() {
            *counts.entry(status.into()).or_default() += 1;
        }
    }
    let stage_error = report["stages"].as_object().is_some_and(|stages| stages.values().any(|v| v["status"] == "error"));
    let error = stage_error || counts.get("error").copied().unwrap_or(0) > 0;
    let incomplete = counts.get("refused").copied().unwrap_or(0) > 0
        || report["stages"].as_object().is_some_and(|stages| stages.values().any(|v| v["status"] == "refused"));
    report["operation_status_counts"] = json!(counts);
    report["outcome"] = json!(if error {
        "error"
    } else if incomplete {
        "incomplete"
    } else {
        "passed"
    });
    report["active_stage"] = Value::Null;
    error
}
fn probe(input: &Path, output: &Path, artifact_dir: Option<&Path>) -> ProbeResult<bool> {
    let mut file = fs::File::open(input).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    check(bytes.len() as u64 <= MAX_INPUT_BYTES, "input exceeds 128 MiB probe limit")?;
    let bytes = Arc::new(bytes);
    let original_sha = digest(&bytes);
    if let Some(dir) = artifact_dir {
        prepare_artifacts(dir)?;
    }
    let mut report = json!({"schema_version":1,"probe":"pdfcraft-engine a11y_corpus_probe","engine_version":env!("CARGO_PKG_VERSION"),"input":input.to_string_lossy(),"active_stage":"input","outcome":"running","stages":{},"edits":[],"limits":{"input_bytes":MAX_INPUT_BYTES,"pages":MAX_PAGES,"render_side":RENDER_SIDE,"objects":MAX_OBJECTS,"graph_values":MAX_GRAPH_NODES,"depth":MAX_DEPTH},"scope":"Operational preservation only; source classifications and independent accessibility oracle are external.","pixel_hash_note":"Same-renderer 512-pixel longest-side preservation; not external rendering correctness."});
    report["stages"]["input"] = json!({"status":"passed","bytes":bytes.len(),"sha256":original_sha});
    checkpoint(output, &report)?;
    report["active_stage"] = json!("open");
    checkpoint(output, &report)?;
    let opened = guarded(|| open_copy(bytes.clone()));
    let (s, id) = match opened {
        Ok(v) => v,
        Err(error) => {
            report["stages"]["open"] = json!({"status":"error","detail":error});
            for op in OPERATIONS {
                report["edits"].as_array_mut().ok_or_else(|| "missing edit array".to_owned())?.push(
                    json!({"operation":op,"status":"not_applicable","reason_code":"inspection_prerequisite_failed","reason":"document did not open"}),
                );
            }
            summarize(&mut report);
            checkpoint(output, &report)?;
            return Ok(true);
        }
    };
    let initial_state = state(&s, id).ok();
    let document = doc(&s, id)?;
    let changed = *document.bytes != *bytes;
    let dynamic = document.xfa.is_some();
    let post_open = if let Some(dir) = artifact_dir {
        let path = dir.join("post_open.pdf");
        fs::write(&path, &*document.bytes).map_err(|e| e.to_string())?;
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    };
    report["stages"]["open"] = json!({"status":"passed","pages":document.info.pages.len(),"encrypted":document.info.encrypted,"warnings":diagnostics(&document.info.warnings),"read_only_reason":document.read_only_reason,"tag_edit_read_only_reason":document.tag_edit_read_only_reason(),"dynamic_xfa_regenerated":dynamic,"bytes_changed_during_open":changed,"open_transform_kind":if dynamic{"dynamic_xfa_layout"}else if changed{"other"}else{"none"},"post_open_sha256":digest(&document.bytes),"post_open_artifact_path":post_open,"source_sha256":original_sha,"source_to_open_preservation":if changed{"transformed"}else{"byte_identical"}});
    let rendered = stage(&mut report, output, "render", || render(document))?;
    let checked = stage(&mut report, output, "checker", || checker(document))?;
    let inspected = guarded(|| tags(&s, id));
    match inspected {
        Ok(tree) => {
            report["stages"]["inspect_tags"] = tree_report(&tree);
            checkpoint(output, &report)?;
            let parity = stage(&mut report, output, "figures_api_parity", || figures(document, &tree, &checked))?;
            if let Some(diagnostic) = parity.get("diagnostic") {
                report["stages"]["figures_api_diagnostic"] = diagnostic.clone();
                checkpoint(output, &report)?;
            }
            stage(&mut report, output, "geometry", || geometry(document, &tree))?;
            for (name, plan, reason) in plans(&tree) {
                report["active_stage"] = json!(format!("edit:{name}"));
                checkpoint(output, &report)?;
                let outcome = if let Some(plan) = plan {
                    if rendered["status"] != "passed" {
                        json!({"operation":name,"status":"error","reason_code":"render_prerequisite_failed","reason":"baseline render unavailable"})
                    } else {
                        match guarded(|| run_edit(bytes.clone(), &tree, &rendered, plan, artifact_dir)) {
                            Ok(v) => v,
                            Err(e) => json!({"operation":name,"status":"error","reason_code":"invariant_failed","reason":e}),
                        }
                    }
                } else {
                    json!({"operation":name,"status":"not_applicable","reason_code":"precondition_absent","reason":reason})
                };
                report["edits"].as_array_mut().ok_or_else(|| "missing edit array".to_owned())?.push(outcome);
                checkpoint(output, &report)?;
            }
            check(Some(state(&s, id)?) == initial_state, "independent edits mutated original session bytes/display/tree/dirty/generations/history")?;
        }
        Err(error) => {
            report["stages"]["inspect_tags"] = json!({"status":"error","detail":error});
            for op in OPERATIONS {
                report["edits"].as_array_mut().ok_or_else(||"missing edit array".to_owned())?.push(json!({"operation":op,"status":"not_applicable","reason_code":"inspection_prerequisite_failed","reason":"tag inspection did not yield an addressable tree"}));
            }
        }
    }
    let original_now = fs::read(input).map_err(|e| e.to_string())?;
    report["stages"]["original_unchanged"] =
        json!({"status":if digest(&original_now)==original_sha{"passed"}else{"error"},"sha256":digest(&original_now)});
    let failed = summarize(&mut report);
    checkpoint(output, &report)?;
    Ok(failed)
}
struct Args {
    input: PathBuf,
    output: PathBuf,
    artifacts: Option<PathBuf>,
}
fn args() -> ProbeResult<Args> {
    let mut input = None;
    let mut output = None;
    let mut artifacts = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let target = match arg.to_str() {
            Some("--input") => &mut input,
            Some("--output") => &mut output,
            Some("--artifacts-dir") => &mut artifacts,
            _ => return Err("usage: a11y_corpus_probe --input PATH --output PATH [--artifacts-dir FRESH_DIRECTORY]".into()),
        };
        check(target.is_none(), "duplicate option")?;
        *target = Some(PathBuf::from(args.next().ok_or_else(|| "option requires path".to_owned())?));
    }
    let input = input.ok_or_else(|| "--input required".to_owned())?;
    let output = output.ok_or_else(|| "--output required".to_owned())?;
    check(input != output, "report cannot replace input")?;
    if let (Ok(a), Ok(b)) = (input.canonicalize(), output.canonicalize()) {
        check(a != b, "report resolves to input")?;
    }
    Ok(Args { input, output, artifacts })
}
fn main() {
    let code = match args() {
        Ok(a) => match guarded(|| probe(&a.input, &a.output, a.artifacts.as_deref())) {
            Ok(false) => 0,
            Ok(true) => 1,
            Err(e) => {
                eprintln!("a11y corpus probe failed: {e}");
                2
            }
        },
        Err(e) => {
            eprintln!("{e}");
            2
        }
    };
    std::process::exit(code);
}
fn index_subtree(cos: &CosDocument, raw: &Object, names: bool) -> ProbeResult<IndexRead> {
    let mut root = Dict::new();
    root.set(b"ProbeIndex", raw.clone());
    read_index(cos, &root, b"ProbeIndex", names)
}
fn paired_array(a: &CosDocument, b: &CosDocument, x: &Object, y: &Object, changed: &mut BTreeSet<ObjRef>) -> ProbeResult<(Vec<Object>, Vec<Object>)> {
    check(x.as_ref() == y.as_ref(), "index container reference identity changed")?;
    let (Object::Array(xv), Object::Array(yv)) = (exact_value(a, x)?, exact_value(b, y)?) else {
        return Err("index container stopped being array".into());
    };
    if let Some(id) = x.as_ref() {
        changed.insert(id);
    }
    Ok((xv, yv))
}
fn index_preservation(
    a: &CosDocument,
    b: &CosDocument,
    before: Option<&Object>,
    after: Option<&Object>,
    names: bool,
    wanted: &BTreeMap<IndexKey, Object>,
) -> ProbeResult<BTreeSet<ObjRef>> {
    let mut arrays = BTreeSet::new();
    let mut budget = MAX_GRAPH_NODES;
    let Some(before) = before else {
        if let Some(after) = after {
            check(names, "unexpected new ParentTree")?;
            let Object::Dict(d) = exact_value(b, after)? else { return Err("new IDTree is not dictionary".into()) };
            check(d.len() == 1 && d.contains(b"Names"), "new IDTree has unexpected data")?;
        }
        return Ok(arrays);
    };
    let after = after.ok_or_else(|| "existing index root removed".to_owned())?;
    let mut pending = vec![(before.clone(), after.clone(), 0usize)];
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    while let Some((old_raw, new_raw, depth)) = pending.pop() {
        count += 1;
        check(depth <= MAX_DEPTH && count + pending.len() <= MAX_OBJECTS, "index preservation proof exceeds limits")?;
        check(old_raw.as_ref() == new_raw.as_ref(), "existing index root/node identity changed")?;
        if let Some(id) = old_raw.as_ref() {
            check(seen.insert(id), "cyclic/shared index in preservation proof")?;
        }
        let (Object::Dict(old), Object::Dict(new)) = (exact_value(a, &old_raw)?, exact_value(b, &new_raw)?) else {
            return Err("index node changed dictionary type".into());
        };
        let mut x = old.clone();
        let mut y = new.clone();
        for key in [b"Nums".as_slice(), b"Names", b"Kids", b"Limits"] {
            x.remove(key);
            y.remove(key);
        }
        check(
            object_digest(&Object::Dict(x), &mut budget)? == object_digest(&Object::Dict(y), &mut budget)?,
            "index node lost/changed unknown dictionary fields",
        )?;
        let new_subtree = index_subtree(b, &new_raw, names)?;
        let bounds = new_subtree.entries.keys().next().zip(new_subtree.entries.keys().next_back());
        if let Some(raw) = new.get(b"Limits") {
            let Object::Array(values) = raw else { return Err("index Limits is not direct array".into()) };
            check(values.len() == 2, "index Limits arity changed")?;
            let key = |o: &Object| -> ProbeResult<IndexKey> {
                if names {
                    Ok(IndexKey::Name(o.as_string().ok_or_else(|| "ID limits not strings".to_owned())?.bytes.clone()))
                } else {
                    Ok(IndexKey::Number(o.as_int().ok_or_else(|| "ParentTree limits not integers".to_owned())?))
                }
            };
            let pair = values.first().zip(values.get(1)).ok_or_else(|| "index bounds missing".to_owned())?;
            let actual = (key(pair.0)?, key(pair.1)?);
            check(bounds.is_some_and(|(lo, hi)| actual == ((*lo).clone(), (*hi).clone())), "index Limits differ from current subtree bounds")?;
        } else if !new_subtree.entries.is_empty() {
            check(depth == 0 && !old.contains(b"Limits"), "index lost required Limits")?;
        }
        let entry_key = if names { b"Names".as_slice() } else { b"Nums" };
        if let Some(old_values) = old.get(entry_key) {
            check(!new.contains(b"Kids"), "index leaf became a branch")?;
            let new_values = new.get(entry_key).ok_or_else(|| "index leaf entries removed".to_owned())?;
            let (old_values, new_values) = paired_array(a, b, old_values, new_values, &mut arrays)?;
            if !names {
                let key = |pair: &[Object]| {
                    pair.first().and_then(Object::as_int).map(IndexKey::Number).ok_or_else(|| "invalid old index leaf key".to_owned())
                };
                let expected_keys: Vec<_> = old_values
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| key(pair))
                    .collect::<ProbeResult<Vec<_>>>()?
                    .into_iter()
                    .filter(|k| wanted.contains_key(k))
                    .collect();
                let actual_keys: Vec<_> = new_values.as_chunks::<2>().0.iter().map(|pair| key(pair)).collect::<ProbeResult<Vec<_>>>()?;
                check(expected_keys == actual_keys, "ParentTree moved a surviving key to another node")?;
                for old_pair in old_values.as_chunks::<2>().0.iter() {
                    let k = key(old_pair)?;
                    if !wanted.contains_key(&k) {
                        continue;
                    }
                    let old_value = old_pair.get(1).ok_or_else(|| "missing old index value".to_owned())?;
                    if matches!(exact_value(a, old_value)?, Object::Array(_)) {
                        let new_value = new_values
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .find(|pair| key(*pair).ok() == Some(k.clone()))
                            .and_then(|pair| pair.get(1))
                            .ok_or_else(|| "surviving ParentTree key absent".to_owned())?;
                        paired_array(a, b, old_value, new_value, &mut arrays)?;
                    }
                }
            }
        } else if let Some(old_kids) = old.get(b"Kids") {
            let Object::Array(old_children) = exact_value(a, old_kids)? else { return Err("old index Kids is not array".into()) };
            let mut survivors = Vec::new();
            let mut discarded = Vec::new();
            for child in old_children {
                let keys = index_subtree(a, &child, names)?.entries;
                if names || keys.keys().any(|key| wanted.contains_key(key)) {
                    survivors.push(child);
                } else {
                    discarded.push(child);
                }
            }
            for child in discarded {
                if child.as_ref().is_some() {
                    pending.push((child.clone(), child, depth + 1));
                } else {
                    let Object::Dict(mut d) = child else { return Err("discarded direct node is not dictionary".into()) };
                    for key in [b"Nums".as_slice(), b"Kids", b"Limits"] {
                        d.remove(key);
                    }
                    check(d.is_empty(), "pruning would lose unknown data in direct empty index node")?;
                }
            }
            if survivors.is_empty() && !names {
                check(
                    !new.contains(b"Kids") && new.get(b"Nums").and_then(Object::as_array).is_some_and(Vec::is_empty),
                    "empty ParentTree branch did not become empty Nums leaf",
                )?;
                // Its old detached Kids container is not exempted from full comparison.
            } else {
                let new_kids = new.get(b"Kids").ok_or_else(|| "surviving index branch lost Kids".to_owned())?;
                let (_, new_children) = paired_array(a, b, old_kids, new_kids, &mut arrays)?;
                check(survivors.len() == new_children.len(), "index changed surviving child count")?;
                pending.extend(survivors.into_iter().zip(new_children).rev().map(|(x, y)| (x, y, depth + 1)));
            }
        } else {
            return Err("old index node has no entries/children".into());
        }
    }
    Ok(arrays)
}
fn prepare_artifacts(dir: &Path) -> ProbeResult<()> {
    if dir.exists() {
        check(dir.is_dir(), "artifacts path is not a directory")?;
        check(fs::read_dir(dir).map_err(|e| e.to_string())?.next().is_none(), "artifacts directory must be empty")?;
    } else {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_cos::{PdfString, Stream};
    fn fixture() -> Arc<Vec<u8>> {
        let bytes = Session::new().create_blank(120.0, 90.0, 1).unwrap();
        let mut cos = CosDocument::open(bytes).unwrap();
        let page = pdfcraft_model::pages(&cos)[0].obj;
        let document = a11y::tags::create(&mut cos, None, 0, &TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
        let p = a11y::tags::create(&mut cos, Some(document), 0, &TagProperties { role: Some("P".into()), ..Default::default() }).unwrap();
        let span = a11y::tags::create(&mut cos, Some(document), 1, &TagProperties { role: Some("Span".into()), ..Default::default() }).unwrap();
        let content = cos.add(Stream::from_raw(
            Dict::new(),
            b"/P <</MCID 0>> BDC 1 0 0 rg 10 10 20 20 re f EMC\n/Span <</MCID 1>> BDC 0 0 1 rg 60 10 20 20 re f EMC".to_vec(),
        ));
        cos.update_dict(page, |d| {
            d.set(b"Contents", Object::Ref(content));
            d.set(b"StructParents", 0);
        })
        .unwrap();
        let mut private = Dict::new();
        private.set(b"UnknownFlag", Object::Bool(true));
        let private = cos.add(private);
        for (id, mcid) in [(p, 0), (span, 1)] {
            cos.update_dict(id, |d| {
                d.set(b"Pg", Object::Ref(page));
                d.set(b"K", mcid);
                d.set(b"PrivateProbeData", Object::Ref(private));
            })
            .unwrap();
        }
        let root = a11y::tags::inspect(&cos).unwrap().root.unwrap();
        let mut pt = Dict::new();
        pt.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(p), Object::Ref(span)])]));
        cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(pt))).unwrap();
        Arc::new(pdfcraft_cos::write_full(&cos, &Default::default()).unwrap())
    }
    fn untagged_fixture(mode: &str) -> Arc<Vec<u8>> {
        let bytes = Session::new().create_blank(120.0, 90.0, 1).unwrap();
        let mut cos = CosDocument::open(bytes).unwrap();
        let catalog = cos.root().unwrap();
        let page = pdfcraft_model::pages(&cos)[0].obj;
        let stream = cos.add(Stream::from_raw(Dict::new(), b"0.2 0.4 0.6 rg 10 15 40 20 re f".to_vec()));
        cos.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
        let mut private = Dict::new();
        private.set(b"Untouched", Object::Bool(true));
        let private = cos.add(private);
        let mut mark = Dict::new();
        mark.set(b"Marked", Object::Bool(false));
        mark.set(b"Suspects", Object::Bool(true));
        mark.set(b"PrivateKeep", Object::Ref(private));
        let value = match mode {
            "absent" => None,
            "direct" => Some(Object::Dict(mark)),
            "indirect" | "stale" => Some(Object::Ref(cos.add(mark))),
            "stream" => Some(Object::Ref(cos.add(Stream::from_raw(mark, b"untouched".to_vec())))),
            "malformed" => Some(Object::Bool(false)),
            _ => panic!("fixture mode"),
        };
        cos.update_dict(catalog, |d| {
            d.remove(b"StructTreeRoot");
            d.remove(b"MarkInfo");
            d.set(b"PrivateCatalog", Object::Ref(private));
            if let Some(v) = value {
                d.set(b"MarkInfo", v);
            }
        })
        .unwrap();
        let bytes = Arc::new(pdfcraft_cos::write_full(&cos, &Default::default()).unwrap());
        if mode != "stale" {
            return bytes;
        }
        let mut cos = CosDocument::open(bytes).unwrap();
        let catalog = cos.root().unwrap();
        let id = cos.get(catalog).as_dict().unwrap().reference(b"MarkInfo").unwrap();
        cos.update_dict(catalog, |d| d.set(b"MarkInfo", Object::Ref(ObjRef::new(id.num, id.generation + 1)))).unwrap();
        Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap())
    }
    fn chosen(tree: &TagTree, name: &str) -> Plan {
        plans(tree).into_iter().find(|(n, _, _)| *n == name).unwrap().1.unwrap()
    }
    fn run_named(bytes: Arc<Vec<u8>>, name: &str) -> ProbeResult<Value> {
        let (s, id) = open_copy(bytes.clone())?;
        let tree = tags(&s, id)?;
        let render = render(doc(&s, id)?)?;
        run_edit(bytes, &tree, &render, chosen(&tree, name), None)
    }
    #[test]
    fn sha256_hex_is_complete() {
        assert_eq!(digest(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
    #[test]
    fn all_applicable_operations_prove_effect_undo_redo_save_and_artwork() {
        let bytes = fixture();
        let (s, id) = open_copy(bytes.clone()).unwrap();
        let tree = tags(&s, id).unwrap();
        let pixels = render(doc(&s, id).unwrap()).unwrap();
        let operations = plans(&tree);
        assert_eq!(operations.len(), 14);
        for (name, plan, _) in operations {
            if let Some(plan) = plan {
                let report = run_edit(bytes.clone(), &tree, &pixels, plan, None).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(report["status"], "passed", "{name}: {report}");
                assert_eq!(report["observed_edit"], true);
                for invariant in ["undo", "redo", "save_reopen", "exact_ownership", "all_page_artwork", "unknown_data_save_reopen"] {
                    assert_eq!(report[invariant], "passed");
                }
            } else {
                assert!(name == "create_root" || name.starts_with("leaf_move_"));
            }
        }
    }
    #[test]
    fn outcome_does_not_count_refusal_or_inapplicability_as_pass() {
        let mut r = json!({"edits":[{"status":"refused"},{"status":"not_applicable"}],"stages":{}});
        assert!(!summarize(&mut r));
        assert_eq!(r["outcome"], "incomplete");
        assert_eq!(r["operation_status_counts"]["refused"], 1);
        assert!(r["operation_status_counts"]["passed"].is_null());
        r["edits"] = json!([{"status":"error"}]);
        assert!(summarize(&mut r));
    }
    #[test]
    fn malformed_parent_tree_refusal_is_atomically_unchanged() {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let root = a11y::tags::inspect(&cos).unwrap().root.unwrap();
        let mut pt = Dict::new();
        pt.set(b"Nums", Object::Array(vec![0.into()]));
        cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(pt))).unwrap();
        let bytes = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let report = run_named(bytes, "reorder").unwrap();
        assert_eq!(report["status"], "refused");
        assert_eq!(report["reason_code"], "structure_edit_rejected");
        assert_eq!(report["unchanged_on_failure"], true);
    }
    #[test]
    fn harmless_metadata_preserves_existing_orphans_and_invalid_checker_findings() {
        let bytes = empty_preserved_orphans_fixture(false);
        let mut cos = CosDocument::open(bytes).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        a11y::tags::create(&mut cos, None, 0, &TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
        let bytes = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        for name in ["title_property", "metadata_properties"] {
            let report = run_named(bytes.clone(), name).unwrap();
            assert_eq!(report["status"], "passed");
        }
        assert!(!tree.issues.is_empty());
    }
    #[test]
    fn artifact_proof_rejects_plain_delete_substitution() {
        let bytes = fixture();
        let (s, id) = open_copy(bytes.clone()).unwrap();
        let tree = tags(&s, id).unwrap();
        let mut plan = chosen(&tree, "artifact");
        let Expected::Remove { id: target, .. } = plan.expected else { panic!("expected artifact removal") };
        plan.edit = Edit::DeleteTag { id: target, mode: DeleteMode::Subtree };
        assert!(run_edit(bytes, &tree, &render(doc(&s, id).unwrap()).unwrap(), plan, None).unwrap_err().contains("Artifact"));
    }
    #[test]
    fn raw_graph_rejects_unknown_property_and_reference_generation_changes() {
        let bytes = fixture();
        let original = graph(bytes.clone(), PropertyMask::None).unwrap();
        let mut cos = CosDocument::open(bytes).unwrap();
        let tag = a11y::tags::inspect(&cos).unwrap().elements[1].id;
        cos.update_dict(tag, |d| d.set(b"PrivateProbeData", Object::Ref(ObjRef::new(999, 7)))).unwrap();
        let changed = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert!(compare_graph(&original, &graph(changed, PropertyMask::None).unwrap()).is_err());
    }
    #[test]
    fn create_root_preserves_absent_direct_indirect_mark_info() {
        for mode in ["absent", "direct", "indirect"] {
            let bytes = untagged_fixture(mode);
            assert!(root_absence(bytes.clone()).unwrap().1);
            let report = run_named(bytes, "create_root").unwrap();
            assert_eq!(report["status"], "passed", "{mode}: {report}");
            assert_eq!(report["root_creation_evidence"]["marked_after"], true);
            assert_eq!(report["root_creation_evidence"]["content_associations_created"], 0);
        }
    }
    #[test]
    fn create_root_preserves_invalid_mark_info_on_atomic_refusal() {
        for mode in ["malformed", "stream", "stale"] {
            let report = run_named(untagged_fixture(mode), "create_root").unwrap();
            assert_eq!(report["status"], "refused", "{mode}: {report}");
            assert_eq!(report["unchanged_on_failure"], true);
        }
    }
    #[test]
    fn create_root_does_not_replace_null_or_malformed_existing_root_values() {
        let bytes = untagged_fixture("direct");
        let (s, id) = open_copy(bytes.clone()).unwrap();
        let tree = tags(&s, id).unwrap();
        for value in [Object::Null, Object::Bool(false), Object::Int(7)] {
            let mut cos = CosDocument::open(bytes.clone()).unwrap();
            let catalog = cos.root().unwrap();
            cos.update_dict(catalog, |d| d.set(b"StructTreeRoot", value)).unwrap();
            let bad = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
            assert!(!root_absence(bad.clone()).unwrap().1);
            assert!(root_creation_mask(bad, &tree).is_err());
        }
    }
    #[test]
    fn root_proof_rejects_wrong_role_or_unknown_mark_info_loss() {
        let bytes = untagged_fixture("direct");
        let (s, id) = open_copy(bytes.clone()).unwrap();
        let before = doc(&s, id).unwrap().bytes.clone();
        let mut cos = CosDocument::open(before.clone()).unwrap();
        let tag = a11y::tags::create(&mut cos, None, 0, &TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
        let good = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let tree = a11y::tags::inspect(&cos).unwrap();
        assert!(created_root_proof(before.clone(), good, &tree).is_ok());
        cos.update_dict(tag, |d| d.set(b"S", Object::name("P"))).unwrap();
        let changed = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert!(created_root_proof(before, changed, &tree).is_err());
    }
    fn empty_preserved_orphans_fixture(indirect: bool) -> Arc<Vec<u8>> {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let root = tree.root.unwrap();
        let children = if indirect { Object::Ref(cos.add(Object::Array(Vec::new()))) } else { Object::Array(Vec::new()) };
        let mut parent_tree = cos.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().clone();
        if indirect {
            let mut d = parent_tree.as_dict().unwrap().clone();
            let nums = d.get(b"Nums").unwrap().clone();
            let nums = cos.add(nums);
            d.set(b"Nums", Object::Ref(nums));
            let leaf = cos.add(d);
            let mut branch = Dict::new();
            branch.set(b"Kids", Object::Array(vec![Object::Ref(leaf)]));
            parent_tree = Object::Ref(cos.add(branch));
        }
        cos.update_dict(root, |d| {
            d.set(b"K", children);
            d.set(b"PrivateRoot", Object::String(pdfcraft_cos::PdfString::text("preserve empty root")));
            d.set(b"ParentTree", parent_tree);
        })
        .unwrap();
        Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap())
    }

    #[test]
    fn create_first_tag_preserves_explicit_orphan_slots_owner_graph_and_warnings() {
        for indirect in [false, true] {
            let bytes = empty_preserved_orphans_fixture(indirect);
            let (s, id) = open_copy(bytes.clone()).unwrap();
            let tree = tags(&s, id).unwrap();
            assert!(tree.elements.is_empty() && tree.children.is_empty() && tree.edit_blockers.is_empty());
            assert_eq!(tree.preserved_associations.len(), 2);
            assert_eq!(tree.issues.len(), 2);
            let pixels = render(doc(&s, id).unwrap()).unwrap();
            let plan = plans(&tree).into_iter().find(|(name, _, _)| *name == "create_root").unwrap().1.unwrap();
            let report = run_edit(bytes, &tree, &pixels, plan, None).unwrap();
            assert_eq!(report["status"], "passed", "{report}");
            let proof = &report["root_creation_evidence"];
            assert_eq!(proof["mode"], "create_first_tag_existing_root");
            assert_eq!(proof["struct_tree_root_ref"], reference(tree.root.unwrap()));
            assert_eq!(proof["content_associations_created"], 0);
            let proof = &proof["preserved_associations"];
            assert_eq!(proof["status"], "passed");
            assert_eq!(proof["count"], 2);
            assert_eq!(proof["warnings_exactly_classified"], true);
            assert_eq!(proof["edit_blockers"], 0);
            for field in ["index_slots", "index_graph", "owner_graph"] {
                assert_eq!(proof[format!("{field}_sha256_before")], proof[format!("{field}_sha256_after")]);
            }
            assert!(proof["owner_objects_checked"].as_u64().unwrap() >= 2);
            assert!(proof["index_objects_checked"].as_u64().unwrap() >= 1);
            assert_eq!(proof["verified_snapshots"], json!(["after_edit", "after_redo", "after_save_reopen"]));
            for invariant in ["undo", "redo", "save_reopen", "all_page_artwork", "unknown_data_create_root", "unknown_data_save_reopen"] {
                assert_eq!(report[invariant], "passed");
            }
        }
    }

    #[test]
    fn empty_root_creation_rejects_unclassified_warnings_and_blocking_or_ambiguous_associations() {
        let bytes = empty_preserved_orphans_fixture(false);
        let (s, id) = open_copy(bytes.clone()).unwrap();
        let original = tags(&s, id).unwrap();
        for case in ["no_classification", "extra_warning", "blocker", "reachable_kind", "duplicate_slot"] {
            let mut tree = original.clone();
            match case {
                "no_classification" => tree.preserved_associations.clear(),
                "extra_warning" => tree.issues.push("some unrelated warning".into()),
                "blocker" => tree.edit_blockers.push("ambiguous owner graph".into()),
                "reachable_kind" => tree.preserved_associations.first_mut().unwrap().kind = ParentAssociationKind::UnclaimedScalar,
                "duplicate_slot" => {
                    tree.preserved_associations.push(tree.preserved_associations.first().unwrap().clone());
                    tree.issues.push(tree.issues.first().unwrap().clone());
                }
                _ => unreachable!(),
            }
            assert!(!empty_root_creation_allowed(&tree), "{case}");
            assert!(plans(&tree).into_iter().find(|(name, _, _)| *name == "create_root").unwrap().1.is_none(), "{case}");
            assert!(root_creation_mask(bytes.clone(), &tree).is_err(), "{case}");
        }
        let mut cos = CosDocument::open(bytes).unwrap();
        let owner = original.preserved_associations.first().unwrap().owner;
        cos.update_dict(owner, |d| d.set(b"Type", Object::name("Annot"))).unwrap();
        let malformed = a11y::tags::inspect(&cos).unwrap();
        assert!(!malformed.edit_blockers.is_empty());
        assert!(!empty_root_creation_allowed(&malformed));
        let before = pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap();
        assert!(a11y::tags::create(&mut cos, None, 0, &TagProperties { role: Some("Document".into()), ..Default::default() }).is_err());
        assert_eq!(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap(), before);
    }

    #[test]
    fn preserved_empty_root_proof_rejects_aliases_stale_references_and_changed_slots_or_owner_data() {
        let bytes = empty_preserved_orphans_fixture(true);
        let cos = CosDocument::open(bytes.clone()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let root = tree.root.unwrap();
        let owner = tree.preserved_associations.first().unwrap().owner;
        let child_array = cos.get(root).as_dict().unwrap().reference(b"K").unwrap();
        for case in ["root_child", "shared_array", "root_owner", "stale_child", "stale_owner", "duplicate_key"] {
            let mut cos = CosDocument::open(bytes.clone()).unwrap();
            match case {
                "root_child" => cos.update_dict(owner, |d| d.set(b"K", Object::Ref(root))).unwrap(),
                "shared_array" => cos.update_dict(owner, |d| d.set(b"K", Object::Ref(child_array))).unwrap(),
                "stale_child" => {
                    cos.update_dict(owner, |d| d.set(b"K", Object::Ref(ObjRef::new(child_array.num, child_array.generation + 1)))).unwrap()
                }
                "root_owner" | "stale_owner" | "duplicate_key" => {
                    let replacement = match case {
                        "root_owner" => root,
                        "stale_owner" => ObjRef::new(owner.num, owner.generation + 1),
                        _ => owner,
                    };
                    let mut nums = vec![Object::Int(0), Object::Array(vec![Object::Ref(replacement)])];
                    if case == "duplicate_key" {
                        nums.extend([Object::Int(0), Object::Array(vec![Object::Ref(owner)])]);
                    }
                    let mut pt = Dict::new();
                    pt.set(b"Nums", Object::Array(nums));
                    cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(pt))).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(preserved_root_snapshot(&cos, &tree).is_err(), "{case}");
        }
        for field in [b"PrivatePreservation".as_slice(), b"K"] {
            let mut candidate = CosDocument::open(bytes.clone()).unwrap();
            a11y::tags::create(&mut candidate, None, 0, &TagProperties { role: Some("Document".into()), ..Default::default() }).unwrap();
            candidate.update_dict(owner, |d| d.set(field, Object::Int(999))).unwrap();
            let after_tree = a11y::tags::inspect(&candidate).unwrap();
            assert!(preserved_creation_proof(&cos, &candidate, &tree, &after_tree).is_err());
        }
    }

    #[test]
    fn structural_proof_rejects_removed_unrelated_k_and_index_unknown_identity_corruption() {
        for case in ["removed_k", "unrelated_array", "direct_index_private", "index_root_identity"] {
            let mut a = CosDocument::open(fixture()).unwrap();
            let tree = a11y::tags::inspect(&a).unwrap();
            let root = tree.root.unwrap();
            let p = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
            let span = tree.elements.iter().find(|e| e.role == "Span").unwrap().id;
            let array = a.add(Object::Array(vec![Object::Int(1)]));
            a.update_dict(span, |d| d.set(b"K", Object::Ref(array))).unwrap();
            let mut pt = a.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
            pt.set(b"PrivateIndex", Object::String(PdfString::text("retain")));
            let pt = if case == "index_root_identity" { Object::Ref(a.add(pt)) } else { Object::Dict(pt) };
            a.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
            let before = Arc::new(pdfcraft_cos::write_incremental(&a, &Default::default()).unwrap());
            let tree = a11y::tags::inspect(&a).unwrap();
            let mut b = CosDocument::open(before.clone()).unwrap();
            a11y::tags::delete(&mut b, p, DeleteMode::Subtree).unwrap();
            let good_tree = a11y::tags::inspect(&b).unwrap();
            match case {
                "removed_k" => b.update_dict(p, |d| d.set(b"K", Object::Int(999))).unwrap(),
                "unrelated_array" => b.set(array, Object::Array(vec![Object::Int(999)])),
                "direct_index_private" => {
                    let mut d = b.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
                    d.remove(b"PrivateIndex");
                    b.update_dict(root, |r| r.set(b"ParentTree", Object::Dict(d))).unwrap();
                }
                "index_root_identity" => {
                    let old = b.get(root).as_dict().unwrap().reference(b"ParentTree").unwrap();
                    let new = b.add((*b.get(old)).clone());
                    b.update_dict(root, |d| d.set(b"ParentTree", Object::Ref(new))).unwrap();
                }
                _ => unreachable!(),
            }
            let after = Arc::new(pdfcraft_cos::write_incremental(&b, &Default::default()).unwrap());
            assert!(raw_structural_proof(before, after, &tree, &good_tree, &Expected::Remove { id: p, unwrap: false }, false).is_err(), "{case}");
        }
    }
    fn explicit_leaf_fixture(kind: &str) -> Arc<Vec<u8>> {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let root = tree.root.unwrap();
        let page = tree.pages[0];
        let source = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        let mut leaf = Dict::new();
        leaf.set(b"Pg", Object::Ref(page));
        let (content, nums) = match kind {
            "page_mcr" => {
                leaf.set(b"Type", Object::name("MCR"));
                leaf.set(b"MCID", 0);
                (None, None)
            }
            "form_mcr" => {
                let mut d = Dict::new();
                d.set(b"Type", Object::name("XObject"));
                d.set(b"Subtype", Object::name("Form"));
                d.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 120.into(), 90.into()]));
                d.set(b"Resources", Object::Dict(Dict::new()));
                d.set(b"StructParents", 1);
                let plain = cos.add(Stream::from_raw(d, b"/P <</MCID 0>> BDC 1 0 0 rg 10 10 20 20 re f EMC".to_vec()));
                let form = ObjRef::new(plain.num, 7);
                cos.set(form, (*cos.get(plain)).clone());
                let mut resources = Dict::new();
                let mut xobjects = Dict::new();
                xobjects.set(b"Fm", Object::Ref(form));
                resources.set(b"XObject", Object::Dict(xobjects));
                cos.update_dict(page, |d| d.set(b"Resources", Object::Dict(resources))).unwrap();
                leaf.set(b"Type", Object::name("MCR"));
                leaf.set(b"MCID", 0);
                leaf.set(b"Stm", Object::Ref(form));
                (Some(b"/Fm Do\n/Span <</MCID 1>> BDC 0 0 1 rg 60 10 20 20 re f EMC".to_vec()), Some(Object::Array(vec![Object::Ref(source)])))
            }
            "objr" => {
                let mut d = Dict::new();
                d.set(b"Type", Object::name("Annot"));
                d.set(b"Subtype", Object::name("Link"));
                d.set(b"Rect", Object::Array(vec![10.into(), 10.into(), 30.into(), 30.into()]));
                d.set(b"StructParent", 1);
                d.set(b"P", Object::Ref(page));
                let plain = cos.add(d);
                let object = ObjRef::new(plain.num, 3);
                cos.set(object, (*cos.get(plain)).clone());
                cos.update_dict(page, |d| d.set(b"Annots", Object::Array(vec![Object::Ref(object)]))).unwrap();
                leaf.set(b"Type", Object::name("OBJR"));
                leaf.set(b"Obj", Object::Ref(object));
                (
                    Some(b"/Artifact BMC 1 0 0 rg 10 10 20 20 re f EMC\n/Span <</MCID 1>> BDC 0 0 1 rg 60 10 20 20 re f EMC".to_vec()),
                    Some(Object::Ref(source)),
                )
            }
            _ => panic!("typed fixture kind"),
        };
        leaf.set(b"PrivateLeaf", Object::String(PdfString::text("retain leaf properties")));
        let leaf = cos.add(leaf);
        let mut attrs = Dict::new();
        attrs.set(b"O", Object::name("Layout"));
        attrs.set(b"Placement", Object::name("Block"));
        let attrs = cos.add(attrs);
        cos.update_dict(source, |d| {
            d.remove(b"Pg");
            d.set(b"K", Object::Ref(leaf));
            d.set(b"A", Object::Ref(attrs));
        })
        .unwrap();
        if let Some(content) = content {
            let stream = cos.add(Stream::from_raw(Dict::new(), content));
            cos.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
        }
        if let Some(extra) = nums {
            let span = tree.elements.iter().find(|e| e.role == "Span").unwrap().id;
            let mut pt = Dict::new();
            pt.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Null, Object::Ref(span)]), 1.into(), extra]));
            cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(pt))).unwrap();
        }
        Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap())
    }
    #[test]
    fn typed_leaf_moves_and_explicit_page_reparent_preserve_all_identities() {
        for kind in ["page_mcr", "form_mcr", "objr"] {
            let bytes = explicit_leaf_fixture(kind);
            let report = run_named(bytes.clone(), &format!("leaf_move_{kind}")).unwrap();
            assert_eq!(report["status"], "passed", "{kind}: {report}");
            assert_eq!(report["leaf_move_coverage"]["selected_kind"], kind);
            let report = run_named(bytes, "reparent").unwrap();
            assert_eq!(report["status"], "passed", "{kind}: {report}");
            assert_eq!(report["inherited_container_page_change"]["status"], "passed");
            assert_eq!(report["inherited_container_page_change"]["explicit_content_leaves"], 1);
        }
    }
    fn unchecked_reparent(cos: &mut CosDocument, tree: &TagTree) -> Expected {
        let source = tree.elements.iter().find(|e| e.role == "P").unwrap();
        let destination = tree.elements.iter().find(|e| e.role == "Span").unwrap();
        let old_parent = source.parent.unwrap();
        let mut kids = raw_kids(cos, &exact_dictionary(cos, old_parent).unwrap()).unwrap();
        kids.retain(|k| k.as_ref() != Some(source.id));
        cos.update_dict(old_parent, |d| d.set(b"K", Object::Array(kids))).unwrap();
        let mut kids = raw_kids(cos, &exact_dictionary(cos, destination.id).unwrap()).unwrap();
        kids.insert(0, Object::Ref(source.id));
        cos.update_dict(destination.id, |d| d.set(b"K", Object::Array(kids))).unwrap();
        cos.update_dict(source.id, |d| d.set(b"P", Object::Ref(destination.id))).unwrap();
        Expected::Move { id: source.id, parent: Some(destination.id), index: 0 }
    }
    #[test]
    fn explicit_page_proof_rejects_implicit_integer_nested_attribute_and_stale_cases() {
        for case in ["implicit", "integer", "empty", "nested", "bbox", "class", "attributes", "attribute_stream", "stale_attribute", "stale_page"] {
            let mut a = CosDocument::open(explicit_leaf_fixture("page_mcr")).unwrap();
            let tree = a11y::tags::inspect(&a).unwrap();
            let source = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
            let d = exact_dictionary(&a, source).unwrap();
            let leaf = d.reference(b"K").unwrap();
            match case {
                "implicit" => a
                    .update_dict(leaf, |d| {
                        d.remove(b"Pg");
                    })
                    .unwrap(),
                "integer" => a.update_dict(source, |d| d.set(b"K", 0)).unwrap(),
                "empty" => a
                    .update_dict(source, |d| {
                        d.remove(b"K");
                    })
                    .unwrap(),
                "nested" => {
                    let mut d = Dict::new();
                    d.set(b"Type", Object::name("StructElem"));
                    d.set(b"S", Object::name("Span"));
                    d.set(b"P", Object::Ref(source));
                    let child = a.add(d);
                    a.update_dict(source, |d| d.set(b"K", Object::Ref(child))).unwrap();
                }
                "bbox" => a.update_dict(source, |d| d.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]))).unwrap(),
                "class" => a.update_dict(source, |d| d.set(b"C", Object::name("PrivateClass"))).unwrap(),
                "attributes" => {
                    let attr = d.reference(b"A").unwrap();
                    a.update_dict(attr, |d| d.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]))).unwrap();
                }
                "attribute_stream" => {
                    let attr = d.reference(b"A").unwrap();
                    let dict = exact_dictionary(&a, attr).unwrap();
                    a.set(attr, Stream::from_raw(dict, Vec::new()));
                }
                "stale_attribute" => {
                    let attr = d.reference(b"A").unwrap();
                    a.update_dict(source, |d| d.set(b"A", Object::Ref(ObjRef::new(attr.num, attr.generation + 1)))).unwrap();
                }
                "stale_page" => {
                    let pg = tree.pages[0];
                    a.update_dict(leaf, |d| d.set(b"Pg", Object::Ref(ObjRef::new(pg.num, pg.generation + 1)))).unwrap();
                }
                _ => unreachable!(),
            }
            let before = Arc::new(pdfcraft_cos::write_incremental(&a, &Default::default()).unwrap());
            let before_tree = a11y::tags::inspect(&a).unwrap();
            let mut b = CosDocument::open(before.clone()).unwrap();
            let expected = unchecked_reparent(&mut b, &before_tree);
            let after_tree = a11y::tags::inspect(&b).unwrap();
            let after = Arc::new(pdfcraft_cos::write_incremental(&b, &Default::default()).unwrap());
            assert!(prove_move_page_change(before, after, &before_tree, &after_tree, &expected).is_err(), "{case}");
        }
    }
    #[test]
    fn artifact_proof_rejects_selected_page_resources_corruption() {
        let bytes = fixture();
        let mut cos = CosDocument::open(bytes.clone()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let target = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        a11y::artifact::convert(&mut cos, target, false).unwrap();
        cos.update_dict(tree.pages[0], |d| {
            let mut resources = Dict::new();
            resources.set(b"PrivateCorruption", 1);
            d.set(b"Resources", Object::Dict(resources));
        })
        .unwrap();
        let after = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert!(artifact_proof(bytes, after, &tree, target).is_err());
    }
    #[test]
    fn caller_created_empty_artifact_directory_is_supported_but_nonempty_is_rejected() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-probe-artifact-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(prepare_artifacts(&dir).is_ok());
        fs::write(dir.join("existing"), b"must not replace").unwrap();
        assert!(prepare_artifacts(&dir).is_err());
        fs::remove_file(dir.join("existing")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn unrelated_integer_leaf_reference_cannot_be_replaced_under_touched_parent() {
        let mut a = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&a).unwrap();
        let p = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        let span = tree.elements.iter().find(|e| e.role == "Span").unwrap().id;
        let integer = a.add(Object::Int(0));
        a.update_dict(p, |d| d.set(b"K", Object::Ref(integer))).unwrap();
        let before = Arc::new(pdfcraft_cos::write_incremental(&a, &Default::default()).unwrap());
        let tree = a11y::tags::inspect(&a).unwrap();
        let mut b = CosDocument::open(before.clone()).unwrap();
        a11y::tags::move_content(&mut b, span, 0, p, 1).unwrap();
        let after_tree = a11y::tags::inspect(&b).unwrap();
        let mut kids = raw_kids(&b, &exact_dictionary(&b, p).unwrap()).unwrap();
        *kids.first_mut().unwrap() = Object::Int(0);
        b.update_dict(p, |d| d.set(b"K", Object::Array(kids))).unwrap();
        let after = Arc::new(pdfcraft_cos::write_incremental(&b, &Default::default()).unwrap());
        assert!(
            raw_structural_proof(
                before,
                after,
                &tree,
                &after_tree,
                &Expected::Leaf { source: span, source_index: 0, destination: p, index: 1 },
                false
            )
            .unwrap_err()
            .contains("integer MCID raw identity")
        );
    }
    #[test]
    fn identifier_proof_rejects_same_text_with_mismatched_raw_id_bytes() {
        let before = fixture();
        let mut cos = CosDocument::open(before.clone()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let id = tree.elements[0].id;
        let value = "ASCII-Identifier".to_owned();
        a11y::tags::set_identifier(&mut cos, id, Some(&value)).unwrap();
        let after_tree = a11y::tags::inspect(&cos).unwrap();
        let mut alternate = vec![0xfe, 0xff];
        for c in value.encode_utf16() {
            alternate.extend_from_slice(&c.to_be_bytes());
        }
        cos.update_dict(id, |d| d.set(b"ID", Object::String(PdfString::literal(alternate)))).unwrap();
        assert_eq!(a11y::tags::inspect(&cos).unwrap().element(id).unwrap().identifier.as_deref(), Some(value.as_str()));
        let after = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert!(
            raw_structural_proof(before, after, &tree, &after_tree, &Expected::Identifier { id, value }, false)
                .unwrap_err()
                .contains("raw tag ID bytes")
        );
    }
    #[test]
    fn deleted_objr_proof_rejects_dangling_struct_parent_backlink() {
        let before = explicit_leaf_fixture("objr");
        let mut cos = CosDocument::open(before.clone()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let target = tree.elements.iter().find(|e| e.role == "P").unwrap();
        let id = target.id;
        let TagKid::Objr { object, .. } = target.kids[0] else { panic!("OBJR fixture") };
        a11y::tags::delete(&mut cos, id, DeleteMode::Subtree).unwrap();
        let after_tree = a11y::tags::inspect(&cos).unwrap();
        cos.update_dict(object, |d| d.set(b"StructParent", 1)).unwrap();
        let after = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert!(
            raw_structural_proof(before, after, &tree, &after_tree, &Expected::Remove { id, unwrap: false }, false)
                .unwrap_err()
                .contains("StructParent backlink")
        );
    }
    #[test]
    fn figure_uncertainty_is_explicit_refusal_with_exact_checker_binding() {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let figure = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        cos.update_dict(figure, |d| d.set(b"S", Object::Name(b"Figure".to_vec()))).unwrap();
        let clean = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let (s, id) = open_copy(clean).unwrap();
        let document = doc(&s, id).unwrap();
        assert_eq!(figures(document, &tags(&s, id).unwrap(), &checker(document).unwrap()).unwrap()["status"], "passed");
        let root = tree.root.unwrap();
        let mut mapping = Dict::new();
        mapping.set(b"Cycle", Object::Name(b"Cycle".to_vec()));
        cos.update_dict(root, |d| d.set(b"RoleMap", Object::Dict(mapping))).unwrap();
        let invalid = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let (s, id) = open_copy(invalid).unwrap();
        let document = doc(&s, id).unwrap();
        let tree = tags(&s, id).unwrap();
        let checked = checker(document).unwrap();
        let value = figures(document, &tree, &checked).unwrap();
        assert_eq!(value["status"], "refused");
        assert_eq!(value["reason_code"], "structure_resolution_incomplete");
        assert_eq!(value["diagnostic"]["legacy_empty_fallback_verified"], true);
        let expected = BTreeSet::from([figure]);
        let error = document.try_figures().unwrap_err();
        let evidence = figure_diagnostic(&error, &checked, &tree, &expected).unwrap();
        assert_eq!(evidence["tag_tree_known_figure_count"], 1);
        assert!(figure_diagnostic(&EditError::Accessibility("panic or arbitrary error".into()), &checked, &tree, &expected).is_err());
        assert!(figure_diagnostic(&EditError::ReadOnly("blocked".into()), &checked, &tree, &expected).is_err());
        let mut mismatch = checked.clone();
        for rule in mismatch["rules"].as_array_mut().unwrap() {
            if rule["rule"] == "FiguresAltText" {
                rule["findings"] = json!([]);
            }
        }
        assert!(figure_diagnostic(&error, &mismatch, &tree, &expected).is_err());
        for status in ["passed", "skipped"] {
            let mut mismatch = checked.clone();
            for rule in mismatch["rules"].as_array_mut().unwrap() {
                if rule["rule"] == "FiguresAltText" {
                    rule["status"] = json!(status);
                }
            }
            assert!(figure_diagnostic(&error, &mismatch, &tree, &expected).is_err());
        }
        let mut report = json!({"stages":{"figures_api_parity":value},"edits":[]});
        assert!(!summarize(&mut report));
        assert_eq!(report["outcome"], "incomplete");
        assert!(report["active_stage"].is_null());
    }
    #[test]
    fn geometry_discloses_unowned_bounds_and_joins_exact_namespaces() {
        let bytes = fixture();
        let (mut s, id) = open_copy(bytes).unwrap();
        let tree = tags(&s, id).unwrap();
        let target = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        s.apply(id, Edit::DeleteTag { id: target, mode: DeleteMode::Subtree }).unwrap();
        let tree = tags(&s, id).unwrap();
        let coverage = geometry(doc(&s, id).unwrap(), &tree).unwrap();
        assert_eq!(coverage["geometry_identities_without_owner"], 1);
        assert_eq!(coverage["pages"][0]["geometry_identities_without_owner"], 1);
        assert_eq!(coverage["unowned_geometry"]["items"][0]["mcid"], 0);
        assert_eq!(coverage["unowned_geometry"]["omitted"], 0);
        let page = tree.pages[0];
        let known = ContentKey::Mcid { page: Some(page), stream: None, mcid: 1 };
        let mut regions = BTreeMap::new();
        regions.insert(known.clone(), vec![[0., 0., 1., 1.]]);
        regions.insert(ContentKey::Mcid { page: Some(ObjRef::new(page.num, page.generation + 1)), stream: None, mcid: 1 }, vec![[0., 0., 1., 1.]]);
        regions.insert(ContentKey::Mcid { page: Some(page), stream: Some(ObjRef::new(999, 3)), mcid: 1 }, vec![[0., 0., 1., 1.]]);
        regions.insert(ContentKey::Objr { page: Some(page), object: ObjRef::new(999, 4) }, vec![[0., 0., 1., 1.]]);
        regions.insert(ContentKey::Mcid { page: Some(page), stream: None, mcid: 42 }, Vec::new());
        let items = unowned_geometry(&tree, &ownership(&tree), &regions);
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|v| v["owner_refs"] == json!([])));
        let many = vec![json!({"identity":"bounded"}); MAX_DIAGNOSTICS + 2];
        assert_eq!(bounded_geometry_examples(&many)["omitted"], 2);
    }
    #[test]
    fn delete_targets_first_non_root_subtree_and_preserves_protected_scalar_refusal() {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let parent = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        let child = tree.elements.iter().find(|e| e.role == "Span").unwrap().id;
        a11y::tags::move_tag(&mut cos, child, Some(parent), 1).unwrap();
        let nested = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let tree = a11y::tags::inspect(&cos).unwrap();
        assert!(matches!(chosen(&tree, "delete").expected, Expected::Remove { id, unwrap: false } if id == parent));
        assert!(matches!(chosen(&tree, "unwrap").expected, Expected::Remove { id, unwrap: true } if id == parent));
        assert!(matches!(chosen(&tree, "artifact").expected, Expected::Remove { id, unwrap: false } if id == child));
        assert_eq!(run_named(nested, "delete").unwrap()["status"], "passed");
        let root = tree.root.unwrap();
        let mut index = exact_dictionary(&cos, root).unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
        let mut nums = index.get(b"Nums").unwrap().as_array().unwrap().to_vec();
        nums.extend([Object::Int(1), Object::Ref(child)]);
        index.set(b"Nums", Object::Array(nums));
        cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(index))).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        assert!(tree.edit_blockers.is_empty(), "{tree:?}");
        assert!(tree.preserved_associations.iter().any(|a| a.kind == ParentAssociationKind::UnclaimedScalar && a.owner == child));
        let protected = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        let report = run_named(protected, "delete").unwrap();
        assert_eq!(report["status"], "refused");
        assert_eq!(report["reason_code"], "structure_edit_rejected");
        assert_eq!(report["reason"], "edit would remove a protected owner of a globally unclaimed ParentTree scalar");
        assert_eq!(report["unchanged_on_failure"], true);
    }
    #[test]
    fn form_dictionary_proof_accepts_numeric_normalization_but_rejects_actual_changes() {
        let mut original = Dict::new();
        original.set(b"BBox", Object::Array(vec![Object::Real(0.0), Object::Real(0.0), Object::Int(120), Object::Int(90)]));
        original.set(b"Matrix", Object::Array(vec![1.into(), Object::Real(0.0), Object::Real(0.0), 1.into(), 0.into(), 0.into()]));
        original.set(b"PrivateKeep", Object::Bool(true));
        let mut normalized = original.clone();
        for key in [b"BBox".as_slice(), b"Matrix"] {
            let mut values = normalized.get(key).unwrap().as_array().unwrap().to_vec();
            for value in &mut values {
                if matches!(value, Object::Real(v) if *v == 0.0) {
                    *value = Object::Int(0);
                }
            }
            normalized.set(key, Object::Array(values));
        }
        assert_ne!(original, normalized);
        let mut budget = MAX_GRAPH_NODES;
        assert!(same_form_dictionary(&original, &normalized, &mut budget).is_ok());
        for key in [b"BBox".as_slice(), b"Matrix", b"PrivateKeep"] {
            let mut bad = normalized.clone();
            if key == b"PrivateKeep" {
                bad.set(key, Object::Bool(false));
            } else {
                let mut values = bad.get(key).unwrap().as_array().unwrap().to_vec();
                values[0] = Object::Int(9);
                bad.set(key, Object::Array(values));
            }
            let mut budget = MAX_GRAPH_NODES;
            assert!(same_form_dictionary(&original, &bad, &mut budget).is_err(), "{key:?}");
        }
        assert!(same_form_dictionary(&original, &normalized, &mut 1).is_err());
    }
    fn nested_page_binding_fixture() -> (Arc<Vec<u8>>, ObjRef, ObjRef, ObjRef) {
        let mut cos = CosDocument::open(fixture()).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        let source = tree.elements.iter().find(|e| e.role == "P").unwrap().id;
        let sibling = tree.elements.iter().find(|e| e.role == "Span").unwrap().id;
        let mut middle = Dict::new();
        middle.set(b"Type", Object::name("StructElem"));
        middle.set(b"S", Object::name("Div"));
        middle.set(b"P", Object::Ref(source));
        let middle = cos.add(middle);
        let mut boundary = Dict::new();
        boundary.set(b"Type", Object::name("StructElem"));
        boundary.set(b"S", Object::name("P"));
        boundary.set(b"P", Object::Ref(middle));
        boundary.set(b"Pg", Object::Ref(tree.pages[0]));
        boundary.set(b"K", 0);
        let boundary = cos.add(boundary);
        cos.update_dict(source, |d| {
            d.remove(b"Pg");
            d.set(b"K", Object::Ref(middle));
        })
        .unwrap();
        cos.update_dict(middle, |d| d.set(b"K", Object::Ref(boundary))).unwrap();
        let mut index = Dict::new();
        index.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(boundary), Object::Ref(sibling)])]));
        cos.update_dict(tree.root.unwrap(), |d| d.set(b"ParentTree", Object::Dict(index))).unwrap();
        (Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap()), source, middle, boundary)
    }
    fn attach_structure_destination(cos: &mut CosDocument, target: ObjRef, by_id: bool) {
        let target = if by_id {
            a11y::tags::set_identifier(cos, target, Some("probe-page-binding-target")).unwrap();
            Object::String(PdfString::text("probe-page-binding-target"))
        } else {
            Object::Ref(target)
        };
        let mut action = Dict::new();
        action.set(b"S", Object::name("GoTo"));
        action.set(b"SD", Object::Array(vec![target, Object::name("Fit")]));
        let action = cos.add(action);
        cos.update_dict(cos.root().unwrap(), |d| d.set(b"OpenAction", Object::Ref(action))).unwrap();
    }
    #[test]
    fn recursive_page_binding_proof_checks_all_affected_containers_and_explicit_boundaries() {
        let (bytes, source, middle, boundary) = nested_page_binding_fixture();
        let report = run_named(bytes.clone(), "reparent").unwrap();
        assert_eq!(report["status"], "passed", "{report}");
        assert_eq!(report["inherited_container_page_change"]["affected_container_refs"], json!([reference(source), reference(middle)]));
        assert_eq!(report["inherited_container_page_change"]["explicit_page_boundary_refs"], json!([reference(boundary)]));
        assert_eq!(report["inherited_container_page_change"]["explicit_content_leaves"], 0);
        for by_id in [false, true] {
            let mut cos = CosDocument::open(bytes.clone()).unwrap();
            attach_structure_destination(&mut cos, boundary, by_id);
            let valid = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
            let report = run_named(valid, "reparent").unwrap();
            assert_eq!(report["status"], "passed", "{report}");
            assert_eq!(report["inherited_container_page_change"]["structure_destinations_checked"], 1);
        }
        let mut cos = CosDocument::open(bytes).unwrap();
        let tree = a11y::tags::inspect(&cos).unwrap();
        cos.update_dict(tree.root.unwrap(), |d| d.set(b"IDTree", Object::Dict(Dict::new()))).unwrap();
        let malformed_unused = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
        assert_eq!(run_named(malformed_unused, "reparent").unwrap()["status"], "passed");
    }
    #[test]
    fn recursive_page_binding_proof_rejects_unsafe_middle_nodes_incoming_sd_and_preserved_owners() {
        for case in ["class", "bbox", "reference", "attribute", "integer", "empty", "stale_page", "sd_ref", "sd_id", "preserved_owner"] {
            let (bytes, _, middle, boundary) = nested_page_binding_fixture();
            let mut cos = CosDocument::open(bytes).unwrap();
            match case {
                "class" => cos.update_dict(middle, |d| d.set(b"C", Object::name("Unsafe"))).unwrap(),
                "bbox" => cos.update_dict(middle, |d| d.set(b"BBox", Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]))).unwrap(),
                "reference" => cos.update_dict(middle, |d| d.set(b"Ref", Object::Array(vec![]))).unwrap(),
                "attribute" => {
                    let mut attrs = Dict::new();
                    attrs.set(b"O", Object::name("Layout"));
                    attrs.set(b"Width", 3);
                    cos.update_dict(middle, |d| d.set(b"A", Object::Dict(attrs))).unwrap();
                }
                "integer" => cos.update_dict(middle, |d| d.set(b"K", 0)).unwrap(),
                "empty" => cos
                    .update_dict(middle, |d| {
                        d.remove(b"K");
                    })
                    .unwrap(),
                "stale_page" => {
                    let page = a11y::tags::inspect(&cos).unwrap().pages[0];
                    cos.update_dict(boundary, |d| d.set(b"Pg", Object::Ref(ObjRef::new(page.num, page.generation + 1)))).unwrap();
                }
                "sd_ref" => attach_structure_destination(&mut cos, middle, false),
                "sd_id" => attach_structure_destination(&mut cos, middle, true),
                "preserved_owner" => {
                    let root = a11y::tags::inspect(&cos).unwrap().root.unwrap();
                    let mut owner = Dict::new();
                    owner.set(b"Type", Object::name("StructElem"));
                    owner.set(b"S", Object::name("Span"));
                    owner.set(b"P", Object::Ref(middle));
                    owner.set(b"K", 0);
                    let owner = cos.add(owner);
                    let mut index = exact_dictionary(&cos, root).unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
                    let mut nums = index.get(b"Nums").unwrap().as_array().unwrap().to_vec();
                    nums.extend([2.into(), Object::Array(vec![Object::Ref(owner)])]);
                    index.set(b"Nums", Object::Array(nums));
                    cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(index))).unwrap();
                }
                _ => unreachable!(),
            }
            if matches!(case, "integer" | "empty") {
                let root = a11y::tags::inspect(&cos).unwrap().root.unwrap();
                let mut index = exact_dictionary(&cos, root).unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
                let mut nums = index.get(b"Nums").unwrap().as_array().unwrap().to_vec();
                let mut slots = nums[1].as_array().unwrap().to_vec();
                slots[0] = if case == "integer" { Object::Ref(middle) } else { Object::Null };
                nums[1] = Object::Array(slots);
                index.set(b"Nums", Object::Array(nums));
                cos.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(index))).unwrap();
            }
            let before_tree = a11y::tags::inspect(&cos).unwrap();
            if case == "preserved_owner" {
                assert!(!before_tree.preserved_associations.is_empty());
            } else {
                assert!(before_tree.preserved_associations.is_empty(), "{case}");
            }
            let before = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
            let expected = unchecked_reparent(&mut cos, &before_tree);
            let after_tree = a11y::tags::inspect(&cos).unwrap();
            let after = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
            assert!(prove_move_page_change(before, after, &before_tree, &after_tree, &expected).is_err(), "{case}");
        }
    }
    #[test]
    fn recursive_page_binding_proof_rejects_changed_boundary_data_or_content_page() {
        for change_page in [false, true] {
            let (before, _, _, boundary) = nested_page_binding_fixture();
            let mut cos = CosDocument::open(before.clone()).unwrap();
            let tree = a11y::tags::inspect(&cos).unwrap();
            let expected = unchecked_reparent(&mut cos, &tree);
            cos.update_dict(boundary, |d| {
                if change_page {
                    d.remove(b"Pg");
                } else {
                    d.set(b"PrivateCorruption", Object::Bool(true));
                }
            })
            .unwrap();
            let after_tree = a11y::tags::inspect(&cos).unwrap();
            let after = Arc::new(pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap());
            assert!(prove_move_page_change(before, after, &tree, &after_tree, &expected).is_err());
        }
    }
    #[test]
    fn destination_inventory_rejects_oversized_or_compound_values_before_copy() {
        let mut budget = 32;
        assert!(bounded_destination_clone(&Object::Array(vec![Object::Ref(ObjRef::new(1, 0)), Object::name("Fit")]), &mut budget).is_ok());
        assert!(bounded_destination_clone(&Object::Array(vec![Object::Null; 7]), &mut budget).is_err());
        assert!(bounded_destination_clone(&Object::Array(vec![Object::Dict(Dict::new()), Object::name("Fit")]), &mut budget).is_err());
        assert!(bounded_destination_clone(&Object::String(PdfString::literal(vec![b'x'; 33])), &mut budget).is_err());
        let mut budget = 10;
        assert!(bounded_destination_clone(&Object::String(PdfString::literal(vec![b'x'; 6])), &mut budget).is_ok());
        assert!(bounded_destination_clone(&Object::String(PdfString::literal(vec![b'x'; 6])), &mut budget).is_err());
    }
}
