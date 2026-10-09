//! Tag panel workflows through the real egui UI and the transactional engine.
use egui::{Key, Modifiers};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use pdfcraft_engine::{
    ObjRef,
    a11y::tags::{TagKid, TagTree},
};
use pdfcraft_ui_egui::{PdfCraftApp, RightPanel};

const TAGGED: &[u8] = b"%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R /MarkInfo << /Marked true >> >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /StructParents 0 /Contents 5 0 R >> endobj
4 0 obj << /Type /Page /Parent 2 0 R /StructParents 1 >> endobj
5 0 obj << /Length 67 >> stream
/P <</MCID 0>> BDC 0.2 0.5 0.8 rg 20 320 200 40 re f EMC
endstream endobj
6 0 obj << /Type /StructTreeRoot /K [7 0 R] /ParentTree 11 0 R >> endobj
7 0 obj << /Type /StructElem /S /Document /P 6 0 R /K [8 0 R 9 0 R 10 0 R] >> endobj
8 0 obj << /Type /StructElem /S /H1 /T (Introduction) /P 7 0 R /Pg 3 0 R /K 0 >> endobj
9 0 obj << /Type /StructElem /S /P /T (Body) /P 7 0 R /Pg 3 0 R >> endobj
10 0 obj << /Type /StructElem /S /Figure /T (Chart) /Alt (Blue chart) /P 7 0 R /Pg 4 0 R >> endobj
11 0 obj << /Nums [0 [8 0 R] 1 []] >> endobj
trailer << /Root 1 0 R >>
%%EOF";

fn harness() -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).build_eframe(|_| {
        let mut app = PdfCraftApp::new();
        app.open_bytes("tags.pdf", None, TAGGED.to_vec()).unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "tags").unwrap();
        app
    });
    h.run_steps(6);
    h
}
fn tree(h: &Harness<'static, PdfCraftApp>) -> TagTree {
    let id = h.state().views[0].id;
    h.state().session.get(id).unwrap().tags().unwrap()
}
fn menu(h: &mut Harness<'static, PdfCraftApp>, label: &str, item: &str) {
    h.get_by_label(label).click_secondary();
    h.run_steps(2);
    h.get_by_label(item).click();
    h.run_steps(3);
}
fn tag_ids(h: &Harness<'static, PdfCraftApp>) -> Vec<u32> {
    tree(h).element(ObjRef::new(7, 0)).unwrap().kids.iter().filter_map(|k| if let TagKid::Element { id } = k { Some(id.num) } else { None }).collect()
}

#[test]
fn pointer_selection_stays_visible_when_inspector_shrinks_tree() {
    let mut h = harness();
    h.set_size(egui::vec2(1178.0, 814.0));
    h.run_steps(3);
    h.get_by_label("<Figure> Chart").click();
    h.run_steps(4);
    let selected = h.get_by_label("<Figure> Chart");
    assert!(selected.accesskit_node().is_selected().unwrap_or(false));
    assert!(selected.rect().bottom() < h.get_by_label("Go to page").rect().top(), "the selected row stays above the inspector footer");
    h.get_by_label("Apply properties");
    h.get_by_label("Cancel properties");
}

#[test]
fn edit_properties_persist_and_undo() {
    let mut h = harness();
    assert_eq!(h.state().right, Some(RightPanel::Tags));
    h.get_by_label("<H1> Introduction").click();
    h.run_steps(3);
    h.get_by_label("Title").click();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_label("Title").type_text("Overview");
    h.run_steps(2);
    h.get_by_label("Apply properties").click();
    h.run_steps(4);
    assert_eq!(tree(&h).element(ObjRef::new(8, 0)).unwrap().title.as_deref(), Some("Overview"));
    h.get_by_label("<H1> Overview");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    h.get_by_label("<H1> Introduction");
    let id = h.state().views[0].id;
    h.state_mut().session.redo(id).unwrap();
    h.run_steps(3);
    let bytes = h.state().session.save_bytes(id).unwrap();
    let mut reopened = pdfcraft_engine::Session::new();
    let id = reopened.open("reopened.pdf", None, bytes, None).unwrap();
    assert_eq!(reopened.get(id).unwrap().tags().unwrap().elements.iter().find(|e| e.role == "H1").unwrap().title.as_deref(), Some("Overview"));
}

