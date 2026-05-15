use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as AnyhowContext, Result, anyhow};
use cairo_annotations::annotations::TryFromDebugInfo;
use cairo_annotations::annotations::coverage::{
    CodeLocation, CoverageAnnotationsV1 as SierraCodeLocations,
};
use cairo_annotations::annotations::debugger::DebuggerAnnotationsV1 as FunctionsDebugInfo;
use cairo_annotations::annotations::profiler::{
    FunctionName, ProfilerAnnotationsV1 as SierraFunctionNames,
};
use cairo_annotations::annotations::type_names::{
    EnumInfo, SierraTypeId, StructInfo, TypeNamesAnnotationsV1 as TypeNames,
};
use cairo_annotations::{MappingResult, map_pc_to_sierra_statement_id};
use cairo_lang_sierra::extensions::core::{CoreConcreteLibfunc, CoreTypeConcrete};
use cairo_lang_sierra::extensions::lib_func::BranchSignature;
use cairo_lang_sierra::extensions::types::TypeInfo;
use cairo_lang_sierra::extensions::{ConcreteLibfunc, ConcreteType};
use cairo_lang_sierra::ids::{ConcreteTypeId, VarId};
use cairo_lang_sierra::program::{
    Function, GenBranchTarget, GenInvocation, Program, ProgramArtifact, Statement, StatementIdx,
};
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, SierraToCasmConfig};
use cairo_lang_sierra_to_casm::metadata::calc_metadata;
use cairo_lang_sierra_type_size::ProgramRegistryInfo;
use scarb_metadata::MetadataCommand;

use crate::debugger::context::file_locations::{FileCodeLocationsData, build_file_locations_map};
use crate::debugger::context::variables::build_cairo_var_to_casm_map;

mod file_locations;
#[cfg(feature = "dev")]
mod readable_sierra_ids;
mod variables;

pub use file_locations::Line;
pub use variables::{CairoVarId, CairoVarReference, CairoVarsInStatement};

/// Struct that holds all the initial data needed for the debugger during execution.
pub struct Context {
    pub root_path: PathBuf,
    sierra_context: SierraContext,
    casm_debug_info: CairoProgramDebugInfo,
    files_data: HashMap<PathBuf, FileCodeLocationsData>,
    pub cairo_var_map: HashMap<StatementIdx, CairoVarsInStatement>,
    #[cfg(feature = "dev")]
    labels: HashMap<usize, String>,
}

struct SierraContext {
    program: Program,
    program_registry_info: ProgramRegistryInfo,
    code_locations: SierraCodeLocations,
    function_names: SierraFunctionNames,
    type_names: Option<TypeNames>,
}

impl Context {
    pub fn new(sierra_path: &Path) -> Result<Self> {
        let root_path = get_project_root_path(sierra_path)?;
        let content = fs::read_to_string(sierra_path).context("failed to read sierra file")?;

        let sierra_program: ProgramArtifact = serde_json::from_str(&content)?;
        let program = sierra_program.program;
        let program_registry_info =
            ProgramRegistryInfo::new(&program).context("creating program registry failed")?;

        let debug_info = sierra_program.debug_info.ok_or_else(|| {
            anyhow!("sierra debug info is missing - enable generating it in your Scarb.toml")
        })?;

        let code_locations = SierraCodeLocations::try_from_debug_info(&debug_info)
            .context("statements code locations debug info is missing - enable generating it in your Scarb.toml")?;
        let functions_debug_info = FunctionsDebugInfo::try_from_debug_info(&debug_info)
            .context("functions debug info is missing - enable generating it in your Scarb.toml")?;
        let function_names = SierraFunctionNames::try_from_debug_info(&debug_info).context(
            "statements functions debug info is missing - enable generating it in your Scarb.toml",
        )?;
        let type_names = TypeNames::try_from_debug_info(&debug_info).ok();

        // TODO(#61)
        let casm_debug_info =
            compile_sierra_to_get_casm_debug_info(&program, &program_registry_info)?;
        let cairo_var_map =
            build_cairo_var_to_casm_map(&program, &casm_debug_info, functions_debug_info);

        let files_data = build_file_locations_map(&casm_debug_info, &code_locations);

        #[cfg(feature = "dev")]
        let labels = readable_sierra_ids::extract_labels(&program);

        let sierra_context = SierraContext {
            program,
            program_registry_info,
            code_locations,
            function_names,
            type_names,
        };

        Ok(Self {
            #[cfg(feature = "dev")]
            labels,

            root_path,
            sierra_context,
            casm_debug_info,
            files_data,
            cairo_var_map,
        })
    }

