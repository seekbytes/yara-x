/*! This module implements the YARA scanner.

The scanner takes the rules produces by the compiler and scans data with them.
*/

use std::fs;
use std::io::Read;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::ptr::{null, NonNull};
use std::rc::Rc;
use std::slice::Iter;

use bitvec::prelude::*;
use fmmap::{MmapFile, MmapFileExt};
use rustc_hash::FxHashMap;
use thiserror::Error;
use wasmtime::{
    AsContext, AsContextMut, Global, GlobalType, MemoryType, Mutability,
    Store, TypedFunc, Val, ValType,
};

use crate::compiler::{IdentId, PatternId, RuleId, RuleInfo, Rules};
use crate::string_pool::BStringPool;
use crate::types::{Struct, TypeValue};
use crate::variables::VariableError;
use crate::wasm::MATCHING_RULES_BITMAP_BASE;
use crate::{modules, wasm, Variable};

pub(crate) use crate::scanner::context::*;
pub use crate::scanner::matches::*;

mod context;
mod matches;

#[cfg(test)]
mod tests;

/// Error returned by [`Scanner::scan_file`].
#[derive(Error, Debug)]
pub enum ScanError {
    #[error("can not open `{path}`: {source}")]
    OpenError { path: PathBuf, source: std::io::Error },
    #[error("can not map `{path}`: {source}")]
    MapError { path: PathBuf, source: fmmap::error::Error },
}

/// Scans data with already compiled YARA rules.
///
/// The scanner receives a set of compiled [`Rules`] and scans data with those
/// rules. The same scanner can be used for scanning multiple files or in-memory
/// data sequentially, but you need multiple scanners for scanning in parallel.
pub struct Scanner<'r> {
    wasm_store: Pin<Box<Store<ScanContext<'r>>>>,
    wasm_main_fn: TypedFunc<(), ()>,
    filesize: Global,
}

impl<'r> Scanner<'r> {
    /// Creates a new scanner.
    pub fn new(rules: &'r Rules) -> Self {
        // The ScanContext structure belongs to the WASM store, but at the same
        // time it must have a reference to the store because it is required
        // for accessing the WASM memory from code that only has a reference
        // to ScanContext. This kind of circular data structures are not
        // natural to Rust, and they can be achieved either by using unsafe
        // pointers, or by using Rc::Weak. In this case we are storing a pointer
        // to the store in ScanContext. The store is put into a pinned box in
        // order to make sure that it doesn't move from its original memory
        // address and the pointer remains valid.
        let mut wasm_store = Box::pin(Store::new(
            &crate::wasm::ENGINE,
            ScanContext {
                wasm_store: NonNull::dangling(),
                compiled_rules: rules,
                string_pool: BStringPool::new(),
                current_struct: None,
                root_struct: rules.globals(),
                scanned_data: null(),
                scanned_data_len: 0,
                rules_matching: Vec::new(),
                global_rules_matching: FxHashMap::default(),
                main_memory: None,
                vars_stack: Vec::new(),
                module_outputs: FxHashMap::default(),
                pattern_matches: FxHashMap::default(),
                unconfirmed_matches: FxHashMap::default(),
            },
        ));

        // Initialize the ScanContext.wasm_store pointer that was initially
        // dangling.
        wasm_store.data_mut().wasm_store =
            NonNull::from(wasm_store.as_ref().deref());

        // Global variable that will hold the value for `filesize`. This is
        // initialized to 0 because the file size is not known until some
        // data is scanned.
        let filesize = Global::new(
            wasm_store.as_context_mut(),
            GlobalType::new(ValType::I64, Mutability::Var),
            Val::I64(0),
        )
        .unwrap();

        let num_rules = rules.rules().len() as u32;
        let num_patterns = rules.num_patterns() as u32;

        // Compute the base offset for the bitmap that contains matching
        // information for patterns. This bitmap has 1 bit per pattern,
        // the N-th bit is set if pattern with PatternId = N matched. The
        // bitmap starts right after the bitmap that contains matching
        // information for rules.
        let matching_patterns_bitmap_base =
            wasm::MATCHING_RULES_BITMAP_BASE as u32 + num_rules / 8 + 1;

        // Compute the required memory size in 64KB pages.
        let mem_size =
            matching_patterns_bitmap_base + num_patterns / 8 % 65536 + 1;

        let matching_patterns_bitmap_base = Global::new(
            wasm_store.as_context_mut(),
            GlobalType::new(ValType::I32, Mutability::Const),
            Val::I32(matching_patterns_bitmap_base as i32),
        )
        .unwrap();

        // Create module's main memory.
        let main_memory = wasmtime::Memory::new(
            wasm_store.as_context_mut(),
            MemoryType::new(mem_size, None),
        )
        .unwrap();

        // Instantiate the module. This takes the wasm code provided by the
        // `compiled_wasm_mod` function and links its imported functions with
        // the implementations that YARA provides (see wasm.rs).
        let wasm_instance = wasm::new_linker()
            .define(wasm_store.as_context(), "yara_x", "filesize", filesize)
            .unwrap()
            .define(
                wasm_store.as_context(),
                "yara_x",
                "matching_patterns_bitmap_base",
                matching_patterns_bitmap_base,
            )
            .unwrap()
            .define(
                wasm_store.as_context(),
                "yara_x",
                "main_memory",
                main_memory,
            )
            .unwrap()
            .instantiate(
                wasm_store.as_context_mut(),
                rules.compiled_wasm_mod(),
            )
            .unwrap();

        // Obtain a reference to the "main" function exported by the module.
        let wasm_main_fn = wasm_instance
            .get_typed_func::<(), ()>(wasm_store.as_context_mut(), "main")
            .unwrap();

        wasm_store.data_mut().main_memory = Some(main_memory);

        Self { wasm_store, wasm_main_fn, filesize }
    }