#[test]
fn reorder_indent_outdent_delete_and_undo() {
    let mut h = harness();
    assert_eq!(tag_ids(&h), [8, 9, 10]);
    menu(&mut h, "<P> Body", "Move up");
    assert_eq!(tag_ids(&h), [9, 8, 10]);
    menu(&mut h, "<P> Body", "Move down");
    assert_eq!(tag_ids(&h), [8, 9, 10]);
    menu(&mut h, "<P> Body", "Indent");
    assert_eq!(tree(&h).element(ObjRef::new(9, 0)).unwrap().parent, Some(ObjRef::new(8, 0)));
    menu(&mut h, "<P> Body", "Outdent");
    assert_eq!(tag_ids(&h), [8, 9, 10]);
    menu(&mut h, "<P> Body", "Delete tag…");
    h.get_by_label("Unwrap tag").click();
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8, 10]);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8, 9, 10]);
}

#[test]
fn keyboard_disclosure_navigation_and_property_focus() {
    let mut h = harness();
    h.get_by_label("<Document>").click();
    h.run_steps(2);
    h.key_press(Key::ArrowLeft);
    h.run_steps(2);
    assert!(h.query_by_label("<H1> Introduction").is_none());
    h.key_press(Key::ArrowRight);
    h.run_steps(2);
    h.get_by_label("<H1> Introduction");
    h.key_press(Key::End);
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert_eq!(h.state().views[0].current, 1);
    h.get_by_label("Title").click();
    h.key_press(Key::Home);
    h.key_press(Key::Delete);
    h.run_steps(2);
    assert_eq!(tree(&h).elements.len(), 4, "Delete in text editor does not delete a tag");
    h.get_by_label("Cancel properties").click();
    h.run_steps(2);
    assert_eq!(tree(&h).element(ObjRef::new(10, 0)).unwrap().title.as_deref(), Some("Chart"));
}

#[test]
fn new_child_filter_and_cancel_preserve_document() {
    let mut h = harness();
    menu(&mut h, "<Document>", "New child tag");
    h.get_by_label("Title").click();
    h.get_by_label("Title").type_text("New section");
    h.run_steps(2);
    h.get_by_label("Apply properties").click();
    h.run_steps(3);
    assert_eq!(tag_ids(&h).len(), 4);
    h.get_by_label("<P> New section");
    h.get_by_label("Find tags").click();
    h.get_by_label("Find tags").type_text("chart");
    h.run_steps(3);
    h.get_by_label("<Document>");
    h.get_by_label("<Figure> Chart");
    assert!(h.query_by_label("<P> Body").is_none());
    h.get_by_label("<Figure> Chart").click();
    h.run_steps(2);
    h.get_by_label("Alternate text").click();
    h.get_by_label("Alternate text").type_text(" discarded");
    h.run_steps(2);
    h.get_by_label("Cancel properties").click();
    h.run_steps(2);
    assert_eq!(tree(&h).element(ObjRef::new(10, 0)).unwrap().alt.as_deref(), Some("Blue chart"));
}

#[test]
fn dirty_reclick_and_stale_undo_do_not_discard_typed_properties() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.get_by_label("Title").click();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_label("Title").type_text("Unapplied draft");
    h.run_steps(2);
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    assert!(h.query_all_by_value("Unapplied draft").next().is_some());
    h.get_by_label("Apply properties").click();
    h.run_steps(3);
    h.get_by_label("<P> Unapplied draft").click();
    h.run_steps(2);
    h.get_by_label("Title").click();
    h.key_press(Key::End);
    h.get_by_label("Title").type_text(" pending");
    h.run_steps(2);
    let id = h.state().views[0].id;
    h.state_mut().session.undo(id).unwrap();
    h.run_steps(3);
    h.get_by_label("The document changed. Cancel to reload these properties.");
    assert_eq!(tree(&h).element(ObjRef::new(9, 0)).unwrap().title.as_deref(), Some("Body"));
    h.get_by_label("Cancel properties").click();
    h.run_steps(2);
    h.get_by_label("<P> Body");
}

