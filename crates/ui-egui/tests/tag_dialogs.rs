//! Advanced tag dialogs exercised through accessible egui controls and the real engine.
use egui::{Key, Modifiers};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use pdfcraft_engine::{
    Edit, ObjRef,
    a11y::tags::{TagKid, TagProperties, TagTree},
};
use pdfcraft_ui_egui::PdfCraftApp;

const TAGGED: &[u8] = b"%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R /MarkInfo << /Marked true >> >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /StructParents 0 /Contents 5 0 R >> endobj
4 0 obj << /Type /Page /Parent 2 0 R /StructParents 1 >> endobj
5 0 obj << /Length 67 >> stream
/P <</MCID 0>> BDC 0.2 0.5 0.8 rg 20 320 200 40 re f EMC
endstream endobj
6 0 obj << /Type /StructTreeRoot /K [7 0 R] /ParentTree 11 0 R /RoleMap << /ChapterTitle /H1 >> >> endobj
7 0 obj << /Type /StructElem /S /Document /P 6 0 R /K [8 0 R 9 0 R 10 0 R] >> endobj
8 0 obj << /Type /StructElem /S /ChapterTitle /T (Introduction) /P 7 0 R /Pg 3 0 R /K 0 >> endobj
9 0 obj << /Type /StructElem /S /P /T (Body) /P 7 0 R /Pg 3 0 R >> endobj
10 0 obj << /Type /StructElem /S /Figure /T (Chart) /Alt (Blue chart) /P 7 0 R /Pg 4 0 R >> endobj
11 0 obj << /Nums [0 [8 0 R] 1 []] >> endobj
trailer << /Root 1 0 R >>
%%EOF";

fn harness() -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).build_eframe(|_| {
        let mut app = PdfCraftApp::new();
        app.open_bytes("tag-dialogs.pdf", None, TAGGED.to_vec()).unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "tags").unwrap();
        app
    });
    h.run_steps(6);
    h
}
fn tree(h: &Harness<'static, PdfCraftApp>) -> TagTree {
    h.state().session.get(h.state().views[0].id).unwrap().tags().unwrap()
}
fn replace(h: &mut Harness<'static, PdfCraftApp>, label: &str, text: &str) {
    h.get_by_label(label).click();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_label(label).type_text(text);
    h.run_steps(2);
}
fn open_roles(h: &mut Harness<'static, PdfCraftApp>) {
    h.get_by_label("Tag options").click();
    h.run_steps(2);
    h.get_by_label("Edit role mappings…").click();
    h.run_steps(3);
}
fn open_content_move(h: &mut Harness<'static, PdfCraftApp>) {
    h.get_by_label("Tag options").click();
    h.run_steps(2);
    h.get_by_label("Expand all tags").click();
    h.run_steps(3);
    h.get_by_label("Content · MCID 0").click_secondary();
    h.run_steps(2);
    h.get_by_label("Move content to…").click();
    h.run_steps(3);
}
fn change_document(h: &mut Harness<'static, PdfCraftApp>) {
    let id = h.state().views[0].id;
    h.state_mut()
        .session
        .apply(
            id,
            Edit::UpdateTag {
                id: ObjRef::new(9, 0),
                properties: TagProperties { title: Some(Some("Changed externally".into())), ..TagProperties::default() },
            },
        )
        .unwrap();
    h.run_steps(3);
}

#[test]
fn role_mapping_changes_all_matching_tags_and_survives_save_and_undo() {
    let mut h = harness();
    open_roles(&mut h);
    h.get_by_label("ChapterTitle → H1").click();
    h.run_steps(2);
    replace(&mut h, "Maps to", "H2");
    h.get_by_label("Apply mapping").click();
    h.run_steps(4);
    assert!(h.query_by_label("Apply mapping").is_none());
    assert_eq!(tree(&h).element(ObjRef::new(8, 0)).unwrap().standard_role, "H2");
    let id = h.state().views[0].id;
    let bytes = h.state().session.save_bytes(id).unwrap();
    let mut session = pdfcraft_engine::Session::new();
    let reopened = session.open("saved.pdf", None, bytes, None).unwrap();
    assert_eq!(session.get(reopened).unwrap().tags().unwrap().role_map.get("ChapterTitle").map(String::as_str), Some("H2"));
    h.state_mut().session.undo(id).unwrap();
    h.run_steps(3);
    assert_eq!(tree(&h).element(ObjRef::new(8, 0)).unwrap().standard_role, "H1");
}

#[test]
fn rejected_role_mapping_keeps_draft_for_correction() {
    let mut h = harness();
    open_roles(&mut h);
    replace(&mut h, "Custom role", "ChapterTitle");
    replace(&mut h, "Maps to", "MissingTarget");
    h.get_by_label("Apply mapping").click();
    h.run_steps(4);
    h.get_by_label_contains("Could not apply:");
    assert!(h.query_all_by_value("ChapterTitle").next().is_some());
    assert!(h.query_all_by_value("MissingTarget").next().is_some());
    assert_eq!(tree(&h).role_map.get("ChapterTitle").map(String::as_str), Some("H1"));
    replace(&mut h, "Maps to", "H3");
    h.get_by_label("Apply mapping").click();
    h.run_steps(4);
    assert_eq!(tree(&h).element(ObjRef::new(8, 0)).unwrap().standard_role, "H3");
}

