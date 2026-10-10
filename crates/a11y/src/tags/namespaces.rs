//! ISO 32000-2 §§14.7.3–14.8.6: namespace-aware role resolution, without authoring
//! namespace maps. An omitted NS is not inherited. Table 356's single-name
//! RoleMapNS targets are PDF 1.7, not another trip through the root RoleMap.
use super::*;

pub const PDF_17_NAMESPACE: &str = "http://iso.org/pdf/ssn";
pub const PDF_20_NAMESPACE: &str = "http://iso.org/pdf2/ssn";
pub const MATHML_NAMESPACE: &str = "http://www.w3.org/1998/Math/MathML";
const COMMON: &str = "Document Part Sect Div Caption NonStruct P H H1 H2 H3 H4 H5 H6 L LI Lbl LBody Table TR TH TD THead TBody TFoot Span Link Annot Ruby RB RT RP Warichu WT WP Figure Formula Form";
const PDF17_ONLY: &str = "Art BlockQuote TOC TOCI Index Private Quote Note Reference BibEntry Code";
const PDF20_ONLY: &str = "DocumentFragment Aside Title FENote Sub Em Strong Artifact";
// Element-name recognition only, not MathML grammar/conformance validation.
// W3C MathML 3, Appendix I.1: https://www.w3.org/TR/MathML3/appendixi.html
const MATHML: &str = "abs and annotation annotation-xml apply approx arccos arccosh arccot arccoth arccsc arccsch arcsec arcsech arcsin arcsinh arctan arctanh arg bind bvar card cartesianproduct cbytes ceiling cerror ci cn codomain complexes compose condition conjugate cos cosh cot coth cs csc csch csymbol curl declare degree determinant diff divergence divide domain domainofapplication emptyset eq equivalent eulergamma exists exp exponentiale factorial factorof false floor fn forall gcd geq grad gt ident image imaginary imaginaryi implies in infinity int integers intersect interval inverse lambda laplacian lcm leq limit list ln log logbase lowlimit lt maction maligngroup malignmark math matrix matrixrow max mean median menclose merror mfenced mfrac mglyph mi min minus mlabeledtr mlongdiv mmultiscripts mn mo mode moment momentabout mover mpadded mphantom mprescripts mroot mrow ms mscarries mscarry msgroup msline mspace msqrt msrow mstack mstyle msub msubsup msup mtable mtd mtext mtr munder munderover naturalnumbers neq none not notanumber notin notprsubset notsubset or otherwise outerproduct partialdiff pi piece piecewise plus power primes product prsubset quotient rationals real reals reln rem root scalarproduct sdev sec sech selector semantics sep set setdiff share sin sinh subset sum tan tanh tendsto times transpose true union uplimit variance vector vectorproduct xor";

