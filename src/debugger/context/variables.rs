use std::collections::HashMap;
use std::fmt::{Debug, Formatter};

use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_annotations::annotations::debugger::{
    DebuggerAnnotationsV1 as FunctionsDebugInfo, FunctionDebugInfo, SierraFunctionId, SierraVarId,
};
use cairo_lang_sierra::ids::{FunctionId, VarId};
use cairo_lang_sierra::program::{GenBranchTarget, Program, Statement, StatementIdx};
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, StatementKindDebugInfo};
use cairo_lang_sierra_to_casm::references::{ReferenceExpression, build_function_parameters_refs};
use cairo_lang_sierra_type_size::TypeSizeMap;

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

        let consumed = extract_cairo_var_map(consumed, func_debug_info);
        let produced: HashMap<_, _> = produced
            .into_iter()
            .map(|(branch_target, cairo_var_refs)| {
                let produced_in_branch =
                    extract_produced_cairo_var_map(cairo_var_refs, func_debug_info);
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
/// They are independent of further usage of corresponding Sierra variables in the function body -
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
                    .filter_map(|(id, var_definition)| {
                        let sierra_id = VarId::new(id.0);
                        let ref_expr = param_refs.get(&sierra_id)?.expression.clone();
                        let cairo_var_ref = CairoVarReference { sierra_id, ref_expr };
                        let cairo_var_id = CairoVarId {
                            name: var_definition.name.clone(),
                            definition_span: var_definition.span.clone(),
                        };
                        Some((cairo_var_id, cairo_var_ref))
                    })
                    .collect()
            } else {
                // Fallback when `parameters` field is not present in debug info (Scarb < 2.20.0).
                // We still know the `CairoVarReference` of the parameters - we try to find their
                // `CairoVarId` in `sierra_to_cairo_variable` by their sierra var id.
                //
                // Note that this fallback works just as good as using the `parameters` field
                // in the vast majority of cases. `parameters` works better for cases where
                // sierra var ids are reused between params and local variables, such as:
                //
                // fn foo(x: felt252) {
                //   assert(true, '');
                //   // `x` has var id `0` - since it is a first param
                //   // The line below compiles to `store_temp([0]) -> ([0])`
                //   // so `y` has var id `0` as well.
                //   // This will cause `0: y` entry to override `0: x` entry in
                //   // `sierra_to_cairo_variable`.
                //   let mut y = x;
                //   y += 1;
                // }
                //
                // This may lead to wrong variable names being shown for a while, since
                // the parameter name we get from `sierra_to_cairo_variable` is in fact the local
                // variable name. In the example above in mappings we will have sierra var id
                // 0 mapped to `y`, so we will wrongly assume that the `x` param is named `y`.
                // Assuming the function was called via `foo(5)`:
                //
                // fn foo(x: felt252) {
                //   assert(true, ''); // here `y = 5` will be shown instead of `x = 5`
                //   let mut y = x;
                //   y += 1;
                // }
                //
                // We are fine with such errors - we prefer being wrong sometimes over not showing
                // param values until their usage (after usage they become local vars and are shown
                // anyways) for older Scarb version.
                let param_references = param_refs
                    .into_iter()
                    .map(|(sierra_id, ref_value)| CairoVarReference {
                        sierra_id,
                        ref_expr: ref_value.expression,
                    })
                    .collect();
                extract_cairo_var_map(param_references, debug_info)
            };

            (function.id.clone(), cairo_var_map)
        })
        .collect()
}

fn extract_produced_cairo_var_map(
    var_refs: Vec<CairoVarReference>,
    func_debug_info: &FunctionDebugInfo,
) -> HashMap<CairoVarId, CairoVarReference> {
    // `add_assign` produces the updated `ref` value followed by its zero-sized `()` return value,
    // and debugger annotations may associate both results with the mutated Cairo variable. Drop
    // zero-sized results before collecting so they cannot replace data-carrying values.
    let var_refs =
        var_refs.into_iter().filter(|var_ref| !var_ref.ref_expr.cells.is_empty()).collect();
    extract_cairo_var_map(var_refs, func_debug_info)
}

/// For each var reference use its sierra var id to get the Cairo variable it corresponds to.
fn extract_cairo_var_map(
    var_refs: Vec<CairoVarReference>,
    func_debug_info: &FunctionDebugInfo,
) -> HashMap<CairoVarId, CairoVarReference> {
    var_refs
        .into_iter()
        .filter_map(|var_ref| {
            let (name, span) =
                func_debug_info.sierra_to_cairo_variable.get(&SierraVarId(var_ref.sierra_id.id))?;
            let var_id = CairoVarId { name: name.clone(), definition_span: span.clone() };

            Some((var_id, var_ref))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use cairo_annotations::annotations::coverage::{
        ColumnNumber, LineNumber, SourceCodeLocation, SourceFileFullPath,
    };
    use cairo_lang_casm::cell_expression::CellExpression;

    use super::*;

    #[test]
    fn unit_result_of_add_assign_does_not_replace_mutated_variable() {
        // `x += 5` produces both the updated `x` and the zero-sized `()` returned by
        // `add_assign`. Debug annotations may associate both Sierra variables with `x`.
        let location = SourceCodeLocation { line: LineNumber(1), col: ColumnNumber(0) };
        let span = SourceCodeSpan { start: location.clone(), end: location };
        let x = ("x".to_owned(), span.clone());
        let func_debug_info = FunctionDebugInfo {
            function_file_path: SourceFileFullPath("lib.cairo".to_owned()),
            function_code_span: span.clone(),
            sierra_to_cairo_variable: HashMap::from([
                (SierraVarId(0), x.clone()),
                (SierraVarId(1), x),
            ]),
            parameters: None,
        };
        let updated_x = CairoVarReference {
            sierra_id: VarId::new(0),
            ref_expr: ReferenceExpression { cells: vec![CellExpression::Immediate(5.into())] },
        };
        let unit_result = CairoVarReference {
            sierra_id: VarId::new(1),
            ref_expr: ReferenceExpression::zero_sized(),
        };

        let variables =
            extract_produced_cairo_var_map(vec![updated_x, unit_result], &func_debug_info);
        let x = CairoVarId { name: "x".to_owned(), definition_span: span };

        assert_eq!(variables[&x].sierra_id, VarId::new(0));
        assert_eq!(variables[&x].ref_expr.cells.len(), 1);
    }
}
