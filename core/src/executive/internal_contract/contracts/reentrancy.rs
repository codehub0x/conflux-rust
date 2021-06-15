// Copyright 2020 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use super::{
    super::impls::reentrancy::*, macros::*, ExecutionTrait, SolFnTable,
};
use crate::{
    evm::{ActionParams, Spec},
    executive::InternalRefContext,
    spec::CommonParams,
    trace::{trace::ExecTrace, Tracer},
    vm,
};
use cfx_parameters::internal_contract_addresses::ANTI_REENTRANCY_CONTRACT_ADDRESS;
use cfx_state::state_trait::StateOpsTrait;
use cfx_types::{Address, U256};
#[cfg(test)]
use rustc_hex::FromHex;

make_solidity_contract! {
    pub struct AntiReentrancyConfig(ANTI_REENTRANCY_CONTRACT_ADDRESS,
        generate_fn_table,
        initialize: |params: &CommonParams| params.transition_numbers.cip71a,
        is_active: |spec: &Spec| spec.cip71a);
}
fn generate_fn_table() -> SolFnTable {
    make_function_table!(
        AllowReentrancy,
        AllowReentrancyByAdmin,
        IsReentrancyAllowed
    )
}
group_impl_is_active!(
    |spec: &Spec| spec.cip71a,
    AllowReentrancy,
    AllowReentrancyByAdmin,
    IsReentrancyAllowed
);

make_solidity_function! {
    struct AllowReentrancy(bool, "allowReentrancy(bool)");
}
impl_function_type!(AllowReentrancy, "non_payable_write", gas: |spec: &Spec| spec.sstore_reset_gas);

impl ExecutionTrait for AllowReentrancy {
    fn execute_inner(
        &self, input: bool, params: &ActionParams,
        context: &mut InternalRefContext,
        _tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<()>
    {
        let storage_owner = params.storage_owner;
        let contract_address = params.sender;
        set_reentrancy_allowance(
            &contract_address,
            input,
            context.state,
            context.substate,
            storage_owner,
        )
        .map_err(|err| err.into())
    }
}

make_solidity_function! {
    struct AllowReentrancyByAdmin((Address,bool), "allowReentrancyByAdmin(address,bool)");
}
impl_function_type!(AllowReentrancyByAdmin, "non_payable_write", gas: |spec: &Spec| spec.sstore_reset_gas);

impl ExecutionTrait for AllowReentrancyByAdmin {
    fn execute_inner(
        &self, input: (Address, bool), params: &ActionParams,
        context: &mut InternalRefContext,
        _tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<()>
    {
        let storage_owner = params.storage_owner;
        set_reentrancy_allowance(
            &input.0,
            input.1,
            context.state,
            context.substate,
            storage_owner,
        )
        .map_err(|err| err.into())
    }
}

make_solidity_function! {
    struct IsReentrancyAllowed(Address, "isReentrancyAllowed(address)", bool);
}
impl_function_type!(IsReentrancyAllowed, "query_with_default_gas");

impl ExecutionTrait for IsReentrancyAllowed {
    fn execute_inner(
        &self, input: Address, _params: &ActionParams,
        context: &mut InternalRefContext,
        _tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<bool>
    {
        get_reentrancy_allowance(&input, context.state, context.substate)
            .map_err(|err| err.into())
    }
}
