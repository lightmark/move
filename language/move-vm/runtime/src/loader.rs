// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    logging::expect_no_verification_errors,
    native_functions::{NativeFunction, NativeFunctions},
    session::LoadedFunctionInstantiation,
};
use move_binary_format::{
    access::{ModuleAccess, ScriptAccess},
    binary_views::BinaryIndexedView,
    errors::{verification_error, Location, PartialVMError, PartialVMResult, VMResult},
    file_format::{
        AbilitySet, Bytecode, CompiledModule, CompiledScript, Constant, ConstantPoolIndex,
        FieldHandleIndex, FieldInstantiationIndex, FunctionDefinition, FunctionDefinitionIndex,
        FunctionHandleIndex, FunctionInstantiationIndex, Signature, SignatureIndex, SignatureToken,
        StructDefInstantiationIndex, StructDefinition, StructDefinitionIndex,
        StructFieldInformation, TableIndex,
    },
    IndexKind,
};
use move_bytecode_verifier::{self, cyclic_dependencies, dependencies};
use move_core_types::{
    identifier::{IdentStr, Identifier},
    language_storage::{ModuleId, StructTag, TypeTag},
    value::{MoveStructLayout, MoveTypeLayout},
    vm_status::StatusCode,
};
use move_vm_types::{
    data_store::DataStore,
    loaded_data::runtime_types::{CachedStructIndex, StructType, Type},
};
use parking_lot::RwLock;
use sha3::{Digest, Sha3_256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Debug,
    hash::Hash,
    sync::Arc,
};
use tracing::error;

type ScriptHash = [u8; 32];

// A simple cache that offers both a HashMap and a Vector lookup.
// Values are forced into a `Arc` so they can be used from multiple thread.
// Access to this cache is always under a `RwLock`.
struct BinaryCache<K, V> {
    id_map: HashMap<K, usize>,
    binaries: Vec<Arc<V>>,
}

impl<K, V> BinaryCache<K, V>
where
    K: Eq + Hash,
{
    fn new() -> Self {
        Self {
            id_map: HashMap::new(),
            binaries: vec![],
        }
    }

    fn insert(&mut self, key: K, binary: V) -> &Arc<V> {
        self.binaries.push(Arc::new(binary));
        let idx = self.binaries.len() - 1;
        self.id_map.insert(key, idx);
        self.binaries
            .last()
            .expect("BinaryCache: last() after push() impossible failure")
    }

    fn get(&self, key: &K) -> Option<&Arc<V>> {
        self.id_map.get(key).and_then(|idx| self.binaries.get(*idx))
    }
}

// A script cache is a map from the hash value of a script and the `Script` itself.
// Script are added in the cache once verified and so getting a script out the cache
// does not require further verification (except for parameters and type parameters)
struct ScriptCache {
    scripts: BinaryCache<ScriptHash, Script>,
}

impl ScriptCache {
    fn new() -> Self {
        Self {
            scripts: BinaryCache::new(),
        }
    }

    fn get(&self, hash: &ScriptHash) -> Option<(Arc<Function>, Vec<Type>, Vec<Type>)> {
        self.scripts.get(hash).map(|script| {
            (
                script.entry_point(),
                script.parameter_tys.clone(),
                script.return_tys.clone(),
            )
        })
    }

    fn insert(
        &mut self,
        hash: ScriptHash,
        script: Script,
    ) -> (Arc<Function>, Vec<Type>, Vec<Type>) {
        match self.get(&hash) {
            Some(cached) => cached,
            None => {
                let script = self.scripts.insert(hash, script);
                (
                    script.entry_point(),
                    script.parameter_tys.clone(),
                    script.return_tys.clone(),
                )
            }
        }
    }
}

// A ModuleCache is the core structure in the Loader.
// It holds all Modules, Types and Functions loaded.
// Types and Functions are pushed globally to the ModuleCache.
// All accesses to the ModuleCache are under lock (exclusive).
pub struct ModuleCache {
    modules: BinaryCache<ModuleId, Module>,
    structs: Vec<Arc<StructType>>,
    functions: Vec<Arc<Function>>,
}

impl ModuleCache {
    fn new() -> Self {
        Self {
            modules: BinaryCache::new(),
            structs: vec![],
            functions: vec![],
        }
    }

    //
    // Common "get" operations
    //

    // Retrieve a module by `ModuleId`. The module may have not been loaded yet in which
    // case `None` is returned
    fn module_at(&self, id: &ModuleId) -> Option<Arc<Module>> {
        self.modules.get(id).map(Arc::clone)
    }

    // Retrieve a function by index
    fn function_at(&self, idx: usize) -> Arc<Function> {
        Arc::clone(&self.functions[idx])
    }

    // Retrieve a struct by index
    fn struct_at(&self, idx: CachedStructIndex) -> Arc<StructType> {
        Arc::clone(&self.structs[idx.0])
    }

    //
    // Insertion is under lock and it's a pretty heavy operation.
    // The VM is pretty much stopped waiting for this to finish
    //

    fn insert(
        &mut self,
        natives: &NativeFunctions,
        id: ModuleId,
        module: CompiledModule,
    ) -> VMResult<Arc<Module>> {
        if let Some(cached) = self.module_at(&id) {
            return Ok(cached);
        }

        // we need this operation to be transactional, if an error occurs we must
        // leave a clean state
        self.add_module(natives, &module)?;
        match Module::new(module, self) {
            Ok(module) => Ok(Arc::clone(self.modules.insert(id, module))),
            Err((err, module)) => {
                // remove all structs and functions that have been pushed
                let strut_def_count = module.struct_defs().len();
                self.structs.truncate(self.structs.len() - strut_def_count);
                let function_count = module.function_defs().len();
                self.functions
                    .truncate(self.functions.len() - function_count);
                Err(err.finish(Location::Undefined))
            }
        }
    }

    fn add_module(&mut self, natives: &NativeFunctions, module: &CompiledModule) -> VMResult<()> {
        let starting_idx = self.structs.len();
        for (idx, struct_def) in module.struct_defs().iter().enumerate() {
            let st = self.make_struct_type(module, struct_def, StructDefinitionIndex(idx as u16));
            self.structs.push(Arc::new(st));
        }
        self.load_field_types(module, starting_idx).map_err(|err| {
            // clean up the structs that were cached
            self.structs.truncate(starting_idx);
            err.finish(Location::Undefined)
        })?;
        for (idx, func) in module.function_defs().iter().enumerate() {
            let findex = FunctionDefinitionIndex(idx as TableIndex);
            let function = Function::new(natives, findex, func, module);
            self.functions.push(Arc::new(function));
        }
        Ok(())
    }

    fn make_struct_type(
        &self,
        module: &CompiledModule,
        struct_def: &StructDefinition,
        idx: StructDefinitionIndex,
    ) -> StructType {
        let struct_handle = module.struct_handle_at(struct_def.struct_handle);
        let abilities = struct_handle.abilities;
        let name = module.identifier_at(struct_handle.name).to_owned();
        let type_parameters = struct_handle.type_parameters.clone();
        let module = module.self_id();
        StructType {
            fields: vec![],
            abilities,
            type_parameters,
            name,
            module,
            struct_def: idx,
        }
    }

    fn load_field_types(
        &mut self,
        module: &CompiledModule,
        starting_idx: usize,
    ) -> PartialVMResult<()> {
        let mut field_types = vec![];
        for struct_def in module.struct_defs() {
            let fields = match &struct_def.field_information {
                StructFieldInformation::Native => unreachable!("native structs have been removed"),
                StructFieldInformation::Declared(fields) => fields,
            };

            let mut field_tys = vec![];
            for field in fields {
                let ty = self.make_type_while_loading(module, &field.signature.0)?;
                debug_assert!(field_tys.len() < usize::max_value());
                field_tys.push(ty);
            }

            field_types.push(field_tys);
        }
        let mut struct_idx = starting_idx;
        for fields in field_types {
            match Arc::get_mut(&mut self.structs[struct_idx]) {
                Some(struct_type) => struct_type.fields = fields,
                None => {
                    // we have pending references to the `Arc` which is impossible,
                    // given the code that adds the `Arc` is above and no reference to
                    // it should exist.
                    // So in the spirit of not crashing we just rewrite the entire `Arc`
                    // over and log the issue.
                    error!("Arc<StructType> cannot have any live reference while publishing");
                    let mut struct_type = (*self.structs[struct_idx]).clone();
                    struct_type.fields = fields;
                    self.structs[struct_idx] = Arc::new(struct_type);
                }
            }
            struct_idx += 1;
        }
        Ok(())
    }

    // `make_type` is the entry point to "translate" a `SignatureToken` to a `Type`
    fn make_type(&self, module: BinaryIndexedView, tok: &SignatureToken) -> PartialVMResult<Type> {
        self.make_type_internal(module, tok, &|struct_name, module_id| {
            Ok(self.resolve_struct_by_name(struct_name, module_id)?.0)
        })
    }

