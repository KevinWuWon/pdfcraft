//! Contributor-original synthetic object graphs, recovered from retained test source.
//! Recovery requires fresh test execution; historical results do not validate this file.
use super::*;

struct Fixture {
    doc: Document,
    top: ObjRef,
    group: ObjRef,
    child: ObjRef,
    sibling: ObjRef,
    page: ObjRef,
    parent_tree: ObjRef,
    orphan: ObjRef,
}
fn props(role: &str) -> TagProperties {
    TagProperties { role: Some(role.into()), ..Default::default() }
}
fn bytes(doc: &Document) -> Vec<u8> {
    pdfcraft_cos::write_full(doc, &Default::default()).unwrap()
}
fn fixture() -> Fixture {
    let mut doc = Document::new_empty();
    let top = create(&mut doc, None, 0, &props("Document")).unwrap();
    let group = create(&mut doc, Some(top), 0, &props("Sect")).unwrap();
    let child = create(&mut doc, Some(group), 0, &props("P")).unwrap();
    let sibling = create(&mut doc, Some(group), 1, &props("P")).unwrap();
    let pages = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
    let mut pd = Dict::new();
    pd.set(b"Type", Object::name("Page"));
    pd.set(b"Parent", Object::Ref(pages));
    pd.set(b"StructParents", 0);
    let page = doc.add(pd);
    doc.update_dict(pages, |d| {
        d.set(b"Kids", Object::Array(vec![Object::Ref(page)]));
        d.set(b"Count", 1);
    })
    .unwrap();
    doc.update_dict(group, |d| d.set(b"Pg", Object::Ref(page))).unwrap();
    doc.update_dict(child, |d| d.set(b"K", 0)).unwrap();
    let mut orphan = Dict::new();
    orphan.set(b"Type", Object::name("StructElem"));
    orphan.set(b"S", Object::name("P"));
    orphan.set(b"P", Object::Ref(group));
    orphan.set(b"Pg", Object::Ref(page));
    orphan.set(b"K", 1);
    orphan.set(b"Private", Object::String(PdfString::text("retain verbatim")));
    let orphan = doc.add(orphan);
    let mut pt = Dict::new();
    pt.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child), Object::Ref(orphan)])]));
    let parent_tree = doc.add(pt);
    let root = inspect(&doc).unwrap().root.unwrap();
    doc.update_dict(root, |d| d.set(b"ParentTree", Object::Ref(parent_tree))).unwrap();
    Fixture { doc, top, group, child, sibling, page, parent_tree, orphan }
}
fn slot(doc: &Document, parent_tree: ObjRef, key: i64, mcid: usize) -> Object {
    let entries = parent_entries(doc, doc.get(inspect(doc).unwrap().root.unwrap()).as_dict().unwrap()).unwrap();
    assert!(doc.get(parent_tree).as_dict().is_some());
    let entry = entries.iter().find(|e| e.key == key).unwrap();
    doc.resolve(&entry.value).as_array().unwrap().get(mcid).unwrap().clone()
}

#[test]
fn preserved_same_key_orphans_allow_safe_structural_edits_and_roundtrip() {
    let Fixture { mut doc, group, child, sibling, parent_tree, orphan, .. } = fixture();
    let before = doc.get(orphan);
    let tree = inspect(&doc).unwrap();
    assert!(!tree.issues.is_empty());
    assert!(tree.edit_blockers.is_empty(), "{:?}", tree.edit_blockers);
    assert_eq!(
        tree.preserved_associations,
        vec![ParentAssociation { kind: ParentAssociationKind::UnreachableOwner, key: 0, mcid: Some(1), owner: orphan }]
    );
    create(&mut doc, Some(group), 2, &props("Span")).unwrap();
    update(&mut doc, child, &props("Span")).unwrap();
    move_tag(&mut doc, child, Some(group), 1).unwrap();
    move_content(&mut doc, child, 0, sibling, 0).unwrap();
    assert_eq!(slot(&doc, parent_tree, 0, 0), Object::Ref(sibling));
    delete(&mut doc, sibling, DeleteMode::Subtree).unwrap();
    assert_eq!(slot(&doc, parent_tree, 0, 0), Object::Null);
    assert_eq!(slot(&doc, parent_tree, 0, 1), Object::Ref(orphan));
    assert_eq!(doc.get(orphan), before);
    let reopened = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
    let reopened_tree = inspect(&reopened).unwrap();
    assert!(reopened_tree.edit_blockers.is_empty());
    assert_eq!(reopened_tree.preserved_associations.len(), 1);
    let association = reopened_tree.preserved_associations.first().unwrap();
    assert_eq!((association.key, association.mcid), (0, Some(1)));
    assert!(reopened_tree.element(association.owner).is_none());
    // Full saves densely renumber objects. Compare the complete owner dictionary
    // after mapping its two references to the unique surviving section and page.
    let groups: Vec<_> = reopened_tree.elements.iter().filter(|e| e.role == "Sect").collect();
    assert_eq!(groups.len(), 1);
    assert_eq!(reopened_tree.pages.len(), 1);
    let mut expected = before.as_dict().unwrap().clone();
    expected.set(b"P", Object::Ref(groups.first().unwrap().id));
    expected.set(b"Pg", Object::Ref(*reopened_tree.pages.first().unwrap()));
    assert_eq!(&*reopened.get(association.owner), &Object::Dict(expected));
}

