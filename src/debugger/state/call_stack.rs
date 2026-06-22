use std::collections::HashMap;
use std::iter;
use std::path::Path;

use cairo_annotations::annotations::coverage::{CodeLocation, SourceFileFullPath};
use cairo_annotations::annotations::profiler::FunctionName;
use cairo_lang_casm::operand::{CellRef, Register};
use cairo_lang_sierra::program::{BranchTarget, GenBranchTarget, StatementIdx};
use cairo_vm::types::relocatable::Relocatable;
use cairo_vm::vm::vm_core::VirtualMachine;
use dap::types::{Scope, ScopePresentationhint, StackFrame, Variable};
use dap::types::{Source, StackFramePresentationhint};
use indexmap::IndexMap;

use crate::debugger::MIN_OBJECT_REFERENCE;
use crate::debugger::context::Context;
use crate::debugger::state::call_stack::variables::get_values_of_variables;

mod variables;

pub use variables::CairoValue;

type SubStack = Vec<(StackFrame, FunctionVariables)>;

/// Nested variable references start above any realistic frame scope reference value.
const NESTED_VAR_REF_START: i64 = 100_000;

pub struct CallStack {
    /// Stack of Cairo function frames and values of variables in frames corresponding
    /// to these functions.
    ///
    /// 1. Each function frame corresponds to a Cairo function that is currently on the Cairo call
    ///    stack.
    ///    This includes inlined functions, even though they are not present on the call stack
    ///    if one looks from Sierra/CASM POV.
    ///
    /// 2. The stack is divided into substacks.
    ///    Each substack corresponds to a Sierra function call statement that is currently
    ///    on the Sierra call stack.
    ///    The substack contains zero to many frames of inlined Cairo functions together
    ///    with exactly one non-inlined Cairo function frame at the end of the substack.
    ///
    /// 3. The stack does ***not*** contain a substack corresponding to the current statement.
    ///
    /// The stack also contains [`SierraFunctionContext`] corresponding to the function frame.
    /// It is done to ensure each Sierra function frame has its own instance of the context.
    ///
    /// [Object references](https://microsoft.github.io/debug-adapter-protocol/overview#lifetime-of-objects-references):
    /// object reference for each stack frame is equal to its `1 + 2 * flat_index`
    /// where `flat_index` is its position (0-indexed) in the flattened vector (vector of tuples).
    /// For the variables' scope, the object reference is equal to `2 + 2 * flat_index`.
    call_frames_and_vars: Vec<(SubStack, SierraFunctionContext)>,

    /// The context of the currently executed ***Sierra*** function.
    current_sierra_function_context: SierraFunctionContext,

    /// Modification that should be applied to the stack when a new sierra statement is reached.
    ///
    /// This field is there to ensure that a correct stack trace is returned when a current
    /// statement maps to a function call or a return statement.
    /// The stack should be modified ***after*** such a statement is executed.
    action_on_new_statement: Option<Action>,

    /// Registry mapping DAP `variables_reference` values to their child variables.
    /// Used for expanding nested struct/enum values in the IDE.
    /// Only entries for the current function frame are cleared on each new Sierra statement.
    nested_var_registry: HashMap<i64, Vec<(String, CairoValue)>>,

    /// Counter for assigning nested variable reference IDs.
    /// Starts at [`NESTED_VAR_REF_START`] and is reset to the current frame's boundary each step,
    /// reclaiming IDs for the current frame (uniqueness is only required within a stopped state).
    next_nested_ref: i64,
}

impl Default for CallStack {
    fn default() -> Self {
        Self {
            call_frames_and_vars: Default::default(),
            current_sierra_function_context: Default::default(),
            action_on_new_statement: Default::default(),
            nested_var_registry: Default::default(),
            next_nested_ref: NESTED_VAR_REF_START,
        }
    }
}

impl CallStack {
    pub fn depth(&self, statement_idx: StatementIdx, ctx: &Context) -> usize {
        self.flat_length() + self.build_stack_frames(ctx, statement_idx).count()
    }

    pub fn update_post_step(&mut self) {
        // We can be sure that the next `statement_idx` is different from the one which was the arg
        // when `action_on_new_statement` was set.
        // The reason is that both function call and return in sierra compile to one CASM instruction each.
        // https://github.com/starkware-libs/cairo/blob/20eca60c88a35f7da13f573b2fc68818506703a9/crates/cairo-lang-sierra-to-casm/src/invocations/function_call.rs#L46
        // https://github.com/starkware-libs/cairo/blob/d52acf845fc234f1746f814de7c64b535563d479/crates/cairo-lang-sierra-to-casm/src/compiler.rs#L533
        match self.action_on_new_statement.take() {
            Some(Action::Push(frames_and_variables)) => {
                let sierra_function_context =
                    std::mem::take(&mut self.current_sierra_function_context);

                self.call_frames_and_vars.push((frames_and_variables, sierra_function_context));
                self.current_sierra_function_context.nested_ref_start = self.next_nested_ref;
            }
            Some(Action::Pop) => {
                if let Some((_, sierra_function_context)) = self.call_frames_and_vars.pop() {
                    let start = sierra_function_context.nested_ref_start;
                    self.nested_var_registry.retain(|&k, _| k < start);
                    self.next_nested_ref = start;

                    self.current_sierra_function_context = sierra_function_context;
                }
            }
            None => {}
        }
    }

