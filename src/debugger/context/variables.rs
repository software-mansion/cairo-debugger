use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::slice;

use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_annotations::annotations::debugger::{
    DebuggerAnnotationsV2 as FunctionsDebugInfo, SierraFunctionId, SierraVarId,
};
use cairo_lang_sierra::ids::{FunctionId, VarId};
use cairo_lang_sierra::program::{GenBranchTarget, Program, Statement, StatementIdx};
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, StatementKindDebugInfo};
use cairo_lang_sierra_to_casm::references::{ReferenceExpression, build_function_parameters_refs};
use cairo_lang_sierra_type_size::TypeSizeMap;

use crate::debugger::context::sierra_function_for_statement;

pub struct CairoVarToCasmMaps {
    pub function_params: HashMap<FunctionId, HashMap<CairoVarId, CairoVarReference>>,
    pub local_vars: HashMap<StatementIdx, CairoVarsInStatement>,
}

pub struct CairoVarsInStatement {
    #[expect(dead_code)]
    /// Variables consumed by the sierra statement.
    pub consumed: HashMap<CairoVarId, CairoVarReference>,

    /// Variables produced when entering branches of the sierra statement.
    pub produced: HashMap<GenBranchTargetHashable, HashMap<CairoVarId, CairoVarReference>>,
}

impl Debug for CairoVarsInStatement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CairoVarsInStatement").field("produced", &self.produced).finish()
    }
}

/// Unique identifier of a Cairo variable.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CairoVarId {
    pub name: String,
    pub definition_span: SourceCodeSpan,
}

