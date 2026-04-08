use std::collections::HashMap;
use std::fmt::{Debug, Formatter};

use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_annotations::annotations::debugger::{
    DebuggerAnnotationsV1 as FunctionsDebugInfo, FunctionDebugInfo, SierraFunctionId, SierraVarId,
};
use cairo_lang_sierra::ids::VarId;
use cairo_lang_sierra::program::{GenBranchTarget, Program, Statement, StatementIdx};
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, StatementKindDebugInfo};
use cairo_lang_sierra_to_casm::references::ReferenceExpression;

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
    functions_debug_info: FunctionsDebugInfo,
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
                let produced_in_branch = extract_cairo_var_map(cairo_var_refs, func_debug_info);
                (branch_target, produced_in_branch)
            })
            .collect();

        if !consumed.is_empty() || !produced.is_empty() {
            result.insert(StatementIdx(idx), CairoVarsInStatement { consumed, produced });
        }
    }

    result
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
