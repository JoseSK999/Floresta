//! A collection of functions that implement the consensus rules for the Bitcoin Network.
//! This module contains functions that are used to verify blocks and transactions, and doesn't
//! assume anything about the chainstate, so it can be used in any context.
//! We use this to avoid code reuse among the different implementations of the chainstate.
extern crate alloc;

pub mod block_validation;
pub mod tx_validation;

use bitcoin::block::Header as BlockHeader;
use bitcoin::hashes::sha256;
use bitcoin::Block;
use bitcoin::CompactTarget;
use bitcoin::Target;
use floresta_common::prelude::*;
use rustreexo::accumulator::proof::Proof;
use rustreexo::accumulator::stump::Stump;

use super::chainparams::ChainParams;
use super::error::BlockValidationErrors;
use super::error::BlockchainError;
use super::udata;

/// The value of a single coin in satoshis.
pub const COIN_VALUE: u64 = 100_000_000;

/// The version tag to be prepended to the leafhash. It's just the sha512 hash of the string
/// `UtreexoV1` represented as a vector of [u8] ([85 116 114 101 101 120 111 86 49]).
/// The same tag is "5574726565786f5631" as a hex string.
pub const UTREEXO_TAG_V1: [u8; 64] = [
    0x5b, 0x83, 0x2d, 0xb8, 0xca, 0x26, 0xc2, 0x5b, 0xe1, 0xc5, 0x42, 0xd6, 0xcc, 0xed, 0xdd, 0xa8,
    0xc1, 0x45, 0x61, 0x5c, 0xff, 0x5c, 0x35, 0x72, 0x7f, 0xb3, 0x46, 0x26, 0x10, 0x80, 0x7e, 0x20,
    0xae, 0x53, 0x4d, 0xc3, 0xf6, 0x42, 0x99, 0x19, 0x99, 0x31, 0x77, 0x2e, 0x03, 0x78, 0x7d, 0x18,
    0x15, 0x6e, 0xb3, 0x15, 0x1e, 0x0e, 0xd1, 0xb3, 0x09, 0x8b, 0xdc, 0x84, 0x45, 0x86, 0x18, 0x85,
];

/// The unspendable UTXO on block 91_722 that exists because of the historical
/// [BIP30 violation](https://bips.dev/30/). For Utreexo, this UTXO is not overwritten
/// as we commit the block hash in the leafhash. But since non-Utreexo nodes consider
/// this as unspendable as it's already been overwritten, we also need to make it not spendable.
///
/// Encoded in hex string is 84b3af0783b410b4564c5d1f361868559f7cf77cfc65ce2be951210357022fe3.
pub const UNSPENDABLE_BIP30_UTXO_91722: [u8; 32] = [
    0x84, 0xb3, 0xaf, 0x07, 0x83, 0xb4, 0x10, 0xb4, 0x56, 0x4c, 0x5d, 0x1f, 0x36, 0x18, 0x68, 0x55,
    0x9f, 0x7c, 0xf7, 0x7c, 0xfc, 0x65, 0xce, 0x2b, 0xe9, 0x51, 0x21, 0x03, 0x57, 0x02, 0x2f, 0xe3,
];

/// The unspendable UTXO on block 91_812 that exists because of the historical
/// [BIP30 violation](https://bips.dev/30/). For Utreexo, this UTXO is not overwritten
/// as we commit the block hash in the leafhash. But since non-Utreexo nodes consider
/// this as unspendable as it's already been overwritten, we also need to make it not spendable.
///
/// Encoded in hex string is bc6b4bf7cebbd33a18d6b0fe1f8ecc7aa5403083c39ee343b985d51fd0295ad8.
pub const UNSPENDABLE_BIP30_UTXO_91812: [u8; 32] = [
    0xbc, 0x6b, 0x4b, 0xf7, 0xce, 0xbb, 0xd3, 0x3a, 0x18, 0xd6, 0xb0, 0xfe, 0x1f, 0x8e, 0xcc, 0x7a,
    0xa5, 0x40, 0x30, 0x83, 0xc3, 0x9e, 0xe3, 0x43, 0xb9, 0x85, 0xd5, 0x1f, 0xd0, 0x29, 0x5a, 0xd8,
];
/// This struct contains all the information and methods needed to validate a block,
/// it is used by the [ChainState](crate::ChainState) to validate blocks and transactions.
#[derive(Debug, Clone)]
pub struct Consensus {
    /// The parameters of the chain we are validating, it is usually hardcoded
    /// constants. See [ChainParams] for more information.
    pub parameters: ChainParams,
}

impl Consensus {
    /// Checks if a testnet4 block is compliant with the anti-timewarp rules of BIP94.
    ///
    /// a. The block's nTime field MUST be greater than or equal to the nTime
    /// field of the immediately prior block minus 600 seconds
    pub fn check_bip94_time(
        block: &BlockHeader,
        prev_block: &BlockHeader,
    ) -> Result<(), BlockValidationErrors> {
        if block.time < (prev_block.time - 600) {
            return Err(BlockValidationErrors::BIP94TimeWarp);
        }

        Ok(())
    }

    /// Calculates the next target for the proof of work algorithm, given the
    /// first and last block headers inside a difficulty adjustment period.
    pub fn calc_next_work_required(
        last_block: &BlockHeader,
        first_block: &BlockHeader,
        params: ChainParams,
    ) -> Target {
        let actual_timespan = last_block.time - first_block.time;
        // from bip 94:
        //  a. The base difficulty value MUST be taken from the first block of the previous
        //     difficulty period
        //
        //  b. NOT from the last block as in previous implementations
        let bits = match params.enforce_bip94 {
            true => first_block.bits,
            false => last_block.bits,
        };

        CompactTarget::from_next_work_required(bits, actual_timespan as u64, params).into()
    }

    /// Updates our accumulator with the new block. This is done by calculating the new
    /// root hash of the accumulator, and then verifying the proof of inclusion of the
    /// deleted nodes. If the proof is valid, we return the new accumulator. Otherwise,
    /// we return an error.
    /// This function is pure, it doesn't modify the accumulator, but returns a new one.
    pub fn update_acc(
        acc: &Stump,
        block: &Block,
        height: u32,
        proof: Proof,
        del_hashes: Vec<sha256::Hash>,
    ) -> Result<Stump, BlockchainError> {
        let block_hash = block.block_hash();

        // Check if there is a spend of an unspendable UTXO (BIP30)
        if Self::contains_unspendable_utxo(&del_hashes) {
            return Err(BlockValidationErrors::UnspendableUTXO)?;
        }

        // Convert to BitcoinNodeHash, from rustreexo
        let del_hashes: Vec<_> = del_hashes.into_iter().map(Into::into).collect();

        let adds = udata::proof_util::get_block_adds(block, height, block_hash);

        // Update the accumulator
        let acc = acc.modify(&adds, &del_hashes, &proof)?.0;
        Ok(acc)
    }

    fn contains_unspendable_utxo(del_hashes: &[sha256::Hash]) -> bool {
        del_hashes.iter().any(|hash| {
            let bytes = hash.as_ref();
            bytes == UNSPENDABLE_BIP30_UTXO_91722 || bytes == UNSPENDABLE_BIP30_UTXO_91812
        })
    }
}
