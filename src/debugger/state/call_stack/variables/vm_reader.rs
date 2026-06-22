use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use cairo_lang_casm::operand::{CellRef, DerefOrImmediate};
use cairo_vm::Felt252;
use cairo_vm::types::relocatable::{MaybeRelocatable, Relocatable};
use cairo_vm::vm::vm_core::VirtualMachine;
use starknet_types_core::felt::{Felt, NonZeroFelt};
use tracing::error;

use crate::debugger::state::call_stack::RegistersValues;

/// Bundles a VM snapshot with the register values for a single statement,
/// providing typed read methods.
pub struct VmReader<'a> {
    vm: &'a VirtualMachine,
    registers: &'a RegistersValues,
}

impl<'a> VmReader<'a> {
    pub fn new(vm: &'a VirtualMachine, registers: &'a RegistersValues) -> Self {
        Self { vm, registers }
    }

    pub fn read_relocatable(&self, addr: Relocatable) -> Option<MaybeRelocatable> {
        match self.vm.segments.memory.get_maybe_relocatable(addr) {
            Ok(value) => Some(value),
            Err(err) => {
                error!("error when reading memory at {addr}: {err:?}");
                None
            }
        }
    }

    pub fn read_cell(&self, cell: &CellExpression) -> Option<MaybeRelocatable> {
        match cell {
            CellExpression::Deref(cell_ref) => {
                self.read_relocatable(self.registers.relocatable_from_cell_ref(cell_ref))
            }
            CellExpression::DoubleDeref(cell_ref, offset) => {
                let addr = self.registers.relocatable_from_cell_ref(cell_ref);
                let mut inner = match self.vm.segments.memory.get_relocatable(addr) {
                    Ok(value) => value,
                    Err(err) => {
                        error!("error when extracting relocatable from VM: {err:?}");
                        return None;
                    }
                };
                inner.offset = (inner.offset as isize + *offset as isize) as usize;
                self.read_relocatable(inner)
            }
            CellExpression::Immediate(value) => Some(MaybeRelocatable::Int(Felt252::from(value))),
            CellExpression::BinOp { op, a, b } => {
                let a_felt = self.extract_felt(a)?;
                let b_felt = match b {
                    DerefOrImmediate::Deref(cell_ref) => self.extract_felt(cell_ref),
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

    fn extract_felt(&self, cell_ref: &CellRef) -> Option<Felt> {
        let addr = self.registers.relocatable_from_cell_ref(cell_ref);
        match self.vm.segments.memory.get_integer(addr) {
            Ok(value) => Some(*value),
            Err(err) => {
                error!("error when extracting felt from VM: {err:?}");
                None
            }
        }
    }
}
