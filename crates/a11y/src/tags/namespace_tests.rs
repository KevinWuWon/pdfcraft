use super::*;
use std::sync::Arc;
fn prop(role: &str) -> TagProperties {
    TagProperties { role: Some(role.into()), ..Default::default() }
}
fn fixture() -> (Document, ObjRef, ObjRef, ObjRef) {
    let mut doc = Document::new_empty();
    let top = create(&mut doc, None, 0, &prop("Document")).unwrap();
    let a = create(&mut doc, Some(top), 0, &prop("P")).unwrap();
    let b = create(&mut doc, Some(top), 1, &prop("P")).unwrap();
    (doc, top, a, b)
}
fn ns(doc: &mut Document, uri: &str, map: Option<Dict>) -> ObjRef {
    let mut d = Dict::new();
    d.set(b"NS", Object::String(PdfString::text(uri)));
    d.set(b"Private", Object::Int(71));
    if let Some(map) = map {
        d.set(b"RoleMapNS", Object::Dict(map));
    }
    doc.add(d)
}
fn declare(doc: &mut Document, ids: &[ObjRef]) -> ObjRef {
    let root = inspect(doc).unwrap().root.unwrap();
    doc.update_dict(root, |d| d.set(b"Namespaces", Object::Array(ids.iter().map(|r| Object::Ref(*r)).collect()))).unwrap();
    root
}
fn assign(doc: &mut Document, tag: ObjRef, namespace: ObjRef, role: &str) {
    doc.update_dict(tag, |d| {
        d.set(b"NS", Object::Ref(namespace));
        d.set(b"S", Object::name(role));
    })
    .unwrap();
}
fn bytes(doc: &Document) -> Vec<u8> {
    pdfcraft_cos::write_full(doc, &Default::default()).unwrap()
}

