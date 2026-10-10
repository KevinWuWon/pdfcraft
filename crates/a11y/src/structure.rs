//! The structure tree (tags) as the checker needs it, and the rules that read it: alternate
//! text, tables, lists and headings (ISO 32000-2 §14.7–14.8).

use std::collections::{BTreeMap, HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object};
use pdfcraft_model::Page;

use crate::{Finding, Rule};

/// One allowance for all visited values, including arrays, references and content
/// leaves. Element-only limits leave huge arrays of invalid values unbounded.
const MAX_VISITS: usize = 100_000;
const MAX_DEPTH: usize = 128;
const MAX_ISSUES: usize = 200;
const MAX_TABLE_COLUMNS: usize = 10_000;
const MAX_TABLE_WORK: usize = 1_000_000;

fn checked_reference(doc: &Document, id: ObjRef) -> Result<std::sync::Arc<Object>, String> {
    if doc.generation(id.num) != id.generation {
        return Err("stale structure object generation".into());
    }
    doc.try_get(id.num).map_err(|e| format!("unreadable structure object: {e}"))
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Elem {
    pub obj: Option<ObjRef>,
    /// The standard type after the role map (or the type as written if it maps to nothing).
    pub ty: Vec<u8>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Has /Alt (or /ActualText, which also replaces the content).
    pub alt: bool,
    /// Marked content or an object reference directly under it.
    pub content: bool,
    /// Annotations it references (OBJR).
    pub annots: Vec<ObjRef>,
    pub page: Option<usize>,
    pub attrs: Vec<Dict>,
}

#[derive(Debug, Default)]
pub(crate) struct Tree {
    /// A /StructTreeRoot exists.
    pub exists: bool,
    /// Unsafe role resolution must not turn checker rules into clean empty results.
    pub issues: Vec<String>,
    pub elems: Vec<Elem>,
    /// Top-level elements in order.
    pub roots: Vec<usize>,
    table_shapes: BTreeMap<usize, Result<Option<String>, String>>,
}

impl Tree {
    pub fn read(doc: &Document, pages: &[Page]) -> Tree {
        let mut t = Tree::default();
        let Some(catalog_id) = doc.root() else {
            t.issue("missing catalog prevents structure inspection");
            return t;
        };
        let catalog = match checked_reference(doc, catalog_id) {
            Ok(v) => v,
            Err(e) => {
                t.issue(e);
                return t;
            }
        };
        let Object::Dict(cat) = &*catalog else {
            t.issue("catalog must be a dictionary, not a stream or other object");
            return t;
        };
        let Some(raw) = cat.get(b"StructTreeRoot") else { return t };
        // Presence is distinct from validity. Malformed present roots cannot be
        // treated as an ordinary untagged document with clean structure rules.
        t.exists = true;
        match raw {
            Object::Ref(id) => match checked_reference(doc, *id) {
                Ok(value) => match &*value {
                    Object::Dict(root) => t.read_root(doc, root, pages, Some(*id)),
                    _ => t.issue("StructTreeRoot must be a dictionary, not a stream or other object"),
                },
                Err(e) => t.issue(e),
            },
            Object::Dict(root) => t.read_root(doc, root, pages, None),
            _ => t.issue("StructTreeRoot must be a dictionary, not a stream or other object"),
        }
        t
    }

    fn issue(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.issues.len() < MAX_ISSUES && !self.issues.contains(&message) {
            self.issues.push(message);
        }
    }

    fn read_root(&mut self, doc: &Document, root: &Dict, pages: &[Page], root_id: Option<ObjRef>) {
        if root.name(b"Type") != Some(b"StructTreeRoot") {
            self.issue("StructTreeRoot has missing or invalid Type");
            return;
        }
        if root.len() > MAX_VISITS {
            self.issue("structure root exceeds safe limits");
            return;
        }
        let resolver = crate::tags::RoleResolver::read(doc, root);
        if let Err(e) = &resolver {
            self.issue(e.to_string());
        }
        let page_index: HashMap<ObjRef, usize> = pages.iter().enumerate().map(|(i, p)| (p.obj, i)).collect();
        let mut seen = HashSet::new();
        if let Some(id) = root_id {
            seen.insert(id);
        }
        let mut reader = Reader { doc, resolver: resolver.as_ref().ok(), page_index: &page_index, seen: &mut seen, remaining: MAX_VISITS };
        if let Some(k) = root.get(b"K") {
            let mut roots = Vec::new();
            reader.kids(self, k, None, None, &mut roots, 0);
            self.roots = roots;
        }
        self.check_tables();
    }

    /// Table geometry exhaustion only makes regularity uncertain. It does not
    /// invalidate already confirmed Figure identities or other structural roles.
    pub fn issues_for(&self, rule: Rule) -> Vec<&str> {
        let mut issues: Vec<&str> = self.issues.iter().map(String::as_str).collect();
        if rule == Rule::TableRegularity {
            issues.extend(self.table_shapes.values().filter_map(|shape| shape.as_ref().err().map(String::as_str)).take(MAX_ISSUES));
        }
        issues
    }

    fn check_tables(&mut self) {
        let mut work = MAX_TABLE_WORK;
        for i in 0..self.elems.len() {
            if self.is(i, b"Table") {
                self.table_shapes.insert(i, self.irregular(i, &mut work));
            }
        }
    }

    /// Elements in document (pre-)order.
    fn preorder(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.elems.len());
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            out.push(i);
            stack.extend(self.elems[i].children.iter().rev().copied());
        }
        out
    }

    fn ancestors(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.elems[i].parent, move |p| self.elems[*p].parent)
    }

    /// `i` and everything under it.
    fn subtree(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![i];
        while let Some(j) = stack.pop() {
            out.push(j);
            stack.extend(self.elems[j].children.iter().copied());
        }
        out
    }

    fn is(&self, i: usize, ty: &[u8]) -> bool {
        self.elems[i].ty == ty
    }

    fn parent_is(&self, i: usize, types: &[&[u8]]) -> bool {
        self.elems[i].parent.is_some_and(|p| types.contains(&self.elems[p].ty.as_slice()))
    }

    fn finding(&self, i: usize, what: &str) -> Finding {
        let e = &self.elems[i];
        let ty = String::from_utf8_lossy(&e.ty);
        let at = e.page.map(|p| format!(" on page {}", p + 1)).unwrap_or_default();
        Finding { page: e.page, message: format!("{ty} element{at}: {what}") }
    }

    pub fn rule(&self, rule: Rule) -> Vec<Finding> {
        let order = self.preorder();
        let mut out = Vec::new();
        match rule {
            Rule::FiguresAltText => {
                for &i in &order {
                    if self.is(i, b"Figure") && !self.elems[i].alt && !self.ancestors(i).any(|a| self.elems[a].alt) {
                        out.push(self.finding(i, "no alternate text"));
                    }
                }
            }
            Rule::OtherElementsAltText => {
                for &i in &order {
                    if self.is(i, b"Formula") && !self.elems[i].alt && !self.ancestors(i).any(|a| self.elems[a].alt) {
                        out.push(self.finding(i, "no alternate text"));
                    }
                }
            }
            Rule::NestedAltText => {
                for &i in &order {
                    if self.elems[i].alt && self.ancestors(i).any(|a| self.elems[a].alt) {
                        out.push(self.finding(i, "its alternate text is inside another element's and will never be read"));
                    }
                }
            }
            Rule::AltTextAssociated => {
                for &i in &order {
                    if self.elems[i].alt && !self.subtree(i).iter().any(|j| self.elems[*j].content) {
                        out.push(self.finding(i, "alternate text on an element with no page content"));
                    }
                }
            }
            Rule::AltTextHidesAnnotation => {
                for &i in &order {
                    if self.elems[i].alt && self.subtree(i).iter().any(|j| !self.elems[*j].annots.is_empty()) {
                        out.push(self.finding(i, "alternate text hides an annotation inside it"));
                    }
                }
            }
            Rule::TableRows => {
                for &i in &order {
                    if self.is(i, b"TR") && !self.parent_is(i, &[b"Table", b"THead", b"TBody", b"TFoot"]) {
                        out.push(self.finding(i, "not in a Table, THead, TBody or TFoot"));
                    }
                }
            }
            Rule::TableCells => {
                for &i in &order {
                    if (self.is(i, b"TH") || self.is(i, b"TD")) && !self.parent_is(i, &[b"TR"]) {
                        out.push(self.finding(i, "not in a TR"));
                    }
                }
            }
            Rule::TableHeaders => {
                for &i in &order {
                    if self.is(i, b"Table") && !self.table_cells(i).iter().any(|(_, c)| self.is(*c, b"TH")) {
                        out.push(self.finding(i, "no header cells (TH)"));
                    }
                }
            }
            Rule::TableRegularity => {
                for &i in &order {
                    if self.is(i, b"Table")
                        && let Some(Ok(Some(why))) = self.table_shapes.get(&i)
                    {
                        out.push(self.finding(i, why));
                    }
                }
            }
            Rule::TableSummary => {
                for &i in &order {
                    if self.is(i, b"Table") && self.attr(i, b"Summary").is_none() {
                        out.push(self.finding(i, "no summary"));
                    }
                }
            }
            Rule::ListItems => {
                for &i in &order {
                    if self.is(i, b"LI") && !self.parent_is(i, &[b"L"]) {
                        out.push(self.finding(i, "not in an L (list)"));
                    }
                }
            }
            Rule::LblLBody => {
                for &i in &order {
                    if (self.is(i, b"Lbl") || self.is(i, b"LBody")) && !self.parent_is(i, &[b"LI"]) && !self.inside_toc_or_note(i) {
                        out.push(self.finding(i, "not in an LI (list item)"));
                    }
                }
            }
            Rule::HeadingNesting => {
                let mut prev = "0".to_owned();
                for &i in &order {
                    let role = String::from_utf8_lossy(&self.elems[i].ty);
                    let Some(digits) = role.strip_prefix('H').filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) else { continue };
                    let level = digits;
                    let mut next = prev.as_bytes().to_vec();
                    let mut carry = true;
                    for digit in next.iter_mut().rev() {
                        if *digit == b'9' {
                            *digit = b'0';
                        } else {
                            *digit += 1;
                            carry = false;
                            break;
                        }
                    }
                    if carry {
                        next.insert(0, b'1');
                    }
                    let skipped = level.len() > next.len() || (level.len() == next.len() && level.as_bytes() > next.as_slice());
                    if skipped {
                        let what = if prev == "0" { format!("the first heading is an H{level}") } else { format!("H{level} follows an H{prev}") };
                        out.push(self.finding(i, &format!("{what}; headings should not skip levels")));
                    }
                    prev = level.to_owned();
                }
            }
            _ => {}
        }
        out
    }

    /// Lbl is also allowed in TOCI and Note-like contexts (PDF 2.0); only list misuse is flagged.
    fn inside_toc_or_note(&self, i: usize) -> bool {
        self.parent_is(i, &[b"TOCI", b"BibEntry", b"Note", b"FENote"])
    }

    /// An attribute from the element's attribute objects (/A), any owner.
    fn attr(&self, i: usize, key: &[u8]) -> Option<&Object> {
        self.elems[i].attrs.iter().find_map(|d| d.get(key))
    }

    fn span(&self, i: usize, key: &[u8]) -> Result<usize, String> {
        let value = match self.attr(i, key) {
            None => return Ok(1),
            Some(Object::Int(n)) if *n >= 1 => usize::try_from(*n).ok(),
            _ => None,
        }
        .ok_or_else(|| "table span is invalid or cannot be represented safely".to_owned())?;
        let limit = if key == b"ColSpan" { MAX_TABLE_COLUMNS } else { MAX_VISITS };
        if value > limit {
            return Err("table span exceeds safe regularity limits".into());
        }
        Ok(value)
    }

    /// The rows of a table (through THead/TBody/TFoot, not into nested tables) and their cells.
    fn table_cells(&self, table: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (r, row) in self.table_rows(table).into_iter().enumerate() {
            for &c in &self.elems[row].children {
                if self.is(c, b"TH") || self.is(c, b"TD") {
                    out.push((r, c));
                }
            }
        }
        out
    }

    fn table_rows(&self, table: usize) -> Vec<usize> {
        let mut rows = Vec::new();
        for &k in &self.elems[table].children {
            match self.elems[k].ty.as_slice() {
                b"TR" => rows.push(k),
                b"THead" | b"TBody" | b"TFoot" => rows.extend(self.elems[k].children.iter().copied().filter(|r| self.is(*r, b"TR"))),
                _ => {}
            }
        }
        rows
    }

    /// One shared work budget for all tables and a hard row-width bound before
    /// allocation. Never clamp spans and then claim the approximated grid is regular.
    fn irregular(&self, table: usize, work: &mut usize) -> Result<Option<String>, String> {
        fn charge(work: &mut usize, amount: usize) -> Result<(), String> {
            *work = work.checked_sub(amount).ok_or_else(|| "aggregate table regularity work budget exceeded".to_owned())?;
            Ok(())
        }
        let rows = self.table_rows(table);
        charge(work, rows.len())?;
        let mut carry: Vec<usize> = Vec::new();
        let mut range: Option<(usize, usize)> = None;
        for row in rows {
            let mut col = 0usize;
            charge(work, carry.len())?;
            let mut next = carry.iter().map(|n| n.saturating_sub(1)).collect::<Vec<_>>();
            for &c in self.elems[row].children.iter().filter(|c| self.is(**c, b"TH") || self.is(**c, b"TD")) {
                charge(work, 1)?;
                while carry.get(col).is_some_and(|n| *n > 0) {
                    charge(work, 1)?;
                    col = col.saturating_add(1);
                }
                let (cs, rs) = (self.span(c, b"ColSpan")?, self.span(c, b"RowSpan")?);
                let end = col
                    .checked_add(cs)
                    .filter(|end| *end <= MAX_TABLE_COLUMNS)
                    .ok_or_else(|| "table row width exceeds safe regularity limits".to_owned())?;
                charge(work, cs.saturating_add(end.saturating_sub(next.len())))?;
                if next.len() < end {
                    next.resize(end, 0);
                }
                for n in next.iter_mut().skip(col).take(cs) {
                    *n = (*n).max(rs.saturating_sub(1));
                }
                col = end;
            }
            while carry.get(col).is_some_and(|n| *n > 0) {
                charge(work, 1)?;
                col = col.saturating_add(1);
            }
            range = Some(range.map_or((col, col), |(lo, hi)| (lo.min(col), hi.max(col))));
            carry = next;
        }
        Ok(range.filter(|(lo, hi)| lo != hi).map(|(lo, hi)| format!("rows have between {lo} and {hi} columns")))
    }
}