#[test]
fn deleting_requires_current_named_target() {
    let mut h = harness();
    menu(&mut h, "<P> Body", "Delete tag…");
    h.get_by_label_contains("Selected for deletion: <P> Body");
    h.get_by_label("<Figure> Chart").click();
    h.run_steps(2);
    assert!(h.query_by_label("Delete subtree").is_none());
    assert_eq!(tag_ids(&h), [8, 9, 10]);
}

#[test]
fn drag_after_adjacent_sibling_preserves_mixed_child_order() {
    let mut h = harness();
    let from = h.get_by_label("<H1> Introduction").rect().center();
    let target = h.get_by_label("<P> Body").rect();
    let to = egui::pos2(target.center().x, target.bottom() - 2.0);
    h.hover_at(from);
    h.run_steps(1);
    h.drag_at(from);
    h.run_steps(1);
    h.hover_at(from + egui::vec2(8.0, 5.0));
    h.run_steps(2);
    h.hover_at(to);
    h.run_steps(2);
    h.drop_at(to);
    h.run_steps(4);
    assert_eq!(tag_ids(&h), [9, 8, 10]);
    assert!(matches!(tree(&h).element(ObjRef::new(8, 0)).unwrap().kids.first(), Some(TagKid::Mcid { id: 0, .. })));
}

#[test]
fn pick_painted_content_reveals_exact_owner_tag() {
    let mut h = harness();
    h.get_by_label("Select content on page").click();
    h.run_steps(2);
    let page = h.state().views[0].page_screen_rect(0).unwrap();
    // The fixture paints x20..220 and y320..360 in 300x400 PDF user space.
    let at = egui::pos2(page.left() + page.width() * 0.3, page.top() + page.height() * 0.15);
    h.hover_at(at);
    h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(4);
    assert!(h.query_all_by_value("Introduction").next().is_some());
    h.get_by_label("1 matching tags");
    h.get_by_label("Outlines show content selection bounds, not pixel-exact painted shapes.");
}

#[test]
fn property_drafts_and_selection_stay_with_their_document() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.get_by_label("Title").click();
    h.key_press(Key::End);
    h.get_by_label("Title").type_text(" first draft");
    h.run_steps(2);
    h.state_mut().open_bytes("other.pdf", None, TAGGED.to_vec()).unwrap();
    h.run_steps(4);
    assert!(h.query_by_label("Tag properties").is_none());
    h.get_by_label("<Figure> Chart").click();
    h.run_steps(2);
    assert!(h.query_all_by_value("Chart").next().is_some());
    h.state_mut().active = Some(0);
    h.run_steps(3);
    assert!(h.query_all_by_value("Body first draft").next().is_some());
    h.get_by_label("Cancel properties").click();
    h.run_steps(2);
    assert_eq!(tree(&h).element(ObjRef::new(9, 0)).unwrap().title.as_deref(), Some("Body"));
}

#[test]
fn large_tree_virtualizes_rows_and_search_retains_ancestors() {
    let mut pdf = String::from(
        "%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 400] >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n4 0 obj << /Type /StructTreeRoot /K 5 0 R >> endobj\n5 0 obj << /Type /StructElem /S /Document /P 4 0 R /K [",
    );
    for n in 0..10_000 {
        pdf.push_str(&format!("{} 0 R ", n + 6));
    }
    pdf.push_str("] >> endobj\n");
    for n in 0..10_000 {
        pdf.push_str(&format!("{} 0 obj << /Type /StructElem /S /P /P 5 0 R /T (Item {n}) >> endobj\n", n + 6));
    }
    pdf.push_str("trailer << /Root 1 0 R >>\n%%EOF");
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).build_eframe(move |_| {
        let mut app = PdfCraftApp::new();
        app.open_bytes("large-tags.pdf", None, pdf.into_bytes()).unwrap();
        app.set_option("panel", "tags").unwrap();
        app
    });
    h.run_steps(4);
    h.get_by_label("<P> Item 0").click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::End);
    h.run_steps(3);
    h.get_by_label("10000 tags selected");
    assert!(h.query_all_by_role(egui::accesskit::Role::TreeItem).count() < 60, "offscreen rows are not allocated or painted");
    h.get_by_label("Find tags").click();
    h.get_by_label("Find tags").type_text("Item 9999");
    h.run_steps(3);
    h.get_by_label("<Document>");
    h.get_by_label("<P> Item 9999");
    assert!(h.query_by_label("<P> Item 0").is_none());
}