#[test]
fn unwrap_preserves_orphan_whose_parent_is_the_removed_tag() {
    let Fixture { mut doc, top, group, child, parent_tree, orphan, .. } = fixture();
    let before = doc.get(orphan);
    delete(&mut doc, group, DeleteMode::Unwrap).unwrap();
    assert_eq!(inspect(&doc).unwrap().element(child).unwrap().parent, Some(top));
    assert_eq!(slot(&doc, parent_tree, 0, 1), Object::Ref(orphan));
    assert_eq!(doc.get(orphan), before);
    assert_eq!(doc.get(orphan).as_dict().unwrap().reference(b"P"), Some(group));
}

#[test]
fn stale_dangling_and_non_structure_orphan_owners_remain_blocking() {
    for case in 0..4 {
        let Fixture { mut doc, group, orphan, .. } = fixture();
        match case {
            0 => doc.set(ObjRef::new(orphan.num, orphan.generation + 1), (*doc.get(orphan)).clone()),
            1 => doc.free(orphan),
            2 => {
                doc.update_dict(orphan, |d| d.set(b"Type", Object::name("Annot"))).unwrap();
            }
            _ => {
                doc.update_dict(orphan, |d| {
                    d.remove(b"S");
                })
                .unwrap();
            }
        }
        let before = bytes(&doc);
        assert!(!inspect(&doc).unwrap().edit_blockers.is_empty(), "case {case}");
        assert!(create(&mut doc, Some(group), 2, &props("P")).is_err(), "case {case}");
        assert_eq!(bytes(&doc), before);
    }
}

#[test]
fn extra_slot_pointing_to_reachable_owner_is_not_preservable() {
    let Fixture { mut doc, child, group, parent_tree, .. } = fixture();
    doc.update_dict(parent_tree, |d| d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child), Object::Ref(child)])])))
        .unwrap();
    let tree = inspect(&doc).unwrap();
    assert!(tree.preserved_associations.is_empty());
    assert!(!tree.edit_blockers.is_empty());
    let before = bytes(&doc);
    assert!(move_tag(&mut doc, child, Some(group), 1).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn scalar_object_orphan_and_its_annotation_are_untouched() {
    let Fixture { mut doc, child, sibling, page, parent_tree, orphan, .. } = fixture();
    let mut annotation = Dict::new();
    annotation.set(b"Type", Object::name("Annot"));
    annotation.set(b"Subtype", Object::name("Link"));
    annotation.set(b"StructParent", 7);
    let annotation = doc.add(annotation);
    let mut objr = Dict::new();
    objr.set(b"Type", Object::name("OBJR"));
    objr.set(b"Obj", Object::Ref(annotation));
    objr.set(b"Pg", Object::Ref(page));
    doc.update_dict(orphan, |d| d.set(b"K", Object::Dict(objr))).unwrap();
    doc.update_dict(parent_tree, |d| {
        d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child)]), 7.into(), Object::Ref(orphan)]))
    })
    .unwrap();
    let before_owner = doc.get(orphan);
    let before_annotation = doc.get(annotation);
    move_content(&mut doc, child, 0, sibling, 0).unwrap();
    delete(&mut doc, sibling, DeleteMode::Subtree).unwrap();
    let tree = inspect(&doc).unwrap();
    assert_eq!(
        tree.preserved_associations,
        vec![ParentAssociation { kind: ParentAssociationKind::UnreachableOwner, key: 7, mcid: None, owner: orphan }]
    );
    assert_eq!(doc.get(orphan), before_owner);
    assert_eq!(doc.get(annotation), before_annotation);
}

