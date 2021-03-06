//! This module defines `Containers` used to hold results of analysis
//!
//! Containers are broken down into three levels of hierarchy to reflect different
//! levels of program analysis.
//!
//! The top-level container, `RadecoProject`, is in most cases, the gateway
//! to start using radeco-lib. A project essentially contains all the state/information
//! required for analysis of a binary. A single `RadecoProject` contains several
//! `RadecoModule`, one module for the main binary and, optionally, one for every
//! shared library. Lastly, `RadecoModule` is broken down into `RadecoFunction`, which holds
//! the per-function information for every identified function in a `RadecoModule`.
//!
//! Corresponding to each of the containers is a loader that constructs the containers.
//! Loaders are configurable, with sane defaults, and are designed to work independently
//! of loaders above it. Although the loaders are powerful way to interact with the loading
//! process, the basic process is quite straight forward. Here is a quick example with all
//! default options:
//!
//! ```rust ignore
//! # extern crate radeco_lib;
//! # use radeco_lib::frontend::radeco_containers::{RadecoProject, ProjectLoader};
//! # fn main() {
//! let mut rp: RadecoProject = ProjectLoader::default()  // setup the default loader
//!                                 .path("/bin/ls")      // path to bin to analyze
//!                                 .load();              // fire-off the loading
//! # }
//! ```
//!
//! All default options are defined under `radeco_containers::loader_defaults`.
//!
//! For more examples of loading, check the `examples/` directory of this project.


use frontend::bindings::{Binding, RBindings, RadecoBindings};
use frontend::llanalyzer;
use frontend::radeco_source::{WrappedR2Api, Source};
use frontend::ssaconstructor::SSAConstruct;
use frontend::imports::ImportInfo;

use middle::ir;
use middle::regfile::SubRegisterFile;
use middle::ssa::cfg_traits::CFG;
use middle::ssa::ssa_traits::{SSA, NodeData, NodeType};

use middle::ssa::ssastorage::SSAStorage;
use petgraph::Direction;

use petgraph::graph::{NodeIndex, Graph};
use petgraph::visit::EdgeRef;
use r2api::api_trait::R2Api;
use r2api::structs::{LOpInfo, LRegInfo, LSymbolInfo, LRelocInfo, LImportInfo, LExportInfo,
                     LSectionInfo, LEntryInfo, LSymbolType};

use r2pipe::r2::R2;
use rayon::prelude::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::collections::{btree_map, hash_map};
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;
use std::slice;
use std::sync::Arc;

// use cpuprofiler::PROFILER;

/// Defines sane defaults for the loading process.
pub mod loader_defaults {
    use frontend::radeco_source::Source;
    use r2api::structs::LSymbolType;
    use std::borrow::Cow;
    use std::rc::Rc;
    use super::{FLResult, PredicatedLoader};
    use super::{RadecoModule, RadecoFunction};

    /// Use symbol information to identify functions
    pub fn strat_use_symbols(source: Option<&Rc<Source>>,
                             fl: &FLResult,
                             rmod: &RadecoModule)
                             -> FLResult {
        rmod.symbols
            .iter()
            .filter(|f| if let Some(LSymbolType::Func) = f.stype {
                true
            } else {
                false
            })
            .fold(FLResult::default(), |mut acc, s| {
                let mut rfn = RadecoFunction::default();
                rfn.name = Cow::from(s.name.as_ref().unwrap().to_owned());
                rfn.offset = s.vaddr.unwrap();
                rfn.size = s.size.unwrap();

                acc.functions.insert(rfn.offset, rfn);
                acc.new += 1;
                acc
            })
    }

    /// Use analysis that `Source` provides to identify functions
    pub fn strat_use_source(source: Option<&Rc<Source>>,
                            fl: &FLResult,
                            rmod: &RadecoModule)
                            -> FLResult {
        // Load function information fom `Source`
        if let Some(ref src) = source {
            let mut new_fl = FLResult::default();
            if let Ok(ref functions) = src.functions() {
                for function in functions {
                    let mut rfn = RadecoFunction::default();
                    rfn.offset = function.offset.unwrap();
                    rfn.size = function.size.unwrap();
                    rfn.name = Cow::from(function.name.as_ref().unwrap().to_owned());
                    new_fl.functions.insert(rfn.offset, rfn);
                    new_fl.new = new_fl.new + 1;
                }
            }
            new_fl
        } else {
            fl.clone()
        }
    }
}