    /// Scans a file.
    pub fn scan_file<'s, P>(
        &'s mut self,
        path: P,
    ) -> Result<ScanResults<'s, 'r>, ScanError>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();

        let mut file = fs::File::open(path).map_err(|err| {
            ScanError::OpenError { path: path.to_path_buf(), source: err }
        })?;

        let size = file.metadata().map(|m| m.len()).unwrap_or(0);

        let mut buffered_file;
        let mapped_file;

        // For files smaller than ~500MB reading the whole file is faster than
        // using a memory-mapped file.
        let data = if size < 500_000_000 {
            buffered_file = Vec::with_capacity(size as usize);
            file.read_to_end(&mut buffered_file).map_err(|err| {
                ScanError::OpenError { path: path.to_path_buf(), source: err }
            })?;
            buffered_file.as_slice()
        } else {
            mapped_file = MmapFile::open(path).map_err(|err| {
                ScanError::MapError { path: path.to_path_buf(), source: err }
            })?;
            mapped_file.as_slice()
        };

        Ok(self.scan(data))
    }

    /// Scans in-memory data.
    pub fn scan<'s>(&'s mut self, data: &[u8]) -> ScanResults<'s, 'r> {
        // Clear information about matches found in a previous scan, if any.
        self.clear_matches();

        // Set the global variable `filesize` to the size of the scanned data.
        self.filesize
            .set(self.wasm_store.as_context_mut(), Val::I64(data.len() as i64))
            .unwrap();

        let ctx = self.wasm_store.data_mut();

        ctx.scanned_data = data.as_ptr();
        ctx.scanned_data_len = data.len();

        // If the string pool is too large, destroy it and create a new empty
        // one. Re-using the same string pool across multiple scans improves
        // performance, but the price to pay is the accumulation of strings in
        // the pool.
        if ctx.string_pool.size() > 1_000_000 {
            ctx.string_pool = BStringPool::new();
        }

        for module_name in ctx.compiled_rules.imports() {
            // Lookup the module in the list of built-in modules.
            let module = modules::BUILTIN_MODULES.get(module_name).unwrap();

            // Call the module's main function, if any. This function returns
            // a data structure serialized as a protocol buffer. The format of
            // the data is specified by the .proto file associated to the
            // module.
            let module_output = if let Some(main_fn) = module.main_fn {
                main_fn(ctx)
            } else {
                // Implement the case in which the module doesn't have a main
                // function and the serialized data should be provided by the
                // user.
                todo!()
            };

            // Make sure that the module is returning a protobuf message of the
            // expected type.
            debug_assert_eq!(
                module_output.descriptor_dyn().full_name(),
                module.root_struct_descriptor.full_name(),
                "main function of module `{}` must return `{}`, but returned `{}`",
                module_name,
                module.root_struct_descriptor.full_name(),
                module_output.descriptor_dyn().full_name(),
            );

            // Make sure that the module is returning a protobuf message where
            // all required fields are initialized. This only applies to proto2,
            // proto3 doesn't have "required" fields, all fields are optional.
            debug_assert!(
                module_output.is_initialized_dyn(),
                "module `{}` returned a protobuf `{}` where some required fields are not initialized ",
                module_name,
                module.root_struct_descriptor.full_name()
            );

            // When constant folding is enabled we don't need to generate
            // structure fields for enums. This is because during the
            // optimization process symbols like MyEnum.ENUM_ITEM are resolved
            // to their constant values at compile time. In other words, the
            // compiler determines that MyEnum.ENUM_ITEM is equal to some value
            // X, and uses that value in the generated code.
            //
            // However, without constant folding, enums are treated as any
            // other field in a struct, and their values are determined at scan
            // time. For that reason these fields must be generated for enums
            // when constant folding is disabled.
            let generate_fields_for_enums =
                !cfg!(feature = "constant-folding");

            let module_struct = Struct::from_proto_msg(
                module_output.deref(),
                generate_fields_for_enums,
            );

            // Update the module's output in stored in ScanContext.
            ctx.module_outputs.insert(
                module_output.descriptor_dyn().full_name().to_string(),
                module_output,
            );

            // The data structure obtained from the module is added to the
            // root structure. Any data from previous scans will be replaced
            // with the new data structure.
            ctx.root_struct.add_field(
                module_name,
                TypeValue::Struct(Rc::new(module_struct)),
            );
        }

        // Invoke the main function, which evaluates the rules' conditions. It
        // triggers the Aho-Corasick scanning phase only if necessary. See
        // ScanContext::search_for_patterns.
        self.wasm_main_fn.call(self.wasm_store.as_context_mut(), ()).unwrap();

        let ctx = self.wasm_store.data_mut();

        // Set pointer to data back to nil. This means that accessing
        // `scanned_data` from within `ScanResults` is not possible.
        ctx.scanned_data = null();
        ctx.scanned_data_len = 0;

        // Clear the value of `current_struct` as it may contain a reference
        // to some struct.
        ctx.current_struct = None;

        // Move all the rules in `global_rules_matching` to `rules_matching`,
        // leaving `global_rules_matching` empty.
        for rules in ctx.global_rules_matching.values_mut() {
            ctx.rules_matching.append(rules)
        }

        ScanResults::new(ctx)
    }

    /// Sets the value of a global variable.
    ///
    /// The variable must has been previously defined by calling
    /// [`crate::Compiler::define_global`], and the type it has during the definition
    /// must match the type of the new value (`T`).
    ///
    /// The variable will retain the new value in subsequent scans, unless this
    /// function is called again for setting a new value.
    pub fn set_global<T: Into<Variable>>(
        &mut self,
        ident: &str,
        value: T,
    ) -> Result<&mut Self, VariableError> {
        let ctx = self.wasm_store.data_mut();

        if let Some(field) = ctx.root_struct.field_by_name_mut(ident) {
            let variable: Variable = value.into();
            let type_value: TypeValue = variable.into();
            // The new type must match the the old one.
            if type_value.eq_type(&field.type_value) {
                field.type_value = type_value;
            } else {
                return Err(VariableError::InvalidType {
                    variable: ident.to_string(),
                    expected_type: field.type_value.ty().to_string(),
                    actual_type: type_value.ty().to_string(),
                });
            }
        } else {
            return Err(VariableError::Undeclared(ident.to_string()));
        }

        Ok(self)
    }

    // Clear information about previous matches.
    fn clear_matches(&mut self) {
        let ctx = self.wasm_store.data_mut();
        let num_rules = ctx.compiled_rules.rules().len();
        let num_patterns = ctx.compiled_rules.num_patterns();

        // Clear the unconfirmed matches.
        for (_, matches) in ctx.unconfirmed_matches.iter_mut() {
            matches.clear()
        }

        // If some pattern or rule matched, clear the matches. Notice that a
        // rule may match without any pattern being matched, because there
        // there are rules without patterns, or that match if the pattern is
        // not found.
        if !ctx.pattern_matches.is_empty() || !ctx.rules_matching.is_empty() {
            // The hash map that tracks the pattern matches is not completely
            // cleared with pattern_matches.clear() because that would cause
            // that all the vectors are deallocated. Instead, each of the
            // vectors are cleared individually, which removes the items
            // while maintaining the vector capacity. This way the vector may
            // be reused in later scans without memory allocations.
            for (_, matches) in ctx.pattern_matches.iter_mut() {
                matches.clear()
            }

            // Clear the list of matching rules.
            ctx.rules_matching.clear();

            let mem = ctx
                .main_memory
                .unwrap()
                .data_mut(self.wasm_store.as_context_mut());

            // Starting at MATCHING_RULES_BITMAP in main memory there's a bitmap
            // were the N-th bit indicates if the rule with ID = N matched or not,
            // If some rule matched in a previous call the bitmap will contain some
            // bits set to 1 and need to be cleared.
            let base = MATCHING_RULES_BITMAP_BASE as usize;
            let bitmap = BitSlice::<_, Lsb0>::from_slice_mut(
                &mut mem[base..base
                    + (num_rules / 8 + 1)
                    + (num_patterns / 8 + 1)],
            );

            // Set to zero all bits in the bitmap.
            bitmap.fill(false);
        }
    }
}