#[test]
fn distinct_standard_sets_and_unbounded_pdf2_heading_names() {
    assert!(is_standard_role("Title", PDF_20_NAMESPACE));
    assert!(!is_standard_role("Title", PDF_17_NAMESPACE));
    assert!(is_standard_role("Note", PDF_17_NAMESPACE));
    assert!(!is_standard_role("Note", PDF_20_NAMESPACE));
    assert!(is_standard_role("H99999999999999999999999999", PDF_20_NAMESPACE));
    for role in ["H0", "H01", "H-1", "H7x"] {
        assert!(!is_standard_role(role, PDF_20_NAMESPACE));
    }
    assert!(is_standard_role("math", MATHML_NAMESPACE));
    assert!(!is_standard_role("Math", MATHML_NAMESPACE));
}
#[test]
fn role_maps_apply_to_default_names_but_not_explicit_namespace_roles() {
    let (mut doc, _, a, b) = fixture();
    let n = ns(&mut doc, PDF_20_NAMESPACE, None);
    let root = declare(&mut doc, &[n]);
    let mut map = Dict::new();
    map.set(b"P", Object::name("Figure"));
    map.set(b"Title", Object::name("P"));
    doc.update_dict(root, |d| d.set(b"RoleMap", map)).unwrap();
    assign(&mut doc, b, n, "P");
    let tree = inspect(&doc).unwrap();
    assert!(tree.issues.is_empty(), "{:?}", tree.issues);
    assert_eq!(tree.element(a).unwrap().standard_role, "Figure");
    assert_eq!(tree.element(b).unwrap().standard_role, "P");
    update(&mut doc, b, &prop("Title")).unwrap();
    assert_eq!(inspect(&doc).unwrap().element(b).unwrap().standard_role, "Title");
    let before = bytes(&doc);
    assert!(update(&mut doc, b, &prop("Note")).is_err());
    assert_eq!(bytes(&doc), before);
    assert_eq!(crate::try_figures(&doc).unwrap().len(), 1);
}
#[test]
fn custom_cross_namespace_same_spelling_and_mapping_only_mathml_target() {
    let (mut doc, _, a, b) = fixture();
    let math = ns(&mut doc, MATHML_NAMESPACE, None);
    let pdf2 = ns(&mut doc, PDF_20_NAMESPACE, None);
    let mut middle = Dict::new();
    middle.set(b"X", Object::Array(vec![Object::name("Figure"), Object::Ref(pdf2)]));
    let middle = ns(&mut doc, "urn:middle", Some(middle));
    let mut map = Dict::new();
    map.set(b"X", Object::Array(vec![Object::name("X"), Object::Ref(middle)]));
    map.set(b"Math", Object::Array(vec![Object::name("math"), Object::Ref(math)]));
    let custom = ns(&mut doc, "urn:custom", Some(map));
    declare(&mut doc, &[custom]);
    assign(&mut doc, a, custom, "X");
    assign(&mut doc, b, custom, "Math");
    let tree = inspect(&doc).unwrap();
    assert!(tree.issues.is_empty(), "{:?}", tree.issues);
    assert_eq!(tree.element(a).unwrap().standard_role, "Figure");
    assert_eq!(tree.element(b).unwrap().standard_namespace_uri.as_deref(), Some(MATHML_NAMESPACE));
    assert_eq!(tree.namespaces.len(), 1);
    assert_eq!(crate::try_figures(&doc).unwrap()[0].obj, a);
}
#[test]
fn single_name_namespace_target_bypasses_root_mapping() {
    let (mut doc, _, a, _) = fixture();
    let mut map = Dict::new();
    map.set(b"X", Object::name("P"));
    let custom = ns(&mut doc, "urn:custom", Some(map));
    let root = declare(&mut doc, &[custom]);
    assign(&mut doc, a, custom, "X");
    let mut root_map = Dict::new();
    root_map.set(b"P", Object::name("Figure"));
    doc.update_dict(root, |d| d.set(b"RoleMap", root_map)).unwrap();
    let e = inspect(&doc).unwrap().element(a).unwrap().clone();
    assert_eq!(e.standard_role, "P");
    assert_eq!(e.standard_namespace_uri.as_deref(), Some(PDF_17_NAMESPACE));
}
#[test]
fn namespace_identity_is_preserved_by_create_reorder_and_unwrap() {
    let (mut doc, top, a, b) = fixture();
    let n = ns(&mut doc, PDF_20_NAMESPACE, None);
    declare(&mut doc, &[n]);
    assign(&mut doc, top, n, "Document");
    assign(&mut doc, a, n, "Title");
    let original = doc.get(n);
    let new = create(&mut doc, Some(a), 0, &prop("P")).unwrap();
    assert_eq!(dictionary(&doc, new).unwrap().reference(b"NS"), None);
    move_tag(&mut doc, b, Some(a), 1).unwrap();
    update(&mut doc, a, &TagProperties { title: Some(Some("Heading".into())), ..Default::default() }).unwrap();
    assert_eq!(dictionary(&doc, a).unwrap().reference(b"NS"), Some(n));
    delete(&mut doc, a, DeleteMode::Unwrap).unwrap();
    assert_eq!(dictionary(&doc, b).unwrap().reference(b"NS"), None);
    assert!(Arc::ptr_eq(&original, &doc.get(n)));
    let tree = inspect(&doc).unwrap();
    assert!(tree.issues.is_empty());
    let reopened = Document::open(bytes(&doc).into()).unwrap();
    let after = inspect(&reopened).unwrap();
    assert_tree_isomorphism(&tree, &after);
}
#[test]
fn cyclic_unknown_stale_and_undeclared_namespaces_are_explicit_and_atomic() {
    for mode in 0..5 {
        let (mut doc, _, a, _) = fixture();
        let n = ns(&mut doc, PDF_20_NAMESPACE, None);
        let root = declare(&mut doc, &[n]);
        assign(&mut doc, a, n, "Title");
        match mode {
            0 => {
                doc.update_dict(root, |d| {
                    d.remove(b"Namespaces");
                })
                .unwrap();
            }
            1 => {
                doc.update_dict(a, |d| d.set(b"NS", Object::Ref(ObjRef::new(n.num, n.generation.saturating_add(1))))).unwrap();
            }
            2 => {
                doc.update_dict(n, |d| d.set(b"Type", Object::name("Wrong"))).unwrap();
            }
            3 => {
                doc.update_dict(a, |d| d.set(b"S", Object::name("Unknown"))).unwrap();
            }
            _ => {
                let mut m = Dict::new();
                m.set(b"Title", Object::Array(vec![Object::name("Title"), Object::Ref(n)]));
                doc.update_dict(n, |d| d.set(b"RoleMapNS", m)).unwrap();
            }
        }
        let tree = inspect(&doc).unwrap();
        assert!(!tree.issues.is_empty());
        assert!(tree.element(a).unwrap().standard_namespace_uri.is_none());
        assert!(crate::try_figures(&doc).is_err());
        let before = bytes(&doc);
        assert!(update(&mut doc, a, &prop("P")).is_err());
        assert_eq!(bytes(&doc), before);
        let report = crate::check(&doc, &crate::Options::default());
        assert_eq!(report.results.iter().find(|r| r.rule == crate::Rule::FiguresAltText).unwrap().status, crate::Status::Manual);
    }
}
#[test]
fn definite_checker_failure_survives_an_unresolved_sibling() {
    let (mut doc, _, a, b) = fixture();
    update(&mut doc, a, &prop("Figure")).unwrap();
    doc.update_dict(b, |d| d.set(b"S", Object::name("Unknown"))).unwrap();
    let report = crate::check(&doc, &crate::Options::default());
    assert_eq!(report.results.iter().find(|r| r.rule == crate::Rule::FiguresAltText).unwrap().status, crate::Status::Failed);
}
#[test]
fn shared_rolemap_namespace_dictionary_blocks_authoring() {
    let (mut doc, _, a, _) = fixture();
    let mut map = Dict::new();
    map.set(b"Custom", Object::name("P"));
    let shared = doc.add(map);
    let n = ns(&mut doc, "urn:custom", None);
    doc.update_dict(n, |d| d.set(b"RoleMapNS", Object::Ref(shared))).unwrap();
    let root = declare(&mut doc, &[n]);
    assign(&mut doc, a, n, "Custom");
    doc.update_dict(root, |d| d.set(b"RoleMap", Object::Ref(shared))).unwrap();
    let before = bytes(&doc);
    assert!(set_role_mapping(&mut doc, "Custom", Some("Figure")).is_err());
    assert_eq!(bytes(&doc), before);
}
#[test]
fn metadata_patch_preserves_unrelated_invalid_map_and_indirect_array_identity() {
    let (mut doc, top, a, _) = fixture();
    let root = inspect(&doc).unwrap().root.unwrap();
    let mut map = Dict::new();
    map.set(b"Unrelated", Object::name("NotStandard"));
    doc.update_dict(root, |d| d.set(b"RoleMap", map)).unwrap();
    let array = doc.add(dictionary(&doc, top).unwrap().get(b"K").unwrap().clone());
    doc.update_dict(top, |d| d.set(b"K", Object::Ref(array))).unwrap();
    let before: BTreeMap<_, _> = doc.object_numbers().into_iter().map(|n| (n, doc.get(ObjRef::new(n, doc.generation(n))))).collect();
    let old_issues = inspect(&doc).unwrap().issues;
    update(&mut doc, top, &TagProperties { title: Some(Some("safe".into())), alt: Some(Some("description".into())), ..Default::default() }).unwrap();
    assert_eq!(dictionary(&doc, top).unwrap().reference(b"K"), Some(array));
    for (n, value) in before {
        if n != top.num {
            assert!(Arc::ptr_eq(&value, &doc.get(ObjRef::new(n, doc.generation(n)))));
        }
    }
    assert_eq!(inspect(&doc).unwrap().issues, old_issues);
    assert!(update(&mut doc, a, &prop("P")).is_err());
}
#[test]
fn metadata_aliases_bad_backlinks_and_stale_targets_refuse_atomically() {
    for mode in 0..4 {
        let (mut doc, top, a, _) = fixture();
        let target = if mode == 3 { ObjRef::new(a.num, a.generation.saturating_add(1)) } else { a };
        match mode {
            0 => {
                doc.update_dict(top, |d| d.set(b"K", Object::Array(vec![Object::Ref(a), Object::Ref(a)]))).unwrap();
            }
            1 => {
                doc.update_dict(a, |d| d.set(b"P", Object::Ref(a))).unwrap();
            }
            2 => {
                doc.update_dict(a, |d| d.set(b"Type", Object::name("Wrong"))).unwrap();
            }
            _ => {}
        }
        let before = bytes(&doc);
        assert!(update(&mut doc, target, &TagProperties { title: Some(Some("bad".into())), ..Default::default() }).is_err());
        assert_eq!(bytes(&doc), before);
    }
}
#[test]
fn figure_discovery_and_checker_follow_indirect_child_arrays() {
    let (mut doc, top, a, _) = fixture();
    update(&mut doc, a, &prop("Figure")).unwrap();
    let array = doc.add(dictionary(&doc, top).unwrap().get(b"K").unwrap().clone());
    doc.update_dict(top, |d| d.set(b"K", Object::Ref(array))).unwrap();
    assert_eq!(crate::try_figures(&doc).unwrap()[0].obj, a);
    crate::set_alt(&mut doc, a, Some("description")).unwrap();
    assert_eq!(crate::try_figures(&doc).unwrap()[0].alt.as_deref(), Some("description"));
    let report = crate::check(&doc, &crate::Options::default());
    assert_eq!(report.results.iter().find(|r| r.rule == crate::Rule::FiguresAltText).unwrap().status, crate::Status::Passed);
}

