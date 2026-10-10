//! Preserve proven surplus ParentTree associations without repairing the PDF.
use super::*;

#[derive(Default)]
pub(super) struct AssociationReport {
    pub blockers: Vec<String>,
    pub preserved: Vec<ParentAssociation>,
}

fn content_identity(kid: &TagKid) -> Option<(Option<ObjRef>, Option<i64>)> {
    match kid {
        TagKid::Mcid { id, page } | TagKid::Mcr { id, page, stream: None } => Some((*page, Some(*id))),
        TagKid::Mcr { id, stream: Some(stream), .. } => Some((Some(*stream), Some(*id))),
        TagKid::Objr { object, .. } => Some((Some(*object), None)),
        _ => None,
    }
}

pub(super) fn validate(doc: &Document, tree: &TagTree, entries: &[ParentEntry]) -> AssociationReport {
    let mut report = AssociationReport::default();
    let by_key: BTreeMap<_, _> = entries.iter().map(|e| (e.key, &e.value)).collect();
    let page_ids: BTreeSet<_> = tree.pages.iter().copied().collect();
    let ids: BTreeSet<_> = tree.elements.iter().map(|e| e.id).collect();
    let mut seen = BTreeSet::new();
    let mut expected = BTreeMap::new();
    let mut key_objects = BTreeMap::new();
    for e in &tree.elements {
        for kid in &e.kids {
            let Some((object, mcid)) = content_identity(kid) else { continue };
            let Some(object) = object else {
                report.blockers.push(format!("{} content has no effective page or stream", e.id));
                continue;
            };
            if !seen.insert((object, mcid)) {
                report.blockers.push(format!("duplicate content ownership on {object}"));
            }
            let key_name = if mcid.is_some() { b"StructParents".as_slice() } else { b"StructParent" };
            let object_value = doc.get(object);
            if doc.generation(object.num) != object.generation {
                report.blockers.push(format!("stale content object generation {object}"));
            }
            match kid {
                TagKid::Mcid { .. } | TagKid::Mcr { stream: None, .. } if !page_ids.contains(&object) => {
                    report.blockers.push(format!("{object} is not a document page"))
                }
                TagKid::Mcr { stream: Some(_), .. } if !matches!(&*object_value, Object::Stream(_)) => {
                    report.blockers.push(format!("{object} is not a content stream"))
                }
                _ => {}
            }
            let key = object_value.as_dict().and_then(|d| d.int(key_name));
            let Some(value) = key.and_then(|k| by_key.get(&k)) else {
                report.blockers.push(format!("{} content lacks its ParentTree mapping", e.id));
                continue;
            };
            if let Some(key) = key
                && key_objects.insert(key, object).is_some_and(|old| old != object)
            {
                report.blockers.push(format!("ParentTree key {key} is shared by different content objects"));
            }
            if let Some(key) = key
                && expected.insert((key, mcid), e.id).is_some()
            {
                report.blockers.push(format!("multiple content owners use ParentTree key {key}"));
            }
            let value = doc.resolve(value);
            let owner = if let Some(mcid) = mcid {
                usize::try_from(mcid).ok().and_then(|n| value.as_array().and_then(|a| a.get(n))).and_then(Object::as_ref)
            } else {
                by_key.get(&key.unwrap_or(-1)).and_then(|v| v.as_ref())
            };
            if owner != Some(e.id) {
                report.blockers.push(format!("{} content has inconsistent ParentTree owner", e.id));
            }
        }
    }
    let scalar_candidates: BTreeSet<_> = entries
        .iter()
        .filter_map(|entry| {
            let owner = entry.value.as_ref()?;
            (ids.contains(&owner) && expected.get(&(entry.key, None)) != Some(&owner)).then_some(entry.key)
        })
        .collect();
    let unclaimed = if scalar_candidates.is_empty() {
        BTreeSet::new()
    } else {
        match unclaimed_scalar_keys(doc, &scalar_candidates) {
            Ok((keys, _)) => keys,
            Err(error) => {
                report.blockers.push(error.to_string());
                BTreeSet::new()
            }
        }
    };
    for entry in entries {
        let resolved = doc.resolve(&entry.value);
        match &*resolved {
            Object::Array(array) => {
                for (index, value) in array.iter().enumerate() {
                    if let Some(owner) = value.as_ref() {
                        let Ok(mcid) = i64::try_from(index) else {
                            report.blockers.push("ParentTree MCID exceeds safe limits".into());
                            continue;
                        };
                        classify_unexplained(
                            doc,
                            &ids,
                            &expected,
                            &unclaimed,
                            ParentAssociation { kind: ParentAssociationKind::UnreachableOwner, key: entry.key, mcid: Some(mcid), owner },
                            &mut report,
                        );
                    }
                }
            }
            _ => {
                if let Some(owner) = entry.value.as_ref() {
                    classify_unexplained(
                        doc,
                        &ids,
                        &expected,
                        &unclaimed,
                        ParentAssociation { kind: ParentAssociationKind::UnreachableOwner, key: entry.key, mcid: None, owner },
                        &mut report,
                    );
                }
            }
        }
    }
    report
}

