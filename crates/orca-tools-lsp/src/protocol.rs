//! Minimal LSP protocol types: only what the tools read (`LspProtocolTypes.cs`).

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Position {
    pub line: i64,
    pub character: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SymbolInformation {
    pub name: String,
    pub kind: i64,
    pub location: Location,
    pub container_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: i64,
    pub range: Range,
    pub selection_range: Range,
    pub children: Option<Vec<DocumentSymbol>>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: i64,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<i64>,
    /// A number or a string.
    pub code: Option<Value>,
    pub source: Option<String>,
    pub message: String,
}

impl Diagnostic {
    /// The code as text (`object.ToString()`): a string as is, anything else as JSON.
    pub fn code_text(&self) -> Option<String> {
        match self.code.as_ref()? {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// `SymbolKindNames.Get`.
pub fn symbol_kind_name(kind: i64) -> String {
    const NAMES: [&str; 27] = [
        "",
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "EnumMember",
        "Struct",
        "Event",
        "Operator",
        "TypeParameter",
    ];
    usize::try_from(kind)
        .ok()
        .and_then(|k| NAMES.get(k))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Kind({kind})"))
}

/// `DiagnosticSeverityNames.Get`.
pub fn severity_name(severity: Option<i64>) -> &'static str {
    match severity {
        Some(1) => "Error",
        Some(2) => "Warning",
        Some(3) => "Information",
        Some(4) => "Hint",
        _ => "Unknown",
    }
}

/// `GoToDefinitionTool.ParseLocations`: a `Location[]` or a single `Location`.
pub fn parse_locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Array(items) => items
            .iter()
            .filter_map(|i| serde_json::from_value(i.clone()).ok())
            .collect(),
        Value::Object(_) => serde_json::from_value(result.clone())
            .ok()
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}