#[test]
fn first_tag_rejects_stream_or_stale_markinfo_before_allocation() {
    for stream in [false, true] {
        let mut doc = Document::new_empty();
        let obj =
            if stream { doc.add(Object::Stream(pdfcraft_cos::Stream::from_raw(Dict::new(), b"private".to_vec()))) } else { doc.add(Dict::new()) };
        let r = if stream { obj } else { ObjRef::new(obj.num, obj.generation.saturating_add(1)) };
        doc.update_dict(doc.root().unwrap(), |d| d.set(b"MarkInfo", Object::Ref(r))).unwrap();
        let before = bytes(&doc);
        let numbers = doc.object_numbers();
        assert!(create(&mut doc, None, 0, &prop("Document")).is_err());
        assert_eq!(bytes(&doc), before);
        assert_eq!(doc.object_numbers(), numbers);
    }
}
fn content_fixture() -> (Document, ObjRef, ObjRef, ObjRef, ObjRef) {
    let (mut doc, top, a, b) = fixture();
    let mut page = Dict::new();
    page.set(b"Type", Object::name("Page"));
    page.set(b"StructParents", Object::Int(0));
    let page = doc.add(page);
    let pages = dictionary(&doc, doc.root().unwrap()).unwrap().reference(b"Pages").unwrap();
    doc.update_dict(pages, |d| {
        d.set(b"Kids", Object::Array(vec![Object::Ref(page)]));
        d.set(b"Count", Object::Int(1));
    })
    .unwrap();
    doc.update_dict(a, |d| {
        d.set(b"Pg", Object::Ref(page));
        d.set(b"K", Object::Int(0));
    })
    .unwrap();
    let mut pt = Dict::new();
    pt.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(a)])]));
    let root = inspect(&doc).unwrap().root.unwrap();
    doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
    (doc, top, a, b, page)
}
#[test]
fn namespace_content_moves_change_only_ownership_and_preserve_maps() {
    let (mut doc, _, a, b, page) = content_fixture();
    let n = ns(&mut doc, PDF_20_NAMESPACE, None);
    let root = declare(&mut doc, &[n]);
    assign(&mut doc, a, n, "P");
    assign(&mut doc, b, n, "Aside");
    let original = doc.get(n);
    let namespaces = dictionary(&doc, root).unwrap().get(b"Namespaces").unwrap().clone();
    move_content(&mut doc, a, 0, b, 0).unwrap();
    let tree = inspect(&doc).unwrap();
    assert!(tree.edit_blockers.is_empty(), "{:?}", tree.edit_blockers);
    assert_eq!(tree.element(a).unwrap().namespace, Some(n));
    assert_eq!(tree.element(b).unwrap().namespace, Some(n));
    assert_eq!(tree.element(b).unwrap().kids, vec![TagKid::Mcr { id: 0, page: Some(page), stream: None }]);
    assert_eq!(dictionary(&doc, root).unwrap().get(b"Namespaces"), Some(&namespaces));
    assert!(Arc::ptr_eq(&original, &doc.get(n)));
}
#[test]
fn metadata_keeps_unrelated_conflicting_parent_associations_exact() {
    let (mut doc, top, a, b, page) = content_fixture();
    doc.update_dict(b, |d| {
        d.set(b"Pg", Object::Ref(page));
        d.set(b"K", Object::Int(0));
    })
    .unwrap();
    let tree = inspect(&doc).unwrap();
    assert!(!tree.edit_blockers.is_empty());
    let before: BTreeMap<_, _> = doc.object_numbers().into_iter().map(|n| (n, doc.get(ObjRef::new(n, doc.generation(n))))).collect();
    update(&mut doc, top, &TagProperties { actual_text: Some(Some("safe patch".into())), ..Default::default() }).unwrap();
    for (n, v) in before {
        if n != top.num {
            assert!(Arc::ptr_eq(&v, &doc.get(ObjRef::new(n, doc.generation(n)))));
        }
    }
    assert_eq!(inspect(&doc).unwrap().edit_blockers, tree.edit_blockers);
    assert!(update(&mut doc, a, &prop("P")).is_err());
}
#[test]
fn overlong_mapping_chain_refuses_without_recursion_or_mutation() {
    let (mut doc, _, a, _) = fixture();
    let mut map = Dict::new();
    for i in 0..DEPTH + 1 {
        map.set(format!("X{i}").as_bytes(), Object::name(&format!("X{}", i + 1)));
    }
    map.set(format!("X{}", DEPTH + 1).as_bytes(), Object::name("P"));
    let n = ns(&mut doc, "urn:bounded", Some(map));
    // A single-name namespace target is terminal PDF1.7 and cannot refer to X1.
    declare(&mut doc, &[n]);
    assign(&mut doc, a, n, "X0");
    assert!(!inspect(&doc).unwrap().edit_blockers.is_empty());
    let root = inspect(&doc).unwrap().root.unwrap();
    doc.update_dict(root, |d| {
        d.remove(b"Namespaces");
    })
    .unwrap();
    // Test the actual transitive root chain separately.
    let mut map = Dict::new();
    for i in 0..DEPTH + 1 {
        map.set(format!("X{i}").as_bytes(), Object::name(&format!("X{}", i + 1)));
    }
    map.set(format!("X{}", DEPTH + 1).as_bytes(), Object::name("P"));
    doc.update_dict(root, |d| d.set(b"RoleMap", map)).unwrap();
    doc.update_dict(a, |d| {
        d.remove(b"NS");
    })
    .unwrap();
    let before = bytes(&doc);
    assert!(update(&mut doc, a, &prop("P")).is_err());
    assert_eq!(bytes(&doc), before);
}