/// Recognized terminal roles in a particular standard or domain namespace.
pub fn is_standard_role(role: &str, uri: &str) -> bool {
    let contains = |words: &str| words.split_ascii_whitespace().any(|s| s == role);
    match uri {
        PDF_17_NAMESPACE => contains(COMMON) || contains(PDF17_ONLY),
        PDF_20_NAMESPACE => contains(COMMON) || contains(PDF20_ONLY) || numbered_heading(role),
        MATHML_NAMESPACE => contains(MATHML),
        _ => false,
    }
}
fn numbered_heading(role: &str) -> bool {
    role.strip_prefix('H').is_some_and(|n| n.as_bytes().first().is_some_and(|c| (b'1'..=b'9').contains(c)) && n.bytes().all(|c| c.is_ascii_digit()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct TagNamespace {
    pub id: ObjRef,
    pub uri: String,
}
#[derive(Clone)]
enum Target {
    Default(Vec<u8>),
    Namespace(Vec<u8>, ObjRef),
}
struct Namespace {
    uri: String,
    map: BTreeMap<Vec<u8>, Target>,
    map_refs: BTreeSet<ObjRef>,
}
pub(crate) struct RoleResolver {
    pub(super) declared: Vec<TagNamespace>,
    declared_ids: BTreeSet<ObjRef>,
    namespaces: BTreeMap<ObjRef, Namespace>,
    root_map: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Resolving a COS reference must validate every generation, not only the first.
pub(super) fn checked_resolve(doc: &Document, object: &Object) -> Result<Object> {
    fn bounded(value: &Object) -> Result<()> {
        let size = match value {
            Object::Array(values) => values.len(),
            Object::Dict(dict) => dict.iter().count(),
            Object::Stream(stream) => stream.dict.iter().count(),
            _ => 0,
        };
        if size > LIMIT {
            return Err(invalid("indirect container exceeds safe limits"));
        }
        Ok(())
    }
    bounded(object)?;
    let mut value = object.clone();
    let mut seen = BTreeSet::new();
    for _ in 0..DEPTH {
        let Object::Ref(id) = value else { return Ok(value) };
        if doc.generation(id.num) != id.generation || !seen.insert(id) {
            return Err(invalid("stale or cyclic indirect reference"));
        }
        let next = doc.try_get(id.num).map_err(|_| invalid("unreadable indirect object"))?;
        bounded(&next)?;
        value = (*next).clone();
    }
    Err(invalid("indirect reference depth exceeds safe limits"))
}
fn name(doc: &Document, object: &Object) -> Result<Vec<u8>> {
    let value = checked_resolve(doc, object)?;
    let name =
        value.as_name().filter(|n| !n.is_empty() && n.len() <= 127).ok_or_else(|| invalid("role must be a nonempty name of at most 127 bytes"))?;
    Ok(name.to_vec())
}
fn read_map(doc: &Document, d: &Dict, key: &[u8]) -> Result<Dict> {
    match d.get(key) {
        None => Ok(Dict::new()),
        Some(v) => match checked_resolve(doc, v)? {
            Object::Dict(d) if d.len() <= LIMIT => Ok(d),
            _ => Err(invalid("role map must be a bounded dictionary")),
        },
    }
}
pub(super) fn namespace_uri(doc: &Document, id: ObjRef) -> Result<String> {
    let d = dictionary(doc, id)?;
    if d.contains(b"Type") && d.name(b"Type") != Some(b"Namespace") {
        return Err(invalid("namespace dictionary has invalid Type"));
    }
    let value = checked_resolve(doc, d.get(b"NS").ok_or_else(|| invalid("namespace dictionary has no NS name"))?)?;
    let s = value
        .as_string()
        .filter(|s| !s.bytes.is_empty() && s.bytes.len() <= 4096)
        .ok_or_else(|| invalid("namespace NS must be a nonempty bounded string"))?;
    Ok(s.to_text())
}
fn reference_chain(doc: &Document, raw: Option<&Object>) -> Result<BTreeSet<ObjRef>> {
    let mut refs = BTreeSet::new();
    let mut v = raw.cloned().unwrap_or(Object::Null);
    for _ in 0..DEPTH {
        let Object::Ref(id) = v else { return Ok(refs) };
        if doc.generation(id.num) != id.generation || !refs.insert(id) {
            return Err(invalid("stale or cyclic role-map reference"));
        }
        v = (*doc.get(id)).clone();
    }
    Err(invalid("role-map reference depth exceeds safe limits"))
}
impl RoleResolver {
    pub(crate) fn read(doc: &Document, root: &Dict) -> Result<Self> {
        let mut this = Self { declared: Vec::new(), declared_ids: BTreeSet::new(), namespaces: BTreeMap::new(), root_map: BTreeMap::new() };
        let mut work = 0usize;
        for (source, value) in read_map(doc, root, b"RoleMap")?.iter() {
            if source.is_empty() || source.len() > 127 {
                return Err(invalid("invalid RoleMap source name"));
            }
            this.root_map.insert(source.clone(), name(doc, value)?);
            work = work.saturating_add(1);
        }
        let mut pending = Vec::new();
        if let Some(raw) = root.get(b"Namespaces") {
            let Object::Array(list) = checked_resolve(doc, raw)? else { return Err(invalid("Namespaces must be an array")) };
            if list.len() > LIMIT {
                return Err(invalid("too many namespaces"));
            }
            for item in list {
                let id = item.as_ref().ok_or_else(|| invalid("Namespaces entries must be indirect dictionaries"))?;
                if !this.declared_ids.insert(id) {
                    return Err(invalid("duplicate namespace reference"));
                }
                pending.push(id);
                this.declared.push(TagNamespace { id, uri: namespace_uri(doc, id)? });
            }
        }
        let mut uri_bytes = 0usize;
        while let Some(id) = pending.pop() {
            if this.namespaces.contains_key(&id) {
                continue;
            }
            work = work.saturating_add(1);
            if work > LIMIT {
                return Err(invalid("namespace graph exceeds safe limits"));
            }
            let uri = namespace_uri(doc, id)?;
            uri_bytes = uri_bytes.saturating_add(uri.len());
            if uri_bytes > 1_048_576 {
                return Err(invalid("namespace names exceed safe limits"));
            }
            let d = dictionary(doc, id)?;
            let mut map = BTreeMap::new();
            for (source, value) in read_map(doc, &d, b"RoleMapNS")?.iter() {
                work = work.saturating_add(1);
                if work > LIMIT || source.is_empty() || source.len() > 127 {
                    return Err(invalid("namespace map exceeds safe limits or has invalid names"));
                }
                let target = match checked_resolve(doc, value)? {
                    Object::Name(n) if !n.is_empty() && n.len() <= 127 => Target::Default(n),
                    Object::Array(a) if a.len() == 2 => {
                        let role = name(doc, a.first().ok_or_else(|| invalid("missing mapped role"))?)?;
                        let ns = a.get(1).and_then(Object::as_ref).ok_or_else(|| invalid("RoleMapNS target namespace must be indirect"))?;
                        pending.push(ns);
                        Target::Namespace(role, ns)
                    }
                    _ => return Err(invalid("RoleMapNS value must be a name or [name namespace-reference]")),
                };
                map.insert(source.clone(), target);
            }
            this.namespaces.insert(id, Namespace { uri, map, map_refs: reference_chain(doc, d.get(b"RoleMapNS"))? });
        }
        // Validate unused mappings too; bound the aggregate quadratic-chain work.
        let mut budget = 1_000_000usize;
        for role in this.root_map.keys() {
            this.resolve_name(None, role, &mut budget)?;
        }
        for (id, ns) in &this.namespaces {
            for role in ns.map.keys() {
                this.resolve_name(Some(*id), role, &mut budget)?;
            }
        }
        Ok(this)
    }
    fn resolve_name(&self, mut ns: Option<ObjRef>, original: &[u8], budget: &mut usize) -> Result<(String, String)> {
        let mut role = original.to_vec();
        let mut seen = BTreeSet::new();
        for _ in 0..DEPTH {
            *budget = budget.checked_sub(1).ok_or_else(|| invalid("role-map validation work limit exceeded"))?;
            if !seen.insert((ns, role.clone())) {
                return Err(invalid("cyclic namespace role mapping"));
            }
            let (uri, target) = match ns {
                None => (PDF_17_NAMESPACE, self.root_map.get(&role).cloned().map(Target::Default)),
                Some(id) => {
                    let n = self.namespaces.get(&id).ok_or_else(|| invalid("missing referenced namespace"))?;
                    (n.uri.as_str(), n.map.get(&role).cloned())
                }
            };
            match target {
                Some(Target::Namespace(next, target_ns)) => {
                    role = next;
                    ns = Some(target_ns);
                }
                Some(Target::Default(next)) if ns.is_none() => {
                    role = next;
                }
                Some(Target::Default(next)) => return terminal(&next, PDF_17_NAMESPACE),
                None => return terminal(&role, uri),
            }
        }
        Err(invalid("namespace role mapping depth exceeds safe limits"))
    }
    pub(crate) fn resolve(&self, doc: &Document, d: &Dict) -> Result<(String, String)> {
        let ns = match d.get(b"NS") {
            None => None,
            Some(v) => {
                let id = v.as_ref().ok_or_else(|| invalid("StructElem NS must be an indirect namespace dictionary"))?;
                if !self.declared_ids.contains(&id) {
                    return Err(invalid("StructElem NS is missing from root Namespaces"));
                }
                Some(id)
            }
        };
        let role = name(doc, d.get(b"S").ok_or_else(|| invalid("structure element has no S"))?)?;
        let mut budget = DEPTH;
        self.resolve_name(ns, &role, &mut budget)
    }
    pub(super) fn reject_shared_root_map(&self, doc: &Document, root: &Dict) -> Result<()> {
        let refs = reference_chain(doc, root.get(b"RoleMap"))?;
        if self.namespaces.values().any(|n| !n.map_refs.is_disjoint(&refs)) {
            return Err(invalid("RoleMap shares a dictionary with RoleMapNS; namespace-map authoring is unsupported"));
        }
        Ok(())
    }
}
fn terminal(role: &[u8], uri: &str) -> Result<(String, String)> {
    let role = std::str::from_utf8(role).map_err(|_| invalid("unrecognized non-UTF8 structure role"))?;
    if !is_standard_role(role, uri) {
        return Err(invalid(format!("unresolved structure role {role} in namespace {uri}")));
    }
    Ok((role.into(), uri.into()))
}

/// Metadata does not need a valid role chain or ParentTree. It does need exactly one
/// addressable target; ambiguity/truncated traversal must never edit several rows.
pub(super) fn validate_metadata_target(doc: &Document, tree: &TagTree, id: ObjRef) -> Result<()> {
    let e = element(tree, id)?;
    let d = dictionary(doc, id)?;
    if d.name(b"S").is_none_or(|s| s.is_empty()) || (d.contains(b"Type") && d.name(b"Type") != Some(b"StructElem")) {
        return Err(invalid("metadata target must be a valid StructElem dictionary"));
    }
    let root = tree.root.ok_or_else(|| invalid("direct structure root is read-only"))?;
    let parent = e.parent.unwrap_or(root);
    dictionary(doc, parent)?;
    if d.reference(b"P") != Some(parent) {
        return Err(invalid("metadata target has inconsistent P"));
    }
    if let Some(pg) = d.get(b"Pg") {
        let pg = pg.as_ref().ok_or_else(|| invalid("invalid metadata target Pg"))?;
        dictionary(doc, pg)?;
        if !tree.pages.contains(&pg) {
            return Err(invalid("metadata target Pg is not a document page"));
        }
    }
    if let Some(ns) = d.get(b"NS") {
        namespace_uri(doc, ns.as_ref().ok_or_else(|| invalid("metadata target NS must be indirect"))?)?;
    }
    let rd = dictionary(doc, root)?;
    let mut pending = rd.get(b"K").cloned().map(|k| vec![(k, 0usize)]).unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    let mut found = 0usize;
    while let Some((raw, depth)) = pending.pop() {
        count = count.saturating_add(1);
        if count > LIMIT || depth > DEPTH {
            return Err(invalid("cannot establish metadata target uniqueness within traversal limits"));
        }
        let value = checked_resolve(doc, &raw)?;
        match value {
            Object::Array(a) => {
                if let Some(r) = raw.as_ref()
                    && !seen.insert(r)
                {
                    return Err(invalid("shared/cyclic structure array makes metadata target ambiguous"));
                }
                if pending.len().saturating_add(a.len()).saturating_add(count) > LIMIT {
                    return Err(invalid("metadata traversal limit exceeded"));
                }
                pending.extend(a.into_iter().map(|v| (v, depth.saturating_add(1))));
            }
            Object::Dict(d) if d.contains(b"S") => {
                let r = raw.as_ref().ok_or_else(|| invalid("direct structure element makes metadata target uniqueness uncertain"))?;
                if !seen.insert(r) {
                    return Err(invalid("shared/cyclic structure element makes metadata target ambiguous"));
                }
                if r == id {
                    found = found.saturating_add(1);
                }
                if let Some(k) = d.get(b"K") {
                    pending.push((k.clone(), depth.saturating_add(1)));
                }
            }
            Object::Dict(d) if matches!(d.name(b"Type"), Some(b"MCR" | b"OBJR")) => {}
            Object::Int(n) if n >= 0 => {}
            _ => return Err(invalid("unsupported structure child prevents safe metadata traversal")),
        }
    }
    if found != 1 {
        return Err(invalid("metadata target is not uniquely reachable"));
    }
    Ok(())
}
