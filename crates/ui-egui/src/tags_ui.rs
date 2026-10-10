//! Accessibility structure editing. The tree is a cached projection, never the PDF model.
//! All document changes go through engine edits; drafts and focus belong to each document.

use crate::{
    icons,
    panels::Nav,
    theme::{self, Tokens},
    widgets,
};
use egui::{Key, Sense};
use pdfcraft_engine::a11y::tags::{DeleteMode, PDF_17_NAMESPACE, PDF_20_NAMESPACE, TagElement, TagKid, TagProperties, TagTree, is_standard_role};
use pdfcraft_engine::{Document, Edit, ObjRef};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Node {
    Tag(ObjRef),
    Content(Option<ObjRef>, usize),
}

#[derive(Clone)]
struct Row {
    node: Node,
    parent: Option<ObjRef>,
    index: usize,
    depth: usize,
    label: String,
    page: Option<usize>,
    expandable: bool,
}

#[derive(Clone, PartialEq)]
struct Fields {
    role: String,
    title: String,
    alt: String,
    actual_text: String,
    lang: String,
    expansion: String,
    identifier: String,
}

impl Fields {
    fn from_element(e: &TagElement) -> Self {
        Self {
            role: e.role.clone(),
            title: e.title.clone().unwrap_or_default(),
            alt: e.alt.clone().unwrap_or_default(),
            actual_text: e.actual_text.clone().unwrap_or_default(),
            lang: e.lang.clone().unwrap_or_default(),
            expansion: e.expansion.clone().unwrap_or_default(),
            identifier: e.identifier.clone().unwrap_or_default(),
        }
    }
    fn patch(&self, original: Option<&Self>) -> TagProperties {
        let changed = |value: &String, old: Option<&String>| {
            if old == Some(value) { None } else { Some((!value.is_empty()).then(|| value.clone())) }
        };
        TagProperties {
            role: (original.map(|o| &o.role) != Some(&self.role)).then(|| self.role.trim().to_owned()),
            title: changed(&self.title, original.map(|o| &o.title)),
            alt: changed(&self.alt, original.map(|o| &o.alt)),
            actual_text: changed(&self.actual_text, original.map(|o| &o.actual_text)),
            lang: changed(&self.lang, original.map(|o| &o.lang)),
            expansion: changed(&self.expansion, original.map(|o| &o.expansion)),
        }
    }
}

enum Target {
    Existing(ObjRef),
    New { parent: Option<ObjRef>, index: usize },
}
struct Draft {
    target: Target,
    fields: Fields,
    original: Option<Fields>,
    generation: u64,
}
impl Draft {
    fn changed(&self) -> bool {
        self.original.as_ref() != Some(&self.fields)
    }
}

#[derive(Default)]
pub(crate) struct State {
    generation: Option<u64>,
    identity_generation: Option<u64>,
    tree: Option<TagTree>,
    error: Option<String>,
    expanded: HashSet<ObjRef>,
    selected: Option<Node>,
    selected_tags: HashSet<ObjRef>,
    anchor: Option<Node>,
    focused_row: Option<Node>,
    pending_focus: Option<Node>,
    focus_properties: bool,
    filter: String,
    search_open: bool,
    search_focus: bool,
    properties_open: bool,
    warnings_open: bool,
    show_content: bool,
    initialized: bool,
    draft: Option<Draft>,
    delete: Option<(Vec<ObjRef>, u64)>,
    artifact: Option<(Vec<ObjRef>, u64)>,
    role_map: crate::tag_dialogs::RoleMapState,
    move_dialog: crate::tag_dialogs::MoveState,
    notice: Option<String>,
    reveal: bool,
    geometry: HashMap<usize, Result<pdfcraft_render::tagged::PageTagContent, String>>,
    highlight: bool,
    highlighted: Option<(Option<Node>, usize, bool)>,
    pub(crate) marks: Vec<(usize, [f32; 4])>,
    pub(crate) pick_content: bool,
    pub(crate) hit_request: Option<(usize, Vec<[f32; 4]>)>,
    matches: Vec<ObjRef>,
    geometry_notice: Option<String>,
}

#[derive(Clone)]
struct DragTag {
    document: pdfcraft_engine::DocId,
    generation: u64,
    node: Node,
    tags: Vec<ObjRef>,
}

fn drop_edit(tree: &TagTree, payload: &DragTag, parent: Option<ObjRef>, index: usize) -> Option<Edit> {
    match payload.node {
        Node::Tag(_) => {
            let removed_before =
                children(tree, parent).iter().take(index).filter(|k| matches!(k,TagKid::Element{id} if payload.tags.contains(id))).count();
            Some(Edit::MoveTags { ids: payload.tags.clone(), parent, index: index.saturating_sub(removed_before) })
        }
        Node::Content(Some(source), source_index) => {
            let destination = parent?;
            Some(Edit::MoveTagContent {
                source,
                source_index,
                destination,
                index: index.saturating_sub(usize::from(source == destination && source_index < index)),
            })
        }
        _ => None,
    }
}

fn top_level_selected(tree: &TagTree, selected: &HashSet<ObjRef>) -> Vec<ObjRef> {
    let parents: HashMap<_, _> = tree.elements.iter().map(|e| (e.id, e.parent)).collect();
    tree.elements
        .iter()
        .filter(|e| selected.contains(&e.id))
        .filter(|e| {
            let mut parent = e.parent;
            let mut seen = HashSet::new();
            while let Some(id) = parent {
                if !seen.insert(id) {
                    return false;
                }
                if selected.contains(&id) {
                    return false;
                }
                parent = parents.get(&id).copied().flatten();
            }
            true
        })
        .map(|e| e.id)
        .collect()
}

fn children(tree: &TagTree, parent: Option<ObjRef>) -> &[TagKid] {
    match parent {
        Some(id) => tree.elements.iter().find(|e| e.id == id).map_or(&[], |e| e.kids.as_slice()),
        None => &tree.children,
    }
}
fn element(tree: &TagTree, id: ObjRef) -> Option<&TagElement> {
    tree.elements.iter().find(|e| e.id == id)
}

