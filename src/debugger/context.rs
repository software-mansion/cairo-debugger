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
use cairo_annotations::{MappingResult, map_pc_to_sierra_statement_id};
use cairo_lang_sierra::extensions::core::{CoreConcreteLibfunc, CoreLibfunc, CoreType};
use cairo_lang_sierra::program::{Function, Program, ProgramArtifact, Statement, StatementIdx};
use cairo_lang_sierra::program_registry::ProgramRegistry;
use cairo_lang_sierra_to_casm::compiler::{CairoProgramDebugInfo, SierraToCasmConfig};
use cairo_lang_sierra_to_casm::metadata::calc_metadata;
use scarb_metadata::MetadataCommand;

use crate::debugger::context::file_locations::{FileCodeLocationsData, build_file_locations_map};
use crate::debugger::context::variables::{CairoVarsInStatement, build_cairo_var_to_casm_map};

mod file_locations;
#[cfg(feature = "dev")]
mod readable_sierra_ids;
mod variables;

pub use file_locations::Line;

/// Struct that holds all the initial data needed for the debugger during execution.
pub struct Context {
    pub root_path: PathBuf,
    sierra_context: SierraContext,
    casm_debug_info: CairoProgramDebugInfo,
    files_data: HashMap<PathBuf, FileCodeLocationsData>,
    #[expect(dead_code)]
    cairo_var_map: HashMap<StatementIdx, CairoVarsInStatement>,
    #[cfg(feature = "dev")]
    labels: HashMap<usize, String>,
}

struct SierraContext {
    program: Program,
    sierra_program_registry: ProgramRegistry<CoreType, CoreLibfunc>,
    code_locations: SierraCodeLocations,
    function_names: SierraFunctionNames,
}

impl Context {
    pub fn new(sierra_path: &Path) -> Result<Self> {
        let root_path = get_project_root_path(sierra_path)?;
        let content = fs::read_to_string(sierra_path).expect("Failed to load sierra file");

        let sierra_program: ProgramArtifact = serde_json::from_str(&content)?;
        let program = sierra_program.program;
        let sierra_program_registry =
            ProgramRegistry::new(&program).expect("creating program registry failed");

        let debug_info = sierra_program
            .debug_info
            .ok_or_else(|| anyhow!("debug_info must be present in compiled sierra"))?;

        let code_locations = SierraCodeLocations::try_from_debug_info(&debug_info)?;
        let functions_debug_info = FunctionsDebugInfo::try_from_debug_info(&debug_info)?;
        let function_names = SierraFunctionNames::try_from_debug_info(&debug_info)?;

        // TODO(#61)
        let casm_debug_info = compile_sierra_to_get_casm_debug_info(&program)?;
        let cairo_var_map =
            build_cairo_var_to_casm_map(&program, &casm_debug_info, functions_debug_info);

        let files_data = build_file_locations_map(&casm_debug_info, &code_locations);

        #[cfg(feature = "dev")]
        let labels = readable_sierra_ids::extract_labels(&program);

        let sierra_context =
            SierraContext { program, sierra_program_registry, code_locations, function_names };

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
                    self.sierra_context.sierra_program_registry.get_libfunc(&invocation.libfunc_id),
                    Ok(CoreConcreteLibfunc::FunctionCall(_))
                )
            }
            Statement::Return(_) => false,
        }
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

        eprintln!("{with_labels}")
    }
}

fn sierra_function_for_statement(statement_idx: usize, program: &Program) -> &Function {
    &program.funcs[program.funcs.partition_point(|x| x.entry_point.0 <= statement_idx) - 1]
}

fn compile_sierra_to_get_casm_debug_info(program: &Program) -> Result<CairoProgramDebugInfo> {
    let metadata = calc_metadata(program, Default::default())
        .with_context(|| "Failed calculating metadata.")?;
    let cairo_program = cairo_lang_sierra_to_casm::compiler::compile(
        program,
        &metadata,
        SierraToCasmConfig { gas_usage_check: true, max_bytecode_size: usize::MAX },
    )
    .with_context(|| "Compilation failed.")?;

    Ok(cairo_program.debug_info)
}

// TODO(#50)
fn get_project_root_path(sierra_path: &Path) -> Result<PathBuf> {
    Ok(MetadataCommand::new()
        .current_dir(sierra_path.parent().expect("Compiled Sierra must be in target directory"))
        .inherit_stderr()
        .exec()
        .context("Failed to get project metadata from Scarb")?
        .workspace
        .root
        .into())
}