    // While in the process of loading, and before a `Module` is saved into the cache the loader
    // needs to resolve type references to the module itself (self) "manually"; that is,
    // looping through the types of the module itself
    fn make_type_while_loading(
        &self,
        module: &CompiledModule,
        tok: &SignatureToken,
    ) -> PartialVMResult<Type> {
        let self_id = module.self_id();
        self.make_type_internal(
            BinaryIndexedView::Module(module),
            tok,
            &|struct_name, module_id| {
                if module_id == &self_id {
                    // module has not been published yet, loop through the types
                    for (idx, struct_type) in self.structs.iter().enumerate().rev() {
                        if &struct_type.module != module_id {
                            break;
                        }
                        if struct_type.name.as_ident_str() == struct_name {
                            return Ok(CachedStructIndex(idx));
                        }
                    }
                    Err(
                        PartialVMError::new(StatusCode::TYPE_RESOLUTION_FAILURE).with_message(
                            format!(
                                "Cannot find {:?}::{:?} in publishing module",
                                module_id, struct_name
                            ),
                        ),
                    )
                } else {
                    Ok(self.resolve_struct_by_name(struct_name, module_id)?.0)
                }
            },
        )
    }

    // `make_type_internal` returns a `Type` given a signature and a resolver which
    // is resonsible to map a local struct index to a global one
    fn make_type_internal<F>(
        &self,
        module: BinaryIndexedView,
        tok: &SignatureToken,
        resolver: &F,
    ) -> PartialVMResult<Type>
    where
        F: Fn(&IdentStr, &ModuleId) -> PartialVMResult<CachedStructIndex>,
    {
        let res = match tok {
            SignatureToken::Bool => Type::Bool,
            SignatureToken::U8 => Type::U8,
            SignatureToken::U64 => Type::U64,
            SignatureToken::U128 => Type::U128,
            SignatureToken::Address => Type::Address,
            SignatureToken::Signer => Type::Signer,
            SignatureToken::TypeParameter(idx) => Type::TyParam(*idx as usize),
            SignatureToken::Vector(inner_tok) => {
                let inner_type = self.make_type_internal(module, inner_tok, resolver)?;
                Type::Vector(Box::new(inner_type))
            }
            SignatureToken::Reference(inner_tok) => {
                let inner_type = self.make_type_internal(module, inner_tok, resolver)?;
                Type::Reference(Box::new(inner_type))
            }
            SignatureToken::MutableReference(inner_tok) => {
                let inner_type = self.make_type_internal(module, inner_tok, resolver)?;
                Type::MutableReference(Box::new(inner_type))
            }
            SignatureToken::Struct(sh_idx) => {
                let struct_handle = module.struct_handle_at(*sh_idx);
                let struct_name = module.identifier_at(struct_handle.name);
                let module_handle = module.module_handle_at(struct_handle.module);
                let module_id = ModuleId::new(
                    *module.address_identifier_at(module_handle.address),
                    module.identifier_at(module_handle.name).to_owned(),
                );
                let def_idx = resolver(struct_name, &module_id)?;
                Type::Struct(def_idx)
            }
            SignatureToken::StructInstantiation(sh_idx, tys) => {
                let type_parameters: Vec<_> = tys
                    .iter()
                    .map(|tok| self.make_type_internal(module, tok, resolver))
                    .collect::<PartialVMResult<_>>()?;
                let struct_handle = module.struct_handle_at(*sh_idx);
                let struct_name = module.identifier_at(struct_handle.name);
                let module_handle = module.module_handle_at(struct_handle.module);
                let module_id = ModuleId::new(
                    *module.address_identifier_at(module_handle.address),
                    module.identifier_at(module_handle.name).to_owned(),
                );
                let def_idx = resolver(struct_name, &module_id)?;
                Type::StructInstantiation(def_idx, type_parameters)
            }
        };
        Ok(res)
    }

    // Given a module id, returns whether the module cache has the module or not
    fn has_module(&self, module_id: &ModuleId) -> bool {
        self.modules.id_map.contains_key(module_id)
    }

    // Given a ModuleId::struct_name, retrieve the `StructType` and the index associated.
    // Return and error if the type has not been loaded
    fn resolve_struct_by_name(
        &self,
        struct_name: &IdentStr,
        module_id: &ModuleId,
    ) -> PartialVMResult<(CachedStructIndex, Arc<StructType>)> {
        match self
            .modules
            .get(module_id)
            .and_then(|module| module.struct_map.get(struct_name))
        {
            Some(struct_idx) => Ok((*struct_idx, Arc::clone(&self.structs[struct_idx.0]))),
            None => Err(
                PartialVMError::new(StatusCode::TYPE_RESOLUTION_FAILURE).with_message(format!(
                    "Cannot find {:?}::{:?} in cache",
                    module_id, struct_name
                )),
            ),
        }
    }

    // Given a ModuleId::func_name, retrieve the `StructType` and the index associated.
    // Return and error if the function has not been loaded
    fn resolve_function_by_name(
        &self,
        func_name: &IdentStr,
        module_id: &ModuleId,
    ) -> PartialVMResult<usize> {
        match self
            .modules
            .get(module_id)
            .and_then(|module| module.function_map.get(func_name))
        {
            Some(func_idx) => Ok(*func_idx),
            None => Err(
                PartialVMError::new(StatusCode::FUNCTION_RESOLUTION_FAILURE).with_message(format!(
                    "Cannot find {:?}::{:?} in cache",
                    module_id, func_name
                )),
            ),
        }
    }
}

//
// Loader
//

// A Loader is responsible to load scripts and modules and holds the cache of all loaded
// entities. Each cache is protected by a `RwLock`. Operation in the Loader must be thread safe
// (operating on values on the stack) and when cache needs updating the mutex must be taken.
// The `pub(crate)` API is what a Loader offers to the runtime.
pub(crate) struct Loader {
    scripts: RwLock<ScriptCache>,
    module_cache: RwLock<ModuleCache>,
    type_cache: RwLock<TypeCache>,
    natives: NativeFunctions,
}

impl Loader {
    pub(crate) fn new(natives: NativeFunctions) -> Self {
        Self {
            scripts: RwLock::new(ScriptCache::new()),
            module_cache: RwLock::new(ModuleCache::new()),
            type_cache: RwLock::new(TypeCache::new()),
            natives,
        }
    }

    //
    // Script verification and loading
    //

    // Scripts are verified and dependencies are loaded.
    // Effectively that means modules are cached from leaf to root in the dependency DAG.
    // If a dependency error is found, loading stops and the error is returned.
    // However all modules cached up to that point stay loaded.

    // Entry point for script execution (`MoveVM::execute_script`).
    // Verifies the script if it is not in the cache of scripts loaded.
    // Type parameters are checked as well after every type is loaded.
    pub(crate) fn load_script(
        &self,
        script_blob: &[u8],
        ty_args: &[TypeTag],
        data_store: &impl DataStore,
    ) -> VMResult<(Arc<Function>, LoadedFunctionInstantiation)> {
        // retrieve or load the script
        let mut sha3_256 = Sha3_256::new();
        sha3_256.update(script_blob);
        let hash_value: [u8; 32] = sha3_256.finalize().into();

        let mut scripts = self.scripts.write();
        let (main, parameters, return_) = match scripts.get(&hash_value) {
            Some(cached) => cached,
            None => {
                let ver_script = self.deserialize_and_verify_script(script_blob, data_store)?;
                let script = Script::new(ver_script, &hash_value, &self.module_cache.read())?;
                scripts.insert(hash_value, script)
            }
        };

        // verify type arguments
        let mut type_arguments = vec![];
        for ty in ty_args {
            type_arguments.push(self.load_type(ty, data_store)?);
        }
        self.verify_ty_args(main.type_parameters(), &type_arguments)
            .map_err(|e| e.finish(Location::Script))?;
        let instantiation = LoadedFunctionInstantiation {
            type_arguments,
            parameters,
            return_,
        };
        Ok((main, instantiation))
    }

    // The process of deserialization and verification is not and it must not be under lock.
    // So when publishing modules through the dependency DAG it may happen that a different
    // thread had loaded the module after this process fetched it from storage.
    // Caching will take care of that by asking for each dependency module again under lock.
    fn deserialize_and_verify_script(
        &self,
        script: &[u8],
        data_store: &impl DataStore,
    ) -> VMResult<CompiledScript> {
        let script = match CompiledScript::deserialize(script) {
            Ok(script) => script,
            Err(err) => {
                error!("[VM] deserializer for script returned error: {:?}", err,);
                let msg = format!("Deserialization error: {:?}", err);
                return Err(PartialVMError::new(StatusCode::CODE_DESERIALIZATION_ERROR)
                    .with_message(msg)
                    .finish(Location::Script));
            }
        };

        match self.verify_script(&script) {
            Ok(_) => {
                // verify dependencies
                let loaded_deps = script
                    .immediate_dependencies()
                    .into_iter()
                    .map(|module_id| self.load_module(&module_id, data_store))
                    .collect::<VMResult<_>>()?;
                self.verify_script_dependencies(&script, loaded_deps)?;
                Ok(script)
            }
            Err(err) => {
                error!(
                    "[VM] bytecode verifier returned errors for script: {:?}",
                    err
                );
                Err(err)
            }
        }
    }

