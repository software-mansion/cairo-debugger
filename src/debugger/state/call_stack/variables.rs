use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use cairo_lang_casm::operand::{CellRef, DerefOrImmediate};
use cairo_lang_sierra::extensions::core::CoreTypeConcrete;
use cairo_lang_sierra::extensions::modules::starknet::StarknetTypeConcrete;
use cairo_lang_sierra::ids::ConcreteTypeId;
use cairo_lang_sierra::program::{ConcreteTypeLongId, GenericArg};
use cairo_vm::Felt252;
use cairo_vm::types::relocatable::{MaybeRelocatable, Relocatable};
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
    Array { element_type: String, elements: Vec<CairoValue> },
    Snapshot(Box<CairoValue>),
    NonZero(Box<CairoValue>),
    Other(String),
}

pub fn get_values_of_variables(
    ctx: &Context,
    vm: &VirtualMachine,
    post_statements_registers: &PostStatementsRegisters,
) -> FunctionVariables {
    let mut current_var_values: IndexMap<String, (SourceCodeSpan, usize, CairoValue)> =
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

            let num_cells = ref_expr.cells.len();

            // TODO(#128): fix unit type mappings
            if num_cells == 0 {
                continue;
            }

            let Some(value) =
                extract_var_value(&ref_expr.cells, &type_id, registers_values, vm, ctx)
            else {
                continue;
            };

            if let Some((curr_span, curr_num_cells, _)) = current_var_values.get(name) {
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
                if span == curr_span && num_cells < *curr_num_cells {
                    continue;
                }
            }

            current_var_values.insert(name.clone(), (span.clone(), num_cells, value));
        }

        // TODO(#99): drop consumed values.
    }

    let names_to_values =
        current_var_values.into_iter().map(|(name, (_, _, value))| (name, value)).collect();

    FunctionVariables { names_to_values }
}

fn extract_var_value(
    cells: &[CellExpression],
    type_id: &ConcreteTypeId,
    registers_values: &RegistersValues,
    vm: &VirtualMachine,
    ctx: &Context,
) -> Option<CairoValue> {
    let values = cells
        .iter()
        .map(|cell| maybe_extract_maybe_relocatable_from_cell(cell, registers_values, vm))
        .collect::<Option<Vec<_>>>()?;

    maybe_relocatables_to_cairo_value(&values, type_id, vm, ctx)
}

fn maybe_relocatables_to_cairo_value(
    values: &[MaybeRelocatable],
    type_id: &ConcreteTypeId,
    vm: &VirtualMachine,
    ctx: &Context,
) -> Option<CairoValue> {
    let Some(concrete_type) = ctx.get_concrete_type(type_id) else {
        return fallback_value(values, type_id);
    };

    match concrete_type {
        CoreTypeConcrete::Array(info) | CoreTypeConcrete::Span(info) => {
            let (start_ptr, end_ptr) = extract_array_pointers(values)?;
            extract_array_from_pointers(start_ptr, end_ptr, &info.ty, vm, ctx)
        }
        CoreTypeConcrete::Snapshot(inner) => {
            maybe_relocatables_to_cairo_value(values, &inner.ty, vm, ctx)
                .map(|v| CairoValue::Snapshot(Box::new(v)))
        }
        CoreTypeConcrete::NonZero(inner) => {
            maybe_relocatables_to_cairo_value(values, &inner.ty, vm, ctx)
                .map(|v| CairoValue::NonZero(Box::new(v)))
        }
        CoreTypeConcrete::Struct(struct_type) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;

            if is_tuple(type_long_id) {
                let mut offset = 0;
                let mut elements = Vec::with_capacity(struct_type.members.len());
                for member_type_id in &struct_type.members {
                    let size = ctx.type_size(member_type_id);
                    let member_values = &values[offset..offset + size];
                    offset += size;
                    elements.push(maybe_relocatables_to_cairo_value(
                        member_values,
                        member_type_id,
                        vm,
                        ctx,
                    )?);
                }
                return Some(CairoValue::Tuple(elements));
            }

            let struct_info = ctx.struct_info(type_id);
            let type_name = struct_info
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id));
            let mut offset = 0;
            let mut fields = Vec::with_capacity(struct_type.members.len());
            for (i, member_type_id) in struct_type.members.iter().enumerate() {
                let size = ctx.type_size(member_type_id);
                let member_values = &values[offset..offset + size];
                offset += size;
                let name = struct_info
                    .and_then(|info| info.members.get(i))
                    .cloned()
                    .unwrap_or_else(|| format!(".{i}"));
                let value =
                    maybe_relocatables_to_cairo_value(member_values, member_type_id, vm, ctx)?;
                fields.push((name, value));
            }
            Some(CairoValue::Struct { type_name, fields })
        }
        CoreTypeConcrete::Enum(enum_type) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;
            let Some(MaybeRelocatable::Int(discriminant_felt)) = values.first() else {
                warn!("expected felt for enum discriminant, got relocatable or empty slice");
                return None;
            };
            if is_bool(type_long_id) {
                let stored: usize = discriminant_felt
                    .as_ref()
                    .to_biguint()
                    .try_into()
                    .expect("bool discriminant larger than usize::MAX");
                return Some(CairoValue::Bool(stored != 0));
            }

            let stored_discriminant: usize = discriminant_felt
                .as_ref()
                .to_biguint()
                .try_into()
                .expect("enum discriminant larger than usize::MAX");
            let n_variants = enum_type.variants.len();
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
            // The variant's data occupies the last `variant_size` values of the payload.
            let payload = &values[1..];
            let variant_values = &payload[payload.len() - variant_size..];
            let variant_value =
                maybe_relocatables_to_cairo_value(variant_values, variant_id, vm, ctx)?;
            let enum_info = ctx.enum_info(type_id);
            let type_name = enum_info
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id));
            let variant_name = enum_info
                .and_then(|info| info.variants.get(variant_index))
                .cloned()
                .unwrap_or_else(|| format!("variant_{variant_index}"));
            Some(CairoValue::Enum {
                type_name,
                variant_name,
                variant_value: Box::new(variant_value),
            })
        }
        CoreTypeConcrete::Starknet(starknet_type) => {
            let type_name = match starknet_type {
                StarknetTypeConcrete::ContractAddress(_) => Some("ContractAddress"),
                StarknetTypeConcrete::ClassHash(_) => Some("ClassHash"),
                StarknetTypeConcrete::StorageAddress(_) => Some("StorageAddress"),
                StarknetTypeConcrete::StorageBaseAddress(_) => Some("StorageBaseAddress"),
                _ => None,
            };
            let MaybeRelocatable::Int(felt) = &values[0] else {
                unreachable!("starknet type expected to be a felt")
            };
            match type_name {
                Some(name) => Some(CairoValue::Other(format!("{name}(0x{:x})", felt.to_biguint()))),
                None => fallback_value(values, type_id),
            }
        }
        _ => fallback_value(values, type_id),
    }
}