#[test]
fn conflicting_unreachable_page_form_annotation_and_direct_claimants_refuse_key_changes() {
    for kind in 0..4 {
        let Fixture { mut doc, group, child, sibling, .. } = fixture();
        let mut claimant = Dict::new();
        match kind {
            0 => {
                claimant.set(b"Type", Object::name("Page"));
                claimant.set(b"StructParents", 0);
            }
            1 => {
                claimant.set(b"Type", Object::name("XObject"));
                claimant.set(b"Subtype", Object::name("Form"));
                claimant.set(b"StructParents", 0);
            }
            2 => {
                claimant.set(b"Type", Object::name("Annot"));
                claimant.set(b"StructParent", 0);
            }
            _ => {
                claimant.set(b"StructParents", 0);
            }
        }
        if kind == 1 {
            doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(claimant, Vec::new())));
        } else if kind == 3 {
            let mut outer = Dict::new();
            outer.set(b"Private", Object::Dict(claimant));
            doc.add(outer);
        } else {
            doc.add(claimant);
        }
        // No association rewrite: reordering remains safe with an untouched conflict.
        move_tag(&mut doc, child, Some(group), 1).unwrap();
        let before = bytes(&doc);
        assert!(move_content(&mut doc, child, 0, sibling, 0).is_err(), "kind {kind}");
        assert_eq!(bytes(&doc), before);
        assert!(delete(&mut doc, child, DeleteMode::Subtree).is_err(), "kind {kind}");
        assert_eq!(bytes(&doc), before);
    }
}

#[test]
fn shared_orphan_parent_arrays_can_be_preserved_without_rewriting_them() {
    let Fixture { mut doc, group, child, parent_tree, orphan, .. } = fixture();
    let array = doc.add(Object::Array(vec![Object::Ref(orphan)]));
    doc.update_dict(parent_tree, |d| {
        d.set(
            b"Nums",
            Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child)]), 8.into(), Object::Ref(array), 9.into(), Object::Ref(array)]),
        )
    })
    .unwrap();
    let original = doc.get(array);
    let tree = inspect(&doc).unwrap();
    assert_eq!(tree.preserved_associations.len(), 2);
    create(&mut doc, Some(group), 2, &props("P")).unwrap();
    delete(&mut doc, child, DeleteMode::Subtree).unwrap();
    assert_eq!(doc.get(array), original);
    assert_eq!(inspect(&doc).unwrap().preserved_associations, tree.preserved_associations);
}

