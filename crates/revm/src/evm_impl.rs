use crate::{
    db::Database,
    gas,
    interpreter::{self, bytecode::Bytecode},
    interpreter::{Contract, Interpreter},
    journaled_state::{Account, JournaledState, State},
    models::SelfDestructResult,
    precompiles, return_ok, return_revert, AnalysisKind, CallContext, CallInputs, CallScheme,
    CreateInputs, CreateScheme, Env, ExecutionResult, Gas, Inspector, Log, Return, Spec,
    SpecId::{self, *},
    TransactOut, TransactTo, Transfer, KECCAK_EMPTY,
};
use alloc::vec::Vec;
use bytes::Bytes;
use core::{cmp::min, marker::PhantomData};
use hashbrown::HashMap as Map;
use primitive_types::{H160, H256, U256};
use revm_precompiles::{Precompile, PrecompileOutput, Precompiles};
use sha3::{Digest, Keccak256};

pub struct EVMData<'a, DB: Database> {
    pub env: &'a mut Env,
    pub journaled_state: JournaledState,
    pub db: &'a mut DB,
    pub error: Option<DB::Error>,
}

pub struct EVMImpl<'a, GSPEC: Spec, DB: Database, const INSPECT: bool> {
    data: EVMData<'a, DB>,
    precompiles: Precompiles,
    inspector: &'a mut dyn Inspector<DB>,
    _phantomdata: PhantomData<GSPEC>,
}

pub trait Transact {
    /// Do transaction.
    /// Return Return, Output for call or Address if we are creating contract, gas spend, gas refunded, State that needs to be applied.
    fn transact(&mut self) -> (ExecutionResult, State);
}

impl<'a, GSPEC: Spec, DB: Database, const INSPECT: bool> Transact
    for EVMImpl<'a, GSPEC, DB, INSPECT>
{
    fn transact(&mut self) -> (ExecutionResult, State) {
        let caller = self.data.env.tx.caller;
        let value = self.data.env.tx.value;
        let data = self.data.env.tx.data.clone();
        let gas_limit = self.data.env.tx.gas_limit;

        println!("Start 1");
        let exit = |reason: Return| (ExecutionResult::new_with_reason(reason), State::new());

        println!("Start 2");
        if GSPEC::enabled(LONDON) {
            if let Some(priority_fee) = self.data.env.tx.gas_priority_fee {
                if priority_fee > self.data.env.tx.gas_price {
                    // or gas_max_fee for eip1559
                    return exit(Return::GasMaxFeeGreaterThanPriorityFee);
                }
            }
            let effective_gas_price = self.data.env.effective_gas_price();
            let basefee = self.data.env.block.basefee;

            // check minimal cost against basefee
            // TODO maybe do this checks when creating evm. We already have all data there
            // or should be move effective_gas_price inside transact fn
            if effective_gas_price < basefee {
                return exit(Return::GasPriceLessThenBasefee);
            }
            // check if priority fee is lower then max fee
        }

        println!("Start 3");
        // unusual to be found here, but check if gas_limit is more then block_gas_limit
        if U256::from(gas_limit) > self.data.env.block.gas_limit {
            return exit(Return::CallerGasLimitMoreThenBlock);
        }
        println!("Start 4");
        let mut gas = Gas::new(gas_limit);
        // record initial gas cost. if not using gas metering init will return 0
        if !gas.record_cost(self.initialization::<GSPEC>()) {
            return exit(Return::OutOfGas);
        }
        println!("Start 5");
        // load acc
        if self
            .data
            .journaled_state
            .load_account(caller, self.data.db)
            .is_err()
        {
            return exit(Return::FatalExternalError);
        }
        println!("Start 6");
        // substract gas_limit*gas_price from current account.
        if let Some(payment_value) =
            U256::from(gas_limit).checked_mul(self.data.env.effective_gas_price())
        {
            let balance = &mut self
                .data
                .journaled_state
                .state
                .get_mut(&caller)
                .unwrap()
                .info
                .balance;
            if payment_value > *balance {
                return exit(Return::LackOfFundForGasLimit);
            }
            *balance -= payment_value;
        } else {
            return exit(Return::OverflowPayment);
        }
        println!("Start 7");
        // check if we have enought balance for value transfer.
        let difference = self.data.env.tx.gas_price - self.data.env.effective_gas_price();
        if difference + value > self.data.journaled_state.account(caller).info.balance {
            return exit(Return::OutOfFund);
        }
        println!("Start 8");
        // record all as cost;
        let gas_limit = gas.remaining();
        if crate::USE_GAS {
            gas.record_cost(gas_limit);
        }
        println!("Start 9");
        // call inner handling of call/create
        let (exit_reason, ret_gas, out) = match self.data.env.tx.transact_to {
            TransactTo::Call(address) => {
                println!("Start 10");
                if self.data.journaled_state.inc_nonce(caller).is_none() {
                    // overflow
                    return exit(Return::NonceOverflow);
                }
                let context = CallContext {
                    caller,
                    address,
                    code_address: address,
                    apparent_value: value,
                    scheme: CallScheme::Call,
                };
                let mut call_input = CallInputs {
                    contract: address,
                    transfer: Transfer {
                        source: caller,
                        target: address,
                        value,
                    },
                    input: data,
                    gas_limit,
                    context,
                };
                println!("Start 11");
                let (exit, gas, bytes) = self.call_inner::<GSPEC>(&mut call_input);
                (exit, gas, TransactOut::Call(bytes))
            }
            TransactTo::Create(scheme) => {
                let mut create_input = CreateInputs {
                    caller,
                    scheme,
                    value,
                    init_code: data,
                    gas_limit,
                };
                let (exit, address, ret_gas, bytes) = self.create_inner::<GSPEC>(&mut create_input);
                (exit, ret_gas, TransactOut::Create(bytes, address))
            }
        };
        println!("Start 13");
        if crate::USE_GAS {
            match exit_reason {
                return_ok!() => {
                    gas.erase_cost(ret_gas.remaining());
                    gas.record_refund(ret_gas.refunded());
                }
                return_revert!() => {
                    gas.erase_cost(ret_gas.remaining());
                }
                _ => {}
            }
        }
        println!("Start 12");
        let (state, logs, gas_used, gas_refunded) = self.finalize::<GSPEC>(caller, &gas);
        (
            ExecutionResult {
                exit_reason,
                out,
                gas_used,
                gas_refunded,
                logs,
            },
            state,
        )
    }
}

