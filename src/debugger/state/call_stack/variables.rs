use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use cairo_lang_casm::operand::{CellRef, DerefOrImmediate};
use cairo_lang_sierra::extensions::core::CoreTypeConcrete;
use cairo_lang_sierra::extensions::modules::starknet::StarknetTypeConcrete;
use cairo_lang_sierra::ids::ConcreteTypeId;
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

#[derive(Clone, Debug, PartialEq)]
pub enum CairoValue {
    Bool(bool),
    FeltLike(Felt252),
    Struct { type_name: String, fields: Vec<(String, CairoValue)> },
    Enum { type_name: String, variant_name: String, variant_value: Box<CairoValue> },
    Tuple(Vec<CairoValue>),
    Snapshot(Box<CairoValue>),
    NonZero(Box<CairoValue>),
    Other(String),
}

pub fn get_values_of_variables(
    ctx: &Context,
    vm: &VirtualMachine,
    post_statements_registers: &PostStatementsRegisters,
) -> FunctionVariables {
    let mut current_var_values: IndexMap<String, (SourceCodeSpan, ConcreteTypeId, Vec<Felt252>)> =
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

            // Skip if there was an error while extracting felts since it makes us unable to rely on
            // type sizes (which we have to rely on).
            if cells_vals.len() != ref_expr.cells.len() {
                continue;
            }

            if let Some((curr_span, _, curr_cells)) = current_var_values.get(name) {
                // If there is a var with the same name in the map already,
                // and it is further in the code, ignore the current var.
                if span.start.line < curr_span.start.line
                    || (span.start.line == curr_span.start.line
                        && span.start.col < curr_span.start.col)
                {
                    continue;
                }
                // TODO(#136)
                // The same definition span but fewer cells: a struct_deconstruct intermediate field
                // variable can be mapped back to the struct's Cairo name in debugger mappings,
                // which would degrade a complete multi-felt struct to just its first field.
                if span == curr_span && cells_vals.len() < curr_cells.len() {
                    continue;
                }
            }

            // TODO(#128): fix unit type mappings
            if cells_vals.is_empty() {
                continue;
            }

            current_var_values.insert(name.clone(), (span.clone(), type_id, cells_vals));
        }

        // TODO(#99): drop consumed values.
    }

    let names_to_values = current_var_values
        .into_iter()
        .map(|(name, (_, type_id, felts))| {
            let value = felts_to_cairo_value(&felts, &type_id, ctx);
            (name, value)
        })
        .collect();

    FunctionVariables { names_to_values }
}