fn rows(tree: &TagTree, state: &State) -> Vec<Row> {
    let elements: HashMap<ObjRef, &TagElement> = tree.elements.iter().map(|e| (e.id, e)).collect();
    let query = state.filter.trim().to_lowercase();
    let mut included = HashSet::new();
    if !query.is_empty() {
        for e in &tree.elements {
            let text = format!(
                "{} {} {} {} {}",
                e.role,
                e.standard_role,
                e.title.as_deref().unwrap_or(""),
                e.alt.as_deref().unwrap_or(""),
                e.actual_text.as_deref().unwrap_or("")
            );
            if text.to_lowercase().contains(&query) {
                let mut next = Some(e.id);
                while let Some(id) = next {
                    if !included.insert(id) {
                        break;
                    }
                    next = elements.get(&id).and_then(|e| e.parent);
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut stack: Vec<_> = tree.children.iter().enumerate().rev().map(|(i, k)| (None, i, 0, k)).collect();
    let mut seen = HashSet::new();
    while let Some((parent, index, depth, kid)) = stack.pop() {
        if out.len() >= 100_000 {
            break;
        }
        match kid {
            TagKid::Element { id } => {
                if !seen.insert(*id) || (!query.is_empty() && !included.contains(id)) {
                    continue;
                }
                let Some(e) = elements.get(id) else {
                    continue;
                };
                let label = match e.title.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => format!("<{}> {}", e.role, t),
                    None => format!("<{}>", e.role),
                };
                out.push(Row { node: Node::Tag(*id), parent, index, depth, label, page: e.page_index, expandable: !e.kids.is_empty() });
                if state.expanded.contains(id) || !query.is_empty() {
                    stack.extend(e.kids.iter().enumerate().rev().map(|(i, k)| (Some(*id), i, depth.saturating_add(1), k)));
                }
            }
            _ if state.show_content && query.is_empty() => {
                let (label, page) = match kid {
                    TagKid::Mcid { id, page, .. } => (
                        crate::i18n::fmt(tl!("Content · MCID {id}"), &[("id", &id.to_string())]),
                        page.and_then(|p| tree.pages.iter().position(|r| *r == p)),
                    ),
                    TagKid::Mcr { id, page, stream, .. } => (
                        crate::i18n::fmt(
                            if stream.is_some() { tl!("Content · MCID {id} · stream") } else { tl!("Content · MCID {id}") },
                            &[("id", &id.to_string())],
                        ),
                        page.and_then(|p| tree.pages.iter().position(|r| *r == p)),
                    ),
                    TagKid::Objr { object, page, .. } => (
                        crate::i18n::fmt(tl!("Object reference · {object}"), &[("object", &object.to_string())]),
                        page.and_then(|p| tree.pages.iter().position(|r| *r == p)),
                    ),
                    TagKid::Unsupported { description } => {
                        (crate::i18n::fmt(tl!("Unsupported · {description}"), &[("description", description)]), None)
                    }
                    TagKid::Element { .. } => continue,
                };
                let preview = page
                    .and_then(|p| state.geometry.get(&p))
                    .and_then(|g| g.as_ref().ok())
                    .and_then(|g| match kid {
                        TagKid::Mcid { id, .. } => g.items.iter().find(|c| c.mcid == *id && c.stream.is_none()),
                        TagKid::Mcr { id, stream, .. } => g.items.iter().find(|c| c.mcid == *id && c.stream == stream.map(|s| (s.num, s.generation))),
                        _ => None,
                    })
                    .map(|c| c.text.chars().take(80).collect::<String>())
                    .filter(|s| !s.is_empty());
                let label = preview.map_or(label.clone(), |text| format!("{text} · {label}"));
                out.push(Row { node: Node::Content(parent, index), parent, index, depth, label, page, expandable: false });
            }
            _ => {}
        }
    }
    out
}

fn load_draft(state: &mut State, tree: &TagTree, node: Node, generation: u64) {
    if state.draft.as_ref().is_some_and(Draft::changed) {
        state.notice = Some(tl!("Apply or cancel the property changes before selecting another tag.").into());
        return;
    }
    if state.selected != Some(node) {
        state.delete = None;
        state.artifact = None;
    }
    state.selected = Some(node);
    state.selected_tags = match node {
        Node::Tag(id) => [id].into(),
        _ => HashSet::new(),
    };
    state.notice = None;
    state.draft = match node {
        Node::Tag(id) => element(tree, id).map(|e| {
            let fields = Fields::from_element(e);
            Draft { target: Target::Existing(id), original: Some(fields.clone()), fields, generation }
        }),
        Node::Content(..) => None,
    };
}

fn create_draft(state: &mut State, parent: Option<ObjRef>, index: usize, generation: u64) {
    if state.draft.as_ref().is_some_and(Draft::changed) {
        state.notice = Some(tl!("Apply or cancel the property changes first.").into());
        return;
    }
    state.draft = Some(Draft {
        target: Target::New { parent, index },
        fields: Fields {
            role: "P".into(),
            title: String::new(),
            alt: String::new(),
            actual_text: String::new(),
            lang: String::new(),
            expansion: String::new(),
            identifier: String::new(),
        },
        original: None,
        generation,
    });
    state.notice = None;
    state.properties_open = true;
    state.focus_properties = true;
}

fn move_edit(tree: &TagTree, row: &Row, direction: &str) -> Option<Edit> {
    let Node::Tag(id) = row.node else {
        return None;
    };
    let siblings = children(tree, row.parent);
    let (parent, index) = match direction {
        "up" if row.index > 0 => (row.parent, row.index - 1),
        "down" if row.index + 1 < siblings.len() => (row.parent, row.index + 1),
        "in" if row.index > 0 => {
            let TagKid::Element { id: parent } = siblings.get(row.index - 1)? else {
                return None;
            };
            (Some(*parent), children(tree, Some(*parent)).len())
        }
        "out" => {
            let parent = element(tree, row.parent?)?;
            let index = children(tree, parent.parent).iter().position(|k| matches!(k,TagKid::Element{id} if *id==parent.id))?;
            (parent.parent, index + 1)
        }
        _ => return None,
    };
    Some(Edit::MoveTag { id, parent, index })
}

/// Candidate inventory spans both PDF namespaces. The engine remains authoritative about
/// membership; custom and MathML roles stay manually editable.
const PDF_STANDARD_TYPE_CANDIDATES: &[&str] = &[
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

fn standard_type_suggestions(tree: &TagTree, target: &Target) -> Vec<&'static str> {
    let namespace = match target {
        // NS is never inherited from a new tag's parent.
        Target::New { .. } => Some(PDF_17_NAMESPACE),
        Target::Existing(id) => {
            tree.element(*id).and_then(|e| if e.namespace.is_none() { Some(PDF_17_NAMESPACE) } else { e.namespace_uri.as_deref() })
        }
    };
    match namespace {
        Some(namespace @ (PDF_17_NAMESPACE | PDF_20_NAMESPACE)) => {
            PDF_STANDARD_TYPE_CANDIDATES.iter().copied().filter(|role| is_standard_role(role, namespace)).collect()
        }
        _ => Vec::new(),
    }
}

fn properties(ctx: &egui::Context, state: &mut State, tree: &TagTree, editable: bool, structure_editable: bool, generation: u64) -> Option<Edit> {
    if !state.properties_open {
        return None;
    }
    let focus_type = state.focus_properties;
    let mut focus_acquired = false;
    if focus_type {
        state.pending_focus = None;
    }
    let mut edit = None;
    let mut cancel = false;
    let mut open = true;
    let escape = !egui::Popup::is_any_open(ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
    let Some(d) = &mut state.draft else {
        state.properties_open = false;
        return None;
    };
    let stale = d.generation != generation;
    let standard_types = standard_type_suggestions(tree, &d.target);
    let title = if matches!(d.target, Target::New { .. }) { tl!("New tag") } else { tl!("Tag properties") };
    let max_height = (ctx.content_rect().height() - 190.0).clamp(100.0, 470.0);
    egui::Window::new(title)
        .id(egui::Id::new("tag-properties-window"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .default_pos(ctx.content_rect().center() - egui::vec2(260.0, 200.0))
        .min_width(350.0)
        .show(ctx, |ui| {
            if let Target::Existing(id) = d.target {
                let target = format!(
                    "<{}> {} · {id}",
                    d.original.as_ref().map_or(d.fields.role.as_str(), |o| o.role.as_str()),
                    d.original.as_ref().map_or("", |o| o.title.as_str())
                );
                ui.add(egui::Label::new(egui::RichText::new(&target).strong()).truncate()).on_hover_text(&target);
                if let Some(e) = tree.element(id) {
                    if let Some(namespace) = &e.namespace_uri {
                        ui.add(egui::Label::new(egui::RichText::new(format!("{} {}", tl!("Namespace:"), namespace)).small()).truncate())
                            .on_hover_text(namespace);
                    }
                    if e.role != e.standard_role {
                        ui.small(format!("{} {}", tl!("Mapped type:"), e.standard_role));
                    }
                }
            }
            if stale {
                ui.label(tl!("The document changed. Cancel to reload these properties."));
            }
            if let Some(notice) = &state.notice {
                ui.label(notice);
            }
            ui.separator();
            egui::ScrollArea::vertical().id_salt("tag-properties-fields").max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
                // Native TextEdit retains its focused stroke; unfocused fields use shared tokens.
                let tokens = Tokens::get(ui.ctx());
                ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::new(1.0, tokens.border);
                ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::new(1.0, tokens.accent);
                ui.add_enabled_ui(editable || stale, |ui| {
                    egui::Grid::new("tag-property-grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                        let label = ui.label(tl!("Type"));
                        ui.add_enabled_ui(structure_editable && !stale, |ui| {
                            ui.horizontal(|ui| {
                                let input = ui.add(egui::TextEdit::singleline(&mut d.fields.role).desired_width(110.0)).labelled_by(label.id);
                                if focus_type && input.enabled() {
                                    focus_acquired = input.has_focus();
                                    input.request_focus();
                                }
                                let combo = ui
                                    .add_enabled_ui(!standard_types.is_empty(), |ui| {
                                        egui::ComboBox::from_id_salt("tag-types").selected_text(tl!("Standard types")).show_ui(ui, |ui| {
                                            for role in &standard_types {
                                                ui.selectable_value(&mut d.fields.role, (*role).into(), *role);
                                            }
                                        })
                                    })
                                    .inner;
                                combo.response.widget_info(|| {
                                    egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, combo.response.enabled(), tl!("Standard types"))
                                });
                            });
                            if standard_types.is_empty() {
                                ui.small(tl!("No PDF standard suggestions for this namespace. Enter a type defined by its namespace."));
                            }
                        });
                        ui.end_row();
                        for (label, value, multiline) in [
                            ("Title", &mut d.fields.title, false),
                            ("Alternate text", &mut d.fields.alt, true),
                            ("Actual text", &mut d.fields.actual_text, true),
                            ("Language", &mut d.fields.lang, false),
                            ("Abbreviation expansion", &mut d.fields.expansion, false),
                        ] {
                            let focus_field = focus_type && (!structure_editable || stale) && label == "Title";
                            let label = ui.label(tl!(label));
                            let response = if multiline {
                                ui.add(egui::TextEdit::multiline(value).desired_rows(2).desired_width(320.0))
                            } else {
                                ui.add(egui::TextEdit::singleline(value).desired_width(320.0))
                            };
                            if focus_field && response.enabled() {
                                focus_acquired = response.has_focus();
                                response.request_focus();
                            }
                            response.labelled_by(label.id);
                            ui.end_row();
                        }
                        if matches!(d.target, Target::Existing(_)) {
                            let label = ui.label(tl!("Tag ID"));
                            ui.add_enabled(structure_editable && !stale, egui::TextEdit::singleline(&mut d.fields.identifier).desired_width(320.0))
                                .labelled_by(label.id)
                                .on_hover_text(tl!("Changing an ID can break external links that use the old ID."));
                            ui.end_row();
                        }
                    });
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled_ui(!stale && editable && d.changed() && !d.fields.role.trim().is_empty(), |ui| {
                        widgets::pill_button(ui, tl!("Apply properties"), true)
                    })
                    .inner
                    .clicked()
                {
                    let properties = d.fields.patch(d.original.as_ref());
                    edit = Some(match d.target {
                        Target::Existing(id) => {
                            let update = Edit::UpdateTag { id, properties };
                            if d.original.as_ref().is_some_and(|o| o.identifier != d.fields.identifier) {
                                Edit::Batch {
                                    label: "Edit tag properties".into(),
                                    edits: vec![
                                        update,
                                        Edit::SetTagIdentifier {
                                            id,
                                            identifier: (!d.fields.identifier.is_empty()).then(|| d.fields.identifier.clone()),
                                        },
                                    ],
                                }
                            } else {
                                update
                            }
                        }
                        Target::New { parent, index } => Edit::CreateTag { parent, index, properties },
                    });
                }
                if widgets::pill_button(ui, tl!("Cancel properties"), false).clicked() {
                    cancel = true;
                }
            });
        });
    // A native Window may discard a sizing pass. Keep focus pending until an input owns it.
    if focus_type {
        state.focus_properties = !focus_acquired;
    }
    if !open || escape {
        if d.changed() {
            state.notice = Some(tl!("Unsaved property changes. Apply them or choose Cancel properties to discard them.").into());
        } else {
            cancel = true;
        }
    }
    if cancel {
        state.draft = None;
        state.notice = None;
        state.properties_open = false;
        state.focus_properties = false;
        state.pending_focus = state.selected;
        state.reveal = true;
    }
    edit
}

fn kid_page(tree: &TagTree, kid: &TagKid) -> Option<usize> {
    let page = match kid {
        TagKid::Mcid { page, .. } | TagKid::Mcr { page, .. } | TagKid::Objr { page, .. } => *page,
        _ => None,
    }?;
    tree.pages.iter().position(|p| *p == page)
}

fn content_kids(tree: &TagTree, node: Node) -> Vec<(ObjRef, &TagKid)> {
    if let Node::Content(Some(parent), index) = node {
        return children(tree, Some(parent)).get(index).map(|k| vec![(parent, k)]).unwrap_or_default();
    }
    let Node::Tag(id) = node else {
        return Vec::new();
    };
    content_kids_for_roots(tree, vec![id])
}

fn content_kids_for_roots(tree: &TagTree, mut stack: Vec<ObjRef>) -> Vec<(ObjRef, &TagKid)> {
    let map: HashMap<_, _> = tree.elements.iter().map(|e| (e.id, e)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(e) = map.get(&id) {
            for k in &e.kids {
                match k {
                    TagKid::Element { id } => stack.push(*id),
                    _ => out.push((e.id, k)),
                }
            }
        }
    }
    out
}

fn geometry<'a>(state: &'a mut State, doc: &Document, page: usize) -> &'a Result<pdfcraft_render::tagged::PageTagContent, String> {
    // The cache is document/generation scoped and bounded independently of PDF page count.
    if state.geometry.len() >= 8 && !state.geometry.contains_key(&page) {
        state.geometry.clear();
    }
    state.geometry.entry(page).or_insert_with(|| doc.tag_content(page).map_err(|e| e.to_string()))
}

fn reveal_ancestors(state: &mut State, tree: &TagTree, id: ObjRef) {
    let mut next = element(tree, id).and_then(|e| e.parent);
    let mut seen = HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            break;
        }
        state.expanded.insert(id);
        next = element(tree, id).and_then(|e| e.parent);
    }
}

