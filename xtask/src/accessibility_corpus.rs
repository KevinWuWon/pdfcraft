//! Strict accessibility corpus evidence. PDFs remain external and are never baselined from observed output.
//! Reconstructed after workspace loss; historical reports are not test results for this implementation.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROBE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LOG_BYTES: usize = 256 * 1024;
const MAX_ORACLE_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const REQUIRED_STAGES: &[&str] = &["input", "open", "render", "inspect_tags", "checker", "figures_api_parity", "geometry", "original_unchanged"];
const REQUIRED_EDITS: &[&str] = &[
    "title_property",
    "reorder",
    "reparent",
    "leaf_move_integer_mcid",
    "leaf_move_page_mcr",
    "leaf_move_form_mcr",
    "leaf_move_objr",
    "role_map",
    "identifier",
    "unwrap",
    "delete",
    "artifact",
    "metadata_properties",
    "create_root",
];
const EDIT_INVARIANTS: &[&str] = &["undo", "redo", "save_reopen", "exact_ownership", "all_page_artwork", "unknown_data_save_reopen"];
const SCOPE: &str = "Operational support is separate from upstream target-rule expectations and overall accessibility conformance.";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Source {
    id: String,
    revision: String,
    expected_files: usize,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    schema_version: u32,
    sources: Vec<Source>,
    files: Vec<CorpusFile>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Profile {
    Ua1,
    Ua2,
}
impl Profile {
    fn flavour(self) -> &'static str {
        match self {
            Self::Ua1 => "ua1",
            Self::Ua2 => "ua2",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Ua1 => "PDF/UA-1 validation profile",
            Self::Ua2 => "PDF/UA-2 + Tagged PDF validation profile",
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Expected {
    Pass,
    Fail,
    Unknown,
}
impl Expected {
    fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Conformance {
    Unknown,
    ReferencePositive,
}
fn test_id<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        Value::String(s) if !s.is_empty() => Ok(Some(s)),
        Value::Number(n) if n.as_u64().is_some() => Ok(Some(n.to_string())),
        _ => Err(serde::de::Error::custom("test must be a string, unsigned integer or null")),
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Target {
    #[serde(default)]
    clause: Option<String>,
    #[serde(default, deserialize_with = "test_id")]
    test: Option<String>,
    expected: Expected,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExpectedRefusal {
    operation: String,
    reason_code: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExpectedStageRefusal {
    stage: String,
    reason_code: String,
    reason: String,
    category: String,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CorpusFile {
    source: String,
    path: String,
    sha256: String,
    profile: Profile,
    feature_tags: Vec<String>,
    upstream_target: Target,
    overall_conformance: Conformance,
    #[serde(default)]
    independent_inventory: Option<Value>,
    #[serde(default)]
    expected_open_transform: Option<Value>,
    #[serde(default)]
    expected_target_inapplicability: Option<Value>,
    #[serde(default)]
    expected_refusals: Vec<ExpectedRefusal>,
    #[serde(default)]
    expected_stage_refusals: Vec<ExpectedStageRefusal>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Debug)]
struct Options {
    manifest: PathBuf,
    probe: PathBuf,
    output: PathBuf,
    roots: BTreeMap<String, PathBuf>,
    sources: BTreeSet<String>,
    timeout: Duration,
    jobs: usize,
    require_full_support: bool,
    verapdf: Option<PathBuf>,
    resume: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let mut result = Options {
        manifest: super::gates::root().join("xtask/baselines/accessibility-corpus.json"),
        probe: PathBuf::new(),
        output: PathBuf::new(),
        roots: BTreeMap::new(),
        sources: BTreeSet::new(),
        timeout: Duration::from_secs(60),
        jobs: 1,
        require_full_support: false,
        verapdf: None,
        resume: false,
    };
    let mut args = args.iter();
    while let Some(key) = args.next() {
        if key == "--require-full-support" {
            result.require_full_support = true;
            continue;
        }
        if key == "--resume" {
            result.resume = true;
            continue;
        }
        let value = args.next().with_context(|| format!("missing value for {key}"))?;
        match key.as_str() {
            "--manifest" => result.manifest = value.into(),
            "--probe" => result.probe = value.into(),
            "--output" => result.output = value.into(),
            "--verapdf" => result.verapdf = Some(value.into()),
            "--source" => {
                ensure!(result.sources.insert(value.clone()), "duplicate selected source {value}");
            }
            "--corpus-root" => {
                let (id, path) = value.split_once('=').context("--corpus-root requires ID=DIR")?;
                ensure!(
                    !id.is_empty() && !path.is_empty() && result.roots.insert(id.into(), path.into()).is_none(),
                    "empty or duplicate corpus root"
                );
            }
            "--timeout" => {
                let seconds: u64 = value.parse().context("invalid timeout")?;
                ensure!((1..=3600).contains(&seconds), "timeout must be 1..3600 seconds");
                result.timeout = Duration::from_secs(seconds);
            }
            "--jobs" => {
                result.jobs = value.parse().context("invalid jobs")?;
                ensure!((1..=4).contains(&result.jobs), "jobs must be 1..4");
            }
            _ => bail!("unknown a11y-corpus option {key}"),
        }
    }
    ensure!(!result.probe.as_os_str().is_empty() && !result.output.as_os_str().is_empty(), "--probe and --output are required");
    Ok(result)
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn relative_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\\') && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(file.metadata()?.len() <= limit, "file exceeds size cap: {}", path.display());
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "file grew beyond size cap");
    Ok(bytes)
}
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        if let Some(part) = bytes.get(..n) {
            hash.update(part);
        }
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.pending");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}
fn status(value: &Value) -> Option<&str> {
    value.get("status").and_then(Value::as_str)
}
fn string_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_u64().is_some() => Some(n.to_string()),
        _ => None,
    }
}
fn has_evidence(value: &Value) -> bool {
    value.get("justification").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty())
        && value
            .get("source_evidence")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty() && items.iter().all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())))
}
fn validate_manifest(m: &Manifest, selection: &BTreeSet<String>) -> Result<()> {
    ensure!(
        m.schema_version == 1 && !m.files.is_empty() && m.files.len() <= MAX_FILES,
        "manifest must contain 1..{MAX_FILES} files and schema_version=1"
    );
    let mut sources = BTreeMap::new();
    for source in &m.sources {
        ensure!(
            !source.id.is_empty() && !source.revision.is_empty() && source.expected_files > 0 && sources.insert(source.id.as_str(), source).is_none(),
            "invalid/duplicate source definition"
        );
    }
    ensure!(selection.iter().all(|id| sources.contains_key(id.as_str())), "unknown selected source");
    let mut paths = BTreeSet::new();
    let mut counts = BTreeMap::<&str, usize>::new();
    for f in &m.files {
        ensure!(
            sources.contains_key(f.source.as_str()) && relative_path(&f.path) && valid_sha256(&f.sha256) && paths.insert((&f.source, &f.path)),
            "invalid or duplicate source/path/hash"
        );
        *counts.entry(&f.source).or_default() += 1;
        ensure!(
            f.upstream_target.clause.is_some() == f.upstream_target.test.is_some(),
            "partial target mapping is not allowed; use both null when unverified"
        );
        if let Some(clause) = &f.upstream_target.clause {
            ensure!(!clause.trim().is_empty(), "empty target clause");
        }
        if let Some(inventory) = &f.independent_inventory {
            ensure!(inventory.is_object(), "invalid independent inventory");
            for name in ["pages", "addressable_structure_elements"] {
                if let Some(value) = inventory.get(name).filter(|v| !v.is_null()) {
                    ensure!(value.as_u64().is_some(), "inventory {name} must be unsigned");
                }
            }
        }
        let mut operations = BTreeSet::new();
        for refusal in &f.expected_refusals {
            ensure!(
                REQUIRED_EDITS.contains(&refusal.operation.as_str()) && operations.insert(&refusal.operation) && !refusal.reason_code.is_empty(),
                "duplicate or invalid expected refusal"
            );
            ensure!(
                refusal.category.as_deref().is_none_or(|v| ["safe_invalid_input", "permission_restricted", "unsupported_valid_input"].contains(&v)),
                "invalid refusal category"
            );
            if let Some(reason) = &refusal.reason {
                ensure!(!reason.trim().is_empty(), "empty exact refusal reason");
            }
            if refusal.reason_code == "structure_edit_rejected" {
                ensure!(
                    refusal.reason.is_some() && has_evidence(&serde_json::to_value(refusal)?),
                    "structural refusal requires exact reason and independent evidence"
                );
            }
        }
        let mut stages = BTreeSet::new();
        for refusal in &f.expected_stage_refusals {
            ensure!(
                refusal.stage == "figures_api_parity"
                    && stages.insert(&refusal.stage)
                    && refusal.reason_code == "structure_resolution_incomplete"
                    && refusal.reason.strip_prefix("cannot enumerate figures: ").is_some_and(|s| !s.trim().is_empty())
                    && ["safe_invalid_input", "permission_restricted", "unsupported_valid_input"].contains(&refusal.category.as_str())
                    && has_evidence(&serde_json::to_value(refusal)?),
                "stage refusal requires one exact Figure diagnostic, category and independent source evidence"
            );
        }
        if let Some(transform) = &f.expected_open_transform {
            ensure!(
                transform.get("kind").and_then(Value::as_str) == Some("dynamic_xfa_layout") && has_evidence(transform),
                "invalid expected open transform"
            );
        }
        if let Some(expected) = &f.expected_target_inapplicability {
            ensure!(
                f.upstream_target.expected == Expected::Pass
                    && has_evidence(expected)
                    && expected.get("specification").and_then(Value::as_str).is_some()
                    && expected.get("specification") == f.upstream_target.metadata.get("specification")
                    && expected.get("clause").and_then(Value::as_str) == f.upstream_target.clause.as_deref()
                    && string_id(expected.get("test")) == f.upstream_target.test
                    && f.upstream_target.test.is_some(),
                "target inapplicability must match an exact PASS-intent rule with independent evidence"
            );
        }
        for value in [f.metadata.get("manual_review_required"), f.metadata.get("technique_handling").and_then(|v| v.get("manual_review_required"))]
            .into_iter()
            .flatten()
        {
            ensure!(value.is_boolean(), "manual_review_required must be boolean");
        }
        if let Some(value) = f.metadata.get("technique_handling") {
            ensure!(value.is_object(), "technique_handling must be an object");
        }
    }
    for (id, source) in sources {
        ensure!(counts.get(id).copied().unwrap_or(0) == source.expected_files, "source {id} population does not match pinned count");
    }
    ensure!(m.files.iter().any(|f| selection.is_empty() || selection.contains(&f.source)), "empty selected corpus");
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Captured {
    text: String,
    total_bytes: u64,
    truncated: bool,
    read_error: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProcessResult {
    exit_code: Option<i32>,
    success: bool,
    timed_out: bool,
    output_limit_exceeded: bool,
    elapsed_ms: u64,
    stdout: Captured,
    stderr: Captured,
}
fn capture<R: Read>(mut reader: R, limit: usize, exceeded: Arc<AtomicBool>) -> Captured {
    let mut out = Captured::default();
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                out.total_bytes = out.total_bytes.saturating_add(n as u64);
                let keep = n.min(limit.saturating_sub(retained.len()));
                if let Some(bytes) = buffer.get(..keep) {
                    retained.extend_from_slice(bytes);
                }
                if out.total_bytes > limit as u64 {
                    out.truncated = true;
                    exceeded.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => {
                out.read_error = Some(error.to_string());
                break;
            }
        }
    }
    out.text = String::from_utf8_lossy(&retained).into_owned();
    out
}
fn terminate_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/PID", &child.id().to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}
fn execute(command: &mut Command, timeout: Duration, stdout_cap: usize, watched: Option<(&Path, u64)>) -> Result<ProcessResult> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = command.spawn().context("spawn isolated process")?;
    let stdout = child.stdout.take().context("child stdout unavailable")?;
    let stderr = child.stderr.take().context("child stderr unavailable")?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let out_flag = Arc::clone(&exceeded);
    let err_flag = Arc::clone(&exceeded);
    let out_reader = thread::spawn(move || capture(stdout, stdout_cap, out_flag));
    let err_reader = thread::spawn(move || capture(stderr, MAX_LOG_BYTES, err_flag));
    let mut result = ProcessResult::default();
    let mut wait_error = None;
    let exit = loop {
        result.timed_out = started.elapsed() >= timeout;
        result.output_limit_exceeded =
            exceeded.load(Ordering::Relaxed) || watched.is_some_and(|(path, cap)| fs::metadata(path).is_ok_and(|m| m.len() > cap));
        if result.timed_out || result.output_limit_exceeded {
            terminate_group(&mut child);
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(exit)) => break Some(exit),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                wait_error = Some(error.to_string());
                terminate_group(&mut child);
                break child.wait().ok();
            }
        }
    };
    // Descendants may otherwise keep pipes open after the immediate child exits.
    terminate_group(&mut child);
    result.stdout = out_reader.join().map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))?;
    result.stderr = err_reader.join().map_err(|_| anyhow::anyhow!("stderr capture thread panicked"))?;
    result.output_limit_exceeded |= result.stdout.truncated || result.stderr.truncated;
    result.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result.exit_code = exit.as_ref().and_then(std::process::ExitStatus::code);
    result.success = exit.is_some_and(|s| s.success())
        && !result.timed_out
        && !result.output_limit_exceeded
        && result.stdout.read_error.is_none()
        && result.stderr.read_error.is_none()
        && wait_error.is_none();
    if let Some(error) = wait_error {
        result.stderr.read_error = Some(error);
    }
    Ok(result)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FrozenProbe {
    source_executable: PathBuf,
    executable: PathBuf,
    sha256: String,
}
fn freeze_probe(source: &Path, output: &Path) -> Result<FrozenProbe> {
    let source = source.canonicalize().context("probe executable missing")?;
    ensure!(source.is_file(), "probe is not a file");
    let sha256 = hash_file(&source)?;
    let tools = output.join("tools");
    fs::create_dir_all(&tools)?;
    let executable = tools.join(if cfg!(windows) { "a11y_corpus_probe.exe" } else { "a11y_corpus_probe" });
    let mut input = File::open(&source)?;
    let mut copy = OpenOptions::new().write(true).create_new(true).open(&executable)?;
    std::io::copy(&mut input, &mut copy)?;
    copy.sync_all()?;
    drop(copy);
    fs::set_permissions(&executable, fs::metadata(&source)?.permissions())?;
    let mut permissions = fs::metadata(&executable)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&executable, permissions)?;
    ensure!(hash_file(&source)? == sha256 && hash_file(&executable)? == sha256, "probe executable changed during freezing");
    Ok(FrozenProbe { source_executable: source, executable, sha256 })
}
fn planned_output(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_owned() } else { std::env::current_dir()?.join(path) };
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(existing.file_name().context("output path has no existing ancestor")?.to_owned());
        existing = existing.parent().context("output path has no parent")?;
    }
    let mut resolved = existing.canonicalize()?;
    for item in suffix.into_iter().rev() {
        resolved.push(item);
    }
    Ok(resolved)
}
fn source_path(f: &CorpusFile, roots: &BTreeMap<String, PathBuf>) -> Result<PathBuf> {
    let root = roots.get(&f.source).with_context(|| format!("corpus root missing: {}", f.source))?.canonicalize()?;
    let path = root.join(&f.path).canonicalize().with_context(|| format!("selected source PDF missing: {}/{}", f.source, f.path))?;
    ensure!(path.starts_with(&root) && path.is_file(), "source path escaped its root or is not a file");
    Ok(path)
}
fn verified_input(f: &CorpusFile, roots: &BTreeMap<String, PathBuf>) -> Result<(PathBuf, Vec<u8>)> {
    let path = source_path(f, roots)?;
    let bytes = read_bounded(&path, MAX_INPUT_BYTES)?;
    ensure!(digest(&bytes) == f.sha256, "selected source SHA-256 mismatch");
    if let Some(size) = f.metadata.get("bytes").and_then(Value::as_u64) {
        ensure!(bytes.len() as u64 == size, "selected source byte count mismatch");
    }
    Ok((path, bytes))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Assessment {
    errors: Vec<String>,
    expected_refusals: usize,
    expected_refusal_categories: BTreeMap<String, usize>,
    expected_stage_refusals: usize,
    expected_stage_refusal_categories: BTreeMap<String, usize>,
    stage_refusal_expectations: BTreeMap<String, String>,
    refusal_expectations: BTreeMap<String, String>,
    unexpected_refusals: usize,
    supported_edits: usize,
    not_applicable_edits: usize,
    stage_outcomes: BTreeMap<String, String>,
    independent_inventory: BTreeMap<String, Value>,
    open_transform: Value,
    post_open_edits: usize,
    manual_review: Value,
}
fn manual_review_assessment(f: &CorpusFile) -> Value {
    let handling = f.metadata.get("technique_handling");
    let required = handling.and_then(|h| h.get("manual_review_required")).and_then(Value::as_bool).unwrap_or(false)
        || f.metadata.get("manual_review_required").and_then(Value::as_bool).unwrap_or(false);
    let declared =
        handling.and_then(|h| h.get("manual_review_status")).or_else(|| f.metadata.get("manual_tests_status")).cloned().unwrap_or(Value::Null);
    json!({"required":required,"status":if !required {"not_required"} else if declared.as_str() == Some("not_performed") {"not_performed"} else {"unverified"},"declared_status":declared,"accessibility_conformance":"not_established_by_operational_probe"})
}
fn assess_open_transform(f: &CorpusFile, p: &Value) -> Value {
    let open = p.pointer("/stages/open").unwrap_or(&Value::Null);
    let changed = open.get("bytes_changed_during_open").and_then(Value::as_bool);
    let kind = open.get("open_transform_kind").and_then(Value::as_str);
    let status = match changed {
        Some(false) if kind == Some("none") => "not_transformed",
        Some(true)
            if kind == Some("dynamic_xfa_layout")
                && open.get("dynamic_xfa_regenerated").and_then(Value::as_bool) == Some(true)
                && f.expected_open_transform.as_ref().and_then(|v| v.get("kind")).and_then(Value::as_str) == kind =>
        {
            "expected_unsupported"
        }
        Some(true) => "unexpected",
        _ => "unverified",
    };
    json!({"status":status,"observed_kind":kind,"bytes_changed":changed,"post_open_sha256":open.get("post_open_sha256"),"source_preservation":open.get("source_to_open_preservation")})
}
fn compare_inventory(f: &CorpusFile, p: &Value, transform: &Value) -> BTreeMap<String, Value> {
    [("pages", "/stages/open/pages"), ("addressable_structure_elements", "/stages/inspect_tags/elements")]
        .into_iter()
        .map(|(name, pointer)| {
            let expected = f.independent_inventory.as_ref().and_then(|v| v.get(name)).and_then(Value::as_u64);
            let observed = p.pointer(pointer).and_then(Value::as_u64);
            let status = match (expected, observed) {
                (None, _) => "uninventoried",
                (Some(_), None) => "probe_missing",
                (Some(a), Some(b)) if a == b => "matched",
                _ if status(transform) == Some("expected_unsupported") => "expected_unsupported",
                _ => "mismatch",
            };
            (name.into(), json!({"expected":expected,"observed":observed,"status":status}))
        })
        .collect()
}
fn matching_expected_refusal<'a>(f: &'a CorpusFile, name: &str, edit: &Value) -> Option<&'a ExpectedRefusal> {
    f.expected_refusals.iter().find(|e| {
        e.operation == name
            && edit.get("reason_code").and_then(Value::as_str) == Some(e.reason_code.as_str())
            && e.reason.as_deref().is_none_or(|r| edit.get("reason").and_then(Value::as_str) == Some(r))
    })
}
// Retained source of the final proof helper. Surrounding runner was lost.
fn verify_preserved_creation_associations(probe: &Value, operation: &Value, count: u64) -> Result<()> {
    ensure!(count > 0, "preserved-association proof requires a nonzero warning count");
    let evidence = operation.get("root_creation_evidence").context("create_root evidence missing")?;
    ensure!(
        evidence.get("mode").and_then(Value::as_str) == Some("create_first_tag_existing_root")
            && probe.pointer("/stages/inspect_tags/root_children").and_then(Value::as_u64) == Some(0),
        "preserved associations are allowed only for first-tag creation in an existing empty root"
    );
    let blockers = probe.pointer("/stages/inspect_tags/edit_blockers").context("create_root edit-blocker classification missing")?;
    ensure!(
        blockers.get("total").and_then(Value::as_u64) == Some(0)
            && blockers.get("omitted").and_then(Value::as_u64) == Some(0)
            && blockers.get("items").and_then(Value::as_array).is_some_and(Vec::is_empty),
        "create_root has nonzero or incompletely reported edit blockers"
    );
    let issues = probe.pointer("/stages/inspect_tags/issues").context("create_root inspection warnings missing")?;
    let warnings = issues.get("items").and_then(Value::as_array).context("create_root warning items missing")?;
    let count_usize = usize::try_from(count).context("create_root association count is too large")?;
    ensure!(
        issues.get("omitted").and_then(Value::as_u64) == Some(0) && warnings.len() == count_usize,
        "create_root warning classification must be complete"
    );
    let associations = probe.pointer("/stages/inspect_tags/preserved_associations").context("create_root typed associations missing")?;
    let entries = associations.get("items").and_then(Value::as_array).context("create_root typed association items missing")?;
    ensure!(
        associations.get("total").and_then(Value::as_u64) == Some(count)
            && associations.get("omitted").and_then(Value::as_u64) == Some(0)
            && entries.len() == count_usize,
        "create_root typed association counts do not exactly match all warnings"
    );
    let mut identities = BTreeSet::new();
    let mut owners = BTreeSet::new();
    let mut expected_warnings = Vec::new();
    let mut inventory = Vec::new();
    for association in entries {
        ensure!(association.get("kind").and_then(Value::as_str) == Some("unreachable_owner"), "create_root has another association kind");
        let key = association.get("key").and_then(Value::as_i64).context("create_root association key missing or invalid")?;
        ensure!(key >= 0, "create_root association key is negative");
        let mcid = association.get("mcid").context("create_root association MCID field missing")?;
        let mcid = if mcid.is_null() {
            None
        } else {
            let value = mcid.as_i64().context("create_root association MCID invalid")?;
            ensure!(value >= 0, "create_root association MCID is negative");
            Some(value)
        };
        let owner = association.get("owner_ref").and_then(Value::as_str).context("create_root association owner reference missing")?;
        let mut fields = owner.split_whitespace();
        let number =
            fields.next().and_then(|field| field.parse::<u32>().ok()).filter(|number| *number > 0).context("create_root owner number invalid")?;
        let generation = fields.next().and_then(|field| field.parse::<u16>().ok()).context("create_root owner generation missing or invalid")?;
        ensure!(
            fields.next() == Some("R") && fields.next().is_none() && owner == format!("{number} {generation} R"),
            "create_root owner must be an exact generation-bearing indirect reference"
        );
        ensure!(identities.insert((key, mcid)), "create_root contains duplicate typed association slots");
        inventory.push(json!(["unreachable_owner", key, mcid, owner]));
        owners.insert(owner);
        expected_warnings.push(format!("ParentTree key {key} refers to unreachable tag {owner} (preserved association)"));
    }
    let mut actual_warnings =
        warnings.iter().map(|warning| warning.as_str().map(str::to_owned).context("create_root warning is not text")).collect::<Result<Vec<_>>>()?;
    expected_warnings.sort();
    actual_warnings.sort();
    ensure!(actual_warnings == expected_warnings, "create_root warnings are not exactly the typed preserved associations");
    let proof = evidence.get("preserved_associations").context("create_root raw preserved-association proof missing")?;
    ensure!(
        status(proof) == Some("passed")
            && proof.get("count").and_then(Value::as_u64) == Some(count)
            && proof.get("classification").and_then(Value::as_str) == Some("unreachable_owner")
            && proof.get("warnings_exactly_classified").and_then(Value::as_bool) == Some(true)
            && proof.get("edit_blockers").and_then(Value::as_u64) == Some(0),
        "create_root preservation proof classification/counts do not match inspection"
    );
    let inventory_sha =
        proof.get("association_inventory_sha256").and_then(Value::as_str).context("create_root typed association inventory hash missing")?;
    ensure!(
        valid_sha256(inventory_sha) && inventory_sha == digest(&serde_json::to_vec(&inventory)?),
        "create_root typed association keys/MCIDs/owner generations do not match their preservation proof"
    );
    for graph in ["index_slots", "owner_graph", "index_graph"] {
        let before =
            proof.get(format!("{graph}_sha256_before")).and_then(Value::as_str).context("create_root before-preservation SHA-256 missing")?;
        let after = proof.get(format!("{graph}_sha256_after")).and_then(Value::as_str).context("create_root after-preservation SHA-256 missing")?;
        ensure!(valid_sha256(before) && valid_sha256(after) && before == after, "create_root {graph} changed or has invalid hashes");
    }
    let owners_count = u64::try_from(owners.len()).context("create_root owner count is too large")?;
    ensure!(
        proof.get("owner_objects_checked").and_then(Value::as_u64).is_some_and(|checked| checked >= owners_count)
            && proof.get("index_objects_checked").and_then(Value::as_u64).is_some_and(|checked| checked > 0),
        "create_root preservation proof did not check all original owner/index objects"
    );
    let snapshots = proof.get("verified_snapshots").and_then(Value::as_array).context("create_root preserved-association snapshots missing")?;
    let snapshot_names = snapshots.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>();
    ensure!(
        snapshots.len() == 3 && snapshot_names == BTreeSet::from(["after_edit", "after_redo", "after_save_reopen"]),
        "create_root preservation was not verified after edit, redo and save/reopen"
    );
    Ok(())
}

