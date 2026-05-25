//! ABI bindings — re-exported from [`zone_primitives::abi`].

pub use zone_primitives::abi::*;

use alloy_sol_types::sol;

sol! {
    /// Subset of the darkpool orderbook precompile used by the private zone
    /// RPC. Reads aggregate book metadata only — does not expose order
    /// owners, individual order IDs, or anything beyond the best-resting
    /// price/quantity at each side.
    ///
    /// `bestBid(base)` / `bestAsk(base)` implicitly resolve the quote via the
    /// base token's own `quoteToken()`. The RPC layer must therefore validate
    /// the requested pair against the canonical alpha market before reading,
    /// otherwise the response would be the wrong book under the wrong label.
    #[sol(rpc)]
    contract DarkpoolReader {
        function bestBid(address base) external view returns (uint128 price, uint128 quantity);
        function bestAsk(address base) external view returns (uint128 price, uint128 quantity);
        #[allow(non_snake_case)]
        function MIN_ORDER_AMOUNT() external pure returns (uint128);
    }
}