fn find_from_regions(state: &mut State, tree: &TagTree, doc: &Document, page: usize, regions: &[[f32; 4]]) {
    match doc.find_tags_at_regions(page, regions) {
        Ok(found) => {
            state.matches = found.ids;
            state.geometry_notice = (!found.issues.is_empty()).then(|| found.issues.join("; "));
        }
        Err(error) => {
            state.matches.clear();
            state.geometry_notice = Some(error.to_string());
        }
    }
    if state.matches.is_empty() {
        let message = tl!("No tag with supported content bounds intersects this selection.");
        state.geometry_notice = Some(match state.geometry_notice.take() {
            Some(details) => format!("{message} {details}"),
            None => message.into(),
        });
    }
    if let [id] = state.matches.as_slice() {
        let id = *id;
        state.filter.clear();
        reveal_ancestors(state, tree, id);
        load_draft(state, tree, Node::Tag(id), doc.edit_generation());
        state.reveal = true;
        state.highlighted = None;
    }
}

/// Display-only grouping within one marked-content or object identity. Raw engine regions
/// remain unchanged for picking. Small whitespace may be enclosed; these are not ink masks.
fn display_regions(regions: &[[f32; 4]]) -> Vec<[f32; 4]> {
    #[derive(Clone)]
    struct Group {
        bounds: [f32; 4],
        // All vertical tests use extrema of ORIGINAL rectangles, never the envelope.
        min_height: f32,
        max_height: f32,
        min_bottom: f32,
        max_bottom: f32,
        max_top: f32,
    }
    impl Group {
        fn new(rect: [f32; 4]) -> Self {
            let height = rect[3] - rect[1];
            Self { bounds: rect, min_height: height, max_height: height, min_bottom: rect[3], max_bottom: rect[3], max_top: rect[1] }
        }
        fn try_merge(&mut self, other: &Self) -> bool {
            let small = self.min_height.min(other.min_height);
            let large = self.max_height.max(other.max_height);
            let bottom_min = self.min_bottom.min(other.min_bottom);
            let bottom_max = self.max_bottom.max(other.max_bottom);
            let top = self.max_top.max(other.max_top);
            let gap = (other.bounds[0] - self.bounds[2]).max(self.bounds[0] - other.bounds[2]).max(0.0);
            if small < large * 0.55 || bottom_min - top < small * 0.7 || bottom_max - bottom_min > large * 0.25 || gap > small * 0.4 {
                return false;
            }
            self.bounds = [
                self.bounds[0].min(other.bounds[0]),
                self.bounds[1].min(other.bounds[1]),
                self.bounds[2].max(other.bounds[2]),
                self.bounds[3].max(other.bounds[3]),
            ];
            self.min_height = small;
            self.max_height = large;
            self.min_bottom = bottom_min;
            self.max_bottom = bottom_max;
            self.max_top = top;
            true
        }
    }
    fn by_y(a: &[f32; 4], b: &[f32; 4]) -> std::cmp::Ordering {
        a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])).then(a[3].total_cmp(&b[3])).then(a[2].total_cmp(&b[2]))
    }
    let mut sorted: Vec<_> = regions
        .iter()
        .copied()
        .filter(|r| r.iter().all(|v| v.is_finite()) && r[2] > r[0] && r[3] > r[1] && (r[2] - r[0]).is_finite() && (r[3] - r[1]).is_finite())
        .collect();
    sorted.sort_by(by_y);
    // Fixed-size array indexes are statically in range. Both passes inspect at most
    // 32 neighboring groups per input, within the renderer's source-region cap.
    let mut groups: Vec<Group> = Vec::new();
    for rect in sorted {
        let next = Group::new(rect);
        if !groups.iter_mut().rev().take(32).any(|group| group.try_merge(&next)) {
            groups.push(next);
        }
    }
    // Ascenders are visited before x-height letters in the first pass. Those later
    // letters can bridge initial islands, so reconcile left-to-right once. Retained
    // source extrema prevent transitive vertical drift or union-height gap expansion.
    groups.sort_by(|a, b| {
        a.bounds[0]
            .total_cmp(&b.bounds[0])
            .then(a.bounds[1].total_cmp(&b.bounds[1]))
            .then(a.bounds[2].total_cmp(&b.bounds[2]))
            .then(a.bounds[3].total_cmp(&b.bounds[3]))
    });
    let mut joined: Vec<Group> = Vec::new();
    for next in groups {
        if !joined.iter_mut().rev().take(32).any(|group| group.try_merge(&next)) {
            joined.push(next);
        }
    }
    let mut result: Vec<_> = joined.into_iter().map(|group| group.bounds).collect();
    result.sort_by(by_y);
    result
}