struct Reader<'a> {
    doc: &'a Document,
    resolver: Option<&'a crate::tags::RoleResolver>,
    page_index: &'a HashMap<ObjRef, usize>,
    seen: &'a mut HashSet<ObjRef>,
    remaining: usize,
}

impl Reader<'_> {
    fn visit(&mut self, t: &mut Tree, depth: usize) -> bool {
        if depth > MAX_DEPTH || self.remaining == 0 {
            t.issue("structure traversal exceeds safe limits");
            return false;
        }
        self.remaining -= 1;
        true
    }

    fn page_of(&self, t: &mut Tree, d: &Dict, inherited: Option<usize>) -> Option<usize> {
        let Some(raw) = d.get(b"Pg") else { return inherited };
        let Some(id) = raw.as_ref() else {
            t.issue("structure Pg must be an indirect page reference");
            return None;
        };
        match checked_reference(self.doc, id) {
            Ok(value) if matches!(&*value, Object::Dict(p) if p.name(b"Type") == Some(b"Page")) => {
                let page = self.page_index.get(&id).copied();
                if page.is_none() {
                    t.issue("structure Pg is not a document page");
                }
                page
            }
            _ => {
                t.issue("structure Pg is stale, unreadable or not a page dictionary");
                None
            }
        }
    }

    /// Read every value, never silently discard a malformed child. Budget checks
    /// happen before iterating containers; exhaustion stops siblings too.
    fn kids(&mut self, t: &mut Tree, k: &Object, parent: Option<usize>, page: Option<usize>, out: &mut Vec<usize>, depth: usize) {
        if !self.visit(t, depth) {
            return;
        }
        match k {
            Object::Array(a) => {
                if a.len() > self.remaining {
                    t.issue("structure child array exceeds remaining traversal budget");
                    return;
                }
                for x in a {
                    if self.remaining == 0 {
                        t.issue("structure traversal exceeds safe limits");
                        break;
                    }
                    self.kids(t, x, parent, page, out, depth.saturating_add(1));
                }
            }
            Object::Int(mcid) => {
                if *mcid < 0 {
                    t.issue("structure MCID must be nonnegative");
                } else if let Some(p) = parent {
                    t.elems[p].content = true;
                    if page.is_none() {
                        t.issue("integer MCID has no valid inherited page");
                    }
                } else {
                    t.issue("content item cannot be directly under StructTreeRoot");
                }
            }
            Object::Ref(r) => {
                let obj = match checked_reference(self.doc, *r) {
                    Ok(value) => value,
                    Err(e) => {
                        t.issue(e);
                        return;
                    }
                };
                // Identical indirect integers can validly be reused on different pages.
                if !matches!(&*obj, Object::Int(_)) && !self.seen.insert(*r) {
                    t.issue("shared or cyclic structure reference");
                    return;
                }
                if let Object::Dict(d) = &*obj {
                    if self.visit(t, depth.saturating_add(1)) {
                        self.dict(t, d, Some(*r), (parent, page), out, depth.saturating_add(1));
                    }
                } else {
                    self.kids(t, &obj, parent, page, out, depth.saturating_add(1));
                }
            }
            Object::Dict(d) => self.dict(t, d, None, (parent, page), out, depth),
            Object::Stream(_) => t.issue("structure child must not be a stream"),
            _ => t.issue("invalid structure child: expected StructElem, MCID, MCR or OBJR"),
        }
    }

    fn dict(&mut self, t: &mut Tree, d: &Dict, obj: Option<ObjRef>, context: (Option<usize>, Option<usize>), out: &mut Vec<usize>, depth: usize) {
        let (parent, page) = context;
        if d.len() > self.remaining {
            t.issue("structure dictionary exceeds remaining traversal budget");
            return;
        }
        let page = self.page_of(t, d, page);
        match d.name(b"Type") {
            Some(b"MCR") => {
                if d.contains(b"S") {
                    t.issue("MCR must not also declare a structure type S");
                }
                if !matches!(d.get(b"MCID"), Some(Object::Int(n)) if *n >= 0) {
                    t.issue("MCR requires a nonnegative integer MCID");
                    return;
                }
                if let Some(raw) = d.get(b"Stm") {
                    let valid = raw.as_ref().and_then(|r| checked_reference(self.doc, r).ok()).is_some_and(|v| matches!(&*v, Object::Stream(_)));
                    if !valid {
                        t.issue("MCR Stm must be an exact indirect stream reference");
                        return;
                    }
                } else if page.is_none() {
                    t.issue("MCR has neither a stream nor a valid associated page");
                }
                if let Some(p) = parent {
                    t.elems[p].content = true;
                } else {
                    t.issue("MCR cannot be directly under StructTreeRoot");
                }
                return;
            }
            Some(b"OBJR") => {
                if d.contains(b"S") {
                    t.issue("OBJR must not also declare a structure type S");
                }
                let Some(id) = d.reference(b"Obj") else {
                    t.issue("OBJR requires an indirect Obj reference");
                    return;
                };
                let obj = match checked_reference(self.doc, id) {
                    Ok(v) if matches!(&*v, Object::Dict(_) | Object::Stream(_)) => v,
                    _ => {
                        t.issue("OBJR Obj is stale, missing or not a dictionary/stream");
                        return;
                    }
                };
                if page.is_none() {
                    t.issue("OBJR has no valid associated page");
                }
                if let Some(p) = parent {
                    t.elems[p].content = true;
                    if obj.as_dict().is_some_and(|a| a.contains(b"Subtype") && a.contains(b"Rect")) {
                        t.elems[p].annots.push(id);
                    }
                } else {
                    t.issue("OBJR cannot be directly under StructTreeRoot");
                }
                return;
            }
            Some(b"StructElem") | None => {}
            _ => t.issue("structure element has invalid Type"),
        }
        if !d.contains(b"S") {
            t.issue("structure element is missing S");
            return;
        }
        let ty = match self.resolver.map(|r| r.resolve(self.doc, d)) {
            Some(Ok((role, _))) => role.into_bytes(),
            Some(Err(error)) => {
                t.issue(error.to_string());
                Vec::new()
            }
            None => Vec::new(),
        };
        let alt = [b"Alt".as_slice(), b"ActualText"].iter().fold(false, |present, key| {
            let valid = d.get(key).is_some_and(|raw| self.text_present(t, raw, depth.saturating_add(1)));
            present || valid
        });
        let mut attrs = Vec::new();
        if let Some(a) = d.get(b"A") {
            self.attributes(t, a, &mut attrs, &mut HashSet::new(), depth.saturating_add(1));
        }
        let i = t.elems.len();
        t.elems.push(Elem { obj, ty, parent, alt, page, attrs, ..Elem::default() });
        if let Some(p) = parent {
            t.elems[p].children.push(i);
        }
        out.push(i);
        if let Some(k) = d.get(b"K") {
            let mut ignored = Vec::new();
            self.kids(t, k, Some(i), page, &mut ignored, depth.saturating_add(1));
        }
    }

    fn text_present(&mut self, t: &mut Tree, raw: &Object, depth: usize) -> bool {
        if !self.visit(t, depth) {
            return false;
        }
        match raw {
            Object::Ref(id) => match checked_reference(self.doc, *id) {
                Ok(value) => self.text_present(t, &value, depth.saturating_add(1)),
                Err(e) => {
                    t.issue(e);
                    false
                }
            },
            Object::String(s) if s.bytes.len() <= 1_048_576 => !s.to_text().trim().is_empty(),
            _ => {
                t.issue("alternate/actual text must be a bounded string");
                false
            }
        }
    }

    fn span_value(&mut self, t: &mut Tree, raw: &Object, depth: usize) -> Option<i64> {
        if !self.visit(t, depth) {
            return None;
        }
        match raw {
            Object::Int(n) if *n >= 1 => Some(*n),
            Object::Ref(id) => match checked_reference(self.doc, *id) {
                Ok(value) => self.span_value(t, &value, depth.saturating_add(1)),
                Err(e) => {
                    t.issue(e);
                    None
                }
            },
            _ => {
                t.issue("table span attribute must be a positive integer");
                None
            }
        }
    }

    fn attributes(&mut self, t: &mut Tree, raw: &Object, out: &mut Vec<Dict>, path: &mut HashSet<ObjRef>, depth: usize) {
        if !self.visit(t, depth) {
            return;
        }
        match raw {
            Object::Ref(id) => {
                if !path.insert(*id) {
                    t.issue("cyclic structure attributes");
                    return;
                }
                match checked_reference(self.doc, *id) {
                    Ok(v) => self.attributes(t, &v, out, path, depth.saturating_add(1)),
                    Err(e) => t.issue(e),
                }
                path.remove(id);
            }
            Object::Array(values) => {
                if values.len() > self.remaining {
                    t.issue("structure attribute array exceeds remaining traversal budget");
                    return;
                }
                for value in values {
                    if self.remaining == 0 {
                        t.issue("structure attributes exceed safe limits");
                        break;
                    }
                    self.attributes(t, value, out, path, depth.saturating_add(1));
                }
            }
            Object::Dict(d) if d.len() <= self.remaining => {
                // Only these attributes feed checker rules. Do not clone arbitrary
                // unknown nested containers merely to inspect table dimensions.
                let mut checked = Dict::new();
                if d.contains(b"Summary") {
                    checked.set(b"Summary", Object::Bool(true));
                }
                for key in [b"RowSpan".as_slice(), b"ColSpan"] {
                    if let Some(raw) = d.get(key) {
                        checked.set(key, self.span_value(t, raw, depth.saturating_add(1)).map(Object::Int).unwrap_or(Object::Null));
                    }
                }
                out.push(checked);
            }
            Object::Int(n) if *n >= 0 => {} // Attribute revision number.
            _ => t.issue("invalid or oversized structure attribute object"),
        }
    }
}