fn classify_unexplained(
    doc: &Document,
    reachable: &BTreeSet<ObjRef>,
    expected: &BTreeMap<(i64, Option<i64>), ObjRef>,
    unclaimed: &BTreeSet<i64>,
    mut association: ParentAssociation,
    report: &mut AssociationReport,
) {
    if expected.get(&(association.key, association.mcid)) == Some(&association.owner) {
        return;
    }
    if reachable.contains(&association.owner) {
        // Arrays are never eligible. A scalar is inert only after a whole-object
        // proof that no page, stream, annotation or direct dictionary claims it.
        if association.mcid.is_none()
            && unclaimed.contains(&association.key)
            && dictionary(doc, association.owner)
                .is_ok_and(|d| d.name(b"S").is_some_and(|name| !name.is_empty()) && (!d.contains(b"Type") || d.name(b"Type") == Some(b"StructElem")))
        {
            association.kind = ParentAssociationKind::UnclaimedScalar;
            report.preserved.push(association);
        } else {
            report.blockers.push(format!("ParentTree key {} contains an orphan association to reachable tag {}", association.key, association.owner));
        }
        return;
    }
    let valid_owner = dictionary(doc, association.owner)
        .is_ok_and(|d| d.name(b"S").is_some_and(|name| !name.is_empty()) && (!d.contains(b"Type") || d.name(b"Type") == Some(b"StructElem")));
    if !valid_owner {
        report
            .blockers
            .push(format!("ParentTree key {} has stale, dangling or non-StructElem unreachable owner {}", association.key, association.owner));
        return;
    }
    report.preserved.push(association);
}

/// Snapshot owner dictionaries and their indirect K containers/content dictionaries.
/// Otherwise commit_dict could mutate an orphan's shared K array while its dictionary
/// retained exactly the same reference. Do not follow page/resources/stream objects.
fn orphan_objects(doc: &Document, tree: &TagTree) -> Result<BTreeMap<ObjRef, Object>> {
    collect_orphan_objects(doc, tree).map(|(objects, _)| objects)
}

pub(super) fn preservation_work(doc: &Document, tree: &TagTree) -> Result<usize> {
    if tree.preserved_associations.is_empty() {
        return Ok(0);
    }
    let (_, visits) = collect_orphan_objects(doc, tree)?;
    let keys: BTreeSet<_> = tree.preserved_associations.iter().filter(|a| a.kind == ParentAssociationKind::UnclaimedScalar).map(|a| a.key).collect();
    let scan_visits = if keys.is_empty() { 0 } else { unclaimed_scalar_keys(doc, &keys)?.1 };
    Ok(visits.saturating_add(tree.preserved_associations.len()).saturating_add(scan_visits))
}

