//! Headless selection-to-structure lookup using exact content identities.
use crate::{
    Document, EditError, ObjRef,
    a11y::tags::{TagKid, TagTree},
};
use pdfcraft_render::tagged::PageTagContent;

/// Every direct content owner hit by a selection, plus coverage/structure warnings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagMatches {
    pub ids: Vec<ObjRef>,
    pub issues: Vec<String>,
}

impl Document {
    /// Find all direct tag owners intersecting selection rectangles (page-view points).
    /// Ambiguous selections return multiple IDs, never an arbitrary first match.
    pub fn tags_at_regions(&self, page: usize, regions: &[[f32; 4]]) -> Result<Vec<ObjRef>, EditError> {
        self.find_tags_at_regions(page, regions).map(|m| m.ids)
    }

    /// Like `tags_at_regions`, retaining geometry and structure warnings even with no matches.
    pub fn find_tags_at_regions(&self, page: usize, regions: &[[f32; 4]]) -> Result<TagMatches, EditError> {
        validate(regions)?;
        let tree = self.tags()?;
        let content = self.tag_content(page)?;
        matching(&tree, &content, regions)
    }
}

fn validate(regions: &[[f32; 4]]) -> Result<(), EditError> {
    if regions.is_empty() || regions.len() > 4096 {
        return Err(EditError::Accessibility("selection requires 1–4096 rectangles".into()));
    }
    if regions.iter().any(|r| r.iter().any(|v| !v.is_finite()) || r[0] > r[2] || r[1] > r[3]) {
        return Err(EditError::Accessibility(
            "selection rectangles must be finite [left, top, right, bottom] with nonnegative width and height".into(),
        ));
    }
    Ok(())
}

fn matching(tree: &TagTree, content: &PageTagContent, selection: &[[f32; 4]]) -> Result<TagMatches, EditError> {
    validate(selection)?;
    let mut out = TagMatches { ids: Vec::new(), issues: content.issues.clone() };
    out.issues.extend(tree.issues.iter().cloned());
    let Some(page) = tree.pages.get(content.page) else { return Ok(out) };
    let tuple = |r: ObjRef| (r.num, r.generation);
    let items: std::collections::BTreeMap<_, _> = content.items.iter().map(|c| ((c.stream, c.mcid), c.regions.as_slice())).collect();
    let objects: std::collections::BTreeMap<_, _> = content.objects.iter().map(|c| (c.object, c.regions.as_slice())).collect();
    let mut remaining = 10_000_000usize;
    for element in &tree.elements {
        let mut hit = false;
        for kid in &element.kids {
            let bounds = match kid {
                TagKid::Mcid { id, page: Some(p) } if p == page => items.get(&(None, *id)),
                TagKid::Mcr { id, page: Some(p), stream } if p == page => items.get(&(stream.map(tuple), *id)),
                TagKid::Objr { object, page: Some(p) } if p == page => objects.get(&tuple(*object)),
                _ => None,
            };
            for r in bounds.copied().unwrap_or(&[]) {
                for q in selection {
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| EditError::Accessibility("selection is too complex; narrow the selected area".into()))?;
                    if r.iter().all(|v| v.is_finite()) && r[0] <= q[2] && r[2] >= q[0] && r[1] <= q[3] && r[3] >= q[1] {
                        hit = true;
                        break;
                    }
                }
                if hit {
                    break;
                }
            }
            if hit {
                break;
            }
        }
        if hit {
            out.ids.push(element.id);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a11y::tags::TagElement;
    use pdfcraft_render::tagged::{TagContent, TagObjectContent};
    fn element(n: u32, kid: TagKid) -> TagElement {
        TagElement {
            id: ObjRef::new(n, 0),
            parent: None,
            role: "P".into(),
            standard_role: "P".into(),
            namespace: None,
            namespace_uri: None,
            standard_namespace_uri: Some(pdfcraft_a11y::tags::PDF_17_NAMESPACE.into()),
            page: None,
            page_index: None,
            title: None,
            alt: None,
            actual_text: None,
            lang: None,
            expansion: None,
            identifier: None,
            kids: vec![kid],
        }
    }
    #[test]
    fn tag_selection_returns_all_owners_and_matches_exact_stream_generation() {
        let p = ObjRef::new(3, 2);
        let stream = ObjRef::new(20, 4);
        let tree = TagTree {
            pages: vec![p],
            elements: vec![
                element(10, TagKid::Mcid { id: 0, page: Some(p) }),
                element(11, TagKid::Mcid { id: 0, page: Some(p) }),
                element(12, TagKid::Mcr { id: 0, page: Some(p), stream: Some(stream) }),
                element(13, TagKid::Mcr { id: 0, page: Some(p), stream: Some(ObjRef::new(20, 5)) }),
                element(14, TagKid::Mcid { id: 0, page: Some(ObjRef::new(3, 0)) }),
                element(15, TagKid::Objr { object: ObjRef::new(30, 1), page: Some(p) }),
            ],
            ..Default::default()
        };
        let content = PageTagContent {
            page: 0,
            items: vec![
                TagContent { mcid: 0, stream: None, regions: vec![[0.0, 0.0, 10.0, 10.0]], text: String::new() },
                TagContent { mcid: 0, stream: Some((20, 4)), regions: vec![[20.0, 20.0, 30.0, 30.0]], text: String::new() },
            ],
            objects: vec![TagObjectContent { object: (30, 1), regions: vec![[40.0, 40.0, 50.0, 50.0]] }],
            issues: vec!["partial geometry".into()],
        };
        let found = matching(&tree, &content, &[[5.0, 5.0, 5.0, 5.0]]).unwrap();
        assert_eq!(found.ids, vec![ObjRef::new(10, 0), ObjRef::new(11, 0)]);
        assert_eq!(matching(&tree, &content, &[[25.0, 25.0, 25.0, 25.0]]).unwrap().ids, vec![ObjRef::new(12, 0)]);
        assert_eq!(matching(&tree, &content, &[[45.0, 45.0, 45.0, 45.0]]).unwrap().ids, vec![ObjRef::new(15, 0)]);
        let none = matching(&tree, &content, &[[90.0, 90.0, 90.0, 90.0]]).unwrap();
        assert!(none.ids.is_empty());
        assert_eq!(none.issues, vec!["partial geometry"]);
        for rect in [[f32::NAN, 0.0, 1.0, 1.0], [2.0, 0.0, 1.0, 1.0]] {
            assert!(matching(&tree, &content, &[rect]).is_err());
        }
    }
}