    // Script verification steps.
    // See `verify_module()` for module verification steps.
    fn verify_script(&self, script: &CompiledScript) -> VMResult<()> {
        move_bytecode_verifier::verify_script(script)
    }

    fn verify_script_dependencies(
        &self,
        script: &CompiledScript,
        dependencies: Vec<Arc<Module>>,
    ) -> VMResult<()> {
        let mut deps = vec![];
        for dep in &dependencies {
            deps.push(dep.module());
        }
        dependencies::verify_script(script, deps)
    }

    //
    // Module verification and loading
    //

    // Entry point for function execution (`MoveVM::execute_function`).
    // Loading verifies the module if it was never loaded.
    // Type parameters are checked as well after every type is loaded.
    pub(crate) fn load_function(
        &self,
        module_id: &ModuleId,
        function_name: &IdentStr,
        ty_args: &[TypeTag],
        data_store: &impl DataStore,
    ) -> VMResult<(Arc<Module>, Arc<Function>, LoadedFunctionInstantiation)> {
        let module = self.load_module(module_id, data_store)?;
        let idx = self
            .module_cache
            .read()
            .resolve_function_by_name(function_name, module_id)
            .map_err(|err| err.finish(Location::Undefined))?;
        let func = self.module_cache.read().function_at(idx);

        let parameters = func
            .parameters
            .0
            .iter()
            .map(|tok| {
                self.module_cache
                    .read()
                    .make_type(BinaryIndexedView::Module(module.module()), tok)
            })
            .collect::<PartialVMResult<Vec<_>>>()
            .map_err(|err| err.finish(Location::Undefined))?;

        let return_ = func
            .return_
            .0
            .iter()
            .map(|tok| {
                self.module_cache
                    .read()
                    .make_type(BinaryIndexedView::Module(module.module()), tok)
            })
            .collect::<PartialVMResult<Vec<_>>>()
            .map_err(|err| err.finish(Location::Undefined))?;

        // verify type arguments
        let type_arguments = ty_args
            .iter()
            .map(|ty| self.load_type(ty, data_store))
            .collect::<VMResult<Vec<_>>>()?;
        self.verify_ty_args(func.type_parameters(), &type_arguments)
            .map_err(|e| e.finish(Location::Module(module_id.clone())))?;

        let loaded = LoadedFunctionInstantiation {
            type_arguments,
            parameters,
            return_,
        };
        Ok((module, func, loaded))
    }

    // Entry point for module publishing (`MoveVM::publish_module_bundle`).
    //
    // All modules in the bundle to be published must be loadable. This function performs all
    // verification steps to load these modules without actually loading them into the code cache.
    pub(crate) fn verify_module_bundle_for_publication(
        &self,
        modules: &[CompiledModule],
        data_store: &mut impl DataStore,
    ) -> VMResult<()> {
        let mut bundle_unverified: BTreeSet<_> = modules.iter().map(|m| m.self_id()).collect();
        let mut bundle_verified = BTreeMap::new();
        for module in modules {
            let module_id = module.self_id();
            bundle_unverified.remove(&module_id);

            self.verify_module_for_publication(
                module,
                &bundle_verified,
                &bundle_unverified,
                data_store,
            )?;
            bundle_verified.insert(module_id.clone(), module.clone());
        }
        Ok(())
    }

    // A module to be published must be loadable.
    //
    // This step performs all verification steps to load the module without loading it.
    // The module is not added to the code cache. It is simply published to the data cache.
    // See `verify_script()` for script verification steps.
    //
    // If a module `M` is published together with a bundle of modules (i.e., a vector of modules),
    // - the `bundle_verified` argument tracks the modules that have already been verified in the
    //   bundle. Basically, this represents the modules appears before `M` in the bundle vector.
    // - the `bundle_unverified` argument tracks the modules that have not been verified when `M`
    //   is being verified, i.e., the modules appears after `M` in the bundle vector.
    fn verify_module_for_publication(
        &self,
        module: &CompiledModule,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        bundle_unverified: &BTreeSet<ModuleId>,
        data_store: &impl DataStore,
    ) -> VMResult<()> {
        // Performs all verification steps to load the module without loading it, i.e., the new
        // module will NOT show up in `module_cache`. In the module republishing case, it means
        // that the old module is still in the `module_cache`, unless a new Loader is created,
        // which means that a new MoveVM instance needs to be created.
        move_bytecode_verifier::verify_module(module)?;
        self.check_natives(module)?;

        let mut visited = BTreeSet::new();
        let mut friends_discovered = BTreeSet::new();
        visited.insert(module.self_id());
        friends_discovered.extend(module.immediate_friends());

        // downward exploration of the module's dependency graph. Since we know nothing about this
        // target module, we don't know what the module may specify as its dependencies and hence,
        // we allow the loading of dependencies and the subsequent linking to fail.
        self.load_and_verify_dependencies(
            module,
            bundle_verified,
            data_store,
            &mut visited,
            &mut friends_discovered,
            /* allow_dependency_loading_failure */ true,
        )?;

        // upward exploration of the modules's dependency graph. Similar to dependency loading, as
        // we know nothing about this target module, we don't know what the module may specify as
        // its friends and hence, we allow the loading of friends to fail.
        self.load_and_verify_friends(
            friends_discovered,
            bundle_verified,
            bundle_unverified,
            data_store,
            /* allow_friend_loading_failure */ true,
        )?;

        // make sure there is no cyclic dependency
        self.verify_module_cyclic_relations(module, bundle_verified, bundle_unverified)
    }

    fn verify_module_cyclic_relations(
        &self,
        module: &CompiledModule,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        bundle_unverified: &BTreeSet<ModuleId>,
    ) -> VMResult<()> {
        let module_cache = self.module_cache.read();
        cyclic_dependencies::verify_module(
            module,
            |module_id| {
                bundle_verified
                    .get(module_id)
                    .or_else(|| module_cache.modules.get(module_id).map(|m| m.module()))
                    .map(|m| m.immediate_dependencies())
                    .ok_or_else(|| PartialVMError::new(StatusCode::MISSING_DEPENDENCY))
            },
            |module_id| {
                if bundle_unverified.contains(module_id) {
                    // If the module under verification declares a friend which is also in the
                    // bundle (and positioned after this module in the bundle), we defer the cyclic
                    // relation checking when we verify that module.
                    Ok(vec![])
                } else {
                    // Otherwise, we get all the information we need to verify whether this module
                    // creates a cyclic relation.
                    bundle_verified
                        .get(module_id)
                        .or_else(|| module_cache.modules.get(module_id).map(|m| m.module()))
                        .map(|m| m.immediate_friends())
                        .ok_or_else(|| PartialVMError::new(StatusCode::MISSING_DEPENDENCY))
                }
            },
        )
    }

    // All native functions must be known to the loader
    fn check_natives(&self, module: &CompiledModule) -> VMResult<()> {
        fn check_natives_impl(loader: &Loader, module: &CompiledModule) -> PartialVMResult<()> {
            for (idx, native_function) in module
                .function_defs()
                .iter()
                .filter(|fdv| fdv.is_native())
                .enumerate()
            {
                let fh = module.function_handle_at(native_function.function);
                let mh = module.module_handle_at(fh.module);
                loader
                    .natives
                    .resolve(
                        module.address_identifier_at(mh.address),
                        module.identifier_at(mh.name).as_str(),
                        module.identifier_at(fh.name).as_str(),
                    )
                    .ok_or_else(|| {
                        verification_error(
                            StatusCode::MISSING_DEPENDENCY,
                            IndexKind::FunctionHandle,
                            idx as TableIndex,
                        )
                    })?;
            }
            // TODO: fix check and error code if we leave something around for native structs.
            // For now this generates the only error test cases care about...
            for (idx, struct_def) in module.struct_defs().iter().enumerate() {
                if struct_def.field_information == StructFieldInformation::Native {
                    return Err(verification_error(
                        StatusCode::MISSING_DEPENDENCY,
                        IndexKind::FunctionHandle,
                        idx as TableIndex,
                    ));
                }
            }
            Ok(())
        }
        check_natives_impl(self, module).map_err(|e| e.finish(Location::Module(module.self_id())))
    }

    //
    // Helpers for loading and verification
    //