fn verify_create_root_evidence(probe: &Value, operation: &Value) -> Result<()> {
    ensure!(
        probe.pointer("/stages/inspect_tags/elements").and_then(Value::as_u64) == Some(0),
        "create_root requires an independently inspectable empty original tag tree"
    );
    let issues = probe.pointer("/stages/inspect_tags/issues/total").and_then(Value::as_u64).context("create_root original issue count missing")?;
    if issues > 0 {
        verify_preserved_creation_associations(probe, operation, issues)?;
    } else {
        if let Some(items) = probe.pointer("/stages/inspect_tags/issues/items") {
            ensure!(items.as_array().is_some_and(Vec::is_empty), "create_root zero issue count contradicts warning items");
        }
        if let Some(omitted) = probe.pointer("/stages/inspect_tags/issues/omitted") {
            ensure!(omitted.as_u64() == Some(0), "create_root zero issue count has omitted warnings");
        }
        for name in ["edit_blockers", "preserved_associations"] {
            if let Some(summary) = probe.pointer(&format!("/stages/inspect_tags/{name}")) {
                ensure!(
                    summary.get("total").and_then(Value::as_u64) == Some(0)
                        && summary.get("omitted").and_then(Value::as_u64) == Some(0)
                        && summary.get("items").and_then(Value::as_array).is_some_and(Vec::is_empty),
                    "create_root zero issue count contradicts its {name} classification"
                );
            }
        }
        if let Some(proof) = operation.pointer("/root_creation_evidence/preserved_associations") {
            ensure!(
                status(proof) == Some("not_applicable")
                    && proof.get("count").and_then(Value::as_u64) == Some(0)
                    && proof.get("edit_blockers").and_then(Value::as_u64) == Some(0)
                    && proof.get("warnings_exactly_classified").and_then(Value::as_bool) == Some(true),
                "create_root zero issue count contradicts its preservation proof"
            );
        }
    }
    ensure!(
        operation.get("unknown_data_create_root").and_then(Value::as_str) == Some("passed"),
        "create_root unknown-data preservation was not verified"
    );
    let evidence = operation.get("root_creation_evidence").context("create_root evidence missing")?;
    ensure!(
        status(evidence) == Some("passed")
            && evidence.get("original_tag_count").and_then(Value::as_u64) == Some(0)
            && evidence.get("new_document_tag").and_then(Value::as_bool) == Some(true)
            && evidence.get("catalog_parent_links").and_then(Value::as_str) == Some("passed")
            && evidence.get("mark_info_change").and_then(Value::as_str) == Some("passed")
            && evidence.get("content_associations_created").and_then(Value::as_u64) == Some(0),
        "create_root linkage, MarkInfo, or empty-content invariants missing"
    );
    let reference = |name| {
        evidence.get(name).and_then(Value::as_str).filter(|value| !value.trim().is_empty()).with_context(|| format!("create_root missing {name}"))
    };
    let catalog = reference("catalog_ref")?;
    let root = reference("struct_tree_root_ref")?;
    let document = reference("document_tag_ref")?;
    ensure!(catalog != root && root != document && catalog != document, "create_root object identities are not distinct");
    match evidence.get("mode").and_then(Value::as_str) {
        Some("create_structure_root") => ensure!(
            probe.pointer("/stages/inspect_tags/root").is_some_and(Value::is_null)
                && evidence.get("original_struct_tree_root_absent").and_then(Value::as_bool) == Some(true)
                && evidence.get("new_indirect_root").and_then(Value::as_bool) == Some(true)
                && evidence.get("existing_root_identity_preserved").and_then(Value::as_bool) == Some(false)
                && evidence.get("mark_info_action").and_then(Value::as_str) == Some("marked_true")
                && evidence.get("marked_after").and_then(Value::as_bool) == Some(true),
            "new-root creation did not prove original absence and exact permitted MarkInfo change"
        ),
        Some("create_first_tag_existing_root") => {
            let before = evidence.get("marked_before").context("create_root missing original MarkInfo state")?;
            let after = evidence.get("marked_after").context("create_root missing resulting MarkInfo state")?;
            ensure!(
                probe.pointer("/stages/inspect_tags/root").and_then(Value::as_str) == Some(root)
                    && evidence.get("original_struct_tree_root_absent").and_then(Value::as_bool) == Some(false)
                    && evidence.get("new_indirect_root").and_then(Value::as_bool) == Some(false)
                    && evidence.get("existing_root_identity_preserved").and_then(Value::as_bool) == Some(true)
                    && evidence.get("mark_info_action").and_then(Value::as_str) == Some("preserved")
                    && (before.is_boolean() || before.is_null())
                    && before == after,
                "first-tag creation did not preserve the exact original indirect root and MarkInfo state"
            );
        }
        _ => bail!("unrecognized create_root mode"),
    }
    Ok(())
}

fn verified_creation_without_title(probe: &Value, saved: &BTreeMap<String, PathBuf>) -> bool {
    if !saved.contains_key("create_root") || probe.pointer("/stages/open/bytes_changed_during_open").and_then(Value::as_bool) != Some(false) {
        return false;
    }
    let Some(edits) = probe.get("edits").and_then(Value::as_array) else {
        return false;
    };
    let title_absent = edits.iter().any(|operation| {
        operation.get("operation").and_then(Value::as_str) == Some("title_property")
            && status(operation) == Some("not_applicable")
            && operation.get("reason_code").and_then(Value::as_str) == Some("precondition_absent")
    });
    let creation_verified =
        edits.iter().filter(|operation| operation.get("operation").and_then(Value::as_str) == Some("create_root")).any(|operation| {
            status(operation) == Some("passed")
                && operation.get("observed_edit").and_then(Value::as_bool) == Some(true)
                && ["undo", "redo", "save_reopen", "exact_ownership", "all_page_artwork", "unknown_data_save_reopen"]
                    .iter()
                    .all(|invariant| operation.get(*invariant).and_then(Value::as_str) == Some("passed"))
                && operation.get("rendered_pages_per_snapshot").and_then(Value::as_u64) == probe.pointer("/stages/open/pages").and_then(Value::as_u64)
                && verify_create_root_evidence(probe, operation).is_ok()
        });
    title_absent && creation_verified
}

fn exact_indirect_reference(value: &str) -> bool {
    let fields: Vec<_> = value.split_whitespace().collect();
    let [number, generation, marker] = fields.as_slice() else {
        return false;
    };
    let (Ok(number), Ok(generation)) = (number.parse::<u32>(), generation.parse::<u16>()) else {
        return false;
    };
    number > 0 && *marker == "R" && value == format!("{number} {generation} R")
}
fn verify_figure_stage_refusal(p: &Value, expected: &ExpectedStageRefusal) -> Result<()> {
    ensure!(
        expected.stage == "figures_api_parity" && expected.reason_code == "structure_resolution_incomplete",
        "unsupported expected stage refusal"
    );
    let stage = p.pointer("/stages/figures_api_parity").context("Figure stage missing")?;
    ensure!(
        status(stage) == Some("refused")
            && stage.get("reason_code").and_then(Value::as_str) == Some(expected.reason_code.as_str())
            && stage.get("reason").and_then(Value::as_str) == Some(expected.reason.as_str()),
        "Figure stage did not refuse with exact declared diagnosis"
    );
    let diagnostic = stage.get("diagnostic").context("Figure diagnostic missing")?;
    ensure!(p.pointer("/stages/figures_api_diagnostic") == Some(diagnostic), "Figure diagnostic stage is missing or inconsistent");
    ensure!(
        status(diagnostic) == Some("passed")
            && diagnostic.get("classification").and_then(Value::as_str) == Some("structure_resolution_incomplete")
            && diagnostic.get("strict_error").and_then(Value::as_str) == Some(expected.reason.as_str())
            && diagnostic.get("legacy_empty_fallback_verified").and_then(Value::as_bool) == Some(true)
            && stage.get("legacy_figures").and_then(Value::as_u64) == Some(0),
        "Figure diagnostic classification or legacy fallback not verified"
    );
    let sha = diagnostic.get("post_open_sha256").and_then(Value::as_str).context("Figure diagnostic post-open SHA missing")?;
    ensure!(
        valid_sha256(sha) && p.pointer("/stages/open/post_open_sha256").and_then(Value::as_str) == Some(sha),
        "Figure diagnostic is not bound to exact opened bytes"
    );
    for (summary, field) in [("issues", "inspector_issue_count"), ("edit_blockers", "inspector_edit_blocker_count")] {
        let inspected = p.pointer(&format!("/stages/inspect_tags/{summary}")).context("Figure inspector classification missing")?;
        let total = inspected.get("total").and_then(Value::as_u64).context("Figure inspector count missing")?;
        let items = inspected.get("items").and_then(Value::as_array).context("Figure inspector items missing")?;
        ensure!(
            inspected.get("omitted").and_then(Value::as_u64) == Some(0)
                && u64::try_from(items.len())? == total
                && items.iter().all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                && diagnostic.get(field).and_then(Value::as_u64) == Some(total),
            "Figure inspector counts incomplete or inconsistent"
        );
    }
    let ids = diagnostic.get("tag_tree_known_figure_ids").and_then(Value::as_array).context("known unenumerated Figure IDs missing")?;
    let mut unique = BTreeSet::new();
    for id in ids {
        let id = id.as_str().context("known Figure identity is not text")?;
        ensure!(exact_indirect_reference(id) && unique.insert(id), "known Figure IDs must be unique exact generation-bearing references");
    }
    let count = u64::try_from(ids.len())?;
    ensure!(
        diagnostic.get("tag_tree_known_figure_count").and_then(Value::as_u64) == Some(count)
            && stage.get("tag_tree_figures").and_then(Value::as_u64) == Some(count)
            && p.pointer("/stages/inspect_tags/elements").and_then(Value::as_u64).is_some_and(|n| count <= n),
        "known Figure counts are incomplete or inconsistent"
    );
    let stage_ids = stage.get("known_unenumerated_figure_ids").and_then(Value::as_array).context("Figure parity gap IDs missing")?;
    let stage_unique = stage_ids.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>();
    ensure!(stage_ids.len() == ids.len() && stage_unique == unique, "Figure diagnostic and parity gap identities differ");
    let issue =
        expected.reason.strip_prefix("cannot enumerate figures: ").filter(|s| !s.is_empty()).context("Figure refusal diagnostic prefix missing")?;
    let finding = format!("Structure resolution incomplete: {issue}");
    ensure!(
        diagnostic.get("checker_rule").and_then(Value::as_str) == Some("FiguresAltText")
            && diagnostic.get("checker_finding").and_then(Value::as_str) == Some(finding.as_str())
            && p.pointer("/stages/checker/status").and_then(Value::as_str) == Some("passed"),
        "Figure diagnostic lacks completed matching checker evidence"
    );
    let rules = p.pointer("/stages/checker/rules").and_then(Value::as_array).context("Figure checker rules missing")?;
    let matching: Vec<_> = rules.iter().filter(|r| r.get("rule").and_then(Value::as_str) == Some("FiguresAltText")).collect();
    let [rule] = matching.as_slice() else {
        bail!("Figure checker rule missing or duplicated");
    };
    ensure!(
        matches!(status(rule), Some("manual" | "failed")) && rule.get("status") == diagnostic.get("checker_status"),
        "Figure checker did not report exact uncertainty status"
    );
    let findings = rule.get("findings").and_then(Value::as_array).context("Figure checker findings missing")?;
    ensure!(
        findings
            .iter()
            .filter(|v| v.get("page").is_some_and(Value::is_null) && v.get("message").and_then(Value::as_str) == Some(finding.as_str()))
            .count()
            == 1,
        "Figure checker uncertainty must be one exact page-less finding"
    );
    Ok(())
}

