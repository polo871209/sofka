//! `:dir [path]` — browse local manifests and apply or delete them against the
//! current context. Apply always goes through a `kubectl diff` preview first.
//!
//! The selection is rendered once with a client-side dry run into an
//! in-memory snapshot. The diff, the guardrails, and the apply or delete all
//! use that snapshot, passed to kubectl on stdin, so an edit made after the
//! preview is never applied unseen and rendered Secrets never touch the disk.

use super::*;
use crate::json::Pointer as _;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// File names that make a directory a kustomization, so it is applied with
/// `-k` instead of `-f`.
const KUSTOMIZATION_FILES: &[&str] = &["kustomization.yaml", "kustomization.yml", "Kustomization"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirEntryKind {
    Dir,
    Kustomization,
    Manifest,
}

impl DirEntryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::Kustomization => "kustomize",
            Self::Manifest => "manifest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// For display; lossy when the file name is not UTF-8.
    pub name: String,
    /// The file name as the filesystem has it.
    pub file_name: OsString,
    pub kind: DirEntryKind,
    pub size: Option<u64>,
}

#[derive(Default)]
pub struct DirBrowser {
    pub path: PathBuf,
    pub entries: Vec<DirEntry>,
    pub state: ListState,
    /// The plan whose apply preview is open in the diff view.
    pub preview: Option<DirPlan>,
    /// Bumped by every render request and every listing change, so a result
    /// for an earlier selection is dropped instead of opening.
    pub request: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirVerb {
    Apply,
    Delete,
}

impl DirVerb {
    fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Delete => "delete",
        }
    }
}

/// One object from a rendered source, with its effective namespace (empty
/// for a cluster-scoped object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedObject {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

/// A rendered selection, ready to diff, guard, and run.
pub struct DirPlan {
    pub path: PathBuf,
    /// The rendered objects as a JSON `List`, fed to kubectl on stdin.
    pub snapshot: String,
    pub objects: Vec<RenderedObject>,
    /// `kubectl diff` output for an apply; `None` for a delete.
    pub diff: Option<String>,
}

impl DirBrowser {
    fn selected(&self) -> Option<&DirEntry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    fn selected_path(&self) -> Option<(PathBuf, DirEntryKind)> {
        self.selected()
            .map(|e| (self.path.join(&e.file_name), e.kind))
    }
}

/// The directories, kustomizations, and YAML/JSON manifests in `dir`,
/// directories first. Hidden entries are skipped.
pub(super) fn read_manifest_dir(dir: &Path) -> Result<Vec<DirEntry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in read.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        let kind = if meta.is_dir() {
            if is_kustomization(&entry.path()) {
                DirEntryKind::Kustomization
            } else {
                DirEntryKind::Dir
            }
        } else if meta.is_file() && is_manifest(&name) {
            DirEntryKind::Manifest
        } else {
            continue;
        };
        out.push(DirEntry {
            name,
            file_name,
            kind,
            size: (kind == DirEntryKind::Manifest).then_some(meta.len()),
        });
    }
    out.sort_by(|a, b| {
        (b.kind != DirEntryKind::Manifest)
            .cmp(&(a.kind != DirEntryKind::Manifest))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn is_manifest(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".yaml") || lower.ends_with(".yml") || lower.ends_with(".json")
}

fn is_kustomization(dir: &Path) -> bool {
    KUSTOMIZATION_FILES.iter().any(|f| dir.join(f).is_file())
}

/// The `-f`/`-k` source arguments for `path`: a kustomization is built with
/// `-k`, a directory is read recursively, a file is read as is.
pub(super) fn manifest_source(path: &Path, kind: DirEntryKind) -> Vec<OsString> {
    let path = path.as_os_str().to_os_string();
    match kind {
        DirEntryKind::Kustomization => vec!["-k".into(), path],
        DirEntryKind::Dir => vec!["-f".into(), path, "-R".into()],
        DirEntryKind::Manifest => vec!["-f".into(), path],
    }
}

/// Resolve the `:dir` argument: empty means the working directory, `~` is the
/// home directory, and a relative path starts at the working directory.
pub(super) fn resolve_dir_arg(arg: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let arg = arg.trim();
    let path = if arg.is_empty() {
        cwd.to_path_buf()
    } else if arg == "~" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf())
    } else if let (Some(rest), Some(home)) = (arg.strip_prefix("~/"), home) {
        home.join(rest)
    } else {
        cwd.join(arg)
    };
    std::fs::canonicalize(&path).unwrap_or(path)
}