    pub fn statement_idx_for_pc(&self, pc: usize) -> Option<StatementIdx> {
        match map_pc_to_sierra_statement_id(&self.casm_debug_info.sierra_statement_info, pc, 0) {
            MappingResult::SierraStatementIdx(idx) => Some(idx),
            MappingResult::Header | MappingResult::PcOutOfFunctionArea => None,
        }
    }

    /// Return code location for the current statement, not including inlined code locations.
    pub fn code_location_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<CodeLocation> {
        self.sierra_context
            .code_locations
            .statements_code_locations
            .get(&statement_idx)
            .and_then(|locations| locations.first().cloned())
    }

    /// Return code locations for the current statement, including inlined code locations.
    /// The first element is not inlined.
    pub fn code_locations_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<&Vec<CodeLocation>> {
        self.sierra_context.code_locations.statements_code_locations.get(&statement_idx)
    }

    /// Return function names for the current statement, including inlined function names.
    /// The first element is not inlined.
    pub fn function_names_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<&Vec<FunctionName>> {
        self.sierra_context.function_names.statements_functions.get(&statement_idx)
    }

    pub fn statement_idxs_for_breakpoint(
        &self,
        source: &Path,
        line: Line,
    ) -> Option<&Vec<StatementIdx>> {
        self.files_data.get(source)?.lines.get(&line)
    }

    pub fn is_return_statement(&self, statement_idx: StatementIdx) -> bool {
        matches!(self.statement_idx_to_statement(statement_idx), Statement::Return(_))
    }

    pub fn is_function_call_statement(&self, statement_idx: StatementIdx) -> bool {
        match self.statement_idx_to_statement(statement_idx) {
            Statement::Invocation(invocation) => {
                matches!(
                    self.sierra_context
                        .program_registry_info
                        .registry
                        .get_libfunc(&invocation.libfunc_id),
                    Ok(CoreConcreteLibfunc::FunctionCall(_))
                )
            }
            Statement::Return(_) => false,
        }
    }

    pub fn branches_for_statement(&self, statement_idx: StatementIdx) -> Vec<StatementIdx> {
        match self.statement_idx_to_statement(statement_idx) {
            Statement::Invocation(invocation) => invocation
                .branches
                .iter()
                .map(|gen_branch_info| match gen_branch_info.target {
                    GenBranchTarget::Fallthrough => StatementIdx(statement_idx.0 + 1),
                    GenBranchTarget::Statement(idx) => idx,
                })
                .collect(),
            // TODO(#91)
            Statement::Return(_) => Vec::new(),
        }
    }

    pub fn does_compile_to_casm(&self, statement_idx: StatementIdx) -> bool {
        let info = &self.casm_debug_info.sierra_statement_info[statement_idx.0];
        info.start_offset != info.end_offset
    }

    pub fn sierra_function_for_statement(&self, statement_idx: StatementIdx) -> &Function {
        sierra_function_for_statement(statement_idx.0, &self.sierra_context.program)
    }