#[test]
fn orphan_alias_of_reachable_k_array_refuses_mutation_atomically() {
    let Fixture { mut doc, group, child, sibling, orphan, .. } = fixture();
    let array = doc.add(Object::Array(vec![Object::Ref(child), Object::Ref(sibling)]));
    doc.update_dict(group, |d| d.set(b"K", Object::Ref(array))).unwrap();
    doc.update_dict(orphan, |d| d.set(b"K", Object::Ref(array))).unwrap();
    let before = bytes(&doc);
    assert!(create(&mut doc, Some(group), 2, &props("P")).is_err());
    assert_eq!(bytes(&doc), before);
    assert!(move_tag(&mut doc, child, Some(group), 1).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn orphan_alias_of_indirect_content_dictionary_refuses_content_mutation() {
    let Fixture { mut doc, child, sibling, page, orphan, .. } = fixture();
    let mut mcr = Dict::new();
    mcr.set(b"Type", Object::name("MCR"));
    mcr.set(b"MCID", 0);
    // No explicit Pg initially; move_content pins it, exposing the alias mutation.
    let mcr = doc.add(mcr);
    doc.update_dict(child, |d| d.set(b"K", Object::Ref(mcr))).unwrap();
    doc.update_dict(orphan, |d| {
        d.set(b"K", Object::Ref(mcr));
        d.set(b"Pg", Object::Ref(page));
    })
    .unwrap();
    let before = bytes(&doc);
    assert!(move_content(&mut doc, child, 0, sibling, 0).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn stale_orphan_child_generation_refuses_even_otherwise_safe_edits() {
    let Fixture { mut doc, group, orphan, .. } = fixture();
    let array = doc.add(Object::Array(vec![1.into()]));
    doc.update_dict(orphan, |d| d.set(b"K", Object::Ref(ObjRef::new(array.num, array.generation + 1)))).unwrap();
    let before = bytes(&doc);
    assert!(create(&mut doc, Some(group), 2, &props("P")).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn artifact_conversion_preserves_unrelated_stale_slot_and_checks_hidden_key_conflicts() {
    for conflict in [false, true] {
        let Fixture { mut doc, child, page, parent_tree, orphan, .. } = fixture();
        let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), b"/P <</MCID 0>> BDC 0 0 10 10 re f EMC".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
        if conflict {
            let mut other = Dict::new();
            other.set(b"StructParents", 0);
            doc.add(other);
        }
        let original = bytes(&doc);
        let original_orphan = doc.get(orphan);
        let result = crate::artifact::convert(&mut doc, child, false);
        if conflict {
            assert!(result.is_err());
            assert_eq!(bytes(&doc), original);
        } else {
            result.unwrap();
            assert_eq!(doc.get(orphan), original_orphan);
            assert_eq!(slot(&doc, parent_tree, 0, 1), Object::Ref(orphan));
            assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
        }
    }
}

#[test]
fn missing_orphan_parent_is_preserved_without_synthesizing_a_backlink() {
    let Fixture { mut doc, group, orphan, .. } = fixture();
    doc.update_dict(orphan, |d| {
        d.remove(b"P");
    })
    .unwrap();
    let before = doc.get(orphan);
    assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
    create(&mut doc, Some(group), 2, &props("P")).unwrap();
    assert_eq!(doc.get(orphan), before);
}

#[test]
fn artifact_conversion_refuses_an_orphan_claim_on_the_selected_mcid() {
    let Fixture { mut doc, child, page, orphan, .. } = fixture();
    let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), b"/P <</MCID 0>> BDC 0 0 10 10 re f EMC".to_vec())));
    doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
    // Stale ParentTree slot 1 points at a tag whose K nevertheless claims MCID 0.
    doc.update_dict(orphan, |d| d.set(b"K", 0)).unwrap();
    let before = bytes(&doc);
    assert!(crate::artifact::convert(&mut doc, child, false).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn orphan_objr_alias_refuses_clearing_its_annotations_struct_parent() {
    let Fixture { mut doc, child, page, parent_tree, orphan, .. } = fixture();
    let mut annotation = Dict::new();
    annotation.set(b"Type", Object::name("Annot"));
    annotation.set(b"Subtype", Object::name("Link"));
    annotation.set(b"StructParent", 2);
    let annotation = doc.add(annotation);
    let mut objr = Dict::new();
    objr.set(b"Type", Object::name("OBJR"));
    objr.set(b"Obj", Object::Ref(annotation));
    objr.set(b"Pg", Object::Ref(page));
    doc.update_dict(child, |d| d.set(b"K", Object::Dict(objr.clone()))).unwrap();
    doc.update_dict(orphan, |d| d.set(b"K", Object::Dict(objr))).unwrap();
    doc.update_dict(parent_tree, |d| d.set(b"Nums", Object::Array(vec![2.into(), Object::Ref(child), 3.into(), Object::Ref(orphan)]))).unwrap();
    let before = bytes(&doc);
    assert!(delete(&mut doc, child, DeleteMode::Subtree).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn artifact_conversion_refuses_malformed_orphan_page_and_stream_namespaces() {
    for stream_namespace in [false, true] {
        let Fixture { mut doc, child, page, orphan, .. } = fixture();
        let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), b"/P <</MCID 0>> BDC 0 0 10 10 re f EMC".to_vec())));
        doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
        let wrong_namespace = doc.add(Dict::new());
        if stream_namespace {
            let mut mcr = Dict::new();
            mcr.set(b"Type", Object::name("MCR"));
            mcr.set(b"MCID", 1);
            mcr.set(b"Stm", Object::Ref(wrong_namespace));
            doc.update_dict(orphan, |d| d.set(b"K", Object::Dict(mcr))).unwrap();
        } else {
            doc.update_dict(orphan, |d| d.set(b"Pg", Object::Ref(wrong_namespace))).unwrap();
        }
        let before = bytes(&doc);
        assert!(crate::artifact::convert(&mut doc, child, false).is_err(), "stream namespace: {stream_namespace}");
        assert_eq!(bytes(&doc), before);
    }
}

#[test]
fn metadata_rejects_orphan_child_alias_without_blocking_unrelated_metadata() {
    let Fixture { mut doc, top, group, child, sibling, orphan, .. } = fixture();
    let array = doc.add(Object::Array(vec![Object::Ref(child), Object::Ref(sibling)]));
    doc.update_dict(group, |d| d.set(b"K", Object::Ref(array))).unwrap();
    doc.update_dict(orphan, |d| d.set(b"K", Object::Ref(array))).unwrap();
    let root = inspect(&doc).unwrap().root.unwrap();
    let mut broken_map = Dict::new();
    broken_map.set(b"Broken", Object::name("Unmapped"));
    doc.update_dict(root, |d| d.set(b"RoleMap", Object::Dict(broken_map))).unwrap();
    let metadata = TagProperties { title: Some(Some("A title".into())), ..Default::default() };
    let before = bytes(&doc);
    assert!(update(&mut doc, child, &metadata).is_err());
    assert_eq!(bytes(&doc), before);
    let original_orphan = doc.get(orphan);
    update(&mut doc, top, &metadata).unwrap();
    assert_eq!(inspect(&doc).unwrap().element(top).unwrap().title.as_deref(), Some("A title"));
    assert_eq!(doc.get(orphan), original_orphan);
}

#[test]
fn bulk_delete_budget_includes_preserved_slots_and_orphan_child_walks() {
    for many_slots in [false, true] {
        let Fixture { mut doc, group, child, parent_tree, orphan, .. } = fixture();
        let mut selected = Vec::new();
        for index in 0..30 {
            selected.push(create(&mut doc, Some(group), index + 2, &props("P")).unwrap());
        }
        if many_slots {
            let mut slots = vec![Object::Ref(orphan); 50_000];
            *slots.first_mut().unwrap() = Object::Ref(child);
            doc.update_dict(parent_tree, |d| d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(slots)]))).unwrap();
        } else {
            doc.update_dict(orphan, |d| d.set(b"K", Object::Array(vec![1.into(); 50_000]))).unwrap();
        }
        let before = bytes(&doc);
        let error = delete_tags(&mut doc, &selected, DeleteMode::Subtree).unwrap_err();
        assert!(error.to_string().contains("deletion batch exceeds"), "{error}");
        assert_eq!(bytes(&doc), before);
    }
}