fn fallback_value(values: &[MaybeRelocatable], type_id: &ConcreteTypeId) -> Option<CairoValue> {
    match values {
        [MaybeRelocatable::Int(f)] => Some(CairoValue::FeltLike(*f)),
        _ => {
            let joined = values
                .iter()
                .filter_map(|v| match v {
                    MaybeRelocatable::RelocatableValue(_) => None,
                    MaybeRelocatable::Int(f) => Some(f.to_string()),
                })
                .collect::<Vec<_>>()
                .join(", ");
            warn!("unhandled multi-felt value for type {type_id:?}");
            Some(CairoValue::Other(format!("[{joined}]")))
        }
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

fn format_type_name(type_id: &ConcreteTypeId, ctx: &Context) -> String {
    let Some(concrete_type) = ctx.get_concrete_type(type_id) else {
        return extract_short_type_name(type_id);
    };
    match concrete_type {
        CoreTypeConcrete::Array(info) => format!("Array<{}>", format_type_name(&info.ty, ctx)),
        CoreTypeConcrete::Span(info) => format!("Span<{}>", format_type_name(&info.ty, ctx)),
        CoreTypeConcrete::Snapshot(inner) => format!("@{}", format_type_name(&inner.ty, ctx)),
        CoreTypeConcrete::NonZero(inner) => {
            format!("NonZero<{}>", format_type_name(&inner.ty, ctx))
        }
        CoreTypeConcrete::Nullable(inner) => {
            format!("Nullable<{}>", format_type_name(&inner.ty, ctx))
        }
        CoreTypeConcrete::Felt252(_) => "felt252".to_string(),
        CoreTypeConcrete::Uint8(_) => "u8".to_string(),
        CoreTypeConcrete::Uint16(_) => "u16".to_string(),
        CoreTypeConcrete::Uint32(_) => "u32".to_string(),
        CoreTypeConcrete::Uint64(_) => "u64".to_string(),
        CoreTypeConcrete::Uint128(_) => "u128".to_string(),
        CoreTypeConcrete::Sint8(_) => "i8".to_string(),
        CoreTypeConcrete::Sint16(_) => "i16".to_string(),
        CoreTypeConcrete::Sint32(_) => "i32".to_string(),
        CoreTypeConcrete::Sint64(_) => "i64".to_string(),
        CoreTypeConcrete::Sint128(_) => "i128".to_string(),
        CoreTypeConcrete::Bytes31(_) => "bytes31".to_string(),
        CoreTypeConcrete::Struct(_) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;
            if is_tuple(type_long_id) {
                return "Tuple".to_string();
            }
            ctx.struct_info(type_id)
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id))
        }
        CoreTypeConcrete::Enum(_) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;
            if is_bool(type_long_id) {
                return "bool".to_string();
            }
            ctx.enum_info(type_id)
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id))
        }
        CoreTypeConcrete::Starknet(starknet_type) => match starknet_type {
            StarknetTypeConcrete::ContractAddress(_) => "ContractAddress".to_string(),
            StarknetTypeConcrete::ClassHash(_) => "ClassHash".to_string(),
            StarknetTypeConcrete::StorageAddress(_) => "StorageAddress".to_string(),
            StarknetTypeConcrete::StorageBaseAddress(_) => "StorageBaseAddress".to_string(),
            _ => extract_short_type_name(type_id),
        },
        _ => extract_short_type_name(type_id),
    }
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
    type_long_id.generic_id.0 == "Enum"
        && matches!(
            type_long_id.generic_args.first(),
            Some(GenericArg::UserType(user_type))
                if user_type.debug_name.as_ref().is_some_and(|n| n.starts_with("core::panics::PanicResult"))
        )
}

