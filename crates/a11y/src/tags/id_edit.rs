//! Conservative ID and structure-destination edits.
use super::*;
// Resolve bounded reference chains using exact object generations.
pub(super) fn id_reference_value(doc: &Document, raw: &Object, budget: &mut usize) -> Result<std::sync::Arc<Object>> {
    if raw.as_string().is_some_and(|s| s.bytes.len() > 1_048_576) {
        return Err(invalid("ID reference exceeds safe string limits"));
    }
    let mut value = std::sync::Arc::new(raw.clone());
    let mut seen = BTreeSet::new();
    for _ in 0..DEPTH {
        *budget = budget.saturating_add(1);
        if *budget > LIMIT {
            return Err(invalid("ID reference scan exceeds safe limits"));
        }
        let Some(id) = value.as_ref().as_ref() else { return Ok(value) };
        if doc.generation(id.num) != id.generation || !seen.insert(id) {
            return Err(invalid("stale or cyclic ID reference"));
        }
        value = doc.get(id);
        if matches!(&*value, Object::Null) {
            return Err(invalid("unreadable ID reference"));
        }
    }
    Err(invalid("ID reference nesting exceeds safe limits"))
}
fn reject_matching_id(value: &Object, new: &PdfString, string_bytes: &mut usize) -> Result<()> {
    let string = value.as_string().ok_or_else(|| invalid("ID reference must be a byte string"))?;
    *string_bytes = string_bytes.saturating_add(string.bytes.len());
    if string.bytes.len() > 1_048_576 || *string_bytes > 16_777_216 {
        return Err(invalid("ID references exceed safe string limits"));
    }
    if string.bytes == new.bytes {
        return Err(invalid("new identifier would activate an existing ID reference; choose an unrelated identifier"));
    }
    Ok(())
}
pub(super) fn validate_structure_destination_operands(doc: &Document, destination: &[Object], budget: &mut usize) -> Result<()> {
    let mode = id_reference_value(doc, destination.get(1).ok_or_else(|| invalid("missing structure destination mode"))?, budget)?;
    let count = match mode.as_name() {
        Some(b"Fit" | b"FitB") => 2,
        Some(b"FitH" | b"FitV" | b"FitBH" | b"FitBV") => 3,
        Some(b"XYZ") => 5,
        Some(b"FitR") => 6,
        _ => return Err(invalid("unsupported structure destination mode for fresh ID verification")),
    };
    if destination.len() != count {
        return Err(invalid("malformed structure destination operands"));
    }
    for operand in destination.iter().skip(2) {
        let operand = id_reference_value(doc, operand, budget)?;
        let valid = operand.as_f64().is_some_and(f64::is_finite) || (mode.as_name() != Some(b"FitR") && matches!(&*operand, Object::Null));
        if !valid {
            return Err(invalid("malformed structure destination coordinate"));
        }
    }
    Ok(())
}

pub(super) fn check_fresh_id_reference(
    doc: &Document,
    key: &[u8],
    raw: &Object,
    new: &PdfString,
    budget: &mut usize,
    string_bytes: &mut usize,
) -> Result<()> {
    if raw.as_array().is_some_and(|a| a.len() > LIMIT || a.iter().any(|v| v.as_string().is_some_and(|s| s.bytes.len() > 1_048_576))) {
        return Err(invalid("ID reference exceeds safe limits"));
    }
    let value = id_reference_value(doc, raw, budget)?;
    match key {
        b"Headers" => {
            let headers = value.as_array().ok_or_else(|| invalid("Headers must be an array of byte-string IDs"))?;
            if headers.len() > LIMIT {
                return Err(invalid("Headers exceeds safe limits"));
            }
            for header in headers {
                reject_matching_id(&*id_reference_value(doc, header, budget)?, new, string_bytes)?;
            }
        }
        b"ContinuedFrom" => reject_matching_id(&value, new, string_bytes)?,
        b"SD" => {
            let destination = value
                .as_array()
                .filter(|a| (2..=6).contains(&a.len()))
                .ok_or_else(|| invalid("unsupported structure destination for fresh ID verification"))?;
            let raw_target = destination.first().ok_or_else(|| invalid("missing structure destination target"))?;
            let target = id_reference_value(doc, raw_target, budget)?;
            match &*target {
                Object::String(_) => reject_matching_id(&target, new, string_bytes)?,
                Object::Dict(d)
                    if raw_target.as_ref().is_some()
                        && d.name(b"S").is_some()
                        && (!d.contains(b"Type") || d.name(b"Type") == Some(b"StructElem")) => {}
                _ => return Err(invalid("unsupported structure destination target for fresh ID verification")),
            }
            validate_structure_destination_operands(doc, destination, budget)?;
        }
        _ => return Err(invalid("unsupported ID reference kind")),
    }
    Ok(())
}