fn unclaimed_scalar_fixture() -> Fixture {
    let f = fixture();
    let Fixture { mut doc, top, group, child, sibling, page, parent_tree, orphan } = f;
    doc.update_dict(parent_tree, |d| {
        d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child)]), 7.into(), Object::Ref(sibling)]));
    })
    .unwrap();
    Fixture { doc, top, group, child, sibling, page, parent_tree, orphan }
}

#[test]
fn globally_unclaimed_scalar_allows_unrelated_edits_without_repairing_the_entry() {
    let Fixture { mut doc, top, group, child, sibling, parent_tree, .. } = unclaimed_scalar_fixture();
    let protected = doc.get(sibling);
    let original = inspect(&doc).unwrap();
    assert!(original.edit_blockers.is_empty(), "{:?}", original.edit_blockers);
    assert_eq!(
        original.preserved_associations,
        vec![ParentAssociation { kind: ParentAssociationKind::UnclaimedScalar, key: 7, mcid: None, owner: sibling }]
    );
    assert_eq!(ParentAssociationKind::UnclaimedScalar.as_str(), "unclaimed_scalar");
    assert!(original.issues.iter().any(|issue| issue.contains("globally unclaimed")));
    let destination = create(&mut doc, Some(group), 2, &props("P")).unwrap();
    move_tag(&mut doc, child, Some(group), 1).unwrap();
    move_content(&mut doc, child, 0, destination, 0).unwrap();
    update(&mut doc, child, &TagProperties { title: Some(Some("Unrelated title".into())), ..Default::default() }).unwrap();
    update(&mut doc, child, &props("Span")).unwrap();
    set_role_mapping(&mut doc, "LocalCustom", Some("P")).unwrap();
    set_identifier(&mut doc, top, Some("unrelated-root-id")).unwrap();
    delete(&mut doc, destination, DeleteMode::Subtree).unwrap();
    assert_eq!(slot(&doc, parent_tree, 0, 0), Object::Null);
    assert_eq!(doc.get(sibling), protected);
    assert_eq!(inspect(&doc).unwrap().preserved_associations, original.preserved_associations);
    let reopened = Document::open(std::sync::Arc::new(bytes(&doc))).unwrap();
    let reopened_tree = inspect(&reopened).unwrap();
    assert!(reopened_tree.edit_blockers.is_empty());
    let association = reopened_tree.preserved_associations.first().unwrap();
    assert_eq!((association.kind, association.key, association.mcid), (ParentAssociationKind::UnclaimedScalar, 7, None));
    assert!(reopened_tree.element(association.owner).is_some());
}

