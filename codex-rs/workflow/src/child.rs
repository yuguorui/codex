use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkflowChildReference {
    Name { name: String },
    ScriptPath { script_path: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum WorkflowChildReferenceWire {
    Name(WorkflowChildNameWire),
    ScriptPath(WorkflowChildScriptPathWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowChildNameWire {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkflowChildScriptPathWire {
    script_path: String,
}

impl<'de> Deserialize<'de> for WorkflowChildReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkflowChildReferenceWire::deserialize(deserializer)?;
        Ok(match wire {
            WorkflowChildReferenceWire::Name(wire) => Self::Name { name: wire.name },
            WorkflowChildReferenceWire::ScriptPath(wire) => Self::ScriptPath {
                script_path: wire.script_path,
            },
        })
    }
}

impl WorkflowChildReference {
    pub fn from_runtime_value(value: &JsonValue) -> Result<Self, &'static str> {
        match value {
            JsonValue::String(name) => Ok(Self::Name { name: name.clone() }),
            JsonValue::Object(reference) if reference.len() == 1 => {
                if let Some(name) = reference.get("name").and_then(JsonValue::as_str) {
                    Ok(Self::Name {
                        name: name.to_string(),
                    })
                } else if let Some(script_path) =
                    reference.get("scriptPath").and_then(JsonValue::as_str)
                {
                    Ok(Self::ScriptPath {
                        script_path: script_path.to_string(),
                    })
                } else {
                    Err(child_reference_error())
                }
            }
            JsonValue::Object(_)
            | JsonValue::Array(_)
            | JsonValue::Bool(_)
            | JsonValue::Null
            | JsonValue::Number(_) => Err(child_reference_error()),
        }
    }
}

fn child_reference_error() -> &'static str {
    "workflow(nameOrRef) expects a frozen workflow name, {name}, or {scriptPath} reference"
}