/// Top level container used to hold all analysis
pub struct RadecoProject {
    /// Map of loaded modules
    modules: Vec<RadecoModule>,
    /// Register/Arch information for loaded project
    reginfo: Arc<SubRegisterFile>,
}

// Graph where every node is an Address (function start address) and edges are labeled
// by the `callsite`, i.e., the actual location of the call.
pub type CallGraph = Graph<u64, CallContextInfo>;
pub trait CGInfo {
    // Return a list of callers to function at offset, along with their callsites
    fn callers<'a>(&'a self, idx: NodeIndex) -> Box<Iterator<Item = (u64, NodeIndex)> + 'a>;
    // Return (callsite, call target)
    fn callees<'a>(&'a self, idx: NodeIndex) -> Box<Iterator<Item = (u64, NodeIndex)> + 'a>;
}

impl CGInfo for CallGraph {
    // Return a list of callers to function at offset, along with their callsites
    fn callers<'a>(&'a self, idx: NodeIndex) -> Box<Iterator<Item = (u64, NodeIndex)> + 'a> {
        box self.edges_directed(idx, Direction::Incoming).map(|er| (er.weight().csite, er.target()))
    }

    // Return (callsite, call target)
    fn callees<'a>(&'a self, idx: NodeIndex) -> Box<Iterator<Item = (u64, NodeIndex)> + 'a> {
        box self.edges_directed(idx, Direction::Outgoing).map(|er| (er.weight().csite, er.target()))
    }
}

#[derive(Default)]
/// Container to store information about a single loaded binary or library.
pub struct RadecoModule {
    /// Human-readable name for the  module
    name: Cow<'static, str>,
    /// Path on disk to the loaded library
    path: Cow<'static, str>,
    // Information from the loader
    symbols: Vec<LSymbolInfo>,
    sections: Arc<Vec<LSectionInfo>>,
    // Map from PLT entry address to `ImportInfo` for an import
    pub imports: HashMap<u64, ImportInfo>,
    exports: Vec<LExportInfo>,
    relocs: Vec<LRelocInfo>,
    libs: Vec<String>,
    entrypoint: Vec<LEntryInfo>,
    // Information from early/low-level analysis
    /// Call graph for current module
    pub callgraph: CallGraph,
    /// Map of functions loaded
    pub functions: BTreeMap<u64, RadecoFunction>,
    /// Source used to load this module
    pub source: Option<Rc<Source>>,
}

#[derive(Debug, Clone)]
pub enum FunctionType {
    /// Function defined in the current binary
    Function,
    /// Import from another module. Set to u16::max_value() to represent `Unknown`
    /// Fixed up when the corresponding library that defines this function is loaded
    Import(u16),
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum BindingType {
    // Arguments - ith argument
    RegisterArgument(usize),
    StackArgument(usize),
    // Local variables
    RegisterLocal,
    // Stack offset (from "SP")
    StackLocal(usize),
    // Return
    Return,
    // Unknown
    Unknown,
}

impl Default for BindingType {
    fn default() -> BindingType {
        BindingType::Unknown
    }
}

impl BindingType {
    pub fn is_argument(&self) -> bool {
        match *self {
            BindingType::RegisterArgument(_) |
            BindingType::StackArgument(_) => true,
            _ => false,
        }
    }

    pub fn is_local(&self) -> bool {
        match *self {
            BindingType::RegisterLocal |
            BindingType::StackLocal(_) => true,
            _ => false,
        }
    }