#[test]
fn stale_indirect_child_array_refuses_metadata_and_structure_edits() {
    let (mut doc, top, a, _) = fixture();
    let array = doc.add(dictionary(&doc, top).unwrap().get(b"K").unwrap().clone());
    doc.update_dict(top, |d| d.set(b"K", Object::Ref(ObjRef::new(array.num, array.generation.saturating_add(1))))).unwrap();
    let before = bytes(&doc);
    assert!(update(&mut doc, top, &TagProperties { title: Some(Some("no".into())), ..Default::default() }).is_err());
    assert!(move_tag(&mut doc, a, Some(top), 0).is_err());
    assert_eq!(bytes(&doc), before);
}

// Full value equality after a bijective identity remapping: write_full may renumber,
// but must preserve topology, namespace associations and every exposed property.
fn assert_tree_isomorphism(before: &TagTree, after: &TagTree) {
    let mut forward = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    let mut bind = |a: ObjRef, b: ObjRef| {
        assert!(forward.insert(a, b).is_none_or(|old| old == b));
        assert!(reverse.insert(b, a).is_none_or(|old| old == a));
    };
    assert_eq!(before.root.is_some(), after.root.is_some());
    if let (Some(a), Some(b)) = (before.root, after.root) {
        bind(a, b);
    }
    assert_eq!(before.pages.len(), after.pages.len());
    for (a, b) in before.pages.iter().zip(&after.pages) {
        bind(*a, *b);
    }
    assert_eq!(before.namespaces.len(), after.namespaces.len());
    for (a, b) in before.namespaces.iter().zip(&after.namespaces) {
        bind(a.id, b.id);
    }
    assert_eq!(before.elements.len(), after.elements.len());
    for (a, b) in before.elements.iter().zip(&after.elements) {
        bind(a.id, b.id);
    }
    let remap = |id: ObjRef| *forward.get(&id).expect("every identity role must be represented in the bijection");
    let kids = |list: &mut Vec<TagKid>| {
        for k in list {
            match k {
                TagKid::Element { id } => *id = remap(*id),
                TagKid::Mcid { page, .. } => *page = page.map(remap),
                TagKid::Mcr { page, stream, .. } => {
                    *page = page.map(remap);
                    *stream = stream.map(remap);
                }
                TagKid::Objr { object, page } => {
                    *object = remap(*object);
                    *page = page.map(remap);
                }
                TagKid::Unsupported { .. } => {}
            }
        }
    };
    let mut expected = before.clone();
    expected.root = expected.root.map(remap);
    for p in &mut expected.pages {
        *p = remap(*p);
    }
    for ns in &mut expected.namespaces {
        ns.id = remap(ns.id);
    }
    kids(&mut expected.children);
    for e in &mut expected.elements {
        e.id = remap(e.id);
        e.parent = e.parent.map(remap);
        e.page = e.page.map(remap);
        e.namespace = e.namespace.map(remap);
        kids(&mut e.kids);
    }
    assert_eq!(&expected, after);
}