struct KubectlOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl KubectlOutput {
    fn error(&self) -> String {
        self.stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("kubectl failed")
            .to_string()
    }
}

/// Run kubectl, writing `stdin` to it when given.
async fn run_kubectl(argv: Vec<OsString>, stdin: Option<&str>) -> Result<KubectlOutput, String> {
    use tokio::io::AsyncWriteExt as _;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = async {
        let mut child = cmd.spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    match output.await {
        Ok(out) => Ok(KubectlOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("kubectl not found on PATH".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The objects of a client-side dry run (`-o json`: one object, or a `List`
/// of several), without the last-applied annotation the dry run adds.
pub(super) fn rendered_objects(
    json: &str,
) -> Result<(Vec<serde_json::Value>, Vec<RenderedObject>), String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("unreadable dry-run output: {e}"))?;
    let mut items = match value.get("kind").and_then(|k| k.as_str()) {
        Some("List") => value
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => vec![value],
    };
    let mut objects = Vec::with_capacity(items.len());
    for item in &mut items {
        if let Some(annotations) = item
            .pointer_mut("/metadata/annotations")
            .and_then(|a| a.as_object_mut())
        {
            annotations.remove("kubectl.kubernetes.io/last-applied-configuration");
            if annotations.is_empty()
                && let Some(meta) = item
                    .pointer_mut("/metadata")
                    .and_then(|m| m.as_object_mut())
            {
                meta.remove("annotations");
            }
        }
        let field = |p: &str| {
            item.at(p)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        objects.push(RenderedObject {
            api_version: field("/apiVersion"),
            kind: field("/kind"),
            name: field("/metadata/name"),
            namespace: field("/metadata/namespace"),
        });
    }
    if items.is_empty() {
        return Err("no objects to apply".into());
    }
    Ok((items, objects))
}

/// Render `source` with a client-side dry run, snapshot it in memory, and
/// (for an apply) diff the snapshot against the cluster.
pub(super) async fn plan(
    base: Vec<OsString>,
    verb: DirVerb,
    path: PathBuf,
    kind: DirEntryKind,
) -> Result<DirPlan, String> {
    let mut render = base.clone();
    render.extend(["apply", "--dry-run=client", "-o", "json"].map(OsString::from));
    // A delete needs only identities; a manifest holding just `apiVersion`,
    // `kind`, and `metadata.name` is valid input for it.
    if verb == DirVerb::Delete {
        render.push("--validate=false".into());
    }
    render.extend(manifest_source(&path, kind));
    let out = run_kubectl(render, None).await?;
    if out.code != Some(0) {
        return Err(out.error());
    }
    let (items, objects) = rendered_objects(&out.stdout)?;
    let snapshot =
        serde_json::json!({"apiVersion": "v1", "kind": "List", "items": items}).to_string();
    let diff = match verb {
        DirVerb::Delete => None,
        DirVerb::Apply => {
            let mut argv = base;
            argv.extend(["diff", "-f", "-"].map(OsString::from));
            // `kubectl diff` exits 0 when nothing would change and 1 when
            // something would; anything else is an error.
            let out = run_kubectl(argv, Some(&snapshot)).await?;
            match out.code {
                Some(0) => Some(String::new()),
                Some(1) if !out.stdout.trim().is_empty() => Some(out.stdout),
                _ => return Err(out.error()),
            }
        }
    };
    Ok(DirPlan {
        path,
        snapshot,
        objects,
        diff,
    })
}

impl App {
    /// `:dir [path]` — open the manifest browser at `path`.
    pub(super) fn open_dir(&mut self, arg: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = resolve_dir_arg(arg, &cwd, home.as_deref());
        if !path.is_dir() {
            self.flash_warn(&format!("not a directory: {}", path.display()));
            return;
        }
        if self.load_dir(path, None) {
            self.mode = Mode::Dir;
        }
    }

    /// List `path` and select `keep` (or the first entry). Returns whether
    /// the listing succeeded.
    fn load_dir(&mut self, path: PathBuf, keep: Option<&OsStr>) -> bool {
        match read_manifest_dir(&path) {
            Ok(entries) => {
                let at = keep
                    .and_then(|k| entries.iter().position(|e| e.file_name == k))
                    .unwrap_or(0);
                self.dir.entries = entries;
                self.dir.path = path;
                self.dir
                    .state
                    .select((!self.dir.entries.is_empty()).then_some(at));
                self.dir.request += 1;
                true
            }
            Err(e) => {
                self.flash_warn(&e);
                false
            }
        }
    }

    pub(super) fn key_dir(&mut self, key: KeyInput) {
        let len = self.dir.entries.len();
        let before = self.dir.state.selected();
        match key.action {
            Some(Action::Back | Action::Close) => self.mode = Mode::Table,
            Some(Action::Down) => list_step(&mut self.dir.state, len, true),
            Some(Action::Up) => list_step(&mut self.dir.state, len, false),
            Some(Action::First) if len > 0 => self.dir.state.select(Some(0)),
            Some(Action::Last) if len > 0 => self.dir.state.select(Some(len - 1)),
            Some(a @ (Action::PageDown | Action::HalfPageDown)) => list_page(
                &mut self.dir.state,
                len,
                a.page_size(self.picker_page_items),
                true,
            ),
            Some(a @ (Action::PageUp | Action::HalfPageUp)) => list_page(
                &mut self.dir.state,
                len,
                a.page_size(self.picker_page_items),
                false,
            ),
            Some(Action::Accept) => self.dir_accept(),
            Some(Action::Parent) => self.dir_parent(),
            Some(Action::Refresh) => {
                let keep = self.dir.selected().map(|e| e.file_name.clone());
                self.load_dir(self.dir.path.clone(), keep.as_deref());
            }
            Some(Action::Edit) => self.dir_edit(),
            Some(Action::Apply) => self.dir_plan(DirVerb::Apply),
            Some(Action::Delete) => self.dir_plan(DirVerb::Delete),
            _ => {}
        }
        // A render started for another row must not open for this one.
        if self.dir.state.selected() != before {
            self.dir.request += 1;
        }
    }

    fn dir_accept(&mut self) {
        let Some((path, kind)) = self.dir.selected_path() else {
            return;
        };
        if kind == DirEntryKind::Manifest {
            self.dir_view_file(&path);
        } else {
            self.load_dir(path, None);
        }
    }

    fn dir_parent(&mut self) {
        let Some(parent) = self.dir.path.parent().map(Path::to_path_buf) else {
            return;
        };
        let child = self.dir.path.file_name().map(OsStr::to_os_string);
        self.load_dir(parent, child.as_deref());
    }

    fn dir_view_file(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(body) => {
                self.detail = Scrollable {
                    wrap: self.detail.wrap,
                    title: display_path(path),
                    lines: body.lines().map(String::from).collect::<Vec<_>>().into(),
                    yaml: true,
                    ..Default::default()
                };
                self.return_mode = Mode::Dir;
                self.mode = Mode::Detail;
            }
            Err(e) => self.flash_warn(&format!("{}: {e}", path.display())),
        }
    }

    fn dir_edit(&mut self) {
        let Some((path, kind)) = self.dir.selected_path() else {
            return;
        };
        if kind != DirEntryKind::Manifest {
            self.flash_warn("select a manifest to edit");
            return;
        }
        self.pending = Some(Suspend::Shell(secret_edit::editor_argv(
            secret_edit::configured_editor(),
            &path,
        )));
    }

    /// `a` / `d` — render the selection off-thread. An apply opens its diff
    /// when it arrives; a delete goes straight to its guardrails and dialog.
    fn dir_plan(&mut self, verb: DirVerb) {
        if self.deny_readonly() {
            return;
        }
        let Some((path, kind)) = self.dir.selected_path() else {
            return;
        };
        self.dir.request += 1;
        let request = self.dir.request;
        let doing = match verb {
            DirVerb::Apply => "diffing",
            DirVerb::Delete => "reading",
        };
        let claim = self.claim_status(format!("{doing} {}…", display_path(&path)));
        let base = self
            .kubectl_base()
            .into_iter()
            .map(OsString::from)
            .collect();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = plan(base, verb, path.clone(), kind).await;
            let _ = tx
                .send(Msg::DirPlan {
                    generation: genr,
                    claim,
                    request,
                    verb,
                    path,
                    result: Box::new(result),
                })
                .await;
        });
    }

    pub(super) fn finish_dir_plan(
        &mut self,
        claim: StatusClaim,
        request: u64,
        verb: DirVerb,
        path: PathBuf,
        result: Result<DirPlan, String>,
    ) {
        // Superseded by a later request, or the browser was left: the result
        // belongs to a selection that is no longer on screen.
        if request != self.dir.request || self.mode != Mode::Dir {
            return;
        }
        let plan = match result {
            Ok(plan) => plan,
            Err(e) => {
                self.set_claimed_status(
                    claim,
                    format!("{} {} failed: {e}", verb.name(), display_path(&path)),
                    true,
                );
                return;
            }
        };
        match verb {
            DirVerb::Delete => {
                self.set_claimed_status(claim, String::new(), false);
                self.dir_confirm(verb, plan);
            }
            DirVerb::Apply => {
                let diff = plan.diff.clone().unwrap_or_default();
                if diff.is_empty() {
                    self.set_claimed_status(
                        claim,
                        format!("no changes: {} matches the cluster", display_path(&path)),
                        false,
                    );
                    return;
                }
                let apply = self.keymap.first_label("diff", Action::Apply).to_string();
                self.set_claimed_status(
                    claim,
                    format!("review the diff, {apply} applies it"),
                    false,
                );
                self.detail = Scrollable {
                    wrap: self.detail.wrap,
                    title: format!("apply preview ({apply} to apply) · {}", display_path(&path)),
                    lines: preview_lines(&diff).into(),
                    ..Default::default()
                };
                self.dir.preview = Some(plan);
                self.return_mode = Mode::Dir;
                self.mode = Mode::Diff;
            }
        }
    }

    /// `a` in an apply preview — confirm and apply the previewed snapshot.
    pub(super) fn dir_request_apply(&mut self) {
        if self.dir.preview.is_none() || self.deny_readonly() {
            return;
        }
        let Some(plan) = self.dir.preview.take() else {
            return;
        };
        self.mode = Mode::Dir;
        self.dir_confirm(DirVerb::Apply, plan);
    }

    /// Check the guardrails for every rendered object, by its own resource
    /// and namespace, then open the confirmation.
    fn dir_confirm(&mut self, verb: DirVerb, plan: DirPlan) {
        let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for o in &plan.objects {
            let group = o.api_version.rsplit_once('/').map_or("", |(g, _)| g);
            // A guessed plural could miss a guardrail written for the real
            // one (`networkpolicys`), so an undiscovered kind is refused.
            let Some(kind) = self.cluster.resolve_in_group(&o.kind, group) else {
                self.flash_warn(&format!(
                    "can't check guardrails: {} {} is not in this cluster's discovered resources",
                    o.api_version, o.kind
                ));
                return;
            };
            // kubectl's builder defaults the namespace of every namespaced
            // object it renders (`DefaultNamespace()` → `SetNamespace`). If
            // one still arrives without it, a namespace-scoped guardrail
            // would not see it, so refuse instead of trusting that.
            if kind.namespaced && o.namespace.is_empty() {
                self.flash_warn(&format!(
                    "can't check guardrails: {} {} rendered without a namespace",
                    o.kind, o.name
                ));
                return;
            }
            let plural = kind.ar.plural.to_lowercase();
            groups
                .entry(plural)
                .or_default()
                .push((o.name.clone(), o.namespace.clone()));
        }
        let all: Vec<(String, String, String)> = groups
            .iter()
            .flat_map(|(plural, targets)| {
                targets
                    .iter()
                    .map(|(name, ns)| (plural.clone(), name.clone(), ns.clone()))
            })
            .collect();
        if !self.within_bulk_limits(verb.name(), &all) {
            return;
        }
        let mut level = ConfirmLevel::Plain;
        for (plural, targets) in &groups {
            match self.guard(verb.name(), plural, targets, ConfirmLevel::Plain) {
                Some(l) => level = level.max(l),
                None => return,
            }
        }
        let shown = display_path(&plan.path);
        let count = plan.objects.len();
        let objects = if count == 1 {
            "1 object".to_string()
        } else {
            format!("{count} objects")
        };
        let label = match verb {
            DirVerb::Apply => format!("Apply {shown} ({objects}) to {}?", self.cluster.context),
            DirVerb::Delete => format!(
                "Delete the {objects} in {shown} from {}?",
                self.cluster.context
            ),
        };
        let name_hint = match plan.objects.as_slice() {
            [one] => one.name.clone(),
            _ => count.to_string(),
        };
        self.begin_guarded(
            ConfirmAction::Manifest { verb, plan },
            label,
            level,
            name_hint,
        );
    }

    pub(super) fn do_manifest(&mut self, verb: DirVerb, plan: DirPlan) {
        let shown = display_path(&plan.path);
        self.note_action(verb.name(), shown.clone());
        let mut argv: Vec<OsString> = self
            .kubectl_base()
            .into_iter()
            .map(OsString::from)
            .collect();
        argv.extend([verb.name(), "-f", "-"].map(OsString::from));
        if verb == DirVerb::Delete {
            argv.push("--ignore-not-found".into());
        }
        let claim = self.claim_status(format!("{} {shown}…", verb.name()));
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let (message, err) = match run_kubectl(argv, Some(&plan.snapshot)).await {
                Ok(out) if out.code == Some(0) => {
                    (manifest_summary(verb.name(), &shown, &out.stdout), false)
                }
                Ok(out) => (
                    format!("{} {shown} failed: {}", verb.name(), out.error()),
                    true,
                ),
                Err(e) => (format!("{} {shown} failed: {e}", verb.name()), true),
            };
            let _ = tx
                .send(Msg::Flash {
                    generation: genr,
                    claim,
                    message,
                    err,
                })
                .await;
        });
    }
}

/// `kubectl diff` output with the temp-file headers replaced by one line per
/// object, named the way kubectl names it (`apps.v1.Deployment.ns.name`).
pub(super) fn preview_lines(diff: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff ") {
            let object = rest.rsplit(['/', '\\']).next().unwrap_or(rest).to_string();
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(format!("── {object}"));
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// `path` with the home directory shown as `~`.
pub fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `applied k8s/app.yaml: deployment.apps/web configured (+2 more)`.
pub(super) fn manifest_summary(verb: &str, path: &str, out: &str) -> String {
    let past = if verb == "apply" {
        "applied"
    } else {
        "deleted"
    };
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.as_slice() {
        [] => format!("{past} {path}: nothing to do"),
        [one] => format!("{past} {path}: {one}"),
        [first, rest @ ..] => format!("{past} {path}: {first} (+{} more)", rest.len()),
    }
}
