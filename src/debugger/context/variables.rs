use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};

use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_annotations::annotations::debugger::{SierraFunctionId, SierraVarId};
use cairo_lang_sierra::ids::{FunctionId, VarId};
use cairo_lang_sierra::program::{GenBranchTarget, Program, Statement, StatementIdx};
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, StatementKindDebugInfo};
use cairo_lang_sierra_to_casm::references::{ReferenceExpression, build_function_parameters_refs};
use cairo_lang_sierra_type_size::TypeSizeMap;

use crate::debug_info::{FunctionDebugInfo, FunctionsDebugInfo};
use crate::debugger::context::sierra_function_for_statement;

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
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct CairoVarId {
    pub name: String,
    pub definition_span: SourceCodeSpan,
}

/// Sierra and CASM references to a Cairo variable.
#[derive(Debug)]
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

pub fn build_cairo_var_to_casm_map(
    program: &Program,
    casm_debug_info: &CairoProgramDebugInfo,
    functions_debug_info: &FunctionsDebugInfo,
) -> HashMap<StatementIdx, CairoVarsInStatement> {
    let mut result = HashMap::new();
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

        let function_id = &sierra_function_for_statement(idx, program).id;
        let func_debug_info =
            &functions_debug_info.functions_info[&SierraFunctionId(function_id.id)];
        let param_sierra_ids: HashSet<u64> = func_debug_info
            .parameters
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|p| p.sierra_var_id.0)
            .collect();

        let consumed = extract_cairo_var_map(consumed, func_debug_info, &param_sierra_ids);
        let produced: HashMap<_, _> = produced
            .into_iter()
            .map(|(branch_target, cairo_var_refs)| {
                let produced_in_branch =
                    extract_cairo_var_map(cairo_var_refs, func_debug_info, &param_sierra_ids);
                (branch_target, produced_in_branch)
            })
            .collect();

        if !consumed.is_empty() || !produced.is_empty() {
            result.insert(StatementIdx(idx), CairoVarsInStatement { consumed, produced });
        }
    }

    result
}

/// Build the per-function map of param name -> FP-relative reference.
///
/// The references retrieved in this function are FP-only.
/// They are independent from further usage of corresponding Sierra variables in the function body --
/// if a param is used in the function body, it is either consumed or copied to a new, AP-based cell, effectively becoming a local variable.
/// We don't handle the second case here, since we have a dedicated piece of logic for local variables that can shadow the references returned from here.
///
/// # Contract
/// We rely on CairoVM's calling convention to get the initial, FP-relative offsets of function params
/// and return references via FP.
pub fn build_function_to_param_vars_map(
    program: &Program,
    type_sizes: &TypeSizeMap,
    functions_debug_info: &FunctionsDebugInfo,
) -> HashMap<FunctionId, HashMap<CairoVarId, CairoVarReference>> {
    program
        .funcs
        .iter()
        .map(|function| {
            let debug_info =
                &functions_debug_info.functions_info[&SierraFunctionId(function.id.id)];
            let param_refs = build_function_parameters_refs(function, type_sizes)
                .expect("function param refs construction should not fail");

            let cairo_var_map = if let Some(parameters) = &debug_info.parameters {
                parameters
                    .iter()
                    .filter_map(|param_info| {
                        let sierra_id = VarId::new(param_info.sierra_var_id.0);
                        let ref_value = param_refs.get(&sierra_id)?;
                        let cairo_var_id = CairoVarId {
                            name: param_info.name.clone(),
                            definition_span: param_info.definition_span.clone(),
                        };
                        let cairo_var_ref =
                            CairoVarReference { sierra_id, ref_expr: ref_value.expression.clone() };
                        Some((cairo_var_id, cairo_var_ref))
                    })
                    .collect()
            } else {
                let param_references = param_refs
                    .into_iter()
                    .map(|(sierra_id, ref_value)| CairoVarReference {
                        sierra_id,
                        ref_expr: ref_value.expression,
                    })
                    .collect();
                extract_cairo_var_map(param_references, debug_info, &HashSet::new())
            };

            (function.id.clone(), cairo_var_map)
        })
        .collect()
}

/// For each var reference, emit one (CairoVarId, CairoVarReference) entry per Cairo binding
/// observed for its Sierra var. Sierra ids in `param_sierra_ids` are skipped
/// — those entries are owned by the per-function `parameters` list and would otherwise show
/// up twice in the Variables view.
fn extract_cairo_var_map(
    var_refs: Vec<CairoVarReference>,
    func_debug_info: &FunctionDebugInfo,
    param_sierra_ids: &HashSet<u64>,
) -> HashMap<CairoVarId, CairoVarReference> {
    var_refs
        .into_iter()
        .flat_map(|var_ref| {
            let sierra_id = SierraVarId(var_ref.sierra_id.id);
            if param_sierra_ids.contains(&sierra_id.0) {
                return vec![];
            }
            let bindings = func_debug_info
                .sierra_to_cairo_variables
                .get(&sierra_id)
                .cloned()
                .unwrap_or_default();
            bindings
                .into_iter()
                .map(|(name, span)| {
                    let var_id = CairoVarId { name, definition_span: span };
                    let cloned_ref = CairoVarReference {
                        sierra_id: var_ref.sierra_id.clone(),
                        ref_expr: var_ref.ref_expr.clone(),
                    };
                    (var_id, cloned_ref)
                })
                .collect()
        })
        .collect()
}