impl<'a, GSPEC: Spec, DB: Database, const INSPECT: bool> EVMImpl<'a, GSPEC, DB, INSPECT> {
    pub fn new(
        db: &'a mut DB,
        env: &'a mut Env,
        inspector: &'a mut dyn Inspector<DB>,
        precompiles: Precompiles,
    ) -> Self {
        let journaled_state = if GSPEC::enabled(SpecId::SPURIOUS_DRAGON) {
            JournaledState::new(precompiles.len())
        } else {
            JournaledState::new_legacy(precompiles.len())
        };
        Self {
            data: EVMData {
                env,
                journaled_state,
                db,
                error: None,
            },
            precompiles,
            inspector,
            _phantomdata: PhantomData {},
        }
    }

    fn finalize<SPEC: Spec>(
        &mut self,
        caller: H160,
        gas: &Gas,
    ) -> (Map<H160, Account>, Vec<Log>, u64, u64) {
        let coinbase = self.data.env.block.coinbase;
        let (gas_used, gas_refunded) = if crate::USE_GAS {
            let effective_gas_price = self.data.env.effective_gas_price();
            let basefee = self.data.env.block.basefee;
            let max_refund_quotient = if SPEC::enabled(LONDON) { 5 } else { 2 }; // EIP-3529: Reduction in refunds

            let gas_refunded = min(gas.refunded() as u64, gas.spend() / max_refund_quotient);
            let acc_caller = self.data.journaled_state.state().get_mut(&caller).unwrap();
            acc_caller.info.balance = acc_caller
                .info
                .balance
                .saturating_add(effective_gas_price * (gas.remaining() + gas_refunded));

            // EIP-1559
            let coinbase_gas_price = if SPEC::enabled(LONDON) {
                effective_gas_price.saturating_sub(basefee)
            } else {
                effective_gas_price
            };

            // TODO
            let _ = self
                .data
                .journaled_state
                .load_account(coinbase, self.data.db);
            self.data.journaled_state.touch(&coinbase);
            let acc_coinbase = self
                .data
                .journaled_state
                .state()
                .get_mut(&coinbase)
                .unwrap();
            acc_coinbase.info.balance = acc_coinbase
                .info
                .balance
                .saturating_add(coinbase_gas_price * (gas.spend() - gas_refunded));
            (gas.spend() - gas_refunded, gas_refunded)
        } else {
            // touch coinbase
            // TODO return
            let _ = self
                .data
                .journaled_state
                .load_account(coinbase, self.data.db);
            self.data.journaled_state.touch(&coinbase);
            (0, 0)
        };
        let (mut new_state, logs) = self.data.journaled_state.finalize();
        // precompiles are special case. If there is precompiles in finalized Map that means some balance is
        // added to it, we need now to load precompile address from db and add this amount to it so that we
        // will have sum.
        if self.data.env.cfg.perf_all_precompiles_have_balance {
            for address in self.precompiles.addresses() {
                if let Some(precompile) = new_state.get_mut(address) {
                    // we found it.
                    precompile.info.balance += self
                        .data
                        .db
                        .basic(*address)
                        .ok()
                        .flatten()
                        .map(|acc| acc.balance)
                        .unwrap_or_default();
                }
            }
        }

        (new_state, logs, gas_used, gas_refunded)
    }

