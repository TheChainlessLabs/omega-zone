//! Native `TempoState` precompile.
//!
//! Replaces the Solidity TempoState predeploy at `0x1c00...0000` while
//! preserving the zone-facing checkpoint and Tempo storage read ABI.

use alloc::vec::Vec;

use alloy_consensus::BlockHeader;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost, dispatch,
    error::TempoPrecompileError,
    storage::{Handler, StorageCtx, evm::EvmPrecompileStorageProvider},
    view,
};
use tempo_precompiles_macros::contract;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::TempoState as TempoStateAbi;
use zone_primitives::constants::{
    TEMPO_STATE_ADDRESS, ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
};

alloy_sol_types::sol! {
    error Error(string);
    error StaticCallNotAllowed();
}

/// The legacy Solidity implementation packed `tempoBlockNumber` into the low
/// 64 bits of slot 7 alongside gas and timestamp fields.
const LEGACY_PACKED_HEADER_SLOT: u64 = 7;

/// Hash of the native precompile marker bytecode (`0xef`).
const NATIVE_CODE_HASH: B256 =
    b256!("309b8896ee4c1ff7ec1966155373dee42663b6b40c3fedc70ba501684848d2a3");

/// Known Solidity TempoState runtimes shipped by this repository before the
/// native migration. Unknown code at the fixed predeploy address fails closed.
const LEGACY_CODE_HASHES: [B256; 3] = [
    // Committed pre-native `crates/node/tests/assets/zone-test-genesis.json`.
    b256!("018a4ed65d3fd50141b96bd8c93e34b4a5d24849a314bdd7992d3edbd6575367"),
    // Supported `generated/e2e-pass` and `generated/e2e-market` deployments.
    b256!("02c851f19eb44ddfb0e740142bf17fcaaa9322fa90e672dcd40dc2f2772664a8"),
    // Supported `generated/live-zone`, `generated/my-zone`, and `generated/batch-zone` deployments.
    b256!("f97f0a7255f466a558d09772b5e31fc33ac4aebff10c59d7670e5ccc6cfce7ec"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageLayout {
    Native,
    Legacy,
}

/// L1 storage access needed by `readTempoStorageSlot(s)`.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256, PrecompileError>;
}

#[contract(addr = TEMPO_STATE_ADDRESS)]
pub struct TempoState {
    tempo_block_hash: B256,
    tempo_block_number: u64,
}

impl TempoState {
    /// Initializes the predeploy account code and checkpoint from the genesis Tempo header.
    pub fn initialize(&mut self, header_rlp: &[u8]) -> tempo_precompiles::Result<()> {
        self.__initialize()?;
        let mut cursor = header_rlp;
        let header = TempoHeader::decode(&mut cursor).map_err(|err| {
            TempoPrecompileError::Fatal(format!("invalid Tempo genesis header RLP: {err}"))
        })?;
        if !cursor.is_empty() {
            return Err(TempoPrecompileError::Fatal(
                "invalid Tempo genesis header RLP: trailing bytes after header".into(),
            ));
        }
        self.write_checkpoint(header_rlp, header.number())?;
        Ok(())
    }

    fn write_checkpoint(
        &mut self,
        header_rlp: &[u8],
        block_number: u64,
    ) -> tempo_precompiles::Result<B256> {
        let block_hash = keccak256(header_rlp);
        self.tempo_block_hash.write(block_hash)?;
        self.tempo_block_number.write(block_number)?;
        Ok(block_hash)
    }

    fn storage_layout(&self) -> tempo_precompiles::Result<StorageLayout> {
        self.storage
            .with_account_info(TEMPO_STATE_ADDRESS, |info| match info.code_hash {
                NATIVE_CODE_HASH => Ok(StorageLayout::Native),
                code_hash if LEGACY_CODE_HASHES.contains(&code_hash) => Ok(StorageLayout::Legacy),
                code_hash => Err(TempoPrecompileError::Fatal(format!(
                    "unsupported TempoState code hash {code_hash}"
                ))),
            })
    }

    fn read_checkpoint_block_number(
        &self,
        layout: StorageLayout,
    ) -> tempo_precompiles::Result<u64> {
        match layout {
            StorageLayout::Native => self.tempo_block_number.read(),
            StorageLayout::Legacy => {
                let packed = self
                    .storage
                    .sload(TEMPO_STATE_ADDRESS, U256::from(LEGACY_PACKED_HEADER_SLOT))?;
                Ok(packed.as_limbs()[0])
            }
        }
    }

    /// Commit a checkpoint while replacing the old packed layout with the
    /// native two-slot layout. Clearing slot 7 makes the migration one-time.
    fn write_checkpoint_from_legacy(
        &mut self,
        header_rlp: &[u8],
        block_number: u64,
    ) -> tempo_precompiles::Result<B256> {
        let block_hash = keccak256(header_rlp);
        self.__initialize()?;
        self.tempo_block_hash.write(block_hash)?;
        self.storage.sstore(
            TEMPO_STATE_ADDRESS,
            slots::TEMPO_BLOCK_NUMBER,
            U256::from(block_number),
        )?;
        self.storage.sstore(
            TEMPO_STATE_ADDRESS,
            U256::from(LEGACY_PACKED_HEADER_SLOT),
            U256::ZERO,
        )?;
        Ok(block_hash)
    }

    fn is_system_caller(caller: Address) -> bool {
        matches!(
            caller,
            ZONE_INBOX_ADDRESS | ZONE_OUTBOX_ADDRESS | ZONE_CONFIG_ADDRESS
        )
    }

    fn revert_error<E: SolError>(&self, error: E) -> PrecompileResult {
        Ok(self.storage.revert_output(error.abi_encode().into()))
    }

    fn revert_string(&self, message: &str) -> PrecompileResult {
        Ok(self
            .storage
            .revert_output(Error(message.into()).abi_encode().into()))
    }

    fn apply_checkpoint(
        &mut self,
        sender: Address,
        call: TempoStateAbi::finalizeTempoCall,
        layout: StorageLayout,
    ) -> PrecompileResult {
        if self.storage.is_static() {
            return self.revert_error(StaticCallNotAllowed {});
        }
        if sender != ZONE_INBOX_ADDRESS {
            return self.revert_error(TempoStateAbi::OnlyZoneInbox {});
        }

        let prev_block_hash = match self.tempo_block_hash.read() {
            Ok(hash) => hash,
            Err(err) => return self.storage.error_result(err),
        };
        let prev_block_number = match self.read_checkpoint_block_number(layout) {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };

        let mut header_cursor = call.header.as_ref();
        let header = match TempoHeader::decode(&mut header_cursor) {
            Ok(header) => header,
            Err(_) => return self.revert_error(TempoStateAbi::InvalidRlpData {}),
        };
        if !header_cursor.is_empty() {
            return self.revert_error(TempoStateAbi::InvalidRlpData {});
        }

        if header.parent_hash() != prev_block_hash {
            return self.revert_error(TempoStateAbi::InvalidParentHash {});
        }
        if header.number() != prev_block_number.saturating_add(1) {
            return self.revert_error(TempoStateAbi::InvalidBlockNumber {});
        }

        let checkpoint = match layout {
            StorageLayout::Legacy => {
                self.write_checkpoint_from_legacy(&call.header, header.number())
            }
            StorageLayout::Native => self.write_checkpoint(&call.header, header.number()),
        };
        let tempo_block_hash = match checkpoint {
            Ok(hash) => hash,
            Err(err) => return self.storage.error_result(err),
        };
        if let Err(err) = self.emit_event(TempoStateAbi::TempoBlockFinalized {
            blockHash: tempo_block_hash,
            blockNumber: header.number(),
            stateRoot: header.state_root(),
        }) {
            return self.storage.error_result(err);
        }

        Ok(self.storage.success_output(Bytes::new()))
    }

    fn read_tempo_storage_slot<P: L1StorageReader>(
        &mut self,
        provider: &P,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotCall,
        layout: StorageLayout,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.read_checkpoint_block_number(layout) {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };
        let value = provider.read_l1_storage(call.account, call.slot, block_number)?;
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotCall::abi_encode_returns(&value).into(),
        ))
    }

    fn read_tempo_storage_slots<P: L1StorageReader>(
        &mut self,
        provider: &P,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotsCall,
        layout: StorageLayout,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.read_checkpoint_block_number(layout) {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };
        let mut values = Vec::with_capacity(call.slots.len());
        for slot in call.slots {
            values.push(provider.read_l1_storage(call.account, slot, block_number)?);
        }
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotsCall::abi_encode_returns(&values).into(),
        ))
    }

    /// Wraps this precompile for registration in the zone EVM.
    pub fn create<P: L1StorageReader>(
        provider: P,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();

        DynPrecompile::new_stateful(PrecompileId::Custom("TempoState".into()), move |input| {
            if !input.is_direct_call() {
                return Ok(PrecompileOutput::revert(
                    0,
                    SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                    input.reservoir,
                ));
            }

            let mut storage = EvmPrecompileStorageProvider::new(
                input.internals,
                input.gas,
                input.reservoir,
                spec,
                amsterdam_eip8037_enabled,
                input.is_static,
                gas_params.clone(),
            );

            StorageCtx::enter(&mut storage, || {
                Self::new().call_with_provider(&provider, input.data, input.caller)
            })
        })
    }

    fn call_with_provider<P: L1StorageReader>(
        &mut self,
        provider: &P,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }
        let layout = match self.storage_layout() {
            Ok(layout) => layout,
            Err(err) => return self.storage.error_result(err),
        };

        dispatch!(
            calldata,
            |call| match call {
                TempoStateAbi::TempoStateCalls {
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash.read()),
                    tempoBlockNumber(call) => {
                        view(call, |_| self.read_checkpoint_block_number(layout))
                    },
                    finalizeTempo(call) => self.apply_checkpoint(msg_sender, call, layout),
                    readTempoStorageSlot(call) => {
                        self.read_tempo_storage_slot(provider, msg_sender, call, layout)
                    },
                    readTempoStorageSlots(call) => {
                        self.read_tempo_storage_slots(provider, msg_sender, call, layout)
                    },
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_evm::{
        EvmInternals,
        precompiles::{DynPrecompile, Precompile as AlloyEvmPrecompile, PrecompileInput},
    };
    use alloy_primitives::{U256, address, b256};
    use alloy_rlp::Encodable as _;
    use alloy_sol_types::SolCall;
    use revm::{
        Context,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::storage::PrecompileStorageProvider as _;

    type TestContext = Context<
        revm::context::BlockEnv,
        revm::context::TxEnv,
        revm::context::CfgEnv<TempoHardfork>,
        CacheDB<EmptyDB>,
    >;
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Clone)]
    struct MockL1Reader {
        value: B256,
    }

    impl L1StorageReader for MockL1Reader {
        fn read_l1_storage(
            &self,
            _account: Address,
            _slot: B256,
            _block_number: u64,
        ) -> Result<B256, PrecompileError> {
            Ok(self.value)
        }
    }

    fn encode_header(header: &TempoHeader) -> Bytes {
        let mut encoded = Vec::new();
        header.encode(&mut encoded);
        encoded.into()
    }

    fn test_context() -> TestContext {
        Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default())
    }

    fn initialize(ctx: &mut TestContext, header: &[u8]) -> TestResult {
        let spec = ctx.cfg.spec;
        let amsterdam_eip8037_enabled = ctx.cfg.enable_amsterdam_eip8037;
        let gas_params = ctx.cfg.gas_params.clone();
        let mut storage = EvmPrecompileStorageProvider::new(
            EvmInternals::from_context(ctx),
            u64::MAX,
            0,
            spec,
            amsterdam_eip8037_enabled,
            false,
            gas_params,
        );

        StorageCtx::enter(&mut storage, || TempoState::new().initialize(header))?;
        Ok(())
    }

    fn seed_legacy_checkpoint(
        ctx: &mut TestContext,
        code_hash: B256,
        block_hash: B256,
        block_number: u64,
    ) -> TestResult<U256> {
        ctx.journaled_state.database.insert_account_info(
            TEMPO_STATE_ADDRESS,
            AccountInfo {
                code_hash,
                nonce: 1,
                ..Default::default()
            },
        );
        ctx.journaled_state.database.insert_account_storage(
            TEMPO_STATE_ADDRESS,
            slots::TEMPO_BLOCK_HASH,
            U256::from_be_slice(block_hash.as_slice()),
        )?;
        // Legacy slot 1 held packed wrapper gas fields, not the block number.
        ctx.journaled_state.database.insert_account_storage(
            TEMPO_STATE_ADDRESS,
            slots::TEMPO_BLOCK_NUMBER,
            U256::from(30_000_000u64) << 64,
        )?;
        let packed = U256::from(block_number)
            | (U256::from(30_000_000u64) << 64)
            | (U256::from(21_000u64) << 128)
            | (U256::from(1_700_000_000u64) << 192);
        ctx.journaled_state.database.insert_account_storage(
            TEMPO_STATE_ADDRESS,
            U256::from(LEGACY_PACKED_HEADER_SLOT),
            packed,
        )?;
        Ok(packed)
    }

    fn account_code_hash(ctx: &mut TestContext) -> TestResult<B256> {
        let spec = ctx.cfg.spec;
        let amsterdam_eip8037_enabled = ctx.cfg.enable_amsterdam_eip8037;
        let gas_params = ctx.cfg.gas_params.clone();
        let mut storage = EvmPrecompileStorageProvider::new(
            EvmInternals::from_context(ctx),
            u64::MAX,
            0,
            spec,
            amsterdam_eip8037_enabled,
            false,
            gas_params,
        );
        let mut code_hash = B256::ZERO;
        storage.with_account_info(TEMPO_STATE_ADDRESS, &mut |info| {
            code_hash = info.code_hash;
        })?;
        Ok(code_hash)
    }

    fn raw_storage(ctx: &mut TestContext, slot: U256) -> TestResult<U256> {
        let spec = ctx.cfg.spec;
        let amsterdam_eip8037_enabled = ctx.cfg.enable_amsterdam_eip8037;
        let gas_params = ctx.cfg.gas_params.clone();
        let mut storage = EvmPrecompileStorageProvider::new(
            EvmInternals::from_context(ctx),
            u64::MAX,
            0,
            spec,
            amsterdam_eip8037_enabled,
            false,
            gas_params,
        );
        Ok(storage.sload(TEMPO_STATE_ADDRESS, slot)?)
    }

    fn call(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        caller: Address,
        calldata: Bytes,
        is_static: bool,
    ) -> PrecompileResult {
        call_with_bytecode_address(
            ctx,
            precompile,
            caller,
            calldata,
            is_static,
            TEMPO_STATE_ADDRESS,
        )
    }

    fn call_with_bytecode_address(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        caller: Address,
        calldata: Bytes,
        is_static: bool,
        bytecode_address: Address,
    ) -> PrecompileResult {
        AlloyEvmPrecompile::call(
            precompile,
            PrecompileInput {
                data: &calldata,
                gas: u64::MAX,
                reservoir: 0,
                caller,
                value: U256::ZERO,
                target_address: TEMPO_STATE_ADDRESS,
                is_static,
                bytecode_address,
                internals: EvmInternals::from_context(ctx),
            },
        )
    }

    fn child_header(parent_hash: B256, number: u64) -> TempoHeader {
        TempoHeader {
            general_gas_limit: 1_000_000,
            shared_gas_limit: 2_000_000,
            timestamp_millis_part: 123,
            inner: alloy_consensus::Header {
                parent_hash,
                beneficiary: address!("0x000000000000000000000000000000000000bEEF"),
                state_root: b256!(
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                ),
                transactions_root: b256!(
                    "0x2222222222222222222222222222222222222222222222222222222222222222"
                ),
                receipts_root: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                number,
                gas_limit: 30_000_000,
                gas_used: 21_000,
                timestamp: 1_700_000_000,
                mix_hash: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn finalize_calldata(header: Bytes) -> Bytes {
        TempoStateAbi::finalizeTempoCall { header }
            .abi_encode()
            .into()
    }

    fn assert_checkpoint(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        expected_hash: B256,
        expected_number: u64,
    ) -> TestResult {
        let block_hash = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockHashCall::abi_decode_returns(&block_hash.bytes)?,
            expected_hash
        );

        let block_number = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockNumberCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockNumberCall::abi_decode_returns(&block_number.bytes)?,
            expected_number
        );
        Ok(())
    }

    #[test]
    fn initialize_sets_checkpoint() -> TestResult {
        let header = child_header(B256::repeat_byte(0xaa), 42);
        let header_rlp = encode_header(&header);
        let mut ctx = test_context();
        initialize(&mut ctx, &header_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        assert_checkpoint(&mut ctx, &precompile, keccak256(&header_rlp), 42)?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_updates_checkpoint() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child = child_header(genesis_hash, 1);
        let child_rlp = encode_header(&child);
        let child_hash = keccak256(&child_rlp);
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());

        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;
        assert!(output.is_success());
        assert_checkpoint(&mut ctx, &precompile, child_hash, 1)?;

        Ok(())
    }

    #[test]
    fn legacy_layout_views_and_first_finalize_migrate_to_native() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        seed_legacy_checkpoint(
            &mut ctx,
            LEGACY_CODE_HASHES[0],
            genesis_hash,
            genesis.number(),
        )?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        // Legacy block zero is valid even though the low 64 bits of slot 7 are zero.
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, 0)?;

        let child = child_header(genesis_hash, 1);
        let child_rlp = encode_header(&child);
        let child_hash = keccak256(&child_rlp);
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;
        assert!(output.is_success());
        assert_checkpoint(&mut ctx, &precompile, child_hash, 1)?;
        assert_eq!(account_code_hash(&mut ctx)?, NATIVE_CODE_HASH);
        assert_eq!(raw_storage(&mut ctx, slots::TEMPO_BLOCK_NUMBER)?, U256::ONE);
        assert_eq!(
            raw_storage(&mut ctx, U256::from(LEGACY_PACKED_HEADER_SLOT))?,
            U256::ZERO
        );

        // A subsequent finalize uses the native layout, proving migration is one-time.
        let grandchild_rlp = encode_header(&child_header(child_hash, 2));
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(grandchild_rlp),
            false,
        )?;
        assert!(output.is_success());
        Ok(())
    }

    #[test]
    fn invalid_legacy_finalize_does_not_migrate() -> TestResult {
        let parent = child_header(B256::repeat_byte(0xaa), 42);
        let parent_rlp = encode_header(&parent);
        let parent_hash = keccak256(&parent_rlp);
        let mut ctx = test_context();
        let legacy_packed = seed_legacy_checkpoint(
            &mut ctx,
            LEGACY_CODE_HASHES[1],
            parent_hash,
            parent.number(),
        )?;
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());

        let invalid = encode_header(&child_header(parent_hash, 44));
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(invalid),
            false,
        )?;
        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, parent_hash, 42)?;
        assert_eq!(account_code_hash(&mut ctx)?, LEGACY_CODE_HASHES[1]);
        assert_eq!(
            raw_storage(&mut ctx, U256::from(LEGACY_PACKED_HEADER_SLOT))?,
            legacy_packed
        );
        Ok(())
    }

    #[test]
    fn unknown_tempostate_code_hash_fails_closed() -> TestResult {
        let mut ctx = test_context();
        ctx.journaled_state.database.insert_account_info(
            TEMPO_STATE_ADDRESS,
            AccountInfo {
                code_hash: B256::repeat_byte(0x99),
                nonce: 1,
                ..Default::default()
            },
        );
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let err = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
        )
        .expect_err("unknown TempoState code must not be interpreted as native or legacy");
        assert!(err.to_string().contains("unsupported TempoState code hash"));
        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_for_non_inbox_caller() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn delegate_call_reverts() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call_with_bytecode_address(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
            address!("0x000000000000000000000000000000000000dEaD"),
        )?;

        assert!(output.is_revert());

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_static_call() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            true,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_rlp() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(Bytes::from(vec![0xff])),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_trailing_header_bytes() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let mut malformed = child_rlp.to_vec();
        malformed.push(0);
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(Bytes::from(malformed)),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_parent_hash() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(B256::ZERO, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_block_number() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 2));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn read_tempo_storage_slot_is_system_only() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let expected = b256!("0xabababababababababababababababababababababababababababababababab");
        let precompile = TempoState::create(MockL1Reader { value: expected }, &ctx.cfg.clone());
        let calldata: Bytes = TempoStateAbi::readTempoStorageSlotCall {
            account: address!("0x0000000000000000000000000000000000009999"),
            slot: B256::ZERO,
        }
        .abi_encode()
        .into();

        let outsider = call(
            &mut ctx,
            &precompile,
            address!("0x000000000000000000000000000000000000aaaa"),
            calldata.clone(),
            true,
        )?;
        assert!(outsider.is_revert());

        let system = call(&mut ctx, &precompile, ZONE_CONFIG_ADDRESS, calldata, true)?;
        assert_eq!(
            TempoStateAbi::readTempoStorageSlotCall::abi_decode_returns(&system.bytes)?,
            expected
        );

        Ok(())
    }

    #[test]
    fn read_tempo_storage_slots_returns_batch() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let expected = b256!("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
        let precompile = TempoState::create(MockL1Reader { value: expected }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_OUTBOX_ADDRESS,
            TempoStateAbi::readTempoStorageSlotsCall {
                account: address!("0x0000000000000000000000000000000000009999"),
                slots: vec![B256::ZERO, B256::with_last_byte(1)],
            }
            .abi_encode()
            .into(),
            true,
        )?;

        assert_eq!(
            TempoStateAbi::readTempoStorageSlotsCall::abi_decode_returns(&output.bytes)?,
            vec![expected, expected]
        );

        Ok(())
    }
}
