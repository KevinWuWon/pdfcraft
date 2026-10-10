//! Marked-content geometry from the same interpreter as rasterization.
//!
//! Regions are individual painted-primitive bounding boxes in page view space (points,
//! top-left origin, y down, CropBox and page rotation applied). They are selection envelopes,
//! not pixel masks. No text matching, guessed font em boxes, or structure /BBox is used.
//! Page content arrays share one MCID namespace; Form streams have their own namespace.

use std::collections::BTreeMap;

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt,
    interpret_page,
};
use hayro_syntax::{Pdf, object::ObjectIdentifier};
use kurbo::{Affine, BezPath, Rect, Shape};

/// Indirect PDF object identity, including generation.
pub type ContentObject = (u32, u16);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageTagContent {
    pub page: usize,
    pub items: Vec<TagContent>,
    pub objects: Vec<TagObjectContent>,
    /// Reasons geometry may be incomplete. Missing regions must never be replaced by guesses.
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TagContent {
    pub mcid: i64,
    /// None is the page's content sequence; Some is an explicitly identified Form stream.
    pub stream: Option<ContentObject>,
    pub regions: Vec<[f32; 4]>,
    /// Unicode in drawing order, without inferred whitespace or layout.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TagObjectContent {
    pub object: ContentObject,
    /// An annotation's declared Rect or an XObject's painted primitive bounds.
    pub regions: Vec<[f32; 4]>,
}

const MAX_REGIONS: usize = 200_000;
const MAX_MARKS: usize = 256;
const MAX_TEXT: usize = 4 * 1024 * 1024;

type Mark = (Option<ContentObject>, i64);

#[derive(Default)]
struct Collector {
    items: BTreeMap<Mark, TagContent>,
    objects: BTreeMap<ContentObject, Vec<[f32; 4]>>,
    marks: Vec<Option<Mark>>,
    // Object, previous stream, protected marked-content stack depth.
    scopes: Vec<(ContentObject, Option<ContentObject>, usize, usize)>,
    stream: Option<ContentObject>,
    clips: Vec<Rect>,
    issues: Vec<String>,
    regions: usize,
    text_bytes: usize,
    overflow_marks: usize,
    annotations: bool,
    transparent_groups: Vec<bool>,
}

fn axis_aligned_rectangle(path: &BezPath) -> bool {
    let rect = path.bounding_box();
    let points: Vec<_> = path
        .elements()
        .iter()
        .filter_map(|el| match el {
            kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) => Some(*p),
            _ => None,
        })
        .collect();
    if path.elements().iter().any(|el| matches!(el, kurbo::PathEl::QuadTo(..) | kurbo::PathEl::CurveTo(..)))
        || !(points.len() == 4 || points.len() == 5)
    {
        return false;
    }
    let corners = [
        kurbo::Point::new(rect.x0, rect.y0),
        kurbo::Point::new(rect.x1, rect.y0),
        kurbo::Point::new(rect.x1, rect.y1),
        kurbo::Point::new(rect.x0, rect.y1),
    ];
    corners.iter().all(|p| points.contains(p)) && points.windows(2).all(|p| p.first().zip(p.get(1)).is_some_and(|(a, b)| a.x == b.x || a.y == b.y))
}

fn identity(id: ObjectIdentifier) -> Option<ContentObject> {
    Some((u32::try_from(id.obj_number).ok()?, u16::try_from(id.gen_number).ok()?))
}

impl Collector {
    fn issue(&mut self, message: &str) {
        if !self.issues.iter().any(|s| s == message) {
            self.issues.push(message.into());
        }
    }

