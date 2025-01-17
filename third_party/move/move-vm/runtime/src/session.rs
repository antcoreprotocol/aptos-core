// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::VMConfig,
    data_cache::TransactionDataCache,
    loader::{LoadedFunction, ModuleStorageAdapter},
    module_traversal::TraversalContext,
    move_vm::MoveVM,
    native_extensions::NativeContextExtensions,
};
use bytes::Bytes;
use move_binary_format::{
    compatibility::Compatibility,
    errors::*,
    file_format::{AbilitySet, LocalIndex},
};
use move_core_types::{
    account_address::AccountAddress,
    effects::{ChangeSet, Changes},
    gas_algebra::NumBytes,
    identifier::IdentStr,
    language_storage::{ModuleId, TypeTag},
    value::MoveTypeLayout,
    vm_status::StatusCode,
};
use move_vm_types::{
    gas::GasMeter,
    loaded_data::runtime_types::{StructNameIndex, StructType, Type, TypeBuilder},
    values::{GlobalValue, Value},
};
use std::{borrow::Borrow, sync::Arc};

pub struct Session<'r, 'l> {
    pub(crate) move_vm: &'l MoveVM,
    pub(crate) data_cache: TransactionDataCache<'r>,
    pub(crate) module_store: ModuleStorageAdapter,
    pub(crate) native_extensions: NativeContextExtensions<'r>,
}

/// Serialized return values from function/script execution
/// Simple struct is designed just to convey meaning behind serialized values
#[derive(Debug)]
pub struct SerializedReturnValues {
    /// The value of any arguments that were mutably borrowed.
    /// Non-mut borrowed values are not included
    pub mutable_reference_outputs: Vec<(LocalIndex, Vec<u8>, MoveTypeLayout)>,
    /// The return values from the function
    pub return_values: Vec<(Vec<u8>, MoveTypeLayout)>,
}

impl<'r, 'l> Session<'r, 'l> {
    /// Execute a Move entry function.
    ///
    /// NOTE: There are NO checks on the `args` except that they can deserialize
    /// into the provided types. The ability to deserialize `args` into arbitrary
    /// types is *very* powerful, e.g., it can be used to manufacture `signer`s
    /// or `Coin`s from raw bytes. It is the responsibility of the caller to ensure
    /// that this power is used responsibly/securely for its use-case.
    pub fn execute_entry_function(
        &mut self,
        func: LoadedFunction,
        args: Vec<impl Borrow<[u8]>>,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext,
    ) -> VMResult<()> {
        if !func.is_entry() {
            return Err(PartialVMError::new(
                StatusCode::EXECUTE_ENTRY_FUNCTION_CALLED_ON_NON_ENTRY_FUNCTION,
            )
            .finish(Location::Module(
                func.module_id()
                    .expect("Entry function always has module id"),
            )));
        }

        self.move_vm.runtime.execute_function_instantiation(
            func,
            args,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            traversal_context,
            &mut self.native_extensions,
        )?;
        Ok(())
    }

    /// Execute a Move function ignoring its visibility and whether it is entry or not.
    pub fn execute_function_bypass_visibility(
        &mut self,
        module: &ModuleId,
        function_name: &IdentStr,
        ty_args: Vec<TypeTag>,
        args: Vec<impl Borrow<[u8]>>,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext,
    ) -> VMResult<SerializedReturnValues> {
        let func = self.move_vm.runtime.loader().load_function(
            module,
            function_name,
            &ty_args,
            &mut self.data_cache,
            &self.module_store,
        )?;

        self.move_vm.runtime.execute_function_instantiation(
            func,
            args,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            traversal_context,
            &mut self.native_extensions,
        )
    }

    pub fn execute_loaded_function(
        &mut self,
        func: LoadedFunction,
        args: Vec<impl Borrow<[u8]>>,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext,
    ) -> VMResult<SerializedReturnValues> {
        self.move_vm.runtime.execute_function_instantiation(
            func,
            args,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            traversal_context,
            &mut self.native_extensions,
        )
    }

    /// Execute a transaction script.
    ///
    /// The Move VM MUST return a user error (in other words, an error that's not an invariant
    /// violation) if
    ///   - The script fails to deserialize or verify. Not all expressible signatures are valid.
    ///     See `move_bytecode_verifier::script_signature` for the rules.
    ///   - Type arguments refer to a non-existent type.
    ///   - Arguments (senders included) fail to deserialize or fail to match the signature of the
    ///     script function.
    ///
    /// If any other error occurs during execution, the Move VM MUST propagate that error back to
    /// the caller.
    /// Besides, no user input should cause the Move VM to return an invariant violation.
    ///
    /// In case an invariant violation occurs, the whole Session should be considered corrupted and
    /// one shall not proceed with effect generation.
    pub fn execute_script(
        &mut self,
        script: impl Borrow<[u8]>,
        ty_args: Vec<TypeTag>,
        args: Vec<impl Borrow<[u8]>>,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext,
    ) -> VMResult<()> {
        self.move_vm.runtime.execute_script(
            script,
            ty_args,
            args,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            traversal_context,
            &mut self.native_extensions,
        )
    }