#[test]
fn unclaimed_scalar_requires_zero_hidden_page_form_annotation_and_direct_claimants() {
    for kind in 0..4 {
        let Fixture { mut doc, group, .. } = unclaimed_scalar_fixture();
        let mut claimant = Dict::new();
        if kind == 2 {
            claimant.set(b"Type", Object::name("Annot"));
            claimant.set(b"StructParent", 7);
        } else {
            claimant.set(b"StructParents", 7);
            claimant.set(b"Type", Object::name(if kind == 0 { "Page" } else { "XObject" }));
        }
        match kind {
            1 => {
                claimant.set(b"Subtype", Object::name("Form"));
                doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(claimant, Vec::new())));
            }
            3 => {
                let mut container = Dict::new();
                container.set(b"Private", Object::Array(vec![Object::Dict(claimant)]));
                doc.add(container);
            }
            _ => {
                doc.add(claimant);
            }
        }
        let original = bytes(&doc);
        let numbers = doc.object_numbers();
        let tree = inspect(&doc).unwrap();
        assert!(tree.preserved_associations.is_empty(), "kind {kind}");
        assert!(!tree.edit_blockers.is_empty());
        assert!(create(&mut doc, Some(group), 2, &props("P")).is_err());
        assert_eq!(bytes(&doc), original);
        assert_eq!(doc.object_numbers(), numbers);
    }
}

#[test]
fn unclaimed_scalar_does_not_ignore_indirect_or_malformed_key_declarations() {
    for kind in 0..4 {
        let Fixture { mut doc, group, .. } = unclaimed_scalar_fixture();
        let mut hidden = Dict::new();
        let key = match kind {
            0 => Object::Ref(doc.add(Object::Int(7))),
            1 => Object::Int(-1),
            2 => Object::Null,
            _ => {
                let live = doc.add(Object::Int(7));
                Object::Ref(ObjRef::new(live.num, live.generation + 1))
            }
        };
        hidden.set(b"StructParent", key);
        doc.add(hidden);
        let original = bytes(&doc);
        assert!(!inspect(&doc).unwrap().edit_blockers.is_empty());
        assert!(create(&mut doc, Some(group), 2, &props("P")).is_err());
        assert_eq!(bytes(&doc), original);
    }
}