#[test]
fn multiselect_unwrap_is_one_undoable_edit() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.get_by_label("<Figure> Chart").click_modifiers(Modifiers::COMMAND);
    h.run_steps(2);
    h.get_by_label("2 tags selected");
    assert!(h.query_by_label("Tag properties").is_none());
    assert_eq!(h.get_by_label("<P> Body").accesskit_node().is_selected(), Some(true));
    assert_eq!(h.get_by_label("<Figure> Chart").accesskit_node().is_selected(), Some(true));
    h.get_by_label("<Figure> Chart").click_secondary();
    h.run_steps(2);
    h.get_by_label("2 tags selected");
    h.get_by_label("Delete tag…").click();
    h.run_steps(3);
    h.get_by_label("Selected for deletion: <P> Body (9 0 R) + 1 tags");
    h.get_by_label("Unwrap tag").click();
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8], "toast: {:?}", h.state().toast);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8, 9, 10]);
}

#[test]
fn identifier_edit_persists_and_undoes() {
    let mut h = harness();
    h.get_by_label("<H1> Introduction").click();
    h.run_steps(2);
    h.get_by_label("Tag ID").click();
    h.get_by_label("Tag ID").type_text("intro-heading");
    h.run_steps(2);
    h.get_by_label("Apply properties").click();
    h.run_steps(3);
    assert_eq!(tree(&h).element(ObjRef::new(8, 0)).unwrap().identifier.as_deref(), Some("intro-heading"));
    let id = h.state().views[0].id;
    let bytes = h.state().session.save_bytes(id).unwrap();
    let mut reopened = pdfcraft_engine::Session::new();
    let other = reopened.open("saved.pdf", None, bytes, None).unwrap();
    assert!(reopened.get(other).unwrap().tags().unwrap().elements.iter().any(|e| e.identifier.as_deref() == Some("intro-heading")));
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert!(tree(&h).elements.iter().all(|e| e.identifier.is_none()));
}

#[test]
fn artifact_conversion_removes_exact_tag_and_undo_restores_content() {
    let mut h = harness();
    menu(&mut h, "<H1> Introduction", "Convert to artifact…");
    h.get_by_label("Convert marked content").click();
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [9, 10], "toast: {:?}", h.state().toast);
    let id = h.state().views[0].id;
    assert!(h.state().session.get(id).unwrap().tag_content(0).unwrap().items.is_empty());
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8, 9, 10]);
    assert_eq!(h.state().session.get(id).unwrap().tag_content(0).unwrap().items.len(), 1);
}

#[test]
fn shift_arrow_extends_selection_and_artifact_target_clears() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    h.run_steps(3);
    h.get_by_label("2 tags selected");
    h.get_by_label("<H1> Introduction").click();
    h.run_steps(2);
    menu(&mut h, "<H1> Introduction", "Convert to artifact…");
    h.get_by_label("Convert marked content");
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    assert!(h.query_by_label("Convert marked content").is_none());
}

#[test]
fn keyboard_context_menus_reach_tag_and_content_moves() {
    let mut h = harness();
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.key_press(Key::F2);
    h.run_steps(3);
    assert!(h.get_by_label("Type").accesskit_node().is_focused());
    h.get_by_label("<P> Body").click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::F10);
    h.run_steps(3);
    assert!(h.get_by_label("Properties").accesskit_node().is_focused());
    for _ in 0..12 {
        if h.get_by_label("Move selected tags…").accesskit_node().is_focused() {
            break;
        }
        h.key_press(Key::Tab);
        h.run_steps(2);
    }
    assert!(h.get_by_label("Move selected tags…").accesskit_node().is_focused());
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("Find destination");
    h.key_press(Key::Escape);
    h.run_steps(3);
    h.get_by_label("Content · MCID 0").click();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::F10);
    h.run_steps(3);
    assert!(h.get_by_label("Move content to…").accesskit_node().is_focused());
    h.key_press(Key::Enter);
    h.run_steps(3);
    h.get_by_label("Find destination");
    h.key_press(Key::Escape);
    h.run_steps(3);
    assert_eq!(tag_ids(&h), [8, 9, 10]);
}