/// Restore navigation across an identity epoch only when the complete structure still
/// identifies the same ordered tag/content bindings. Descriptive metadata may differ;
/// no mutation draft or confirmation is rebased by this comparison.
fn same_structure_bindings(a: &TagTree, b: &TagTree) -> bool {
    a.root == b.root
        && a.pages == b.pages
        && a.children == b.children
        && a.namespaces == b.namespaces
        && a.role_map == b.role_map
        && a.preserved_associations == b.preserved_associations
        && a.edit_blockers == b.edit_blockers
        && a.elements.len() == b.elements.len()
        && a.elements.iter().zip(&b.elements).all(|(a, b)| {
            a.id == b.id
                && a.parent == b.parent
                && a.role == b.role
                && a.standard_role == b.standard_role
                && a.namespace == b.namespace
                && a.namespace_uri == b.namespace_uri
                && a.standard_namespace_uri == b.standard_namespace_uri
                && a.identifier == b.identifier
                && a.page == b.page
                && a.page_index == b.page_index
                && a.kids == b.kids
        })
}

fn update_highlight(state: &mut State, tree: &TagTree, doc: &Document, page: usize) {
    let key = (state.selected, page, state.highlight);
    if state.highlighted == Some(key) {
        return;
    }
    state.highlighted = Some(key);
    state.marks.clear();
    if !state.highlight {
        return;
    }
    let Some(selected) = state.selected else {
        return;
    };
    let kids = if state.selected_tags.len() > 1 {
        content_kids_for_roots(tree, top_level_selected(tree, &state.selected_tags))
    } else {
        content_kids(tree, selected)
    };
    let mut marks = Vec::new();
    match geometry(state, doc, page) {
        Ok(content) => {
            let marked: HashMap<_, _> = content.items.iter().map(|c| ((c.stream, c.mcid), c.regions.as_slice())).collect();
            let objects: HashMap<_, _> = content.objects.iter().map(|c| (c.object, c.regions.as_slice())).collect();
            let page_ref = tree.pages.get(page).copied();
            for (_, kid) in &kids {
                let regions = match kid {
                    TagKid::Mcid { id, page } if *page == page_ref => marked.get(&(None, *id)),
                    TagKid::Mcr { id, page, stream } if *page == page_ref => marked.get(&(stream.map(|r| (r.num, r.generation)), *id)),
                    TagKid::Objr { object, page } if *page == page_ref => objects.get(&(object.num, object.generation)),
                    _ => None,
                };
                if let Some(regions) = regions {
                    marks.extend(display_regions(regions).into_iter().map(|r| (page, r)));
                }
            }
            state.geometry_notice = if !content.issues.is_empty() {
                Some(content.issues.join("; "))
            } else if marks.is_empty() {
                Some(tl!("No supported painted content bounds for this tag on this page.").into())
            } else {
                Some(tl!("Outlines show content selection bounds, not pixel-exact painted shapes.").into())
            };
        }
        Err(error) => state.geometry_notice = Some(error.clone()),
    }
    state.marks = marks;
}

pub(crate) struct PanelContext<'a> {
    pub doc: &'a Document,
    pub modal: bool,
    pub current_page: usize,
    pub text_selection: Option<(usize, Vec<[f32; 4]>)>,
}

/// A tree row is a keyboard target, not a text editor. Document Undo/Redo may run here,
/// but must never steal text history or run underneath a tag window.
pub(crate) fn tree_has_focus(state: &State, ctx: &egui::Context, document: pdfcraft_engine::DocId) -> bool {
    !(state.properties_open || state.role_map.open || state.move_dialog.open || state.delete.is_some() || state.artifact.is_some())
        && state.focused_row.is_some_and(|node| ctx.memory(|m| m.focused()) == Some(egui::Id::new(("accessibility-tag-row", document, node))))
}

/// Called before global shortcuts, only for the active document's visible tag panel.
pub(crate) fn shortcuts(state: &mut State, ctx: &egui::Context, document: pdfcraft_engine::DocId) -> bool {
    if state.properties_open || state.role_map.open || state.move_dialog.open || state.delete.is_some() || state.artifact.is_some() {
        return true;
    }
    if tree_has_focus(state, ctx, document) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
        state.search_open = true;
        state.search_focus = true;
        return true;
    }
    false
}

