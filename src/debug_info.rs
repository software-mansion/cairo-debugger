use std::collections::HashMap;

use anyhow::{Context as _, Result};
use cairo_annotations::annotations::TryFromDebugInfo;
use cairo_annotations::annotations::coverage::{SourceCodeSpan, SourceFileFullPath};
use cairo_annotations::annotations::debugger::{
    ParameterInfo, SierraFunctionId, SierraVarId, VersionedDebuggerAnnotations,
};
use cairo_lang_sierra::debug_info::DebugInfo;

pub struct FunctionsDebugInfo {
    pub functions_info: HashMap<SierraFunctionId, FunctionDebugInfo>,
}

pub struct FunctionDebugInfo {
    #[expect(dead_code)]
    pub function_file_path: SourceFileFullPath,
    #[expect(dead_code)]
    pub function_code_span: SourceCodeSpan,
    /// All Cairo bindings the compiler observed for each Sierra variable, in observation
    /// order.
    pub sierra_to_cairo_variables: HashMap<SierraVarId, Vec<(String, SourceCodeSpan)>>,
    /// `None` for V1 sources (V1 has no explicit params field). `Some` for V2 sources,
    /// in which case it carries the authoritative parameter names — prefer it over probing
    /// `sierra_to_cairo_variables`. May be `Some(vec![])` for a V2 function with no params.
    pub parameters: Option<Vec<ParameterInfo>>,
}

impl FunctionsDebugInfo {
    pub fn try_from_debug_info(debug_info: &DebugInfo) -> Result<Self> {
        let versioned = VersionedDebuggerAnnotations::try_from_debug_info(debug_info)
            .context("functions debug info is missing or malformed - enable generating it in your Scarb.toml")?;
        Ok(versioned.into())
    }
}

impl From<VersionedDebuggerAnnotations> for FunctionsDebugInfo {
    fn from(versioned: VersionedDebuggerAnnotations) -> Self {
        match versioned {
            VersionedDebuggerAnnotations::V1(v1) => {
                eprintln!("Using V1 annotations");
                Self {
                    functions_info: v1
                        .functions_info
                        .into_iter()
                        .map(|(id, f)| {
                            (
                                id,
                                FunctionDebugInfo {
                                    function_file_path: f.function_file_path,
                                    function_code_span: f.function_code_span,
                                    sierra_to_cairo_variables: f
                                        .sierra_to_cairo_variable
                                        .into_iter()
                                        .map(|(k, v)| (k, vec![v]))
                                        .collect(),
                                    parameters: None,
                                },
                            )
                        })
                        .collect(),
                }
            }
            VersionedDebuggerAnnotations::V2(v2) => {
                eprintln!("Using V2 annotations");
                Self {
                    functions_info: v2
                        .functions_info
                        .into_iter()
                        .map(|(id, f)| {
                            (
                                id,
                                FunctionDebugInfo {
                                    function_file_path: f.function_file_path,
                                    function_code_span: f.function_code_span,
                                    sierra_to_cairo_variables: f.sierra_to_cairo_variables,
                                    parameters: Some(f.parameters),
                                },
                            )
                        })
                        .collect(),
                }
            }
        }
    }
}