    pub(crate) fn load_type(
        &self,
        type_tag: &TypeTag,
        data_store: &impl DataStore,
    ) -> VMResult<Type> {
        Ok(match type_tag {
            TypeTag::Bool => Type::Bool,
            TypeTag::U8 => Type::U8,
            TypeTag::U64 => Type::U64,
            TypeTag::U128 => Type::U128,
            TypeTag::Address => Type::Address,
            TypeTag::Signer => Type::Signer,
            TypeTag::Vector(tt) => Type::Vector(Box::new(self.load_type(tt, data_store)?)),
            TypeTag::Struct(struct_tag) => {
                let module_id = ModuleId::new(struct_tag.address, struct_tag.module.clone());
                self.load_module(&module_id, data_store)?;
                let (idx, struct_type) = self
                    .module_cache
                    .read()
                    // GOOD module was loaded above
                    .resolve_struct_by_name(&struct_tag.name, &module_id)
                    .map_err(|e| e.finish(Location::Undefined))?;
                if struct_type.type_parameters.is_empty() && struct_tag.type_params.is_empty() {
                    Type::Struct(idx)
                } else {
                    let mut type_params = vec![];
                    for ty_param in &struct_tag.type_params {
                        type_params.push(self.load_type(ty_param, data_store)?);
                    }
                    self.verify_ty_args(struct_type.type_param_constraints(), &type_params)
                        .map_err(|e| e.finish(Location::Undefined))?;
                    Type::StructInstantiation(idx, type_params)
                }
            }
        })
    }

    // The interface for module loading. Aligned with `load_type` and `load_function`, this function
    // verifies that the module is OK instead of expect it.
    pub(crate) fn load_module(
        &self,
        id: &ModuleId,
        data_store: &impl DataStore,
    ) -> VMResult<Arc<Module>> {
        self.load_module_internal(id, &BTreeMap::new(), &BTreeSet::new(), data_store)
    }

    // Load the transitive closure of the target module first, and then verify that the modules in
    // the closure do not have cyclic dependencies.
    fn load_module_internal(
        &self,
        id: &ModuleId,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        bundle_unverified: &BTreeSet<ModuleId>,
        data_store: &impl DataStore,
    ) -> VMResult<Arc<Module>> {
        // if the module is already in the code cache, load the cached version
        if let Some(cached) = self.module_cache.read().module_at(id) {
            return Ok(cached);
        }

        // otherwise, load the transitive closure of the target module
        let module_ref = self.load_and_verify_module_and_dependencies_and_friends(
            id,
            bundle_verified,
            bundle_unverified,
            data_store,
            /* allow_module_loading_failure */ true,
        )?;

        // verify that the transitive closure does not have cycles
        self.verify_module_cyclic_relations(
            module_ref.module(),
            bundle_verified,
            bundle_unverified,
        )
        .map_err(expect_no_verification_errors)?;
        Ok(module_ref)
    }

    // Load, deserialize, and check the module with the bytecode verifier, without linking
    fn load_and_verify_module(
        &self,
        id: &ModuleId,
        data_store: &impl DataStore,
        allow_loading_failure: bool,
    ) -> VMResult<CompiledModule> {
        // bytes fetching, allow loading to fail if the flag is set
        let bytes = match data_store.load_module(id) {
            Ok(bytes) => bytes,
            Err(err) if allow_loading_failure => return Err(err),
            Err(err) => {
                error!("[VM] Error fetching module with id {:?}", id);
                return Err(expect_no_verification_errors(err));
            }
        };

        // for bytes obtained from the data store, they should always deserialize and verify.
        // It is an invariant violation if they don't.
        let module = CompiledModule::deserialize(&bytes)
            .map_err(|err| {
                let msg = format!("Deserialization error: {:?}", err);
                PartialVMError::new(StatusCode::CODE_DESERIALIZATION_ERROR)
                    .with_message(msg)
                    .finish(Location::Module(id.clone()))
            })
            .map_err(expect_no_verification_errors)?;

        // bytecode verifier checks that can be performed with the module itself
        move_bytecode_verifier::verify_module(&module).map_err(expect_no_verification_errors)?;
        self.check_natives(&module)
            .map_err(expect_no_verification_errors)?;
        Ok(module)
    }

    // Everything in `load_and_verify_module` and also recursively load and verify all the
    // dependencies of the target module.
    fn load_and_verify_module_and_dependencies(
        &self,
        id: &ModuleId,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        data_store: &impl DataStore,
        visited: &mut BTreeSet<ModuleId>,
        friends_discovered: &mut BTreeSet<ModuleId>,
        allow_module_loading_failure: bool,
    ) -> VMResult<Arc<Module>> {
        // dependency loading does not permit cycles
        if visited.contains(id) {
            return Err(PartialVMError::new(StatusCode::CYCLIC_MODULE_DEPENDENCY)
                .finish(Location::Undefined));
        }

        // module self-check
        let module = self.load_and_verify_module(id, data_store, allow_module_loading_failure)?;
        visited.insert(id.clone());
        friends_discovered.extend(module.immediate_friends());

        // downward exploration of the module's dependency graph. For a module that is loaded from
        // the data_store, we should never allow its dependencies to fail to load.
        self.load_and_verify_dependencies(
            &module,
            bundle_verified,
            data_store,
            visited,
            friends_discovered,
            /* allow_dependency_loading_failure */ false,
        )?;

        // if linking goes well, insert the module to the code cache
        let mut locked_cache = self.module_cache.write();
        let module_ref = locked_cache.insert(&self.natives, id.clone(), module)?;
        drop(locked_cache); // explicit unlock

        Ok(module_ref)
    }

    // downward exploration of the module's dependency graph
    fn load_and_verify_dependencies(
        &self,
        module: &CompiledModule,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        data_store: &impl DataStore,
        visited: &mut BTreeSet<ModuleId>,
        friends_discovered: &mut BTreeSet<ModuleId>,
        allow_dependency_loading_failure: bool,
    ) -> VMResult<()> {
        // all immediate dependencies of the module being verified should be in one of the locations
        // - the verified portion of the bundle (e.g., verified before this module)
        // - the code cache (i.e., loaded already)
        // - the data store (i.e., not loaded to code cache yet)
        let mut bundle_deps = vec![];
        let mut cached_deps = vec![];
        for module_id in module.immediate_dependencies() {
            if let Some(cached) = bundle_verified.get(&module_id) {
                bundle_deps.push(cached);
            } else {
                let locked_cache = self.module_cache.read();
                let loaded = match locked_cache.module_at(&module_id) {
                    None => {
                        drop(locked_cache); // explicit unlock
                        self.load_and_verify_module_and_dependencies(
                            &module_id,
                            bundle_verified,
                            data_store,
                            visited,
                            friends_discovered,
                            allow_dependency_loading_failure,
                        )?
                    }
                    Some(cached) => cached,
                };
                cached_deps.push(loaded);
            }
        }

        // once all dependencies are loaded, do the linking check
        let all_imm_deps = bundle_deps
            .into_iter()
            .chain(cached_deps.iter().map(|m| m.module()));
        let result = dependencies::verify_module(module, all_imm_deps);

        // if dependencies loading is not allowed to fail, the linking should not fail as well
        if allow_dependency_loading_failure {
            result
        } else {
            result.map_err(expect_no_verification_errors)
        }
    }

    // Everything in `load_and_verify_module_and_dependencies` and also recursively load and verify
    // all the friends modules of the newly loaded modules, until the friends frontier covers the
    // whole closure.
    fn load_and_verify_module_and_dependencies_and_friends(
        &self,
        id: &ModuleId,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        bundle_unverified: &BTreeSet<ModuleId>,
        data_store: &impl DataStore,
        allow_module_loading_failure: bool,
    ) -> VMResult<Arc<Module>> {
        // load the closure of the module in terms of dependency relation
        let mut visited = BTreeSet::new();
        let mut friends_discovered = BTreeSet::new();
        let module_ref = self.load_and_verify_module_and_dependencies(
            id,
            bundle_verified,
            data_store,
            &mut visited,
            &mut friends_discovered,
            allow_module_loading_failure,
        )?;

        // upward exploration of the module's friendship graph and expand the friendship frontier.
        // For a module that is loaded from the data_store, we should never allow that its friends
        // fail to load.
        self.load_and_verify_friends(
            friends_discovered,
            bundle_verified,
            bundle_unverified,
            data_store,
            /* allow_friend_loading_failure */ false,
        )?;
        Ok(module_ref)
    }

    // upward exploration of the module's dependency graph
    fn load_and_verify_friends(
        &self,
        friends_discovered: BTreeSet<ModuleId>,
        bundle_verified: &BTreeMap<ModuleId, CompiledModule>,
        bundle_unverified: &BTreeSet<ModuleId>,
        data_store: &impl DataStore,
        allow_friend_loading_failure: bool,
    ) -> VMResult<()> {
        // for each new module discovered in the frontier, load them fully and expand the frontier.
        // apply three filters to the new friend modules discovered
        // - `!locked_cache.has_module(mid)`
        //   If we friend a module that is already in the code cache, then we know that the
        //   transitive closure of that module is loaded into the cache already, skip the loading
        // - `!bundle_verified.contains_key(mid)`
        //   In the case of publishing a bundle, we don't actually put the published module into
        //   code cache. This `bundle_verified` cache is a temporary extension of the code cache
        //   in the bundle publication scenario. If a module is already verified, we don't need to
        //   re-load it again.
        // - `!bundle_unverified.contains(mid)
        //   If the module under verification declares a friend which is also in the bundle (and
        //   positioned after this module in the bundle), we defer the loading of that module when
        //   it is the module's turn in the bundle.
        let locked_cache = self.module_cache.read();
        let new_imm_friends: Vec<_> = friends_discovered
            .into_iter()
            .filter(|mid| {
                !locked_cache.has_module(mid)
                    && !bundle_verified.contains_key(mid)
                    && !bundle_unverified.contains(mid)
            })
            .collect();
        drop(locked_cache); // explicit unlock

        for module_id in new_imm_friends {
            self.load_and_verify_module_and_dependencies_and_friends(
                &module_id,
                bundle_verified,
                bundle_unverified,
                data_store,
                allow_friend_loading_failure,
            )?;
        }
        Ok(())
    }