    pub fn is_return(&self) -> bool {
        match *self {
            BindingType::Return => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VarBinding {
    pub btype: BindingType,
    name: Cow<'static, str>,
    // Index of the register in regfile that represents this varbinding
    pub ridx: Option<u64>,
    pub idx: NodeIndex, // Some arbitrary, serializable data can be added to these fields later.
}

impl VarBinding {
    pub fn new(btype: BindingType, mut name: Option<String>, idx: NodeIndex, ridx: Option<u64>) -> VarBinding {
        let name = Cow::from(name.unwrap_or_default());
        VarBinding {
            name: name,
            btype: btype,
            idx: idx,
            ridx: ridx,
        }
    }

    pub fn index(&self) -> NodeIndex {
        self.idx
    }

    pub fn btype(&self) -> BindingType {
        self.btype
    }

    pub fn btype_mut(&mut self) -> &mut BindingType {
        &mut self.btype
    }
}

#[derive(Debug, Clone, Default)]
pub struct VarBindings(Vec<VarBinding>);

impl<'a> IntoIterator for &'a VarBindings {
    type Item = &'a VarBinding;
    type IntoIter = VarBindingIter<'a>;
    fn into_iter(self) -> VarBindingIter<'a> {
        VarBindingIter(self.0.iter())
    }
}

pub struct VarBindingIter<'a>(slice::Iter<'a, VarBinding>);

impl<'a> Iterator for VarBindingIter<'a> {
    type Item = &'a VarBinding;
    fn next(&mut self) -> Option<&'a VarBinding> {
        self.0.next()
    }
}

#[derive(Debug, Clone, Default)]
/// Container to store information about identified function.
/// Used as a basic unit in intra-functional analysis.
pub struct RadecoFunction {
    // Represents the type of function
    // ftype: FunctionType,
    /// Raw instruction information for the current function
    pub instructions: Vec<LOpInfo>,
    /// Is current function known to be recursive
    is_recursive: Option<bool>,
    /// Human readable name for the function. Taken either from
    /// the symbol table or assigned based on offset.
    pub name: Cow<'static, str>,
    /// Start address of the function
    pub offset: u64,
    /// Size of the function in bytes
    size: u64,
    /// List of (data-) addresses this function references
    datarefs: Vec<u64>,
    /// Constructed SSA for the function
    ssa: SSAStorage,
    /// Node index in the module-level callgraph
    cgid: NodeIndex,
    /// Variable bindings
    bindings: VarBindings,
}

#[derive(Default)]
/// Top-level loader used to initialize a `RadecoProject`
pub struct ProjectLoader<'a> {
    load_libs: bool,
    path: Cow<'static, str>,
    load_library_path: Option<Cow<'static, str>>,
    filter_modules: Option<fn(&RadecoModule) -> bool>,
    source: Option<Rc<Source>>,
    mloader: Option<ModuleLoader<'a>>,
}

impl<'a> ProjectLoader<'a> {
    // TODO:
    //  - Associate identified bins/libs with their ModuleLoaders
    //  - Implement loading of libraries
    //  - Parallelize module loading as they should have different sources
    //  - Use filter option
    //  - Setup arch information in `RadecoProject`
    /// Enable loading of libraries
    pub fn load_libs(mut self) -> ProjectLoader<'a> {
        self.load_libs = true;
        self
    }

    /// Set path to load from
    pub fn path<T: AsRef<str>>(mut self, path: T) -> ProjectLoader<'a> {
        self.path = Cow::from(path.as_ref().to_owned());
        self
    }

    /// Setup and configure `ModuleLoader` to use
    pub fn module_loader<'b: 'a>(mut self, mloader: ModuleLoader<'b>) -> ProjectLoader<'a> {
        self.mloader = Some(mloader);
        self
    }

    /// Set the source to use for loading. This is propagated to every `ModuleLoader`
    /// unless it is reconfigured.
    pub fn source(mut self, source: Rc<Source>) -> ProjectLoader<'a> {
        self.source = Some(source);
        self
    }

    /// Set path to look for libraries. The `ProjectLoader` looks for
    /// matching filenames recursively within this directory.
    /// Only used if `load_libs` is true.
    pub fn load_library_path(mut self, path: &'static str) -> ProjectLoader<'a> {
        self.load_library_path = Some(Cow::from(path));
        self
    }

    /// Filter loading of `RadecoModules` based on `f`
    pub fn filter_modules(mut self, f: fn(&RadecoModule) -> bool) -> ProjectLoader<'a> {
        self.filter_modules = Some(f);
        self
    }

    /// Kick everything off based on the config/defaults
    pub fn load(mut self) -> RadecoProject {
        if self.source.is_none() {
            // Load r2 source.
            let mut r2 = R2::new(Some(&self.path)).expect("Unable to open r2");
            let mut r2w: WrappedR2Api<R2> = Rc::new(RefCell::new(r2));
            self.source = Some(Rc::new(r2w));
        };

        let source = self.source.as_ref().unwrap();

        // TODO: Load more arch specific information from the source

        if self.mloader.is_none() {
            self.mloader = Some(ModuleLoader::default().source(Rc::clone(source)));
        }

        let mut mod_map = Vec::new();

        {
            let mod_loader = self.mloader.as_mut().unwrap();
            // TODO: Set name correctly
            mod_map.push(mod_loader.load(Rc::clone(source)));
        }

        // Clear out irrelevant fields in self and move it into project loader
        // XXX: Do when needed!
        // self.mod_loader = None;
        let regfile = SubRegisterFile::new(&source.register_profile()
            .expect("Unable to load register profile"));

        RadecoProject {
            modules: mod_map,
            // XXX
            reginfo: Arc::new(regfile),
        }
    }
}