    pub fn update_pre_step(
        &mut self,
        statement_idx: StatementIdx,
        ctx: &Context,
        vm: &VirtualMachine,
    ) {
        // This should never happen, but better safe than sorry.
        if Some(statement_idx) == self.current_sierra_function_context.last_executed_statement {
            return;
        }

        // Invalidate only the current function's entries: keys below nested_ref_start
        // belong to past (caller) frames whose variables are fixed and can remain cached.
        // Reset the counter to reclaim those IDs — uniqueness is only required within a single
        // stopped state, not across the whole session.
        let start = self.current_sierra_function_context.nested_ref_start;
        self.nested_var_registry.retain(|&k, _| k < start);
        self.next_nested_ref = start;

        self.current_sierra_function_context.handle_branch_entrances(statement_idx, ctx, vm);
        self.current_sierra_function_context.last_executed_statement = Some(statement_idx);

        if ctx.is_function_call_statement(statement_idx) {
            let frames: Vec<_> = self.build_stack_frames(ctx, statement_idx).collect();
            // TODO(#95): handle variables of inlined functions.
            let vars = iter::repeat_n(FunctionVariables::default(), frames.len() - 1)
                .chain(iter::once(self.get_current_function_variables(ctx, vm)));

            let frames_and_vars = frames.into_iter().zip(vars).collect();

            self.action_on_new_statement = Some(Action::Push(frames_and_vars));
        } else if ctx.is_return_statement(statement_idx) {
            self.action_on_new_statement = Some(Action::Pop);
        }
    }

    pub fn get_frames(&self, statement_idx: StatementIdx, ctx: &Context) -> Vec<StackFrame> {
        self.call_frames_and_vars
            .iter()
            .flat_map(|substack| &substack.0)
            .map(|(frame, _)| frame)
            .cloned()
            .chain(self.build_stack_frames(ctx, statement_idx))
            // DAP expects frames to start from the most nested element.
            .rev()
            .collect()
    }

    pub fn get_scopes_for_frame(&self, frame_id: i64) -> Vec<Scope> {
        let scope = Scope {
            name: "Locals".to_string(),
            variables_reference: frame_id + 1,
            presentation_hint: Some(ScopePresentationhint::Locals),
            ..Default::default()
        };
        vec![scope]
    }

    /// Returns the semantic variable values for the current function without modifying
    /// the nested variable registry or incrementing reference counters.
    pub fn get_current_function_variables(
        &self,
        ctx: &Context,
        vm: &VirtualMachine,
    ) -> FunctionVariables {
        get_values_of_variables(
            ctx,
            vm,
            &self.current_sierra_function_context.post_statements_registers,
        )
    }

    pub fn get_variables(
        &mut self,
        variables_reference: i64,
        ctx: &Context,
        vm: &VirtualMachine,
    ) -> Vec<Variable> {
        // Check the nested registry first (handles expansion of struct/enum children).
        if let Some(children) = self.nested_var_registry.get(&variables_reference).cloned() {
            return children
                .into_iter()
                .map(|(name, value)| self.cairo_value_to_variable(name, value))
                .collect();
        }

        let flat_index = (variables_reference / 2 - 1) as usize;

        let FunctionVariables { names_to_values } = if flat_index >= self.flat_length() {
            self.get_current_function_variables(ctx, vm)
        } else {
            self.call_frames_and_vars
                .iter()
                .flat_map(|substack| &substack.0)
                .map(|(_, vars)| vars)
                .nth(flat_index)
                .unwrap()
                .clone()
        };

        names_to_values
            .into_iter()
            .map(|(name, value)| self.cairo_value_to_variable(name, value))
            .collect()
    }