    fn initialization<SPEC: Spec>(&mut self) -> u64 {
        let is_create = matches!(self.data.env.tx.transact_to, TransactTo::Create(_));
        let input = &self.data.env.tx.data;

        if crate::USE_GAS {
            let zero_data_len = input.iter().filter(|v| **v == 0).count() as u64;
            let non_zero_data_len = input.len() as u64 - zero_data_len;
            let (accessed_accounts, accessed_slots) = {
                if SPEC::enabled(BERLIN) {
                    let mut accessed_slots = 0_u64;

                    for (address, slots) in self.data.env.tx.access_list.iter() {
                        // TODO return
                        let _ = self
                            .data
                            .journaled_state
                            .load_account(*address, self.data.db);
                        accessed_slots += slots.len() as u64;
                        // TODO return
                        for slot in slots {
                            let _ = self
                                .data
                                .journaled_state
                                .sload(*address, *slot, self.data.db);
                        }
                    }
                    (self.data.env.tx.access_list.len() as u64, accessed_slots)
                } else {
                    (0, 0)
                }
            };

            let transact = if is_create {
                if SPEC::enabled(HOMESTEAD) {
                    // EIP-2: Homestead Hard-fork Changes
                    53000
                } else {
                    21000
                }
            } else {
                21000
            };

            // EIP-2028: Transaction data gas cost reduction
            let gas_transaction_non_zero_data = if SPEC::enabled(ISTANBUL) { 16 } else { 68 };

            transact
                + zero_data_len * gas::TRANSACTION_ZERO_DATA
                + non_zero_data_len * gas_transaction_non_zero_data
                + accessed_accounts * gas::ACCESS_LIST_ADDRESS
                + accessed_slots * gas::ACCESS_LIST_STORAGE_KEY
        } else {
            0
        }
    }

