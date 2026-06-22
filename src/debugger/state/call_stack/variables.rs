use cairo_annotations::annotations::coverage::SourceCodeSpan;
use cairo_lang_casm::cell_expression::CellExpression;
use cairo_lang_sierra::extensions::core::CoreTypeConcrete;
use cairo_lang_sierra::extensions::modules::starknet::StarknetTypeConcrete;
use cairo_lang_sierra::ids::ConcreteTypeId;
use cairo_vm::types::relocatable::{MaybeRelocatable, Relocatable};
use cairo_vm::vm::vm_core::VirtualMachine;
use indexmap::IndexMap;
use tracing::warn;

use crate::debugger::context::{CairoVarId, CairoVarReference, Context};
use crate::debugger::state::call_stack::{FunctionVariables, PostStatementsRegisters};

mod type_name;
mod vm_reader;

use type_name::{extract_short_type_name, format_type_name, is_bool, is_panic_result, is_tuple};
use vm_reader::VmReader;

#[derive(Clone, Debug, PartialEq)]
pub enum CairoValue {
    Bool(bool),
    FeltLike(cairo_vm::Felt252),
    Struct { type_name: String, fields: Vec<(String, CairoValue)> },
    Enum { type_name: String, variant_name: String, variant_value: Box<CairoValue> },
    Tuple(Vec<CairoValue>),
    Array { element_type: String, elements: Vec<CairoValue> },
    Snapshot(Box<CairoValue>),
    NonZero(Box<CairoValue>),
    Other(String),
}

impl CairoValue {
    /// Returns true if this value carries no data — an empty struct or empty tuple.
    /// Used to decide whether an enum variant should be displayed as a leaf node.
    pub fn is_like_unit_type(&self) -> bool {
        match self {
            Self::Struct { fields, .. } => fields.is_empty(),
            Self::Tuple(elems) => elems.is_empty(),
            _ => false,
        }
    }
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

        let reader = VmReader::new(vm, registers_values);

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

            let Some(value) = extract_var_value(&ref_expr.cells, &type_id, &reader, ctx) else {
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
    reader: &VmReader<'_>,
    ctx: &Context,
) -> Option<CairoValue> {
    let values = cells.iter().map(|cell| reader.read_cell(cell)).collect::<Option<Vec<_>>>()?;

    maybe_relocatables_to_cairo_value(&values, type_id, reader, ctx)
}

fn maybe_relocatables_to_cairo_value(
    values: &[MaybeRelocatable],
    type_id: &ConcreteTypeId,
    reader: &VmReader<'_>,
    ctx: &Context,
) -> Option<CairoValue> {
    let Some(concrete_type) = ctx.get_concrete_type(type_id) else {
        return fallback_value(values, type_id);
    };

    match concrete_type {
        CoreTypeConcrete::Array(info) | CoreTypeConcrete::Span(info) => {
            let (start_ptr, end_ptr) = extract_array_pointers(values)?;
            extract_array_from_pointers(start_ptr, end_ptr, &info.ty, reader, ctx)
        }
        CoreTypeConcrete::Snapshot(inner) => {
            maybe_relocatables_to_cairo_value(values, &inner.ty, reader, ctx)
                .map(|v| CairoValue::Snapshot(Box::new(v)))
        }
        CoreTypeConcrete::NonZero(inner) => {
            maybe_relocatables_to_cairo_value(values, &inner.ty, reader, ctx)
                .map(|v| CairoValue::NonZero(Box::new(v)))
        }
        CoreTypeConcrete::Struct(struct_type) => {
            let type_long_id = &ctx.var_type_info(type_id).long_id;
            let slices = member_slices(&struct_type.members, values, ctx)?;

            if is_tuple(type_long_id) {
                let elements = slices
                    .into_iter()
                    .map(|(ty, vals)| maybe_relocatables_to_cairo_value(vals, ty, reader, ctx))
                    .collect::<Option<Vec<_>>>()?;
                return Some(CairoValue::Tuple(elements));
            }

            let struct_info = ctx.struct_info(type_id);
            let type_name = struct_info
                .map(|info| info.name.clone())
                .unwrap_or_else(|| extract_short_type_name(type_id));
            let fields = slices
                .into_iter()
                .enumerate()
                .map(|(i, (ty, vals))| {
                    let name = struct_info
                        .and_then(|info| info.members.get(i))
                        .cloned()
                        .unwrap_or_else(|| format!(".{i}"));
                    let value = maybe_relocatables_to_cairo_value(vals, ty, reader, ctx)?;
                    Some((name, value))
                })
                .collect::<Option<Vec<_>>>()?;
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
                maybe_relocatables_to_cairo_value(variant_values, variant_id, reader, ctx)?;
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
    reader: &VmReader<'_>,
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
                reader.read_relocatable(Relocatable {
                    segment_index: start_ptr.segment_index,
                    offset: start_ptr.offset + i * element_size + j,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        elements.push(maybe_relocatables_to_cairo_value(
            &element_values,
            element_type_id,
            reader,
            ctx,
        )?);
    }

    Some(CairoValue::Array { element_type, elements })
}

/// Splits array of [`MaybeRelocatable`]s that contain data corresponding to the entire struct
/// to subarrays so that each subarray contains data corresponding to a consecutive field of that
/// struct.
fn member_slices<'m, 'v>(
    members: &'m [ConcreteTypeId],
    values: &'v [MaybeRelocatable],
    ctx: &Context,
) -> Option<Vec<(&'m ConcreteTypeId, &'v [MaybeRelocatable])>> {
    let mut offset = 0;
    members
        .iter()
        .map(|ty| {
            let size = ctx.type_size(ty);
            let end = offset + size;
            let slice = values.get(offset..end)?;
            offset = end;
            Some((ty, slice))
        })
        .collect()
}