fn assess_probe(p: &Value, f: &CorpusFile, full: bool) -> Assessment {
    let transform = assess_open_transform(f, p);
    let mut a = Assessment {
        independent_inventory: compare_inventory(f, p, &transform),
        open_transform: transform,
        manual_review: manual_review_assessment(f),
        ..Default::default()
    };
    if full && f.upstream_target.expected != Expected::Unknown && f.upstream_target.test.is_none() {
        a.errors.push("--require-full-support rejects an unmapped known upstream PASS/FAIL target".into());
    }
    if full && a.manual_review.get("required").and_then(Value::as_bool) == Some(true) {
        a.errors.push("--require-full-support rejects unperformed or unverified required manual review".into());
    }
    if ![Some("not_transformed"), Some("expected_unsupported")].contains(&status(&a.open_transform)) {
        a.errors.push("source-to-open transformation is unverified or unexpected".into());
    }
    if a.open_transform.get("bytes_changed").and_then(Value::as_bool) == Some(false)
        && a.open_transform.get("post_open_sha256").and_then(Value::as_str) != Some(f.sha256.as_str())
    {
        a.errors.push("unchanged open bytes disagree with source SHA-256".into());
    }
    if full && status(&a.open_transform) == Some("expected_unsupported") {
        a.errors.push("--require-full-support rejects declared unsupported open transformation".into());
    }
    for (metric, observation) in &a.independent_inventory {
        if matches!(status(observation), Some("mismatch" | "probe_missing")) {
            a.errors.push(format!("independent inventory {metric}: {observation}"));
        }
    }
    if p.get("schema_version").and_then(Value::as_u64) != Some(1) {
        a.errors.push("missing or unsupported probe schema_version".into());
    }
    if !matches!(p.get("outcome").and_then(Value::as_str), Some("passed" | "incomplete")) {
        a.errors.push("probe outcome missing, interrupted or error".into());
    }
    if !p.get("active_stage").is_some_and(Value::is_null) {
        a.errors.push("probe active_stage missing or unfinished".into());
    }
    if p.pointer("/stages/input/sha256").and_then(Value::as_str) != Some(f.sha256.as_str()) {
        a.errors.push("probe input SHA-256 differs from selected manifest bytes".into());
    }
    for stage in REQUIRED_STAGES {
        let observed = p.pointer(&format!("/stages/{stage}")).and_then(status).unwrap_or("not_run");
        let name = if *stage == "inspect_tags" { "inspect" } else { stage };
        a.stage_outcomes.insert(name.into(), observed.into());
        let declarations: Vec<_> = f.expected_stage_refusals.iter().filter(|e| e.stage == *stage).collect();
        if let [expected] = declarations.as_slice() {
            match verify_figure_stage_refusal(p, expected) {
                Ok(()) => {
                    a.expected_stage_refusals += 1;
                    *a.expected_stage_refusal_categories.entry(expected.category.clone()).or_default() += 1;
                    a.stage_refusal_expectations.insert((*stage).into(), "observed".into());
                    a.stage_outcomes.insert((*stage).into(), "expected_refusal".into());
                    if full {
                        a.errors.push("--require-full-support rejects expected stage refusals and known-unenumerated Figure coverage".into());
                    }
                }
                Err(error) => {
                    a.stage_refusal_expectations.insert((*stage).into(), "expectation_failed".into());
                    a.stage_outcomes.insert((*stage).into(), "expectation_failed".into());
                    a.errors.push(format!("expected stage refusal {stage} not observed exactly with bound evidence: {error:#}"));
                }
            }
        } else if !declarations.is_empty() {
            a.stage_refusal_expectations.insert((*stage).into(), "duplicate_declaration".into());
            a.stage_outcomes.insert((*stage).into(), "expectation_failed".into());
            a.errors.push(format!("duplicate expected stage refusal {stage}"));
        } else if observed != "passed" {
            a.errors.push(format!("required stage {stage}: {observed}"));
        }
    }
    for expected in &f.expected_stage_refusals {
        if !REQUIRED_STAGES.contains(&expected.stage.as_str()) {
            a.errors.push(format!("expected refusal targets unsupported stage {}", expected.stage));
        }
    }
    let pages = p.pointer("/stages/open/pages").and_then(Value::as_u64);
    if pages.is_none() {
        a.errors.push("opened page count not reported".into());
    }
    for pointer in ["/stages/render/pages_rendered", "/stages/geometry/pages_inspected", "/stages/geometry/whole_page_reverse_lookups"] {
        if pages.is_none() || p.pointer(pointer).and_then(Value::as_u64) != pages {
            a.errors.push(format!("{pointer} did not cover every opened page"));
        }
    }
    if full {
        for metric in ["owned_identities_without_geometry", "geometry_issue_count"] {
            if p.pointer(&format!("/stages/geometry/{metric}")).and_then(Value::as_u64) != Some(0) {
                a.errors.push(format!("--require-full-support: geometry {metric} is missing or nonzero (coverage gap)"));
            }
        }
    }
    let edit_errors_before = a.errors.len();
    let mut seen = BTreeSet::new();
    let mut present = BTreeSet::<String>::new();
    let mut tested = BTreeSet::<String>::new();
    let edits = p.get("edits").and_then(Value::as_array).cloned().unwrap_or_default();
    for edit in &edits {
        let Some(name) = edit.get("operation").and_then(Value::as_str) else {
            a.errors.push("edit missing operation name".into());
            continue;
        };
        if !REQUIRED_EDITS.contains(&name)
            || !seen.insert(name.to_owned())
            || edits.iter().filter(|candidate| candidate.get("operation").and_then(Value::as_str) == Some(name)).count() != 1
        {
            a.errors.push(format!("unknown or duplicate edit {name}"));
            continue;
        }
        if name.starts_with("leaf_move_")
            && let Some(kinds) = edit.pointer("/leaf_move_coverage/present_kinds").and_then(Value::as_array)
        {
            present.extend(kinds.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        match status(edit) {
            Some("passed") => {
                let before = a.errors.len();
                if f.expected_refusals.iter().any(|expected| expected.operation == name) {
                    a.errors.push(format!("expected refusal {name} was unexpectedly accepted"));
                }
                if edit.get("observed_edit").and_then(Value::as_bool) != Some(true) {
                    a.errors.push(format!("{name}: successful edit did not observe a change"));
                }
                for invariant in EDIT_INVARIANTS {
                    if edit.get(*invariant).and_then(Value::as_str) != Some("passed") {
                        a.errors.push(format!("{name}: missing successful {invariant} invariant"));
                    }
                }
                if pages.is_none() || edit.get("rendered_pages_per_snapshot").and_then(Value::as_u64) != pages {
                    a.errors.push(format!("{name}: artwork comparison did not cover all pages"));
                }
                if name == "title_property" {
                    for invariant in ["unknown_data_harmless_edit", "checker_findings_preserved"] {
                        if edit.get(invariant).and_then(Value::as_str) != Some("passed") {
                            a.errors.push(format!("{name}: missing successful {invariant} invariant"));
                        }
                    }
                }
                if name == "metadata_properties" {
                    let changed: BTreeSet<_> =
                        edit.get("changed_properties").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    if edit.get("unknown_data_property_edit").and_then(Value::as_str) != Some("passed")
                        || changed != BTreeSet::from(["alt", "actual_text", "lang", "expansion"])
                    {
                        a.errors.push("metadata_properties: all four properties and unknown data were not verified".into());
                    }
                }
                if name == "artifact"
                    && (edit.get("semantic_artifact_rewrite").and_then(Value::as_str) != Some("passed")
                        || !edit.pointer("/artifact_rewrite_evidence/mcid_sequences_converted").and_then(Value::as_u64).is_some_and(|n| n > 0))
                {
                    a.errors.push("artifact: actual marked-content conversion not verified".into());
                }
                if name == "create_root"
                    && let Err(error) = verify_create_root_evidence(p, edit)
                {
                    a.errors.push(format!("create_root: {error:#}"));
                }
                if name.starts_with("leaf_move_") {
                    if let Some(kind) = edit
                        .pointer("/leaf_move_coverage/selected_kind")
                        .and_then(Value::as_str)
                        .filter(|kind| name.strip_prefix("leaf_move_") == Some(*kind))
                    {
                        if a.errors.len() == before {
                            tested.insert(kind.into());
                        }
                    } else {
                        a.errors.push(format!("{name}: exact selected content kind missing"));
                    }
                }
                if a.errors.len() == before {
                    a.supported_edits += 1;
                }
            }
            Some("not_applicable") => {
                a.not_applicable_edits += 1;
                if edit.get("reason_code").and_then(Value::as_str) != Some("precondition_absent") {
                    a.errors.push(format!("{name}: unmet prerequisite is not legitimate inapplicability"));
                }
            }
            Some("refused") => {
                if edit.get("unchanged_on_failure").and_then(Value::as_bool) != Some(true) {
                    a.errors.push(format!("{name}: refusal did not prove unchanged input/session"));
                }
                if let Some(expected) = matching_expected_refusal(f, name, edit) {
                    if edit.get("unchanged_on_failure").and_then(Value::as_bool) == Some(true)
                        && f.expected_refusals.iter().filter(|e| e.operation == name).count() == 1
                    {
                        a.expected_refusals += 1;
                        *a.expected_refusal_categories.entry(expected.category.clone().unwrap_or_else(|| "unclassified".into())).or_default() += 1;
                    }
                } else {
                    a.unexpected_refusals += 1;
                    a.errors.push(format!(
                        "unclassified refusal: {name}: {}",
                        edit.get("reason_code").and_then(Value::as_str).unwrap_or("missing reason_code")
                    ));
                }
            }
            other => a.errors.push(format!("edit {name}: {}", other.unwrap_or("missing status"))),
        }
    }
    for expected in &f.expected_refusals {
        if f.expected_refusals.iter().filter(|e| e.operation == expected.operation).count() != 1 {
            a.errors.push(format!("duplicate expected refusal declaration {}", expected.operation));
        }
        let actual: Vec<_> = edits.iter().filter(|edit| edit.get("operation").and_then(Value::as_str) == Some(expected.operation.as_str())).collect();
        let outcome = match actual.as_slice() {
            [] => "missing_operation",
            [edit] => match status(edit) {
                Some("passed") => "unexpected_acceptance",
                Some("not_applicable") => "not_applicable",
                Some("error") => "operation_error",
                Some("refused") if edit.get("reason_code").and_then(Value::as_str) != Some(expected.reason_code.as_str()) => "wrong_code",
                Some("refused") if expected.reason.as_deref().is_some_and(|reason| edit.get("reason").and_then(Value::as_str) != Some(reason)) => {
                    "wrong_reason"
                }
                Some("refused") if edit.get("unchanged_on_failure").and_then(Value::as_bool) != Some(true) => "unchanged_unverified",
                Some("refused") => "observed",
                _ => "invalid_status",
            },
            _ => "duplicate_operation",
        };
        a.refusal_expectations.insert(expected.operation.clone(), outcome.into());
        if outcome != "observed" && outcome != "unexpected_acceptance" {
            a.errors.push(format!(
                "expected refusal {} was not exercised exactly once with exact reason and unchanged state ({outcome})",
                expected.operation
            ));
        }
    }
    for required in REQUIRED_EDITS {
        if !seen.contains(*required) {
            a.errors.push(format!("missing required edit operation {required}"));
        }
    }
    if full {
        for kind in present.difference(&tested) {
            a.errors.push(format!("--require-full-support: present leaf kind {kind} not exercised"));
        }
        if a.expected_refusals > 0 {
            a.errors.push("--require-full-support rejects expected refusals".into());
        }
    }
    if a.supported_edits == 0 && a.expected_refusals == 0 && a.unexpected_refusals == 0 {
        a.errors.push("no supported edit or explicitly classified refusal was exercised".into());
    }
    let bad_edits = a.errors.len() > edit_errors_before;
    a.stage_outcomes.insert(
        "edits".into(),
        if bad_edits {
            "error"
        } else if a.expected_refusals > 0 {
            "expected_refusal"
        } else if a.supported_edits > 0 {
            "passed"
        } else {
            "not_exercised"
        }
        .into(),
    );
    a.stage_outcomes.insert(
        "roundtrip".into(),
        if bad_edits {
            "error"
        } else if a.supported_edits == 0 {
            "not_exercised"
        } else if a.expected_refusals + a.unexpected_refusals > 0 {
            "partial"
        } else {
            "passed"
        }
        .into(),
    );
    a.stage_outcomes.insert(
        "refusal".into(),
        if a.refusal_expectations.values().any(|v| v != "observed") {
            "expectation_failed"
        } else if a.unexpected_refusals > 0 {
            "unexpected"
        } else if a.expected_refusals > 0 {
            "expected"
        } else {
            "none"
        }
        .into(),
    );
    if a.open_transform.get("bytes_changed").and_then(Value::as_bool) == Some(true) {
        a.post_open_edits = a.supported_edits;
        a.supported_edits = 0;
        a.stage_outcomes.insert("roundtrip".into(), if a.post_open_edits > 0 { "post_open_only" } else { "not_exercised" }.into());
        if a.post_open_edits > 0 {
            a.errors.push("tag edits proceeded after source-transforming open; post-open edits are not source-preserving support".into());
        }
    }
    a
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Rule {
    specification: String,
    clause: String,
    test: String,
    status: String,
    passed_checks: u64,
    failed_checks: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OracleObservation {
    profile: Profile,
    profile_name: String,
    machine_profile_compliant: bool,
    target_outcome: String,
    upstream_target_verification: String,
    rules: Vec<Rule>,
    report_file: String,
}
fn expected_target_is_inapplicable(f: &CorpusFile, rules: &[Rule], outcome: &str) -> bool {
    let Some(expected) = &f.expected_target_inapplicability else {
        return false;
    };
    if outcome != "not_evaluated"
        || f.upstream_target.expected != Expected::Pass
        || expected.get("clause").and_then(Value::as_str) != f.upstream_target.clause.as_deref()
        || string_id(expected.get("test")) != f.upstream_target.test
        || expected.get("specification") != f.upstream_target.metadata.get("specification")
    {
        return false;
    }
    let matching: Vec<_> = rules
        .iter()
        .filter(|r| {
            expected.get("specification").and_then(Value::as_str) == Some(r.specification.as_str())
                && expected.get("clause").and_then(Value::as_str) == Some(r.clause.as_str())
                && string_id(expected.get("test")).as_deref() == Some(r.test.as_str())
        })
        .collect();
    matches!(matching.as_slice(), [r] if r.status == "passed" && r.passed_checks == 0 && r.failed_checks == 0)
}
fn parse_oracle(value: &Value, f: &CorpusFile, report_file: &Path) -> Result<OracleObservation> {
    let jobs = value.pointer("/report/jobs").and_then(Value::as_array).context("veraPDF jobs missing")?;
    ensure!(jobs.len() == 1, "veraPDF must report exactly one job");
    let results = jobs.first().and_then(|v| v.get("validationResult")).and_then(Value::as_array).context("veraPDF validationResult missing")?;
    ensure!(results.len() == 1, "veraPDF must run exactly one profile");
    let result = results.first().context("veraPDF result missing")?;
    ensure!(result.get("jobEndStatus").and_then(Value::as_str) == Some("normal"), "veraPDF did not end normally");
    ensure!(result.get("profileName").and_then(Value::as_str) == Some(f.profile.name()), "veraPDF used wrong explicit profile");
    let compliant = result.get("compliant").and_then(Value::as_bool).context("veraPDF compliant flag missing")?;
    let details = result.get("details").context("veraPDF details missing")?;
    let summaries = details.get("ruleSummaries").and_then(Value::as_array).context("veraPDF ruleSummaries missing; --success is required")?;
    ensure!(!summaries.is_empty(), "veraPDF contains no rules");
    let mut rules = Vec::new();
    let mut identities = BTreeSet::new();
    let mut passed = 0u64;
    let mut failed = 0u64;
    for value in summaries {
        let field = |key| value.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()).with_context(|| format!("veraPDF rule {key} missing"));
        let rule = Rule {
            specification: field("specification")?.into(),
            clause: field("clause")?.into(),
            test: string_id(value.get("testNumber")).context("veraPDF testNumber missing")?,
            status: field("status")?.into(),
            passed_checks: value.get("passedChecks").and_then(Value::as_u64).context("veraPDF passedChecks missing")?,
            failed_checks: value.get("failedChecks").and_then(Value::as_u64).context("veraPDF failedChecks missing")?,
        };
        ensure!(identities.insert((rule.specification.clone(), rule.clause.clone(), rule.test.clone())), "duplicate veraPDF rule identity");
        match rule.status.as_str() {
            "passed" => {
                ensure!(rule.failed_checks == 0, "passed rule contains failed checks");
                passed += 1;
            }
            "failed" => {
                ensure!(rule.failed_checks > 0, "failed rule contains no failed checks");
                failed += 1;
            }
            _ => bail!("unknown veraPDF rule status"),
        }
        rules.push(rule);
    }
    ensure!(
        details.get("passedRules").and_then(Value::as_u64) == Some(passed) && details.get("failedRules").and_then(Value::as_u64) == Some(failed),
        "incomplete veraPDF rule vector"
    );
    ensure!(compliant == (failed == 0), "veraPDF overall result contradicts rule vector");
    rules.sort();
    let target = &f.upstream_target;
    let outcome = if let (Some(clause), Some(test)) = (&target.clause, &target.test) {
        let specification = target.metadata.get("specification").and_then(Value::as_str);
        let matching: Vec<_> =
            rules.iter().filter(|r| r.clause == *clause && r.test == *test && specification.is_none_or(|s| r.specification == s)).collect();
        match matching.as_slice() {
            [] => "not_found",
            [rule] if rule.failed_checks > 0 => "fail",
            [rule] if rule.passed_checks > 0 => "pass",
            [_] => "not_evaluated",
            _ => "ambiguous",
        }
    } else {
        "not_specified"
    };
    let verification = if target.expected == Expected::Unknown
        && f.overall_conformance == Conformance::ReferencePositive
        && target.clause.is_none()
        && target.test.is_none()
    {
        "not_applicable_reference_profile"
    } else if target.expected == Expected::Unknown {
        "incomplete_unknown_expectation"
    } else if target.test.is_none() {
        "incomplete_unmapped_or_unevaluated"
    } else if expected_target_is_inapplicable(f, &rules, outcome) {
        "expected_incomplete"
    } else if outcome == target.expected.name() {
        "verified"
    } else {
        "mismatch"
    };
    Ok(OracleObservation {
        profile: f.profile,
        profile_name: f.profile.name().into(),
        machine_profile_compliant: compliant,
        target_outcome: outcome.into(),
        upstream_target_verification: verification.into(),
        rules,
        report_file: report_file.to_string_lossy().into_owned(),
    })
}
fn verify_oracle_support(observed: &OracleObservation, f: &CorpusFile, full: bool) -> Result<()> {
    ensure!(
        f.overall_conformance != Conformance::ReferencePositive || observed.machine_profile_compliant,
        "reference-positive source did not pass its machine profile"
    );
    if f.upstream_target.expected != Expected::Unknown {
        if f.upstream_target.test.is_none() {
            ensure!(!full, "--require-full-support rejects unmapped known target");
        } else if observed.upstream_target_verification == "expected_incomplete" {
            ensure!(!full, "--require-full-support rejects expected-incomplete target applicability");
        } else {
            ensure!(
                observed.upstream_target_verification == "verified",
                "upstream target expected {}, observed {} (not overall conformance)",
                f.upstream_target.expected.name(),
                observed.target_outcome
            );
        }
    }
    Ok(())
}
fn run_oracle(executable: &Path, input: &Path, f: &CorpusFile, prefix: &Path, timeout: Duration) -> Result<OracleObservation> {
    let mut command = Command::new(executable);
    command.args(["--format", "json", "--flavour", f.profile.flavour(), "--success", "--maxfailuresdisplayed", "1"]).arg(input);
    let process = execute(&mut command, timeout, MAX_ORACLE_BYTES, None)?;
    write_json(&prefix.with_extension("process.json"), &process)?;
    ensure!(
        !process.timed_out
            && !process.output_limit_exceeded
            && process.stdout.read_error.is_none()
            && process.stderr.read_error.is_none()
            && matches!(process.exit_code, Some(0 | 1)),
        "veraPDF process failed, timed out, or exceeded output limits"
    );
    let value: Value = serde_json::from_str(&process.stdout.text).context("invalid veraPDF JSON")?;
    let path = prefix.with_extension("json");
    write_json(&path, &value)?;
    parse_oracle(&value, f, &path)
}
fn oracle_rule_differences(before: &OracleObservation, after: &OracleObservation) -> Value {
    let key = |rule: &Rule| (rule.specification.clone(), rule.clause.clone(), rule.test.clone());
    let b: BTreeMap<_, _> = before.rules.iter().map(|r| (key(r), r)).collect();
    let a: BTreeMap<_, _> = after.rules.iter().map(|r| (key(r), r)).collect();
    let added: Vec<_> = a.iter().filter(|(key, _)| !b.contains_key(*key)).map(|(_, rule)| *rule).collect();
    let removed: Vec<_> = b.iter().filter(|(key, _)| !a.contains_key(*key)).map(|(_, rule)| *rule).collect();
    let changed: Vec<_> =
        b.iter().filter_map(|(key, rule)| a.get(key).filter(|new| *new != rule).map(|new| json!({"before":rule,"after":new}))).collect();
    json!({"before_machine_profile_compliant":before.machine_profile_compliant,"after_machine_profile_compliant":after.machine_profile_compliant,"added":added,"removed":removed,"changed":changed})
}
fn collect_jars(directory: &Path, depth: usize, output: &mut BTreeMap<String, String>, root: &Path) -> Result<()> {
    if depth == 0 {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_jars(&path, depth - 1, output, root)?;
        } else if kind.is_file() && path.extension().is_some_and(|v| v == "jar") {
            ensure!(output.len() < 100, "too many installed oracle JARs");
            output.insert(path.strip_prefix(root)?.to_string_lossy().into_owned(), hash_file(&path)?);
        }
    }
    Ok(())
}
fn oracle_identity(executable: &Path) -> Result<Value> {
    let parent = executable.parent().context("oracle path lacks parent")?;
    let mut jars = BTreeMap::new();
    collect_jars(parent, 3, &mut jars, parent)?;
    Ok(json!({"executable_sha256":hash_file(executable)?,"installed_jar_sha256":jars}))
}
fn verify_profile_digest(expected: &str, bytes: &[u8]) -> Result<String> {
    ensure!(valid_sha256(expected) && !bytes.is_empty(), "missing or invalid pinned profile SHA/bytes");
    let observed = digest(bytes);
    ensure!(observed == expected, "installed profile bytes do not match pinned SHA-256");
    Ok(observed)
}
fn verify_resume_identity(previous: &Value, current: &Value) -> Result<()> {
    for key in ["runner_source_sha256", "runner_executable_sha256", "manifest_sha256", "probe_sha256"] {
        ensure!(current.get(key).and_then(Value::as_str).is_some_and(valid_sha256), "resume identity lacks valid {key}");
    }
    ensure!(previous == current, "resume contract differs; use a fresh output directory");
    Ok(())
}
fn verify_installed_profiles(executable: &Path, manifest: &Manifest, selection: &BTreeSet<String>, timeout: Duration) -> Result<Value> {
    let root = executable.parent().context("oracle path lacks parent")?;
    let mut jars = BTreeMap::new();
    collect_jars(root, 3, &mut jars, root)?;
    let candidates: Vec<_> = jars
        .keys()
        .filter(|path| Path::new(path).file_name().and_then(|v| v.to_str()).is_some_and(|name| name.starts_with("cli-") && name.ends_with(".jar")))
        .collect();
    ensure!(candidates.len() == 1, "oracle requires exactly one installed CLI JAR; missing or ambiguous installation");
    let jar = root.join(candidates.first().context("oracle CLI JAR missing")?);
    let selected: BTreeSet<_> =
        manifest.files.iter().filter(|f| selection.is_empty() || selection.contains(&f.source)).map(|f| f.profile.flavour()).collect();
    let mut actual = BTreeMap::new();
    for flavour in selected {
        let declared = manifest
            .metadata
            .get("oracle")
            .and_then(|v| v.get("profiles"))
            .and_then(|v| v.get(flavour))
            .context("manifest profile provenance missing")?;
        let entry = match flavour {
            "ua1" => "org/verapdf/pdfa/validation/PDFUA-1.xml",
            "ua2" => "org/verapdf/pdfa/validation/PDFUA-2-ISO32005.xml",
            _ => bail!("unknown oracle flavour"),
        };
        ensure!(declared.get("jar_entry").and_then(Value::as_str) == Some(entry), "manifest selected profile entry differs from explicit flavour");
        let expected = declared.get("sha256").and_then(Value::as_str).filter(|s| valid_sha256(s)).context("manifest profile SHA-256 missing")?;
        let process = execute(Command::new("unzip").arg("-p").arg(&jar).arg(entry), timeout, 4 * 1024 * 1024, None)
            .context("unzip is required to independently pin-check installed oracle profiles")?;
        ensure!(process.success && !process.stdout.text.is_empty(), "installed oracle profile is missing, ambiguous or unreadable");
        let observed =
            verify_profile_digest(expected, process.stdout.text.as_bytes()).with_context(|| format!("installed {flavour} profile mismatch"))?;
        actual.insert(flavour, json!({"jar":jar,"jar_entry":entry,"sha256":observed}));
    }
    Ok(json!(actual))
}
fn oracle_provenance(executable: &Path, manifest: &Manifest, selection: &BTreeSet<String>, timeout: Duration) -> Result<Value> {
    let process = execute(Command::new(executable).arg("--version"), timeout, MAX_LOG_BYTES, None)?;
    ensure!(process.success && !process.stdout.text.trim().is_empty(), "cannot determine veraPDF version");
    let declared = manifest.metadata.get("oracle").cloned().unwrap_or(Value::Null);
    if let Some(version) = declared.get("version").and_then(Value::as_str) {
        ensure!(process.stdout.text.contains(&format!("veraPDF {version}")), "installed oracle version does not match manifest provenance");
    }
    let mut result = oracle_identity(executable)?;
    result["verified_profiles"] = verify_installed_profiles(executable, manifest, selection, timeout)?;
    result["executable"] = json!(executable);
    result["version"] = json!(process.stdout.text);
    result["declared_provenance"] = declared;
    result["command"] = json!(["--format", "json", "--flavour", "<per-file ua1|ua2>", "--success", "--maxfailuresdisplayed", "1"]);
    result["note"] = json!(
        "Declared archive provenance is separate; executable/JAR hashes identify the actual oracle. Machine profiles do not establish human accessibility."
    );
    Ok(result)
}
fn verify_artifact(path: &Value, expected: &Value, directory: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(path.as_str().context("artifact path missing")?).canonicalize()?;
    ensure!(path.starts_with(directory.canonicalize()?) && path.is_file(), "artifact escaped requested directory");
    let sha = expected.as_str().filter(|s| valid_sha256(s)).context("saved artifact SHA-256 missing/invalid")?;
    ensure!(digest(&read_bounded(&path, MAX_INPUT_BYTES)?) == sha, "saved artifact SHA-256 mismatch");
    Ok(path)
}
fn verify_artifacts(p: &Value, directory: &Path, original_sha: &str) -> Result<BTreeMap<String, PathBuf>> {
    let mut saved = BTreeMap::new();
    for edit in p.get("edits").and_then(Value::as_array).into_iter().flatten().filter(|edit| status(edit) == Some("passed")) {
        let name = edit.get("operation").and_then(Value::as_str).context("successful edit has no name")?;
        ensure!(REQUIRED_EDITS.contains(&name), "unknown saved operation");
        ensure!(
            edit.get("saved_sha256").and_then(Value::as_str) != Some(original_sha),
            "successful edit artifact is byte-identical to original input"
        );
        let path = verify_artifact(edit.get("artifact_path").unwrap_or(&Value::Null), edit.get("saved_sha256").unwrap_or(&Value::Null), directory)?;
        ensure!(saved.insert(name.into(), path).is_none(), "duplicate saved edit artifact");
    }
    Ok(saved)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileResult {
    index: usize,
    file: CorpusFile,
    source_revision: String,
    outcome: String,
    errors: Vec<String>,
    assessment: Assessment,
    probe: Option<Value>,
    process: Option<ProcessResult>,
    oracle: Value,
    source_sha256_after: Option<String>,
    elapsed_ms: u64,
}
fn interrupted_result(index: usize, f: &CorpusFile, revision: &str, reason: &str) -> FileResult {
    FileResult {
        index,
        file: f.clone(),
        source_revision: revision.into(),
        outcome: "failed".into(),
        errors: vec![reason.into()],
        assessment: assess_probe(&Value::Null, f, false),
        probe: None,
        process: None,
        oracle: json!({"status":"not_run","conformance":"incomplete","reason":reason}),
        source_sha256_after: None,
        elapsed_ms: 0,
    }
}
fn compatible_outcome(a: &Assessment, oracle: &Value) -> &'static str {
    if status(&a.open_transform) == Some("expected_unsupported") {
        "expected_unsupported"
    } else if oracle.pointer("/before/upstream_target_verification").and_then(Value::as_str) == Some("expected_incomplete") {
        "expected_incomplete"
    } else if a.expected_refusals > 0 || a.expected_stage_refusals > 0 {
        "expected_refusal"
    } else {
        "passed"
    }
}
fn verify_post_open(p: &Value, directory: &Path, source_sha: &str) -> Result<Option<PathBuf>> {
    if p.pointer("/stages/open/bytes_changed_during_open").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let sha = p.pointer("/stages/open/post_open_sha256").context("post-open SHA-256 missing")?;
    ensure!(sha.as_str() != Some(source_sha), "changed-open artifact equals source SHA-256");
    Ok(Some(verify_artifact(p.pointer("/stages/open/post_open_artifact_path").unwrap_or(&Value::Null), sha, directory)?))
}
fn apply_oracle(
    result: &mut FileResult,
    p: &Value,
    saved: &BTreeMap<String, PathBuf>,
    post_open: Option<&Path>,
    input: &Path,
    directory: &Path,
    options: &Options,
) {
    let Some(executable) = &options.verapdf else {
        if options.require_full_support
            && (result.file.upstream_target.expected != Expected::Unknown || result.file.overall_conformance == Conformance::ReferencePositive)
        {
            result.errors.push("--require-full-support requires independent --verapdf evidence for known targets/reference positives".into());
        }
        return;
    };
    let f = &result.file;
    let before = match run_oracle(executable, input, f, &directory.join("oracle-before"), options.timeout) {
        Ok(observation) => observation,
        Err(error) => {
            result.errors.push(format!("oracle before: {error:#}"));
            result.oracle = json!({"status":"error","conformance":"incomplete","error":format!("{error:#}")});
            return;
        }
    };
    if let Err(error) = verify_oracle_support(&before, f, options.require_full_support) {
        result.errors.push(format!("oracle before: {error:#}"));
    }
    result.oracle = json!({"status":"incomplete","before":before,"after":{"status":"not_run","reason":"title_property edit did not produce a verified artifact"},"conformance":"incomplete"});
    if let Some(edited) = saved.get("title_property") {
        match run_oracle(executable, edited, f, &directory.join("oracle-after-title"), options.timeout) {
            Ok(after) => {
                if let Err(error) = verify_oracle_support(&after, f, options.require_full_support) {
                    result.errors.push(format!("oracle after: {error:#}"));
                }
                let preserved = before.rules == after.rules && before.machine_profile_compliant == after.machine_profile_compliant;
                if !preserved {
                    result.errors.push("harmless metadata edit changed independent validator rule outcomes/check counts".into());
                }
                result.oracle = json!({"status":if preserved {"completed"} else {"regression"},"before":before,"after":after,"rule_outcomes_preserved":preserved,"conformance":"machine_profile_only","note":"A machine validator cannot establish human accessibility, WCAG or complete PDF/UA conformance."});
            }
            Err(error) => {
                result.errors.push(format!("oracle after: {error:#}"));
                result.oracle = json!({"status":"error","before":before,"after_error":format!("{error:#}"),"conformance":"incomplete"});
            }
        }
    } else if verified_creation_without_title(p, saved) {
        result.oracle["status"] = json!("source_only");
        result.oracle["after"] = json!({"status":"not_exercised","reason":"Original tag tree was empty; verified create_root exercised tag creation, and no existing title was available for a harmless title edit.","verified_creation_artifact":saved.get("create_root"),"source_intent_unchanged":true});
    } else {
        let expected_title = p.get("edits").and_then(Value::as_array).into_iter().flatten().any(|edit| {
            edit.get("operation").and_then(Value::as_str) == Some("title_property")
                && status(edit) == Some("refused")
                && edit.get("unchanged_on_failure").and_then(Value::as_bool) == Some(true)
                && matching_expected_refusal(f, "title_property", edit).is_some()
        });
        if !expected_title {
            result.errors.push("oracle could not validate a harmless-edit artifact".into());
        }
    }
    result.oracle["source_to_open_preservation"] = json!(match result.assessment.open_transform.get("bytes_changed").and_then(Value::as_bool) {
        Some(true) => "transformed_not_source_preserving",
        Some(false) => "byte_identical",
        None => "unverified",
    });
    if let Some(path) = post_open {
        match run_oracle(executable, path, f, &directory.join("oracle-post-open"), options.timeout) {
            Ok(mut after) => {
                after.upstream_target_verification = "not_applied_to_transformed_document".into();
                let differences = oracle_rule_differences(&before, &after);
                result.oracle["source_to_post_open"] = json!({"status":"completed","classification":result.assessment.open_transform.get("status"),"source_to_open_preservation":"transformed_not_source_preserving","post_open":after,"differences":differences});
            }
            Err(error) => {
                result.errors.push(format!("oracle post-open: {error:#}"));
                result.oracle["source_to_post_open"] = json!({"status":"error","error":format!("{error:#}")});
            }
        }
    } else if result.assessment.open_transform.get("bytes_changed").and_then(Value::as_bool) == Some(true) {
        result.oracle["source_to_post_open"] = json!({"status":"not_run","reason":"no verified post-open artifact"});
    }
}
fn attempt_directory(root: &Path, index: usize) -> Result<PathBuf> {
    let directory = root.join(format!("{index:05}"));
    fs::create_dir_all(&directory)?;
    for attempt in 1..=1000 {
        let path = directory.join(format!("attempt-{attempt:05}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    bail!("too many retained attempts for one corpus file")
}
fn run_file(index: usize, f: &CorpusFile, revision: &str, options: &Options, probe: &FrozenProbe) -> FileResult {
    let started = Instant::now();
    let mut result = interrupted_result(index, f, revision, "probe not run");
    result.errors.clear();
    let mut source = None;
    let mut snapshot = None;
    let work = (|| -> Result<()> {
        let directory = attempt_directory(&options.output, index)?;
        let artifacts = directory.join("artifacts");
        fs::create_dir(&artifacts)?;
        let (path, bytes) = verified_input(f, &options.roots)?;
        source = Some(path);
        let input = directory.join("input.pdf");
        let mut copy = OpenOptions::new().create_new(true).write(true).open(&input)?;
        copy.write_all(&bytes)?;
        copy.sync_all()?;
        drop(copy);
        let mut permissions = fs::metadata(&input)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&input, permissions)?;
        snapshot = Some(input.clone());
        ensure!(hash_file(&probe.executable)? == probe.sha256, "frozen probe executable changed");
        let output = directory.join("probe.json");
        let mut command = Command::new(&probe.executable);
        command.args(["--input"]).arg(&input).arg("--output").arg(&output).arg("--artifacts-dir").arg(&artifacts).current_dir(&directory);
        let process = execute(&mut command, options.timeout, MAX_LOG_BYTES, Some((&output, MAX_PROBE_BYTES)))?;
        if !process.success {
            result.errors.push(format!(
                "probe process failed: exit={:?}, timeout={}, output_limit={}",
                process.exit_code, process.timed_out, process.output_limit_exceeded
            ));
        }
        result.process = Some(process);
        ensure!(hash_file(&probe.executable)? == probe.sha256, "frozen probe executable changed during child");
        let p: Value = serde_json::from_slice(&read_bounded(&output, MAX_PROBE_BYTES)?).context("probe JSON missing or invalid")?;
        result.assessment = assess_probe(&p, f, options.require_full_support);
        let saved = match verify_artifacts(&p, &artifacts, &f.sha256) {
            Ok(saved) => saved,
            Err(error) => {
                result.errors.push(format!("artifact verification: {error:#}"));
                result.assessment.supported_edits = 0;
                result.assessment.stage_outcomes.insert("edits".into(), "error".into());
                result.assessment.stage_outcomes.insert("roundtrip".into(), "error".into());
                BTreeMap::new()
            }
        };
        let post_open = match verify_post_open(&p, &artifacts, &f.sha256) {
            Ok(path) => path,
            Err(error) => {
                result.errors.push(format!("post-open artifact verification: {error:#}"));
                None
            }
        };
        apply_oracle(&mut result, &p, &saved, post_open.as_deref(), &input, &directory, options);
        result.probe = Some(p);
        Ok(())
    })();
    if let Err(error) = work {
        result.errors.push(format!("{error:#}"));
    }
    if let Some(source) = source {
        match hash_file(&source) {
            Ok(hash) => {
                if hash != f.sha256 {
                    result.errors.push("source bytes changed during run".into());
                }
                result.source_sha256_after = Some(hash);
            }
            Err(error) => result.errors.push(format!("source reverification failed: {error:#}")),
        }
    }
    if let Some(snapshot) = snapshot {
        match hash_file(&snapshot) {
            Ok(hash) if hash == f.sha256 => {}
            Ok(_) => result.errors.push("probe mutated its read-only input snapshot".into()),
            Err(error) => result.errors.push(format!("input snapshot reverification failed: {error:#}")),
        }
    }
    result.outcome = if result.errors.is_empty() && result.assessment.errors.is_empty() {
        compatible_outcome(&result.assessment, &result.oracle)
    } else {
        "failed"
    }
    .into();
    result.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result
}
fn persist_result(output: &Path, result: &FileResult) -> Result<()> {
    let directory = output.join(format!("{:05}", result.index));
    fs::create_dir_all(&directory)?;
    let path = directory.join("result.json");
    write_json(&path, result)?;
    let hash = hash_file(&path)?;
    write_json(&directory.join("checkpoint.json"), &json!({"schema_version":1,"result_sha256":hash}))
}
fn resumed_result(index: usize, f: &CorpusFile, revision: &str, options: &Options) -> Result<Option<FileResult>> {
    let directory = options.output.join(format!("{index:05}"));
    let checkpoint = directory.join("checkpoint.json");
    if !checkpoint.exists() {
        return Ok(None);
    }
    let checkpoint: Value = serde_json::from_slice(&read_bounded(&checkpoint, 4096)?)?;
    ensure!(checkpoint.get("schema_version").and_then(Value::as_u64) == Some(1), "unknown checkpoint schema");
    let bytes = read_bounded(&directory.join("result.json"), MAX_PROBE_BYTES.saturating_add(4 * MAX_ORACLE_BYTES as u64))?;
    ensure!(checkpoint.get("result_sha256").and_then(Value::as_str) == Some(digest(&bytes).as_str()), "durable per-file checkpoint hash mismatch");
    let result: FileResult = serde_json::from_slice(&bytes)?;
    ensure!(
        result.index == index && result.source_revision == revision && serde_json::to_value(&result.file)? == serde_json::to_value(f)?,
        "checkpoint corpus identity mismatch"
    );
    ensure!(
        ["passed", "failed", "expected_refusal", "expected_incomplete", "expected_unsupported"].contains(&result.outcome.as_str()),
        "checkpoint is not terminal"
    );
    verified_input(f, &options.roots)?;
    if let Some(p) = &result.probe {
        verify_post_open(p, &directory, &f.sha256).context("resume changed-open artifact verification")?;
        for edit in p.get("edits").and_then(Value::as_array).into_iter().flatten().filter(|e| status(e) == Some("passed")) {
            let path = edit.get("artifact_path").and_then(Value::as_str).context("checkpoint artifact path missing")?;
            verify_artifact(&json!(path), edit.get("saved_sha256").unwrap_or(&Value::Null), &directory)?;
        }
    }
    Ok(Some(result))
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct RunSummary {
    completed_files: usize,
    durable_files: usize,
    failed_files: usize,
    outcomes: BTreeMap<String, usize>,
    stage_outcomes: BTreeMap<String, BTreeMap<String, usize>>,
    source_counts: BTreeMap<String, usize>,
    supported_edits: usize,
    expected_refusals: usize,
    expected_refusal_categories: BTreeMap<String, usize>,
    refusal_expectation_outcomes: BTreeMap<String, usize>,
    expected_stage_refusals: usize,
    expected_stage_refusal_categories: BTreeMap<String, usize>,
    stage_refusal_expectation_outcomes: BTreeMap<String, usize>,
    unexpected_refusals: usize,
    not_applicable_edits: usize,
    post_open_edits: usize,
    open_transform_outcomes: BTreeMap<String, usize>,
    manual_review_outcomes: BTreeMap<String, usize>,
    oracle_statuses: BTreeMap<String, usize>,
    upstream_target_verification: BTreeMap<String, usize>,
    independent_inventory: BTreeMap<String, BTreeMap<String, usize>>,
}
fn count(map: &mut BTreeMap<String, usize>, value: &str) {
    *map.entry(value.into()).or_default() += 1;
}
impl RunSummary {
    fn record(&mut self, result: &FileResult, durable: bool) {
        self.completed_files += 1;
        self.durable_files += usize::from(durable);
        self.failed_files += usize::from(result.outcome == "failed");
        count(&mut self.outcomes, &result.outcome);
        count(&mut self.source_counts, &result.file.source);
        let a = &result.assessment;
        self.supported_edits += a.supported_edits;
        self.expected_refusals += a.expected_refusals;
        self.expected_stage_refusals += a.expected_stage_refusals;
        for (category, n) in &a.expected_stage_refusal_categories {
            *self.expected_stage_refusal_categories.entry(category.clone()).or_default() += n;
        }
        for outcome in a.stage_refusal_expectations.values() {
            count(&mut self.stage_refusal_expectation_outcomes, outcome);
        }
        self.unexpected_refusals += a.unexpected_refusals;
        self.not_applicable_edits += a.not_applicable_edits;
        self.post_open_edits += a.post_open_edits;
        for (category, n) in &a.expected_refusal_categories {
            *self.expected_refusal_categories.entry(category.clone()).or_default() += n;
        }
        for outcome in a.refusal_expectations.values() {
            count(&mut self.refusal_expectation_outcomes, outcome);
        }
        for (stage, outcome) in &a.stage_outcomes {
            count(self.stage_outcomes.entry(stage.clone()).or_default(), outcome);
        }
        for (metric, comparison) in &a.independent_inventory {
            count(self.independent_inventory.entry(metric.clone()).or_default(), status(comparison).unwrap_or("unreported"));
        }
        count(&mut self.open_transform_outcomes, status(&a.open_transform).unwrap_or("unverified"));
        count(&mut self.manual_review_outcomes, status(&a.manual_review).unwrap_or("unreported"));
        count(&mut self.oracle_statuses, status(&result.oracle).unwrap_or("not_run"));
        count(
            &mut self.upstream_target_verification,
            result.oracle.pointer("/before/upstream_target_verification").and_then(Value::as_str).unwrap_or("incomplete_not_run"),
        );
    }
}
fn write_progress(options: &Options, summary: &RunSummary, selected: usize, state: &str, errors: &[String]) -> Result<()> {
    write_json(
        &options.output.join("progress.json"),
        &json!({"schema_version":1,"status":state,"selected":selected,"completed":summary.completed_files,"durable":summary.durable_files,"failed":summary.failed_files,"outcomes":summary.outcomes,"infrastructure_errors":errors}),
    )
}
fn parallel_map<T: Send, W: Fn(usize) -> T + Sync, C: FnMut(usize, Option<T>)>(count: usize, jobs: usize, work: W, mut consume: C) -> Vec<String> {
    let next = AtomicUsize::new(0);
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(jobs.max(1));
        let mut handles = Vec::new();
        for worker in 0..jobs {
            let sender = sender.clone();
            let next = &next;
            let work = &work;
            match thread::Builder::new().name(format!("a11y-corpus-{worker}")).spawn_scoped(scope, move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(index))).ok();
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                }
            }) {
                Ok(handle) => handles.push(handle),
                Err(error) => errors.push(format!("could not start worker {worker}: {error}")),
            }
        }
        drop(sender);
        for (index, result) in receiver {
            if index >= count || !seen.insert(index) {
                errors.push(format!("worker returned invalid/duplicate index {index}"));
                continue;
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| consume(index, result))).is_err() {
                errors.push(format!("result consumer panicked at {index}"));
            }
        }
        for handle in handles {
            if handle.join().is_err() {
                errors.push("worker terminated outside per-file guard".into());
            }
        }
    });
    for index in 0..count {
        if !seen.contains(&index) {
            errors.push(format!("worker never returned selected index {index}"));
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| consume(index, None))).is_err() {
                errors.push(format!("missing-result consumer panicked at {index}"));
            }
        }
    }
    errors
}
fn write_aggregate(output: &Path, metadata: &Value, selected: usize) -> Result<()> {
    let mut header = metadata.as_object().context("aggregate metadata must be object")?.clone();
    header.remove("files");
    let mut file = File::create(output.join("report.json.pending"))?;
    file.write_all(b"{")?;
    for (key, value) in header {
        serde_json::to_writer(&mut file, &key)?;
        file.write_all(b":")?;
        serde_json::to_writer(&mut file, &value)?;
        file.write_all(b",\n")?;
    }
    file.write_all(b"\"files\":[\n")?;
    for index in 1..=selected {
        if index > 1 {
            file.write_all(b",\n")?;
        }
        let path = output.join(format!("{index:05}/result.json"));
        let mut input = File::open(path)?;
        std::io::copy(&mut input, &mut file)?;
    }
    file.write_all(b"\n]}\n")?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    if output.join("report.json").exists() {
        fs::remove_file(output.join("report.json"))?;
    }
    fs::rename(output.join("report.json.pending"), output.join("report.json"))?;
    Ok(())
}
pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|v| v == "--help" || v == "-h") {
        println!(
            "cargo xtask a11y-corpus --probe PATH --corpus-root ID=DIR [repeat] --output NEW_DIR [--manifest PATH] [--source ID] [--jobs 1..4] [--timeout SECONDS] [--verapdf PATH] [--require-full-support] [--resume]\nFresh output is the default. Explicit resume requires identical runner/probe/manifest/options/oracle hashes and rechecks every source and durable result."
        );
        return Ok(());
    }
    let started = Instant::now();
    let mut options = options(args)?;
    let manifest_bytes = read_bounded(&options.manifest, MAX_MANIFEST_BYTES)?;
    let manifest_sha = digest(&manifest_bytes);
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    validate_manifest(&manifest, &options.sources)?;
    ensure!(options.roots.keys().all(|id| manifest.sources.iter().any(|s| &s.id == id)), "unknown corpus-root source ID");
    options.output = planned_output(&options.output)?;
    for root in options.roots.values() {
        let root = if root.exists() { root.canonicalize()? } else { planned_output(root)? };
        ensure!(!options.output.starts_with(&root), "output must not be inside a corpus root");
    }
    if !options.resume {
        ensure!(
            !options.output.exists() || fs::read_dir(&options.output)?.next().is_none(),
            "output directory must be fresh; use explicit --resume only with identical evidence inputs"
        );
        fs::create_dir_all(&options.output)?;
    } else {
        ensure!(options.output.is_dir(), "resume directory missing");
    }
    if let Some(executable) = &options.verapdf {
        options.verapdf = Some(executable.canonicalize().context("veraPDF executable missing")?);
    }
    let oracle = if let Some(executable) = &options.verapdf {
        oracle_provenance(executable, &manifest, &options.sources, options.timeout)?
    } else {
        json!({"status":"not_run","conformance":"incomplete"})
    };
    let probe = if options.resume {
        serde_json::from_slice::<FrozenProbe>(&read_bounded(&options.output.join("frozen-probe.json"), MAX_MANIFEST_BYTES)?)?
    } else {
        let probe = freeze_probe(&options.probe, &options.output)?;
        write_json(&options.output.join("frozen-probe.json"), &probe)?;
        probe
    };
    ensure!(hash_file(&probe.executable)? == probe.sha256 && hash_file(&options.probe)? == probe.sha256, "probe differs from frozen execution bytes");
    let runner_sha = digest(include_bytes!("accessibility_corpus.rs"));
    let runner_executable_sha = hash_file(&std::env::current_exe().context("runner executable path unavailable")?)?;
    let identity = json!({"schema_version":1,"manifest_sha256":manifest_sha,"probe_sha256":probe.sha256,"runner_source_sha256":runner_sha,"runner_executable_sha256":runner_executable_sha,"selection":options.sources,"roots":options.roots,"timeout_seconds":options.timeout.as_secs(),"jobs":options.jobs,"require_full_support":options.require_full_support,"oracle":oracle});
    let identity_path = options.output.join("run-identity.json");
    if options.resume {
        let previous: Value = serde_json::from_slice(&read_bounded(&identity_path, MAX_MANIFEST_BYTES)?)?;
        verify_resume_identity(&previous, &identity)?;
    } else {
        write_json(&identity_path, &identity)?;
    }
    let tasks: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| options.sources.is_empty() || options.sources.contains(&f.source))
        .map(|f| {
            let revision = manifest.sources.iter().find(|s| s.id == f.source).map(|s| s.revision.as_str()).unwrap_or("");
            (f, revision)
        })
        .collect();
    let mut summary = RunSummary::default();
    let mut infrastructure_errors = Vec::new();
    let mut completed = BTreeSet::new();
    write_progress(&options, &summary, tasks.len(), "running", &infrastructure_errors)?;
    let worker_errors = parallel_map(
        tasks.len(),
        options.jobs,
        |position| {
            let Some((f, revision)) = tasks.get(position) else {
                return Err(anyhow::anyhow!("worker task index missing"));
            };
            let result = if options.resume {
                match resumed_result(position + 1, f, revision, &options) {
                    Ok(Some(result)) => result,
                    Ok(None) => run_file(position + 1, f, revision, &options, &probe),
                    Err(error) => interrupted_result(position + 1, f, revision, &format!("resume evidence error: {error:#}")),
                }
            } else {
                run_file(position + 1, f, revision, &options, &probe)
            };
            let durable = persist_result(&options.output, &result).map_err(|e| format!("{e:#}"));
            Ok((result, durable))
        },
        |position, completion| {
            let Some((f, revision)) = tasks.get(position) else {
                infrastructure_errors.push("completion task index missing".into());
                return;
            };
            let (mut result, mut persisted) = match completion {
                Some(Ok(value)) => value,
                Some(Err(error)) => (
                    interrupted_result(position + 1, f, revision, &format!("worker error: {error:#}")),
                    Err("worker did not persist a result".into()),
                ),
                None => (interrupted_result(position + 1, f, revision, "worker panicked or returned no result"), Err("worker interrupted".into())),
            };
            if result.index != position + 1 || result.file.path != f.path || result.file.sha256 != f.sha256 || result.file.source != f.source {
                result = interrupted_result(position + 1, f, revision, "worker returned wrong source identity");
                infrastructure_errors.push("worker returned wrong source identity".into());
                persisted = Err("worker returned wrong source identity".into());
            }
            let durable = if let Err(error) = persisted {
                infrastructure_errors.push(error.clone());
                result.errors.push(format!("initial checkpoint failed: {error}"));
                result.outcome = "failed".into();
                persist_result(&options.output, &result).is_ok()
            } else {
                true
            };
            summary.record(&result, durable);
            completed.insert(position);
            eprintln!("a11y-corpus {}/{} [{}] {}/{}", summary.completed_files, tasks.len(), result.outcome, f.source, f.path);
            if let Err(error) = write_progress(&options, &summary, tasks.len(), "running", &infrastructure_errors) {
                infrastructure_errors.push(format!("progress checkpoint: {error:#}"));
            }
        },
    );
    infrastructure_errors.extend(worker_errors);
    for (position, (f, revision)) in tasks.iter().enumerate() {
        if !completed.contains(&position) {
            let result = interrupted_result(position + 1, f, revision, "result consumer failed; explicit missing-result record");
            let durable = persist_result(&options.output, &result).is_ok();
            summary.record(&result, durable);
        }
    }
    match hash_file(&probe.executable) {
        Ok(hash) if hash == probe.sha256 => {}
        _ => infrastructure_errors.push("frozen probe hash changed or disappeared".into()),
    }
    if let Some(executable) = &options.verapdf {
        match oracle_identity(executable) {
            Ok(after)
                if after.get("executable_sha256") == oracle.get("executable_sha256")
                    && after.get("installed_jar_sha256") == oracle.get("installed_jar_sha256") => {}
            _ => infrastructure_errors.push("independent oracle installation changed during run".into()),
        }
    }
    let successful = summary.failed_files == 0
        && summary.completed_files == tasks.len()
        && summary.durable_files == tasks.len()
        && infrastructure_errors.is_empty();
    let metadata = json!({"schema_version":1,"status":if successful {"completed"} else {"failed"},"scope":SCOPE,"implementation_generation":"reconstructed_after_source_loss_requires_new_testing","runner_source_sha256":runner_sha,"runner_executable_sha256":runner_executable_sha,"manifest_path":options.manifest,"manifest_sha256":manifest_sha,"manifest_metadata":manifest.metadata,"sources":manifest.sources,"source_counts":summary.source_counts,"selection":options.sources,"selected_files":tasks.len(),"completed_files":summary.completed_files,"jobs":options.jobs,"timeout_seconds":options.timeout.as_secs(),"require_full_support":options.require_full_support,"probe":probe,"oracle":oracle,"summary":summary,"infrastructure_errors":infrastructure_errors,"elapsed_ms":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)});
    if let Err(error) = write_aggregate(&options.output, &metadata, tasks.len()) {
        infrastructure_errors.push(format!("aggregate write failed: {error:#}"));
        write_progress(&options, &summary, tasks.len(), "failed", &infrastructure_errors)?;
        return Err(error);
    }
    write_progress(&options, &summary, tasks.len(), if successful { "completed" } else { "failed" }, &infrastructure_errors)?;
    ensure!(
        successful,
        "{} selected files failed; {} infrastructure errors; inspect {} (no successful skips)",
        summary.failed_files,
        infrastructure_errors.len(),
        options.output.join("report.json").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("pdfcraft-a11y-runner-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn file() -> CorpusFile {
        serde_json::from_value(json!({"source":"test","path":"fixture.pdf","sha256":digest(b"fixture"),"profile":"ua1","feature_tags":[],"upstream_target":{"expected":"pass","clause":"7.1","test":1,"specification":"ISO 14289-1:2014"},"overall_conformance":"unknown","expected_refusals":[]})).unwrap()
    }
    fn manifest(f: CorpusFile) -> Manifest {
        Manifest {
            schema_version: 1,
            sources: vec![Source { id: "test".into(), revision: "pinned-revision".into(), expected_files: 1, metadata: BTreeMap::new() }],
            files: vec![f],
            metadata: BTreeMap::new(),
        }
    }
    fn passed(name: &str) -> Value {
        let mut edit = json!({"operation":name,"status":"passed","observed_edit":true,"rendered_pages_per_snapshot":1});
        for key in EDIT_INVARIANTS {
            edit[*key] = json!("passed");
        }
        match name {
            "title_property" => {
                edit["unknown_data_harmless_edit"] = json!("passed");
                edit["checker_findings_preserved"] = json!("passed");
            }
            "metadata_properties" => {
                edit["unknown_data_property_edit"] = json!("passed");
                edit["changed_properties"] = json!(["alt", "actual_text", "lang", "expansion"]);
            }
            "artifact" => {
                edit["semantic_artifact_rewrite"] = json!("passed");
                edit["artifact_rewrite_evidence"] = json!({"mcid_sequences_converted":1});
            }
            _ => {}
        }
        if let Some(kind) = name.strip_prefix("leaf_move_") {
            edit["leaf_move_coverage"] = json!({"selected_kind":kind,"present_kinds":[kind],"untested_present_kinds":[]});
        }
        edit
    }
    fn probe(f: &CorpusFile) -> Value {
        let mut stages = json!({});
        for stage in REQUIRED_STAGES {
            stages[*stage] = json!({"status":"passed"});
        }
        stages["input"]["sha256"] = json!(f.sha256);
        stages["open"] = json!({"status":"passed","pages":1,"bytes_changed_during_open":false,"open_transform_kind":"none","post_open_sha256":f.sha256,"source_to_open_preservation":"byte_identical"});
        stages["render"]["pages_rendered"] = json!(1);
        stages["inspect_tags"] = json!({"status":"passed","elements":1,"root":"2 0 R","root_children":1,"issues":{"total":0,"items":[],"omitted":0}});
        stages["geometry"] = json!({"status":"passed","pages_inspected":1,"whole_page_reverse_lookups":1,"owned_identities_without_geometry":0,"geometry_issue_count":0});
        let edits: Vec<_> = REQUIRED_EDITS
            .iter()
            .map(|name| {
                if *name == "title_property" {
                    passed(name)
                } else {
                    json!({"operation":name,"status":"not_applicable","reason_code":"precondition_absent"})
                }
            })
            .collect();
        json!({"schema_version":1,"outcome":"passed","active_stage":null,"stages":stages,"edits":edits})
    }
    fn edit<'a>(p: &'a mut Value, name: &str) -> &'a mut Value {
        p["edits"].as_array_mut().unwrap().iter_mut().find(|e| e["operation"] == name).unwrap()
    }
    fn expect(f: &mut CorpusFile, name: &str) {
        f.expected_refusals.push(
            serde_json::from_value(
                json!({"operation":name,"reason_code":"permission_denied","reason":"tag editing is denied","category":"permission_restricted"}),
            )
            .unwrap(),
        );
    }
    fn refusal(name: &str) -> Value {
        json!({"operation":name,"status":"refused","reason_code":"permission_denied","reason":"tag editing is denied","unchanged_on_failure":true})
    }
    fn settings(root: &Temp) -> Options {
        Options {
            manifest: root.0.join("manifest.json"),
            probe: root.0.join("probe"),
            output: root.0.join("output"),
            roots: BTreeMap::from([("test".into(), root.0.join("source"))]),
            sources: BTreeSet::new(),
            timeout: Duration::from_secs(2),
            jobs: 1,
            require_full_support: false,
            verapdf: None,
            resume: false,
        }
    }
    fn source(root: &Temp, f: &CorpusFile) -> Options {
        let o = settings(root);
        fs::create_dir_all(&o.roots["test"]).unwrap();
        fs::write(o.roots["test"].join(&f.path), b"fixture").unwrap();
        fs::create_dir_all(&o.output).unwrap();
        o
    }
    fn oracle(passed_checks: u64, failed_checks: u64) -> Value {
        let failed = failed_checks > 0;
        json!({"report":{"jobs":[{"validationResult":[{"jobEndStatus":"normal","profileName":Profile::Ua1.name(),"compliant":!failed,"details":{"passedRules":if failed {0} else {1},"failedRules":if failed {1} else {0},"ruleSummaries":[{"specification":"ISO 14289-1:2014","clause":"7.1","testNumber":1,"status":if failed {"failed"} else {"passed"},"passedChecks":passed_checks,"failedChecks":failed_checks}]}}]}]}})
    }
    fn observation(v: &Value, f: &CorpusFile) -> OracleObservation {
        parse_oracle(v, f, Path::new("oracle.json")).unwrap()
    }
    fn allow_zero(f: &mut CorpusFile) {
        f.expected_target_inapplicability = Some(
            json!({"specification":"ISO 14289-1:2014","clause":"7.1","test":1,"justification":"independently verified absence","source_evidence":["raw dictionary has no relevant object"]}),
        );
    }
    fn creation(existing: bool) -> (Value, Value) {
        let mut p = probe(&file());
        p["stages"]["inspect_tags"]["elements"] = json!(0);
        p["stages"]["inspect_tags"]["root_children"] = json!(0);
        p["stages"]["inspect_tags"]["root"] = if existing { json!("2 0 R") } else { Value::Null };
        let mut e = passed("create_root");
        e["unknown_data_create_root"] = json!("passed");
        e["root_creation_evidence"] = json!({"status":"passed","mode":if existing {"create_first_tag_existing_root"} else {"create_structure_root"},"original_tag_count":0,"original_struct_tree_root_absent":!existing,"new_indirect_root":!existing,"existing_root_identity_preserved":existing,"new_document_tag":true,"catalog_parent_links":"passed","mark_info_change":"passed","mark_info_action":if existing {"preserved"} else {"marked_true"},"marked_before":false,"marked_after":!existing,"content_associations_created":0,"catalog_ref":"1 0 R","struct_tree_root_ref":"2 0 R","document_tag_ref":"3 0 R"});
        (p, e)
    }
    fn orphan_creation() -> (Value, Value) {
        let (mut p, mut e) = creation(true);
        p["stages"]["inspect_tags"]["issues"] =
            json!({"total":1,"items":["ParentTree key 4 refers to unreachable tag 8 2 R (preserved association)"],"omitted":0});
        p["stages"]["inspect_tags"]["edit_blockers"] = json!({"total":0,"items":[],"omitted":0});
        p["stages"]["inspect_tags"]["preserved_associations"] =
            json!({"total":1,"items":[{"kind":"unreachable_owner","key":4,"mcid":2,"owner_ref":"8 2 R"}],"omitted":0});
        let sha = digest(b"exact graph");
        e["root_creation_evidence"]["preserved_associations"] = json!({"status":"passed","count":1,"classification":"unreachable_owner","warnings_exactly_classified":true,"edit_blockers":0,"association_inventory_sha256":digest(&serde_json::to_vec(&json!([["unreachable_owner",4,2,"8 2 R"]])).unwrap()),"index_slots_sha256_before":sha,"index_slots_sha256_after":sha,"owner_graph_sha256_before":sha,"owner_graph_sha256_after":sha,"index_graph_sha256_before":sha,"index_graph_sha256_after":sha,"index_objects_checked":1,"owner_objects_checked":1,"verified_snapshots":["after_edit","after_redo","after_save_reopen"]});
        (p, e)
    }

    #[test]
    fn sha256_is_exact_lowercase_hex() {
        assert_eq!(digest(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
    #[test]
    fn pinned_manifest_population_and_reviewed_expectations_are_exact() {
        let bytes = include_bytes!("../baselines/accessibility-corpus.json");
        assert_eq!(digest(bytes), "11a5946e7282db563f42c40eb308c1c891fce4b56e184f89f6c3e2357f13e25d");
        let m: Manifest = serde_json::from_slice(bytes).unwrap();
        validate_manifest(&m, &BTreeSet::new()).unwrap();
        assert_eq!(m.files.len(), 526);
        let counts: BTreeMap<_, _> = m.sources.iter().map(|s| (s.id.as_str(), s.expected_files)).collect();
        assert_eq!(counts, BTreeMap::from([("verapdf", 435), ("techniques", 82), ("reference", 9)]));
        assert_eq!(m.files.iter().map(|f| f.expected_refusals.len()).sum::<usize>(), 217);
        assert_eq!(m.files.iter().map(|f| f.expected_stage_refusals.len()).sum::<usize>(), 16);
        let mut categories = BTreeMap::new();
        for f in &m.files {
            for e in &f.expected_refusals {
                *categories.entry(e.category.as_deref().unwrap()).or_insert(0) += 1;
            }
        }
        assert_eq!(categories, BTreeMap::from([("safe_invalid_input", 164), ("permission_restricted", 10), ("unsupported_valid_input", 43)]));
        assert_eq!(m.files.iter().filter(|f| manual_review_assessment(f)["required"] == true).count(), 73);
        assert_eq!(m.files.iter().filter(|f| f.expected_target_inapplicability.is_some()).count(), 8);
    }
    #[test]
    fn empty_population_is_fatal() {
        let mut m = manifest(file());
        m.files.clear();
        assert!(validate_manifest(&m, &BTreeSet::new()).is_err());
    }
    #[test]
    fn population_count_is_exact() {
        let mut m = manifest(file());
        m.sources[0].expected_files = 2;
        assert!(validate_manifest(&m, &BTreeSet::new()).is_err());
    }
    #[test]
    fn duplicate_paths_are_fatal() {
        let mut m = manifest(file());
        m.files.push(file());
        m.sources[0].expected_files = 2;
        assert!(validate_manifest(&m, &BTreeSet::new()).is_err());
    }
    #[test]
    fn traversal_and_absolute_paths_rejected() {
        for path in ["../a.pdf", "/a.pdf", "a/../b.pdf", ""] {
            let mut f = file();
            f.path = path.into();
            assert!(validate_manifest(&manifest(f), &BTreeSet::new()).is_err());
        }
    }
    #[test]
    fn known_unmapped_expectation_keeps_intent() {
        let mut f = file();
        f.upstream_target.clause = None;
        f.upstream_target.test = None;
        validate_manifest(&manifest(f.clone()), &BTreeSet::new()).unwrap();
        assert_eq!(f.upstream_target.expected, Expected::Pass);
        assert!(assess_probe(&probe(&f), &f, true).errors.iter().any(|e| e.contains("unmapped")));
    }
    #[test]
    fn target_integer_and_string_are_equivalent() {
        let a: Target = serde_json::from_value(json!({"expected":"pass","test":4})).unwrap();
        let b: Target = serde_json::from_value(json!({"expected":"pass","test":"4"})).unwrap();
        assert_eq!(a.test, b.test);
    }
    #[test]
    fn invalid_partial_mapping_rejected() {
        let mut f = file();
        f.upstream_target.test = None;
        assert!(validate_manifest(&manifest(f), &BTreeSet::new()).is_err());
    }
    #[test]
    fn duplicate_refusal_declarations_rejected() {
        let mut f = file();
        expect(&mut f, "title_property");
        f.expected_refusals.push(f.expected_refusals[0].clone());
        assert!(validate_manifest(&manifest(f.clone()), &BTreeSet::new()).is_err());
        let mut p = probe(&f);
        *edit(&mut p, "title_property") = refusal("title_property");
        let a = assess_probe(&p, &f, false);
        assert!(!a.errors.is_empty());
        assert_eq!(a.expected_refusals, 0);
    }
    #[test]
    fn structural_refusal_requires_exact_independent_diagnosis() {
        let mut f = file();
        expect(&mut f, "title_property");
        f.expected_refusals[0].reason_code = "structure_edit_rejected".into();
        assert!(validate_manifest(&manifest(f), &BTreeSet::new()).is_err());
    }
    #[test]
    fn options_bound_parallelism_and_timeout() {
        for value in ["0", "5"] {
            let args = ["--probe", "p", "--output", "o", "--jobs", value].map(str::to_owned);
            assert!(options(&args).is_err());
        }
        let args = ["--probe", "p", "--output", "o", "--jobs", "3"].map(str::to_owned);
        assert_eq!(options(&args).unwrap().jobs, 3);
        let args = ["--probe", "p", "--output", "o", "--timeout", "0"].map(str::to_owned);
        assert!(options(&args).is_err());
    }
    #[test]
    fn no_op_probe_cannot_pass() {
        let a = assess_probe(&json!({"schema_version":1,"outcome":"passed"}), &file(), false);
        assert!(!a.errors.is_empty());
        assert_eq!(a.supported_edits, 0);
    }
    #[test]
    fn all_not_applicable_is_not_support() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "title_property") = json!({"operation":"title_property","status":"not_applicable","reason_code":"precondition_absent"});
        assert!(assess_probe(&p, &f, false).errors.iter().any(|e| e.contains("no supported edit")));
    }
    #[test]
    fn completed_operation_with_all_evidence_is_counted() {
        let f = file();
        let a = assess_probe(&probe(&f), &f, false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(a.supported_edits, 1);
        assert_eq!(a.not_applicable_edits, 13);
    }
    #[test]
    fn missing_success_invariant_is_not_counted() {
        for key in EDIT_INVARIANTS {
            let f = file();
            let mut p = probe(&f);
            edit(&mut p, "title_property").as_object_mut().unwrap().remove(*key);
            let a = assess_probe(&p, &f, false);
            assert_eq!(a.supported_edits, 0);
            assert!(!a.errors.is_empty());
        }
    }
    #[test]
    fn artifact_must_convert_actual_marked_content() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "artifact") = passed("artifact");
        edit(&mut p, "artifact")["artifact_rewrite_evidence"]["mcid_sequences_converted"] = json!(0);
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.supported_edits, 1);
        assert!(a.errors.iter().any(|e| e.contains("marked-content")));
    }
    #[test]
    fn metadata_requires_all_four_properties() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "metadata_properties") = passed("metadata_properties");
        edit(&mut p, "metadata_properties")["changed_properties"] = json!(["alt"]);
        assert_eq!(assess_probe(&p, &f, false).supported_edits, 1);
    }
    #[test]
    fn refusal_must_be_expected_exactly_once() {
        let mut f = file();
        expect(&mut f, "title_property");
        let mut p = probe(&f);
        *edit(&mut p, "title_property") = refusal("title_property");
        let a = assess_probe(&p, &f, false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(a.expected_refusals, 1);
        assert_eq!(a.supported_edits, 0);
        assert_eq!(a.expected_refusal_categories["permission_restricted"], 1);
        assert!(!assess_probe(&p, &f, true).errors.is_empty());
    }
    #[test]
    fn expected_refusal_unexpected_acceptance_is_failure() {
        let mut f = file();
        expect(&mut f, "title_property");
        let a = assess_probe(&probe(&f), &f, false);
        assert_eq!(a.supported_edits, 0);
        assert_eq!(a.refusal_expectations["title_property"], "unexpected_acceptance");
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn expected_refusal_na_missing_duplicate_and_wrong_reason_fail() {
        for variant in ["na", "missing", "duplicate", "wrong_reason", "wrong_code", "unchanged"] {
            let mut f = file();
            expect(&mut f, "title_property");
            let mut p = probe(&f);
            *edit(&mut p, "title_property") = refusal("title_property");
            match variant {
                "na" => {
                    edit(&mut p, "title_property")["status"] = json!("not_applicable");
                    edit(&mut p, "title_property")["reason_code"] = json!("precondition_absent");
                }
                "missing" => p["edits"].as_array_mut().unwrap().retain(|e| e["operation"] != "title_property"),
                "duplicate" => p["edits"].as_array_mut().unwrap().push(refusal("title_property")),
                "wrong_reason" => edit(&mut p, "title_property")["reason"] = json!("different"),
                "wrong_code" => edit(&mut p, "title_property")["reason_code"] = json!("read_only"),
                _ => edit(&mut p, "title_property")["unchanged_on_failure"] = json!(false),
            }
            let a = assess_probe(&p, &f, false);
            assert!(!a.errors.is_empty(), "{variant}");
            assert_eq!(a.expected_refusals, 0, "{variant}");
            assert_eq!(a.supported_edits, 0);
        }
    }
    #[test]
    fn undeclared_refusal_is_failure() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "title_property") = refusal("title_property");
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.unexpected_refusals, 1);
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn interrupted_probe_is_not_complete() {
        let f = file();
        let mut p = probe(&f);
        p["active_stage"] = json!("render");
        assert!(assess_probe(&p, &f, false).errors.iter().any(|e| e.contains("unfinished")));
    }
    #[test]
    fn all_page_coverage_is_required() {
        let f = file();
        let mut p = probe(&f);
        p["stages"]["open"]["pages"] = json!(2);
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.supported_edits, 0);
        assert!(a.errors.iter().any(|e| e.contains("every opened page")));
    }
    #[test]
    fn geometry_gaps_fail_full_support_but_are_not_product_defects() {
        let f = file();
        let mut p = probe(&f);
        p["stages"]["geometry"]["owned_identities_without_geometry"] = json!(2);
        assert!(assess_probe(&p, &f, false).errors.is_empty());
        assert!(assess_probe(&p, &f, true).errors.iter().any(|e| e.contains("coverage gap")));
    }
    #[test]
    fn typed_leaf_coverage_uses_successful_union() {
        let f = file();
        let mut p = probe(&f);
        for kind in ["integer_mcid", "page_mcr"] {
            let name = format!("leaf_move_{kind}");
            let mut e = passed(&name);
            e["leaf_move_coverage"]["present_kinds"] = json!(["integer_mcid", "page_mcr"]);
            e["leaf_move_coverage"]["untested_present_kinds"] = json!([if kind == "integer_mcid" { "page_mcr" } else { "integer_mcid" }]);
            *edit(&mut p, &name) = e;
        }
        let a = assess_probe(&p, &f, true);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(a.supported_edits, 3);
    }
    #[test]
    fn invalid_leaf_identity_cannot_inflate_success_count() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "leaf_move_objr") = passed("leaf_move_objr");
        edit(&mut p, "leaf_move_objr")["leaf_move_coverage"]["selected_kind"] = json!("page_mcr");
        let a = assess_probe(&p, &f, true);
        assert_eq!(a.supported_edits, 1);
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn manual_review_is_explicitly_unperformed() {
        let mut f = file();
        f.metadata.insert("manual_review_required".into(), json!(true));
        assert_eq!(manual_review_assessment(&f)["status"], "unverified");
        f.metadata.insert("manual_tests_status".into(), json!("not_performed"));
        let a = assess_probe(&probe(&f), &f, false);
        assert!(a.errors.is_empty());
        assert_eq!(a.manual_review["status"], "not_performed");
        assert!(!assess_probe(&probe(&f), &f, true).errors.is_empty());
    }
    #[test]
    fn missing_oracle_fails_full_support_for_known_targets() {
        let t = Temp::new();
        let mut o = settings(&t);
        o.require_full_support = true;
        let f = file();
        let p = probe(&f);
        let mut r = interrupted_result(1, &f, "r", "test");
        r.errors.clear();
        apply_oracle(&mut r, &p, &BTreeMap::new(), None, Path::new("input"), &t.0, &o);
        assert!(r.errors.iter().any(|e| e.contains("requires independent")));
        assert_eq!(r.oracle["status"], "not_run");
    }
    #[test]
    fn exact_zero_inventory_is_valid() {
        let mut f = file();
        f.independent_inventory = Some(json!({"addressable_structure_elements":0}));
        let mut p = probe(&f);
        p["stages"]["inspect_tags"]["elements"] = json!(0);
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.independent_inventory["addressable_structure_elements"]["status"], "matched");
    }
    #[test]
    fn inventory_mismatch_and_absent_inventory_are_distinct() {
        let mut f = file();
        let p = probe(&f);
        assert_eq!(assess_probe(&p, &f, false).independent_inventory["pages"]["status"], "uninventoried");
        f.independent_inventory = Some(json!({"pages":2}));
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.independent_inventory["pages"]["status"], "mismatch");
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn source_bound_transform_only_explains_inventory() {
        let mut f = file();
        f.expected_open_transform = Some(json!({"kind":"dynamic_xfa_layout","justification":"known layout","source_evidence":["raw XFA"]}));
        f.independent_inventory = Some(json!({"pages":2}));
        let mut p = probe(&f);
        p["stages"]["open"]["bytes_changed_during_open"] = json!(true);
        p["stages"]["open"]["open_transform_kind"] = json!("dynamic_xfa_layout");
        p["stages"]["open"]["dynamic_xfa_regenerated"] = json!(true);
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.independent_inventory["pages"]["status"], "expected_unsupported");
        assert_eq!(a.supported_edits, 0);
        assert_eq!(a.post_open_edits, 1);
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn new_and_existing_root_creation_have_narrow_proofs() {
        for existing in [false, true] {
            let (p, e) = creation(existing);
            verify_create_root_evidence(&p, &e).unwrap();
        }
    }
    #[test]
    fn existing_root_identity_and_markinfo_must_be_preserved() {
        let (p, mut e) = creation(true);
        e["root_creation_evidence"]["marked_after"] = json!(true);
        assert!(verify_create_root_evidence(&p, &e).is_err());
        let (p, mut e) = creation(true);
        e["root_creation_evidence"]["struct_tree_root_ref"] = json!("99 0 R");
        assert!(verify_create_root_evidence(&p, &e).is_err());
    }
    #[test]
    fn creation_na_cannot_waive_title_oracle() {
        let (mut p, e) = creation(true);
        *edit(&mut p, "title_property") = json!({"operation":"title_property","status":"not_applicable","reason_code":"precondition_absent"});
        let saved = BTreeMap::from([("create_root".into(), PathBuf::from("verified.pdf"))]);
        assert!(!verified_creation_without_title(&p, &saved));
        *edit(&mut p, "create_root") = e;
        assert!(verified_creation_without_title(&p, &saved));
        p["stages"]["open"]["bytes_changed_during_open"] = json!(true);
        assert!(!verified_creation_without_title(&p, &saved));
    }
    #[test]
    fn preserved_orphans_require_exact_raw_graph_proof() {
        let (p, e) = orphan_creation();
        verify_create_root_evidence(&p, &e).unwrap();
        for key in ["index_slots_sha256_after", "owner_graph_sha256_before", "index_graph_sha256_after", "association_inventory_sha256"] {
            let mut bad = e.clone();
            bad["root_creation_evidence"]["preserved_associations"].as_object_mut().unwrap().remove(key);
            assert!(verify_create_root_evidence(&p, &bad).is_err(), "{key}");
        }
    }
    #[test]
    fn preserved_orphan_negative_key_rejected_even_with_matching_hash() {
        let (mut p, mut e) = orphan_creation();
        p["stages"]["inspect_tags"]["preserved_associations"]["items"][0]["key"] = json!(-1);
        p["stages"]["inspect_tags"]["issues"]["items"][0] = json!("ParentTree key -1 refers to unreachable tag 8 2 R (preserved association)");
        e["root_creation_evidence"]["preserved_associations"]["association_inventory_sha256"] =
            json!(digest(&serde_json::to_vec(&json!([["unreachable_owner", -1, 2, "8 2 R"]])).unwrap()));
        assert!(verify_create_root_evidence(&p, &e).is_err());
    }
    #[test]
    fn preserved_orphan_generation_and_mcid_are_hash_bound() {
        for (key, value) in [("mcid", json!(3)), ("owner_ref", json!("8 0 R")), ("kind", json!("unclaimed_scalar"))] {
            let (mut p, e) = orphan_creation();
            p["stages"]["inspect_tags"]["preserved_associations"]["items"][0][key] = value;
            assert!(verify_create_root_evidence(&p, &e).is_err());
        }
    }
    #[test]
    fn zero_issue_count_cannot_hide_nonzero_classification() {
        let (mut p, e) = creation(true);
        p["stages"]["inspect_tags"]["preserved_associations"] = json!({"total":1,"items":[],"omitted":1});
        assert!(verify_create_root_evidence(&p, &e).is_err());
    }
    #[test]
    fn oracle_target_is_distinct_from_overall_profile() {
        let f = file();
        let v = observation(&oracle(1, 0), &f);
        assert_eq!(v.target_outcome, "pass");
        assert_eq!(v.upstream_target_verification, "verified");
        verify_oracle_support(&v, &f, false).unwrap();
    }
    #[test]
    fn oracle_wrong_profile_incomplete_vector_and_bad_counts_rejected() {
        for variant in ["profile", "vector", "checks", "compliance"] {
            let mut v = oracle(1, 0);
            let result = &mut v["report"]["jobs"][0]["validationResult"][0];
            match variant {
                "profile" => result["profileName"] = json!("PDF/A"),
                "vector" => result["details"]["passedRules"] = json!(2),
                "checks" => result["details"]["ruleSummaries"][0]["failedChecks"] = json!(1),
                _ => result["compliant"] = json!(false),
            }
            assert!(parse_oracle(&v, &file(), Path::new("r")).is_err(), "{variant}");
        }
    }
    #[test]
    fn oracle_duplicate_rules_and_empty_report_rejected() {
        let mut v = oracle(1, 0);
        let rule = v["report"]["jobs"][0]["validationResult"][0]["details"]["ruleSummaries"][0].clone();
        v["report"]["jobs"][0]["validationResult"][0]["details"]["ruleSummaries"].as_array_mut().unwrap().push(rule);
        assert!(parse_oracle(&v, &file(), Path::new("r")).is_err());
        assert!(parse_oracle(&json!({}), &file(), Path::new("r")).is_err());
    }
    #[test]
    fn expected_zero_instance_stays_incomplete_and_strict_fails() {
        let mut f = file();
        allow_zero(&mut f);
        validate_manifest(&manifest(f.clone()), &BTreeSet::new()).unwrap();
        let v = observation(&oracle(0, 0), &f);
        assert_eq!(v.target_outcome, "not_evaluated");
        assert_eq!(v.upstream_target_verification, "expected_incomplete");
        verify_oracle_support(&v, &f, false).unwrap();
        assert!(verify_oracle_support(&v, &f, true).is_err());
    }
    #[test]
    fn inapplicability_never_waives_wrong_missing_or_failing_target() {
        let mut f = file();
        allow_zero(&mut f);
        let failing = observation(&oracle(0, 1), &f);
        assert!(verify_oracle_support(&failing, &f, false).is_err());
        let mut missing = oracle(0, 0);
        missing["report"]["jobs"][0]["validationResult"][0]["details"]["ruleSummaries"][0]["testNumber"] = json!(99);
        assert!(verify_oracle_support(&observation(&missing, &f), &f, false).is_err());
        f.upstream_target.expected = Expected::Fail;
        assert!(validate_manifest(&manifest(f), &BTreeSet::new()).is_err());
    }
    #[test]
    fn reference_positive_unknown_target_is_profile_level() {
        let mut f = file();
        f.upstream_target.expected = Expected::Unknown;
        f.upstream_target.clause = None;
        f.upstream_target.test = None;
        f.overall_conformance = Conformance::ReferencePositive;
        let v = observation(&oracle(1, 0), &f);
        assert_eq!(v.upstream_target_verification, "not_applicable_reference_profile");
        verify_oracle_support(&v, &f, true).unwrap();
        assert!(verify_oracle_support(&observation(&oracle(0, 1), &f), &f, false).is_err());
    }
    #[test]
    fn oracle_pair_requires_exact_check_counts_even_if_compliant() {
        let f = file();
        let before = observation(&oracle(1, 0), &f);
        let after = observation(&oracle(2, 0), &f);
        assert_ne!(before.rules, after.rules);
        assert_eq!(before.machine_profile_compliant, after.machine_profile_compliant);
    }
    #[test]
    fn missing_input_and_tamper_are_errors() {
        let t = Temp::new();
        let f = file();
        let o = settings(&t);
        assert!(verified_input(&f, &o.roots).is_err());
        let o = source(&t, &f);
        verified_input(&f, &o.roots).unwrap();
        fs::write(o.roots["test"].join(&f.path), b"tampered").unwrap();
        assert!(verified_input(&f, &o.roots).is_err());
    }
    #[test]
    fn artifact_hash_containment_and_noop_are_enforced() {
        let t = Temp::new();
        let dir = t.0.join("artifacts");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("title.pdf");
        fs::write(&path, b"edited").unwrap();
        verify_artifact(&json!(path), &json!(digest(b"edited")), &dir).unwrap();
        assert!(verify_artifact(&json!(path), &json!(digest(b"wrong")), &dir).is_err());
        let outer = t.0.join("outside.pdf");
        fs::write(&outer, b"edited").unwrap();
        assert!(verify_artifact(&json!(outer), &json!(digest(b"edited")), &dir).is_err());
        let p = json!({"edits":[{"operation":"title_property","status":"passed","artifact_path":path,"saved_sha256":digest(b"edited")}]});
        assert!(verify_artifacts(&p, &dir, &digest(b"edited")).is_err());
    }
    #[test]
    fn frozen_probe_survives_original_replacement() {
        let t = Temp::new();
        let original = t.0.join("original");
        fs::write(&original, b"first executable").unwrap();
        let output = t.0.join("out");
        fs::create_dir(&output).unwrap();
        let frozen = freeze_probe(&original, &output).unwrap();
        fs::write(&original, b"replacement executable").unwrap();
        assert_eq!(hash_file(&frozen.executable).unwrap(), digest(b"first executable"));
        assert_ne!(hash_file(&original).unwrap(), frozen.sha256);
    }
    #[test]
    fn parallel_three_accounts_every_index_once_despite_panic() {
        let mut outcomes = BTreeMap::new();
        let errors = parallel_map(
            53,
            3,
            |i| {
                if i == 17 {
                    panic!("synthetic child panic")
                }
                i
            },
            |i, r| {
                assert!(outcomes.insert(i, r).is_none());
            },
        );
        assert!(errors.is_empty());
        assert_eq!(outcomes.len(), 53);
        assert_eq!(outcomes[&17], None);
        for i in 0..53 {
            if i != 17 {
                assert_eq!(outcomes[&i], Some(i));
            }
        }
    }
    #[test]
    fn zero_workers_yields_explicit_missing_results() {
        let mut n = 0;
        let errors = parallel_map(
            3,
            0,
            |i| i,
            |_, r| {
                assert!(r.is_none());
                n += 1;
            },
        );
        assert_eq!(n, 3);
        assert_eq!(errors.len(), 3);
    }
    #[test]
    fn resume_checks_durable_result_hash_and_source() {
        let t = Temp::new();
        let f = file();
        let o = source(&t, &f);
        let r = interrupted_result(1, &f, "revision", "synthetic");
        persist_result(&o.output, &r).unwrap();
        assert!(resumed_result(1, &f, "revision", &o).unwrap().is_some());
        fs::write(o.output.join("00001/result.json"), b"{}").unwrap();
        assert!(resumed_result(1, &f, "revision", &o).is_err());
    }
    #[test]
    fn resume_rechecks_changed_open_artifact() {
        let t = Temp::new();
        let f = file();
        let o = source(&t, &f);
        let path = o.output.join("00001/post_open.pdf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"transformed").unwrap();
        let mut r = interrupted_result(1, &f, "revision", "synthetic");
        r.probe = Some(
            json!({"stages":{"open":{"bytes_changed_during_open":true,"post_open_artifact_path":path,"post_open_sha256":digest(b"transformed")}},"edits":[]}),
        );
        persist_result(&o.output, &r).unwrap();
        assert!(resumed_result(1, &f, "revision", &o).is_ok());
        fs::write(&path, b"changed").unwrap();
        assert!(resumed_result(1, &f, "revision", &o).is_err());
    }
    #[test]
    fn interrupted_attempt_gets_fresh_directory() {
        let t = Temp::new();
        let first = attempt_directory(&t.0, 1).unwrap();
        let second = attempt_directory(&t.0, 1).unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
    }
    #[cfg(unix)]
    #[test]
    fn process_timeout_is_a_terminal_failure() {
        let result = execute(Command::new("/bin/sh").args(["-c", "sleep 3"]), Duration::from_millis(50), 1024, None).unwrap();
        assert!(result.timed_out);
        assert!(!result.success);
        assert!(result.elapsed_ms < 2500);
    }
    #[cfg(unix)]
    #[test]
    fn process_output_is_capped_and_overflow_fails() {
        let result = execute(Command::new("/bin/sh").args(["-c", "printf 123456789"]), Duration::from_secs(2), 4, None).unwrap();
        assert!(!result.success);
        assert!(result.output_limit_exceeded);
        assert_eq!(result.stdout.text, "1234");
    }
    #[cfg(unix)]
    #[test]
    fn zero_output_successful_process_cannot_fake_probe() {
        let t = Temp::new();
        let f = file();
        let mut o = source(&t, &f);
        o.probe = "/bin/true".into();
        let frozen = freeze_probe(&o.probe, &o.output).unwrap();
        let r = run_file(1, &f, "revision", &o, &frozen);
        assert_eq!(r.outcome, "failed");
        assert_eq!(r.assessment.supported_edits, 0);
        assert!(!r.errors.is_empty());
    }
    #[test]
    fn installed_profile_digest_rejects_alteration_empty_and_invalid_pin() {
        let bytes = b"<profile version='1'>pinned rules</profile>";
        let pin = digest(bytes);
        assert_eq!(verify_profile_digest(&pin, bytes).unwrap(), pin);
        assert!(verify_profile_digest(&pin, b"<profile version='1'>altered rules</profile>").is_err());
        assert!(verify_profile_digest(&pin, b"").is_err());
        assert!(verify_profile_digest("missing", bytes).is_err());
    }
    #[test]
    fn installed_profile_requires_unique_cli_jar() {
        let t = Temp::new();
        let executable = t.0.join("verapdf");
        fs::write(&executable, b"placeholder launcher").unwrap();
        let m = manifest(file());
        assert!(verify_installed_profiles(&executable, &m, &BTreeSet::new(), Duration::from_secs(1)).is_err());
        fs::write(t.0.join("cli-first.jar"), b"placeholder jar one").unwrap();
        fs::write(t.0.join("cli-second.jar"), b"placeholder jar two").unwrap();
        let error = verify_installed_profiles(&executable, &m, &BTreeSet::new(), Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }
    #[test]
    fn resume_rejects_runner_executable_change_with_identical_module() {
        let sha = digest(b"same source");
        let old = json!({"runner_source_sha256":sha,"runner_executable_sha256":digest(b"original binary"),"manifest_sha256":sha,"probe_sha256":sha});
        verify_resume_identity(&old, &old).unwrap();
        let mut changed = old.clone();
        changed["runner_executable_sha256"] = json!(digest(b"changed dependencies or main"));
        assert!(verify_resume_identity(&old, &changed).is_err());
        changed.as_object_mut().unwrap().remove("runner_executable_sha256");
        assert!(verify_resume_identity(&old, &changed).is_err());
    }
    #[test]
    fn invalid_leaf_invariant_does_not_satisfy_present_kind_coverage() {
        let f = file();
        let mut p = probe(&f);
        *edit(&mut p, "leaf_move_objr") = passed("leaf_move_objr");
        edit(&mut p, "leaf_move_objr")["undo"] = json!("error");
        let a = assess_probe(&p, &f, true);
        assert_eq!(a.supported_edits, 1);
        assert!(a.errors.iter().any(|e| e.contains("present leaf kind objr not exercised")));
    }
    #[cfg(unix)]
    #[test]
    fn all_missing_sources_get_durable_results_in_manifest_order() {
        let t = Temp::new();
        let mut m = manifest(file());
        m.sources[0].expected_files = 3;
        m.files = (0..3)
            .map(|i| {
                let mut f = file();
                f.path = format!("case-{i}.pdf");
                f
            })
            .collect();
        let manifest_path = t.0.join("manifest.json");
        write_json(&manifest_path, &m).unwrap();
        let output = t.0.join("output");
        let args = vec![
            "--manifest".into(),
            manifest_path.to_string_lossy().into_owned(),
            "--probe".into(),
            "/bin/true".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
            "--corpus-root".into(),
            format!("test={}", t.0.join("missing-source-root").display()),
            "--jobs".into(),
            "3".into(),
        ];
        assert!(run(&args).is_err());
        let report: Value = serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert_eq!(report["summary"]["completed_files"], 3);
        assert_eq!(report["summary"]["durable_files"], 3);
        for (i, result) in report["files"].as_array().unwrap().iter().enumerate() {
            assert_eq!(result["index"], i + 1);
            assert_eq!(result["file"]["path"], format!("case-{i}.pdf"));
            assert_eq!(result["outcome"], "failed");
            assert!(output.join(format!("{:05}/checkpoint.json", i + 1)).is_file());
        }
    }
    #[cfg(unix)]
    fn fake_oracle(t: &Temp) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = t.0.join("oracle");
        let body = format!("#!/bin/sh\ncat <<'REPORT'\n{}\nREPORT\n", oracle(1, 0));
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[cfg(unix)]
    #[test]
    fn different_expected_refusal_cannot_waive_missing_title_oracle() {
        let t = Temp::new();
        let mut o = settings(&t);
        o.verapdf = Some(fake_oracle(&t));
        let mut f = file();
        expect(&mut f, "delete");
        let mut p = probe(&f);
        *edit(&mut p, "delete") = refusal("delete");
        *edit(&mut p, "title_property") = json!({"operation":"title_property","status":"not_applicable","reason_code":"precondition_absent"});
        let mut r = interrupted_result(1, &f, "revision", "test");
        r.errors.clear();
        r.assessment = assess_probe(&p, &f, false);
        apply_oracle(&mut r, &p, &BTreeMap::new(), None, Path::new("fixture.pdf"), &t.0, &o);
        assert!(r.errors.iter().any(|e| e.contains("harmless-edit artifact")));
    }
    #[cfg(unix)]
    #[test]
    fn proved_existing_root_creation_keeps_source_only_oracle_label() {
        let t = Temp::new();
        let mut o = settings(&t);
        o.verapdf = Some(fake_oracle(&t));
        let f = file();
        let (mut p, e) = orphan_creation();
        *edit(&mut p, "create_root") = e;
        *edit(&mut p, "title_property") = json!({"operation":"title_property","status":"not_applicable","reason_code":"precondition_absent"});
        let mut r = interrupted_result(1, &f, "revision", "test");
        r.errors.clear();
        r.assessment = assess_probe(&p, &f, false);
        let saved = BTreeMap::from([("create_root".into(), PathBuf::from("verified.pdf"))]);
        apply_oracle(&mut r, &p, &saved, None, Path::new("fixture.pdf"), &t.0, &o);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.oracle["status"], "source_only");
        assert_eq!(r.oracle["after"]["status"], "not_exercised");
        assert!(r.oracle.get("rule_outcomes_preserved").is_none());
    }
    fn expected_figure_fixture() -> (CorpusFile, Value) {
        let mut f = file();
        f.expected_stage_refusals.push(serde_json::from_value(json!({"stage":"figures_api_parity","reason_code":"structure_resolution_incomplete","reason":"cannot enumerate figures: independently proved malformed graph","category":"safe_invalid_input","justification":"independent raw source proof","source_evidence":["pinned source object graph proof"]})).unwrap());
        let mut p = probe(&f);
        p["stages"]["inspect_tags"]["issues"] = json!({"total":1,"items":["independently proved malformed graph"],"omitted":0});
        p["stages"]["inspect_tags"]["edit_blockers"] = json!({"total":1,"items":["independently proved malformed graph"],"omitted":0});
        let finding = "Structure resolution incomplete: independently proved malformed graph";
        p["stages"]["checker"]["rules"] = json!([{"rule":"FiguresAltText","status":"manual","findings":[{"page":null,"message":finding}]}]);
        let diagnostic = json!({"status":"passed","classification":"structure_resolution_incomplete","strict_error":f.expected_stage_refusals[0].reason,"checker_rule":"FiguresAltText","checker_status":"manual","checker_finding":finding,"legacy_empty_fallback_verified":true,"post_open_sha256":f.sha256,"inspector_issue_count":1,"inspector_edit_blocker_count":1,"tag_tree_known_figure_count":1,"tag_tree_known_figure_ids":["8 2 R"]});
        p["stages"]["figures_api_parity"] = json!({"status":"refused","reason_code":"structure_resolution_incomplete","reason":f.expected_stage_refusals[0].reason,"legacy_figures":0,"tag_tree_figures":1,"known_unenumerated_figure_ids":["8 2 R"],"diagnostic":diagnostic});
        p["stages"]["figures_api_diagnostic"] = diagnostic;
        (f, p)
    }
    fn sync_figure_diagnostic(p: &mut Value) {
        p["stages"]["figures_api_diagnostic"] = p["stages"]["figures_api_parity"]["diagnostic"].clone();
    }
    #[test]
    fn expected_figure_refusal_is_separate_from_edit_support_and_raw_status() {
        let (f, p) = expected_figure_fixture();
        validate_manifest(&manifest(f.clone()), &BTreeSet::new()).unwrap();
        let a = assess_probe(&p, &f, false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(a.expected_stage_refusals, 1);
        assert_eq!(a.expected_refusals, 0);
        assert_eq!(a.supported_edits, 1);
        assert_eq!(a.expected_stage_refusal_categories["safe_invalid_input"], 1);
        assert_eq!(a.stage_outcomes["figures_api_parity"], "expected_refusal");
        assert_eq!(p["stages"]["figures_api_parity"]["status"], "refused");
        assert_eq!(compatible_outcome(&a, &Value::Null), "expected_refusal");
        assert!(assess_probe(&p, &f, true).errors.iter().any(|e| e.contains("expected stage refusals")));
    }
    #[test]
    fn expected_stage_acceptance_missing_wrong_reason_and_duplicate_fail() {
        for variant in ["accepted", "missing", "reason", "code", "error", "duplicate"] {
            let (mut f, mut p) = expected_figure_fixture();
            match variant {
                "accepted" => p["stages"]["figures_api_parity"]["status"] = json!("passed"),
                "missing" => {
                    p["stages"].as_object_mut().unwrap().remove("figures_api_parity");
                }
                "reason" => p["stages"]["figures_api_parity"]["reason"] = json!("different"),
                "code" => p["stages"]["figures_api_parity"]["reason_code"] = json!("other"),
                "error" => p["stages"]["figures_api_parity"]["status"] = json!("error"),
                _ => f.expected_stage_refusals.push(f.expected_stage_refusals[0].clone()),
            }
            let a = assess_probe(&p, &f, false);
            assert!(!a.errors.is_empty(), "{variant}");
            assert_eq!(a.expected_stage_refusals, 0, "{variant}");
            assert_eq!(a.stage_outcomes["figures_api_parity"], "expectation_failed", "{variant}");
        }
    }
    #[test]
    fn figure_stage_expectation_does_not_waive_any_other_failure() {
        let (f, mut p) = expected_figure_fixture();
        p["stages"]["geometry"]["status"] = json!("error");
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.expected_stage_refusals, 1);
        assert!(a.errors.iter().any(|e| e.contains("required stage geometry")));
        let (f, mut p) = expected_figure_fixture();
        *edit(&mut p, "title_property") = refusal("title_property");
        let a = assess_probe(&p, &f, false);
        assert_eq!(a.expected_stage_refusals, 1);
        assert_eq!(a.unexpected_refusals, 1);
        assert!(!a.errors.is_empty());
    }
    #[test]
    fn figure_diagnostic_requires_exact_checker_hash_and_complete_counts() {
        for variant in
            ["diagnostic_missing", "sha", "checker_status", "checker_page", "checker_duplicate", "inspector_count", "inspector_omitted", "legacy"]
        {
            let (f, mut p) = expected_figure_fixture();
            match variant {
                "diagnostic_missing" => {
                    p["stages"].as_object_mut().unwrap().remove("figures_api_diagnostic");
                }
                "sha" => {
                    p["stages"]["figures_api_parity"]["diagnostic"]["post_open_sha256"] = json!(digest(b"different bytes"));
                    sync_figure_diagnostic(&mut p);
                }
                "checker_status" => p["stages"]["checker"]["rules"][0]["status"] = json!("passed"),
                "checker_page" => p["stages"]["checker"]["rules"][0]["findings"][0]["page"] = json!(0),
                "checker_duplicate" => {
                    let r = p["stages"]["checker"]["rules"][0].clone();
                    p["stages"]["checker"]["rules"].as_array_mut().unwrap().push(r);
                }
                "inspector_count" => p["stages"]["inspect_tags"]["issues"]["total"] = json!(2),
                "inspector_omitted" => p["stages"]["inspect_tags"]["issues"]["omitted"] = json!(1),
                _ => {
                    p["stages"]["figures_api_parity"]["diagnostic"]["legacy_empty_fallback_verified"] = json!(false);
                    sync_figure_diagnostic(&mut p);
                }
            }
            let a = assess_probe(&p, &f, false);
            assert_eq!(a.expected_stage_refusals, 0, "{variant}");
            assert!(!a.errors.is_empty(), "{variant}");
        }
    }
    #[test]
    fn figure_gap_requires_exact_unique_generation_bearing_identities() {
        for variant in ["generation", "missing_generation", "duplicate", "count"] {
            let (f, mut p) = expected_figure_fixture();
            match variant {
                "generation" => p["stages"]["figures_api_parity"]["known_unenumerated_figure_ids"] = json!(["8 0 R"]),
                "missing_generation" => {
                    p["stages"]["figures_api_parity"]["diagnostic"]["tag_tree_known_figure_ids"] = json!(["8 R"]);
                    sync_figure_diagnostic(&mut p);
                }
                "duplicate" => {
                    p["stages"]["figures_api_parity"]["diagnostic"]["tag_tree_known_figure_ids"] = json!(["8 2 R", "8 2 R"]);
                    sync_figure_diagnostic(&mut p);
                }
                _ => p["stages"]["figures_api_parity"]["tag_tree_figures"] = json!(0),
            }
            assert!(verify_figure_stage_refusal(&p, &f.expected_stage_refusals[0]).is_err(), "{variant}");
        }
    }
    #[test]
    fn stage_allowance_requires_narrow_type_and_independent_evidence() {
        for variant in ["stage", "code", "category", "reason", "evidence", "duplicate"] {
            let (mut f, _) = expected_figure_fixture();
            match variant {
                "stage" => f.expected_stage_refusals[0].stage = "render".into(),
                "code" => f.expected_stage_refusals[0].reason_code = "any_error".into(),
                "category" => f.expected_stage_refusals[0].category = "waived".into(),
                "reason" => f.expected_stage_refusals[0].reason = "generic failure".into(),
                "evidence" => {
                    f.expected_stage_refusals[0].metadata.remove("source_evidence");
                }
                _ => f.expected_stage_refusals.push(f.expected_stage_refusals[0].clone()),
            }
            assert!(validate_manifest(&manifest(f), &BTreeSet::new()).is_err(), "{variant}");
        }
    }
    #[test]
    fn stage_summary_counts_never_merge_into_edit_refusals() {
        let (f, p) = expected_figure_fixture();
        let mut r = interrupted_result(1, &f, "revision", "test");
        r.assessment = assess_probe(&p, &f, false);
        r.errors.clear();
        r.outcome = "expected_refusal".into();
        let mut summary = RunSummary::default();
        summary.record(&r, true);
        assert_eq!(summary.expected_stage_refusals, 1);
        assert_eq!(summary.expected_refusals, 0);
        assert_eq!(summary.supported_edits, 1);
        assert_eq!(summary.stage_refusal_expectation_outcomes["observed"], 1);
    }
}