    fn create_inner<SPEC: Spec>(
        &mut self,
        inputs: &mut CreateInputs,
    ) -> (Return, Option<H160>, Gas, Bytes) {
        // Call inspector
        if INSPECT {
            let (ret, address, gas, out) = self.inspector.create(&mut self.data, inputs);
            if ret != Return::Continue {
                return self
                    .inspector
                    .create_end(&mut self.data, inputs, ret, address, gas, out);
            }
        }

        let gas = Gas::new(inputs.gas_limit);
        self.load_account(inputs.caller);

        // Check depth of calls
        if self.data.journaled_state.depth() > interpreter::CALL_STACK_LIMIT {
            return (Return::CallTooDeep, None, gas, Bytes::new());
        }
        // Check balance of caller and value. Do this before increasing nonce
        match self.balance(inputs.caller) {
            Some(i) if i.0 < inputs.value => return (Return::OutOfFund, None, gas, Bytes::new()),
            Some(_) => (),
            _ => return (Return::FatalExternalError, None, gas, Bytes::new()),
        }

        // Increase nonce of caller and check if it overflows
        let old_nonce;
        if let Some(nonce) = self.data.journaled_state.inc_nonce(inputs.caller) {
            old_nonce = nonce - 1;
        } else {
            return (Return::Return, None, gas, Bytes::new());
        }

        // Create address
        let code_hash = H256::from_slice(Keccak256::digest(&inputs.init_code).as_slice());
        let created_address = match inputs.scheme {
            CreateScheme::Create => create_address(inputs.caller, old_nonce),
            CreateScheme::Create2 { salt } => create2_address(inputs.caller, code_hash, salt),
        };
        let ret = Some(created_address);

        // Load account so that it will be hot
        self.load_account(created_address);

        // Enter subroutine
        let checkpoint = self.data.journaled_state.checkpoint();

        // Create contract account and check for collision
        match self.data.journaled_state.create_account(
            created_address,
            self.precompiles.contains(&created_address),
            self.data.db,
        ) {
            Ok(false) => {
                self.data.journaled_state.checkpoint_revert(checkpoint);
                return (Return::CreateCollision, ret, gas, Bytes::new());
            }
            Err(err) => {
                self.data.error = Some(err);
                return (Return::FatalExternalError, ret, gas, Bytes::new());
            }
            Ok(true) => (),
        }

        // Transfer value to contract address
        if let Err(e) = self.data.journaled_state.transfer(
            &inputs.caller,
            &created_address,
            inputs.value,
            self.data.db,
        ) {
            self.data.journaled_state.checkpoint_revert(checkpoint);
            return (e, ret, gas, Bytes::new());
        }

        // EIP-161: State trie clearing (invariant-preserving alternative)
        if SPEC::enabled(SPURIOUS_DRAGON)
            && self
                .data
                .journaled_state
                .inc_nonce(created_address)
                .is_none()
        {
            // overflow
            self.data.journaled_state.checkpoint_revert(checkpoint);
            return (Return::Return, None, gas, Bytes::new());
        }

        // Create new interpreter and execute initcode
        let contract = Contract::new::<SPEC>(
            Bytes::new(),
            Bytecode::new_raw(inputs.init_code.clone()),
            created_address,
            inputs.caller,
            inputs.value,
        );

        #[cfg(feature = "memory_limit")]
        let mut interp = Interpreter::new_with_memory_limit::<SPEC>(
            contract,
            gas.limit(),
            self.data.env.cfg.memory_limit,
        );

        #[cfg(not(feature = "memory_limit"))]
        let mut interp = Interpreter::new::<SPEC>(contract, gas.limit());

        if Self::INSPECT {
            self.inspector
                .initialize_interp(&mut interp, &mut self.data, SPEC::IS_STATIC_CALL);
        }
        let exit_reason = interp.run::<Self, SPEC>(self);

        // Host error if present on execution\
        let (ret, address, gas, out) = match exit_reason {
            return_ok!() => {
                let b = Bytes::new();
                // if ok, check contract creation limit and calculate gas deduction on output len.
                let mut bytes = interp.return_value();

                // EIP-3541: Reject new contract code starting with the 0xEF byte
                if SPEC::enabled(LONDON) && !bytes.is_empty() && bytes.first() == Some(&0xEF) {
                    self.data.journaled_state.checkpoint_revert(checkpoint);
                    return (Return::CreateContractWithEF, ret, interp.gas, b);
                }

                // EIP-170: Contract code size limit
                // By default limit is 0x6000 (~25kb)
                if SPEC::enabled(SPURIOUS_DRAGON)
                    && bytes.len() > self.data.env.cfg.limit_contract_code_size.unwrap_or(0x6000)
                {
                    self.data.journaled_state.checkpoint_revert(checkpoint);
                    return (Return::CreateContractLimit, ret, interp.gas, b);
                }
                if crate::USE_GAS {
                    let gas_for_code = bytes.len() as u64 * crate::gas::CODEDEPOSIT;
                    if !interp.gas.record_cost(gas_for_code) {
                        // record code deposit gas cost and check if we are out of gas.
                        // EIP-2 point 3: If contract creation does not have enough gas to pay for the
                        // final gas fee for adding the contract code to the state, the contract
                        //  creation fails (i.e. goes out-of-gas) rather than leaving an empty contract.
                        if SPEC::enabled(HOMESTEAD) {
                            self.data.journaled_state.checkpoint_revert(checkpoint);
                            return (Return::OutOfGas, ret, interp.gas, b);
                        } else {
                            bytes = Bytes::new();
                        }
                    }
                }
                // if we have enought gas
                self.data.journaled_state.checkpoint_commit();
                // Do analasis of bytecode streight away.
                let bytecode = match self.data.env.cfg.perf_analyse_created_bytecodes {
                    AnalysisKind::Raw => Bytecode::new_raw(bytes),
                    AnalysisKind::Check => Bytecode::new_raw(bytes).to_checked(),
                    AnalysisKind::Analyse => Bytecode::new_raw(bytes).to_analysed::<SPEC>(),
                };

                self.data
                    .journaled_state
                    .set_code(created_address, bytecode);
                (Return::Continue, ret, interp.gas, b)
            }
            _ => {
                self.data.journaled_state.checkpoint_revert(checkpoint);
                (exit_reason, ret, interp.gas, interp.return_value())
            }
        };

        if INSPECT {
            self.inspector
                .create_end(&mut self.data, inputs, ret, address, gas, out)
        } else {
            (ret, address, gas, out)
        }
    }