fn collect_orphan_objects(doc: &Document, tree: &TagTree) -> Result<(BTreeMap<ObjRef, Object>, usize)> {
    let mut objects = BTreeMap::new();
    let mut stack: Vec<_> = tree.preserved_associations.iter().map(|a| (Object::Ref(a.owner), 0usize)).collect();
    let mut count = 0usize;
    while let Some((value, depth)) = stack.pop() {
        count = count.saturating_add(1);
        if count > LIMIT || depth > DEPTH || stack.len().saturating_add(count) > LIMIT {
            return Err(invalid("unreachable association preservation exceeds safe limits"));
        }
        let value = if let Some(id) = value.as_ref() {
            if doc.generation(id.num) != id.generation {
                return Err(invalid("stale object in unreachable association child graph"));
            }
            if objects.contains_key(&id) {
                continue;
            }
            let value = (*doc.try_get(id.num).map_err(|e| invalid(format!("cannot read unreachable child object: {e}")))?).clone();
            if matches!(value, Object::Null) {
                return Err(invalid("missing object in unreachable association child graph"));
            }
            objects.insert(id, value.clone());
            value
        } else {
            value
        };
        match value {
            Object::Array(values) => {
                if stack.len().saturating_add(count).saturating_add(values.len()) > LIMIT {
                    return Err(invalid("unreachable association preservation exceeds safe limits"));
                }
                stack.extend(values.into_iter().map(|v| (v, depth.saturating_add(1))));
            }
            Object::Dict(d) => {
                if let Some(kids) = d.get(b"K") {
                    stack.push((kids.clone(), depth.saturating_add(1)));
                }
                // A reachable OBJR deletion may clear this object's StructParent.
                // Preserve orphan OBJR targets as well as the leaf dictionaries.
                if d.name(b"Type") == Some(b"OBJR") {
                    let object = d.reference(b"Obj").ok_or_else(|| invalid("unreachable OBJR has no exact object reference"))?;
                    stack.push((Object::Ref(object), depth.saturating_add(1)));
                }
            }
            _ => {}
        }
    }
    Ok((objects, count))
}

/// Text-only metadata may preserve unrelated malformed trees, but must not mutate
/// a dictionary aliased by an orphan's retained child/object graph.
pub(super) fn reject_metadata_alias(doc: &Document, tree: &TagTree, target: ObjRef) -> Result<()> {
    if !tree.preserved_associations.is_empty() && orphan_objects(doc, tree)?.contains_key(&target) {
        return Err(invalid("metadata target is also referenced by a preserved ParentTree association"));
    }
    Ok(())
}

pub(super) fn preserve<T>(doc: &mut Document, operation: impl FnOnce(&mut Document) -> Result<T>) -> Result<T> {
    let tree = inspect(doc)?;
    if tree.preserved_associations.is_empty() {
        return operation(doc);
    }
    // Establish preservation bounds before allocating or mutating a candidate.
    orphan_objects(doc, &tree)?;
    let mut candidate = doc.clone();
    let result = operation(&mut candidate)?;
    validate_edit(doc, &tree, &candidate)?;
    *doc = candidate;
    Ok(result)
}

fn parent_values(doc: &Document, tree: &TagTree) -> Result<BTreeMap<i64, (Object, Object)>> {
    let Some(root) = tree.root else { return Ok(BTreeMap::new()) };
    Ok(parent_entries(doc, &dictionary(doc, root)?)?
        .into_iter()
        .map(|entry| {
            let resolved = doc.resolve(&entry.value);
            // A scalar entry owns the exact StructElem reference, not that tag's
            // mutable dictionary. Changing its title/K/P does not retarget the key.
            // Arrays still retain their container identity and ordered MCID slots.
            let ownership = if matches!(&*resolved, Object::Array(_)) { (*resolved).clone() } else { entry.value.clone() };
            (entry.key, (entry.value, ownership))
        })
        .collect())
}

pub(super) fn validate_edit(before: &Document, tree: &TagTree, after: &Document) -> Result<()> {
    // Inspection repeats the whole-object zero-claimant proof on the candidate.
    let after_tree = editable(after)?;
    if tree.preserved_associations.iter().any(|a| a.kind == ParentAssociationKind::UnclaimedScalar && after_tree.element(a.owner).is_none()) {
        return Err(invalid("edit would remove a protected owner of a globally unclaimed ParentTree scalar"));
    }
    if tree.preserved_associations != after_tree.preserved_associations || orphan_objects(before, tree)? != orphan_objects(after, &after_tree)? {
        return Err(invalid("edit would change a preserved ParentTree association or its owner/child objects"));
    }
    let original = parent_values(before, tree)?;
    let candidate = parent_values(after, &after_tree)?;
    let changed: BTreeSet<_> = original.keys().chain(candidate.keys()).copied().filter(|key| original.get(key) != candidate.get(key)).collect();
    if !changed.is_empty() {
        validate_key_claimants(before, tree, &changed)?;
    }
    Ok(())
}