    // Verify the kind (constraints) of an instantiation.
    // Both function and script invocation use this function to verify correctness
    // of type arguments provided
    fn verify_ty_args<'a, I>(&self, constraints: I, ty_args: &[Type]) -> PartialVMResult<()>
    where
        I: IntoIterator<Item = &'a AbilitySet>,
        I::IntoIter: ExactSizeIterator,
    {
        let constraints = constraints.into_iter();
        if constraints.len() != ty_args.len() {
            return Err(PartialVMError::new(
                StatusCode::NUMBER_OF_TYPE_ARGUMENTS_MISMATCH,
            ));
        }
        for (ty, expected_k) in ty_args.iter().zip(constraints) {
            if !expected_k.is_subset(self.abilities(ty)?) {
                return Err(PartialVMError::new(StatusCode::CONSTRAINT_NOT_SATISFIED));
            }
        }
        Ok(())
    }

    //
    // Internal helpers
    //

    fn function_at(&self, idx: usize) -> Arc<Function> {
        self.module_cache.read().function_at(idx)
    }

    fn get_module(&self, idx: &ModuleId) -> Arc<Module> {
        Arc::clone(
            self.module_cache
                .read()
                .modules
                .get(idx)
                .expect("ModuleId on Function must exist"),
        )
    }

    fn get_script(&self, hash: &ScriptHash) -> Arc<Script> {
        Arc::clone(
            self.scripts
                .read()
                .scripts
                .get(hash)
                .expect("Script hash on Function must exist"),
        )
    }

    pub(crate) fn get_struct_type(&self, idx: CachedStructIndex) -> Option<Arc<StructType>> {
        self.module_cache.read().structs.get(idx.0).map(Arc::clone)
    }

    fn abilities(&self, ty: &Type) -> PartialVMResult<AbilitySet> {
        match ty {
            Type::Bool | Type::U8 | Type::U64 | Type::U128 | Type::Address => {
                Ok(AbilitySet::PRIMITIVES)
            }

            // Technically unreachable but, no point in erroring if we don't have to
            Type::Reference(_) | Type::MutableReference(_) => Ok(AbilitySet::REFERENCES),
            Type::Signer => Ok(AbilitySet::SIGNER),

            Type::TyParam(_) => Err(PartialVMError::new(StatusCode::UNREACHABLE).with_message(
                "Unexpected TyParam type after translating from TypeTag to Type".to_string(),
            )),

            Type::Vector(ty) => AbilitySet::polymorphic_abilities(
                AbilitySet::VECTOR,
                vec![false],
                vec![self.abilities(ty)?],
            ),
            Type::Struct(idx) => Ok(self.module_cache.read().struct_at(*idx).abilities),
            Type::StructInstantiation(idx, type_args) => {
                let struct_type = self.module_cache.read().struct_at(*idx);
                let declared_phantom_parameters = struct_type
                    .type_parameters
                    .iter()
                    .map(|param| param.is_phantom);
                let type_argument_abilities = type_args
                    .iter()
                    .map(|arg| self.abilities(arg))
                    .collect::<PartialVMResult<Vec<_>>>()?;
                AbilitySet::polymorphic_abilities(
                    struct_type.abilities,
                    declared_phantom_parameters,
                    type_argument_abilities,
                )
            }
        }
    }
}

//
// Resolver
//

// A simple wrapper for a `Module` or a `Script` in the `Resolver`
enum BinaryType {
    Module(Arc<Module>),
    Script(Arc<Script>),
}

// A Resolver is a simple and small structure allocated on the stack and used by the
// interpreter. It's the only API known to the interpreter and it's tailored to the interpreter
// needs.
pub(crate) struct Resolver<'a> {
    loader: &'a Loader,
    binary: BinaryType,
}

impl<'a> Resolver<'a> {
    fn for_module(loader: &'a Loader, module: Arc<Module>) -> Self {
        let binary = BinaryType::Module(module);
        Self { loader, binary }
    }

    fn for_script(loader: &'a Loader, script: Arc<Script>) -> Self {
        let binary = BinaryType::Script(script);
        Self { loader, binary }
    }

    //
    // Constant resolution
    //

    pub(crate) fn constant_at(&self, idx: ConstantPoolIndex) -> &Constant {
        match &self.binary {
            BinaryType::Module(module) => module.module.constant_at(idx),
            BinaryType::Script(script) => script.script.constant_at(idx),
        }
    }

    //
    // Function resolution
    //

    pub(crate) fn function_from_handle(&self, idx: FunctionHandleIndex) -> Arc<Function> {
        let idx = match &self.binary {
            BinaryType::Module(module) => module.function_at(idx.0),
            BinaryType::Script(script) => script.function_at(idx.0),
        };
        self.loader.function_at(idx)
    }

    pub(crate) fn function_from_instantiation(
        &self,
        idx: FunctionInstantiationIndex,
    ) -> Arc<Function> {
        let func_inst = match &self.binary {
            BinaryType::Module(module) => module.function_instantiation_at(idx.0),
            BinaryType::Script(script) => script.function_instantiation_at(idx.0),
        };
        self.loader.function_at(func_inst.handle)
    }

    pub(crate) fn instantiate_generic_function(
        &self,
        idx: FunctionInstantiationIndex,
        type_params: &[Type],
    ) -> PartialVMResult<Vec<Type>> {
        let func_inst = match &self.binary {
            BinaryType::Module(module) => module.function_instantiation_at(idx.0),
            BinaryType::Script(script) => script.function_instantiation_at(idx.0),
        };
        let mut instantiation = vec![];
        for ty in &func_inst.instantiation {
            instantiation.push(ty.subst(type_params)?);
        }
        Ok(instantiation)
    }

    pub(crate) fn type_params_count(&self, idx: FunctionInstantiationIndex) -> usize {
        let func_inst = match &self.binary {
            BinaryType::Module(module) => module.function_instantiation_at(idx.0),
            BinaryType::Script(script) => script.function_instantiation_at(idx.0),
        };
        func_inst.instantiation.len()
    }

    //
    // Type resolution
    //

    pub(crate) fn get_struct_type(&self, idx: StructDefinitionIndex) -> Type {
        let struct_def = match &self.binary {
            BinaryType::Module(module) => module.struct_at(idx),
            BinaryType::Script(_) => unreachable!("Scripts cannot have type instructions"),
        };
        Type::Struct(struct_def)
    }

    pub(crate) fn instantiate_generic_type(
        &self,
        idx: StructDefInstantiationIndex,
        ty_args: &[Type],
    ) -> PartialVMResult<Type> {
        let struct_inst = match &self.binary {
            BinaryType::Module(module) => module.struct_instantiation_at(idx.0),
            BinaryType::Script(_) => unreachable!("Scripts cannot have type instructions"),
        };
        Ok(Type::StructInstantiation(
            struct_inst.def,
            struct_inst
                .instantiation
                .iter()
                .map(|ty| ty.subst(ty_args))
                .collect::<PartialVMResult<_>>()?,
        ))
    }

    fn single_type_at(&self, idx: SignatureIndex) -> &Type {
        match &self.binary {
            BinaryType::Module(module) => module.single_type_at(idx),
            BinaryType::Script(script) => script.single_type_at(idx),
        }
    }

    pub(crate) fn instantiate_single_type(
        &self,
        idx: SignatureIndex,
        ty_args: &[Type],
    ) -> PartialVMResult<Type> {
        let ty = self.single_type_at(idx);
        ty.subst(ty_args)
    }

    //
    // Fields resolution
    //

    pub(crate) fn field_offset(&self, idx: FieldHandleIndex) -> usize {
        match &self.binary {
            BinaryType::Module(module) => module.field_offset(idx),
            BinaryType::Script(_) => unreachable!("Scripts cannot have field instructions"),
        }
    }

    pub(crate) fn field_instantiation_offset(&self, idx: FieldInstantiationIndex) -> usize {
        match &self.binary {
            BinaryType::Module(module) => module.field_instantiation_offset(idx),
            BinaryType::Script(_) => unreachable!("Scripts cannot have field instructions"),
        }
    }

    pub(crate) fn field_count(&self, idx: StructDefinitionIndex) -> u16 {
        match &self.binary {
            BinaryType::Module(module) => module.field_count(idx.0),
            BinaryType::Script(_) => unreachable!("Scripts cannot have type instructions"),
        }
    }

    pub(crate) fn field_instantiation_count(&self, idx: StructDefInstantiationIndex) -> u16 {
        match &self.binary {
            BinaryType::Module(module) => module.field_instantiation_count(idx.0),
            BinaryType::Script(_) => unreachable!("Scripts cannot have type instructions"),
        }
    }