    fn record(&mut self, rect: Option<Rect>, text: &str) {
        let mut rect = if self.transparent_groups.last().copied().unwrap_or(false) { None } else { rect };
        if let Some(r) = rect.as_mut() {
            for clip in &self.clips {
                *r = r.intersect(*clip);
            }
        }
        let rect = rect
            .filter(|r| r.is_finite() && r.width() > 0.0 && r.height() > 0.0)
            .map(|r| [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32])
            .filter(|r| r.iter().all(|n| n.is_finite()));
        if self.annotations {
            return;
        }
        if self.regions >= MAX_REGIONS || self.items.len() >= MAX_REGIONS || self.objects.len() >= MAX_REGIONS {
            self.issue("Geometry limit reached; remaining regions omitted.");
            return;
        }
        let mut owners: Vec<_> = self.marks.iter().flatten().copied().collect();
        owners.sort_unstable();
        owners.dedup();
        if self.text_bytes.saturating_add(text.len().saturating_mul(owners.len())) > MAX_TEXT {
            self.issue("Text extraction limit reached; remaining text omitted.");
        }
        for (stream, mcid) in owners {
            let item = self.items.entry((stream, mcid)).or_insert_with(|| TagContent { mcid, stream, regions: Vec::new(), text: String::new() });
            if self.text_bytes.saturating_add(text.len()) <= MAX_TEXT {
                item.text.push_str(text);
                self.text_bytes = self.text_bytes.saturating_add(text.len());
            }
            if let Some(r) = rect
                && item.regions.last() != Some(&r)
            {
                item.regions.push(r);
                self.regions = self.regions.saturating_add(1);
            }
        }
        if let Some(r) = rect {
            for (id, _, _, _) in &self.scopes {
                let regions = self.objects.entry(*id).or_default();
                if regions.last() != Some(&r) {
                    regions.push(r);
                    self.regions = self.regions.saturating_add(1);
                }
            }
        }
    }

    fn visible_paint(&mut self, paint: &Paint<'_>) -> bool {
        if let Paint::Color(color) = paint
            && color.to_rgba().components().get(3).is_some_and(|alpha| *alpha <= 0.0)
        {
            self.issue("Fully transparent paint has no painted selection regions.");
            return false;
        }
        true
    }

    fn path(&mut self, path: &BezPath, transform: Affine, mode: &PathDrawMode) {
        if path.elements().len() > MAX_REGIONS {
            self.issue("Oversized path geometry omitted.");
            return;
        }
        if !transform.is_finite() {
            self.issue("Non-finite drawing transform omitted.");
            return;
        }
        let outline = match mode {
            PathDrawMode::Fill(_) => path.clone(),
            PathDrawMode::Stroke(s) => {
                if !s.line_width.is_finite()
                    || s.line_width <= 0.0
                    || s.line_width > 100_000.0
                    || !s.miter_limit.is_finite()
                    || s.miter_limit > 1000.0
                {
                    self.issue("Hairline or invalid/extreme stroke geometry omitted.");
                    return;
                }
                let style = kurbo::Stroke {
                    width: f64::from(s.line_width),
                    join: s.line_join,
                    miter_limit: f64::from(s.miter_limit),
                    start_cap: s.line_cap,
                    end_cap: s.line_cap,
                    ..Default::default()
                };
                // Dashes share the solid stroke envelope; disclose this conservative bound.
                if !s.dash_array.is_empty() {
                    self.issue("Dashed strokes use their solid-stroke selection envelope.");
                }
                kurbo::stroke(path.iter(), &style, &Default::default(), 0.1)
            }
        };
        self.record(Some((transform * outline).bounding_box()), "");
    }
}