/// Sierra and CASM references to a Cairo variable.
#[derive(Clone, Debug)]
pub struct CairoVarReference {
    pub sierra_id: VarId,
    pub ref_expr: ReferenceExpression,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum GenBranchTargetHashable {
    /// Continues the run to the next statement.
    Fallthrough,
    /// Continues the run to the provided statement.
    Statement(StatementIdx),
}

impl From<GenBranchTarget<StatementIdx>> for GenBranchTargetHashable {
    fn from(value: GenBranchTarget<StatementIdx>) -> Self {
        match value {
            GenBranchTarget::Fallthrough => GenBranchTargetHashable::Fallthrough,
            GenBranchTarget::Statement(idx) => GenBranchTargetHashable::Statement(idx),
        }
    }
}

pub fn build_cairo_var_to_casm_maps(
    program: &Program,
    casm_debug_info: &CairoProgramDebugInfo,
    functions_debug_info: &FunctionsDebugInfo,
    type_sizes: &TypeSizeMap,
) -> CairoVarToCasmMaps {
    let mut local_vars = HashMap::new();

    let mut functions_sierra_to_cairo_vars_maps: HashMap<_, HashMap<_, slice::Iter<_>>> =
        functions_debug_info
            .functions_info
            .iter()
            .map(|(function_id, func_debug_info)| {
                let sierra_to_cairo_var_map: HashMap<_, _> = func_debug_info
                    .sierra_to_cairo_variables
                    .iter()
                    .map(|(sierra_id, cairo_vars)| (sierra_id, cairo_vars.iter()))
                    .collect();
                (function_id, sierra_to_cairo_var_map)
            })
            .collect();

    let function_params: HashMap<_, HashMap<_, _>> = program
        .funcs
        .iter()
        .filter_map(|function| {
            let Some(func_sierra_to_cairo_var_map) =
                functions_sierra_to_cairo_vars_maps.get_mut(&SierraFunctionId(function.id.id))
            else {
                // TODO: fix in the compiler
                eprintln!(
                    "function {} should be present in the variable map",
                    function.id.debug_name.as_deref().unwrap_or_default()
                );
                return None;
            };

            let param_refs = build_function_parameters_refs(function, type_sizes)
                .expect("function param refs construction should not fail");

            let cairo_var_map = param_refs
                .into_iter()
                .filter_map(|(sierra_id, ref_value)| {
                    let (name, definition_span) = func_sierra_to_cairo_var_map
                        .get_mut(&SierraVarId(sierra_id.id))?
                        .next()?
                        .clone();

                    let cairo_var_id = CairoVarId { name, definition_span };

                    let cairo_var_ref =
                        CairoVarReference { sierra_id, ref_expr: ref_value.expression.clone() };

                    Some((cairo_var_id, cairo_var_ref))
                })
                .collect();

            Some((function.id.clone(), cairo_var_map))
        })
        .collect();

    for (idx, statement_debug_info) in casm_debug_info.sierra_statement_info.iter().enumerate() {
        // Join information from casm debug info and sierra program to get casm reference for each
        // sierra var id. This is implemented as collecting vectors of `CairoVarReference`.
        let (consumed, produced) =
            match (&program.statements[idx], &statement_debug_info.additional_kind_info) {
                (
                    Statement::Invocation(invocation),
                    StatementKindDebugInfo::Invoke(invocation_debug),
                ) => {
                    let refs = invocation_debug
                        .ref_values
                        .iter()
                        .cloned()
                        .map(|ref_val| ref_val.expression);

                    let consumed = invocation
                        .args
                        .iter()
                        .cloned()
                        .zip(refs)
                        .map(|(sierra_id, ref_expr)| CairoVarReference { sierra_id, ref_expr })
                        .collect();

                    let produced: HashMap<_, _> = invocation
                        .branches
                        .iter()
                        .zip(invocation_debug.result_branch_changes.iter())
                        .map(|(gen_branch_info, branch_changes)| {
                            let refs = branch_changes
                                .refs
                                .iter()
                                .map(|ref_val| ref_val.expression.clone());
                            let cairo_var_refs = gen_branch_info
                                .results
                                .iter()
                                .cloned()
                                .zip(refs)
                                .map(|(sierra_id, ref_expr)| CairoVarReference {
                                    sierra_id,
                                    ref_expr,
                                })
                                .collect();

                            (gen_branch_info.target.into(), cairo_var_refs)
                        })
                        .collect();

                    (consumed, produced)
                }
                (Statement::Return(_), StatementKindDebugInfo::Return(_)) => {
                    // TODO(#91)
                    Default::default()
                }
                _ => unreachable!(),
            };

        let statement_idx = StatementIdx(idx);
        let function_id = &sierra_function_for_statement(idx, program).id;

        let Some(func_sierra_to_cairo_var_map) =
            functions_sierra_to_cairo_vars_maps.get_mut(&SierraFunctionId(function_id.id))
        else {
            // TODO: fix in the compiler
            continue;
        };

        let consumed = extract_cairo_var_map(consumed, func_sierra_to_cairo_var_map);

        let produced: HashMap<_, _> = produced
            .into_iter()
            .map(|(branch_target, cairo_var_refs)| {
                let produced_in_branch =
                    extract_cairo_var_map(cairo_var_refs, func_sierra_to_cairo_var_map);
                (branch_target, produced_in_branch)
            })
            .collect();

        if !consumed.is_empty() || !produced.is_empty() {
            local_vars.insert(statement_idx, CairoVarsInStatement { consumed, produced });
        }
    }

    CairoVarToCasmMaps { function_params, local_vars }
}

/// For each var reference use its Sierra var id to get the Cairo variable it corresponds to.
fn extract_cairo_var_map(
    var_refs: Vec<CairoVarReference>,
    func_sierra_to_cairo_vars_map: &mut HashMap<
        &SierraVarId,
        slice::Iter<(String, SourceCodeSpan)>,
    >,
) -> HashMap<CairoVarId, CairoVarReference> {
    var_refs
        .into_iter()
        .filter_map(|var_ref| {
            let (name, definition_span) = func_sierra_to_cairo_vars_map
                .get_mut(&SierraVarId(var_ref.sierra_id.id))?
                .next()?
                .clone();

            let var_id = CairoVarId { name, definition_span };

            Some((var_id, var_ref))
        })
        .collect()
}
