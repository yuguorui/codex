use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow::MAX_WORKFLOW_SCRIPT_BYTES;
use codex_workflow::ResolvedWorkflowChild;
use codex_workflow::ValidatedWorkflowScript;
use codex_workflow::WorkflowChildFuture;
use codex_workflow::WorkflowChildRequest;
use codex_workflow::WorkflowChildResolver;
use codex_workflow::WorkflowScriptError;
use codex_workflow::validate_workflow_script;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Weak;
use tokio::io::AsyncReadExt;

use codex_core::ThreadManager;
use codex_core::config::Config;

use crate::bundled;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct WorkflowInput {
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub args: JsonValue,
    #[serde(default)]
    pub script_path: Option<String>,
    #[serde(default)]
    pub resume_from_run_id: Option<String>,
}

pub(crate) struct ResolvedWorkflow {
    pub script: ValidatedWorkflowScript,
    pub args: JsonValue,
    pub resume_from_run_id: Option<String>,
    pub origin: WorkflowOrigin,
    pub shadows_existing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowOrigin {
    Inline,
    Bundled,
    File(AbsolutePathBuf),
    Plugin {
        namespace: String,
        path: AbsolutePathBuf,
    },
}

impl WorkflowOrigin {
    pub(crate) fn approval_label(&self) -> String {
        match self {
            Self::Inline => "inline script supplied by the model".to_string(),
            Self::Bundled => "bundled workflow".to_string(),
            Self::File(path) => format!("workflow file {}", path.display()),
            Self::Plugin { namespace, path } => {
                format!("active plugin {namespace} workflow file {}", path.display())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PluginWorkflowRoot {
    namespace: String,
    workflows_dir: AbsolutePathBuf,
}

pub(crate) async fn active_plugin_workflow_roots(
    thread_manager: &Weak<ThreadManager>,
    config: &Config,
) -> Vec<PluginWorkflowRoot> {
    let Some(thread_manager) = thread_manager.upgrade() else {
        return Vec::new();
    };
    let plugins = thread_manager
        .plugins_manager()
        .plugins_for_config(&config.plugins_config_input())
        .await;
    let mut roots = plugins
        .plugins()
        .iter()
        .filter(|plugin| plugin.is_active())
        .filter_map(|plugin| {
            plugin
                .plugin_namespace
                .as_ref()
                .map(|namespace| PluginWorkflowRoot {
                    namespace: namespace.clone(),
                    workflows_dir: plugin.root.join("workflows"),
                })
        })
        .collect::<Vec<_>>();
    roots.sort_unstable_by(|left, right| {
        (&left.namespace, &left.workflows_dir).cmp(&(&right.namespace, &right.workflows_dir))
    });
    roots.dedup();
    roots
}

#[derive(Clone)]
pub(crate) struct SavedWorkflowChildResolver {
    cwd: AbsolutePathBuf,
    codex_home: AbsolutePathBuf,
    plugin_roots: Vec<PluginWorkflowRoot>,
}

impl SavedWorkflowChildResolver {
    pub(crate) fn new(
        cwd: AbsolutePathBuf,
        codex_home: AbsolutePathBuf,
        plugin_roots: Vec<PluginWorkflowRoot>,
    ) -> Self {
        Self {
            cwd,
            codex_home,
            plugin_roots,
        }
    }
}

impl WorkflowChildResolver for SavedWorkflowChildResolver {
    fn resolve_child<'a>(&'a self, request: WorkflowChildRequest) -> WorkflowChildFuture<'a> {
        Box::pin(async move {
            let source = match &request.name_or_ref {
                JsonValue::String(name) => {
                    let (source, _, _) =
                        resolve_named(name, &self.cwd, &self.codex_home, &self.plugin_roots)
                            .await
                            .map_err(|error| error.to_string())?;
                    source
                }
                JsonValue::Object(reference) if reference.len() == 1 => {
                    if let Some(name) = reference.get("name").and_then(JsonValue::as_str) {
                        let (source, _, _) =
                            resolve_named(name, &self.cwd, &self.codex_home, &self.plugin_roots)
                                .await
                                .map_err(|error| error.to_string())?;
                        source
                    } else if let Some(script_path) =
                        reference.get("scriptPath").and_then(JsonValue::as_str)
                    {
                        read_explicit_path(script_path, &self.cwd)
                            .await
                            .map_err(|error| error.to_string())?
                            .0
                    } else {
                        return Err(child_reference_error());
                    }
                }
                JsonValue::Object(_)
                | JsonValue::Array(_)
                | JsonValue::Bool(_)
                | JsonValue::Null
                | JsonValue::Number(_) => return Err(child_reference_error()),
            };
            let script = validate_workflow_script(source).map_err(|error| error.to_string())?;
            Ok(ResolvedWorkflowChild {
                script,
                args: request.args,
            })
        })
    }
}

fn child_reference_error() -> String {
    "workflow(nameOrRef) expects a saved workflow name, {name}, or {scriptPath} reference"
        .to_string()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkflowResolveError {
    #[error("workflow input must provide scriptPath, script, or name")]
    MissingSource,
    #[error("workflow name must not contain path separators or traversal components")]
    InvalidName,
    #[error("workflow run id must match ^wf_[a-z0-9-]{{6,}}$")]
    InvalidRunId,
    #[error("workflow script was not found: {0}")]
    NotFound(String),
    #[error("workflow scripts must use the .js extension; found {0}")]
    NearMiss(String),
    #[error("failed to read workflow script {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    InvalidScript(#[from] WorkflowScriptError),
}

pub(crate) async fn resolve_workflow(
    input: WorkflowInput,
    cwd: &AbsolutePathBuf,
    codex_home: &AbsolutePathBuf,
    plugin_roots: &[PluginWorkflowRoot],
) -> Result<ResolvedWorkflow, WorkflowResolveError> {
    validate_resume_id(input.resume_from_run_id.as_deref())?;
    let _ignored_compatibility_fields = (&input.description, &input.title);
    let (source, origin, shadows_existing) = if let Some(script_path) = input.script_path.as_deref()
    {
        let (source, path) = read_explicit_path(script_path, cwd).await?;
        (source, WorkflowOrigin::File(path), false)
    } else if let Some(script) = input.script {
        (script, WorkflowOrigin::Inline, false)
    } else if let Some(name) = input.name.as_deref() {
        resolve_named(name, cwd, codex_home, plugin_roots).await?
    } else {
        return Err(WorkflowResolveError::MissingSource);
    };
    let script = validate_workflow_script(&source)?;
    Ok(ResolvedWorkflow {
        script,
        args: input.args,
        resume_from_run_id: input.resume_from_run_id,
        origin,
        shadows_existing,
    })
}

async fn read_explicit_path(
    raw_path: &str,
    cwd: &AbsolutePathBuf,
) -> Result<(String, AbsolutePathBuf), WorkflowResolveError> {
    if cfg!(windows) && raw_path.starts_with("\\\\") {
        return Err(WorkflowResolveError::InvalidName);
    }
    let raw_path = PathBuf::from(raw_path);
    let path = if raw_path.is_absolute() {
        raw_path
    } else {
        cwd.join(raw_path).to_path_buf()
    };
    ensure_js_extension(&path)?;
    let absolute = AbsolutePathBuf::try_from(path.clone())
        .map_err(|_| WorkflowResolveError::NotFound(path.display().to_string()))?;
    let source = read_workflow_file(&absolute)
        .await?
        .ok_or_else(|| WorkflowResolveError::NotFound(absolute.display().to_string()))?;
    Ok((source, absolute))
}

async fn resolve_named(
    name: &str,
    cwd: &AbsolutePathBuf,
    codex_home: &AbsolutePathBuf,
    plugin_roots: &[PluginWorkflowRoot],
) -> Result<(String, WorkflowOrigin, bool), WorkflowResolveError> {
    validate_name(name)?;
    let mut selected =
        bundled::get(name).map(|source| (source.to_string(), WorkflowOrigin::Bundled));
    let mut shadows_existing = false;

    if let Some((namespace, workflow_name)) = plugin_workflow_name(name) {
        for plugin in plugin_roots
            .iter()
            .filter(|plugin| plugin.namespace == namespace)
        {
            (selected, shadows_existing) = choose_saved(
                selected,
                shadows_existing,
                &plugin.workflows_dir,
                workflow_name,
                SavedWorkflowSource::Plugin(namespace),
            )
            .await?;
        }
    }

    if let Some(parent) = codex_home.parent() {
        (selected, shadows_existing) = choose_saved(
            selected,
            shadows_existing,
            &parent.join(".claude/workflows"),
            name,
            SavedWorkflowSource::File,
        )
        .await?;
    }
    (selected, shadows_existing) = choose_saved(
        selected,
        shadows_existing,
        &codex_home.join("workflows"),
        name,
        SavedWorkflowSource::File,
    )
    .await?;

    let mut ancestors = cwd.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        (selected, shadows_existing) = choose_saved(
            selected,
            shadows_existing,
            &ancestor.join(".claude/workflows"),
            name,
            SavedWorkflowSource::File,
        )
        .await?;
        (selected, shadows_existing) = choose_saved(
            selected,
            shadows_existing,
            &ancestor.join(".codex/workflows"),
            name,
            SavedWorkflowSource::File,
        )
        .await?;
    }

    selected
        .map(|(source, origin)| (source, origin, shadows_existing))
        .ok_or_else(|| WorkflowResolveError::NotFound(name.to_string()))
}

async fn choose_saved(
    current: Option<(String, WorkflowOrigin)>,
    shadows_existing: bool,
    root: &Path,
    name: &str,
    source_kind: SavedWorkflowSource<'_>,
) -> Result<(Option<(String, WorkflowOrigin)>, bool), WorkflowResolveError> {
    let js_path = root.join(format!("{name}.js"));
    if let Some(source) = read_workflow_file(&js_path).await? {
        let absolute = AbsolutePathBuf::try_from(js_path.clone())
            .map_err(|_| WorkflowResolveError::NotFound(js_path.display().to_string()))?;
        let origin = match source_kind {
            SavedWorkflowSource::File => WorkflowOrigin::File(absolute),
            SavedWorkflowSource::Plugin(namespace) => WorkflowOrigin::Plugin {
                namespace: namespace.to_string(),
                path: absolute,
            },
        };
        return Ok((
            Some((source, origin)),
            shadows_existing || current.is_some(),
        ));
    }

    for extension in ["mjs", "cjs", "ts"] {
        let near_miss = root.join(format!("{name}.{extension}"));
        if tokio::fs::try_exists(&near_miss).await.unwrap_or(false) {
            return Err(WorkflowResolveError::NearMiss(
                near_miss.display().to_string(),
            ));
        }
    }
    Ok((current, shadows_existing))
}

async fn read_workflow_file(path: &Path) -> Result<Option<String>, WorkflowResolveError> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(read_error(path, source)),
    };
    let mut bytes = Vec::with_capacity(MAX_WORKFLOW_SCRIPT_BYTES + 1);
    let mut bounded = file.take((MAX_WORKFLOW_SCRIPT_BYTES + 1) as u64);
    bounded
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| read_error(path, source))?;
    if bytes.len() > MAX_WORKFLOW_SCRIPT_BYTES {
        return Err(WorkflowResolveError::InvalidScript(
            WorkflowScriptError::TooLarge,
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|source| read_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))
}

fn read_error(path: &Path, source: io::Error) -> WorkflowResolveError {
    WorkflowResolveError::Read {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Clone, Copy)]
enum SavedWorkflowSource<'a> {
    File,
    Plugin(&'a str),
}

fn plugin_workflow_name(name: &str) -> Option<(&str, &str)> {
    let (namespace, workflow_name) = name.split_once(':')?;
    (!workflow_name.contains(':')).then_some((namespace, workflow_name))
}

fn validate_name(name: &str) -> Result<(), WorkflowResolveError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
        || name.split(':').any(str::is_empty)
        || name.matches(':').count() > 1
    {
        return Err(WorkflowResolveError::InvalidName);
    }
    Ok(())
}

fn validate_resume_id(run_id: Option<&str>) -> Result<(), WorkflowResolveError> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let suffix = run_id.strip_prefix("wf_").unwrap_or_default();
    if suffix.len() < 6
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(WorkflowResolveError::InvalidRunId);
    }
    Ok(())
}

fn ensure_js_extension(path: &Path) -> Result<(), WorkflowResolveError> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("js") => Ok(()),
        Some("mjs" | "cjs" | "ts") => {
            Err(WorkflowResolveError::NearMiss(path.display().to_string()))
        }
        _ => Err(WorkflowResolveError::NotFound(path.display().to_string())),
    }
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