fn maybe_extract_maybe_relocatable_from_cell(
    cell: &CellExpression,
    registers_values: &RegistersValues,
    vm: &VirtualMachine,
) -> Option<MaybeRelocatable> {
    match cell {
        CellExpression::Deref(cell_ref) => {
            let addr = registers_values.relocatable_from_cell_ref(cell_ref);
            maybe_get_maybe_relocatable(addr, vm)
        }
        CellExpression::DoubleDeref(cell_ref, offset) => {
            let addr = registers_values.relocatable_from_cell_ref(cell_ref);
            let mut inner = match vm.segments.memory.get_relocatable(addr) {
                Ok(value) => value,
                Err(err) => {
                    error!("error when extracting relocatable from VM: {err:?}");
                    return None;
                }
            };
            inner.offset = (inner.offset as isize + *offset as isize) as usize;
            maybe_get_maybe_relocatable(inner, vm)
        }
        CellExpression::Immediate(value) => Some(MaybeRelocatable::Int(Felt252::from(value))),
        CellExpression::BinOp { op, a, b } => {
            let a_felt = maybe_get_felt_from_cell_ref(a, registers_values, vm)?;
            let b_felt = match b {
                DerefOrImmediate::Deref(cell_ref) => {
                    maybe_get_felt_from_cell_ref(cell_ref, registers_values, vm)
                }
                DerefOrImmediate::Immediate(value) => Some(Felt::from(value.value.clone())),
            }?;
            Some(MaybeRelocatable::Int(match op {
                CellOperator::Add => a_felt + b_felt,
                CellOperator::Sub => a_felt - b_felt,
                CellOperator::Mul => a_felt * b_felt,
                CellOperator::Div => a_felt.field_div(&NonZeroFelt::try_from(b_felt).unwrap()),
            }))
        }
    }
}

fn extract_array_pointers(values: &[MaybeRelocatable]) -> Option<(Relocatable, Relocatable)> {
    if values.len() != 2 {
        warn!("expected 2 values for array/span, got {}", values.len());
        return None;
    }
    let MaybeRelocatable::RelocatableValue(start_ptr) = values[0] else {
        warn!("expected relocatable for array start pointer");
        return None;
    };
    let MaybeRelocatable::RelocatableValue(end_ptr) = values[1] else {
        warn!("expected relocatable for array end pointer");
        return None;
    };
    Some((start_ptr, end_ptr))
}

fn extract_array_from_pointers(
    start_ptr: Relocatable,
    end_ptr: Relocatable,
    element_type_id: &ConcreteTypeId,
    vm: &VirtualMachine,
    ctx: &Context,
) -> Option<CairoValue> {
    if start_ptr.segment_index != end_ptr.segment_index {
        warn!("array start and end pointers in different segments");
        return None;
    }

    let element_type = format_type_name(element_type_id, ctx);
    let element_size = ctx.type_size(element_type_id);
    if element_size == 0 {
        return Some(CairoValue::Array { element_type, elements: vec![] });
    }

    let total_cells = end_ptr.offset.checked_sub(start_ptr.offset)?;
    if total_cells % element_size != 0 {
        warn!("array size {total_cells} not divisible by element size {element_size}");
        return None;
    }

    let num_elements = total_cells / element_size;
    let mut elements = Vec::with_capacity(num_elements);

    for i in 0..num_elements {
        let element_values = (0..element_size)
            .map(|j| {
                maybe_get_maybe_relocatable(
                    Relocatable {
                        segment_index: start_ptr.segment_index,
                        offset: start_ptr.offset + i * element_size + j,
                    },
                    vm,
                )
            })
            .collect::<Option<Vec<_>>>()?;
        elements.push(maybe_relocatables_to_cairo_value(
            &element_values,
            element_type_id,
            vm,
            ctx,
        )?);
    }

    Some(CairoValue::Array { element_type, elements })
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

fn maybe_get_maybe_relocatable(addr: Relocatable, vm: &VirtualMachine) -> Option<MaybeRelocatable> {
    match vm.segments.memory.get_maybe_relocatable(addr) {
        Ok(value) => Some(value),
        Err(err) => {
            error!("error when reading memory at {addr}: {err:?}");
            None
        }
    }
}