    pub(crate) fn type_to_type_layout(&self, ty: &Type) -> PartialVMResult<MoveTypeLayout> {
        self.loader.type_to_type_layout(ty)
    }

    // get the loader
    pub(crate) fn loader(&self) -> &Loader {
        self.loader
    }
}

// A Module is very similar to a binary Module but data is "transformed" to a representation
// more appropriate to execution.
// When code executes indexes in instructions are resolved against those runtime structure
// so that any data needed for execution is immediately available
#[derive(Debug)]
pub(crate) struct Module {
    #[allow(dead_code)]
    id: ModuleId,
    // primitive pools
    module: Arc<CompiledModule>,

    //
    // types as indexes into the Loader type list
    //

    // struct references carry the index into the global vector of types.
    // That is effectively an indirection over the ref table:
    // the instruction carries an index into this table which contains the index into the
    // glabal table of types. No instantiation of generic types is saved into the global table.
    #[allow(dead_code)]
    struct_refs: Vec<CachedStructIndex>,
    structs: Vec<StructDef>,
    // materialized instantiations, whether partial or not
    struct_instantiations: Vec<StructInstantiation>,

    // functions as indexes into the Loader function list
    // That is effectively an indirection over the ref table:
    // the instruction carries an index into this table which contains the index into the
    // glabal table of functions. No instantiation of generic functions is saved into
    // the global table.
    function_refs: Vec<usize>,
    // materialized instantiations, whether partial or not
    function_instantiations: Vec<FunctionInstantiation>,

    // fields as a pair of index, first to the type, second to the field position in that type
    field_handles: Vec<FieldHandle>,
    // materialized instantiations, whether partial or not
    field_instantiations: Vec<FieldInstantiation>,

    // function name to index into the Loader function list.
    // This allows a direct access from function name to `Function`
    function_map: HashMap<Identifier, usize>,
    // struct name to index into the Loader type list
    // This allows a direct access from struct name to `Struct`
    struct_map: HashMap<Identifier, CachedStructIndex>,

    // a map of single-token signature indices to type.
    // Single-token signatures are usually indexed by the `SignatureIndex` in bytecode. For example,
    // `VecMutBorrow(SignatureIndex)`, the `SignatureIndex` maps to a single `SignatureToken`, and
    // hence, a single type.
    single_signature_token_map: BTreeMap<SignatureIndex, Type>,
}

