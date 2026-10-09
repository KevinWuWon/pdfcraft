//! Native dialogs for structure-wide role mappings and precise mixed-child moves.
//! Returning an edit does not discard a draft: callers close it only after engine success.
use crate::{i18n, theme::Tokens, widgets};
use pdfcraft_engine::a11y::tags::{TagKid, TagTree};
use pdfcraft_engine::{Document, Edit, ObjRef};
use std::collections::{HashMap, HashSet};

const STANDARD_ROLES: &[&str] = &[
    "Document",
    "DocumentFragment",
    "Part",
    "Art",
    "Sect",
    "Div",
    "BlockQuote",
    "Caption",
    "TOC",
    "TOCI",
    "Index",
    "NonStruct",
    "Private",
    "Aside",
    "Title",
    "FENote",
    "P",
    "H",
    "H1",
    "H2",
    "H3",
    "H4",
    "H5",
    "H6",
    "L",
    "LI",
    "Lbl",
    "LBody",
    "Table",
    "TR",
    "TH",
    "TD",
    "THead",
    "TBody",
    "TFoot",
    "Span",
    "Quote",
    "Note",
    "Reference",
    "BibEntry",
    "Code",
    "Link",
    "Annot",
    "Ruby",
    "RB",
    "RT",
    "RP",
    "Warichu",
    "WT",
    "WP",
    "Figure",
    "Formula",
    "Form",
    "Sub",
    "Em",
    "Strong",
    "Artifact",
];

#[derive(Default)]
pub(crate) struct RoleMapState {
    pub(crate) open: bool,
    pub(crate) generation: u64,
    pub(crate) identity_generation: u64,
    pub(crate) role: String,
    pub(crate) target: String,
    pub(crate) selected: Option<String>,
    pub(crate) error: Option<String>,
}
impl RoleMapState {
    pub(crate) fn begin(doc: &Document) -> Self {
        Self { open: true, generation: doc.edit_generation(), identity_generation: doc.object_identity_generation(), ..Self::default() }
    }
    pub(crate) fn applied(&mut self) {
        *self = Self::default();
    }
}