/// Prove changed keys have one content-object owner, including claimants outside the
/// reachable tag tree. Nested direct dictionaries cannot be identified safely and
/// therefore cause refusal. Scanning all objects also catches unused Forms/annotations.
fn validate_key_claimants(doc: &Document, tree: &TagTree, changed: &BTreeSet<i64>) -> Result<()> {
    let mut expected = BTreeMap::new();
    for element in &tree.elements {
        for kid in &element.kids {
            let Some((Some(object), mcid)) = content_identity(kid) else { continue };
            let key_name = if mcid.is_some() { b"StructParents".as_slice() } else { b"StructParent" };
            let key = doc.get(object).as_dict().and_then(|d| d.int(key_name)).ok_or_else(|| invalid("missing ParentTree key on content object"))?;
            if changed.contains(&key) {
                let owner = (object, mcid.is_some());
                if expected.insert(key, owner).is_some_and(|previous| previous != owner) {
                    return Err(invalid("changed ParentTree key has conflicting reachable content objects"));
                }
            }
        }
    }
    if changed.iter().any(|key| !expected.contains_key(key)) {
        return Err(invalid("cannot change ParentTree keys without an exact reachable content object"));
    }
    scan_key_declarations(doc, |key, identity, array| {
        if changed.contains(&key) && expected.get(&key) != identity.map(|id| (id, array)).as_ref() {
            return Err(invalid(format!("ParentTree key {key} is also claimed by another or direct content object")));
        }
        Ok(())
    })?;
    Ok(())
}

fn unclaimed_scalar_keys(doc: &Document, candidates: &BTreeSet<i64>) -> Result<(BTreeSet<i64>, usize)> {
    let mut unclaimed = candidates.clone();
    let visits = scan_key_declarations(doc, |key, _, _| {
        unclaimed.remove(&key);
        Ok(())
    })?;
    Ok((unclaimed, visits))
}

/// Inspect every live object, including stream dictionaries and nested direct
/// containers. No unknown, indirect or unreadable declaration is silently ignored.
fn scan_key_declarations(doc: &Document, mut declaration: impl FnMut(i64, Option<ObjRef>, bool) -> Result<()>) -> Result<usize> {
    let numbers = doc.object_numbers();
    if numbers.len() > LIMIT {
        return Err(invalid("document too large to verify changed ParentTree key ownership"));
    }
    let mut count = 0usize;
    for num in numbers {
        let id = ObjRef::new(num, doc.generation(num));
        let value = doc.try_get(num).map_err(|e| invalid(format!("cannot verify ParentTree key ownership: {e}")))?;
        let mut stack = vec![(&*value, Some(id), 0usize)];
        while let Some((value, identity, depth)) = stack.pop() {
            count = count.saturating_add(1);
            if count > LIMIT || depth > DEPTH {
                return Err(invalid("ParentTree key ownership check exceeds safe limits"));
            }
            if let Some(d) = value.as_dict() {
                for (name, array) in [(b"StructParents".as_slice(), true), (b"StructParent".as_slice(), false)] {
                    if let Some(raw_key) = d.get(name) {
                        // An unresolved/malformed declaration could conceal a competing
                        // key. Only exact direct nonnegative integer keys are provable.
                        let key = raw_key
                            .as_int()
                            .filter(|key| *key >= 0)
                            .ok_or_else(|| invalid("cannot prove ownership with an indirect or malformed StructParent(s) key"))?;
                        declaration(key, identity, array)?;
                    }
                }
                if stack.len().saturating_add(count).saturating_add(d.len()) > LIMIT {
                    return Err(invalid("ParentTree key ownership check exceeds safe limits"));
                }
                stack.extend(d.iter().map(|(_, value)| (value, None, depth.saturating_add(1))));
            } else if let Object::Array(values) = value {
                if stack.len().saturating_add(count).saturating_add(values.len()) > LIMIT {
                    return Err(invalid("ParentTree key ownership check exceeds safe limits"));
                }
                stack.extend(values.iter().map(|value| (value, None, depth.saturating_add(1))));
            }
        }
    }
    Ok(count)
}

