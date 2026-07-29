use cairo_lang_sierra::extensions::core::CoreTypeConcrete;
use cairo_lang_sierra::extensions::modules::starknet::StarknetTypeConcrete;
use cairo_lang_sierra::ids::ConcreteTypeId;
use cairo_lang_sierra::program::{ConcreteTypeLongId, GenericArg};

use crate::debugger::context::Context;

pub fn extract_short_type_name(type_id: &ConcreteTypeId) -> String {
    if let Some(debug_name) = &type_id.debug_name {
        // Strip generic parameters and take the last path segment.
        let base = debug_name.split('<').next().unwrap_or(debug_name.as_str());
        return base.split("::").last().unwrap_or(base).to_string();
    }
    format!("type_{}", type_id.id)
}

pub fn format_type_name(type_id: &ConcreteTypeId, ctx: &Context) -> String {
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
        CoreTypeConcrete::Felt252Dict(info)
        | CoreTypeConcrete::Felt252DictEntry(info)
        | CoreTypeConcrete::SquashedFelt252Dict(info) => {
            format!("Felt252Dict<{}>", format_type_name(&info.ty, ctx))
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

pub fn is_tuple(type_long_id: &ConcreteTypeLongId) -> bool {
    matches_user_type(type_long_id, "Struct", |n| n == "Tuple")
}

pub fn is_bool(type_long_id: &ConcreteTypeLongId) -> bool {
    matches_user_type(type_long_id, "Enum", |n| n == "core::bool")
}

pub fn is_panic_result(type_long_id: &ConcreteTypeLongId) -> bool {
    matches_user_type(type_long_id, "Enum", |n| n.starts_with("core::panics::PanicResult"))
}

fn matches_user_type(
    type_long_id: &ConcreteTypeLongId,
    generic_kind: &str,
    name_matches: impl Fn(&str) -> bool,
) -> bool {
    type_long_id.generic_id.0.as_str() == generic_kind
        && matches!(
            type_long_id.generic_args.first(),
            Some(GenericArg::UserType(user_type))
                if user_type.debug_name.as_ref().is_some_and(|n| name_matches(n.as_str()))
        )
}
