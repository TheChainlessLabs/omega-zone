#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]
#![allow(clippy::too_many_arguments)]

use eyre as _;

#[cfg(feature = "cli")]
pub mod cli;
pub mod engine;
pub mod node;
pub mod rpc;

/// ABI bindings used by the node-only RPC implementation.
pub mod abi {
    pub use tempo_zone_contracts::*;

    alloy_sol_types::sol! {
        #[sol(rpc)]
        contract DarkpoolReader {
            function bestBid(address base) external view returns (uint128 price, uint128 quantity);
            function bestAsk(address base) external view returns (uint128 price, uint128 quantity);
            #[allow(non_snake_case)]
            function MIN_ORDER_AMOUNT() external pure returns (uint128);
        }
    }
}

pub use zone_sequencer::{midpoint, proof};

pub use engine::ZoneEngine;
pub use node::{ZoneExecutorBuilder, ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig};