#[test]
fn unclaimed_scalar_rechecks_new_claimants_after_the_candidate_edit() {
    for direct in [false, true] {
        let Fixture { mut doc, .. } = unclaimed_scalar_fixture();
        let original = bytes(&doc);
        let numbers = doc.object_numbers();
        let result = associations::preserve(&mut doc, |candidate| {
            let mut claimant = Dict::new();
            claimant.set(b"StructParents", 7);
            if direct {
                let mut container = Dict::new();
                container.set(b"Private", Object::Dict(claimant));
                candidate.add(container);
            } else {
                candidate.add(Object::Stream(pdfcraft_cos::Stream::from_raw(claimant, Vec::new())));
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(bytes(&doc), original);
        assert_eq!(doc.object_numbers(), numbers);
    }
}

#[test]
fn unclaimed_scalar_protects_owner_dictionary_children_and_reachable_membership() {
    let Fixture { mut doc, group, sibling, .. } = unclaimed_scalar_fixture();
    let original = bytes(&doc);
    let numbers = doc.object_numbers();
    assert!(update(&mut doc, sibling, &TagProperties { title: Some(Some("Forbidden".into())), ..Default::default() }).is_err());
    assert!(update(&mut doc, sibling, &props("Span")).is_err());
    assert!(create(&mut doc, Some(sibling), 0, &props("Span")).is_err());
    assert!(delete(&mut doc, sibling, DeleteMode::Subtree).is_err());
    assert!(delete(&mut doc, group, DeleteMode::Subtree).is_err());
    assert!(delete(&mut doc, group, DeleteMode::Unwrap).is_err());
    assert_eq!(bytes(&doc), original);
    assert_eq!(doc.object_numbers(), numbers);
    assert!(
        associations::preserve(&mut doc, |candidate| {
            candidate.update_dict(group, |d| d.set(b"K", Object::Array(Vec::new()))).map_err(|e| invalid(e.to_string()))?;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(bytes(&doc), original);
}

#[test]
fn unclaimed_scalar_refuses_stale_owner_generations_and_array_slots() {
    for stale in [false, true] {
        let Fixture { mut doc, group, child, sibling, parent_tree, .. } = unclaimed_scalar_fixture();
        doc.update_dict(parent_tree, |d| {
            let extra = if stale { Object::Ref(ObjRef::new(sibling.num, sibling.generation + 1)) } else { Object::Array(vec![Object::Ref(sibling)]) };
            d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child)]), 7.into(), extra]));
        })
        .unwrap();
        let original = bytes(&doc);
        let tree = inspect(&doc).unwrap();
        assert!(tree.preserved_associations.is_empty());
        assert!(!tree.edit_blockers.is_empty());
        assert!(create(&mut doc, Some(group), 2, &props("P")).is_err());
        assert_eq!(bytes(&doc), original);
    }
}

#[test]
fn unclaimed_scalar_later_batch_failure_rolls_back_an_earlier_unrelated_delete() {
    let Fixture { mut doc, child, sibling, .. } = unclaimed_scalar_fixture();
    let original = bytes(&doc);
    let tree = inspect(&doc).unwrap();
    assert!(tree.element(child).is_some());
    assert!(tree.element(sibling).is_some());
    assert!(delete_tags(&mut doc, &[child, sibling], DeleteMode::Subtree).is_err());
    assert_eq!(bytes(&doc), original);
    assert_eq!(inspect(&doc).unwrap(), tree);
}

#[test]
fn unclaimed_scalar_artifact_conversion_only_changes_disjoint_content() {
    let Fixture { mut doc, child, sibling, page, .. } = unclaimed_scalar_fixture();
    let stream = doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), b"/P <</MCID 0>> BDC 0 0 10 10 re f EMC".to_vec())));
    doc.update_dict(page, |d| d.set(b"Contents", Object::Ref(stream))).unwrap();
    let protected = doc.get(sibling);
    crate::artifact::convert(&mut doc, child, false).unwrap();
    assert_eq!(doc.get(sibling), protected);
    let tree = inspect(&doc).unwrap();
    assert!(tree.element(child).is_none());
    assert!(tree.element(sibling).is_some());
    assert!(tree.edit_blockers.is_empty());
    assert_eq!(tree.preserved_associations.first().unwrap().kind, ParentAssociationKind::UnclaimedScalar);
}