    fn cairo_value_to_variable(&mut self, name: String, value: CairoValue) -> Variable {
        match value {
            CairoValue::Bool(v) => leaf_variable(name, v.to_string()),
            CairoValue::FeltLike(v) => leaf_variable(name, v.to_string()),
            CairoValue::Other(v) => leaf_variable(name, v),
            CairoValue::Struct { type_name, fields } => {
                if fields.is_empty() {
                    leaf_variable(name, "()".to_string())
                } else {
                    self.expandable_variable(name, type_name, fields)
                }
            }
            CairoValue::Tuple(fields) => {
                if fields.is_empty() {
                    leaf_variable(name, "()".to_string())
                } else {
                    let children =
                        fields.into_iter().enumerate().map(|(i, v)| (format!(".{i}"), v)).collect();
                    self.expandable_variable(name, "(...)".to_string(), children)
                }
            }
            CairoValue::Enum { type_name, variant_name, variant_value } => {
                let display = format!("{type_name}::{variant_name}");
                if variant_value.is_like_unit_type() {
                    leaf_variable(name, display)
                } else {
                    self.expandable_variable(
                        name,
                        display,
                        vec![("value".to_string(), *variant_value)],
                    )
                }
            }
            CairoValue::Array { element_type, elements } => {
                let type_display = format!("Array<{element_type}>");
                if elements.is_empty() {
                    leaf_variable(name, type_display)
                } else {
                    let children = elements
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| (format!("[{i}]"), v))
                        .collect();
                    self.expandable_variable(name, type_display, children)
                }
            }
            CairoValue::Snapshot(v) => {
                let mut var = self.cairo_value_to_variable(name, *v);
                var.value = format!("@{}", var.value);
                var
            }
            CairoValue::NonZero(v) => {
                let mut var = self.cairo_value_to_variable(name, *v);
                var.value = format!("NonZero({})", var.value);
                var
            }
        }
    }

    fn expandable_variable(
        &mut self,
        name: String,
        value: String,
        children: Vec<(String, CairoValue)>,
    ) -> Variable {
        let ref_id = self.register_children(children);
        Variable { name, value, variables_reference: ref_id, ..Default::default() }
    }

    fn register_children(&mut self, children: Vec<(String, CairoValue)>) -> i64 {
        let ref_id = self.next_nested_ref;
        self.nested_var_registry.insert(ref_id, children);

        self.next_nested_ref += 1;

        ref_id
    }

    /// Builds a vector of stack frames, ordered from the least nested to the most nested element.
    fn build_stack_frames<'a>(
        &'a self,
        ctx: &'a Context,
        statement_idx: StatementIdx,
    ) -> Box<dyn DoubleEndedIterator<Item = StackFrame> + 'a> {
        let Some(code_locations) = ctx.code_locations_for_statement_idx(statement_idx) else {
            return Box::new(vec![self.unknown_frame()].into_iter());
        };

        let function_names = ctx
            .function_names_for_statement_idx(statement_idx)
            .cloned()
            .unwrap_or_else(|| vec![FunctionName("test".to_string())]);

        Box::new(code_locations.clone().into_iter().rev().zip(function_names).map(
            |(code_location, function_name)| {
                self.build_stack_frame(&code_location, &function_name, ctx)
            },
        ))
    }

    fn build_stack_frame(
        &self,
        CodeLocation(SourceFileFullPath(source_file), code_span, _): &CodeLocation,
        FunctionName(function_name): &FunctionName,
        ctx: &Context,
    ) -> StackFrame {
        let file_path = Path::new(&source_file);
        let name = function_name.clone();

        let is_user_code = file_path.starts_with(&ctx.root_path);
        let presentation_hint = Some(if is_user_code {
            StackFramePresentationhint::Normal
        } else {
            StackFramePresentationhint::Subtle
        });

        // Annotations from debug info are 0-indexed.
        // UI expects 1-indexed, hence +1 below.
        let line = (code_span.start.line.0 + 1) as i64;
        let column = (code_span.start.col.0 + 1) as i64;

        StackFrame {
            id: self.next_frame_id(),
            name,
            source: Some(Source {
                name: None,
                path: Some(source_file.clone()),
                ..Default::default()
            }),
            line,
            column,
            presentation_hint,
            ..Default::default()
        }
    }

    fn unknown_frame(&self) -> StackFrame {
        StackFrame {
            id: self.next_frame_id(),
            name: "Unknown".to_string(),
            line: 1,
            column: 1,
            presentation_hint: Some(StackFramePresentationhint::Subtle),
            ..Default::default()
        }
    }

    fn next_frame_id(&self) -> i64 {
        MIN_OBJECT_REFERENCE + 2 * self.flat_length() as i64
    }

    fn flat_length(&self) -> usize {
        self.call_frames_and_vars.iter().map(|(frames, _)| frames.len()).sum()
    }
}

type PostStatementsRegisters =
    IndexMap<StatementIdx, (GenBranchTarget<StatementIdx>, RegistersValues)>;