/// Artifact conversion changes painting content, so its selected exact MCIDs must
/// also be disjoint from claims in preserved orphan K graphs. Missing inherited
/// pages or unsupported orphan children are not evidence that content is disjoint.
pub(super) fn reject_content_overlap(doc: &Document, tree: &TagTree, affected: &BTreeSet<(ObjRef, i64)>) -> Result<()> {
    let mut stack = Vec::new();
    for owner in tree.preserved_associations.iter().map(|a| a.owner).collect::<BTreeSet<_>>() {
        stack.push((Object::Ref(owner), orphan_page(doc, tree, owner)?, 0usize));
    }
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    while let Some((raw, page, depth)) = stack.pop() {
        count = count.saturating_add(1);
        if count > LIMIT || depth > DEPTH || stack.len().saturating_add(count) > LIMIT {
            return Err(invalid("unreachable content ownership check exceeds safe limits"));
        }
        match classify(doc, &raw, page) {
            TagKid::Element { id } => {
                if !seen.insert((id, page)) {
                    continue;
                }
                let d = dictionary(doc, id)?;
                let page = d.reference(b"Pg").or(page);
                let kids = raw_kids(doc, &d)?;
                if stack.len().saturating_add(count).saturating_add(kids.len()) > LIMIT {
                    return Err(invalid("unreachable content ownership check exceeds safe limits"));
                }
                stack.extend(kids.into_iter().map(|kid| (kid, page, depth.saturating_add(1))));
            }
            TagKid::Objr { .. } => {}
            TagKid::Unsupported { .. } => return Err(invalid("cannot prove artifact content disjoint from unsupported unreachable children")),
            kid => {
                let Some((Some(object), Some(mcid))) = content_identity(&kid) else {
                    return Err(invalid("cannot prove artifact content disjoint from page-less unreachable children"));
                };
                if doc.generation(object.num) != object.generation || matches!(&*doc.get(object), Object::Null) {
                    return Err(invalid("cannot prove artifact content disjoint from stale unreachable content objects"));
                }
                match kid {
                    TagKid::Mcid { .. } | TagKid::Mcr { stream: None, .. } if !tree.pages.contains(&object) => {
                        return Err(invalid("cannot prove artifact content disjoint from an unreachable non-page namespace"));
                    }
                    TagKid::Mcr { stream: Some(_), .. } if !matches!(&*doc.get(object), Object::Stream(_)) => {
                        return Err(invalid("cannot prove artifact content disjoint from an unreachable non-stream namespace"));
                    }
                    _ => {}
                }
                if affected.contains(&(object, mcid)) {
                    return Err(invalid("artifact content is also claimed by a preserved unreachable tag"));
                }
            }
        }
    }
    Ok(())
}

fn orphan_page(doc: &Document, tree: &TagTree, owner: ObjRef) -> Result<Option<ObjRef>> {
    let mut current = Some(owner);
    let mut seen = BTreeSet::new();
    while let Some(id) = current {
        if Some(id) == tree.root {
            return Ok(None);
        }
        if seen.len() >= DEPTH || !seen.insert(id) {
            return Err(invalid("cannot establish inherited page of unreachable tag"));
        }
        let d = dictionary(doc, id)?;
        if let Some(page) = d.get(b"Pg") {
            return page.as_ref().map(Some).ok_or_else(|| invalid("unreachable tag has malformed Pg"));
        }
        if let Some(element) = tree.element(id) {
            return Ok(element.page);
        }
        current = match d.get(b"P") {
            Some(parent) => Some(parent.as_ref().ok_or_else(|| invalid("unreachable tag has malformed parent"))?),
            None => None,
        };
    }
    Ok(None)
}