    fn call_inner<SPEC: Spec>(&mut self, inputs: &mut CallInputs) -> (Return, Gas, Bytes) {
        // Call the inspector
        if INSPECT {
            let (ret, gas, out) = self
                .inspector
                .call(&mut self.data, inputs, SPEC::IS_STATIC_CALL);
            if ret != Return::Continue {
                return self.inspector.call_end(
                    &mut self.data,
                    inputs,
                    gas,
                    ret,
                    out,
                    SPEC::IS_STATIC_CALL,
                );
            }
        }

        let mut gas = Gas::new(inputs.gas_limit);
        // Load account and get code. Account is now hot.
        let bytecode = if let Some((bytecode, _)) = self.code(inputs.contract) {
            bytecode
        } else {
            return (Return::FatalExternalError, gas, Bytes::new());
        };

        // Check depth
        if self.data.journaled_state.depth() > interpreter::CALL_STACK_LIMIT {
            let (ret, gas, out) = (Return::CallTooDeep, gas, Bytes::new());
            if Self::INSPECT {
                return self.inspector.call_end(
                    &mut self.data,
                    inputs,
                    gas,
                    ret,
                    out,
                    SPEC::IS_STATIC_CALL,
                );
            } else {
                return (ret, gas, out);
            }
        }

        // Create subroutine checkpoint
        let checkpoint = self.data.journaled_state.checkpoint();

        // Touch address. For "EIP-158 State Clear", this will erase empty accounts.
        if inputs.transfer.value.is_zero() {
            self.load_account(inputs.context.address);
            self.data.journaled_state.touch(&inputs.context.address);
        }

        // Transfer value from caller to called account
        if let Err(e) = self.data.journaled_state.transfer(
            &inputs.transfer.source,
            &inputs.transfer.target,
            inputs.transfer.value,
            self.data.db,
        ) {
            self.data.journaled_state.checkpoint_revert(checkpoint);
            let (ret, gas, out) = (e, gas, Bytes::new());
            if Self::INSPECT {
                return self.inspector.call_end(
                    &mut self.data,
                    inputs,
                    gas,
                    ret,
                    out,
                    SPEC::IS_STATIC_CALL,
                );
            } else {
                return (ret, gas, out);
            }
        }

        // Call precompiles
        let (ret, gas, out) = if let Some(precompile) = self.precompiles.get(&inputs.contract) {
            let out = match precompile {
                Precompile::Standard(fun) => fun(inputs.input.as_ref(), inputs.gas_limit),
                Precompile::Custom(fun) => fun(inputs.input.as_ref(), inputs.gas_limit),
            };
            match out {
                Ok(PrecompileOutput { output, cost, logs }) => {
                    if !crate::USE_GAS || gas.record_cost(cost) {
                        logs.into_iter().for_each(|l| {
                            self.data.journaled_state.log(Log {
                                address: l.address,
                                topics: l.topics,
                                data: l.data,
                            })
                        });
                        self.data.journaled_state.checkpoint_commit();
                        (Return::Continue, gas, Bytes::from(output))
                    } else {
                        self.data.journaled_state.checkpoint_revert(checkpoint);
                        (Return::OutOfGas, gas, Bytes::new())
                    }
                }
                Err(e) => {
                    let ret = if let precompiles::Return::OutOfGas = e {
                        Return::OutOfGas
                    } else {
                        Return::PrecompileError
                    };
                    self.data.journaled_state.checkpoint_revert(checkpoint); //TODO check if we are discarding or reverting
                    (ret, gas, Bytes::new())
                }
            }
        } else {
            // Create interpreter and execute subcall
            let contract =
                Contract::new_with_context::<SPEC>(inputs.input.clone(), bytecode, &inputs.context);

            #[cfg(feature = "memory_limit")]
            let mut interp = Interpreter::new_with_memory_limit::<SPEC>(
                contract,
                gas.limit(),
                self.data.env.cfg.memory_limit,
            );

            #[cfg(not(feature = "memory_limit"))]
            let mut interp = Interpreter::new::<SPEC>(contract, gas.limit());

            if Self::INSPECT {
                // create is always no static call.
                self.inspector
                    .initialize_interp(&mut interp, &mut self.data, false);
            }
            let exit_reason = interp.run::<Self, SPEC>(self);
            if matches!(exit_reason, return_ok!()) {
                self.data.journaled_state.checkpoint_commit();
            } else {
                self.data.journaled_state.checkpoint_revert(checkpoint);
            }

            (exit_reason, interp.gas, interp.return_value())
        };

        if INSPECT {
            self.inspector
                .call_end(&mut self.data, inputs, gas, ret, out, SPEC::IS_STATIC_CALL)
        } else {
            (ret, gas, out)
        }
    }
}