    /// Publish the given module.
    ///
    /// The Move VM MUST return a user error, i.e., an error that's not an invariant violation, if
    ///   - The module fails to deserialize or verify.
    ///   - The sender address does not match that of the module.
    ///   - (Republishing-only) the module to be updated is not backward compatible with the old module.
    ///   - (Republishing-only) the module to be updated introduces cyclic dependencies.
    ///
    /// The Move VM should not be able to produce other user errors.
    /// Besides, no user input should cause the Move VM to return an invariant violation.
    ///
    /// In case an invariant violation occurs, the whole Session should be considered corrupted and
    /// one shall not proceed with effect generation.
    pub fn publish_module(
        &mut self,
        module: Vec<u8>,
        sender: AccountAddress,
        gas_meter: &mut impl GasMeter,
    ) -> VMResult<()> {
        self.publish_module_bundle(vec![module], sender, gas_meter)
    }

    /// Publish a series of modules.
    ///
    /// The Move VM MUST return a user error, i.e., an error that's not an invariant violation, if
    /// any module fails to deserialize or verify (see the full list of  failing conditions in the
    /// `publish_module` API). The publishing of the module series is an all-or-nothing action:
    /// either all modules are published to the data store or none is.
    ///
    /// Similar to the `publish_module` API, the Move VM should not be able to produce other user
    /// errors. Besides, no user input should cause the Move VM to return an invariant violation.
    ///
    /// In case an invariant violation occurs, the whole Session should be considered corrupted and
    /// one shall not proceed with effect generation.
    ///
    /// This operation performs compatibility checks if a module is replaced. See also
    /// `move_binary_format::compatibility`.
    pub fn publish_module_bundle(
        &mut self,
        modules: Vec<Vec<u8>>,
        sender: AccountAddress,
        gas_meter: &mut impl GasMeter,
    ) -> VMResult<()> {
        self.move_vm.runtime.publish_module_bundle(
            modules,
            sender,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            Compatibility::full_check(),
        )
    }

    /// Same like `publish_module_bundle` but with a custom compatibility check.
    pub fn publish_module_bundle_with_compat_config(
        &mut self,
        modules: Vec<Vec<u8>>,
        sender: AccountAddress,
        gas_meter: &mut impl GasMeter,
        compat_config: Compatibility,
    ) -> VMResult<()> {
        self.move_vm.runtime.publish_module_bundle(
            modules,
            sender,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            compat_config,
        )
    }

    pub fn publish_module_bundle_relax_compatibility(
        &mut self,
        modules: Vec<Vec<u8>>,
        sender: AccountAddress,
        gas_meter: &mut impl GasMeter,
    ) -> VMResult<()> {
        self.move_vm.runtime.publish_module_bundle(
            modules,
            sender,
            &mut self.data_cache,
            &self.module_store,
            gas_meter,
            Compatibility::no_check(),
        )
    }

    pub fn num_mutated_accounts(&self, sender: &AccountAddress) -> u64 {
        self.data_cache.num_mutated_accounts(sender)
    }

    /// Finish up the session and produce the side effects.
    ///
    /// This function should always succeed with no user errors returned, barring invariant violations.
    ///
    /// This MUST NOT be called if there is a previous invocation that failed with an invariant violation.
    pub fn finish(self) -> VMResult<ChangeSet> {
        self.data_cache
            .into_effects(self.move_vm.runtime.loader())
            .map_err(|e| e.finish(Location::Undefined))
    }

    pub fn finish_with_custom_effects<Resource>(
        self,
        resource_converter: &dyn Fn(Value, MoveTypeLayout, bool) -> PartialVMResult<Resource>,
    ) -> VMResult<Changes<Bytes, Resource>> {
        self.data_cache
            .into_custom_effects(resource_converter, self.move_vm.runtime.loader())
            .map_err(|e| e.finish(Location::Undefined))
    }

    /// Same like `finish`, but also extracts the native context extensions from the session.
    pub fn finish_with_extensions(self) -> VMResult<(ChangeSet, NativeContextExtensions<'r>)> {
        let Session {
            data_cache,
            native_extensions,
            ..
        } = self;
        let change_set = data_cache
            .into_effects(self.move_vm.runtime.loader())
            .map_err(|e| e.finish(Location::Undefined))?;
        Ok((change_set, native_extensions))
    }

    pub fn finish_with_extensions_with_custom_effects<Resource>(
        self,
        resource_converter: &dyn Fn(Value, MoveTypeLayout, bool) -> PartialVMResult<Resource>,
    ) -> VMResult<(Changes<Bytes, Resource>, NativeContextExtensions<'r>)> {
        let Session {
            data_cache,
            native_extensions,
            ..
        } = self;
        let change_set = data_cache
            .into_custom_effects(resource_converter, self.move_vm.runtime.loader())
            .map_err(|e| e.finish(Location::Undefined))?;
        Ok((change_set, native_extensions))
    }