#[test]
fn shared_indirect_integer_mcids_are_distinct_across_pages() {
    let (mut doc, _, a, b, _) = content_fixture();
    update(&mut doc, a, &prop("Figure")).unwrap();
    update(&mut doc, b, &prop("Figure")).unwrap();
    let mut p = Dict::new();
    p.set(b"Type", Object::name("Page"));
    p.set(b"StructParents", Object::Int(1));
    let second = doc.add(p);
    let pages = dictionary(&doc, doc.root().unwrap()).unwrap().reference(b"Pages").unwrap();
    let mut kids = dictionary(&doc, pages).unwrap().get(b"Kids").unwrap().as_array().unwrap().clone();
    kids.push(Object::Ref(second));
    doc.update_dict(pages, |d| {
        d.set(b"Kids", Object::Array(kids));
        d.set(b"Count", Object::Int(2));
    })
    .unwrap();
    let integer = doc.add(Object::Int(0));
    for tag in [a, b] {
        doc.update_dict(tag, |d| {
            d.set(b"K", Object::Ref(integer));
            d.set(b"Alt", Object::String(PdfString::text("description")));
        })
        .unwrap();
    }
    doc.update_dict(b, |d| d.set(b"Pg", Object::Ref(second))).unwrap();
    let mut pt = Dict::new();
    pt.set(b"Nums", Object::Array(vec![Object::Int(0), Object::Array(vec![Object::Ref(a)]), Object::Int(1), Object::Array(vec![Object::Ref(b)])]));
    let root = inspect(&doc).unwrap().root.unwrap();
    doc.update_dict(root, |d| d.set(b"ParentTree", pt)).unwrap();
    assert!(inspect(&doc).unwrap().edit_blockers.is_empty());
    assert_eq!(crate::try_figures(&doc).unwrap().len(), 2);
    let report = crate::check(&doc, &crate::Options::default());
    assert_eq!(report.results.iter().find(|r| r.rule == crate::Rule::FiguresAltText).unwrap().status, crate::Status::Passed);
}
