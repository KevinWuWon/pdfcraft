//! Exact storage ownership before changing a ParentTree number-tree container.
use super::*;

fn resolve_storage(doc: &Document, raw: &Object, storage: &mut BTreeSet<ObjRef>) -> Result<Object> {
    if let Some(id) = raw.as_ref() {
        if doc.generation(id.num) != id.generation {
            return Err(invalid("stale ParentTree storage generation"));
        }
        let object = doc.try_get(id.num).map_err(|_| invalid("unreadable ParentTree storage"))?;
        // Writes preserve direct and single-indirect storage identities. An indirect
        // reference-to-reference is readable but not silently flattened by an edit.
        if matches!(&*object, Object::Ref(_)) {
            return Err(invalid("indirect ParentTree storage chains cannot be rewritten safely"));
        }
        storage.insert(id);
    }
    namespaces::checked_resolve(doc, raw)
}

pub(super) fn validate_rewrite(doc: &Document, root: &Dict, dictionaries: &BTreeMap<ObjRef, Dict>, objects: &BTreeMap<ObjRef, Object>) -> Result<()> {
    let Some(start) = root.get(b"ParentTree") else { return Ok(()) };
    let mut storage = BTreeSet::new();
    let mut pending = vec![(start.clone(), 0usize, None)];
    let mut parents: BTreeMap<ObjRef, BTreeSet<ObjRef>> = BTreeMap::new();
    fn link(parents: &mut BTreeMap<ObjRef, BTreeSet<ObjRef>>, child: Option<ObjRef>, parent: Option<ObjRef>) -> Option<ObjRef> {
        if let (Some(child), Some(parent)) = (child, parent) {
            parents.entry(child).or_default().insert(parent);
        }
        child.or(parent)
    }
    let mut nodes = BTreeSet::new();
    let mut visited = 0usize;
    while let Some((raw, depth, parent)) = pending.pop() {
        visited = visited.saturating_add(1);
        if depth > DEPTH || visited.saturating_add(pending.len()) > LIMIT {
            return Err(invalid("ParentTree storage exceeds safe limits"));
        }
        if let Some(id) = raw.as_ref()
            && !nodes.insert(id)
        {
            return Err(invalid("cyclic or shared ParentTree node"));
        }
        let parent = link(&mut parents, raw.as_ref(), parent);
        let Object::Dict(node) = resolve_storage(doc, &raw, &mut storage)? else {
            return Err(invalid("ParentTree node is not a dictionary"));
        };
        if let Some(raw) = node.get(b"Kids") {
            let Object::Array(kids) = resolve_storage(doc, raw, &mut storage)? else {
                return Err(invalid("ParentTree Kids is not an array"));
            };
            if visited.saturating_add(pending.len()).saturating_add(kids.len()) > LIMIT {
                return Err(invalid("ParentTree storage exceeds safe limits"));
            }
            let parent = link(&mut parents, raw.as_ref(), parent);
            pending.extend(kids.into_iter().map(|kid| (kid, depth.saturating_add(1), parent)));
        }
        if let Some(raw) = node.get(b"Nums") {
            let Object::Array(nums) = resolve_storage(doc, raw, &mut storage)? else {
                return Err(invalid("ParentTree Nums is not an array"));
            };
            if nums.len() % 2 != 0 {
                return Err(invalid("invalid ParentTree Nums length"));
            }
            let parent = link(&mut parents, raw.as_ref(), parent);
            for pair in nums.as_chunks::<2>().0 {
                let Some(value) = pair.get(1) else {
                    return Err(invalid("missing ParentTree value"));
                };
                let resolved = namespaces::checked_resolve(doc, value)?;
                if matches!(resolved, Object::Array(_)) {
                    resolve_storage(doc, value, &mut storage)?;
                    link(&mut parents, value.as_ref(), parent);
                }
            }
        }
    }
    // Only storage that would actually change needs exclusive ownership. Scalar
    // StructElem values are deliberately excluded: their many links are expected.
    let mut touched = BTreeSet::new();
    for (id, value) in objects {
        if storage.contains(id) && *doc.try_get(id.num).map_err(|_| invalid("unreadable ParentTree storage"))? != *value {
            touched.insert(*id);
        }
    }
    for (id, new) in dictionaries {
        let old = doc.try_get(id.num).map_err(|_| invalid("unreadable dictionary before ParentTree rewrite"))?;
        let Some(old_dict) = old.as_dict() else { continue };
        let mut normalized = new.clone();
        for key in [b"Nums".as_slice(), b"Kids"] {
            if let Some(array_id) = old_dict.reference(key)
                && storage.contains(&array_id)
            {
                let old_array = doc.try_get(array_id.num).map_err(|_| invalid("unreadable ParentTree array"))?;
                if let Some(Object::Array(values)) = new.get(key) {
                    let value = Object::Array(values.clone());
                    if *old_array != value {
                        touched.insert(array_id);
                    }
                    normalized.set(key, Object::Ref(array_id));
                }
            }
        }
        if storage.contains(id) && Object::Dict(normalized) != *old {
            touched.insert(*id);
        }
    }
    if touched.is_empty() {
        return Ok(());
    }
    // An alias to an unchanged number-tree ancestor still observes a changed
    // descendant array. Include the storage path, excluding semantic owner tags.
    let mut ancestors: Vec<_> = touched.iter().copied().collect();
    while let Some(child) = ancestors.pop() {
        if let Some(owners) = parents.get(&child) {
            for owner in owners {
                if touched.insert(*owner) {
                    ancestors.push(*owner);
                }
            }
        }
        if touched.len().saturating_add(ancestors.len()) > LIMIT.saturating_mul(2) {
            return Err(invalid("ParentTree storage ancestry exceeds safe limits"));
        }
    }
    validate_unique(doc, &touched, "shared ParentTree storage cannot be rewritten safely")
}

