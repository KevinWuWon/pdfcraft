//! Thin, strictly validated adapters for structure-tree editing.
use crate::{Args, Automation, Result, ToolError, failed, summary};
use pdfcraft_engine::{
    Edit, ObjRef,
    a11y::tags::{DeleteMode, TagProperties},
};
use serde_json::{Value, json};

fn identity(id: ObjRef) -> Value {
    json!({ "number": id.num, "generation": id.generation })
}

fn reference(a: &Args, key: &str) -> Result<ObjRef> {
    let object = a.get(key).and_then(Value::as_object).ok_or_else(|| Args::wrong(key, "an object with number and generation"))?;
    if object.len() != 2 || !object.contains_key("number") || !object.contains_key("generation") {
        return Err(Args::wrong(key, "an object containing only number and generation"));
    }
    let number = object
        .get("number")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| Args::wrong(key, "a reference with number in 1..=4294967295"))?;
    let generation = object
        .get("generation")
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| Args::wrong(key, "a reference with generation in 0..=65535"))?;
    Ok(ObjRef::new(number, generation))
}

fn references(a: &Args) -> Result<Vec<ObjRef>> {
    let values = a
        .get("ids")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| Args::wrong("ids", "1–4096 object references"))?;
    values
        .iter()
        .map(|value| {
            let args = json!({"id": value});
            reference(&Args(&args), "id")
        })
        .collect()
}

fn delete_mode(a: &Args) -> Result<DeleteMode> {
    match a.str("mode")? {
        "unwrap" => Ok(DeleteMode::Unwrap),
        "subtree" => Ok(DeleteMode::Subtree),
        _ => Err(Args::wrong("mode", "unwrap or subtree")),
    }
}

fn parent(a: &Args) -> Result<Option<ObjRef>> {
    // Explicit null is not in the parent schema: omit to select the root.
    if a.0.get("parent").is_some() { reference(a, "parent").map(Some) } else { Ok(None) }
}

fn position(a: &Args) -> Result<usize> {
    a.int("position")?.checked_sub(1).and_then(|n| usize::try_from(n).ok()).ok_or_else(|| Args::wrong("position", "a positive 1-based integer"))
}

fn property(a: &Args, key: &str) -> Result<Option<Option<String>>> {
    match a.0.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(_) => Err(Args::wrong(key, "a string or null")),
    }
}

fn properties(a: &Args) -> Result<TagProperties> {
    let role = match a.0.get("role") {
        None => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return Err(Args::wrong("role", "a nonempty string")),
    };
    Ok(TagProperties {
        role,
        title: property(a, "title")?,
        alt: property(a, "alt")?,
        actual_text: property(a, "actual_text")?,
        lang: property(a, "lang")?,
        expansion: property(a, "expansion")?,
    })
}

impl Automation {
    pub(crate) fn accessibility_tag_content(&self, a: &Args) -> Result<Value> {
        let page =
            a.int("page")?.checked_sub(1).and_then(|n| usize::try_from(n).ok()).ok_or_else(|| Args::wrong("page", "a positive 1-based integer"))?;
        let doc = self.doc(a)?;
        let content = doc.tag_content(page).map_err(failed)?;
        let tree = doc.tags().map_err(failed)?;
        let items: Vec<Value> = content
            .items
            .iter()
            .map(|item| {
                json!({
                    "mcid": item.mcid, "stream": item.stream.map(|(n,g)| identity(ObjRef::new(n,g))),
                    "regions": item.regions, "text": item.text,
                })
            })
            .collect();
        let objects: Vec<Value> = content
            .objects
            .iter()
            .map(|item| {
                json!({
                    "object": identity(ObjRef::new(item.object.0,item.object.1)), "regions": item.regions,
                })
            })
            .collect();
        Ok(
            json!({ "doc": doc.id.0, "page": page.saturating_add(1), "page_object": tree.pages.get(page).copied().map(identity), "items": items, "objects": objects, "issues": content.issues }),
        )
    }

    pub(crate) fn accessibility_tags_find(&self, a: &Args) -> Result<Value> {
        let page =
            a.int("page")?.checked_sub(1).and_then(|n| usize::try_from(n).ok()).ok_or_else(|| Args::wrong("page", "a positive 1-based integer"))?;
        let regions = a
            .get("regions")
            .and_then(Value::as_array)
            .filter(|r| !r.is_empty() && r.len() <= 4096)
            .ok_or_else(|| Args::wrong("regions", "an array of 1–4096 rectangles"))?;
        let regions = regions
            .iter()
            .map(|v| {
                let values = v.as_array().filter(|r| r.len() == 4).ok_or_else(|| Args::wrong("regions", "rectangles of four finite numbers"))?;
                let mut rect = [0.0f32; 4];
                for (slot, value) in rect.iter_mut().zip(values) {
                    *slot = value
                        .as_f64()
                        .filter(|v| v.is_finite() && v.abs() <= f32::MAX as f64)
                        .ok_or_else(|| Args::wrong("regions", "finite representable coordinates"))? as f32;
                }
                if rect[0] > rect[2] || rect[1] > rect[3] {
                    return Err(Args::wrong("regions", "ordered [left,top,right,bottom] rectangles"));
                }
                Ok(rect)
            })
            .collect::<Result<Vec<_>>>()?;
        let doc = self.doc(a)?;
        let found = doc.find_tags_at_regions(page, &regions).map_err(failed)?;
        Ok(
            json!({ "doc": doc.id.0, "page": page.saturating_add(1), "ids": found.ids.iter().copied().map(identity).collect::<Vec<_>>(), "issues": found.issues, "object_identity_generation": doc.object_identity_generation() }),
        )
    }

