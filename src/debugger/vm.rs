use std::any::Any;
use std::collections::HashMap;

use cairo_vm::hint_processor::hint_processor_definition::HintProcessor;
use cairo_vm::types::exec_scope::ExecutionScopes;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use cairo_vm::vm::hooks::StepHooks;
use cairo_vm::vm::vm_core::VirtualMachine;

use crate::CairoDebugger;

impl StepHooks for CairoDebugger {
    fn before_first_step(
        &mut self,
        _vm: &mut VirtualMachine,
        _hints_data: &[Box<dyn Any>],
    ) -> Result<(), VirtualMachineError> {
        Ok(())
    }

    #[tracing::instrument(
        skip(self, vm, _hint_processor, _exec_scopes, _hints_data, _constants),
        err
    )]
    fn pre_step_instruction(
        &mut self,
        vm: &mut VirtualMachine,
        _hint_processor: &mut dyn HintProcessor,
        _exec_scopes: &mut ExecutionScopes,
        _hints_data: &[Box<dyn Any>],
        _constants: &HashMap<String, starknet_types_core::felt::Felt>,
    ) -> Result<(), VirtualMachineError> {
        self.sync_with_vm_pre_step(vm).map_err(VirtualMachineError::Other)
    }

    fn post_step_instruction(
        &mut self,
        _vm: &mut VirtualMachine,
        _hint_processor: &mut dyn HintProcessor,
        _exec_scopes: &mut ExecutionScopes,
        _hints_data: &[Box<dyn Any>],
        _constants: &HashMap<String, starknet_types_core::felt::Felt>,
    ) -> Result<(), VirtualMachineError> {
        self.sync_with_vm_post_step();
        Ok(())
    }
}
