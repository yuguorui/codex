use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow::MAX_WORKFLOW_SCRIPT_BYTES;
use codex_workflow::ResolvedWorkflowChild;
use codex_workflow::ValidatedWorkflowScript;
use codex_workflow::WorkflowChildFuture;
use codex_workflow::WorkflowChildReference;
use codex_workflow::WorkflowChildRequest;
use codex_workflow::WorkflowChildResolver;
use codex_workflow::validate_workflow_script;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

use crate::discovery::PluginWorkflowRoot;
use crate::discovery::WorkflowOrigin;
use crate::discovery::read_explicit_path;
use crate::discovery::resolve_named;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChildWorkflowPolicy {
    FreezeLocal,
    RejectRemote,
}

#[derive(Clone, Debug)]
pub(crate) struct FrozenWorkflowChild {
    pub(crate) reference: WorkflowChildReference,
    pub(crate) script: ValidatedWorkflowScript,
    pub(crate) origin: WorkflowOrigin,
    pub(crate) shadows_existing: bool,
    pub(crate) script_sha256: String,
}

#[derive(Clone, Debug)]
pub(crate) struct FrozenWorkflowComposition {
    definition_sha256: String,
    children: BTreeMap<WorkflowChildReference, FrozenWorkflowChild>,
}

impl FrozenWorkflowComposition {
    pub(crate) fn empty(script: &ValidatedWorkflowScript) -> Self {
        debug_assert!(script.child_references.is_empty());
        Self {
            definition_sha256: composition_sha256(script, std::iter::empty()),
            children: BTreeMap::new(),
        }
    }

    pub(crate) fn definition_sha256(&self) -> &str {
        &self.definition_sha256
    }

    pub(crate) fn children(&self) -> impl Iterator<Item = &FrozenWorkflowChild> {
        self.children.values()
    }

    pub(crate) fn child_count(&self) -> usize {
        self.children.len()
    }

    pub(crate) fn resolver(&self) -> Option<Arc<dyn WorkflowChildResolver>> {
        (!self.children.is_empty()).then(|| {
            Arc::new(FrozenWorkflowChildResolver {
                children: self.children.clone(),
            }) as Arc<dyn WorkflowChildResolver>
        })
    }

    fn from_children(
        script: &ValidatedWorkflowScript,
        children: BTreeMap<WorkflowChildReference, FrozenWorkflowChild>,
    ) -> Result<Self, String> {
        let references = children.keys().cloned().collect::<Vec<_>>();
        if references != script.child_references {
            return Err(
                "freeze child bindings that exactly match the statically analyzed workflow references"
                    .to_string(),
            );
        }
        let definition_sha256 = composition_sha256(script, children.values());
        Ok(Self {
            definition_sha256,
            children,
        })
    }
}

