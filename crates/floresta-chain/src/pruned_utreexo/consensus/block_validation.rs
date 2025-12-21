use core::ffi::c_uint;

use bitcoin::script;
use bitcoin::Block;
use bitcoin::OutPoint;
use bitcoin::Transaction;
use floresta_common::prelude::HashMap;

use super::Consensus;
use super::COIN_VALUE;
use crate::BlockValidationErrors;
use crate::BlockchainError;
use crate::UtxoData;

impl Consensus {
    /// Returns the amount of block subsidy to be paid in a block, given it's height.
    ///
    /// The Bitcoin Core source can be found [here](https://github.com/bitcoin/bitcoin/blob/2b211b41e36f914b8d0487e698b619039cc3c8e2/src/validation.cpp#L1501-L1512).
    pub fn get_subsidy(&self, height: u32) -> u64 {
        let halvings = height / self.parameters.subsidy_halving_interval as u32;
        // Force block reward to zero when right shift is undefined.
        if halvings >= 64 {
            return 0;
        }
        let mut subsidy = 50 * COIN_VALUE;
        // Subsidy is cut in half every 210,000 blocks which will occur approximately every 4 years.
        subsidy >>= halvings;
        subsidy
    }

    pub fn get_bip34_height(&self, block: &Block) -> Option<u32> {
        let cb = block.coinbase()?;
        let input = cb.input.first()?;
        let push = input.script_sig.instructions_minimal().next()?;

        match push {
            Ok(script::Instruction::PushBytes(b)) => {
                let h = script::read_scriptint(b.as_bytes()).ok()?;
                Some(h as u32)
            }

            Ok(script::Instruction::Op(opcode)) => {
                let opcode = opcode.to_u8();
                if (0x51..=0x60).contains(&opcode) {
                    Some(opcode as u32 - 0x50)
                } else {
                    None
                }
            }

            _ => None,
        }
    }

    /// Validates the block without checking whether the inputs are present in the UTXO set. This
    /// function contains the core validation logic.
    ///
    /// The methods `BlockchainInterface::validate_block` and `UpdatableChainstate::connect_block`
    /// call this and additionally verify the inclusion proof (i.e., they perform full validation).
    pub fn validate_block_no_acc(
        &self,
        block: &Block,
        height: u32,
        inputs: HashMap<OutPoint, UtxoData>,
        verify_script: bool,
    ) -> Result<(), BlockchainError> {
        if !block.check_merkle_root() {
            return Err(BlockValidationErrors::BadMerkleRoot)?;
        }

        let bip34_height = self.parameters.params.bip34_height;
        // If bip34 is active, check that the encoded block height is correct
        if height >= bip34_height && self.get_bip34_height(block) != Some(height) {
            return Err(BlockValidationErrors::BadBip34)?;
        }

        if !block.check_witness_commitment() {
            return Err(BlockValidationErrors::BadWitnessCommitment)?;
        }

        if block.weight().to_wu() > 4_000_000 {
            return Err(BlockValidationErrors::BlockTooBig)?;
        }

        // Validate block transactions
        let subsidy = self.get_subsidy(height);

        #[cfg(feature = "bitcoinconsensus")]
        let flags = self
            .parameters
            .get_validation_flags(height, block.block_hash());
        #[cfg(not(feature = "bitcoinconsensus"))]
        let flags = 0;

        Consensus::verify_block_transactions(
            height,
            inputs,
            &block.txdata,
            subsidy,
            verify_script,
            flags,
        )?;
        Ok(())
    }

    /// Verify if all transactions in a block are valid. Here we check the following:
    /// - The block must contain at least one transaction, and this transaction must be coinbase
    /// - The first transaction in the block must be coinbase
    /// - The coinbase transaction must have the correct value (subsidy + fees)
    /// - The block must not create more coins than allowed
    /// - All transactions must be valid, as verified by [`Consensus::verify_transaction`]
    #[allow(unused)]
    pub fn verify_block_transactions(
        height: u32,
        mut utxos: HashMap<OutPoint, UtxoData>,
        transactions: &[Transaction],
        subsidy: u64,
        verify_script: bool,
        flags: c_uint,
    ) -> Result<(), BlockchainError> {
        // Blocks must contain at least one transaction (i.e., the coinbase)
        if transactions.is_empty() {
            return Err(BlockValidationErrors::EmptyBlock)?;
        }

        // Total block fees that the miner can claim in the coinbase
        let mut fee = 0;

        for (n, transaction) in transactions.iter().enumerate() {
            if n == 0 {
                if !transaction.is_coinbase() {
                    return Err(BlockValidationErrors::FirstTxIsNotCoinbase)?;
                }
                Self::verify_coinbase(transaction)?;
                // Skip next checks: coinbase input is exempt, coinbase reward checked later
                continue;
            }

            // Actually verify the transaction
            let (in_value, out_value) =
                Self::verify_transaction(transaction, &mut utxos, height, verify_script, flags)?;

            // Fee is the difference between inputs and outputs
            fee += in_value - out_value;
        }

        // Check coinbase output values to ensure the miner isn't producing excess coins
        let allowed_reward = fee + subsidy;
        let coinbase_total: u64 = transactions[0]
            .output
            .iter()
            .map(|out| out.value.to_sat())
            .sum();

        if coinbase_total > allowed_reward {
            return Err(BlockValidationErrors::BadCoinbaseOutValue)?;
        }

        Ok(())
    }
}