/// Results of a scan operation.
///
/// Allows iterating over both the matching and non-matching rules. For better
/// ergonomics it implements the [`IntoIterator`] trait, which allows iterating
/// over the matching rules in a `for` loop like shown below.
///
/// ```rust
/// # use yara_x;
/// let rules = yara_x::compile(
///     r#"rule test {
///         strings:
///            $a = "foo"
///         condition:
///            $a
///     }"#,
/// ).unwrap();
///
/// for matching_rule in yara_x::Scanner::new(&rules).scan(b"foobar") {
///     // do something with the matching rule ...
/// }
/// ```
pub struct ScanResults<'s, 'r> {
    ctx: &'s ScanContext<'r>,
}

impl<'s, 'r> ScanResults<'s, 'r> {
    fn new(ctx: &'s ScanContext<'r>) -> Self {
        Self { ctx }
    }

    /// Returns an iterator that yields the matching rules in arbitrary order.
    pub fn matching_rules(&self) -> MatchingRules<'s, 'r> {
        MatchingRules::new(self.ctx)
    }

    /// Returns an iterator that yields the non-matching rules in arbitrary order.
    pub fn non_matching_rules(&self) -> NonMatchingRules<'s, 'r> {
        NonMatchingRules::new(self.ctx)
    }
}

