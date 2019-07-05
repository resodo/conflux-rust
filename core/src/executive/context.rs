// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

//! Transaction Execution environment.
use super::executive::*;
use crate::{
    bytes::Bytes,
    machine::Machine,
    state::{CleanupMode, State, Substate},
    vm::{
        self, ActionParams, ActionValue, CallType, Context as ContextTrait,
        ContractCreateResult, CreateContractAddress, Env, MessageCallResult,
        ReturnData, Spec, TrapKind,
    },
};
use cfx_types::{Address, H256, U256};
use primitives::transaction::UNSIGNED_SENDER;
use std::sync::Arc;

/// Policy for handling output data on `RETURN` opcode.
pub enum OutputPolicy {
    /// Return reference to fixed sized output.
    /// Used for message calls.
    Return,
    /// Init new contract as soon as `RETURN` is called.
    InitContract,
}

/// Transaction properties that context need to know about.
pub struct Origin {
    address: Address,
    origin: Address,
    gas_price: U256,
    value: U256,
}

impl Origin {
    /// Populates origin info from action params.
    pub fn from(params: &ActionParams) -> Self {
        Origin {
            address: params.address.clone(),
            origin: params.origin.clone(),
            gas_price: params.gas_price,
            value: match params.value {
                ActionValue::Transfer(val) | ActionValue::Apparent(val) => val,
            },
        }
    }
}

/// Implementation of evm context.
#[allow(dead_code)]
pub struct Context<'a, 'b: 'a> {
    state: &'a mut State<'b>,
    env: &'a Env,
    depth: usize,
    stack_depth: usize,
    origin: &'a Origin,
    substate: &'a mut Substate,
    machine: &'a Machine,
    spec: &'a Spec,
    output: OutputPolicy,
    static_flag: bool,
}

impl<'a, 'b: 'a> Context<'a, 'b> {
    /// Basic `Context` constructor.
    pub fn new(
        state: &'a mut State<'b>, env: &'a Env, machine: &'a Machine,
        spec: &'a Spec, depth: usize, stack_depth: usize, origin: &'a Origin,
        substate: &'a mut Substate, output: OutputPolicy, static_flag: bool,
    ) -> Self
    {
        Context {
            state,
            env,
            depth,
            stack_depth,
            origin,
            substate,
            machine,
            spec,
            output,
            static_flag,
        }
    }
}