pub(super) fn validate_id_index(doc: &Document, tree: &TagTree, entries: &[(PdfString, ObjRef)]) -> Result<BTreeMap<Vec<u8>, ObjRef>> {
    if entries.windows(2).any(|p| p.first().zip(p.get(1)).is_some_and(|(a, b)| a.0.bytes >= b.0.bytes)) {
        return Err(invalid("IDTree names must be unique and sorted by raw bytes"));
    }
    let mut expected = BTreeMap::new();
    let mut identifier_bytes = 0usize;
    let mut budget = 0usize;
    for e in &tree.elements {
        let d = dictionary(doc, e.id)?;
        if let Some(raw) = d.get(b"ID") {
            let raw = id_reference_value(doc, raw, &mut budget)?;
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
    Ok(expected)
}

// Reserve the whole queued workload before extending the traversal stack.
pub(super) fn push_scan_children<'a>(value: &'a Object, stack: &mut Vec<(&'a Object, usize)>, visited: usize, depth: usize) -> Result<()> {
    let count = value.as_dict().map_or_else(|| value.as_array().map_or(0, |a| a.len()), |d| d.len());
    if visited.checked_add(stack.len()).and_then(|n| n.checked_add(count)).is_none_or(|n| n > LIMIT) || (count > 0 && depth >= DEPTH) {
        return Err(invalid("ID reference traversal exceeds safe limits"));
    }
    if let Some(d) = value.as_dict() {
        stack.extend(d.iter().map(|(_, v)| (v, depth.saturating_add(1))));
    } else if let Object::Array(a) = value {
        stack.extend(a.iter().map(|v| (v, depth.saturating_add(1))));
    }
    Ok(())
}

// Every owned name-tree node/array must have exactly its one structural incoming edge.
// Count raw references globally, including unknown keys, streams and stale generations.
pub(super) fn validate_id_tree_ownership(doc: &Document, owned: &BTreeSet<ObjRef>) -> Result<()> {
    if owned.is_empty() {
        return Ok(());
    }
    let mut counts: BTreeMap<_, usize> = owned.iter().map(|r| (r.num, 0)).collect();
    let objects = doc.object_numbers();
    if objects.len() > LIMIT {
        return Err(invalid("document too large to verify IDTree ownership"));
    }
    if doc.trailer().len() > LIMIT {
        return Err(invalid("IDTree ownership trailer exceeds safe limits"));
    }
    let mut visits = 0usize;
    let mut scan = |object: &Object| -> Result<()> {
        let mut stack = vec![(object, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            visits = visits.saturating_add(1);
            if visits > LIMIT || depth > DEPTH {
                return Err(invalid("IDTree ownership scan exceeds safe limits"));
            }
            if let Object::Ref(id) = value
                && let Some(count) = counts.get_mut(&id.num)
            {
                *count = count.saturating_add(1);
                if *count > 1 || !owned.contains(id) {
                    return Err(invalid("shared or stale IDTree storage reference"));
                }
            }
            push_scan_children(value, &mut stack, visits, depth)?;
        }
        Ok(())
    };
    for (_, value) in doc.trailer().iter() {
        scan(value)?;
    }
    for num in objects {
        let object = doc.try_get(num).map_err(|e| invalid(format!("cannot verify IDTree ownership: {e}")))?;
        scan(&object)?;
    }
    if counts.values().any(|n| *n != 1) {
        return Err(invalid("unowned IDTree storage"));
    }
    Ok(())
}

fn reject_id_bearing_references(doc: &Document) -> Result<()> {
    let objects = doc.object_numbers();
    if objects.len() > LIMIT {
        return Err(invalid("document too large to verify deleted-ID references"));
    }
    let mut budget = 0usize;
    for num in objects {
        let object = doc.try_get(num).map_err(|e| invalid(format!("cannot verify deleted-ID references: {e}")))?;
        let mut stack = vec![(&*object, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            budget = budget.saturating_add(1);
            if budget > LIMIT || depth > DEPTH {
                return Err(invalid("deleted-ID reference scan exceeds safe limits"));
            }
            if let Some(d) = value.as_dict()
                && (d.contains(b"Headers") || d.contains(b"ContinuedFrom") || d.contains(b"Ref"))
            {
                return Err(invalid("deletion of identified tags with Headers, ContinuedFrom or Ref is not supported"));
            }
            push_scan_children(value, &mut stack, budget, depth)?;
        }
    }
    Ok(())
}

fn prune_ids(
    doc: &Document,
    node: &IdNode,
    removed: &BTreeSet<ObjRef>,
    is_root: bool,
    changes: &mut BTreeMap<ObjRef, Dict>,
    arrays: &mut BTreeMap<ObjRef, Object>,
) -> Result<(Object, Option<(PdfString, PdfString)>)> {
    let mut d = node.dict.clone();
    let (key, values, range) = if node.children.is_empty() {
        let entries: Vec<_> = node.entries.iter().filter(|(_, id)| !removed.contains(id)).collect();
        if entries.is_empty() && !is_root {
            return Err(invalid("deletion would empty a non-root IDTree leaf"));
        }
        let range = entries.first().zip(entries.last()).map(|(lo, hi)| (lo.0.clone(), hi.0.clone()));
        (b"Names".as_slice(), entries.iter().flat_map(|(name, id)| [Object::String(name.clone()), Object::Ref(*id)]).collect::<Vec<_>>(), range)
    } else {
        let mut kids = Vec::new();
        let mut range: Option<(PdfString, PdfString)> = None;
        for child in &node.children {
            let (kid, bounds) = prune_ids(doc, child, removed, false, changes, arrays)?;
            if let Some((lo, hi)) = bounds {
                range = Some((range.map_or(lo, |r| r.0), hi));
            }
            kids.push(kid);
        }
        (b"Kids".as_slice(), kids, range)
    };
    let value = Object::Array(values);
    if let Some(id) = node.dict.reference(key) {
        if *doc.get(id) != value {
            arrays.insert(id, value);
        }
        d.set(key, Object::Ref(id));
    } else {
        d.set(key, value);
    }
    if !is_root || d.contains(b"Limits") {
        if let Some((lo, hi)) = &range {
            // Preserve existing byte-string representation when bounds do not change.
            let unchanged = d.get(b"Limits").and_then(Object::as_array).is_some_and(|a| {
                a.first().and_then(Object::as_string).is_some_and(|s| s.bytes == lo.bytes)
                    && a.get(1).and_then(Object::as_string).is_some_and(|s| s.bytes == hi.bytes)
            });
            if !unchanged {
                d.set(b"Limits", Object::Array(vec![Object::String(lo.clone()), Object::String(hi.clone())]));
            }
        } else {
            d.remove(b"Limits");
        }
    }
    if let Some(id) = node.original.as_ref() {
        if d != node.dict {
            changes.insert(id, d);
        }
        Ok((Object::Ref(id), range))
    } else {
        Ok((Object::Dict(d), range))
    }
}
pub(super) fn prune_deleted_identifiers(
    doc: &Document,
    tree: &TagTree,
    root: ObjRef,
    root_dict: &mut Dict,
    removed: &BTreeSet<ObjRef>,
    changes: &mut BTreeMap<ObjRef, Dict>,
    arrays: &mut BTreeMap<ObjRef, Object>,
) -> Result<bool> {
    let mut entries = Vec::new();
    let reserved: BTreeSet<_> = tree.elements.iter().map(|element| element.id).chain(std::iter::once(root)).collect();
    let mut seen = reserved.clone();
    let node = match root_dict.get(b"IDTree") {
        Some(value) => {
            let node = read_id_node(doc, value, 0, &mut seen, &mut 0, &mut 0)?;
            collect_ids(&node, &mut entries, &mut 0)?;
            Some(node)
        }
        None => None,
    };
    validate_id_index(doc, tree, &entries)?;
    if !entries.iter().any(|(_, id)| removed.contains(id)) {
        return Ok(false);
    }
    // These ID-bearing links, including class attributes, cannot yet be rewritten.
    reject_id_bearing_references(doc)?;
    let owned = seen.difference(&reserved).copied().collect();
    validate_id_tree_ownership(doc, &owned)?;
    let node = node.ok_or_else(|| invalid("tag identifiers have no IDTree"))?;
    let (rewritten, _) = prune_ids(doc, &node, removed, true, changes, arrays)?;
    let changed = root_dict.get(b"IDTree") != Some(&rewritten);
    root_dict.set(b"IDTree", rewritten);
    Ok(changed)
}

pub(super) fn reject_structure_destinations(doc: &Document, tree: &TagTree, removed: &BTreeSet<ObjRef>) -> Result<()> {
    let reachable: BTreeSet<_> = tree.elements.iter().map(|e| e.id).collect();
    // Object-reference destinations do not need the ID index. Defer its proof until
    // an ID-based destination actually uses it, so unrelated malformed IDs cannot
    // turn an otherwise context-preserving move into a false refusal.
    let mut identifiers: Option<BTreeMap<Vec<u8>, ObjRef>> = None;
    let mut id_bytes = 0usize;
    let objects = doc.object_numbers();
    if objects.len() > LIMIT {
        return Err(invalid("document too large to verify structure destinations before deletion"));
    }
    let mut count = 0usize;
    for num in objects {
        let value = doc.try_get(num).map_err(|e| invalid(format!("cannot verify structure destinations: {e}")))?;
        let mut stack = vec![(&*value, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            count = count.saturating_add(1);
            if count > LIMIT || depth > DEPTH {
                return Err(invalid("structure destination check exceeds safe limits"));
            }
            if let Some(d) = value.as_dict()
                && let Some(raw) = d.get(b"SD")
            {
                if d.name(b"S") != Some(b"GoTo") {
                    return Err(invalid("unsupported or remote structure destination context"));
                }
                if raw.as_array().is_some_and(|a| a.len() > 6 || a.iter().any(|v| v.as_string().is_some_and(|s| s.bytes.len() > 1_048_576))) {
                    return Err(invalid("structure destination exceeds safe limits"));
                }
                let destination = id_reference_value(doc, raw, &mut count)?;
                let destination = destination
                    .as_array()
                    .filter(|a| (2..=6).contains(&a.len()))
                    .ok_or_else(|| invalid("unsupported structure destination before deletion"))?;
                validate_structure_destination_operands(doc, destination, &mut count)?;
                let raw_target = destination.first().ok_or_else(|| invalid("missing structure destination target"))?;
                let target = id_reference_value(doc, raw_target, &mut count)?;
                let target = match &*target {
                    Object::String(id) => {
                        if identifiers.is_none() {
                            let root = tree.root.ok_or_else(|| invalid("missing structure root"))?;
                            let root_dict = dictionary(doc, root)?;
                            let mut entries = Vec::new();
                            if let Some(raw) = root_dict.get(b"IDTree") {
                                let mut seen = tree.elements.iter().map(|e| e.id).chain(std::iter::once(root)).collect();
                                let node = read_id_node(doc, raw, 0, &mut seen, &mut 0, &mut 0)?;
                                collect_ids(&node, &mut entries, &mut 0)?;
                            }
                            let index = validate_id_index(doc, tree, &entries)?;
                            id_bytes = index.keys().fold(id_bytes, |n, key| n.saturating_add(key.len()));
                            identifiers = Some(index);
                        }
                        id_bytes = id_bytes.saturating_add(id.bytes.len());
                        if id.bytes.len() > 1_048_576 || id_bytes > 16_777_216 {
                            return Err(invalid("structure destination IDs exceed safe limits"));
                        }
                        identifiers
                            .as_ref()
                            .and_then(|index| index.get(&id.bytes))
                            .copied()
                            .ok_or_else(|| invalid("dangling structure destination ID"))?
                    }
                    Object::Dict(_) => raw_target
                        .as_ref()
                        .filter(|id| reachable.contains(id))
                        .ok_or_else(|| invalid("structure destination must name an exact reachable tag"))?,
                    _ => return Err(invalid("unsupported structure destination target")),
                };
                if removed.contains(&target) {
                    return Err(invalid("cannot remove a structure destination target"));
                }
            }
            push_scan_children(value, &mut stack, count, depth)?;
        }
    }
    Ok(())
}

// Prove every region whose inherited page changes, stopping only at explicit
// page bindings. The caller already performed complete editable-tree validation.
// No descendants, attributes, content leaves or page bindings are rewritten here.
pub(super) fn explicit_subtree_pages(doc: &Document, tree: &TagTree, roots: &[ObjRef]) -> Result<bool> {
    if roots.is_empty() {
        return Ok(true);
    }
    if roots.len() > LIMIT || tree.elements.len() > LIMIT {
        return Err(invalid("page-context proof exceeds safe limits"));
    }
    let by_id: BTreeMap<_, _> = tree.elements.iter().map(|e| (e.id, e)).collect();
    let pages: BTreeSet<_> = tree.pages.iter().copied().collect();
    let mut stack: Vec<_> = roots.iter().copied().map(|id| (id, 0usize)).collect();
    let mut changed = BTreeSet::new();
    let mut visited = 0usize;
    while let Some((id, depth)) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > LIMIT || depth > DEPTH || !changed.insert(id) {
            return Err(invalid("cyclic, shared or excessive page-context graph"));
        }
        let element = by_id.get(&id).ok_or_else(|| invalid("page-context tag is not reachable"))?;
        let dict = dictionary(doc, id)?;
        if element.page.is_some() || dict.contains(b"Pg") {
            return Ok(false);
        }
        if dict.contains(b"C") || dict.contains(b"BBox") || dict.contains(b"Ref") {
            return Ok(false);
        }
        if let Some(raw) = dict.get(b"A") {
            let a = id_reference_value(doc, raw, &mut visited)?;
            let Object::Dict(a) = &*a else { return Ok(false) };
            if a.name(b"O") != Some(b"Layout")
                || !matches!(a.name(b"Placement"), Some(b"Block" | b"Inline"))
                || a.iter().any(|(k, _)| k.as_slice() != b"O" && k.as_slice() != b"Placement")
            {
                return Ok(false);
            }
        }
        let kids = raw_kids(doc, &dict)?;
        if kids.is_empty() {
            return Ok(false);
        }
        if visited.checked_add(stack.len()).and_then(|n| n.checked_add(kids.len())).is_none_or(|n| n > LIMIT) {
            return Err(invalid("page-context proof exceeds safe limits"));
        }
        for kid in kids {
            visited = visited.saturating_add(1);
            let resolved = id_reference_value(doc, &kid, &mut visited)?;
            let Object::Dict(d) = &*resolved else { return Ok(false) };
            if d.contains(b"S") {
                let Some(child) = kid.as_ref() else { return Ok(false) };
                let child_info = by_id.get(&child).ok_or_else(|| invalid("page-context child is not reachable"))?;
                if child_info.parent != Some(id) || d.reference(b"P") != Some(id) {
                    return Ok(false);
                }
                if let Some(page) = d.reference(b"Pg") {
                    if doc.generation(page.num) != page.generation || !pages.contains(&page) || child_info.page != Some(page) {
                        return Ok(false);
                    }
                    // This explicit page shields all descendant semantics from ancestry.
                } else {
                    if d.contains(b"Pg") || child_info.page.is_some() {
                        return Ok(false);
                    }
                    if depth >= DEPTH || visited.checked_add(stack.len()).and_then(|n| n.checked_add(1)).is_none_or(|n| n > LIMIT) {
                        return Err(invalid("page-context proof exceeds safe limits"));
                    }
                    stack.push((child, depth.saturating_add(1)));
                }
            } else {
                if !matches!(d.name(b"Type"), Some(b"MCR" | b"OBJR")) {
                    return Ok(false);
                }
                let Some(page) = d.reference(b"Pg") else { return Ok(false) };
                if doc.generation(page.num) != page.generation || !pages.contains(&page) {
                    return Ok(false);
                }
            }
        }
    }
    // Preserved owners can have P ancestry into a changed container without being
    // present in its K graph. Raw orphan snapshots cannot prove their inherited page
    // is stable, even for the earlier explicit-content leaf-only exception.
    if !tree.preserved_associations.is_empty() {
        return Err(invalid("page-context move cannot prove preserved association inheritance"));
    }
    // A destination to a newly page-bound intermediate container could change its
    // navigation page/coordinates even when every painted-content leaf is shielded.
    reject_structure_destinations(doc, tree, &changed)
        .map_err(|e| invalid(format!("cannot preserve structure destinations while changing page inheritance: {e}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn props(role: &str) -> TagProperties {
        TagProperties { role: Some(role.into()), ..Default::default() }
    }
    fn fixture() -> (Document, ObjRef, ObjRef, ObjRef) {
        let mut doc = Document::new_empty();
        let top = create(&mut doc, None, 0, &props("Document")).unwrap();
        let group = create(&mut doc, Some(top), 0, &props("Sect")).unwrap();
        let child = create(&mut doc, Some(group), 0, &props("P")).unwrap();
        (doc, top, group, child)
    }
    fn bytes(doc: &Document) -> Vec<u8> {
        pdfcraft_cos::write_full(doc, &Default::default()).unwrap()
    }
    fn action(doc: &mut Document, target: Object) -> ObjRef {
        let mut d = Dict::new();
        d.set(b"S", Object::name("GoTo"));
        d.set(b"SD", Object::Array(vec![target, Object::name("Fit")]));
        let id = doc.add(d);
        doc.update_dict(doc.root().unwrap(), |d| d.set(b"OpenAction", Object::Ref(id))).unwrap();
        id
    }
    #[test]
    fn fresh_ids_allow_unrelated_references_but_not_activation_or_rename() {
        for key in [b"Headers".as_slice(), b"ContinuedFrom", b"SD"] {
            let (mut doc, top, _, _) = fixture();
            let value = Object::String(PdfString::text("dangling"));
            let mut d = Dict::new();
            d.set(
                key,
                match key {
                    b"Headers" => Object::Array(vec![value]),
                    b"SD" => Object::Array(vec![value, Object::name("Fit")]),
                    _ => value,
                },
            );
            let reference = doc.add(d);
            let original = doc.get(reference);
            let before = bytes(&doc);
            assert!(set_identifier(&mut doc, top, Some("dangling")).is_err());
            assert_eq!(bytes(&doc), before);
            set_identifier(&mut doc, top, Some("unrelated-ä")).unwrap();
            assert_eq!(doc.get(reference), original);
            let before = bytes(&doc);
            assert!(set_identifier(&mut doc, top, Some("renamed")).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn malformed_reference_chains_and_fitr_null_reject_fresh_ids() {
        for case in 0..3 {
            let (mut doc, top, _, child) = fixture();
            let id = action(&mut doc, Object::Ref(child));
            match case {
                0 => {
                    doc.update_dict(id, |d| {
                        d.set(
                            b"SD",
                            Object::Array(vec![
                                Object::Ref(child),
                                Object::name("FitR"),
                                Object::Null,
                                Object::Int(0),
                                Object::Int(1),
                                Object::Int(1),
                            ]),
                        )
                    })
                    .unwrap();
                }
                1 => {
                    doc.update_dict(id, |d| d.set(b"SD", Object::Ref(id))).unwrap();
                }
                _ => {
                    doc.update_dict(id, |d| d.set(b"SD", Object::Ref(ObjRef::new(99999, 0)))).unwrap();
                }
            }
            let before = bytes(&doc);
            assert!(set_identifier(&mut doc, top, Some("fresh")).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn idtree_pruning_preserves_indirect_storage_and_rejects_aliases() {
        for alias in [false, true] {
            let (mut doc, top, group, child) = fixture();
            set_identifier(&mut doc, top, Some("a")).unwrap();
            set_identifier(&mut doc, child, Some("z")).unwrap();
            let root = inspect(&doc).unwrap().root.unwrap();
            let mut node = doc.get(root).as_dict().unwrap().get(b"IDTree").unwrap().as_dict().unwrap().clone();
            let names = doc.add(node.get(b"Names").unwrap().clone());
            node.set(b"Names", Object::Ref(names));
            node.set(b"Private", Object::Int(72));
            doc.update_dict(root, |d| d.set(b"IDTree", node)).unwrap();
            if alias {
                let mut d = Dict::new();
                d.set(b"Unknown", Object::Ref(names));
                doc.add(d);
            }
            let before = bytes(&doc);
            if alias {
                assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
                assert_eq!(bytes(&doc), before);
                assert!(set_identifier(&mut doc, top, Some("b")).is_err());
            } else {
                delete(&mut doc, group, DeleteMode::Subtree).unwrap();
                let rd = doc.get(root);
                let node = rd.as_dict().unwrap().get(b"IDTree").unwrap().as_dict().unwrap();
                assert_eq!(node.reference(b"Names"), Some(names));
                assert_eq!(node.int(b"Private"), Some(72));
                assert_eq!(doc.get(names).as_array().unwrap().len(), 2);
            }
        }
    }
    #[test]
    fn destinations_survive_unrelated_delete_and_unwrap_but_block_target_removal() {
        for mode in [DeleteMode::Subtree, DeleteMode::Unwrap] {
            for string in [false, true] {
                let (mut doc, top, group, child) = fixture();
                let sibling = create(&mut doc, Some(top), 1, &props("P")).unwrap();
                let target = if mode == DeleteMode::Unwrap { child } else { sibling };
                set_identifier(&mut doc, target, Some("target")).unwrap();
                set_identifier(&mut doc, group, Some("remove")).unwrap();
                action(&mut doc, if string { Object::String(PdfString::text("target")) } else { Object::Ref(target) });
                let mut doc = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
                let tree = inspect(&doc).unwrap();
                let group = tree.elements.iter().find(|e| e.identifier.as_deref() == Some("remove")).unwrap().id;
                let target = tree.elements.iter().find(|e| e.identifier.as_deref() == Some("target")).unwrap().id;
                let a = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"OpenAction").unwrap();
                let original = doc.get(a);
                delete(&mut doc, group, mode).unwrap();
                assert_eq!(doc.get(a), original);
                let reopened = Document::open(std::sync::Arc::new(pdfcraft_cos::write_incremental(&doc, &Default::default()).unwrap())).unwrap();
                assert_eq!(reopened.get(a), original);
                let before = bytes(&doc);
                assert!(delete(&mut doc, target, DeleteMode::Subtree).is_err());
                assert!(delete(&mut doc, target, DeleteMode::Unwrap).is_err());
                assert_eq!(bytes(&doc), before);
            }
        }
    }
    #[test]
    fn destinations_reject_dangling_stale_alias_remote_and_batch_target_removal() {
        for case in 0..6 {
            let (mut doc, top, group, child) = fixture();
            let target = match case {
                0 => Object::String(PdfString::text("missing")),
                1 => Object::Ref(ObjRef::new(child.num, 1)),
                2 => Object::Ref(doc.add(Object::Ref(child))),
                _ => Object::Ref(top),
            };
            let a = action(&mut doc, target);
            if case == 3 {
                doc.update_dict(a, |d| d.set(b"S", Object::name("GoToR"))).unwrap();
            }
            if case == 4 {
                doc.update_dict(a, |d| d.set(b"SD", Object::Ref(ObjRef::new(99999, 0)))).unwrap();
            }
            let before = bytes(&doc);
            let result =
                if case == 5 { delete_tags(&mut doc, &[child, top], DeleteMode::Unwrap) } else { delete(&mut doc, group, DeleteMode::Subtree) };
            assert!(result.is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn pageless_explicit_content_moves_keep_context_and_reject_ambiguous_cases() {
        for case in 0..7 {
            let (mut doc, top, group, child) = fixture();
            let mut page = Dict::new();
            page.set(b"Type", Object::name("Page"));
            page.set(b"StructParents", Object::Int(0));
            let page = doc.add(page);
            let pages = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
            doc.update_dict(pages, |d| {
                d.set(b"Kids", Object::Array(vec![Object::Ref(page)]));
                d.set(b"Count", Object::Int(1));
            })
            .unwrap();
            let destination = create(&mut doc, Some(top), 1, &props("P")).unwrap();
            doc.update_dict(destination, |d| d.set(b"Pg", Object::Ref(page))).unwrap();
            let mut mcr = Dict::new();
            mcr.set(b"Type", Object::name("MCR"));
            mcr.set(b"MCID", Object::Int(0));
            if case != 2 {
                mcr.set(b"Pg", Object::Ref(page));
            }
            doc.update_dict(child, |d| {
                d.set(b"K", if case == 3 { Object::Int(0) } else { Object::Dict(mcr) });
                if case == 4 {
                    d.set(b"C", Object::name("Custom"));
                }
                if case == 5 {
                    let mut a = Dict::new();
                    a.set(b"O", Object::name("Layout"));
                    a.set(b"BBox", Object::Array(vec![Object::Int(0); 4]));
                    d.set(b"A", a);
                }
                if case == 6 {
                    let mut a = Dict::new();
                    a.set(b"O", Object::name("Layout"));
                    a.set(b"Placement", Object::name("Block"));
                    d.set(b"A", a);
                }
            })
            .unwrap();
            let root = inspect(&doc).unwrap().root.unwrap();
            let mut pt = Dict::new();
            pt.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(child)])]));
            doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
            let before = bytes(&doc);
            let result = if case == 1 {
                move_tags(&mut doc, &[child], Some(destination), 0)
            } else {
                move_tag(&mut doc, if case == 0 { group } else { child }, Some(destination), 0)
            };
            if case == 0 || case == 1 || case == 6 {
                result.unwrap();
            } else {
                assert!(result.is_err(), "case {case}");
                assert_eq!(bytes(&doc), before);
            }
        }
    }
    #[test]
    fn nested_idtree_prune_keeps_unaffected_nodes_and_refuses_empty_leaf() {
        let (mut doc, top, group, child) = fixture();
        set_identifier(&mut doc, top, Some("a")).unwrap();
        set_identifier(&mut doc, group, Some("b")).unwrap();
        set_identifier(&mut doc, child, Some("z")).unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let left_names = doc.add(Object::Array(vec![
            Object::String(PdfString::text("a")),
            Object::Ref(top),
            Object::String(PdfString::text("b")),
            Object::Ref(group),
        ]));
        let right_names = doc.add(Object::Array(vec![Object::String(PdfString::text("z")), Object::Ref(child)]));
        let mut left = Dict::new();
        left.set(b"Names", Object::Ref(left_names));
        left.set(b"Limits", Object::Array(vec![Object::String(PdfString::text("a")), Object::String(PdfString::text("b"))]));
        left.set(b"Unknown", Object::Int(44));
        let left = doc.add(left);
        let mut right = Dict::new();
        right.set(b"Names", Object::Ref(right_names));
        right.set(b"Limits", Object::Array(vec![Object::String(PdfString::text("z")); 2]));
        let right = doc.add(right);
        let kids = doc.add(Object::Array(vec![Object::Ref(left), Object::Ref(right)]));
        let mut tree = Dict::new();
        tree.set(b"Kids", Object::Ref(kids));
        doc.update_dict(root, |d| d.set(b"IDTree", tree)).unwrap();
        let original_right = doc.get(right);
        let original_names = doc.get(right_names);
        let original_kids = doc.get(kids);
        delete(&mut doc, group, DeleteMode::Unwrap).unwrap();
        assert!(std::sync::Arc::ptr_eq(&doc.get(right), &original_right));
        assert!(std::sync::Arc::ptr_eq(&doc.get(right_names), &original_names));
        assert!(std::sync::Arc::ptr_eq(&doc.get(kids), &original_kids));
        assert_eq!(doc.get(left).as_dict().unwrap().int(b"Unknown"), Some(44));
        let before = bytes(&doc);
        assert!(delete(&mut doc, child, DeleteMode::Subtree).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn identified_deletion_keeps_headers_continued_from_ref_strict() {
        for key in [b"Headers".as_slice(), b"ContinuedFrom", b"Ref"] {
            let (mut doc, _, group, _) = fixture();
            set_identifier(&mut doc, group, Some("remove")).unwrap();
            let mut attr = Dict::new();
            attr.set(key, Object::String(PdfString::text("other")));
            let mut classes = Dict::new();
            classes.set(b"Custom", attr);
            let root = inspect(&doc).unwrap().root.unwrap();
            doc.update_dict(root, |d| d.set(b"ClassMap", classes)).unwrap();
            let before = bytes(&doc);
            assert!(delete(&mut doc, group, DeleteMode::Unwrap).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }
    #[test]
    fn structure_destination_string_workloads_are_bounded() {
        for oversized in [true, false] {
            let (mut doc, top, group, _) = fixture();
            if oversized {
                action(&mut doc, Object::String(PdfString { bytes: vec![b'x'; 1_048_577], hex: false }));
            } else {
                let text = "x".repeat(1_048_576);
                set_identifier(&mut doc, top, Some(&text)).unwrap();
                let destination = doc.add(Object::Array(vec![Object::String(PdfString::text(&text)), Object::name("Fit")]));
                for _ in 0..16 {
                    let mut a = Dict::new();
                    a.set(b"S", Object::name("GoTo"));
                    a.set(b"SD", Object::Ref(destination));
                    doc.add(a);
                }
            }
            let before = bytes(&doc);
            assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
            assert_eq!(bytes(&doc), before);
        }
    }

    #[test]
    fn explicit_objr_and_form_mcr_move_without_rewriting_leaf_identity() {
        for form in [false, true] {
            for batch in [false, true] {
                let (mut doc, top, group, child) = fixture();
                let mut page = Dict::new();
                page.set(b"Type", Object::name("Page"));
                let page = doc.add(page);
                let pages = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
                doc.update_dict(pages, |d| {
                    d.set(b"Kids", Object::Array(vec![Object::Ref(page)]));
                    d.set(b"Count", Object::Int(1));
                })
                .unwrap();
                doc.update_dict(group, |d| {
                    d.remove(b"K");
                })
                .unwrap();
                doc.update_dict(top, |d| d.set(b"K", Object::Array(vec![Object::Ref(group), Object::Ref(child)]))).unwrap();
                doc.update_dict(child, |d| d.set(b"P", Object::Ref(top))).unwrap();
                doc.update_dict(group, |d| d.set(b"Pg", Object::Ref(page))).unwrap();
                let mut object = Dict::new();
                object.set(if form { b"StructParents".as_slice() } else { b"StructParent".as_slice() }, Object::Int(0));
                let object = if form {
                    object.set(b"Type", Object::name("XObject"));
                    object.set(b"Subtype", Object::name("Form"));
                    doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(object, b"q Q".to_vec())))
                } else {
                    object.set(b"Type", Object::name("Annot"));
                    object.set(b"Subtype", Object::name("Link"));
                    doc.add(object)
                };
                let mut leaf = Dict::new();
                leaf.set(b"Type", Object::name(if form { "MCR" } else { "OBJR" }));
                leaf.set(b"Pg", Object::Ref(page));
                leaf.set(b"Private", Object::Int(81));
                if form {
                    leaf.set(b"Stm", Object::Ref(object));
                    leaf.set(b"MCID", Object::Int(0));
                } else {
                    leaf.set(b"Obj", Object::Ref(object));
                }
                let leaf = doc.add(leaf);
                doc.update_dict(child, |d| d.set(b"K", Object::Ref(leaf))).unwrap();
                let root = inspect(&doc).unwrap().root.unwrap();
                let mut pt = Dict::new();
                pt.set(b"Nums", Object::Array(vec![Object::Int(0), if form { Object::Array(vec![Object::Ref(child)]) } else { Object::Ref(child) }]));
                doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
                let original = doc.get(leaf);
                let original_object = doc.get(object);
                if batch {
                    move_tags(&mut doc, &[child], Some(group), 0).unwrap();
                } else {
                    move_tag(&mut doc, child, Some(group), 0).unwrap();
                }
                assert!(std::sync::Arc::ptr_eq(&doc.get(leaf), &original));
                assert!(std::sync::Arc::ptr_eq(&doc.get(object), &original_object));
            }
        }
    }
    fn unreadable_live_object_fixture() -> Document {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 3 0 R >>",
            "<< /Type /Pages /Kids [] /Count 0 >>",
            "<< /Type /StructTreeRoot /K 4 0 R /IDTree 6 0 R >>",
            "<< /Type /StructElem /S /Document /P 3 0 R /K 5 0 R >>",
            "<< /Type /StructElem /S /P /P 4 0 R /ID (remove) >>",
            "<< /Names 7 0 R >>",
            "[(remove) 5 0 R]",
            // Two invalid tokens prevent the tolerant empty-object-to-null repair.
            "invalid invalid",
        ];
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
        }
        let xref = bytes.len();
        bytes.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(format!("trailer\n<< /Size 9 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        Document::open(std::sync::Arc::new(bytes)).unwrap()
    }

    #[test]
    fn whole_document_id_scans_refuse_unreadable_live_objects_atomically() {
        let mut doc = unreadable_live_object_fixture();
        assert!(doc.object_numbers().contains(&8));
        assert!(doc.try_get(8).is_err());
        let tree = inspect(&doc).unwrap();
        let removed = BTreeSet::from([ObjRef::new(5, 0)]);
        let owned = BTreeSet::from([ObjRef::new(6, 0), ObjRef::new(7, 0)]);
        assert!(validate_id_tree_ownership(&doc, &owned).unwrap_err().to_string().contains("cannot verify"));
        assert!(reject_id_bearing_references(&doc).unwrap_err().to_string().contains("cannot verify"));
        assert!(reject_structure_destinations(&doc, &tree, &removed).unwrap_err().to_string().contains("cannot verify"));
        let before = bytes(&doc);
        assert!(delete(&mut doc, ObjRef::new(5, 0), DeleteMode::Subtree).is_err());
        assert_eq!(bytes(&doc), before);
        assert!(set_identifier(&mut doc, ObjRef::new(4, 0), Some("fresh")).is_err());
        assert_eq!(bytes(&doc), before);
    }

    #[test]
    fn direct_container_scan_budgets_reject_before_extending_or_mutating() {
        let too_large = Object::Array(vec![Object::Null; LIMIT + 1]);
        let mut stack = Vec::new();
        assert!(push_scan_children(&too_large, &mut stack, 0, 0).is_err());
        assert!(stack.is_empty(), "must check before allocating queued children");
        let smaller = Object::Array(vec![Object::Null; LIMIT / 2]);
        stack.push((&Object::Null, 0));
        let before = stack.len();
        assert!(push_scan_children(&smaller, &mut stack, LIMIT / 2, 0).is_err());
        assert_eq!(stack.len(), before, "aggregate queue work must be checked before extending");
        let (mut doc, top, group, _) = fixture();
        set_identifier(&mut doc, group, Some("remove")).unwrap();
        let root = inspect(&doc).unwrap().root.unwrap();
        let node = doc.add(doc.get(root).as_dict().unwrap().get(b"IDTree").unwrap().clone());
        doc.update_dict(root, |d| d.set(b"IDTree", Object::Ref(node))).unwrap();
        doc.add(too_large);
        let tree = inspect(&doc).unwrap();
        assert!(validate_id_tree_ownership(&doc, &BTreeSet::from([node])).is_err());
        assert!(reject_id_bearing_references(&doc).is_err());
        assert!(reject_structure_destinations(&doc, &tree, &BTreeSet::from([group])).is_err());
        let before = bytes(&doc);
        assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
        assert_eq!(bytes(&doc), before);
        assert!(set_identifier(&mut doc, top, Some("fresh")).is_err());
        assert_eq!(bytes(&doc), before);
    }
    fn recursive_page_fixture() -> (Document, ObjRef, ObjRef, ObjRef, ObjRef, ObjRef, ObjRef) {
        let (mut doc, top, source, middle) = fixture();
        let shield = create(&mut doc, Some(middle), 0, &props("P")).unwrap();
        let destination = create(&mut doc, Some(top), 1, &props("P")).unwrap();
        let mut p = Dict::new();
        p.set(b"Type", Object::name("Page"));
        p.set(b"StructParents", Object::Int(0));
        let content_page = doc.add(p);
        let mut p = Dict::new();
        p.set(b"Type", Object::name("Page"));
        let destination_page = doc.add(p);
        let pages = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
        doc.update_dict(pages, |d| {
            d.set(b"Kids", Object::Array(vec![Object::Ref(content_page), Object::Ref(destination_page)]));
            d.set(b"Count", Object::Int(2));
        })
        .unwrap();
        doc.update_dict(shield, |d| {
            d.set(b"Pg", Object::Ref(content_page));
            d.set(b"K", Object::Int(0));
        })
        .unwrap();
        doc.update_dict(destination, |d| d.set(b"Pg", Object::Ref(destination_page))).unwrap();
        for (id, title) in [(source, "source"), (middle, "middle"), (shield, "shield"), (destination, "destination")] {
            doc.update_dict(id, |d| d.set(b"T", Object::String(PdfString::text(title)))).unwrap();
        }
        let mut pt = Dict::new();
        pt.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(shield)])]));
        let root = inspect(&doc).unwrap().root.unwrap();
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        (doc, source, middle, shield, destination, content_page, destination_page)
    }

    #[test]
    fn recursive_pageless_move_preserves_explicit_barriers_content_and_reopen() {
        for batch in [false, true] {
            let (doc, _, _, _, _, _, _) = recursive_page_fixture();
            let mut doc = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
            let tree = inspect(&doc).unwrap();
            let named = |name: &str| tree.elements.iter().find(|e| e.title.as_deref() == Some(name)).unwrap().id;
            let source = named("source");
            let middle = named("middle");
            let shield = named("shield");
            let destination = named("destination");
            let old_page = tree.element(shield).unwrap().page;
            let new_page = tree.element(destination).unwrap().page;
            assert_ne!(old_page, new_page);
            let before_middle = doc.get(middle);
            let before_shield = doc.get(shield);
            let root = tree.root.unwrap();
            let before_pt = doc.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().clone();
            if batch {
                move_tags(&mut doc, &[source, middle], Some(destination), 0).unwrap();
            } else {
                move_tag(&mut doc, source, Some(destination), 0).unwrap();
            }
            let after = inspect(&doc).unwrap();
            assert_eq!(after.element(source).unwrap().page, new_page);
            assert_eq!(after.element(middle).unwrap().page, new_page);
            assert_eq!(after.element(shield).unwrap().page, old_page);
            assert_eq!(after.element(shield).unwrap().kids, tree.element(shield).unwrap().kids);
            assert_eq!(doc.get(middle), before_middle);
            assert_eq!(doc.get(shield), before_shield);
            assert_eq!(doc.get(root).as_dict().unwrap().get(b"ParentTree"), Some(&before_pt));
            assert!(!doc.get(source).as_dict().unwrap().contains(b"Pg"));
            let reopened = Document::open(std::sync::Arc::new(pdfcraft_cos::write_incremental(&doc, &Default::default()).unwrap())).unwrap();
            assert_eq!(inspect(&reopened).unwrap(), after);
            assert_eq!(reopened.get(middle), before_middle);
            assert_eq!(reopened.get(shield), before_shield);
        }
    }

    #[test]
    fn recursive_pageless_move_refuses_ambiguous_intermediate_nodes_atomically() {
        for case in 0..9 {
            let (mut doc, source, middle, shield, destination, page, _) = recursive_page_fixture();
            match case {
                0 => {
                    doc.update_dict(middle, |d| d.set(b"BBox", Object::Array(vec![Object::Int(0); 4]))).unwrap();
                }
                1 => {
                    doc.update_dict(middle, |d| d.set(b"C", Object::name("Unproved"))).unwrap();
                }
                2 => {
                    let mut a = Dict::new();
                    a.set(b"O", Object::name("Layout"));
                    a.set(b"BBox", Object::Array(vec![Object::Int(0); 4]));
                    doc.update_dict(middle, |d| d.set(b"A", a)).unwrap();
                }
                3 => {
                    let mut a = Dict::new();
                    a.set(b"O", Object::name("Unknown"));
                    doc.update_dict(middle, |d| d.set(b"A", a)).unwrap();
                }
                4 => {
                    create(&mut doc, Some(middle), 1, &props("Div")).unwrap();
                }
                5 => {
                    doc.update_dict(middle, |d| d.set(b"K", Object::Array(vec![Object::Ref(shield), Object::Int(1)]))).unwrap();
                }
                6 => {
                    let mut m = Dict::new();
                    m.set(b"Type", Object::name("MCR"));
                    m.set(b"MCID", Object::Int(1));
                    doc.update_dict(middle, |d| d.set(b"K", Object::Array(vec![Object::Ref(shield), Object::Dict(m)]))).unwrap();
                }
                7 => {
                    doc.update_dict(shield, |d| d.set(b"Pg", Object::Ref(ObjRef::new(page.num, page.generation.saturating_add(1))))).unwrap();
                }
                _ => {
                    doc.update_dict(middle, |d| d.set(b"Ref", Object::Array(vec![Object::Ref(shield)]))).unwrap();
                }
            }
            let before = bytes(&doc);
            assert!(move_tag(&mut doc, source, Some(destination), 0).is_err(), "case {case}");
            assert_eq!(bytes(&doc), before);
            assert!(move_tags(&mut doc, &[source], Some(destination), 0).is_err(), "batch case {case}");
            assert_eq!(bytes(&doc), before);
        }
    }

    #[test]
    fn recursive_pageless_move_checks_changed_container_destination_targets() {
        for string in [false, true] {
            for changed in [false, true] {
                let (mut doc, source, middle, shield, destination, _, _) = recursive_page_fixture();
                let target = if changed { middle } else { shield };
                if string {
                    set_identifier(&mut doc, target, Some("destination-target")).unwrap();
                }
                let a = action(&mut doc, if string { Object::String(PdfString::text("destination-target")) } else { Object::Ref(target) });
                let before_action = doc.get(a);
                let before = bytes(&doc);
                let result = move_tag(&mut doc, source, Some(destination), 0);
                if changed {
                    assert!(result.is_err());
                    assert_eq!(bytes(&doc), before);
                } else {
                    result.unwrap();
                    assert_eq!(doc.get(a), before_action);
                }
            }
        }
    }

    #[test]
    fn recursive_pageless_move_does_not_validate_unused_malformed_idtree() {
        let (mut doc, source, _, _, destination, _, _) = recursive_page_fixture();
        let root = inspect(&doc).unwrap().root.unwrap();
        doc.update_dict(root, |d| d.set(b"IDTree", Dict::new())).unwrap();
        move_tag(&mut doc, source, Some(destination), 0).unwrap();
    }

    #[test]
    fn recursive_pageless_move_refuses_preserved_owner_in_affected_parent_chain() {
        let (mut doc, source, middle, _, destination, _, _) = recursive_page_fixture();
        let mut orphan = Dict::new();
        orphan.set(b"Type", Object::name("StructElem"));
        orphan.set(b"S", Object::name("P"));
        orphan.set(b"P", Object::Ref(middle));
        let orphan = doc.add(orphan);
        let root = inspect(&doc).unwrap().root.unwrap();
        let mut pt = doc.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
        let mut nums = pt.get(b"Nums").unwrap().as_array().unwrap().to_vec();
        nums.extend([Object::Int(1), Object::Array(vec![Object::Ref(orphan)])]);
        pt.set(b"Nums", Object::Array(nums));
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.edit_blockers.is_empty(), "{:?}", tree.edit_blockers);
        assert!(!tree.preserved_associations.is_empty());
        let before = bytes(&doc);
        assert!(move_tag(&mut doc, source, Some(destination), 0).is_err());
        assert_eq!(bytes(&doc), before);
        assert!(move_tags(&mut doc, &[source], Some(destination), 0).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn leaf_pageless_move_refuses_preserved_owner_in_affected_parent_chain() {
        let (mut doc, _, _, leaf, destination, page, _) = recursive_page_fixture();
        let mut mcr = Dict::new();
        mcr.set(b"Type", Object::name("MCR"));
        mcr.set(b"MCID", Object::Int(0));
        mcr.set(b"Pg", Object::Ref(page));
        doc.update_dict(leaf, |d| {
            d.remove(b"Pg");
            d.set(b"K", mcr);
        })
        .unwrap();
        let mut orphan = Dict::new();
        orphan.set(b"Type", Object::name("StructElem"));
        orphan.set(b"S", Object::name("P"));
        orphan.set(b"P", Object::Ref(leaf));
        let orphan = doc.add(orphan);
        let root = inspect(&doc).unwrap().root.unwrap();
        let mut pt = doc.get(root).as_dict().unwrap().get(b"ParentTree").unwrap().as_dict().unwrap().clone();
        let mut nums = pt.get(b"Nums").unwrap().as_array().unwrap().to_vec();
        nums.extend([Object::Int(1), Object::Array(vec![Object::Ref(orphan)])]);
        pt.set(b"Nums", Object::Array(nums));
        doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
        let tree = inspect(&doc).unwrap();
        assert!(tree.edit_blockers.is_empty(), "{:?}", tree.edit_blockers);
        assert!(!tree.preserved_associations.is_empty());
        let before = bytes(&doc);
        assert!(move_tag(&mut doc, leaf, Some(destination), 0).is_err());
        assert_eq!(bytes(&doc), before);
        assert!(move_tags(&mut doc, &[leaf], Some(destination), 0).is_err());
        assert_eq!(bytes(&doc), before);
    }
    #[test]
    fn many_pageless_roots_share_one_batch_page_context_proof() {
        let (mut doc, source, _, _, destination, page, _) = recursive_page_fixture();
        let top = inspect(&doc).unwrap().element(source).unwrap().parent.unwrap();
        let mut roots = Vec::new();
        let mut shields = Vec::new();
        for _ in 0..200 {
            let mut container = Dict::new();
            container.set(b"S", Object::name("Sect"));
            container.set(b"P", Object::Ref(top));
            let container = doc.add(container);
            let mut shield = Dict::new();
            shield.set(b"S", Object::name("P"));
            shield.set(b"P", Object::Ref(container));
            shield.set(b"Pg", Object::Ref(page));
            let shield = doc.add(shield);
            doc.update_dict(container, |d| d.set(b"K", Object::Ref(shield))).unwrap();
            roots.push(container);
            shields.push((shield, doc.get(shield)));
        }
        doc.update_dict(top, |d| {
            d.set(
                b"K",
                Object::Array(std::iter::once(source).chain(std::iter::once(destination)).chain(roots.iter().copied()).map(Object::Ref).collect()),
            )
        })
        .unwrap();
        move_tags(&mut doc, &roots, Some(destination), 0).unwrap();
        let tree = inspect(&doc).unwrap();
        assert_eq!(tree.element(destination).unwrap().kids, roots.iter().map(|id| TagKid::Element { id: *id }).collect::<Vec<_>>());
        for (shield, before) in shields {
            assert_eq!(doc.get(shield), before);
            assert_eq!(tree.element(shield).unwrap().page, Some(page));
        }
    }
}