impl<'s, 'r> IntoIterator for ScanResults<'s, 'r> {
    type Item = Rule<'s, 'r>;
    type IntoIter = MatchingRules<'s, 'r>;

    /// Consumes the scan results and returns a [`MatchingRules`] iterator.
    fn into_iter(self) -> Self::IntoIter {
        self.matching_rules()
    }
}

/// Iterator that yields the rules that matched during a scan.
pub struct MatchingRules<'s, 'r> {
    ctx: &'s ScanContext<'r>,
    iterator: Iter<'s, RuleId>,
}

impl<'s, 'r> MatchingRules<'s, 'r> {
    fn new(ctx: &'s ScanContext<'r>) -> Self {
        Self { ctx, iterator: ctx.rules_matching.iter() }
    }
}

impl<'s, 'r> Iterator for MatchingRules<'s, 'r> {
    type Item = Rule<'s, 'r>;

    fn next(&mut self) -> Option<Self::Item> {
        let rule_id = *self.iterator.next()?;
        let rules = self.ctx.compiled_rules;
        let rule_info = rules.get(rule_id);

        Some(Rule { rule_info, rules, ctx: self.ctx })
    }
}

impl<'s, 'r> ExactSizeIterator for MatchingRules<'s, 'r> {
    #[inline]
    fn len(&self) -> usize {
        self.iterator.len()
    }
}

/// Iterator that yields the rules that didn't match during a scan.
pub struct NonMatchingRules<'s, 'r> {
    ctx: &'s ScanContext<'r>,
    iterator: bitvec::slice::IterZeros<'s, u8, Lsb0>,
    len: usize,
}