// Iterators over RadecoProject to yeils RadecoModules
/// `RadecoModule` with project information `zipped` into it
pub struct ZippedModule<'m> {
    pub project: &'m RadecoProject,
    pub module: &'m RadecoModule,
}

pub struct ModuleIter<'m> {
    project: &'m RadecoProject,
    iter: slice::Iter<'m, RadecoModule>,
}

// TODO: Add a way to access the project
pub struct ZippedModuleMut<'m> {
    pub module: &'m mut RadecoModule,
}

pub struct ModuleIterMut<'m> {
    iter: slice::IterMut<'m, RadecoModule>,
}

impl<'m> Iterator for ModuleIter<'m> {
    type Item = ZippedModule<'m>;
    fn next(&mut self) -> Option<ZippedModule<'m>> {
        if let Some(rmod) = self.iter.next() {
            Some(ZippedModule {
                project: &self.project,
                module: rmod,
            })
        } else {
            None
        }
    }
}

impl<'m> Iterator for ModuleIterMut<'m> {
    type Item = ZippedModuleMut<'m>;
    fn next(&mut self) -> Option<ZippedModuleMut<'m>> {
        if let Some(rmod) = self.iter.next() {
            Some(ZippedModuleMut { module: rmod })
        } else {
            None
        }
    }
}

pub struct ZippedFunction<'f> {
    pub module: &'f RadecoModule,
    pub function: (&'f u64, &'f RadecoFunction),
}

pub struct FunctionIter<'f> {
    module: &'f RadecoModule,
    iter: btree_map::Iter<'f, u64, RadecoFunction>,
}

// TODO: Add a way to access module
pub struct ZippedFunctionMut<'f> {
    pub function: (&'f u64, &'f mut RadecoFunction),
}

pub struct FunctionIterMut<'f> {
    iter: btree_map::IterMut<'f, u64, RadecoFunction>,
}


impl<'f> Iterator for FunctionIter<'f> {
    type Item = ZippedFunction<'f>;
    fn next(&mut self) -> Option<ZippedFunction<'f>> {
        if let Some(rfn) = self.iter.next() {
            Some(ZippedFunction {
                module: self.module,
                function: rfn,
            })
        } else {
            None
        }
    }
}

impl<'f> Iterator for FunctionIterMut<'f> {
    type Item = ZippedFunctionMut<'f>;
    fn next(&mut self) -> Option<ZippedFunctionMut<'f>> {
        if let Some(rfn) = self.iter.next() {
            Some(ZippedFunctionMut { function: rfn })
        } else {
            None
        }
    }
}