pub(super) fn validate_k_targets(doc: &Document, ids: &[ObjRef]) -> Result<()> {
    let mut touched = BTreeSet::new();
    for id in ids {
        let dict = dictionary(doc, *id)?;
        if let Some(array) = dict.reference(b"K") {
            let value = namespaces::checked_resolve(doc, &Object::Ref(array))?;
            if matches!(value, Object::Array(_)) {
                if !matches!(&*doc.try_get(array.num).map_err(|_| invalid("unreadable structure K storage"))?, Object::Array(_)) {
                    return Err(invalid("indirect structure K storage chains cannot be rewritten safely"));
                }
                touched.insert(array);
            }
        }
    }
    validate_unique(doc, &touched, "shared structure K storage cannot be rewritten safely")
}

pub(super) fn validate_k_rewrites(doc: &Document, dictionaries: &BTreeMap<ObjRef, Dict>) -> Result<()> {
    let mut ids = Vec::new();
    for (id, new) in dictionaries {
        let old = dictionary(doc, *id)?;
        if let Some(array) = old.reference(b"K") {
            let value = namespaces::checked_resolve(doc, &Object::Ref(array))?;
            if let Object::Array(values) = value {
                let changed = match new.get(b"K") {
                    Some(Object::Array(next)) => next != &values,
                    None => !values.is_empty(),
                    Some(Object::Ref(next)) if *next == array => false,
                    _ => true,
                };
                if changed {
                    ids.push(*id);
                }
            }
        }
    }
    validate_k_targets(doc, &ids)
}

fn validate_unique(doc: &Document, touched: &BTreeSet<ObjRef>, message: &str) -> Result<()> {
    if touched.is_empty() {
        return Ok(());
    }
    let mut counts: BTreeMap<ObjRef, usize> = touched.iter().map(|id| (*id, 0)).collect();
    scan_objects(doc, &mut counts)?;
    if counts.values().any(|count| *count != 1) {
        return Err(invalid(message));
    }
    Ok(())
}

/// Count direct containers too: a single indirect object can contain many values.
pub(super) fn scan_work(doc: &Document) -> Result<usize> {
    scan_objects(doc, &mut BTreeMap::new())
}