#[test]
fn unclaimed_scalar_protects_indirect_k_objr_and_annotation_dictionaries() {
    let Fixture { mut doc, child, sibling, page, parent_tree, .. } = unclaimed_scalar_fixture();
    let mut annotation = Dict::new();
    annotation.set(b"Type", Object::name("Annot"));
    annotation.set(b"StructParent", 9);
    let annotation = doc.add(annotation);
    let mut objr = Dict::new();
    objr.set(b"Type", Object::name("OBJR"));
    objr.set(b"Pg", Object::Ref(page));
    objr.set(b"Obj", Object::Ref(annotation));
    let objr = doc.add(objr);
    let array = doc.add(Object::Array(vec![Object::Ref(objr)]));
    doc.update_dict(sibling, |d| d.set(b"K", Object::Ref(array))).unwrap();
    doc.update_dict(parent_tree, |d| {
        d.set(
            b"Nums",
            Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child)]), 7.into(), Object::Ref(sibling), 9.into(), Object::Ref(sibling)]),
        )
    })
    .unwrap();
    assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
    let original = bytes(&doc);
    for target in [sibling, objr, annotation] {
        assert!(
            associations::preserve(&mut doc, |candidate| {
                candidate.update_dict(target, |d| d.set(b"Private", Object::Int(42))).map_err(|e| invalid(e.to_string()))?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(bytes(&doc), original);
    }
    assert!(
        associations::preserve(&mut doc, |candidate| {
            candidate.set(array, Object::Array(Vec::new()));
            Ok(())
        })
        .is_err()
    );
    assert_eq!(bytes(&doc), original);
}

#[test]
fn unclaimed_scalar_global_declaration_scan_counts_toward_bulk_delete_budget() {
    let Fixture { mut doc, group, .. } = unclaimed_scalar_fixture();
    let mut selected = Vec::new();
    for index in 0..30 {
        selected.push(create(&mut doc, Some(group), index + 2, &props("P")).unwrap());
    }
    // Few indirect objects, but 20,000 nested direct dictionaries must each be
    // inspected before declaring the protected scalar key globally unclaimed.
    let mut nested = Dict::new();
    nested.set(b"Private", Object::Int(1));
    doc.add(Object::Array(vec![Object::Dict(nested); 20_000]));
    let tree = inspect(&doc).unwrap();
    assert!(tree.edit_blockers.is_empty());
    assert!(associations::preservation_work(&doc, &tree).unwrap() > 40_000);
    let original = bytes(&doc);
    let numbers = doc.object_numbers();
    let error = delete_tags(&mut doc, &selected, DeleteMode::Subtree).unwrap_err();
    assert!(error.to_string().contains("deletion batch exceeds"), "{error}");
    assert_eq!(bytes(&doc), original);
    assert_eq!(doc.object_numbers(), numbers);
}

// Fresh recovery regression for the centrally integrated scalar-comparison fix.
// Its original test source was not retained, so this test is newly reconstructed.
#[test]
fn scalar_entry_tracks_owner_reference_instead_of_resolved_mutable_dictionary() {
    let Fixture { mut doc, group, child, sibling, page, parent_tree, orphan, .. } = fixture();
    let mut annotation = Dict::new();
    annotation.set(b"Type", Object::name("Annot"));
    annotation.set(b"StructParent", 2);
    let annotation = doc.add(annotation);
    let mut objr = Dict::new();
    objr.set(b"Type", Object::name("OBJR"));
    objr.set(b"Obj", Object::Ref(annotation));
    objr.set(b"Pg", Object::Ref(page));
    doc.update_dict(child, |d| d.set(b"K", Object::Array(vec![Object::Int(0), Object::Dict(objr)]))).unwrap();
    doc.update_dict(parent_tree, |d| {
        d.set(b"Nums", Object::Array(vec![0.into(), Object::Array(vec![Object::Ref(child), Object::Ref(orphan)]), 2.into(), Object::Ref(child)]));
    })
    .unwrap();
    let mut hidden = Dict::new();
    hidden.set(b"Type", Object::name("Annot"));
    hidden.set(b"StructParent", 2);
    doc.add(hidden);
    let preserved = inspect(&doc).unwrap().preserved_associations;
    // Pinning/reordering this owner's dictionary does not reassign scalar key 2.
    move_tag(&mut doc, child, Some(group), 1).unwrap();
    assert_eq!(inspect(&doc).unwrap().preserved_associations, preserved);
    let original = bytes(&doc);
    // An actual owner reassignment must still detect the competing key claimant.
    assert!(move_content(&mut doc, child, 1, sibling, 0).is_err());
    assert_eq!(bytes(&doc), original);
}