#[test]
fn stale_role_dialog_preserves_typing_and_blocks_apply() {
    let mut h = harness();
    open_roles(&mut h);
    replace(&mut h, "Custom role", "PendingRole");
    replace(&mut h, "Maps to", "P");
    change_document(&mut h);
    h.get_by_label_contains("The document changed while this dialog was open.");
    assert!(h.query_all_by_value("PendingRole").next().is_some());
    assert!(h.get_by_label("Apply mapping").accesskit_node().is_disabled());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(!tree(&h).role_map.contains_key("PendingRole"));
}

#[test]
fn content_destination_search_moves_exact_leaf_and_undo_restores_owner() {
    let mut h = harness();
    open_content_move(&mut h);
    replace(&mut h, "Find destination", "Body");
    h.get_by_label("<P> Body · 9 0 R").click();
    h.run_steps(2);
    assert!(h.query_by_label("Structure tree root").is_none());
    h.get_by_label("Position (1-based)");
    h.get_by_label("Move").click();
    h.run_steps(4);
    let t = tree(&h);
    assert!(t.element(ObjRef::new(8, 0)).unwrap().kids.is_empty());
    assert!(matches!(t.element(ObjRef::new(9, 0)).unwrap().kids.as_slice(), [TagKid::Mcid { id: 0, .. }] | [TagKid::Mcr { id: 0, .. }]));
    assert!(h.query_by_label("Find destination").is_none());
    let id = h.state().views[0].id;
    h.state_mut().session.undo(id).unwrap();
    h.run_steps(3);
    assert!(matches!(tree(&h).element(ObjRef::new(8, 0)).unwrap().kids.first(), Some(TagKid::Mcid { id: 0, .. })));
}

#[test]
fn stale_content_move_preserves_search_and_blocks_indices() {
    let mut h = harness();
    open_content_move(&mut h);
    replace(&mut h, "Find destination", "Body");
    h.get_by_label("<P> Body · 9 0 R").click();
    h.run_steps(2);
    change_document(&mut h);
    h.get_by_label_contains("The document changed. This move draft is kept");
    assert!(h.query_all_by_value("Body").next().is_some());
    assert!(h.get_by_label("Move").accesskit_node().is_disabled());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(matches!(tree(&h).element(ObjRef::new(8, 0)).unwrap().kids.first(), Some(TagKid::Mcid { id: 0, .. })));
}

#[test]
fn unused_role_mapping_can_be_added_and_removed() {
    let mut h = harness();
    open_roles(&mut h);
    replace(&mut h, "Custom role", "UnusedRole");
    replace(&mut h, "Maps to", "P");
    h.get_by_label("Apply mapping").click();
    h.run_steps(4);
    assert_eq!(tree(&h).role_map.get("UnusedRole").map(String::as_str), Some("P"));
    open_roles(&mut h);
    h.get_by_label("UnusedRole → P").click();
    h.run_steps(2);
    h.get_by_label("Remove mapping").click();
    h.run_steps(4);
    assert!(!tree(&h).role_map.contains_key("UnusedRole"));
    assert_eq!(tree(&h).role_map.get("ChapterTitle").map(String::as_str), Some("H1"));
}

#[test]
fn role_dialog_disables_underlying_properties_without_overwriting_them() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(3);
    open_roles(&mut h);
    assert!(h.get_by_label("Title").accesskit_node().is_disabled());
    assert!(!h.get_by_label("Custom role").accesskit_node().is_disabled());
    replace(&mut h, "Custom role", "TemporaryDraft");
    replace(&mut h, "Maps to", "P");
    assert!(h.query_all_by_value("Body").next().is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(!h.get_by_label("Title").accesskit_node().is_disabled());
    assert!(h.query_all_by_value("Body").next().is_some());
    assert_eq!(tree(&h).element(ObjRef::new(9, 0)).unwrap().title.as_deref(), Some("Body"));
    assert!(!tree(&h).role_map.contains_key("TemporaryDraft"));
}

#[test]
fn escape_cancels_both_dialogs_without_applying_drafts() {
    let mut h = harness();
    open_roles(&mut h);
    replace(&mut h, "Custom role", "EscapeDraft");
    replace(&mut h, "Maps to", "H2");
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Apply mapping").is_none());
    assert!(!tree(&h).role_map.contains_key("EscapeDraft"));
    open_content_move(&mut h);
    replace(&mut h, "Find destination", "Body");
    h.get_by_label("<P> Body · 9 0 R").click();
    h.run_steps(2);
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Find destination").is_none());
    assert!(matches!(tree(&h).element(ObjRef::new(8, 0)).unwrap().kids.first(), Some(TagKid::Mcid { id: 0, .. })));
    assert!(tree(&h).element(ObjRef::new(9, 0)).unwrap().kids.is_empty());
}