impl Module {
    fn new(
        module: CompiledModule,
        cache: &ModuleCache,
    ) -> Result<Self, (PartialVMError, CompiledModule)> {
        let id = module.self_id();

        let mut struct_refs = vec![];
        let mut structs = vec![];
        let mut struct_instantiations = vec![];
        let mut function_refs = vec![];
        let mut function_instantiations = vec![];
        let mut field_handles = vec![];
        let mut field_instantiations: Vec<FieldInstantiation> = vec![];
        let mut function_map = HashMap::new();
        let mut struct_map = HashMap::new();
        let mut single_signature_token_map = BTreeMap::new();

        let mut create = || {
            for struct_handle in module.struct_handles() {
                let struct_name = module.identifier_at(struct_handle.name);
                let module_handle = module.module_handle_at(struct_handle.module);
                let module_id = module.module_id_for_handle(module_handle);
                if module_id == id {
                    // module has not been published yet, loop through the types in reverse order.
                    // At this point all the types of the module are in the type list but not yet
                    // exposed through the module cache. The implication is that any resolution
                    // to types of the module being loaded is going to fail.
                    // So we manually go through the types and find the proper index
                    for (idx, struct_type) in cache.structs.iter().enumerate().rev() {
                        if struct_type.module != module_id {
                            return Err(PartialVMError::new(StatusCode::TYPE_RESOLUTION_FAILURE)
                                .with_message(format!(
                                    "Cannot find {:?}::{:?} in publishing module",
                                    module_id, struct_name
                                )));
                        }
                        if struct_type.name.as_ident_str() == struct_name {
                            struct_refs.push(CachedStructIndex(idx));
                            break;
                        }
                    }
                } else {
                    struct_refs.push(cache.resolve_struct_by_name(struct_name, &module_id)?.0);
                }
            }

            for struct_def in module.struct_defs() {
                let idx = struct_refs[struct_def.struct_handle.0 as usize];
                let field_count = cache.structs[idx.0].fields.len() as u16;
                structs.push(StructDef { field_count, idx });
                let name =
                    module.identifier_at(module.struct_handle_at(struct_def.struct_handle).name);
                struct_map.insert(name.to_owned(), idx);
            }

            for struct_inst in module.struct_instantiations() {
                let def = struct_inst.def.0 as usize;
                let struct_def = &structs[def];
                let field_count = struct_def.field_count;
                let mut instantiation = vec![];
                for ty in &module.signature_at(struct_inst.type_parameters).0 {
                    instantiation.push(cache.make_type_while_loading(&module, ty)?);
                }
                struct_instantiations.push(StructInstantiation {
                    field_count,
                    def: struct_def.idx,
                    instantiation,
                });
            }

            for func_handle in module.function_handles() {
                let func_name = module.identifier_at(func_handle.name);
                let module_handle = module.module_handle_at(func_handle.module);
                let module_id = module.module_id_for_handle(module_handle);
                if module_id == id {
                    // module has not been published yet, loop through the functions
                    for (idx, function) in cache.functions.iter().enumerate().rev() {
                        if function.module_id() != Some(&module_id) {
                            return Err(PartialVMError::new(
                                StatusCode::FUNCTION_RESOLUTION_FAILURE,
                            )
                            .with_message(format!(
                                "Cannot find {:?}::{:?} in publishing module",
                                module_id, func_name
                            )));
                        }
                        if function.name.as_ident_str() == func_name {
                            function_refs.push(idx);
                            break;
                        }
                    }
                } else {
                    function_refs.push(cache.resolve_function_by_name(func_name, &module_id)?);
                }
            }

            for func_def in module.function_defs() {
                let idx = function_refs[func_def.function.0 as usize];
                let name = module.identifier_at(module.function_handle_at(func_def.function).name);
                function_map.insert(name.to_owned(), idx);

                if let Some(code_unit) = &func_def.code {
                    for bc in &code_unit.code {
                        match bc {
                            Bytecode::VecPack(si, _)
                            | Bytecode::VecLen(si)
                            | Bytecode::VecImmBorrow(si)
                            | Bytecode::VecMutBorrow(si)
                            | Bytecode::VecPushBack(si)
                            | Bytecode::VecPopBack(si)
                            | Bytecode::VecUnpack(si, _)
                            | Bytecode::VecSwap(si) => {
                                if !single_signature_token_map.contains_key(si) {
                                    let ty = match module.signature_at(*si).0.get(0) {
                                        None => {
                                            return Err(PartialVMError::new(
                                                StatusCode::VERIFIER_INVARIANT_VIOLATION,
                                            )
                                            .with_message(
                                                "the type argument for vector-related bytecode \
                                                expects one and only one signature token"
                                                    .to_owned(),
                                            ));
                                        }
                                        Some(sig_token) => sig_token,
                                    };
                                    single_signature_token_map
                                        .insert(*si, cache.make_type_while_loading(&module, ty)?);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            for func_inst in module.function_instantiations() {
                let handle = function_refs[func_inst.handle.0 as usize];
                let mut instantiation = vec![];
                for ty in &module.signature_at(func_inst.type_parameters).0 {
                    instantiation.push(cache.make_type_while_loading(&module, ty)?);
                }
                function_instantiations.push(FunctionInstantiation {
                    handle,
                    instantiation,
                });
            }

            for f_handle in module.field_handles() {
                let def_idx = f_handle.owner;
                let owner = structs[def_idx.0 as usize].idx;
                let offset = f_handle.field as usize;
                field_handles.push(FieldHandle { offset, owner });
            }

            for f_inst in module.field_instantiations() {
                let fh_idx = f_inst.handle;
                let owner = field_handles[fh_idx.0 as usize].owner;
                let offset = field_handles[fh_idx.0 as usize].offset;
                field_instantiations.push(FieldInstantiation { offset, owner });
            }

            Ok(())
        };

        match create() {
            Ok(_) => Ok(Self {
                id,
                module: Arc::new(module),
                struct_refs,
                structs,
                struct_instantiations,
                function_refs,
                function_instantiations,
                field_handles,
                field_instantiations,
                function_map,
                struct_map,
                single_signature_token_map,
            }),
            Err(err) => Err((err, module)),
        }
    }

    fn struct_at(&self, idx: StructDefinitionIndex) -> CachedStructIndex {
        self.structs[idx.0 as usize].idx
    }

    fn struct_instantiation_at(&self, idx: u16) -> &StructInstantiation {
        &self.struct_instantiations[idx as usize]
    }

    fn function_at(&self, idx: u16) -> usize {
        self.function_refs[idx as usize]
    }

    fn function_instantiation_at(&self, idx: u16) -> &FunctionInstantiation {
        &self.function_instantiations[idx as usize]
    }

    fn field_count(&self, idx: u16) -> u16 {
        self.structs[idx as usize].field_count
    }

    fn field_instantiation_count(&self, idx: u16) -> u16 {
        self.struct_instantiations[idx as usize].field_count
    }

    pub(crate) fn module(&self) -> &CompiledModule {
        &self.module
    }

    pub(crate) fn arc_module(&self) -> Arc<CompiledModule> {
        self.module.clone()
    }

    fn field_offset(&self, idx: FieldHandleIndex) -> usize {
        self.field_handles[idx.0 as usize].offset
    }

    fn field_instantiation_offset(&self, idx: FieldInstantiationIndex) -> usize {
        self.field_instantiations[idx.0 as usize].offset
    }

    fn single_type_at(&self, idx: SignatureIndex) -> &Type {
        self.single_signature_token_map.get(&idx).unwrap()
    }
}

// A Script is very similar to a `CompiledScript` but data is "transformed" to a representation
// more appropriate to execution.
// When code executes, indexes in instructions are resolved against runtime structures
// (rather then "compiled") to make available data needed for execution
// #[derive(Debug)]
struct Script {
    // primitive pools
    script: CompiledScript,

    // types as indexes into the Loader type list
    // REVIEW: why is this unused?
    #[allow(dead_code)]
    struct_refs: Vec<CachedStructIndex>,

    // functions as indexes into the Loader function list
    function_refs: Vec<usize>,
    // materialized instantiations, whether partial or not
    function_instantiations: Vec<FunctionInstantiation>,

    // entry point
    main: Arc<Function>,

    // parameters of main
    parameter_tys: Vec<Type>,

    // return values
    return_tys: Vec<Type>,

    // a map of single-token signature indices to type
    single_signature_token_map: BTreeMap<SignatureIndex, Type>,
}

impl Script {
    fn new(
        script: CompiledScript,
        script_hash: &ScriptHash,
        cache: &ModuleCache,
    ) -> VMResult<Self> {
        let mut struct_refs = vec![];
        for struct_handle in script.struct_handles() {
            let struct_name = script.identifier_at(struct_handle.name);
            let module_handle = script.module_handle_at(struct_handle.module);
            let module_id = ModuleId::new(
                *script.address_identifier_at(module_handle.address),
                script.identifier_at(module_handle.name).to_owned(),
            );
            struct_refs.push(
                cache
                    .resolve_struct_by_name(struct_name, &module_id)
                    .map_err(|e| e.finish(Location::Script))?
                    .0,
            );
        }

        let mut function_refs = vec![];
        for func_handle in script.function_handles().iter() {
            let func_name = script.identifier_at(func_handle.name);
            let module_handle = script.module_handle_at(func_handle.module);
            let module_id = ModuleId::new(
                *script.address_identifier_at(module_handle.address),
                script.identifier_at(module_handle.name).to_owned(),
            );
            let ref_idx = cache
                .resolve_function_by_name(func_name, &module_id)
                .map_err(|err| err.finish(Location::Undefined))?;
            function_refs.push(ref_idx);
        }

        let mut function_instantiations = vec![];
        for func_inst in script.function_instantiations() {
            let handle = function_refs[func_inst.handle.0 as usize];
            let mut instantiation = vec![];
            for ty in &script.signature_at(func_inst.type_parameters).0 {
                instantiation.push(
                    cache
                        .make_type(BinaryIndexedView::Script(&script), ty)
                        .map_err(|e| e.finish(Location::Script))?,
                );
            }
            function_instantiations.push(FunctionInstantiation {
                handle,
                instantiation,
            });
        }

        let scope = Scope::Script(*script_hash);

        let code: Vec<Bytecode> = script.code.code.clone();
        let parameters = script.signature_at(script.parameters).clone();

        let parameter_tys = parameters
            .0
            .iter()
            .map(|tok| cache.make_type(BinaryIndexedView::Script(&script), tok))
            .collect::<PartialVMResult<Vec<_>>>()
            .map_err(|err| err.finish(Location::Undefined))?;
        let locals = Signature(
            parameters
                .0
                .iter()
                .chain(script.signature_at(script.code.locals).0.iter())
                .cloned()
                .collect(),
        );
        let return_ = Signature(vec![]);
        let return_tys = return_
            .0
            .iter()
            .map(|tok| cache.make_type(BinaryIndexedView::Script(&script), tok))
            .collect::<PartialVMResult<Vec<_>>>()
            .map_err(|err| err.finish(Location::Undefined))?;
        let type_parameters = script.type_parameters.clone();
        // TODO: main does not have a name. Revisit.
        let name = Identifier::new("main").unwrap();
        let native = None; // Script entries cannot be native
        let main: Arc<Function> = Arc::new(Function {
            file_format_version: script.version(),
            index: FunctionDefinitionIndex(0),
            code,
            parameters,
            return_,
            locals,
            type_parameters,
            native,
            scope,
            name,
        });

        let mut single_signature_token_map = BTreeMap::new();
        for bc in &script.code.code {
            match bc {
                Bytecode::VecPack(si, _)
                | Bytecode::VecLen(si)
                | Bytecode::VecImmBorrow(si)
                | Bytecode::VecMutBorrow(si)
                | Bytecode::VecPushBack(si)
                | Bytecode::VecPopBack(si)
                | Bytecode::VecUnpack(si, _)
                | Bytecode::VecSwap(si) => {
                    if !single_signature_token_map.contains_key(si) {
                        let ty = match script.signature_at(*si).0.get(0) {
                            None => {
                                return Err(PartialVMError::new(
                                    StatusCode::VERIFIER_INVARIANT_VIOLATION,
                                )
                                .with_message(
                                    "the type argument for vector-related bytecode \
                                                expects one and only one signature token"
                                        .to_owned(),
                                )
                                .finish(Location::Script));
                            }
                            Some(sig_token) => sig_token,
                        };
                        single_signature_token_map.insert(
                            *si,
                            cache
                                .make_type(BinaryIndexedView::Script(&script), ty)
                                .map_err(|e| e.finish(Location::Script))?,
                        );
                    }
                }
                _ => {}
            }
        }

        Ok(Self {
            script,
            struct_refs,
            function_refs,
            function_instantiations,
            main,
            parameter_tys,
            return_tys,
            single_signature_token_map,
        })
    }

    fn entry_point(&self) -> Arc<Function> {
        self.main.clone()
    }

    fn function_at(&self, idx: u16) -> usize {
        self.function_refs[idx as usize]
    }

    fn function_instantiation_at(&self, idx: u16) -> &FunctionInstantiation {
        &self.function_instantiations[idx as usize]
    }

    fn single_type_at(&self, idx: SignatureIndex) -> &Type {
        self.single_signature_token_map.get(&idx).unwrap()
    }
}

// A simple wrapper for the "owner" of the function (Module or Script)
#[derive(Debug)]
enum Scope {
    Module(ModuleId),
    Script(ScriptHash),
}

// A runtime function
// #[derive(Debug)]
// https://github.com/rust-lang/rust/issues/70263
pub(crate) struct Function {
    #[allow(unused)]
    file_format_version: u32,
    index: FunctionDefinitionIndex,
    code: Vec<Bytecode>,
    parameters: Signature,
    return_: Signature,
    locals: Signature,
    type_parameters: Vec<AbilitySet>,
    native: Option<NativeFunction>,
    scope: Scope,
    name: Identifier,
}

impl Function {
    fn new(
        natives: &NativeFunctions,
        index: FunctionDefinitionIndex,
        def: &FunctionDefinition,
        module: &CompiledModule,
    ) -> Self {
        let handle = module.function_handle_at(def.function);
        let name = module.identifier_at(handle.name).to_owned();
        let module_id = module.self_id();
        let native = if def.is_native() {
            natives.resolve(
                module_id.address(),
                module_id.name().as_str(),
                name.as_str(),
            )
        } else {
            None
        };
        let scope = Scope::Module(module_id);
        let parameters = module.signature_at(handle.parameters).clone();
        // Native functions do not have a code unit
        let (code, locals) = match &def.code {
            Some(code) => (
                code.code.clone(),
                Signature(
                    parameters
                        .0
                        .iter()
                        .chain(module.signature_at(code.locals).0.iter())
                        .cloned()
                        .collect(),
                ),
            ),
            None => (vec![], Signature(vec![])),
        };
        let return_ = module.signature_at(handle.return_).clone();
        let type_parameters = handle.type_parameters.clone();
        Self {
            file_format_version: module.version(),
            index,
            code,
            parameters,
            return_,
            locals,
            type_parameters,
            native,
            scope,
            name,
        }
    }

    #[allow(unused)]
    pub(crate) fn file_format_version(&self) -> u32 {
        self.file_format_version
    }

    pub(crate) fn module_id(&self) -> Option<&ModuleId> {
        match &self.scope {
            Scope::Module(module_id) => Some(module_id),
            Scope::Script(_) => None,
        }
    }

    pub(crate) fn index(&self) -> FunctionDefinitionIndex {
        self.index
    }

    pub(crate) fn get_resolver<'a>(&self, loader: &'a Loader) -> Resolver<'a> {
        match &self.scope {
            Scope::Module(module_id) => {
                let module = loader.get_module(module_id);
                Resolver::for_module(loader, module)
            }
            Scope::Script(script_hash) => {
                let script = loader.get_script(script_hash);
                Resolver::for_script(loader, script)
            }
        }
    }

    pub(crate) fn local_count(&self) -> usize {
        self.locals.len()
    }

    pub(crate) fn arg_count(&self) -> usize {
        self.parameters.len()
    }

    pub(crate) fn name(&self) -> &str {
        self.name.as_str()
    }

    pub(crate) fn code(&self) -> &[Bytecode] {
        &self.code
    }

    pub(crate) fn type_parameters(&self) -> &[AbilitySet] {
        &self.type_parameters
    }

    #[allow(dead_code)]
    pub(crate) fn parameters(&self) -> &Signature {
        &self.parameters
    }

    pub(crate) fn pretty_string(&self) -> String {
        match &self.scope {
            Scope::Script(_) => "Script::main".into(),
            Scope::Module(id) => format!(
                "0x{}::{}::{}",
                id.address(),
                id.name().as_str(),
                self.name.as_str()
            ),
        }
    }

    pub(crate) fn is_native(&self) -> bool {
        self.native.is_some()
    }

    pub(crate) fn get_native(&self) -> PartialVMResult<NativeFunction> {
        self.native.ok_or_else(|| {
            PartialVMError::new(StatusCode::UNREACHABLE)
                .with_message("Missing Native Function".to_string())
        })
    }
}

//
// Internal structures that are saved at the proper index in the proper tables to access
// execution information (interpreter).
// The following structs are internal to the loader and never exposed out.
// The `Loader` will create those struct and the proper table when loading a module.
// The `Resolver` uses those structs to return information to the `Interpreter`.
//

// A function instantiation.
#[derive(Debug)]
struct FunctionInstantiation {
    // index to `ModuleCache::functions` global table
    handle: usize,
    instantiation: Vec<Type>,
}

#[derive(Debug)]
struct StructDef {
    // struct field count
    field_count: u16,
    // `ModuelCache::structs` global table index
    idx: CachedStructIndex,
}

#[derive(Debug)]
struct StructInstantiation {
    // struct field count
    field_count: u16,
    // `ModuelCache::structs` global table index. It is the generic type.
    def: CachedStructIndex,
    instantiation: Vec<Type>,
}

// A field handle. The offset is the only used information when operating on a field
#[derive(Debug)]
struct FieldHandle {
    offset: usize,
    // `ModuelCache::structs` global table index. It is the generic type.
    owner: CachedStructIndex,
}

// A field instantiation. The offset is the only used information when operating on a field
#[derive(Debug)]
struct FieldInstantiation {
    offset: usize,
    // `ModuelCache::structs` global table index. It is the generic type.
    #[allow(unused)]
    owner: CachedStructIndex,
}

//
// Cache for data associated to a Struct, used for de/serialization and more
//

struct StructInfo {
    struct_tag: Option<StructTag>,
    struct_layout: Option<MoveStructLayout>,
}

impl StructInfo {
    fn new() -> Self {
        Self {
            struct_tag: None,
            struct_layout: None,
        }
    }
}

pub(crate) struct TypeCache {
    structs: HashMap<CachedStructIndex, HashMap<Vec<Type>, StructInfo>>,
}

impl TypeCache {
    fn new() -> Self {
        Self {
            structs: HashMap::new(),
        }
    }
}

const VALUE_DEPTH_MAX: usize = 128;

impl Loader {
    fn struct_gidx_to_type_tag(
        &self,
        gidx: CachedStructIndex,
        ty_args: &[Type],
    ) -> PartialVMResult<StructTag> {
        if let Some(struct_map) = self.type_cache.read().structs.get(&gidx) {
            if let Some(struct_info) = struct_map.get(ty_args) {
                if let Some(struct_tag) = &struct_info.struct_tag {
                    return Ok(struct_tag.clone());
                }
            }
        }

        let ty_arg_tags = ty_args
            .iter()
            .map(|ty| self.type_to_type_tag(ty))
            .collect::<PartialVMResult<Vec<_>>>()?;
        let struct_type = self.module_cache.read().struct_at(gidx);
        let struct_tag = StructTag {
            address: *struct_type.module.address(),
            module: struct_type.module.name().to_owned(),
            name: struct_type.name.clone(),
            type_params: ty_arg_tags,
        };

        self.type_cache
            .write()
            .structs
            .entry(gidx)
            .or_insert_with(HashMap::new)
            .entry(ty_args.to_vec())
            .or_insert_with(StructInfo::new)
            .struct_tag = Some(struct_tag.clone());

        Ok(struct_tag)
    }

    fn type_to_type_tag_impl(&self, ty: &Type) -> PartialVMResult<TypeTag> {
        Ok(match ty {
            Type::Bool => TypeTag::Bool,
            Type::U8 => TypeTag::U8,
            Type::U64 => TypeTag::U64,
            Type::U128 => TypeTag::U128,
            Type::Address => TypeTag::Address,
            Type::Signer => TypeTag::Signer,
            Type::Vector(ty) => TypeTag::Vector(Box::new(self.type_to_type_tag(ty)?)),
            Type::Struct(gidx) => TypeTag::Struct(self.struct_gidx_to_type_tag(*gidx, &[])?),
            Type::StructInstantiation(gidx, ty_args) => {
                TypeTag::Struct(self.struct_gidx_to_type_tag(*gidx, ty_args)?)
            }
            Type::Reference(_) | Type::MutableReference(_) | Type::TyParam(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!("no type tag for {:?}", ty)),
                )
            }
        })
    }

    fn struct_gidx_to_type_layout(
        &self,
        gidx: CachedStructIndex,
        ty_args: &[Type],
        depth: usize,
    ) -> PartialVMResult<MoveStructLayout> {
        if let Some(struct_map) = self.type_cache.read().structs.get(&gidx) {
            if let Some(struct_info) = struct_map.get(ty_args) {
                if let Some(layout) = &struct_info.struct_layout {
                    return Ok(layout.clone());
                }
            }
        }

        let struct_type = self.module_cache.read().struct_at(gidx);
        let field_tys = struct_type
            .fields
            .iter()
            .map(|ty| ty.subst(ty_args))
            .collect::<PartialVMResult<Vec<_>>>()?;
        let field_layouts = field_tys
            .iter()
            .map(|ty| self.type_to_type_layout_impl(ty, depth + 1))
            .collect::<PartialVMResult<Vec<_>>>()?;
        let struct_layout = MoveStructLayout::new(field_layouts);

        self.type_cache
            .write()
            .structs
            .entry(gidx)
            .or_insert_with(HashMap::new)
            .entry(ty_args.to_vec())
            .or_insert_with(StructInfo::new)
            .struct_layout = Some(struct_layout.clone());

        Ok(struct_layout)
    }

    fn type_to_type_layout_impl(&self, ty: &Type, depth: usize) -> PartialVMResult<MoveTypeLayout> {
        if depth > VALUE_DEPTH_MAX {
            return Err(PartialVMError::new(StatusCode::VM_MAX_VALUE_DEPTH_REACHED));
        }
        Ok(match ty {
            Type::Bool => MoveTypeLayout::Bool,
            Type::U8 => MoveTypeLayout::U8,
            Type::U64 => MoveTypeLayout::U64,
            Type::U128 => MoveTypeLayout::U128,
            Type::Address => MoveTypeLayout::Address,
            Type::Signer => MoveTypeLayout::Signer,
            Type::Vector(ty) => {
                MoveTypeLayout::Vector(Box::new(self.type_to_type_layout_impl(ty, depth + 1)?))
            }
            Type::Struct(gidx) => {
                MoveTypeLayout::Struct(self.struct_gidx_to_type_layout(*gidx, &[], depth)?)
            }
            Type::StructInstantiation(gidx, ty_args) => {
                MoveTypeLayout::Struct(self.struct_gidx_to_type_layout(*gidx, ty_args, depth)?)
            }
            Type::Reference(_) | Type::MutableReference(_) | Type::TyParam(_) => {
                return Err(
                    PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR)
                        .with_message(format!("no type layout for {:?}", ty)),
                )
            }
        })
    }

    pub(crate) fn type_to_type_tag(&self, ty: &Type) -> PartialVMResult<TypeTag> {
        self.type_to_type_tag_impl(ty)
    }
    pub(crate) fn type_to_type_layout(&self, ty: &Type) -> PartialVMResult<MoveTypeLayout> {
        self.type_to_type_layout_impl(ty, 1)
    }
}

// Public APIs for external uses.
impl Loader {
    pub(crate) fn get_type_layout(
        &self,
        type_tag: &TypeTag,
        move_storage: &impl DataStore,
    ) -> VMResult<MoveTypeLayout> {
        let ty = self.load_type(type_tag, move_storage)?;
        self.type_to_type_layout(&ty)
            .map_err(|e| e.finish(Location::Undefined))
    }
}