impl<'a> Device<'a> for Collector {
    fn set_soft_mask(&mut self, mask: Option<SoftMask<'a>>) {
        if mask.is_some() {
            self.issue("Soft masks are not subtracted from selection envelopes.");
        }
    }
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn push_transparency_group(&mut self, opacity: f32, mask: Option<SoftMask<'a>>, _: BlendMode) {
        self.transparent_groups.push(self.transparent_groups.last().copied().unwrap_or(false) || opacity <= 0.0);
        if opacity <= 0.0 {
            self.issue("Fully transparent groups have no painted selection regions.");
        }
        if mask.is_some() {
            self.issue("Soft masks are not subtracted from selection envelopes.");
        }
    }
    fn pop_transparency_group(&mut self) {
        self.transparent_groups.pop();
    }
    fn draw_path(&mut self, p: &BezPath, t: Affine, paint: &Paint<'a>, mode: &PathDrawMode) {
        if self.visible_paint(paint) {
            self.path(p, t, mode);
        }
    }
    fn push_clip_path(&mut self, clip: &ClipPath) {
        // Bounding intersection is deliberately an envelope, not a claim of pixel visibility.
        if !axis_aligned_rectangle(&clip.path) {
            self.issue("Non-rectangular clipping paths use bounding selection envelopes.");
        }
        self.clips.push(clip.path.bounding_box());
    }
    fn pop_clip_path(&mut self) {
        self.clips.pop();
    }
    fn text_glyph(&mut self, glyph: &Glyph<'a>) {
        let text = glyph
            .as_unicode()
            .map(|s| match s {
                hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
            })
            .unwrap_or_default();
        self.record(None, &text);
    }
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: Affine, glyph_transform: Affine, paint: &Paint<'a>, mode: &GlyphDrawMode) {
        if !self.visible_paint(paint) {
            return;
        }
        if matches!(mode, GlyphDrawMode::Invisible) {
            self.issue("Invisible text has identity/text but no painted geometry.");
            return;
        }
        match glyph {
            Glyph::Outline(g) => {
                let mode = match mode {
                    GlyphDrawMode::Stroke(s) => PathDrawMode::Stroke(s.clone()),
                    _ => PathDrawMode::Fill(hayro::hayro_interpret::FillRule::NonZero),
                };
                self.path(&g.outline(), transform * glyph_transform, &mode);
            }
            Glyph::Type3(_) => self.issue("Type 3 glyph geometry is unavailable; text identity is preserved."),
        }
    }
    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        let rect = Rect::new(0.0, 0.0, f64::from(image.width()), f64::from(image.height()));
        self.record(Some((transform * rect.to_path(0.1)).bounding_box()), "");
    }
    fn end_page_content(&mut self) {
        if !self.marks.is_empty() {
            self.issue("Unclosed page marked content was isolated from annotations.");
        }
        self.marks.clear();
        self.overflow_marks = 0;
        self.annotations = true;
    }
    fn begin_marked_content(&mut self, _: &[u8], mcid: Option<i32>) {
        if self.annotations {
            return;
        }
        if self.marks.len() >= MAX_MARKS || self.overflow_marks > 0 {
            self.overflow_marks = self.overflow_marks.saturating_add(1);
            self.issue("Marked-content nesting limit reached.");
            return;
        }
        let mark = mcid.filter(|n| *n >= 0).map(|n| (self.stream, i64::from(n)));
        self.marks.push(mark);
        self.record(None, "");
    }
    fn end_marked_content(&mut self) {
        if self.annotations {
            return;
        }
        let protected_overflow = self.scopes.last().map_or(0, |(_, _, _, overflow)| *overflow);
        if self.overflow_marks > protected_overflow {
            self.overflow_marks -= 1;
            return;
        }
        let base = self.scopes.last().map_or(0, |(_, _, depth, _)| *depth);
        if self.marks.len() > base {
            self.marks.pop();
        } else {
            self.issue("Unmatched marked-content end ignored.");
        }
    }
    fn begin_xobject(&mut self, id: ObjectIdentifier, form: bool) {
        if let Some(id) = identity(id) {
            self.scopes.push((id, self.stream, self.marks.len(), self.overflow_marks));
            if form {
                self.stream = Some(id);
            }
        }
    }
    fn end_xobject(&mut self) {
        if let Some((_, stream, depth, overflow)) = self.scopes.pop() {
            if self.marks.len() != depth {
                self.issue("Unclosed Form marked content was isolated from its caller.");
            }
            self.marks.truncate(depth);
            self.stream = stream;
            self.overflow_marks = overflow;
        }
    }
    fn annotation_rect(&mut self, id: ObjectIdentifier, rect: Rect) {
        if self.regions >= MAX_REGIONS || self.objects.len() >= MAX_REGIONS {
            self.issue("Geometry limit reached; remaining annotation regions omitted.");
            return;
        }
        let Some(id) = identity(id) else { return };
        let rect = self.clips.first().map_or(rect, |page| rect.intersect(*page));
        let region = [rect.x0 as f32, rect.y0 as f32, rect.x1 as f32, rect.y1 as f32];
        if rect.width() > 0.0 && rect.height() > 0.0 && region.iter().all(|n| n.is_finite()) {
            self.objects.entry(id).or_default().push(region);
            self.regions = self.regions.saturating_add(1);
        }
    }
}