impl<'s, 'r> NonMatchingRules<'s, 'r> {
    fn new(ctx: &'s ScanContext<'r>) -> Self {
        let num_rules = ctx.compiled_rules.rules().len();
        let main_memory =
            ctx.main_memory.unwrap().data(unsafe { ctx.wasm_store.as_ref() });

        let base = MATCHING_RULES_BITMAP_BASE as usize;

        // Create a BitSlice that covers the region of main memory containing
        // the bitmap that tells which rules matched and which did not.
        let matching_rules_bitmap = BitSlice::<_, Lsb0>::from_slice(
            &main_memory[base..base + num_rules / 8 + 1],
        );

        // The BitSlice will cover more bits than necessary, for example, if
        // there are 3 rules the BitSlice will have 8 bits because it is
        // created from a u8 slice that has 1 byte. Here we make sure that
        // the BitSlice has exactly as many bits as existing rules.
        let matching_rules_bitmap = &matching_rules_bitmap[0..num_rules];

        Self {
            ctx,
            iterator: matching_rules_bitmap.iter_zeros(),
            // The number of non-matching rules is the total minus the number of
            // matching rules.
            len: ctx.compiled_rules.rules().len() - ctx.rules_matching.len(),
        }
    }
}

impl<'s, 'r> Iterator for NonMatchingRules<'s, 'r> {
    type Item = Rule<'s, 'r>;

    fn next(&mut self) -> Option<Self::Item> {
        self.len = self.len.saturating_sub(1);
        let rule_id = RuleId::from(self.iterator.next()?);
        let rules = self.ctx.compiled_rules;
        let rule_info = rules.get(rule_id);

        Some(Rule { rule_info, rules, ctx: self.ctx })
    }
}

impl<'s, 'r> ExactSizeIterator for NonMatchingRules<'s, 'r> {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }
}

/// A structure that describes a rule.
pub struct Rule<'s, 'r> {
    ctx: &'s ScanContext<'r>,
    pub(crate) rules: &'r Rules,
    pub(crate) rule_info: &'r RuleInfo,
}

impl<'s, 'r> Rule<'s, 'r> {
    /// Returns the rule's name.
    pub fn name(&self) -> &str {
        self.rules.ident_pool().get(self.rule_info.ident_id).unwrap()
    }

    /// Returns the rule's namespace.
    pub fn namespace(&self) -> &str {
        self.rules.ident_pool().get(self.rule_info.namespace_ident_id).unwrap()
    }

    /// Returns the patterns defined by this rule.
    pub fn patterns(&self) -> Patterns<'s, 'r> {
        Patterns { ctx: self.ctx, iterator: self.rule_info.patterns.iter() }
    }
}

/// An iterator that returns the patterns defined by a rule.
pub struct Patterns<'s, 'r> {
    ctx: &'s ScanContext<'r>,
    iterator: Iter<'s, (IdentId, PatternId)>,
}

impl<'s, 'r> Iterator for Patterns<'s, 'r> {
    type Item = Pattern<'s, 'r>;

    fn next(&mut self) -> Option<Self::Item> {
        let (ident_id, pattern_id) = self.iterator.next()?;
        Some(Pattern {
            ctx: self.ctx,
            pattern_id: *pattern_id,
            ident_id: *ident_id,
        })
    }
}

/// Represents a pattern defined by a rule.
pub struct Pattern<'s, 'r> {
    ctx: &'s ScanContext<'r>,
    pattern_id: PatternId,
    ident_id: IdentId,
}

impl<'r> Pattern<'_, 'r> {
    /// Returns the pattern's identifier (e.g: $a, $b).
    pub fn identifier(&self) -> &'r str {
        self.ctx.compiled_rules.ident_pool().get(self.ident_id).unwrap()
    }

    /// Returns the matches found for this pattern.
    pub fn matches(&self) -> Matches {
        Matches {
            iterator: self
                .ctx
                .pattern_matches
                .get(&self.pattern_id)
                .map(|match_list| match_list.iter()),
        }
    }
}

/// Iterator that returns the matches for a pattern.
pub struct Matches<'a> {
    iterator: Option<Iter<'a, Match>>,
}

impl Iterator for Matches<'_> {
    type Item = Match;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(iter) = &mut self.iterator {
            let next = iter.next()?;
            Some(next.clone())
        } else {
            None
        }
    }
}

pub(crate) type RuntimeStringId = u32;