impl PersistedWorkflowComposition {
    #[cfg(test)]
    pub(crate) fn empty(script: &ValidatedWorkflowScript) -> Self {
        let composition = FrozenWorkflowComposition::empty(script);
        Self {
            definition_sha256: composition.definition_sha256,
            children: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn unavailable() -> Self {
        Self {
            definition_sha256: String::new(),
            children: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct FrozenWorkflowChildResolver {
    children: BTreeMap<WorkflowChildReference, FrozenWorkflowChild>,
}

impl WorkflowChildResolver for FrozenWorkflowChildResolver {
    fn resolve_child<'a>(&'a self, request: WorkflowChildRequest) -> WorkflowChildFuture<'a> {
        Box::pin(async move {
            let reference = WorkflowChildReference::from_runtime_value(&request.name_or_ref)
                .map_err(str::to_string)?;
            let child = self.children.get(&reference).ok_or_else(|| {
                "FrozenWorkflowBindingError: child reference was not approved in the frozen composition"
                    .to_string()
            })?;
            Ok(ResolvedWorkflowChild {
                script: child.script.clone(),
                args: request.args,
            })
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PersistedWorkflowComposition {
    pub(crate) definition_sha256: String,
    pub(crate) children: Vec<PersistedWorkflowChild>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PersistedWorkflowChild {
    pub(crate) reference: WorkflowChildReference,
    pub(crate) origin: WorkflowOrigin,
    pub(crate) shadows_existing: bool,
    pub(crate) artifact_file: String,
    pub(crate) script_sha256: String,
}

pub(crate) async fn freeze_workflow_composition(
    script: &ValidatedWorkflowScript,
    policy: ChildWorkflowPolicy,
    cwd: &AbsolutePathBuf,
    codex_home: &AbsolutePathBuf,
    plugin_roots: &[PluginWorkflowRoot],
) -> Result<FrozenWorkflowComposition, String> {
    if script.child_references.is_empty() {
        return Ok(FrozenWorkflowComposition::empty(script));
    }
    if policy == ChildWorkflowPolicy::RejectRemote {
        return Err(
            "run child workflow composition with a local execution environment filesystem"
                .to_string(),
        );
    }

    let mut children = BTreeMap::new();
    for reference in &script.child_references {
        let (source, origin, shadows_existing) = match reference {
            WorkflowChildReference::Name { name } => {
                resolve_named(name, cwd, codex_home, plugin_roots)
                    .await
                    .map_err(|error| error.to_string())?
            }
            WorkflowChildReference::ScriptPath { script_path } => {
                let (source, path) = read_explicit_path(script_path, cwd)
                    .await
                    .map_err(|error| error.to_string())?;
                (source, WorkflowOrigin::File { path }, false)
            }
        };
        let child_script = validate_workflow_script(source).map_err(|error| error.to_string())?;
        if !child_script.child_references.is_empty() {
            return Err(format!(
                "call child workflow `{}` directly from the root workflow",
                child_script.meta.name
            ));
        }
        let script_sha256 = sha256(&child_script.source);
        let child = FrozenWorkflowChild {
            reference: reference.clone(),
            script: child_script,
            origin,
            shadows_existing,
            script_sha256,
        };
        children.insert(reference.clone(), child);
    }
    FrozenWorkflowComposition::from_children(script, children)
}

pub(crate) async fn persist_workflow_composition(
    composition: &FrozenWorkflowComposition,
    children_dir: &AbsolutePathBuf,
) -> Result<PersistedWorkflowComposition, String> {
    if !composition.children.is_empty() {
        tokio::fs::create_dir_all(children_dir)
            .await
            .map_err(|error| error.to_string())?;
    }
    let mut persisted = Vec::with_capacity(composition.children.len());
    for child in composition.children.values() {
        let artifact_file = artifact_file_name(&child.script_sha256);
        let artifact_path = children_dir.join(&artifact_file);
        let artifact_matches = match tokio::fs::read(&artifact_path).await {
            Ok(existing) => existing == child.script.source.as_bytes(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        if !artifact_matches {
            let artifact_path = artifact_path.to_path_buf();
            let source = child.script.source.clone();
            tokio::task::spawn_blocking(move || {
                codex_utils_path::write_atomically(&artifact_path, &source)
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        }
        persisted.push(PersistedWorkflowChild {
            reference: child.reference.clone(),
            origin: child.origin.clone(),
            shadows_existing: child.shadows_existing,
            artifact_file,
            script_sha256: child.script_sha256.clone(),
        });
    }
    Ok(PersistedWorkflowComposition {
        definition_sha256: composition.definition_sha256.clone(),
        children: persisted,
    })
}

pub(crate) async fn restore_workflow_composition(
    script: &ValidatedWorkflowScript,
    persisted: &PersistedWorkflowComposition,
    children_dir: &AbsolutePathBuf,
) -> Result<FrozenWorkflowComposition, String> {
    if persisted.children.len() != script.child_references.len() {
        return Err(
            "persisted child workflow manifest does not match the analyzed binding count"
                .to_string(),
        );
    }
    if !is_sha256(&persisted.definition_sha256) {
        return Err("persisted child workflow composition hash is invalid".to_string());
    }
    let mut children = BTreeMap::new();
    for child in &persisted.children {
        if !is_sha256(&child.script_sha256) {
            return Err("persisted child workflow artifact hash is invalid".to_string());
        }
        if child.artifact_file != artifact_file_name(&child.script_sha256) {
            return Err("persisted child workflow artifact path is invalid".to_string());
        }
        let origin_matches_binding = match &child.reference {
            WorkflowChildReference::Name { .. } => match &child.origin {
                WorkflowOrigin::Inline => false,
                WorkflowOrigin::Bundled
                | WorkflowOrigin::File { .. }
                | WorkflowOrigin::Plugin { .. } => true,
            },
            WorkflowChildReference::ScriptPath { .. } => {
                matches!(&child.origin, WorkflowOrigin::File { .. }) && !child.shadows_existing
            }
        };
        if !origin_matches_binding {
            return Err(
                "persisted child workflow origin is incompatible with its binding".to_string(),
            );
        }
        let source = read_bounded_source(&children_dir.join(&child.artifact_file)).await?;
        if sha256(&source) != child.script_sha256 {
            return Err(format!(
                "persisted child workflow artifact {} failed SHA-256 verification",
                child.artifact_file
            ));
        }
        let child_script = validate_workflow_script(source).map_err(|error| error.to_string())?;
        if !child_script.child_references.is_empty() {
            return Err("persist child workflow calls only in the root workflow".to_string());
        }
        let frozen = FrozenWorkflowChild {
            reference: child.reference.clone(),
            script: child_script,
            origin: child.origin.clone(),
            shadows_existing: child.shadows_existing,
            script_sha256: child.script_sha256.clone(),
        };
        if children.insert(child.reference.clone(), frozen).is_some() {
            return Err(
                "persisted child workflow manifest contains duplicate bindings".to_string(),
            );
        }
    }
    let composition = FrozenWorkflowComposition::from_children(script, children)?;
    if composition.definition_sha256 != persisted.definition_sha256 {
        return Err(
            "persisted child workflow composition hash does not match its manifest".to_string(),
        );
    }
    Ok(composition)
}

fn composition_sha256<'a>(
    script: &ValidatedWorkflowScript,
    children: impl Iterator<Item = &'a FrozenWorkflowChild>,
) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, b"codex-workflow-composition-v1");
    digest_field(&mut digest, script.source.as_bytes());
    for child in children {
        match &child.reference {
            WorkflowChildReference::Name { name } => {
                digest_field(&mut digest, b"name");
                digest_field(&mut digest, name.as_bytes());
            }
            WorkflowChildReference::ScriptPath { script_path } => {
                digest_field(&mut digest, b"scriptPath");
                digest_field(&mut digest, script_path.as_bytes());
            }
        }
        match &child.origin {
            WorkflowOrigin::Inline => digest_field(&mut digest, b"inline"),
            WorkflowOrigin::Bundled => digest_field(&mut digest, b"bundled"),
            WorkflowOrigin::File { path } => {
                digest_field(&mut digest, b"file");
                digest_field(&mut digest, path.as_os_str().as_encoded_bytes());
            }
            WorkflowOrigin::Plugin { namespace, path } => {
                digest_field(&mut digest, b"plugin");
                digest_field(&mut digest, namespace.as_bytes());
                digest_field(&mut digest, path.as_os_str().as_encoded_bytes());
            }
        }
        digest_field(&mut digest, &[u8::from(child.shadows_existing)]);
        digest_field(&mut digest, child.script_sha256.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(value);
}

fn artifact_file_name(script_sha256: &str) -> String {
    format!("{script_sha256}.js")
}

async fn read_bounded_source(path: &AbsolutePathBuf) -> Result<String, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(MAX_WORKFLOW_SCRIPT_BYTES + 1);
    file.take((MAX_WORKFLOW_SCRIPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_WORKFLOW_SCRIPT_BYTES {
        return Err("persist a focused child workflow artifact".to_string());
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