impl<'a, GSPEC: Spec, DB: Database + 'a, const INSPECT: bool> Host
    for EVMImpl<'a, GSPEC, DB, INSPECT>
{
    const INSPECT: bool = INSPECT;
    type DB = DB;

    fn step(&mut self, interp: &mut Interpreter, is_static: bool) -> Return {
        self.inspector.step(interp, &mut self.data, is_static)
    }

    fn step_end(&mut self, interp: &mut Interpreter, is_static: bool, ret: Return) -> Return {
        self.inspector
            .step_end(interp, &mut self.data, is_static, ret)
    }

    fn env(&mut self) -> &mut Env {
        self.data.env
    }

    fn block_hash(&mut self, number: U256) -> Option<H256> {
        self.data
            .db
            .block_hash(number)
            .map_err(|e| self.data.error = Some(e))
            .ok()
    }

    fn load_account(&mut self, address: H160) -> Option<(bool, bool)> {
        self.data
            .journaled_state
            .load_account_exist(address, self.data.db)
            .map_err(|e| self.data.error = Some(e))
            .ok()
    }

    fn balance(&mut self, address: H160) -> Option<(U256, bool)> {
        let db = &mut self.data.db;
        let journal = &mut self.data.journaled_state;
        let error = &mut self.data.error;
        journal
            .load_account(address, db)
            .map_err(|e| *error = Some(e))
            .ok()
            .map(|(acc, is_cold)| (acc.info.balance, is_cold))
    }

    fn code(&mut self, address: H160) -> Option<(Bytecode, bool)> {
        let journal = &mut self.data.journaled_state;
        let db = &mut self.data.db;
        let error = &mut self.data.error;

        let (acc, is_cold) = journal
            .load_code(address, db)
            .map_err(|e| *error = Some(e))
            .ok()?;
        Some((acc.info.code.clone().unwrap(), is_cold))
    }

    /// Get code hash of address.
    fn code_hash(&mut self, address: H160) -> Option<(H256, bool)> {
        let journal = &mut self.data.journaled_state;
        let db = &mut self.data.db;
        let error = &mut self.data.error;

        let (acc, is_cold) = journal
            .load_code(address, db)
            .map_err(|e| *error = Some(e))
            .ok()?;
        //asume that all precompiles have some balance
        let is_precompile = self.precompiles.contains(&address);
        if is_precompile && self.data.env.cfg.perf_all_precompiles_have_balance {
            return Some((KECCAK_EMPTY, is_cold));
        }
        if acc.is_empty() {
            // TODO check this for pre tangerine fork
            return Some((H256::zero(), is_cold));
        }

        Some((acc.info.code_hash, is_cold))
    }

    fn sload(&mut self, address: H160, index: U256) -> Option<(U256, bool)> {
        // account is always hot. reference on that statement https://eips.ethereum.org/EIPS/eip-2929 see `Note 2:`
        self.data
            .journaled_state
            .sload(address, index, self.data.db)
            .map_err(|e| self.data.error = Some(e))
            .ok()
    }

    fn sstore(
        &mut self,
        address: H160,
        index: U256,
        value: U256,
    ) -> Option<(U256, U256, U256, bool)> {
        self.data
            .journaled_state
            .sstore(address, index, value, self.data.db)
            .map_err(|e| self.data.error = Some(e))
            .ok()
    }

    fn log(&mut self, address: H160, topics: Vec<H256>, data: Bytes) {
        if INSPECT {
            self.inspector.log(&mut self.data, &address, &topics, &data);
        }
        let log = Log {
            address,
            topics,
            data,
        };
        self.data.journaled_state.log(log);
    }

    fn selfdestruct(&mut self, address: H160, target: H160) -> Option<SelfDestructResult> {
        if INSPECT {
            self.inspector.selfdestruct();
        }
        self.data
            .journaled_state
            .selfdestruct(address, target, self.data.db)
            .map_err(|e| self.data.error = Some(e))
            .ok()
    }

    fn create<SPEC: Spec>(
        &mut self,
        inputs: &mut CreateInputs,
    ) -> (Return, Option<H160>, Gas, Bytes) {
        self.create_inner::<SPEC>(inputs)
    }

    fn call<SPEC: Spec>(&mut self, inputs: &mut CallInputs) -> (Return, Gas, Bytes) {
        self.call_inner::<SPEC>(inputs)
    }
}

