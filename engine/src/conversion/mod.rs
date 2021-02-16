// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod analysis;
mod api;
mod codegen_cpp;
mod codegen_rs;
#[cfg(test)]
mod conversion_tests;
mod parse;
mod utilities;

use analysis::fun::FnAnalyzer;
pub(crate) use api::ConvertError;
use autocxx_parser::TypeDatabase;
pub(crate) use codegen_cpp::type_to_cpp::type_to_cpp;
pub(crate) use codegen_cpp::CppCodeGenerator;
pub(crate) use codegen_cpp::CppCodegenResults;
use parse::type_converter::TypeConverter;
use syn::{Item, ItemMod};

use crate::UnsafePolicy;

use self::{
    analysis::{
        gc::filter_apis_by_following_edges_from_allowlist,
        pod::{analyze_pod_apis, identify_byvalue_safe_types},
    },
    codegen_rs::RsCodeGenerator,
    parse::ParseBindgen,
};

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
/// In fact, most of the actual operation happens within an
/// individual `BridgeConversion`.
///
/// # Flexibility in handling bindgen output
///
/// autocxx is inevitably tied to the details of the bindgen output;
/// e.g. the creation of a 'root' mod when namespaces are enabled.
/// At the moment this crate takes the view that it's OK to panic
/// if the bindgen output is not as expected. It may be in future that
/// we need to be a bit more graceful, but for now, that's OK.
pub(crate) struct BridgeConverter<'a> {
    include_list: &'a [String],
    type_database: &'a TypeDatabase,
}

/// C++ and Rust code generation output.
pub(crate) struct CodegenResults {
    pub(crate) rs: Vec<Item>,
    pub(crate) cpp: Option<CppCodegenResults>,
}

impl<'a> BridgeConverter<'a> {
    pub fn new(include_list: &'a [String], type_database: &'a TypeDatabase) -> Self {
        Self {
            include_list,
            type_database,
        }
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    pub(crate) fn convert(
        &self,
        mut bindgen_mod: ItemMod,
        exclude_utilities: bool,
        unsafe_policy: UnsafePolicy,
        inclusions: String,
    ) -> Result<CodegenResults, ConvertError> {
        match &mut bindgen_mod.content {
            None => Err(ConvertError::NoContent),
            Some((_, items)) => {
                // First let's look at this bindgen mod to find the items
                // which we'll need to convert.
                let items_to_process = items.drain(..).collect();
                // And ensure that the namespace/mod structure is as expected.
                let items_in_root = Self::find_items_in_root(items_to_process)?;
                // Now, let's confirm that the items requested by the user to be
                // POD really are POD, and thusly mark any dependent types.
                let byvalue_checker =
                    identify_byvalue_safe_types(&items_in_root, &self.type_database)?;
                // Create a database to track all our types.
                let mut type_converter = TypeConverter::new();
                // Parse the bindgen mod.
                let parser = ParseBindgen::new(&self.type_database, &mut type_converter);
                let parse_results = parser.convert_items(items_in_root, exclude_utilities)?;
                // The code above will have contributed lots of Apis to self.apis.
                // Now analyze which of them can be POD (i.e. trivial, movable, pass-by-value
                // versus which need to be opaque).
                let analyzed_apis =
                    analyze_pod_apis(parse_results.apis, &byvalue_checker, &mut type_converter)?;
                // Next, figure out how we materialize different functions.
                // Some will be simple entries in the cxx::bridge module; others will
                // require C++ wrapper functions.
                let analyzed_apis = FnAnalyzer::analyze_functions(
                    analyzed_apis,
                    unsafe_policy,
                    &mut type_converter,
                    &byvalue_checker,
                    self.type_database,
                )?;
                // We now garbage collect the ones we don't need...
                let mut analyzed_apis = filter_apis_by_following_edges_from_allowlist(
                    analyzed_apis,
                    &self.type_database,
                );
                // Determine what variably-sized C types (e.g. int) we need to include
                analysis::ctypes::append_ctype_information(&mut analyzed_apis);
                // And finally pass them to the code gen phases, which outputs
                // code suitable for cxx to consume.
                let cpp = CppCodeGenerator::generate_cpp_code(inclusions, &analyzed_apis);
                let rs = RsCodeGenerator::generate_rs_code(
                    analyzed_apis,
                    self.include_list,
                    parse_results.use_stmts_by_mod,
                    bindgen_mod,
                );
                Ok(CodegenResults { rs, cpp })
            }
        }
    }

    fn find_items_in_root(items: Vec<Item>) -> Result<Vec<Item>, ConvertError> {
        for item in items {
            match item {
                Item::Mod(root_mod) => {
                    // With namespaces enabled, bindgen always puts everything
                    // in a mod called 'root'. We don't want to pass that
                    // onto cxx, so jump right into it.
                    assert!(root_mod.ident == "root");
                    if let Some((_, items)) = root_mod.content {
                        return Ok(items);
                    }
                }
                _ => return Err(ConvertError::UnexpectedOuterItem),
            }
        }
        Ok(Vec::new())
    }
}
