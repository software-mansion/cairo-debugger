use std::collections::HashMap;

use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_lang_casm::cell_expression::CellExpression;
use cairo_lang_casm::operand::{CellRef, Register};
use cairo_lang_sierra::ids::VarId;
use cairo_lang_sierra::program::StatementIdx;
use cairo_vm::Felt252;
use cairo_vm::types::relocatable::{MaybeRelocatable, Relocatable};
use cairo_vm::vm::vm_core::VirtualMachine;
use starknet_types_core::felt::Felt;
use tracing::{error, trace, warn};

use crate::debugger::context::{CairoVarId, CairoVarReference, Context};
use crate::debugger::state::call_stack::PostStatementsRegisters;

pub fn get_values_of_variables(
    ctx: &Context,
    current_statement_idx: StatementIdx,
    vm: &VirtualMachine,
    post_statements_registers: &PostStatementsRegisters,
) -> HashMap<String, String> {
    let function_entrypoint = &ctx.sierra_function_for_statement(current_statement_idx).entry_point;

    eprintln!("{post_statements_registers:?}");
    let mut current_var_values: HashMap<String, (SourceCodeSpan, VarId, Vec<Felt252>)> =
        HashMap::new();

    #[cfg(feature = "dev")]
    ctx.print_statement(current_statement_idx);

    // TODO: check if the statement was executed at all - e.g. if we are in the same branch...
    //  We should probably use a trace of statement indexes.
    for idx in function_entrypoint.0..current_statement_idx.0 {
        let idx = StatementIdx(idx);
        let Some(variables) = ctx.cairo_var_map.get(&idx) else { continue };

        // Some statements don't compile to any CASM instructions, so they won't
        // appear in the statement trace map (since they cannot be executed).
        // To prevent them from being ignored in this logic, for each statement
        // we find the first statement
        // from the set `{this_statement} + statements_after_this_statement`
        // that compiles to non-zero CASM instructions.
        eprintln!("OG STATEMENT: {idx:?}");
        let first_statement_compiling_to_sth =
            ctx.statement_idx_for_pc(ctx.casm_offsets.statement_to_pc[idx.0]);
        eprintln!(
            "FIRST STATEMENT COMPILING TO NON-EMPTY CASM : {first_statement_compiling_to_sth:?}"
        );

        let (iterator, registers_values) = if first_statement_compiling_to_sth != idx {
            let iterator: Vec<_> = variables.produced.values().flatten().collect();
            (
                iterator,
                // TODO: This is bad, fix for ifs.
                post_statements_registers
                    .get(&first_statement_compiling_to_sth)
                    .map(|x| &x.1)
                    .cloned()
                    .unwrap_or_else(|| RegistersValues {
                        ap: vm.get_ap().offset,
                        fp: vm.get_fp().offset,
                    }),
            )
        } else {
            let Some((branch_target, registers_values)) = post_statements_registers.get(&idx)
            else {
                // We may have not executed a statement due to branching. Skip such a statement.
                continue;
            };

            let iterator: Vec<_> = variables
                .produced
                .get(&branch_target.clone().into())
                .into_iter()
                .flatten()
                .collect();
            (iterator, registers_values.clone())
        };

        for (
            CairoVarId { name, definition_span: span },
            CairoVarReference { sierra_id: var_id, ref_expr },
        ) in iterator
        {
            let cells_vals: Vec<_> = ref_expr
                .cells
                .iter()
                .filter_map(|cell| maybe_extract_felt_from_cell(cell, &registers_values, vm))
                .collect();

            eprintln!("VALUE OF VAR: {cells_vals:?}");

            if let Some((curr_span, _, _)) = current_var_values.get(name) {
                // If there is a var with the same name in the map already,
                // and it is further in the code, ignore the current var.
                if span.start.line < curr_span.start.line
                    || (span.start.line == curr_span.start.line
                        && span.start.col < curr_span.start.col)
                {
                    continue;
                }
            }

            if cells_vals.is_empty() {
                continue;
            }

            current_var_values.insert(name.clone(), (span.clone(), var_id.clone(), cells_vals));
        }

        // TODO: mark values as dropped.
        //  Right now we don't care, since we only support simple numeric values which are all copy.
        // // Marks consumed vars as invalid in case of an invocation.
        // match &ctx.program.statements[idx] {
        //     Statement::Invocation(invocation) => {
        //         match ctx.sierra_program_registry.get_libfunc(&invocation.libfunc_id).unwrap()
        //         {
        //             // Ignore `drop` since it does not consume the var at Cairo level.
        //             CoreConcreteLibfunc::Drop(_) => {}
        //             _ => {
        //                 let dummy = Vec::new();
        //                 let vars_to_preserve = if let [
        //                     GenBranchInfo { target: GenBranchTarget::Fallthrough, results },
        //                 ] = invocation.branches.as_slice()
        //                 {
        //                     results
        //                 } else {
        //                     &dummy
        //                 };
        //
        //                 for var_id in
        //                     invocation.args.iter().filter(|var| !vars_to_preserve.contains(var))
        //                 {
        //                     if let Some(name_to_remove) = current_var_values
        //                         .iter()
        //                         .find(|(_, (_, id, _))| id.id == var_id.id)
        //                         .map(|(name, _)| name.clone())
        //                     {
        //                         current_var_values.remove(&name_to_remove);
        //                     }
        //                 }
        //             }
        //         }
        //     }
        //     Statement::Return(_) => {}
        // }
    }

    eprintln!();

    current_var_values
        .into_iter()
        .filter_map(|(name, (loc, var_id, value_in_felts))| {
            if value_in_felts.len() == 1 {
                Some((name, value_in_felts[0].to_string()))
            } else {
                warn!("unsupported value: ({name}, {loc:?}) {var_id:?} {value_in_felts:?}");
                None
            }
        })
        .collect()
}

fn maybe_extract_felt_from_cell(
    cell: &CellExpression,
    registers_values: &RegistersValues,
    vm: &VirtualMachine,
) -> Option<Felt> {
    match cell {
        CellExpression::Deref(cell_ref) => {
            let relocatable = registers_values.relocatable_from_cell_ref(cell_ref);

            eprintln!("RELOCATABLE: {relocatable:?}");

            match vm.segments.memory.get_maybe_relocatable(relocatable) {
                Ok(MaybeRelocatable::Int(value)) => Some(value),
                Ok(MaybeRelocatable::RelocatableValue(relocatable)) => {
                    error!("unexpected relocatable (maybe an array): {relocatable:?}");
                    None
                }
                Err(err) => {
                    error!("error when extracting maybe relocatable from VM: {err:?}");
                    None
                }
            }
        }
        CellExpression::DoubleDeref(..) => {
            // TODO
            trace!("DOUBLE Ds");
            None
        }
        CellExpression::Immediate(value) => Some(Felt::from(value)),
        CellExpression::BinOp { .. } => {
            // TODO
            trace!("BINOP");
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct RegistersValues {
    pub ap: usize,
    pub fp: usize,
}

impl RegistersValues {
    fn relocatable_from_cell_ref(&self, cell_ref: &CellRef) -> Relocatable {
        let original_offset = match cell_ref.register {
            Register::AP => self.ap,
            Register::FP => self.fp,
        };
        // TODO: can we always unwrap here?
        let offset = (original_offset as isize + cell_ref.offset as isize).try_into().unwrap();

        // Segment index is always one for ap and fp.
        Relocatable { segment_index: 1, offset }
    }
}