fn felts_to_cairo_value(felts: &[Felt252], type_id: &ConcreteTypeId, ctx: &Context) -> CairoValue {
    let Some(concrete_type) = ctx.get_concrete_type(type_id) else {
        return fallback_value(felts, type_id);
    };

    match concrete_type {
        CoreTypeConcrete::Struct(struct_type) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;

            if is_tuple(type_long_id) {
                let fields =
                    collect_member_fields(&struct_type.members, felts, ctx, |_| String::new())
                        .into_iter()
                        .map(|(_, v)| v)
                        .collect();
                CairoValue::Tuple(fields)
            } else {
                let struct_info = ctx.struct_info(type_id);
                let type_name = struct_info
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| extract_short_type_name(type_id));
                let fields = collect_member_fields(&struct_type.members, felts, ctx, |i| {
                    struct_info
                        .and_then(|info| info.members.get(i))
                        .cloned()
                        .unwrap_or_else(|| format!(".{i}"))
                });
                CairoValue::Struct { type_name, fields }
            }
        }
        CoreTypeConcrete::Enum(enum_type) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;
            if is_bool(type_long_id) {
                let stored_discriminant: usize = felts[0]
                    .as_ref()
                    .to_biguint()
                    .try_into()
                    .expect("bool discriminant larger than usize::MAX");
                return CairoValue::Bool(stored_discriminant != 0);
            }

            let enum_info = ctx.enum_info(type_id);
            let type_name = enum_info
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id));
            let n_variants = enum_type.variants.len();
            let stored_discriminant: usize = felts[0]
                .as_ref()
                .to_biguint()
                .try_into()
                .expect("enum discriminant larger than usize::MAX");
            // For n <= 2 variants: stored value == variant index directly.
            // For n > 2 variants: Sierra stores a jump-table offset: stored = 2 * (n - index) - 1,
            // so index = n - (stored + 1) / 2.
            let variant_index = if n_variants <= 2 {
                stored_discriminant
            } else {
                n_variants.saturating_sub(stored_discriminant.div_ceil(2))
            };

            let variant_id = &enum_type.variants[variant_index];
            let variant_size = ctx.type_size(variant_id);
            // Sierra pads enum payloads at the START: [discriminant | padding | variant_data].
            // The variant's data occupies the last `variant_size` felts of the payload.
            let payload = &felts[1..];
            let variant_felts = &payload[payload.len() - variant_size..];
            let variant_value = felts_to_cairo_value(variant_felts, variant_id, ctx);
            let variant_name = enum_info
                .and_then(|info| info.variants.get(variant_index))
                .cloned()
                .unwrap_or_else(|| format!("variant_{variant_index}"));
            CairoValue::Enum { type_name, variant_name, variant_value: Box::new(variant_value) }
        }
        CoreTypeConcrete::NonZero(inner) => {
            CairoValue::NonZero(felts_to_cairo_value(felts, &inner.ty, ctx).into())
        }
        CoreTypeConcrete::Snapshot(inner) => {
            CairoValue::Snapshot(felts_to_cairo_value(felts, &inner.ty, ctx).into())
        }
        CoreTypeConcrete::Starknet(starknet_type) => {
            let type_name = match starknet_type {
                StarknetTypeConcrete::ContractAddress(_) => Some("ContractAddress"),
                StarknetTypeConcrete::ClassHash(_) => Some("ClassHash"),
                StarknetTypeConcrete::StorageAddress(_) => Some("StorageAddress"),
                StarknetTypeConcrete::StorageBaseAddress(_) => Some("StorageBaseAddress"),
                _ => None,
            };
            match type_name {
                Some(name) => CairoValue::Other(format!("{name}(0x{:x})", felts[0].to_biguint())),
                None => fallback_value(felts, type_id),
            }
        }
        _ => fallback_value(felts, type_id),
    }
}

fn fallback_value(felts: &[Felt252], type_id: &ConcreteTypeId) -> CairoValue {
    if felts.len() == 1 {
        CairoValue::FeltLike(felts[0])
    } else {
        warn!("unhandled multi-felt value for type {type_id:?}: {felts:?}");
        let joined = felts.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(", ");
        CairoValue::Other(format!("[{joined}]"))
    }
}

fn extract_short_type_name(type_id: &ConcreteTypeId) -> String {
    if let Some(debug_name) = &type_id.debug_name {
        // Strip generic parameters and take the last path segment.
        let base = debug_name.split('<').next().unwrap_or(debug_name.as_str());
        return base.split("::").last().unwrap_or(base).to_string();
    }
    format!("type_{}", type_id.id)
}

fn collect_member_fields<F>(
    members: &[ConcreteTypeId],
    felts: &[Felt252],
    ctx: &Context,
    name_fn: F,
) -> Vec<(String, CairoValue)>
where
    F: Fn(usize) -> String,
{
    let mut offset = 0;
    members
        .iter()
        .enumerate()
        .map(|(i, concrete_type_id)| {
            let size = ctx.type_size(concrete_type_id);
            let member_felts = &felts[offset..offset + size];
            offset += size;
            let value = felts_to_cairo_value(member_felts, concrete_type_id, ctx);
            (name_fn(i), value)
        })
        .collect()
}

fn is_tuple(type_long_id: &ConcreteTypeLongId) -> bool {
    type_long_id.generic_id.0 == "Struct"
        && matches!(
            type_long_id.generic_args.first(),
            Some(GenericArg::UserType(user_type))
                if user_type.debug_name.as_ref().is_some_and(|n| n.as_str() == "Tuple")
        )
}

fn is_bool(type_long_id: &ConcreteTypeLongId) -> bool {
    type_long_id.generic_id.0 == "Enum"
        && matches!(
            type_long_id.generic_args.first(),
            Some(GenericArg::UserType(user_type))
                if user_type.debug_name.as_ref().is_some_and(|n| n.as_str() == "core::bool")
        )
}

fn is_panic_result(type_long_id: &ConcreteTypeLongId) -> bool {
    if type_long_id.generic_id.0 == "Enum"
        && let GenericArg::UserType(user_type) = &type_long_id.generic_args[0]
        && user_type.debug_name.as_ref().is_some_and(|n| n.starts_with("core::panics::PanicResult"))
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