    /// Try to load a resource from remote storage and create a corresponding GlobalValue
    /// that is owned by the data store.
    pub fn load_resource(
        &mut self,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<(&mut GlobalValue, Option<NumBytes>)> {
        self.data_cache
            .load_resource(self.move_vm.runtime.loader(), addr, ty, &self.module_store)
    }

    /// Get the serialized format of a `CompiledModule` given a `ModuleId`.
    pub fn load_module(&self, module_id: &ModuleId) -> VMResult<Bytes> {
        self.data_cache
            .load_module(module_id)
            .map_err(|e| e.finish(Location::Undefined))
    }

    /// Check if this module exists.
    pub fn exists_module(&self, module_id: &ModuleId) -> VMResult<bool> {
        self.data_cache.exists_module(module_id)
    }

    /// Load a script and all of its types into cache
    pub fn load_script(
        &mut self,
        script: impl Borrow<[u8]>,
        ty_args: &[TypeTag],
    ) -> VMResult<LoadedFunction> {
        self.move_vm.runtime.loader().load_script(
            script.borrow(),
            ty_args,
            &mut self.data_cache,
            &self.module_store,
        )
    }

    /// Load a module, a function, and all of its types into cache
    pub fn load_function_with_type_arg_inference(
        &mut self,
        module_id: &ModuleId,
        function_name: &IdentStr,
        expected_return_type: &Type,
    ) -> VMResult<LoadedFunction> {
        self.move_vm
            .runtime
            .loader()
            .load_function_with_type_arg_inference(
                module_id,
                function_name,
                expected_return_type,
                &mut self.data_cache,
                &self.module_store,
            )
    }

    /// Load a module, a function, and all of its types into cache
    pub fn load_function(
        &mut self,
        module_id: &ModuleId,
        function_name: &IdentStr,
        ty_args: &[TypeTag],
    ) -> VMResult<LoadedFunction> {
        self.move_vm.runtime.loader().load_function(
            module_id,
            function_name,
            ty_args,
            &mut self.data_cache,
            &self.module_store,
        )
    }

    pub fn load_type(&mut self, type_tag: &TypeTag) -> VMResult<Type> {
        self.move_vm
            .runtime
            .loader()
            .load_type(type_tag, &mut self.data_cache, &self.module_store)
    }

    pub fn get_type_layout(&mut self, type_tag: &TypeTag) -> VMResult<MoveTypeLayout> {
        self.move_vm.runtime.loader().get_type_layout(
            type_tag,
            &mut self.data_cache,
            &self.module_store,
        )
    }

    pub fn get_fully_annotated_type_layout(
        &mut self,
        type_tag: &TypeTag,
    ) -> VMResult<MoveTypeLayout> {
        self.move_vm
            .runtime
            .loader()
            .get_fully_annotated_type_layout(type_tag, &mut self.data_cache, &self.module_store)
    }

    pub fn get_type_tag(&self, ty: &Type) -> VMResult<TypeTag> {
        self.move_vm
            .runtime
            .loader()
            .type_to_type_tag(ty)
            .map_err(|e| e.finish(Location::Undefined))
    }

    /// Gets the abilities for this type, at it's particular instantiation
    pub fn get_type_abilities(&self, ty: &Type) -> VMResult<AbilitySet> {
        ty.abilities().map_err(|e| e.finish(Location::Undefined))
    }

    /// Gets the underlying native extensions.
    pub fn get_native_extensions(&mut self) -> &mut NativeContextExtensions<'r> {
        &mut self.native_extensions
    }

    pub fn get_move_vm(&self) -> &'l MoveVM {
        self.move_vm
    }

    pub fn get_vm_config(&self) -> &'l VMConfig {
        self.move_vm.runtime.loader().vm_config()
    }

    pub fn get_ty_builder(&self) -> &'l TypeBuilder {
        self.move_vm.runtime.loader().ty_builder()
    }

    pub fn get_struct_type(&self, index: StructNameIndex) -> Option<Arc<StructType>> {
        let name = self
            .move_vm
            .runtime
            .loader()
            .name_cache
            .idx_to_identifier(index);
        self.module_store
            .get_struct_type_by_identifier(&name.name, &name.module)
            .ok()
    }

    pub fn check_dependencies_and_charge_gas<'a, I>(
        &mut self,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext<'a>,
        ids: I,
    ) -> VMResult<()>
    where
        I: IntoIterator<Item = (&'a AccountAddress, &'a IdentStr)>,
        I::IntoIter: DoubleEndedIterator,
    {
        self.move_vm
            .runtime
            .loader()
            .check_dependencies_and_charge_gas(
                &self.module_store,
                &mut self.data_cache,
                gas_meter,
                &mut traversal_context.visited,
                traversal_context.referenced_modules,
                ids,
            )
    }

    pub fn check_script_dependencies_and_check_gas(
        &mut self,
        gas_meter: &mut impl GasMeter,
        traversal_context: &mut TraversalContext,
        script: impl Borrow<[u8]>,
    ) -> VMResult<()> {
        self.move_vm
            .runtime
            .loader()
            .check_script_dependencies_and_check_gas(
                &self.module_store,
                &mut self.data_cache,
                gas_meter,
                traversal_context,
                script.borrow(),
            )
    }
}