/// Render the panel and return at most one transactional edit per frame.
pub(crate) fn panel(ui: &mut egui::Ui, t: &Tokens, state: &mut State, context: PanelContext<'_>, nav: &mut Option<Nav>) -> Option<Edit> {
    let PanelContext { doc, modal, current_page, text_selection } = context;
    let external_modal = modal;
    let modal = modal || state.role_map.open || state.move_dialog.open || state.properties_open || state.delete.is_some() || state.artifact.is_some();
    let generation = doc.edit_generation();
    let identity_generation = doc.object_identity_generation();
    let identity_changed = state.identity_generation != Some(identity_generation);
    if state.generation != Some(generation) || identity_changed {
        let changed = state.generation.is_some();
        let current_tree = doc.tags();
        let preserve_navigation =
            current_tree.as_ref().ok().zip(state.tree.as_ref()).is_some_and(|(current, previous)| same_structure_bindings(previous, current));
        if identity_changed {
            state.identity_generation = Some(identity_generation);
            state.delete = None;
            state.artifact = None;
            if !preserve_navigation {
                if let Some(focused) = state.focused_row {
                    ui.memory_mut(|m| m.surrender_focus(egui::Id::new(("accessibility-tag-row", doc.id, focused))));
                }
                state.selected = None;
                state.selected_tags.clear();
                state.focused_row = None;
                state.pending_focus = None;
                state.focus_properties = false;
                state.anchor = None;
                state.expanded.clear();
                state.reveal = false;
                state.initialized = false;
            }
            // Keep dirty text visible but stale; never retarget or rebase its generation.
            if !state.draft.as_ref().is_some_and(Draft::changed) {
                state.draft = None;
            }
        }
        match current_tree {
            Ok(tree) => {
                state.tree = Some(tree);
                state.error = None;
            }
            Err(e) => {
                state.tree = None;
                state.error = Some(e.to_string());
            }
        }
        state.generation = Some(generation);
        state.geometry.clear();
        state.geometry_notice = None;
        state.highlighted = None;
        state.marks.clear();
        state.matches.clear();
        if !state.initialized {
            state.show_content = true;
            state.highlight = true;
            state.initialized = true;
            if let Some(tree) = &state.tree {
                state.expanded.extend(tree.elements.iter().take(1000).map(|e| e.id));
            }
        }
        if changed {
            state.delete = None;
            state.artifact = None;
            if let Some(tree) = &state.tree {
                state.selected_tags.retain(|id| tree.element(*id).is_some());
            }
            if matches!(state.selected, Some(Node::Content(..))) && !preserve_navigation {
                if let Some(focused) = state.focused_row {
                    ui.memory_mut(|m| m.surrender_focus(egui::Id::new(("accessibility-tag-row", doc.id, focused))));
                }
                state.selected = None;
                state.focused_row = None;
                state.pending_focus = None;
                state.anchor = None;
                state.reveal = false;
            }
            // An applied edit or undo invalidates the cached projection. Reload unchanged drafts;
            // retain an in-progress draft as stale rather than silently overwriting user input.
            if !state.draft.as_ref().is_some_and(Draft::changed) {
                state.draft = None;
            }
            if let Some(Node::Tag(id)) = state.selected
                && !state.tree.as_ref().is_some_and(|tree| element(tree, id).is_some())
            {
                state.selected = None;
            }
        }
    }
    if let Some(e) = &state.error {
        ui.colored_label(egui::Color32::DARK_RED, e);
        return None;
    }
    let tree = state.tree.take()?;
    let properties_editable = doc.allows_modification() && doc.read_only_reason.is_none() && doc.tag_edit_read_only_reason().is_none();
    let editable = properties_editable && tree.edit_blockers.is_empty();
    let dialog_open = state.role_map.open || state.move_dialog.open || state.properties_open || state.delete.is_some() || state.artifact.is_some();
    let modal = modal || dialog_open;
    if modal {
        ui.disable();
    }
    let structure_editable = editable && !modal && !state.draft.as_ref().is_some_and(Draft::changed);
    let mut edit = None;
    let mut selected = None;
    let mut selection_modifiers = egui::Modifiers::NONE;
    let mut new = None;
    let mut delete = None;
    ui.horizontal(|ui| {
        let selected_tag = matches!(state.selected, Some(Node::Tag(_))) && state.selected_tags.len() == 1;
        if ui.add_enabled(selected_tag, egui::Button::new(tl!("Properties…"))).on_hover_text(tl!("Tag properties (F2)")).clicked() {
            selected = state.selected;
            state.focus_properties = true;
        }
        if icons::button(ui, "search", 28.0, state.search_open, tl!("Search tags")).clicked() {
            state.search_open = !state.search_open;
            if !state.search_open {
                state.filter.clear();
            }
        }
        if ui.add_enabled_ui(structure_editable, |ui| icons::button(ui, "plus", 28.0, false, tl!("New tag"))).inner.clicked() {
            new = Some((None, tree.children.len()));
        }
        let more = icons::button(ui, "ellipsis", 28.0, false, tl!("Tag options"));
        egui::Popup::menu(&more).show(|ui| {
            if ui.button(tl!("Expand all tags")).clicked() {
                state.expanded.extend(tree.elements.iter().map(|e| e.id));
                ui.close();
            }
            if ui.button(tl!("Collapse all tags")).clicked() {
                state.expanded.clear();
                ui.close();
            }
            ui.checkbox(&mut state.show_content, tl!("Show content items"));
            ui.checkbox(&mut state.highlight, tl!("Highlight content"));
            ui.separator();
            if ui.toggle_value(&mut state.pick_content, tl!("Select content on page")).clicked() {
                ui.close();
            }
            if ui.add_enabled(text_selection.is_some(), egui::Button::new(tl!("Find tag from selection"))).clicked() {
                state.hit_request = text_selection.clone();
                ui.close();
            }
            if ui.add_enabled(structure_editable, egui::Button::new(tl!("Edit role mappings…"))).clicked() {
                state.role_map = crate::tag_dialogs::RoleMapState::begin(doc);
                ui.close();
            }
            if !tree.issues.is_empty() && ui.button(tl!("Structure warnings")).clicked() {
                state.warnings_open = true;
                ui.close();
            }
        });
    });
    let find_key = !modal && tree_has_focus(state, ui.ctx(), doc.id) && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F));
    if find_key {
        state.search_open = true;
    }
    if state.search_open {
        let input = ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text(tl!("Find tags")).desired_width(f32::INFINITY));
        input.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, ui.is_enabled(), tl!("Find tags")));
        if find_key || std::mem::take(&mut state.search_focus) {
            input.request_focus();
        }
    }
    if let Some(reason) = doc.tag_edit_read_only_reason() {
        ui.label(reason);
    } else if !properties_editable {
        ui.label(tl!("This document does not allow structure editing."));
    } else if !editable {
        ui.horizontal_wrapped(|ui| {
            ui.small(tl!("Structure is read-only. Metadata can still be edited."));
            if ui.small_button(tl!("Details")).clicked() {
                state.warnings_open = true;
            }
        });
    }
    if state.pick_content {
        ui.small(tl!("Click page content to find its tag. Escape cancels."));
    }
    if let Some((page, regions)) = state.hit_request.take() {
        find_from_regions(state, &tree, doc, page, &regions);
    }
    if !state.matches.is_empty() {
        let matches_label = format!("{} {}", state.matches.len(), tl!("matching tags"));
        let response = egui::ComboBox::from_id_salt("tag-hit-matches").selected_text(&matches_label).show_ui(ui, |ui| {
            for id in state.matches.clone() {
                if let Some(e) = element(&tree, id)
                    && ui
                        .selectable_label(state.selected == Some(Node::Tag(id)), format!("<{}> {}", e.role, e.title.as_deref().unwrap_or("")))
                        .clicked()
                {
                    state.filter.clear();
                    reveal_ancestors(state, &tree, id);
                    selected = Some(Node::Tag(id));
                    state.reveal = true;
                }
            }
        });
        response.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), &matches_label));
    }
    let visible = rows(&tree, state);
    if visible.is_empty() {
        ui.add_space(12.0);
        ui.label(if tree.elements.is_empty() { tl!("This document has no accessibility tags.") } else { tl!("No matching tags.") });
        ui.label(egui::RichText::new(tl!("Creating a tag does not automatically associate page content.")).small().color(t.text_muted));
    }
    let row_id = |node: Node| egui::Id::new(("accessibility-tag-row", doc.id, node));
    // Independent windows keep the tree height unchanged when selection changes.
    let tree_height = (ui.available_height() - 30.0).max(60.0);
    let scroll = egui::ScrollArea::vertical().id_salt("tag-tree-scroll").max_height(tree_height).auto_shrink([false, false]);
    let scroll = if state.reveal {
        let index = state.selected.and_then(|n| visible.iter().position(|r| r.node == n)).unwrap_or(0);
        let stride = 28.0 + ui.spacing().item_spacing.y;
        let content_height = (visible.len() as f32 * stride - ui.spacing().item_spacing.y).max(0.0);
        // Clamp before virtualizing. An out-of-range offset would allocate rows at one
        // position and clamp them afterward, leaving stale hit targets for a frame.
        let offset = (index as f32 * stride - (tree_height - 28.0) * 0.5).clamp(0.0, (content_height - tree_height).max(0.0));
        scroll.vertical_scroll_offset(offset)
    } else {
        scroll
    };
    scroll.show_rows(ui, 28.0, visible.len(), |ui, range| {
        ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
            node.set_role(egui::accesskit::Role::Tree);
            node.set_label(tl!("Accessibility tags"));
        });
        let rendered = visible.get(range).unwrap_or(&[]);
        if let Some(focused) = state.focused_row
            && ui.memory(|m| m.focused()) == Some(row_id(focused))
            && !rendered.iter().any(|row| row.node == focused)
        {
            ui.memory_mut(|m| m.surrender_focus(row_id(focused)));
        }
        for row in rendered {
            let selected_row = match row.node {
                Node::Tag(id) => state.selected_tags.contains(&id),
                _ => state.selected == Some(row.node),
            };
            let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), 28.0));
            let response = ui.interact(rect, row_id(row.node), Sense::click_and_drag());
            let response = response.on_hover_text(&row.label);
            let keyboard_menu = !modal && response.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, Key::F10));
            if keyboard_menu {
                egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&response));
            }
            if state.pending_focus == Some(row.node) {
                response.request_focus();
                state.pending_focus = None;
            }
            if response.has_focus() {
                state.focused_row = Some(row.node);
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(response.id, egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() })
                });
            }
            response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, ui.is_enabled(), selected_row, &row.label));
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_role(egui::accesskit::Role::TreeItem);
                node.set_level(row.depth + 1);
                node.set_selected(selected_row);
                if row.expandable
                    && let Node::Tag(id) = row.node
                {
                    node.set_expanded(state.expanded.contains(&id) || !state.filter.is_empty());
                }
            });
            if selected_row || response.hovered() {
                ui.painter().rect_filled(rect, 4.0, if selected_row { t.selected } else { t.hover });
            }
            if response.has_focus() {
                ui.painter().rect_stroke(rect.shrink(1.0), 4.0, egui::Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
            }
            let indent = (row.depth as f32 * 17.0).min((rect.width() - 120.0).max(0.0));
            let disclosure = egui::Rect::from_min_size(rect.min + egui::vec2(indent, 2.0), egui::vec2(23.0, 24.0));
            if let Node::Tag(id) = row.node {
                if row.expandable {
                    icons::paint(
                        ui,
                        disclosure,
                        if state.expanded.contains(&id) || !state.filter.is_empty() { "chevron-down" } else { "chevron-right" },
                        14.0,
                        t.icon,
                    );
                    let toggle = ui.interact(disclosure, ui.make_persistent_id(("tag-disclosure", id)), Sense::click());
                    toggle.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            format!("{} {}", if state.expanded.contains(&id) { tl!("Collapse") } else { tl!("Expand") }, row.label),
                        )
                    });
                    if toggle.clicked() && !state.expanded.remove(&id) {
                        state.expanded.insert(id);
                    }
                }
                if structure_editable {
                    response.dnd_set_drag_payload(DragTag {
                        document: doc.id,
                        generation,
                        node: row.node,
                        tags: if state.selected_tags.contains(&id) { top_level_selected(&tree, &state.selected_tags) } else { vec![id] },
                    });
                }
                if let Some(payload) = response
                    .dnd_hover_payload::<DragTag>()
                    .filter(|p| p.document == doc.id && p.generation == generation && p.node != row.node && !p.tags.contains(&id))
                {
                    let y = ui.input(|i| i.pointer.hover_pos().map_or(rect.center().y, |p| p.y));
                    let (parent, index, inside) = if y < rect.top() + 7.0 {
                        (row.parent, row.index, false)
                    } else if y > rect.bottom() - 7.0 {
                        (row.parent, row.index + 1, false)
                    } else {
                        (Some(id), children(&tree, Some(id)).len(), true)
                    };
                    if inside {
                        ui.painter().rect_stroke(rect.shrink(1.0), 4.0, egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
                    } else {
                        let y = if index == row.index { rect.top() } else { rect.bottom() };
                        ui.painter().line_segment([egui::pos2(disclosure.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(2.0, t.accent));
                    }
                    if response.dnd_release_payload::<DragTag>().is_some() && structure_editable {
                        edit = drop_edit(&tree, &payload, parent, index);
                    }
                }
                egui::Popup::context_menu(&response).at_position(response.rect.left_bottom()).show(|ui| {
                    let properties = ui.button(tl!("Properties"));
                    if keyboard_menu {
                        properties.request_focus();
                    }
                    if properties.clicked() {
                        selected = Some(row.node);
                        state.focus_properties = true;
                        ui.close();
                    }
                    if ui.add_enabled(editable, egui::Button::new(tl!("New child tag"))).clicked() {
                        new = Some((Some(id), children(&tree, Some(id)).len()));
                        state.expanded.insert(id);
                        ui.close();
                    }
                    if ui.add_enabled(editable, egui::Button::new(tl!("New tag after"))).clicked() {
                        new = Some((row.parent, row.index + 1));
                        ui.close();
                    }
                    ui.separator();
                    for (label, direction) in [("Move up", "up"), ("Move down", "down"), ("Indent", "in"), ("Outdent", "out")] {
                        let candidate = move_edit(&tree, row, direction);
                        if ui
                            .add_enabled(structure_editable && state.selected_tags.len() <= 1 && candidate.is_some(), egui::Button::new(tl!(label)))
                            .clicked()
                        {
                            edit = candidate;
                            ui.close();
                        }
                    }
                    if ui.add_enabled(structure_editable, egui::Button::new(tl!("Move selected tags…"))).clicked() {
                        let ids = if state.selected_tags.contains(&id) { top_level_selected(&tree, &state.selected_tags) } else { vec![id] };
                        state.move_dialog =
                            crate::tag_dialogs::MoveState::begin(doc, crate::tag_dialogs::MoveSource::Tags(ids), row.parent, row.index + 1);
                        ui.close();
                    }
                    if ui.add_enabled(structure_editable, egui::Button::new(tl!("Convert to artifact…"))).clicked() {
                        let ids = if state.selected_tags.contains(&id) { top_level_selected(&tree, &state.selected_tags) } else { vec![id] };
                        state.artifact = Some((ids, generation));
                        ui.close();
                    }
                    if ui.add_enabled(structure_editable, egui::Button::new(tl!("Delete tag…"))).clicked() {
                        delete = Some(id);
                        ui.close();
                    }
                });
            }
            if let Node::Content(Some(source), source_index) = row.node {
                if structure_editable {
                    response.dnd_set_drag_payload(DragTag { document: doc.id, generation, node: row.node, tags: Vec::new() });
                }
                egui::Popup::context_menu(&response).at_position(response.rect.left_bottom()).show(|ui| {
                    let move_content = ui.add_enabled(structure_editable, egui::Button::new(tl!("Move content to…")));
                    if keyboard_menu {
                        move_content.request_focus();
                    }
                    if move_content.clicked() {
                        state.move_dialog = crate::tag_dialogs::MoveState::begin(
                            doc,
                            crate::tag_dialogs::MoveSource::Content { source, index: source_index },
                            Some(source),
                            source_index + 1,
                        );
                        ui.close();
                    }
                    for (label, position, enabled) in [
                        ("Move content up", source_index.saturating_sub(1), source_index > 0),
                        ("Move content down", source_index + 1, source_index + 1 < children(&tree, Some(source)).len()),
                    ] {
                        if ui.add_enabled(structure_editable && enabled, egui::Button::new(tl!(label))).clicked() {
                            edit = Some(Edit::MoveTagContent { source, source_index, destination: source, index: position });
                            ui.close();
                        }
                    }
                });
                if let Some(payload) =
                    response.dnd_hover_payload::<DragTag>().filter(|p| p.document == doc.id && p.generation == generation && p.node != row.node)
                {
                    let after = ui.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y > rect.center().y));
                    let y = if after { rect.bottom() } else { rect.top() };
                    ui.painter().line_segment([egui::pos2(disclosure.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(2.0, t.accent));
                    if response.dnd_release_payload::<DragTag>().is_some() && structure_editable {
                        edit = drop_edit(&tree, &payload, Some(source), source_index + usize::from(after));
                    }
                }
            }
            let page_text = row.page.map(|p| format!("{}", p + 1));
            if let Some(page) = page_text {
                ui.painter().text(rect.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, page, theme::regular(11.0), t.text_faint);
            }
            let icon = match row.node {
                Node::Tag(id) => match element(&tree, id).map(|e| e.standard_role.as_str()) {
                    Some("Figure") => "image",
                    Some("Link") => "link",
                    Some("L" | "LI" | "Lbl" | "LBody") => "list",
                    Some("H" | "H1" | "H2" | "H3" | "H4" | "H5" | "H6") => "heading",
                    _ => "tag",
                },
                Node::Content(..) => "file-text",
            };
            icons::paint(ui, egui::Rect::from_min_size(egui::pos2(disclosure.right(), rect.top()), egui::vec2(19.0, 28.0)), icon, 13.0, t.icon);
            let text_rect =
                egui::Rect::from_min_max(egui::pos2(disclosure.right() + 22.0, rect.top()), egui::pos2(rect.right() - 30.0, rect.bottom()));
            let mut job = egui::text::LayoutJob::simple_singleline(
                row.label.clone(),
                theme::regular(12.5),
                if matches!(row.node, Node::Tag(_)) { t.text } else { t.text_muted },
            );
            job.wrap.max_width = text_rect.width().max(20.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            ui.painter().galley(egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0), galley, t.text);
            if response.clicked() && !(row.expandable && disclosure.contains(response.interact_pointer_pos().unwrap_or(rect.max))) {
                selected = Some(row.node);
                selection_modifiers = ui.input(|i| i.modifiers);
                response.request_focus();
            }
            if response.double_clicked() {
                if matches!(row.node, Node::Tag(_)) {
                    selected = Some(row.node);
                    state.focus_properties = true;
                } else if let Some(p) = row.page {
                    *nav = Some(Nav::Page(p));
                }
            }
        }
    });
    state.reveal = false;
    // One focus target owns tree navigation. Text editors and modal windows retain their keys.
    let tree_has_focus = ui.memory(|m| m.focused()).is_some_and(|focused| visible.iter().any(|r| row_id(r.node) == focused));
    if !modal && !egui::Popup::is_any_open(ui.ctx()) && tree_has_focus && !visible.is_empty() {
        let index = state.selected.and_then(|n| visible.iter().position(|r| r.node == n)).unwrap_or(0);
        if let Some(row) = visible.get(index) {
            let key = ui.input(|i| {
                [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Home, Key::End, Key::Enter, Key::Delete, Key::F2]
                    .into_iter()
                    .find(|k| i.key_pressed(*k))
            });
            if let Some(key) = key {
                selection_modifiers = ui.input(|i| i.modifiers);
                ui.input_mut(|i| {
                    i.consume_key(selection_modifiers, key);
                });
                match key {
                    Key::ArrowUp => selected = visible.get(index.saturating_sub(1)).map(|r| r.node),
                    Key::ArrowDown => selected = visible.get((index + 1).min(visible.len() - 1)).map(|r| r.node),
                    Key::Home => selected = visible.first().map(|r| r.node),
                    Key::End => selected = visible.last().map(|r| r.node),
                    Key::ArrowRight => {
                        if let Node::Tag(id) = row.node
                            && !state.expanded.insert(id)
                        {
                            selected = visible.get(index + 1).filter(|r| r.parent == Some(id)).map(|r| r.node);
                        }
                    }
                    Key::ArrowLeft => {
                        let collapsed = if let Node::Tag(id) = row.node { state.expanded.remove(&id) } else { false };
                        if !collapsed {
                            selected = row.parent.map(Node::Tag);
                        }
                    }
                    Key::Enter => {
                        if let Some(p) = row.page {
                            *nav = Some(Nav::Page(p));
                        }
                    }
                    Key::F2 => {
                        selected = Some(row.node);
                        state.focus_properties = true;
                    }
                    Key::Delete if structure_editable => {
                        if let Node::Tag(id) = row.node {
                            delete = Some(id);
                        }
                    }
                    _ => {}
                }
                state.reveal = true;
            }
        }
    }
    if let Some(node) = selected {
        let mut chosen = HashSet::new();
        if let Node::Tag(id) = node {
            if selection_modifiers.command {
                chosen = state.selected_tags.clone();
                if !chosen.remove(&id) {
                    chosen.insert(id);
                }
            } else if selection_modifiers.shift {
                let a = state.anchor.and_then(|anchor| visible.iter().position(|r| r.node == anchor));
                let b = visible.iter().position(|r| r.node == node);
                if let Some((a, b)) = a.zip(b) {
                    for row in visible.get(a.min(b)..=a.max(b)).unwrap_or(&[]) {
                        if let Node::Tag(id) = row.node {
                            chosen.insert(id);
                        }
                    }
                } else {
                    chosen.insert(id);
                }
            } else {
                chosen.insert(id);
            }
        }
        if state.draft.as_ref().is_some_and(Draft::changed) {
            state.notice = Some(tl!("Apply or cancel the property changes before selecting another tag.").into());
        } else {
            let focus = match node {
                Node::Tag(id) if !chosen.contains(&id) => chosen.iter().next().copied().map(Node::Tag),
                _ => Some(node),
            };
            if let Some(focus) = focus {
                load_draft(state, &tree, focus, generation);
                state.reveal = true;
                if !selection_modifiers.shift {
                    state.anchor = Some(focus);
                }
                state.selected_tags = chosen;
                if state.selected_tags.len() > 1 {
                    state.draft = None;
                }
                if tree_has_focus {
                    // A virtualized destination may not exist in this frame's AccessKit tree.
                    // Reveal it first, then focus only after its row is allocated next frame.
                    state.pending_focus = Some(focus);
                }
                if let Some(page) = content_kids(&tree, focus).iter().find_map(|(_, k)| kid_page(&tree, k)) {
                    *nav = Some(Nav::Page(page));
                }
            } else {
                state.selected = None;
                state.selected_tags.clear();
                state.draft = None;
            }
            state.highlighted = None;
        }
    }
    if let Some((parent, index)) = new {
        create_draft(state, parent, index, generation);
        if tree.elements.is_empty()
            && let Some(draft) = state.draft.as_mut()
        {
            draft.fields.role = "Document".into();
        }
    }
    if let Some(id) = delete {
        let ids = if state.selected_tags.contains(&id) {
            tree.elements.iter().filter(|e| state.selected_tags.contains(&e.id)).map(|e| e.id).collect()
        } else {
            vec![id]
        };
        state.delete = Some((ids, generation));
    }
    if !external_modal && let Some((ids, delete_generation)) = state.delete.clone() {
        let id = ids.first().copied()?;
        let mut keep_open = true;
        egui::Window::new(tl!("Delete tags"))
            .id(egui::Id::new("tag-delete-confirmation"))
            .open(&mut keep_open)
            .collapsible(false)
            .default_width(440.0)
            .show(ui.ctx(), |ui| {
                let target = element(&tree, id)
                    .map(|e| format!("<{}> {} ({id})", e.role, e.title.as_deref().unwrap_or("").chars().take(60).collect::<String>()))
                    .unwrap_or_else(|| id.to_string());
                ui.label(format!(
                    "{} {target}{}",
                    tl!("Selected for deletion:"),
                    if ids.len() > 1 { crate::i18n::fmt(tl!(" + {count} tags"), &[("count", &(ids.len() - 1).to_string())]) } else { String::new() }
                ));
                ui.label(tl!("Delete tag structure? Page artwork stays unchanged; it does not become an artifact."));
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(editable && delete_generation == generation, egui::Button::new(tl!("Unwrap tag"))).clicked() {
                        edit = Some(Edit::DeleteTags { ids: ids.clone(), mode: DeleteMode::Unwrap });
                        state.delete = None;
                    }
                    if ui.add_enabled(editable && delete_generation == generation, egui::Button::new(tl!("Delete subtree"))).clicked() {
                        edit = Some(Edit::DeleteTags { ids: ids.clone(), mode: DeleteMode::Subtree });
                        state.delete = None;
                    }
                    if ui.button(tl!("Keep tag")).clicked() {
                        state.delete = None;
                    }
                });
            });
        if !keep_open || ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            state.delete = None;
            state.pending_focus = state.selected;
        }
    }
    if !external_modal && let Some((ids, request_generation)) = state.artifact.clone() {
        let mut keep_open = true;
        egui::Window::new(tl!("Convert to artifact")).id(egui::Id::new("tag-artifact-confirmation")).open(&mut keep_open).collapsible(false).default_width(460.0).show(ui.ctx(), |ui| {
        let names = ids
            .iter()
            .filter_map(|id| element(&tree, *id))
            .map(|e| format!("<{}> {}", e.role, e.title.as_deref().unwrap_or("").chars().take(40).collect::<String>()))
            .take(3)
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(format!("{} {names}", tl!("Convert to artifact:")));
        ui.label(tl!("The selected tags and descendants will be removed from the accessibility tree. Their marked content becomes decorative; page artwork stays visible."));
        ui.label(egui::RichText::new(tl!("Ambiguous, reused or unsupported content is refused without changing the document.")).small());
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(editable && request_generation == generation, egui::Button::new(tl!("Convert marked content"))).clicked() {
                edit = Some(Edit::Batch {
                    label: "Convert selected tags to artifacts".into(),
                    edits: ids.iter().map(|id| Edit::ArtifactTag { id: *id, include_descendants: true }).collect(),
                });
            }
            if ui.button(tl!("Keep tagged content")).clicked() {
                state.artifact = None;
            }
        });
        });
        if !keep_open || ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            state.artifact = None;
            state.pending_focus = state.selected;
        }
    }
    if state.focus_properties && state.draft.is_some() {
        state.properties_open = true;
    }
    let auxiliary_was_open = state.role_map.open || state.move_dialog.open;
    if !external_modal && edit.is_none() {
        edit = crate::tag_dialogs::role_map(ui.ctx(), doc, &tree, &mut state.role_map);
    }
    if !external_modal && edit.is_none() {
        edit = crate::tag_dialogs::move_destination(ui.ctx(), doc, &tree, &mut state.move_dialog);
    }
    if !external_modal && edit.is_none() && state.delete.is_none() && state.artifact.is_none() {
        edit = properties(ui.ctx(), state, &tree, properties_editable, editable, generation);
    }
    if auxiliary_was_open && !state.role_map.open && !state.move_dialog.open {
        state.pending_focus = state.selected;
        state.reveal = true;
    }
    if !external_modal && state.warnings_open {
        egui::Window::new(tl!("Structure warnings"))
            .id(egui::Id::new("tag-structure-warnings"))
            .open(&mut state.warnings_open)
            .default_width(460.0)
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                    for warning in &tree.issues {
                        ui.label(warning);
                    }
                });
            });
    }
    let page = match nav {
        Some(Nav::Page(page)) | Some(Nav::Flash(page, _)) => *page,
        None => current_page,
    };
    update_highlight(state, &tree, doc, page);
    ui.horizontal(|ui| {
        let summary = if state.selected_tags.len() > 1 {
            format!("{} {}", state.selected_tags.len(), tl!("tags selected"))
        } else {
            crate::i18n::fmt(
                if tree.elements.len() == 1 { tl!("{count} tag") } else { tl!("{count} tags") },
                &[("count", &tree.elements.len().to_string())],
            )
        };
        ui.small(summary);
        if !tree.issues.is_empty()
            && ui.small_button(format!("{} {}", tree.issues.len(), if tree.issues.len() == 1 { tl!("warning") } else { tl!("warnings") })).clicked()
        {
            state.warnings_open = true;
        }
        if let Some(notice) = state.notice.as_ref().filter(|_| !state.properties_open) {
            ui.label(egui::RichText::new(tl!("Action needed")).small().color(t.text_muted)).on_hover_text(notice);
        }
        if state.highlight
            && let Some(notice) = &state.geometry_notice
        {
            let label = if state.marks.is_empty() {
                tl!("Content bounds unavailable")
            } else if notice == tl!("Outlines show content selection bounds, not pixel-exact painted shapes.") {
                tl!("Content highlighted")
            } else {
                tl!("Approximate content bounds")
            };
            ui.label(egui::RichText::new(label).small().color(t.text_muted)).on_hover_text(notice);
        }
    });
    state.tree = Some(tree);
    edit
}