    pub fn branch_signature_and_results(
        &self,
        statement_idx: StatementIdx,
        branch_target: &GenBranchTarget<StatementIdx>,
    ) -> Option<(&BranchSignature, &Vec<VarId>)> {
        let Statement::Invocation(GenInvocation { libfunc_id, branches, .. }) =
            self.statement_idx_to_statement(statement_idx)
        else {
            return None;
        };

        let branch_index = branches.iter().position(|info| &info.target == branch_target).unwrap();
        let branch_signature = &self
            .sierra_context
            .program_registry_info
            .registry
            .get_libfunc(libfunc_id)
            .unwrap()
            .branch_signatures()[branch_index];
        let branch_results = &branches[branch_index].results;

        Some((branch_signature, branch_results))
    }

    pub fn var_type_info(&self, concrete_type_id: &ConcreteTypeId) -> &TypeInfo {
        self.sierra_context
            .program_registry_info
            .registry
            .get_type(concrete_type_id)
            .unwrap()
            .info()
    }

    pub fn var_type_id<'a>(
        &self,
        var_id: &VarId,
        branch_signature: &'a BranchSignature,
        branch_results: &[VarId],
    ) -> &'a ConcreteTypeId {
        let var_index = branch_results.iter().position(|id| id == var_id).unwrap();
        &branch_signature.vars[var_index].ty
    }

    pub fn type_size(&self, type_id: &ConcreteTypeId) -> usize {
        *self
            .sierra_context
            .program_registry_info
            .type_sizes
            .get(type_id)
            .expect("type id is expected to exist in type size map") as usize
    }

    pub fn get_concrete_type(&self, type_id: &ConcreteTypeId) -> Option<&CoreTypeConcrete> {
        self.sierra_context.program_registry_info.registry().get_type(type_id).ok()
    }

    pub fn struct_info(&self, type_id: &ConcreteTypeId) -> Option<&StructInfo> {
        self.sierra_context.type_names.as_ref()?.structs.get(&SierraTypeId(type_id.id))
    }

    pub fn enum_info(&self, type_id: &ConcreteTypeId) -> Option<&EnumInfo> {
        self.sierra_context.type_names.as_ref()?.enums.get(&SierraTypeId(type_id.id))
    }

    fn statement_idx_to_statement(&self, statement_idx: StatementIdx) -> &Statement {
        &self.sierra_context.program.statements[statement_idx.0]
    }

    #[cfg(feature = "dev")]
    #[allow(unused)]
    pub fn print_statement(&self, statement_idx: StatementIdx) {
        let statement = self.statement_idx_to_statement(statement_idx);
        let with_labels = readable_sierra_ids::replace_statement_id(statement.clone(), |idx| {
            self.labels[&idx.0].clone()
        });

        eprintln!("{} {with_labels}", statement_idx.0);
    }
}

fn sierra_function_for_statement(statement_idx: usize, program: &Program) -> &Function {
    &program.funcs[program.funcs.partition_point(|x| x.entry_point.0 <= statement_idx) - 1]
}

fn compile_sierra_to_get_casm_debug_info(
    program: &Program,
    program_registry_info: &ProgramRegistryInfo,
) -> Result<CairoProgramDebugInfo> {
    let metadata = calc_metadata(program, program_registry_info, Default::default())
        .with_context(|| "failed calculating CASM metadata.")?;
    let cairo_program = cairo_lang_sierra_to_casm::compiler::compile(
        program,
        program_registry_info,
        &metadata,
        SierraToCasmConfig { gas_usage_check: true, max_bytecode_size: usize::MAX },
    )
    .with_context(|| "sierra to CASM compilation failed.")?;

    Ok(cairo_program.debug_info)
}

// TODO(#50)
fn get_project_root_path(sierra_path: &Path) -> Result<PathBuf> {
    Ok(MetadataCommand::new()
        .current_dir(sierra_path.parent().expect("compiled sierra must be in target directory"))
        .inherit_stderr()
        .exec()
        .context("failed to get project metadata from scarb")?
        .workspace
        .root
        .into())
}