fn scan_objects(doc: &Document, counts: &mut BTreeMap<ObjRef, usize>) -> Result<usize> {
    let mut work = 0usize;
    fn walk(value: &Object, counts: &mut BTreeMap<ObjRef, usize>, work: &mut usize) -> Result<()> {
        let mut stack = vec![(value, 0usize)];
        while let Some((value, depth)) = stack.pop() {
            *work = work.saturating_add(1);
            if *work > LIMIT.saturating_mul(32) || depth > DEPTH {
                return Err(invalid("ParentTree ownership scan exceeds safe limits"));
            }
            match value {
                Object::Ref(id) => {
                    // A stale alias also conflicts with writing the live storage object.
                    for (target, count) in counts.range_mut(ObjRef::new(id.num, 0)..=ObjRef::new(id.num, u16::MAX)) {
                        *count = count.saturating_add(if target.generation == id.generation { 1 } else { 2 });
                    }
                }
                Object::Array(values) => {
                    if work.saturating_add(stack.len()).saturating_add(values.len()) > LIMIT.saturating_mul(32) {
                        return Err(invalid("ParentTree ownership scan exceeds safe limits"));
                    }
                    stack.extend(values.iter().map(|v| (v, depth.saturating_add(1))));
                }
                Object::Dict(dict) | Object::Stream(pdfcraft_cos::Stream { dict, .. }) => {
                    if work.saturating_add(stack.len()).saturating_add(dict.iter().count()) > LIMIT.saturating_mul(32) {
                        return Err(invalid("ParentTree ownership scan exceeds safe limits"));
                    }
                    stack.extend(dict.iter().map(|(_, v)| (v, depth.saturating_add(1))));
                }
                _ => {}
            }
        }
        Ok(())
    }
    let numbers = doc.object_numbers();
    if numbers.len() > LIMIT.saturating_mul(32) {
        return Err(invalid("ParentTree ownership scan exceeds safe limits"));
    }
    for number in numbers {
        let value = doc.try_get(number).map_err(|_| invalid("unreadable object prevents proving ParentTree storage ownership"))?;
        walk(&value, counts, &mut work)?;
    }
    for (_, value) in doc.trailer().iter() {
        walk(value, counts, &mut work)?;
    }
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rejects(mut doc: Document, child: ObjRef) {
        let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(delete(&mut doc, child, DeleteMode::Subtree).is_err());
        assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
    }
    #[test]
    fn parent_tree_stream_nodes_and_stale_storage_refuse_atomically() {
        for mode in 0..5 {
            let (mut doc, _, _, child, _, pt) = super::super::tests::fixture();
            let original = dictionary(&doc, pt).unwrap();
            match mode {
                0 => {
                    doc.set(pt, Object::Stream(pdfcraft_cos::Stream::from_raw(original, b"private bytes".to_vec())));
                }
                1 => {
                    let nums = doc.add(original.get(b"Nums").unwrap().clone());
                    doc.update_dict(pt, |d| d.set(b"Nums", Object::Ref(ObjRef::new(nums.num, 1)))).unwrap();
                }
                2 => {
                    let leaf = doc.add(original);
                    let kids = doc.add(Object::Array(vec![Object::Ref(leaf)]));
                    doc.update_dict(pt, |d| {
                        d.remove(b"Nums");
                        d.set(b"Kids", Object::Ref(ObjRef::new(kids.num, 1)));
                    })
                    .unwrap();
                }
                3 => {
                    let slots = doc.add(Object::Array(vec![Object::Ref(child)]));
                    doc.update_dict(pt, |d| d.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Ref(ObjRef::new(slots.num, 1))]))).unwrap();
                }
                _ => {
                    let nums = doc.add(original.get(b"Nums").unwrap().clone());
                    let chain = doc.add(Object::Ref(ObjRef::new(nums.num, 1)));
                    doc.update_dict(pt, |d| d.set(b"Nums", Object::Ref(chain))).unwrap();
                }
            }
            rejects(doc, child);
        }
    }
    #[test]
    fn parent_storage_unknown_dictionary_stream_and_trailer_aliases_refuse_atomically() {
        for mode in 0..5 {
            let (mut doc, _, _, child, _, pt) = super::super::tests::fixture();
            let original = dictionary(&doc, pt).unwrap();
            let nums = doc.add(original.get(b"Nums").unwrap().clone());
            if mode != 4 {
                doc.update_dict(pt, |d| d.set(b"Nums", Object::Ref(nums))).unwrap();
            }
            let target = if mode >= 3 { pt } else { nums };
            let mut alias = Dict::new();
            alias.set(b"Private", Object::Ref(target));
            match mode {
                1 => {
                    doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(alias, b"opaque".to_vec())));
                }
                2 => {
                    doc.trailer_mut().set(b"Private", Object::Ref(target));
                }
                _ => {
                    doc.add(alias);
                }
            }
            rejects(doc, child);
        }
    }
    #[test]
    fn private_aliases_to_unchanged_nested_ancestors_refuse() {
        for level in 0..3 {
            let (mut doc, _, _, child, _, pt) = super::super::tests::fixture();
            let leaf = doc.add(dictionary(&doc, pt).unwrap());
            let kids = doc.add(Object::Array(vec![Object::Ref(leaf)]));
            doc.update_dict(pt, |d| {
                d.remove(b"Nums");
                d.set(b"Kids", Object::Ref(kids));
            })
            .unwrap();
            let target = [pt, kids, leaf][level];
            let mut private = Dict::new();
            private.set(b"Opaque", Object::Ref(target));
            doc.add(private);
            rejects(doc, child);
        }
    }
    #[test]
    fn bulk_delete_accounts_for_direct_container_scan_work() {
        let (mut doc, _, group, child, _, _) = super::super::tests::fixture();
        let other = create(&mut doc, Some(group), 1, &TagProperties::default()).unwrap();
        doc.add(Object::Array(vec![Object::Null; DELETE_BATCH_WORK_LIMIT / 9]));
        let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        let error = delete_tags(&mut doc, &[child, other], DeleteMode::Subtree).unwrap_err();
        assert!(error.to_string().contains("estimated tag/content/object visits"));
        assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
    }
    #[test]
    fn pruning_direct_empty_number_nodes_preserves_private_data_or_refuses_atomically() {
        for private in [false, true] {
            let (mut doc, _, _, child, page, pt) = super::super::tests::fixture();
            let mut annotation = Dict::new();
            annotation.set(b"Type", Object::name("Annot"));
            annotation.set(b"Subtype", Object::name("Link"));
            annotation.set(b"P", Object::Ref(page));
            annotation.set(b"StructParent", Object::Int(1));
            annotation.set(b"Rect", Object::Array(vec![Object::Int(0), Object::Int(0), Object::Int(10), Object::Int(10)]));
            let annotation = doc.add(annotation);
            doc.update_dict(page, |d| {
                d.remove(b"StructParents");
                d.set(b"Annots", Object::Array(vec![Object::Ref(annotation)]));
            })
            .unwrap();
            let mut objr = Dict::new();
            objr.set(b"Type", Object::name("OBJR"));
            objr.set(b"Obj", Object::Ref(annotation));
            objr.set(b"Pg", Object::Ref(page));
            doc.update_dict(child, |d| d.set(b"K", Object::Dict(objr))).unwrap();
            let mut leaf = Dict::new();
            leaf.set(b"Nums", Object::Array(vec![Object::Int(1), Object::Ref(child)]));
            leaf.set(b"Limits", Object::Array(vec![Object::Int(1), Object::Int(1)]));
            if private {
                leaf.set(b"Private", Object::Int(73));
            }
            doc.update_dict(pt, |d| {
                d.remove(b"Nums");
                d.set(b"Kids", Object::Array(vec![Object::Dict(leaf)]));
            })
            .unwrap();
            assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
            let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
            let result = delete(&mut doc, child, DeleteMode::Subtree);
            if private {
                assert!(result.unwrap_err().to_string().contains("unknown properties"));
                assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
            } else {
                result.unwrap();
                assert!(!dictionary(&doc, annotation).unwrap().contains(b"StructParent"));
                assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
            }
        }
    }
    #[test]
    fn direct_parent_node_keeps_indirect_nums_identity_on_rewrite() {
        let (mut doc, _, _, child, _, pt) = super::super::tests::fixture();
        let root = inspect(&doc).unwrap().root.unwrap();
        let mut node = dictionary(&doc, pt).unwrap();
        let nums = doc.add(node.get(b"Nums").unwrap().clone());
        node.set(b"Nums", Object::Ref(nums));
        node.set(b"Private", Object::Int(73));
        doc.update_dict(root, |d| d.set(b"ParentTree", Object::Dict(node))).unwrap();
        delete(&mut doc, child, DeleteMode::Subtree).unwrap();
        let rd = dictionary(&doc, root).unwrap();
        let node = rd.get(b"ParentTree").unwrap().as_dict().unwrap();
        assert_eq!(node.reference(b"Nums"), Some(nums));
        assert_eq!(node.int(b"Private"), Some(73));
        assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
        let saved = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(inspect(&Document::open(std::sync::Arc::new(saved)).unwrap()).unwrap().edit_blockers.is_empty());
    }
    #[test]
    fn unchanged_shared_number_storage_is_not_a_rewrite_conflict() {
        let (mut doc, _, group, _, _, pt) = super::super::tests::fixture();
        let original = dictionary(&doc, pt).unwrap();
        let nums = doc.add(original.get(b"Nums").unwrap().clone());
        doc.update_dict(pt, |d| d.set(b"Nums", Object::Ref(nums))).unwrap();
        let mut alias = Dict::new();
        alias.set(b"Private", Object::Ref(nums));
        doc.add(alias);
        let new = create(&mut doc, Some(group), 1, &TagProperties { role: Some("P".into()), ..Default::default() }).unwrap();
        let before = doc.get(nums);
        delete(&mut doc, new, DeleteMode::Subtree).unwrap();
        assert_eq!(doc.get(nums), before);
    }
    #[test]
    fn shared_k_storage_refuses_before_allocation_or_any_partial_edit() {
        for alias_kind in 0..4 {
            for action in 0..5 {
                let (mut doc, top, group, child, _, _) = super::super::tests::fixture();
                let owner = if action == 4 { child } else { group };
                let array = doc.add(Object::Array(raw_kids(&doc, &dictionary(&doc, owner).unwrap()).unwrap()));
                doc.update_dict(owner, |d| d.set(b"K", Object::Ref(array))).unwrap();
                let alias_ref = if alias_kind == 3 { ObjRef::new(array.num, 1) } else { array };
                let mut alias = Dict::new();
                alias.set(b"Private", Object::Ref(alias_ref));
                match alias_kind {
                    1 => {
                        doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(alias, b"opaque".to_vec())));
                    }
                    2 => {
                        doc.trailer_mut().set(b"Private", Object::Ref(alias_ref));
                    }
                    _ => {
                        doc.add(alias);
                    }
                }
                assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
                let before = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
                let numbers = doc.object_numbers();
                let result = match action {
                    0 => create(&mut doc, Some(group), 1, &TagProperties::default()).map(|_| ()),
                    1 => move_tag(&mut doc, child, Some(top), 1),
                    2 => move_tags(&mut doc, &[child], Some(top), 1),
                    3 => delete(&mut doc, child, DeleteMode::Subtree),
                    _ => move_content(&mut doc, child, 0, group, 1),
                };
                assert!(result.is_err(), "alias {alias_kind}, action {action}");
                assert_eq!(pdfcraft_cos::write_full(&doc, &Default::default()).unwrap(), before);
                assert_eq!(doc.object_numbers(), numbers, "refusal allocated an object");
                let old_array = doc.get(array);
                update(&mut doc, owner, &TagProperties { title: Some(Some("Metadata only".into())), ..Default::default() }).unwrap();
                assert_eq!(doc.get(array), old_array);
            }
        }
    }
}