/// Called only when the engine accepted an edit, so failed edits retain their draft.
pub(crate) fn applied(state: &mut State) {
    state.properties_open = false;
    state.focus_properties = false;
    state.pending_focus = state.selected;
    state.reveal = true;
    state.draft = None;
    state.notice = None;
    state.delete = None;
    state.artifact = None;
    state.role_map.applied();
    state.move_dialog.applied();
}

/// Keep validation errors with the dialog that submitted them.
pub(crate) fn failed(state: &mut State, message: String) {
    if state.role_map.open {
        state.role_map.error = Some(message.clone());
    }
    if state.move_dialog.open {
        state.move_dialog.error = Some(message.clone());
    }
    state.notice = Some(message);
}

#[cfg(test)]
mod display_region_tests {
    use super::display_regions;
    // Exact PageTagContent MCID0 geometry captured from the contributor-original
    // accessibility-demo.pdf, "Introduction" in repository OFL Inter SemiBold 26pt.
    const INTRODUCTION: [[f32; 4]; 12] = [
        [39.9045, 36.083782, 43.293842, 55.0],
        [46.950397, 40.628906, 59.35362, 55.0],
        [61.359375, 37.429688, 69.81466, 55.190533],
        [72.03634, 40.616314, 80.09756, 55.0],
        [81.73525, 40.628906, 95.30685, 55.292095],
        [97.56631, 36.083782, 110.921776, 55.253906],
        [114.413284, 40.80644, 126.8291, 55.177937],
        [129.71083, 40.628906, 142.64746, 55.292095],
        [143.98047, 37.429688, 152.43575, 55.190533],
        [154.39052, 35.18272, 158.2373, 55.0],
        [160.83974, 40.628906, 174.41133, 55.292095],
        [177.29317, 40.628906, 189.69638, 55.0],
    ];
    #[test]
    fn real_inter_heading_is_one_envelope_independent_of_input_order() {
        let expected = vec![[39.9045, 35.18272, 189.69638, 55.292095]];
        assert_eq!(display_regions(&INTRODUCTION), expected);
        for rotation in 0..INTRODUCTION.len() {
            let mut shuffled = INTRODUCTION;
            shuffled.rotate_left(rotation);
            assert_eq!(display_regions(&shuffled), expected);
            shuffled.reverse();
            assert_eq!(display_regions(&shuffled), expected);
        }
    }
    #[test]
    fn original_extrema_stop_two_sided_baseline_growth() {
        let regions = [[0.0, 2.0, 5.0, 12.0], [6.0, 0.0, 11.0, 10.0], [12.0, 4.0, 17.0, 14.0]];
        let result = display_regions(&regions);
        assert_eq!(result.len(), 2);
        assert!(!result.contains(&[0.0, 0.0, 17.0, 14.0]));
        let mut shuffled = regions;
        shuffled.reverse();
        assert_eq!(display_regions(&shuffled), result);
    }
    #[test]
    fn groups_adjacent_glyphs_and_small_spaces_but_not_columns_or_lines() {
        let source = [[20.0, 10.0, 25.0, 20.0], [10.0, 10.0, 16.0, 20.0], [100.0, 10.0, 106.0, 20.0], [10.0, 24.0, 16.0, 34.0]];
        assert_eq!(display_regions(&source), vec![[10.0, 10.0, 25.0, 20.0], [100.0, 10.0, 106.0, 20.0], [10.0, 24.0, 16.0, 34.0]]);
        assert_eq!(source[0], [20.0, 10.0, 25.0, 20.0]);
    }
    #[test]
    fn mixed_case_uses_baseline_without_joining_superscripts_or_tall_shapes() {
        let source = [[0.0, 10.0, 5.0, 20.0], [6.0, 13.0, 10.0, 20.0], [11.0, 13.0, 15.0, 22.0], [16.0, 5.0, 19.0, 10.0], [20.0, 0.0, 25.0, 25.0]];
        let result = display_regions(&source);
        assert!(result.contains(&[0.0, 10.0, 15.0, 22.0]));
        assert!(result.contains(&[16.0, 5.0, 19.0, 10.0]));
        assert!(result.contains(&[20.0, 0.0, 25.0, 25.0]));
        assert_eq!(result.len(), 3);
    }
    #[test]
    fn preserves_rotated_baselines_negative_coordinates_and_rejects_nonfinite_bounds() {
        let source = [
            [-20.0, -10.0, -15.0, 0.0],
            [-14.0, -10.0, -9.0, 0.0],
            [30.0, 0.0, 35.0, 10.0],
            [36.0, 4.0, 41.0, 14.0],
            [42.0, 8.0, 47.0, 18.0],
            [f32::NAN, 0.0, 1.0, 2.0],
            [0.0, 0.0, f32::INFINITY, 2.0],
            [2.0, 0.0, 1.0, 2.0],
        ];
        let result = display_regions(&source);
        assert_eq!(result.len(), 4);
        assert!(result.contains(&[-20.0, -10.0, -9.0, 0.0]));
    }
    #[test]
    fn baseline_anchor_does_not_drift_into_another_line() {
        assert_eq!(
            display_regions(&[[0.0, 0.0, 5.0, 10.0], [6.0, 2.0, 11.0, 12.0], [12.0, 4.0, 17.0, 14.0]]),
            vec![[0.0, 0.0, 11.0, 12.0], [12.0, 4.0, 17.0, 14.0]]
        );
    }
}