impl<'a, 'b: 'a> ContextTrait for Context<'a, 'b> {
    fn initial_storage_at(&self, key: &H256) -> vm::Result<H256> {
        self.state
            .checkpoint_storage_at(0, &self.origin.address, key)
            .map(|v| v.unwrap_or(H256::zero()))
            .map_err(Into::into)
    }

    fn storage_at(&self, key: &H256) -> vm::Result<H256> {
        self.state
            .storage_at(&self.origin.address, key)
            .map_err(Into::into)
    }

    fn set_storage(&mut self, key: H256, value: H256) -> vm::Result<()> {
        if self.static_flag {
            Err(vm::Error::MutableCallInStaticContext)
        } else {
            self.state
                .set_storage(&self.origin.address, key, value)
                .map_err(Into::into)
        }
    }

    fn is_static(&self) -> bool { return self.static_flag; }

    fn exists(&self, address: &Address) -> vm::Result<bool> {
        self.state.exists(address).map_err(Into::into)
    }

    fn exists_and_not_null(&self, address: &Address) -> vm::Result<bool> {
        self.state.exists_and_not_null(address).map_err(Into::into)
    }

    fn origin_balance(&self) -> vm::Result<U256> {
        self.balance(&self.origin.address).map_err(Into::into)
    }

    fn balance(&self, address: &Address) -> vm::Result<U256> {
        self.state.balance(address).map_err(Into::into)
    }

    fn blockhash(&mut self, _number: &U256) -> H256 {
        // TODO: I have no idea why we need this function
        H256::default()
    }

    fn create(
        &mut self, gas: &U256, value: &U256, code: &[u8],
        address_scheme: CreateContractAddress, trap: bool,
    ) -> ::std::result::Result<ContractCreateResult, TrapKind>
    {
        // create new contract address
        let (address, code_hash) = match self.state.nonce(&self.origin.address)
        {
            Ok(nonce) => self::contract_address(
                address_scheme,
                &self.origin.address,
                &nonce,
                &code,
            ),
            Err(e) => {
                debug!(target: "context", "Database corruption encountered: {:?}", e);
                return Ok(ContractCreateResult::Failed);
            }
        };

        // prepare the params
        let params = ActionParams {
            code_address: address.clone(),
            address: address.clone(),
            sender: self.origin.address.clone(),
            origin: self.origin.origin.clone(),
            gas: *gas,
            gas_price: self.origin.gas_price,
            value: ActionValue::Transfer(*value),
            code: Some(Arc::new(code.to_vec())),
            code_hash,
            data: None,
            call_type: CallType::None,
            params_type: vm::ParamsType::Embedded,
        };

        if !self.static_flag {
            if !self.spec.keep_unsigned_nonce
                || params.sender != UNSIGNED_SENDER
            {
                if let Err(e) = self.state.inc_nonce(&self.origin.address) {
                    debug!(target: "ext", "Database corruption encountered: {:?}", e);
                    return Ok(ContractCreateResult::Failed);
                }
            }
        }

        if trap {
            return Err(TrapKind::Create(params, address));
        }

        // TODO: handle internal error separately
        let mut ex = Executive::from_parent(
            self.state,
            self.env,
            self.machine,
            self.spec,
            self.depth,
            self.static_flag,
        );
        let out = ex.create_with_crossbeam(
            params,
            self.substate,
            self.stack_depth + 1,
        );
        Ok(into_contract_create_result(out, &address, self.substate))
    }

    fn call(
        &mut self, gas: &U256, sender_address: &Address,
        receive_address: &Address, value: Option<U256>, data: &[u8],
        code_address: &Address, call_type: CallType, trap: bool,
    ) -> ::std::result::Result<MessageCallResult, TrapKind>
    {
        trace!(target: "context", "call");

        assert!(trap);

        let code_with_hash = self.state.code(code_address).and_then(|code| {
            self.state.code_hash(code_address).map(|hash| (code, hash))
        });

        let (code, code_hash) = match code_with_hash {
            Ok((code, hash)) => (code, hash),
            Err(_) => return Ok(MessageCallResult::Failed),
        };

        let mut params = ActionParams {
            sender: sender_address.clone(),
            address: receive_address.clone(),
            value: ActionValue::Apparent(self.origin.value),
            code_address: code_address.clone(),
            origin: self.origin.origin.clone(),
            gas: *gas,
            gas_price: self.origin.gas_price,
            code,
            code_hash,
            data: Some(data.to_vec()),
            call_type,
            params_type: vm::ParamsType::Separate,
        };

        if let Some(value) = value {
            params.value = ActionValue::Transfer(value);
        }

        return Err(TrapKind::Call(params));
    }

    fn extcode(&self, address: &Address) -> vm::Result<Option<Arc<Bytes>>> {
        Ok(self.state.code(address)?)
    }

    fn extcodehash(&self, address: &Address) -> vm::Result<Option<H256>> {
        Ok(self.state.code_hash(address)?)
    }

    fn extcodesize(&self, address: &Address) -> vm::Result<Option<usize>> {
        Ok(self.state.code_size(address)?)
    }

    fn ret(
        self, gas: &U256, data: &ReturnData, apply_state: bool,
    ) -> vm::Result<U256>
    where Self: Sized {
        match self.output {
            OutputPolicy::Return => Ok(*gas),
            OutputPolicy::InitContract if apply_state => {
                let return_cost = U256::from(data.len())
                    * U256::from(self.spec.create_data_gas);
                if return_cost > *gas
                    || data.len() > self.spec.create_data_limit
                {
                    return match self.spec.exceptional_failed_code_deposit {
                        true => Err(vm::Error::OutOfGas),
                        false => Ok(*gas),
                    };
                }
                self.state.init_code(&self.origin.address, data.to_vec())?;
                Ok(*gas - return_cost)
            }
            OutputPolicy::InitContract => Ok(*gas),
        }
    }

    fn log(&mut self, topics: Vec<H256>, data: &[u8]) -> vm::Result<()> {
        use primitives::log_entry::LogEntry;

        if self.static_flag {
            return Err(vm::Error::MutableCallInStaticContext);
        }

        let address = self.origin.address.clone();
        self.substate.logs.push(LogEntry {
            address,
            topics,
            data: data.to_vec(),
        });

        Ok(())
    }

    fn suicide(&mut self, refund_address: &Address) -> vm::Result<()> {
        if self.static_flag {
            return Err(vm::Error::MutableCallInStaticContext);
        }

        let address = self.origin.address.clone();
        let balance = self.balance(&address)?;
        if &address == refund_address {
            self.state.sub_balance(
                &address,
                &balance,
                &mut CleanupMode::NoEmpty,
            )?;
        } else {
            trace!(target: "context", "Suiciding {} -> {} (xfer: {})", address, refund_address, balance);
            self.state.transfer_balance(
                &address,
                refund_address,
                &balance,
                self.substate.to_cleanup_mode(&self.spec),
            )?;
        }

        self.substate.suicides.insert(address);

        Ok(())
    }

    fn spec(&self) -> &Spec { &self.spec }

    fn env(&self) -> &Env { &self.env }

    fn depth(&self) -> usize { self.depth }

    fn add_sstore_refund(&mut self, value: usize) {
        self.substate.sstore_clears_refund += value as i128;
    }

    fn sub_sstore_refund(&mut self, value: usize) {
        self.substate.sstore_clears_refund -= value as i128;
    }

    fn trace_next_instruction(
        &mut self, _pc: usize, _instruction: u8, _current_gas: U256,
    ) -> bool {
        // TODO
        false
    }

    fn trace_prepare_execute(
        &mut self, _pc: usize, _instruction: u8, _gas_cost: U256,
        _mem_written: Option<(usize, usize)>,
        _store_written: Option<(U256, U256)>,
    )
    {
        // TODO
    }

    fn trace_executed(
        &mut self, _gas_used: U256, _stack_push: &[U256], _mem: &[u8],
    ) {
        // TODO
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use crate::{
        machine::new_machine,
        statedb::StateDb,
        storage::{
            new_storage_manager_for_testing, state::StateTrait, StorageManager,
            StorageManagerTrait,
        },
        test_helpers::get_state_for_genesis_write,
        vm::Env,
        vm_factory::VmFactory,
    };
    use cfx_types::{Address, U256};
    use std::ops::Deref;

    fn get_test_origin() -> Origin {
        Origin {
            address: Address::zero(),
            origin: Address::zero(),
            gas_price: U256::zero(),
            value: U256::zero(),
        }
    }

    fn get_test_env() -> Env {
        Env {
            number: 100,
            author: 0.into(),
            timestamp: 0,
            difficulty: 0.into(),
            last_hashes: Arc::new(vec![]),
            gas_used: 0.into(),
            gas_limit: 0.into(),
        }
    }

    struct TestSetup {
        storage_manager: Option<Box<StorageManager>>,
        state: Option<State<'static>>,
        machine: Machine,
        spec: Spec,
        substate: Substate,
        env: Env,
    }

    impl TestSetup {
        fn init_state(&mut self, storage_manager: &'static StorageManager) {
            self.state = Some(get_state_for_genesis_write(storage_manager));
        }

        fn new() -> Self {
            let storage_manager = Box::new(new_storage_manager_for_testing());
            let machine = new_machine();
            let env = get_test_env();
            let spec = machine.spec_at(env.number);

            let mut setup = Self {
                storage_manager: None,
                state: None,
                machine,
                spec,
                substate: Substate::new(),
                env,
            };
            setup.storage_manager = Some(storage_manager);
            setup.init_state(unsafe {
                &*(setup.storage_manager.as_ref().unwrap().as_ref()
                    as *const StorageManager)
            });

            setup
        }
    }

    #[test]
    fn can_be_created() {
        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        let ctx = Context::new(
            state,
            &setup.env,
            &setup.machine,
            &setup.spec,
            0,
            0,
            &origin,
            &mut setup.substate,
            OutputPolicy::InitContract,
            false,
        );

        assert_eq!(ctx.env().number, 100);
    }

    #[test]
    fn can_return_block_hash_no_env() {
        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        let mut ctx = Context::new(
            state,
            &setup.env,
            &setup.machine,
            &setup.spec,
            0,
            0,
            &origin,
            &mut setup.substate,
            OutputPolicy::InitContract,
            false,
        );

        let hash = ctx.blockhash(
            &"0000000000000000000000000000000000000000000000000000000000120000"
                .parse::<U256>()
                .unwrap(),
        );

        assert_eq!(hash, H256::zero());
    }

    #[test]
    #[should_panic]
    fn can_call_fail_empty() {
        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        let mut ctx = Context::new(
            state,
            &setup.env,
            &setup.machine,
            &setup.spec,
            0,
            0,
            &origin,
            &mut setup.substate,
            OutputPolicy::InitContract,
            false,
        );

        // this should panic because we have no balance on any account
        ctx.call(
            &"0000000000000000000000000000000000000000000000000000000000120000".parse::<U256>().unwrap(),
            &Address::new(),
            &Address::new(),
            Some("0000000000000000000000000000000000000000000000000000000000150000".parse::<U256>().unwrap()),
            &[],
            &Address::new(),
            CallType::Call,
            false,
        ).ok().unwrap();
    }

    #[test]
    fn can_log() {
        let log_data = vec![120u8, 110u8];
        let log_topics = vec![H256::from(
            "af0fa234a6af46afa23faf23bcbc1c1cb4bcb7bcbe7e7e7ee3ee2edddddddddd",
        )];

        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        {
            let mut ctx = Context::new(
                state,
                &setup.env,
                &setup.machine,
                &setup.spec,
                0,
                0,
                &origin,
                &mut setup.substate,
                OutputPolicy::InitContract,
                false,
            );
            ctx.log(log_topics, &log_data).unwrap();
        }

        assert_eq!(setup.substate.logs.len(), 1);
    }

    #[test]
    fn can_suicide() {
        let refund_account = &Address::new();

        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        {
            let mut ctx = Context::new(
                state,
                &setup.env,
                &setup.machine,
                &setup.spec,
                0,
                0,
                &origin,
                &mut setup.substate,
                OutputPolicy::InitContract,
                false,
            );
            ctx.suicide(refund_account).unwrap();
        }

        assert_eq!(setup.substate.suicides.len(), 1);
    }

    #[test]
    fn can_create() {
        use std::str::FromStr;

        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        let address = {
            let mut ctx = Context::new(
                state,
                &setup.env,
                &setup.machine,
                &setup.spec,
                0,
                0,
                &origin,
                &mut setup.substate,
                OutputPolicy::InitContract,
                false,
            );
            match ctx.create(&U256::max_value(), &U256::zero(), &[], CreateContractAddress::FromSenderAndNonce, false) {
                Ok(ContractCreateResult::Created(address, _)) => address,
                _ => panic!("Test create failed; expected Created, got Failed/Reverted."),
            }
        };

        assert_eq!(
            address,
            Address::from_str("bd770416a3345f91e4b34576cb804a576fa48eb1")
                .unwrap()
        );
    }

    #[test]
    fn can_create2() {
        use std::str::FromStr;

        let mut setup = TestSetup::new();
        let state = &mut setup.state.unwrap();
        let origin = get_test_origin();

        let address = {
            let mut ctx = Context::new(
                state,
                &setup.env,
                &setup.machine,
                &setup.spec,
                0,
                0,
                &origin,
                &mut setup.substate,
                OutputPolicy::InitContract,
                false,
            );

            match ctx.create(&U256::max_value(), &U256::zero(), &[], CreateContractAddress::FromSenderSaltAndCodeHash(H256::default()), false) {
                Ok(ContractCreateResult::Created(address, _)) => address,
                _ => panic!("Test create failed; expected Created, got Failed/Reverted."),
            }
        };

        assert_eq!(
            address,
            Address::from_str("e33c0c7f7df4809055c3eba6c09cfe4baf1bd9e0")
                .unwrap()
        );
    }
}