/// Returns the address for the legacy `CREATE` scheme: [`CreateScheme::Create`]
pub fn create_address(caller: H160, nonce: u64) -> H160 {
    let mut stream = rlp::RlpStream::new_list(2);
    stream.append(&caller);
    stream.append(&nonce);
    let out = H256::from_slice(Keccak256::digest(&stream.out()).as_slice());
    let out = H160::from_slice(&out.as_bytes()[12..]);
    out
}

/// Returns the address for the `CREATE2` scheme: [`CreateScheme::Create2`]
pub fn create2_address(caller: H160, code_hash: H256, salt: U256) -> H160 {
    let mut temp: [u8; 32] = [0; 32];
    salt.to_big_endian(&mut temp);

    let mut hasher = Keccak256::new();
    hasher.update([0xff]);
    hasher.update(&caller[..]);
    hasher.update(temp);
    hasher.update(&code_hash[..]);
    H160::from_slice(&hasher.finalize().as_slice()[12..])
}

/// EVM context host.
pub trait Host {
    const INSPECT: bool;

    type DB: Database;

    fn step(&mut self, interp: &mut Interpreter, is_static: bool) -> Return;
    fn step_end(&mut self, interp: &mut Interpreter, is_static: bool, ret: Return) -> Return;

    fn env(&mut self) -> &mut Env;

    /// load account. Returns (is_cold,is_new_account)
    fn load_account(&mut self, address: H160) -> Option<(bool, bool)>;
    /// Get environmental block hash.
    fn block_hash(&mut self, number: U256) -> Option<H256>;
    /// Get balance of address.
    fn balance(&mut self, address: H160) -> Option<(U256, bool)>;
    /// Get code of address.
    fn code(&mut self, address: H160) -> Option<(Bytecode, bool)>;
    /// Get code hash of address.
    fn code_hash(&mut self, address: H160) -> Option<(H256, bool)>;
    /// Get storage value of address at index.
    fn sload(&mut self, address: H160, index: U256) -> Option<(U256, bool)>;
    /// Set storage value of address at index. Return if slot is cold/hot access.
    fn sstore(
        &mut self,
        address: H160,
        index: U256,
        value: U256,
    ) -> Option<(U256, U256, U256, bool)>;
    /// Create a log owned by address with given topics and data.
    fn log(&mut self, address: H160, topics: Vec<H256>, data: Bytes);
    /// Mark an address to be deleted, with funds transferred to target.
    fn selfdestruct(&mut self, address: H160, target: H160) -> Option<SelfDestructResult>;
    /// Invoke a create operation.
    fn create<SPEC: Spec>(
        &mut self,
        inputs: &mut CreateInputs,
    ) -> (Return, Option<H160>, Gas, Bytes);
    /// Invoke a call operation.
    fn call<SPEC: Spec>(&mut self, input: &mut CallInputs) -> (Return, Gas, Bytes);
}