struct SierraFunctionContext {
    /// Mapping from sierra statements executed during the function frame execution to values of registers
    /// right after entering any branch of the statement (by definition only one branch is entered)
    /// and the entered branch target.
    ///
    /// Note that in general this is ***NOT*** the same as values of registers right after executing
    /// the statement itself, even though it may be the case for a lot of simple libfuncs.
    ///
    /// The mapping uses [`IndexMap`] to memorise the order of statement execution, allowing for
    /// retrieval of the trace of executed sierra statements within the function frame.
    post_statements_registers: PostStatementsRegisters,

    last_executed_statement: Option<StatementIdx>,

    /// The lowest `nested_var_registry` key that belongs to this function frame.
    /// Entries with keys >= this value are cleared when execution moves to a new statement.
    nested_ref_start: i64,
}

impl Default for SierraFunctionContext {
    fn default() -> Self {
        Self {
            post_statements_registers: Default::default(),
            last_executed_statement: Default::default(),
            nested_ref_start: NESTED_VAR_REF_START,
        }
    }
}

impl SierraFunctionContext {
    fn handle_branch_entrances(
        &mut self,
        statement_idx: StatementIdx,
        ctx: &Context,
        vm: &VirtualMachine,
    ) {
        let registers_values = RegistersValues { ap: vm.get_ap().offset, fp: vm.get_fp().offset };

        let Some(previous_statement_idx) = &self.last_executed_statement else {
            // If it is the first statement in this frame, all statements from the function
            // entrypoint to this statement:
            // 1. Have not compiled to any CASM instructions.
            // 2. Would have been executed if the execution happened via sierra executor.
            let entry_point = ctx.sierra_function_for_statement(statement_idx).entry_point;
            let executed_zero_casm_statements = (entry_point.0..statement_idx.0).map(|idx| {
                (
                    StatementIdx(idx),
                    (
                        // All statements compiling to no CASM have only a fallthrough branch.
                        BranchTarget::Fallthrough,
                        registers_values.clone(),
                    ),
                )
            });

            self.post_statements_registers.extend(executed_zero_casm_statements);
            return;
        };

        // We had to hit one a branch of the last executed statement in this function.
        // To find which one, find a path that goes from any branch entrance to `statement_idx`,
        // but only through statements that compile to no CASM.
        // Such statements they are the only ones that could have been "executed"
        // between the current statement and the lastly executed one.
        let hit_branch_entrance = ctx
            .branches_for_statement(*previous_statement_idx)
            .into_iter()
            // We never encountered a case where there is more than one such path.
            // If there exists a case like this, we cannot know which one was taken anyway,
            // so we take the first one.
            .find(|entrance| {
                let mut current = *entrance;
                while current != statement_idx && !ctx.does_compile_to_casm(current) {
                    // All statements compiling to no CASM have only a fallthrough branch.
                    current.0 += 1;
                }

                current == statement_idx
            })
            // There has to be at least one path like this.
            .unwrap();

        let hit_branch_target = if hit_branch_entrance.0 == previous_statement_idx.0 + 1 {
            GenBranchTarget::Fallthrough
        } else {
            GenBranchTarget::Statement(hit_branch_entrance)
        };

        let executed_branch_hit =
            (*previous_statement_idx, (hit_branch_target, registers_values.clone()));
        let executed_zero_casm_statements =
            ((hit_branch_entrance.0 + 1)..statement_idx.0).map(|idx| {
                (
                    StatementIdx(idx),
                    (
                        // All statements compiling to no CASM have only a fallthrough branch.
                        BranchTarget::Fallthrough,
                        registers_values.clone(),
                    ),
                )
            });
        let executed_statements =
            iter::once(executed_branch_hit).chain(executed_zero_casm_statements);

        self.post_statements_registers.extend(executed_statements);
    }
}

fn leaf_variable(name: String, value: String) -> Variable {
    Variable { name, value, variables_reference: 0, ..Default::default() }
}

enum Action {
    Push(SubStack),
    Pop,
}

#[derive(Default, Clone, PartialEq)]
pub struct FunctionVariables {
    names_to_values: IndexMap<String, CairoValue>,
}

#[derive(Clone, Debug)]
struct RegistersValues {
    ap: usize,
    fp: usize,
}

impl RegistersValues {
    fn relocatable_from_cell_ref(&self, cell_ref: &CellRef) -> Relocatable {
        let original_offset = match cell_ref.register {
            Register::AP => self.ap,
            Register::FP => self.fp,
        };
        let offset = (original_offset as isize + cell_ref.offset as isize).try_into().unwrap();

        // Segment index is always one for ap and fp.
        // https://web.archive.org/web/20240228050216/http://docs.cairo-lang.org/how_cairo_works/segments.html
        Relocatable { segment_index: 1, offset }
    }
}