    pub(crate) fn accessibility_tags(&self, a: &Args) -> Result<Value> {
        let doc = self.doc(a)?;
        let tree = doc.tags().map_err(failed)?;
        Ok(json!({ "document": summary(doc), "tree": tree_json(&tree), "tag_edit_read_only_reason": doc.tag_edit_read_only_reason() }))
    }

    pub(crate) fn accessibility_tag_edit(&mut self, a: &Args, action: &str) -> Result<Value> {
        let before: std::collections::BTreeSet<ObjRef> =
            if action == "create" { self.doc(a)?.tags().map_err(failed)?.elements.iter().map(|e| e.id).collect() } else { Default::default() };
        let edit = match action {
            "identifier" => Edit::SetTagIdentifier {
                id: reference(a, "id")?,
                identifier: property(a, "identifier")?.ok_or_else(|| Args::wrong("identifier", "a string or null"))?,
            },
            "artifact" => Edit::ArtifactTag {
                id: reference(a, "id")?,
                include_descendants: a
                    .get("include_descendants")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| Args::wrong("include_descendants", "a boolean"))?,
            },
            "move_many" => Edit::MoveTags { ids: references(a)?, parent: parent(a)?, index: position(a)? },
            "delete_many" => Edit::DeleteTags { ids: references(a)?, mode: delete_mode(a)? },
            "role_map" => Edit::SetTagRoleMapping {
                role: a.str("role")?.into(),
                target: property(a, "target")?.ok_or_else(|| Args::wrong("target", "a string or null"))?,
            },
            "content_move" => Edit::MoveTagContent {
                source: reference(a, "source")?,
                source_index: a
                    .int("source_position")?
                    .checked_sub(1)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| Args::wrong("source_position", "a positive integer"))?,
                destination: reference(a, "destination")?,
                index: position(a)?,
            },
            "create" => Edit::CreateTag { parent: parent(a)?, index: position(a)?, properties: properties(a)? },
            "update" => Edit::UpdateTag { id: reference(a, "id")?, properties: properties(a)? },
            "move" => Edit::MoveTag { id: reference(a, "id")?, parent: parent(a)?, index: position(a)? },
            "delete" => Edit::DeleteTag {
                id: reference(a, "id")?,
                mode: match a.str("mode")? {
                    "unwrap" => DeleteMode::Unwrap,
                    "subtree" => DeleteMode::Subtree,
                    _ => return Err(Args::wrong("mode", "unwrap or subtree")),
                },
            },
            _ => return Err(ToolError::InvalidArgs("unknown tag action".into())),
        };
        let id = self.doc(a)?.id;
        self.session.apply(id, edit).map_err(failed)?;
        let mut result = self.accessibility_tags(a)?;
        if action == "create" {
            let after = self.doc(a)?.tags().map_err(failed)?;
            result["created"] = after.elements.iter().find(|e| !before.contains(&e.id)).map(|e| identity(e.id)).unwrap_or(Value::Null);
        }
        Ok(result)
    }
}

fn kids_json(kids: &[pdfcraft_engine::a11y::tags::TagKid]) -> Vec<Value> {
    use pdfcraft_engine::a11y::tags::TagKid;
    kids.iter()
        .enumerate()
        .map(|(index, kid)| {
            let mut value = match kid {
                TagKid::Element { id } => json!({ "kind": "element", "id": identity(*id) }),
                TagKid::Mcid { id, page } => json!({ "kind": "mcid", "mcid": id, "page_object": page.map(identity) }),
                TagKid::Mcr { id, page, stream } => {
                    json!({ "kind": "mcr", "mcid": id, "page_object": page.map(identity), "stream": stream.map(identity) })
                }
                TagKid::Objr { object, page } => json!({ "kind": "objr", "object": identity(*object), "page_object": page.map(identity) }),
                TagKid::Unsupported { description } => json!({ "kind": "unsupported", "description": description }),
            };
            value["position"] = json!(index.saturating_add(1));
            value
        })
        .collect()
}

fn tree_json(tree: &pdfcraft_engine::a11y::tags::TagTree) -> Value {
    let elements: Vec<Value> = tree
        .elements
        .iter()
        .map(|e| {
            json!({
                "id": identity(e.id), "parent": e.parent.map(identity),
                "role": e.role, "standard_role": e.standard_role,
                "page_object": e.page.map(identity), "page": e.page_index.map(|p| p.saturating_add(1)),
                "title": e.title, "alt": e.alt, "actual_text": e.actual_text, "lang": e.lang,
                "expansion": e.expansion, "identifier": e.identifier, "kids": kids_json(&e.kids),
            })
        })
        .collect();
    json!({ "role_map": tree.role_map, "root": tree.root.map(identity), "pages": tree.pages.iter().copied().map(identity).collect::<Vec<_>>(), "children": kids_json(&tree.children), "elements": elements, "issues": tree.issues, "edit_blockers": tree.edit_blockers, "preserved_associations": tree.preserved_associations.iter().map(|a| json!({"kind":a.kind.as_str(),"key":a.key,"mcid":a.mcid,"owner":identity(a.owner)})).collect::<Vec<_>>() })
}