/// The engine validates role names, mapping chains, and the complete document on apply.
pub(crate) fn role_map(ctx: &egui::Context, doc: &Document, tree: &TagTree, state: &mut RoleMapState) -> Option<Edit> {
    if !state.open {
        return None;
    }
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        state.open = false;
        return None;
    }
    let mut open = state.open;
    let mut cancel = false;
    let mut edit = None;
    let stale = state.generation != doc.edit_generation() || state.identity_generation != doc.object_identity_generation();
    egui::Window::new(tl!("Edit role mappings"))
        .id(egui::Id::new("tag_role_map_dialog"))
        .open(&mut open)
        .collapsible(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            let tokens = Tokens::get(ctx);
            ui.label(tl!("Map a custom tag role to a standard structure type. Each mapping affects every tag using that role in this document."));
            ui.add_space(8.0);
            if tree.role_map.is_empty() {
                ui.weak(tl!("No role mappings yet."));
            }
            egui::ScrollArea::vertical().id_salt("role_mapping_list").max_height(180.0).show(ui, |ui| {
                for (role, target) in tree.role_map.iter().take(100_000) {
                    if ui.selectable_label(state.selected.as_ref() == Some(role), format!("{role} → {target}")).clicked() {
                        state.selected = Some(role.clone());
                        state.role.clone_from(role);
                        state.target.clone_from(target);
                        state.error = None;
                    }
                }
            });
            ui.separator();
            egui::Grid::new("role_mapping_fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                let label = ui.label(tl!("Custom role"));
                ui.add(egui::TextEdit::singleline(&mut state.role).hint_text(tl!("For example: ChapterTitle")).char_limit(4096))
                    .labelled_by(label.id);
                ui.end_row();
                let label = ui.label(tl!("Maps to"));
                ui.add(egui::TextEdit::singleline(&mut state.target).hint_text(tl!("For example: H1")).char_limit(4096)).labelled_by(label.id);
                ui.end_row();
            });
            egui::ComboBox::from_label(tl!("Standard type")).selected_text(tl!("Choose a standard type…")).show_ui(ui, |ui| {
                for role in STANDARD_ROLES {
                    if ui.selectable_label(state.target == *role, *role).clicked() {
                        state.target = (*role).to_owned();
                    }
                }
            });
            let role = state.role.trim();
            let affected = tree.elements.iter().filter(|element| element.role == role).count();
            ui.colored_label(
                tokens.text_muted,
                i18n::fmt(tl!("{affected} tag(s) currently use this custom role."), &[("affected", &affected.to_string())]),
            );
            if stale {
                ui.label(tl!(
                    "The document changed while this dialog was open. Your draft is kept. Reopen the dialog to work with the current document."
                ));
            }
            if let Some(error) = &state.error {
                ui.label(i18n::fmt(tl!("Could not apply: {error}"), &[("error", error)]));
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled_ui(!stale && !role.is_empty() && !state.target.trim().is_empty(), |ui| {
                        widgets::pill_button(ui, tl!("Apply mapping"), true)
                    })
                    .inner
                    .clicked()
                {
                    edit = Some(Edit::SetTagRoleMapping { role: role.to_owned(), target: Some(state.target.trim().to_owned()) });
                }
                if ui.add_enabled(!stale && tree.role_map.contains_key(role), egui::Button::new(tl!("Remove mapping"))).clicked() {
                    edit = Some(Edit::SetTagRoleMapping { role: role.to_owned(), target: None });
                }
                if ui.button(tl!("Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
    state.open = open && !cancel;
    edit
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MoveSource {
    Tags(Vec<ObjRef>),
    Content { source: ObjRef, index: usize },
}

#[derive(Default)]
pub(crate) struct MoveState {
    pub(crate) open: bool,
    pub(crate) generation: u64,
    pub(crate) identity_generation: u64,
    pub(crate) source: Option<MoveSource>,
    pub(crate) destination: Option<ObjRef>,
    /// One-based insertion position in the destination's mixed children after removal.
    pub(crate) position: usize,
    pub(crate) search: String,
    pub(crate) error: Option<String>,
}
impl MoveState {
    pub(crate) fn begin(doc: &Document, source: MoveSource, destination: Option<ObjRef>, position: usize) -> Self {
        Self {
            open: true,
            generation: doc.edit_generation(),
            identity_generation: doc.object_identity_generation(),
            source: Some(source),
            destination,
            position: position.max(1),
            ..Self::default()
        }
    }
    pub(crate) fn applied(&mut self) {
        *self = Self::default();
    }
}

fn children(tree: &TagTree, parent: Option<ObjRef>) -> Option<&[TagKid]> {
    match parent {
        None => Some(&tree.children),
        Some(id) => tree.element(id).map(|element| element.kids.as_slice()),
    }
}

fn move_limit(tree: &TagTree, source: &MoveSource, destination: Option<ObjRef>) -> Option<usize> {
    let kids = children(tree, destination)?;
    let removed = match source {
        MoveSource::Tags(ids) => {
            let ids: HashSet<_> = ids.iter().copied().collect();
            kids.iter().filter(|kid| matches!(kid, TagKid::Element { id } if ids.contains(id))).count()
        }
        MoveSource::Content { source, .. } => usize::from(Some(*source) == destination),
    };
    Some(kids.len().saturating_sub(removed).saturating_add(1))
}

/// Reject descendants and self using full object references, with a cycle guard.
fn eligible_destination(tree: &TagTree, source: &MoveSource, destination: Option<ObjRef>) -> bool {
    let MoveSource::Tags(ids) = source else {
        return destination.is_some_and(|id| tree.element(id).is_some());
    };
    let mut next = destination;
    let mut seen = HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) || ids.contains(&id) {
            return false;
        }
        let Some(element) = tree.element(id) else { return false };
        next = element.parent;
    }
    true
}

fn valid_source(tree: &TagTree, source: &MoveSource) -> bool {
    match source {
        MoveSource::Tags(ids) => !ids.is_empty() && ids.iter().all(|id| tree.element(*id).is_some()),
        MoveSource::Content { source, index } => tree
            .element(*source)
            .and_then(|e| e.kids.get(*index))
            .is_some_and(|kid| !matches!(kid, TagKid::Element { .. } | TagKid::Unsupported { .. })),
    }
}

pub(crate) fn move_destination(ctx: &egui::Context, doc: &Document, tree: &TagTree, state: &mut MoveState) -> Option<Edit> {
    if !state.open {
        return None;
    }
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        state.open = false;
        return None;
    }
    let source = state.source.as_ref()?;
    let stale = state.generation != doc.edit_generation() || state.identity_generation != doc.object_identity_generation();
    // Compute excluded subtrees once, rather than walking every ancestor for every row.
    let mut excluded = HashSet::new();
    if let MoveSource::Tags(ids) = source {
        let elements: HashMap<_, _> = tree.elements.iter().map(|element| (element.id, element)).collect();
        let mut pending = ids.clone();
        while let Some(id) = pending.pop() {
            if !excluded.insert(id) {
                continue;
            }
            if let Some(element) = elements.get(&id) {
                pending.extend(element.kids.iter().filter_map(|kid| match kid {
                    TagKid::Element { id } => Some(*id),
                    _ => None,
                }));
            }
        }
    }
    let mut open = state.open;
    let mut cancel = false;
    let mut edit = None;
    egui::Window::new(tl!("Move to tag"))
        .id(egui::Id::new("tag_move_destination_dialog"))
        .open(&mut open)
        .collapsible(false)
        .default_width(480.0)
        .show(ctx, |ui| {
            match source {
                MoveSource::Tags(ids) => { ui.label(i18n::fmt(tl!("Move {count} selected tag(s), keeping their subtrees and content."), &[("count", &ids.len().to_string())])); }
                MoveSource::Content { source, index } => { ui.label(i18n::fmt(tl!("Move content child {index} from tag {source}."), &[("index", &index.saturating_add(1).to_string()), ("source", &source.to_string())])); }
            }
            let search_label = ui.label(tl!("Find destination"));
            ui.add(egui::TextEdit::singleline(&mut state.search).hint_text(tl!("Find destination by role, title, or object reference")).char_limit(4096)).labelled_by(search_label.id);
            let query = state.search.trim().to_lowercase();
            egui::ScrollArea::vertical().id_salt("tag_destination_list").max_height(220.0).show(ui, |ui| {
                if matches!(source, MoveSource::Tags(_)) && ui.selectable_label(state.destination.is_none(), tl!("Structure tree root")).clicked() {
                    state.destination = None;
                }
                let mut shown = 0usize;
                for element in &tree.elements {
                    let label = format!("<{}> {} · {}", element.role, element.title.as_deref().unwrap_or(""), element.id);
                    if !label.to_lowercase().contains(&query) || excluded.contains(&element.id) {
                        continue;
                    }
                    if shown >= 500 {
                        ui.weak(tl!("More destinations available. Refine your search."));
                        break;
                    }
                    shown = shown.saturating_add(1);
                    if ui.selectable_label(state.destination == Some(element.id), label).clicked() {
                        state.destination = Some(element.id);
                    }
                }
            });
            let destination_name = match state.destination {
                Some(id) => tree.element(id).map_or_else(|| i18n::fmt(tl!("Unavailable tag {id}"), &[("id", &id.to_string())]), |e| format!("<{}> · {id}", e.role)),
                None if matches!(source, MoveSource::Tags(_)) => tl!("Structure tree root").to_owned(),
                None => tl!("Choose a destination tag").to_owned(),
            };
            ui.label(i18n::fmt(tl!("Destination: {destination_name}"), &[("destination_name", &destination_name)]));
            let limit = move_limit(tree, source, state.destination).unwrap_or(1);
            ui.horizontal(|ui| {
                let position_label = ui.label(tl!("Position (1-based)"));
                // Do not clamp a retained draft when the destination/document changes.
                ui.add(egui::DragValue::new(&mut state.position).range(1..=usize::MAX)).labelled_by(position_label.id);
                ui.weak(i18n::fmt(tl!("Valid range: 1–{limit}"), &[("limit", &limit.to_string())]));
            });
            ui.weak(tl!("Positions count all siblings, including content leaves. The position is measured after the moving items are removed; the last position appends."));
            let eligible = eligible_destination(tree, source, state.destination);
            let source_exists = valid_source(tree, source);
            if stale {
                ui.label(tl!("The document changed. This move draft is kept, but reopen the dialog before applying so content positions and tag identities can be checked again."));
            } else if !source_exists {
                ui.label(tl!("The original selection is no longer available."));
            } else if !eligible {
                ui.label(tl!("Choose a destination outside the moving subtrees. Content must belong to a tag."));
            }
            if let Some(error) = &state.error {
                ui.label(i18n::fmt(tl!("Could not move: {error}"), &[("error", error)]));
            }
            ui.separator();
            ui.horizontal(|ui| {
                let valid = !stale && source_exists && eligible && state.position >= 1 && state.position <= limit;
                if ui.add_enabled_ui(valid, |ui| widgets::pill_button(ui, tl!("Move"), true)).inner.clicked() {
                    let index = state.position.saturating_sub(1);
                    edit = match source {
                        MoveSource::Tags(ids) => Some(Edit::MoveTags { ids: ids.clone(), parent: state.destination, index }),
                        MoveSource::Content { source, index: source_index } => state.destination.map(|destination| Edit::MoveTagContent { source: *source, source_index: *source_index, destination, index }),
                    };
                }
                if ui.button(tl!("Cancel")).clicked() { cancel = true; }
            });
        });
    state.open = open && !cancel;
    edit
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_engine::a11y::tags::TagElement;
    fn tag(id: ObjRef, parent: Option<ObjRef>, kids: Vec<TagKid>) -> TagElement {
        TagElement {
            id,
            parent,
            role: "P".into(),
            standard_role: "P".into(),
            page: None,
            page_index: None,
            expansion: None,
            identifier: None,
            title: None,
            alt: None,
            actual_text: None,
            lang: None,
            kids,
        }
    }
    #[test]
    fn move_positions_count_mixed_children_after_removal() {
        let parent = ObjRef::new(10, 2);
        let child = ObjRef::new(11, 3);
        let tree = TagTree {
            children: vec![TagKid::Element { id: parent }],
            elements: vec![
                tag(parent, None, vec![TagKid::Mcid { id: 0, page: None }, TagKid::Element { id: child }, TagKid::Mcid { id: 1, page: None }]),
                tag(child, Some(parent), vec![]),
            ],
            ..TagTree::default()
        };
        assert_eq!(move_limit(&tree, &MoveSource::Tags(vec![child]), Some(parent)), Some(3));
        assert_eq!(move_limit(&tree, &MoveSource::Content { source: parent, index: 0 }, Some(parent)), Some(3));
        assert!(!eligible_destination(&tree, &MoveSource::Tags(vec![parent]), Some(child)));
        assert!(eligible_destination(&tree, &MoveSource::Tags(vec![child]), None));
        assert!(!eligible_destination(&tree, &MoveSource::Content { source: parent, index: 0 }, None));
        assert!(!valid_source(&tree, &MoveSource::Tags(vec![ObjRef::new(11, 0)])));
        assert!(!valid_source(&tree, &MoveSource::Content { source: parent, index: 1 }));
        assert!(!valid_source(&tree, &MoveSource::Content { source: parent, index: usize::MAX }));
    }
}