/// Extract one page, preserving stream-scoped MCIDs. Use PageRenderer::tag_content for the
/// untrusted-input panic guard and configured font/layer settings.
pub fn extract_page(pdf: &Pdf, page: usize, settings: &InterpreterSettings) -> Option<PageTagContent> {
    let p = pdf.pages().get(page)?;
    let (w, h) = p.render_dimensions();
    let cache = InterpreterCache::new();
    let bounds = Rect::new(0.0, 0.0, f64::from(w), f64::from(h));
    let mut context = Context::new(p.initial_transform(true).to_kurbo(), bounds, &cache, p.xref(), settings.clone());
    let mut device = Collector { clips: vec![bounds], ..Default::default() };
    interpret_page(p, &mut context, &mut device);
    if device.items.values().any(|item| !item.text.is_empty() && item.regions.is_empty()) {
        device
            .issue("Some tagged text has no painted regions (for example whitespace, cropped-out, clipping-only, invisible, or unsupported glyphs).");
    }
    Some(PageTagContent {
        page,
        items: device.items.into_values().collect(),
        objects: device.objects.into_iter().map(|(object, regions)| TagObjectContent { object, regions }).collect(),
        issues: device.issues,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PageRenderer, RenderConfig};
    use std::sync::Arc;

    fn fixture(page: &str, content: &str, extra: &str) -> Vec<u8> {
        format!("%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R {page} >> endobj\n4 0 obj << /Length {} >> stream\n{content}\nendstream endobj\n{extra}\ntrailer << /Root 1 0 R >>\n%%EOF", content.len()).into_bytes()
    }
    fn extract(bytes: Vec<u8>) -> PageTagContent {
        PageRenderer::new(Arc::new(bytes), RenderConfig::default()).tag_content(0).expect("geometry")
    }
    #[test]
    fn primitive_regions_named_properties_and_multiple_regions() {
        let content =
            extract(fixture("/Resources << /Properties << /Named << /MCID 7 >> >> >>", "/P /Named BDC 10 20 30 10 re f 100 50 10 20 re f EMC", ""));
        assert_eq!(content.items.len(), 1);
        let item = &content.items[0];
        assert_eq!((item.mcid, item.stream), (7, None));
        assert_eq!(item.regions, vec![[10.0, 70.0, 40.0, 80.0], [100.0, 30.0, 110.0, 50.0]]);
    }
    #[test]
    fn forms_scope_duplicate_mcids_and_preserve_transformed_invocations() {
        let form = "/Span << /MCID 1 >> BDC 0 0 10 10 re f EMC";
        let extra = format!(
            "5 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 20 20] /Matrix [2 0 0 2 0 0] /Length {} >> stream\n{form}\nendstream endobj",
            form.len()
        );
        let content = extract(fixture(
            "/Resources << /XObject << /Fm 5 0 R >> >>",
            "/P << /MCID 1 >> BDC 1 1 2 2 re f q 1 0 0 1 20 30 cm /Fm Do Q q 1 0 0 1 80 10 cm /Fm Do Q EMC",
            &extra,
        ));
        assert_eq!(content.items.len(), 2);
        let page = content.items.iter().find(|i| i.stream.is_none()).unwrap();
        let form = content.items.iter().find(|i| i.stream == Some((5, 0))).unwrap();
        assert_eq!(page.regions.len(), 3);
        assert_eq!(form.regions, vec![[20.0, 50.0, 40.0, 70.0], [80.0, 70.0, 100.0, 90.0]]);
        assert_eq!(content.objects[0].object, (5, 0));
        assert_eq!(content.objects[0].regions, form.regions);
    }
    #[test]
    fn page_rotation_crop_and_objr_annotation_rect_use_view_space() {
        let content = extract(fixture(
            "/CropBox [10 10 110 90] /Rotate 90 /Annots [6 0 R]",
            "/P << /MCID 2 >> BDC 20 30 10 20 re f EMC",
            "6 0 obj << /Type /Annot /Subtype /Link /Rect [20 30 30 50] >> endobj",
        ));
        assert_eq!(content.items[0].regions, vec![[20.0, 10.0, 40.0, 20.0]]);
        assert_eq!(content.objects[0].object, (6, 0));
        assert_eq!(content.objects[0].regions, content.items[0].regions);
    }
    #[test]
    fn text_outline_identity_without_matching_strings() {
        let content = extract(fixture(
            "/Resources << /Font << /F 5 0 R >> >>",
            "/P << /MCID 4 >> BDC BT /F 12 Tf 20 70 Td (Hi) Tj ET EMC /P << /MCID 5 >> BDC BT /F 12 Tf 90 70 Td (Hi) Tj ET EMC",
            "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj",
        ));
        assert_eq!(content.items.len(), 2);
        assert_eq!(content.items[0].text, "Hi");
        assert_eq!(content.items[1].text, "Hi");
        assert_eq!(content.items[0].regions.len(), 2);
        assert!(content.items[0].regions[0][0] < 30.0);
        assert!(content.items[1].regions[0][0] > 80.0);
    }
    #[test]
    fn image_pixel_dimensions_map_to_the_full_pdf_image_square() {
        let content = extract(fixture(
            "/Resources << /XObject << /Im 5 0 R >> >>",
            "/Figure << /MCID 3 >> BDC q 40 0 0 20 10 30 cm /Im Do Q EMC",
            "5 0 obj << /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 4 >> stream\nABCD\nendstream endobj",
        ));
        assert_eq!(content.items[0].regions, vec![[10.0, 50.0, 50.0, 70.0]]);
        assert_eq!(content.objects[0].object, (5, 0));
    }
    #[test]
    fn invisible_text_keeps_identity_without_fabricated_geometry() {
        let content = extract(fixture(
            "/Resources << /Font << /F 5 0 R >> >>",
            "/P << /MCID 4 >> BDC BT /F 12 Tf 3 Tr 20 70 Td (Hidden) Tj ET EMC",
            "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj",
        ));
        assert_eq!(content.items[0].text, "Hidden");
        assert!(content.items[0].regions.is_empty());
        assert!(!content.issues.is_empty());
    }
    #[test]
    fn empty_and_whitespace_marks_keep_identity_without_fabricated_geometry() {
        // Real tagged documents can give separators their own MCIDs. A space's advance
        // width is not painted content and must not become a guessed selection region.
        let content = extract(fixture(
            "/Resources << /Font << /F 5 0 R >> >>",
            "/P << /MCID 0 >> BDC EMC /Span << /MCID 1 >> BDC BT /F 12 Tf 20 70 Td ( ) Tj ET EMC /P << /MCID 2 >> BDC 10 20 30 10 re f EMC",
            "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj",
        ));
        assert_eq!(content.items.len(), 3);
        assert_eq!(content.items[0].mcid, 0);
        assert!(content.items[0].text.is_empty());
        assert!(content.items[0].regions.is_empty());
        assert_eq!(content.items[1].mcid, 1);
        assert_eq!(content.items[1].text, " ");
        assert!(content.items[1].regions.is_empty());
        assert_eq!(content.items[2].regions, vec![[10.0, 70.0, 40.0, 80.0]]);
        assert!(content.issues.iter().any(|issue| issue.contains("whitespace")));
    }
    #[test]
    fn cropped_out_text_keeps_identity_and_reappears_only_when_crop_expands() {
        let resources = "/Resources << /Font << /F 5 0 R >> >>";
        let drawing = "/P << /MCID 0 >> BDC BT /F 12 Tf 20 10 Td (Outside) Tj ET EMC /P << /MCID 1 >> BDC BT /F 12 Tf 20 70 Td (Inside) Tj ET EMC";
        let font = "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj";
        let cropped = extract(fixture(&format!("/CropBox [0 40 200 100] {resources}"), drawing, font));
        assert_eq!(cropped.items[0].text, "Outside");
        assert!(cropped.items[0].regions.is_empty());
        assert!(!cropped.items[1].regions.is_empty());
        assert!(cropped.issues.iter().any(|issue| issue.contains("cropped-out")));

        let expanded = extract(fixture(resources, drawing, font));
        assert_eq!(expanded.items[0].text, cropped.items[0].text);
        assert!(!expanded.items[0].regions.is_empty());
        // The original crop is y=0..60 in the expanded page's top-left view space.
        assert!(expanded.items[0].regions.iter().all(|region| region[1] >= 60.0));
    }
    #[test]
    fn hidden_noview_empty_and_offpage_annotations_have_no_selection_regions() {
        let content = extract(fixture(
            "/Annots [6 0 R 7 0 R 8 0 R 9 0 R 10 0 R]",
            "",
            "6 0 obj << /Type /Annot /Subtype /Link /F 2 /Rect [20 30 30 50] >> endobj\n7 0 obj << /Type /Annot /Subtype /Link /F 32 /Rect [20 30 30 50] >> endobj\n8 0 obj << /Type /Annot /Subtype /Link /F 4 /Rect [20 30 20 30] >> endobj\n9 0 obj << /Type /Annot /Subtype /Link /F 4 /Rect [800 800 810 810] >> endobj\n10 0 obj << /Type /Annot /Subtype /Link /F 4 /Rect [20 30 30 50] >> endobj",
        ));
        assert_eq!(content.objects.len(), 1);
        assert_eq!(content.objects[0].object, (10, 0));
        assert_eq!(content.objects[0].regions, vec![[20.0, 50.0, 30.0, 70.0]]);
    }
    #[test]
    fn filled_and_stroked_glyphs_contribute_text_once() {
        for mode in [2, 6, 7] {
            let content = extract(fixture(
                "/Resources << /Font << /F 5 0 R >> >>",
                &format!("/P << /MCID 4 >> BDC BT /F 12 Tf {mode} Tr 20 70 Td (Hi) Tj ET EMC"),
                "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj",
            ));
            assert_eq!(content.items[0].text, "Hi", "render mode {mode}");
            assert_eq!(content.items[0].regions.is_empty(), mode == 7);
        }
    }
    #[test]
    fn annotation_limits_and_nonrectangular_clip_are_reported() {
        let mut collector = Collector { regions: MAX_REGIONS, ..Default::default() };
        collector.annotation_rect(ObjectIdentifier::new(6, 0), Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!(collector.objects.is_empty());
        let mut triangle = BezPath::new();
        triangle.move_to((0.0, 0.0));
        triangle.line_to((10.0, 0.0));
        triangle.line_to((5.0, 10.0));
        triangle.close_path();
        collector.push_clip_path(&ClipPath { path: triangle, fill: hayro::hayro_interpret::FillRule::NonZero });
        assert_eq!(collector.issues.len(), 2);
    }
    #[test]
    fn transparent_paint_is_not_highlighted() {
        let content = extract(fixture(
            "/Resources << /ExtGState << /Transparent << /ca 0 >> >> >>",
            "/P << /MCID 8 >> BDC /Transparent gs 10 20 30 10 re f EMC",
            "",
        ));
        assert!(content.items[0].regions.is_empty());
        assert!(content.issues.iter().any(|s| s.contains("transparent")));
    }
    #[test]
    fn malformed_form_cannot_close_callers_marked_content() {
        let form = "EMC /P << /MCID 2 >> BDC 0 0 10 10 re f";
        let extra = format!("5 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 20 20] /Length {} >> stream\n{form}\nendstream endobj", form.len());
        let content = extract(fixture("/Resources << /XObject << /Fm 5 0 R >> >>", "/P << /MCID 1 >> BDC /Fm Do 50 50 10 10 re f EMC", &extra));
        let page = content.items.iter().find(|i| i.stream.is_none()).unwrap();
        assert_eq!(page.regions.len(), 2);
        let form = content.items.iter().find(|i| i.stream.is_some()).unwrap();
        assert_eq!(form.regions.len(), 1);
        assert_eq!(content.issues.len(), 2);
    }
    #[test]
    fn same_mcid_on_distinct_pages_stays_page_scoped() {
        let first = "/P << /MCID 1 >> BDC 10 10 10 10 re f EMC";
        let second = "/P << /MCID 1 >> BDC 50 50 10 10 re f EMC";
        let bytes = fixture(
            "",
            first,
            &format!(
                "7 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 8 0 R >> endobj\n8 0 obj << /Length {} >> stream\n{second}\nendstream endobj",
                second.len()
            ),
        );
        let bytes = String::from_utf8(bytes).unwrap().replace("/Kids [3 0 R] /Count 1", "/Kids [3 0 R 7 0 R] /Count 2").into_bytes();
        let mut renderer = PageRenderer::new(Arc::new(bytes), RenderConfig::default());
        let a = renderer.tag_content(0).unwrap();
        let b = renderer.tag_content(1).unwrap();
        assert_eq!((a.page, b.page), (0, 1));
        assert_eq!(a.items[0].mcid, b.items[0].mcid);
        assert_ne!(a.items[0].regions, b.items[0].regions);
        assert!(renderer.tag_content(2).is_err());
    }
}