#[derive(Default)]
/// Module-level loader used to construct a `RadecoModule`
pub struct ModuleLoader<'a> {
    source: Option<Rc<Source>>,
    floader: Option<FunctionLoader<'a>>,
    filter: Option<fn(&RadecoFunction) -> bool>,
    build_callgraph: bool,
    build_ssa: bool,
    load_datarefs: bool,
    load_locals: bool,
    parallel: bool,
    assume_cc: bool,
    stub_imports: bool,
}

impl<'a> ModuleLoader<'a> {
    // TODO:
    //  1. Callgraph from source
    //  2. As a part of above, fill in callrefs and callxrefs in RadecoFunction
    //  3. Expose SSA Construction as a part of loading process with options
    //     to parallelize it.
    //  4. Optionally load datarefs
    //  5. Optionally load local var information for functions
    /// Setup `Source` for `ModuleLoader`
    pub fn source<'b: 'a>(mut self, src: Rc<Source>) -> ModuleLoader<'a> {
        self.source = Some(src);
        self
    }

    /// Builds callgraph. Needs support from `Source`
    pub fn build_callgraph(mut self) -> ModuleLoader<'a> {
        self.build_callgraph = true;
        self
    }

    /// Builds SSA for loaded functions
    pub fn build_ssa(mut self) -> ModuleLoader<'a> {
        self.build_ssa = true;
        self
    }

    /// Loads information about datareferences for loaded functions.
    /// Needs support from `Source`
    pub fn load_datarefs(mut self) -> ModuleLoader<'a> {
        self.load_datarefs = true;
        self
    }

    /// Loads local variable information for loaded functions.
    /// Needs support from `Source`
    pub fn load_locals(mut self) -> ModuleLoader<'a> {
        self.load_locals = true;
        self
    }

    /// Executes parallelizable functions in parallel. Uses `num_thread` number
    /// of threads. Defaults to 8 if `None`.
    pub fn parallel(mut self) -> ModuleLoader<'a> {
        self.parallel = true;
        self
    }

    /// Assume calling convention information in regfile to be true. This is used for setting up
    /// bindings for arguments and return values for functions.
    pub fn assume_cc(mut self) -> ModuleLoader<'a> {
        self.assume_cc = true;
        self
    }

    /// Create blank, stub entries for imported functions.
    /// Required for load-libs, auto set when load_libs is true for the project loader.
    pub fn stub_imports(mut self) -> ModuleLoader<'a> {
        self.stub_imports = true;
        self
    }

    fn init_fn_bindings(rfn: &mut RadecoFunction, sub_reg_f: &SubRegisterFile) {
        // Setup binding information for functions based on reg_p. Note that this essential
        // marks the "potential" arguments without worrying about if they're ever used. Future
        // analysis can refine this information to make argument recognition more precise.

        // Get register state at entry block (for arguments) and at exit block (for returns).
        let (entry_state, exit_state) = {
            let ssa = rfn.ssa();
            let entry = ssa.entry_node().expect("No entry node found for function!");
            let exit = ssa.exit_node().expect("No exit node found for function!");

            let entry_state = ssa.registers_in(entry).expect("No registers found in entry");
            let exit_state = ssa.registers_in(exit).expect("No registers found in entry");
            (ssa.operands_of(entry_state), ssa.operands_of(exit_state))
        };

        let mut tbindings: Vec<VarBinding> = sub_reg_f.alias_info
            .iter()
            .enumerate()
            .filter_map(|(i, reg)| {
                let alias = reg.0;
                if let &Some(idx) = &["A0", "A1", "A2", "A3", "A4", "A5", "SN"]
                    .iter()
                    .position(|f| f == alias) {
                    let mut vb = VarBinding::default();
                    if idx < 6 {
                        vb.btype = BindingType::RegisterArgument(idx);
                        vb.idx = *entry_state.iter()
                            .find(|&&ridx| {
                                if let Ok(NodeType::Comment(ref s)) =
                                    rfn.ssa().node_data(ridx).map(|n| n.nt) {
                                    if s == reg.1 { true } else { false }
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(&NodeIndex::end());
                    } else {
                        vb.btype = BindingType::Return;
                        vb.idx = *exit_state.iter()
                            .find(|&&ridx| {
                                if let Ok(NodeType::Comment(ref s)) =
                                    rfn.ssa().node_data(ridx).map(|n| n.nt) {
                                    if s == reg.1 { true } else { false }
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(&NodeIndex::end());
                    }
                    vb.ridx = sub_reg_f.register_id_by_alias(alias);
                    Some(vb)
                } else {
                    None
                }
            })
            .collect();

        tbindings.sort_by(|x, y| {
            match (x.btype, y.btype) {
                (BindingType::RegisterArgument(i), BindingType::RegisterArgument(ref j)) => {
                    i.cmp(j)
                }
                (BindingType::RegisterArgument(_), _) => Ordering::Less,
                (_, BindingType::RegisterArgument(_)) => Ordering::Greater, 
                (_, _) => Ordering::Equal,
            }
        });

        rfn.bindings = VarBindings(tbindings);
    }

    /// Kick everything off and load module information based on config and defaults
    pub fn load(&mut self, src: Rc<Source>) -> RadecoModule {
        let source = if self.source.is_some() {
            self.source.as_ref().unwrap()
        } else {
            &src
        };

        if self.floader.is_none() {
            self.floader = Some(FunctionLoader::default().include_defaults());
        }
        // Setup source for the FunctionLoader
        let floader = self.floader.as_mut().unwrap();
        floader.source = Some(Rc::clone(source));

        let mut rmod = RadecoModule::default();

        // Fill in module level information from the `Source`
        match source.symbols() {
            Ok(sym_info) => rmod.symbols = sym_info,
            Err(e) => radeco_warn!(e),
        }

        match source.sections() {
            Ok(section_info) => rmod.sections = Arc::new(section_info),
            Err(e) => radeco_warn!(e),
        }

        match source.imports() {
            // TODO: Set the node in callgraph, either now or later.
            Ok(import_info) => {
                rmod.imports = import_info.iter().filter_map(|ii| {
                    if let Some(plt) = ii.plt {
                        Some((plt, ImportInfo::new_stub(plt, Cow::from(ii.name.as_ref().unwrap().clone()))))
                    } else {
                        None
                    }
                }).collect();
            },
            Err(e) => radeco_warn!(e),
        }

        match source.exports() {
            Ok(exports) => rmod.exports = exports,
            Err(e) => radeco_warn!(e),
        }

        match source.relocs() {
            Ok(relocs) => rmod.relocs = relocs,
            Err(e) => radeco_warn!(e),
        }

        match source.libraries() {
            Ok(libs) => rmod.libs = libs,
            Err(e) => radeco_warn!(e),
        }

        match source.entrypoint() {
            Ok(ep) => rmod.entrypoint = ep,
            Err(e) => radeco_warn!(e),
        }

        let mut flresult = floader.load(&rmod);
        flresult.functions = if self.filter.is_some() {
            let filter_fn = self.filter.as_ref().unwrap();
            flresult.functions.into_iter().filter(|&(ref x, ref v)| filter_fn(v)).collect()
        } else {
            flresult.functions
        };

        rmod.functions = flresult.functions;

        // Load instructions into functions
        for (_, rfn) in rmod.functions.iter_mut() {
            rfn.instructions = source.disassemble_n_bytes(rfn.size, rfn.offset)
                .unwrap_or(Vec::new());
        }

        // Optionally construct the SSA.
        let reg_p = source.register_profile().expect("Unable to load register profile");
        let sub_reg_f = SubRegisterFile::new(&reg_p);
        if self.build_ssa {
            if self.parallel {
                let ascc = self.assume_cc;
                rmod.functions.par_iter_mut().for_each(|(_, rfn)| {
                    SSAConstruct::<SSAStorage>::construct(rfn, &reg_p, ascc);
                });
            } else {
                for (off, rfn) in rmod.functions.iter_mut() {
                    SSAConstruct::<SSAStorage>::construct(rfn, &reg_p, self.assume_cc);
                }
            }
        }

        if self.stub_imports {
            for (off, ifn) in rmod.imports.iter_mut() {
                SSAConstruct::<SSAStorage>::construct(&mut ifn.rfn.borrow_mut(), &reg_p, self.assume_cc);
            }
        }

        // Load optional information. These need support from `Source` for analysis
        if self.build_callgraph || self.load_datarefs || self.load_locals {
            let aux_info = match source.functions() {
                Ok(info) => info,
                Err(e) => {
                    radeco_warn!(e);
                    Vec::new()
                }
            };

            if self.build_callgraph {
                rmod.callgraph = llanalyzer::load_call_graph(aux_info.as_slice(), &rmod);
                // Iterate through nodes and associate nodes with the correct functions
                for nidx in rmod.callgraph.node_indices() {
                    if let Some(cg_addr) = rmod.callgraph.node_weight(nidx) {
                        if let Some(rfn) = rmod.functions.get_mut(cg_addr) {
                            // Functions defined in this binary
                            rfn.cgid = nidx;
                        } else if let Some(ifn) = rmod.imports.get_mut(cg_addr) {
                            // Handle imports
                            ifn.rfn.borrow_mut().cgid = nidx;
                        }
                    }
                }
            }

            if self.load_datarefs {
                for info in &aux_info {
                    if let Some(mut rfn) = rmod.functions.get_mut(&info.offset.unwrap()) {
                        rfn.datarefs = info.datarefs.clone().unwrap_or_default();
                    }
                }
            }

            if self.load_locals {
                unimplemented!()
            }
        }

        if self.build_callgraph && self.assume_cc {
            for (off, rfn) in rmod.functions.iter_mut() {
                ModuleLoader::init_fn_bindings(rfn, &sub_reg_f);
            }
            // Do the same for imports.
            for (plt, ifn) in rmod.imports.iter_mut() {
                ModuleLoader::init_fn_bindings(&mut ifn.rfn.borrow_mut(), &sub_reg_f);
            }

            llanalyzer::init_call_ctx(&mut rmod);
        }

        // Set source
        rmod.source = Some(Rc::clone(&source));

        rmod
    }

    /// Setup a function loader for the module
    pub fn function_loader(mut self, f: FunctionLoader<'a>) -> ModuleLoader<'a> {
        self.floader = Some(f);
        self
    }

    /// Filter identified/loaded functions based on filter function
    pub fn filter(mut self, f: fn(&RadecoFunction) -> bool) -> ModuleLoader<'a> {
        self.filter = Some(f);
        self
    }
}

#[derive(Default)]
/// Breaks down `RadecoModule` into functions
/// Performs low-level function identification.
pub struct FunctionLoader<'a> {
    source: Option<Rc<Source>>,
    strategies: Vec<&'a PredicatedLoader>,
}

pub trait PredicatedLoader {
    /// Decide if the current loading strategy should be executed, based on previous results.
    ///
    /// Defaults to true, need not implement if the Loader is not conditional
    fn predicate(&self, x: &FLResult) -> bool {
        true
    }
    /// Function to execute to breakdown the `RadecoModule`
    fn strategy(&self,
                source: Option<&Rc<Source>>,
                last: &FLResult,
                rmod: &RadecoModule)
                -> FLResult;
}

impl<T> PredicatedLoader for T
    where T: Fn(Option<&Rc<Source>>, &FLResult, &RadecoModule) -> FLResult
{
    fn strategy(&self,
                source: Option<&Rc<Source>>,
                last: &FLResult,
                rmod: &RadecoModule)
                -> FLResult {
        self(source, last, rmod)
    }
}

#[derive(Default, Clone)]
/// Results from `FunctionLoader`
pub struct FLResult {
    /// Map from identified function offset to the RadecoFunction instance
    functions: BTreeMap<u64, RadecoFunction>,
    /// Number of functions identified
    new: u32,
}

impl<'a> FunctionLoader<'a> {
    /// Add a function identification strategy to the pipeline
    pub fn strategy<'b: 'a>(mut self, strat: &'b PredicatedLoader) -> FunctionLoader<'a> {
        self.strategies.push(strat);
        self
    }

    /// Kick everything off and breakdown a radeco module into functions
    pub fn load(&mut self, rmod: &RadecoModule) -> FLResult {
        self.strategies.iter().fold(FLResult::default(), |mut acc, f| {
            if f.predicate(&acc) {
                let fl = f.strategy(self.source.as_ref(), &acc, rmod);
                acc.new += fl.new;
                acc.functions.extend(fl.functions.into_iter());
            }
            acc
        })
    }

    /// Include default strategies to identify functions in the loaded binary
    pub fn include_defaults(mut self) -> FunctionLoader<'a> {
        // TODO: Append these to the front
        self.strategies.push(&loader_defaults::strat_use_symbols);
        self.strategies.push(&loader_defaults::strat_use_source);
        self
    }
}

impl RadecoProject {
    pub fn new() -> RadecoProject {
        RadecoProject {
            modules: Vec::new(),
            reginfo: Arc::new(SubRegisterFile::default()),
        }
    }

    pub fn regfile(&self) -> &Arc<SubRegisterFile> {
        &self.reginfo
    }

    pub fn nth_module(&self, idx: usize) -> Option<&RadecoModule> {
        if self.modules.len() > idx {
            Some(&self.modules[idx])
        } else {
            None
        }
    }

    pub fn nth_module_mut<'a>(&mut self, idx: usize) -> Option<&mut RadecoModule> {
        if self.modules.len() > idx {
            Some(&mut self.modules[idx])
        } else {
            None
        }
    }

    pub fn iter<'a>(&'a self) -> ModuleIter<'a> {
        ModuleIter {
            project: &self,
            iter: self.modules.iter(),
        }
    }

    pub fn iter_mut<'a>(&'a mut self) -> ModuleIterMut<'a> {
        ModuleIterMut { iter: self.modules.iter_mut() }
    }
}

