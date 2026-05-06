use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use cairo_lang_casm::operand::{CellRef, DerefOrImmediate};
use cairo_lang_sierra::ids::VarId;
use cairo_lang_sierra::program::{ConcreteTypeLongId, GenericArg};
use cairo_vm::Felt252;
use cairo_vm::vm::vm_core::VirtualMachine;
use indexmap::IndexMap;
use starknet_types_core::felt::{Felt, NonZeroFelt};
use tracing::{error, warn};

use crate::debugger::context::{CairoVarId, CairoVarReference, Context};
use crate::debugger::state::call_stack::{
    FunctionVariables, PostStatementsRegisters, RegistersValues,
};

pub fn get_values_of_variables(
    ctx: &Context,
    vm: &VirtualMachine,
    post_statements_registers: &PostStatementsRegisters,
) -> FunctionVariables {
    let mut current_var_values: IndexMap<String, (SourceCodeSpan, VarId, Vec<Felt252>)> =
        IndexMap::new();

    for (idx, (branch_target, registers_values)) in post_statements_registers {
        let Some(variables) = ctx.cairo_var_map.get(idx) else { continue };

        let Some(produced_vars) = variables.produced.get(&((*branch_target).into())) else {
            continue;
        };

        let (branch_signature, branch_results) =
            ctx.branch_signature_and_results(*idx, branch_target).expect(
                "return statement not expected - or we wouldn't be in this function frame already",
            );

        for (
            CairoVarId { name, definition_span: span },
            CairoVarReference { sierra_id: var_id, ref_expr },
        ) in produced_vars
        {
            let type_id = ctx.var_type_id(var_id, branch_signature, branch_results).clone();
            let type_long_id = &ctx.var_type_info(&type_id).long_id;

            if is_panic_result(type_long_id) {
                continue;
            }

            let cells_vals: Vec<_> = ref_expr
                .cells
                .iter()
                .filter_map(|cell| maybe_extract_felt_from_cell(cell, registers_values, vm))
                .collect();

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

        // TODO(#99): drop consumed values.
    }

    let names_to_values = current_var_values
        .into_iter()
        .filter_map(|(name, (loc, var_id, value_in_felts))| {
            if value_in_felts.len() == 1 {
                Some((name, value_in_felts[0].to_string()))
            } else {
                warn!("unsupported value: ({name}, {loc:?}) {var_id:?} {value_in_felts:?}");
                None
            }
        })
        .collect();

    FunctionVariables { names_to_values }
}

fn is_panic_result(type_long_id: &ConcreteTypeLongId) -> bool {
    if type_long_id.generic_id.0 == "Enum"
        && let GenericArg::UserType(user_type) = &type_long_id.generic_args[0]
        // `core::panics::PanicResult` always has a debug name for some reason.
        && user_type
        .debug_name
        .clone()
        .is_some_and(|x| x.starts_with("core::panics::PanicResult"))
    {
        true
    } else {
        false
    }
}

fn maybe_extract_felt_from_cell(
    cell: &CellExpression,
    registers_values: &RegistersValues,
    vm: &VirtualMachine,
) -> Option<Felt> {
    match cell {
        CellExpression::Deref(cell_ref) => {
            maybe_get_felt_from_cell_ref(cell_ref, registers_values, vm)
        }
        CellExpression::DoubleDeref(cell_ref, offset) => {
            let relocatable = registers_values.relocatable_from_cell_ref(cell_ref);

            // [cell_ref]
            let mut relocatable = match vm.segments.memory.get_relocatable(relocatable) {
                Ok(value) => Some(value),
                Err(err) => {
                    error!("error when extracting relocatable from VM: {err:?}");
                    None
                }
            }?;

            relocatable.offset = (relocatable.offset as isize + *offset as isize) as usize;
            // [[cell_ref] + offset]
            match vm.segments.memory.get_integer(relocatable) {
                Ok(value) => Some(*value),
                Err(err) => {
                    error!("error when extracting felt from VM: {err:?}");
                    None
                }
            }
        }
        CellExpression::Immediate(value) => Some(Felt::from(value)),
        CellExpression::BinOp { op, a, b } => {
            let a_felt = maybe_get_felt_from_cell_ref(a, registers_values, vm)?;
            let b_felt = match b {
                DerefOrImmediate::Deref(cell_ref) => {
                    maybe_get_felt_from_cell_ref(cell_ref, registers_values, vm)
                }
                DerefOrImmediate::Immediate(value) => Some(Felt::from(value.value.clone())),
            }?;

            Some(match op {
                CellOperator::Add => a_felt + b_felt,
                CellOperator::Sub => a_felt - b_felt,
                CellOperator::Mul => a_felt * b_felt,
                CellOperator::Div => a_felt.field_div(&NonZeroFelt::try_from(b_felt).unwrap()),
            })
        }
    }
}

fn maybe_get_felt_from_cell_ref(
    cell_ref: &CellRef,
    registers_values: &RegistersValues,
    vm: &VirtualMachine,
) -> Option<Felt> {
    let relocatable = registers_values.relocatable_from_cell_ref(cell_ref);
    match vm.segments.memory.get_integer(relocatable) {
        Ok(value) => Some(*value),
        Err(err) => {
            error!("error when extracting felt from VM: {err:?}");
            None
        }
    }
}