impl RadecoModule {
    pub fn new(path: String) -> RadecoModule {
        let mut rmod = RadecoModule::default();
        rmod.name = Cow::from(path.clone());
        rmod
    }

    pub fn function(&self, offset: u64) -> Option<&RadecoFunction> {
        self.functions.get(&offset)
    }

    pub fn function_mut(&mut self, offset: u64) -> Option<&mut RadecoFunction> {
        self.functions.get_mut(&offset)
    }

    pub fn iter<'a>(&'a self) -> FunctionIter<'a> {
        FunctionIter {
            module: &self,
            iter: self.functions.iter(),
        }
    }

    pub fn iter_mut<'a>(&'a mut self) -> FunctionIterMut<'a> {
        FunctionIterMut { iter: self.functions.iter_mut() }
    }

    pub fn sections(&self) -> &Arc<Vec<LSectionInfo>> {
        &self.sections
    }

    pub fn callgraph(&self) -> &CallGraph {
        &self.callgraph
    }
}

impl RadecoFunction {
    pub fn new() -> RadecoFunction {
        RadecoFunction::default()
    }

    pub fn instructions(&self) -> &[LOpInfo] {
        self.instructions.as_slice()
    }

    pub fn ssa(&self) -> &SSAStorage {
        &self.ssa
    }

    pub fn ssa_mut(&mut self) -> &mut SSAStorage {
        &mut self.ssa
    }

    /// Returns the id in the call graph for this function.
    pub fn cgid(&self) -> NodeIndex {
        self.cgid
    }

    pub fn bindings(&self) -> &VarBindings {
        &self.bindings
    }
}

#[derive(Clone, Debug, Default)]
pub struct CallContextInfo {
    /// NodeIndex mapping from a node in the caller's context to a node in callee's context
    pub map: Vec<(NodeIndex, NodeIndex)>,
    /// NodeIndex corresponding to callsite (`OpCall`) in the caller context
    pub csite_node: NodeIndex,
    /// Address of callsite
    pub csite: u64,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_fn_loader() {
        // let ld = |x: &FLResult, y: &RadecoModule| -> FLResult { unimplemented!() };

        // let mut fl = FunctionLoader::default();
        // fl.strategy(&ld);
    }
}
