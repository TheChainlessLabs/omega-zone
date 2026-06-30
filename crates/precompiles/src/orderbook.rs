//! Darkpool orderbook precompile.
//!
//! A privacy-preserving central limit order book where orders match against
//! resting liquidity using price-time priority. Key properties:
//!
//! - **Owner-scoped order history** — live order reads are maker-gated, and
//!   full history is reconstructed through private RPC.
//! - **Owner-scoped reads** — only the maker can view their own orders.
//! - **Price-time priority** — same-side orders sorted by best price first,
//!   then by insertion time (lower order ID = earlier).
//! - **Internal balances** — users deposit tokens once and trade gas-efficiently
//!   without repeated TIP-20 transfers. Withdrawals push tokens back to the
//!   user's TIP-20 wallet.
//!
//! `bestBid` / `bestAsk` are exposed as temporary aggregate alpha-readiness
//! helpers. Strict darkpool/privacy claims should not rely on public
//! top-of-book visibility.
//!
//! Matching happens eagerly on placement: a new bid matches against the best
//! ask first (and vice versa), with price improvement refunded to the taker
//! when their limit price is better than the resting order.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolValue};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile,
    storage::{ContractStorage, Handler, Mapping, StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{ITIP20, TIP20Token},
};
use tempo_precompiles_macros::{Storable, contract};

/// Address for the darkpool orderbook precompile.
pub const DARKPOOL_ADDRESS: Address = Address::new([
    0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

/// Minimum order quantity to prevent dust spam.
pub const MIN_ORDER_AMOUNT: u128 = 100;

alloy_sol_types::sol! {
    #[derive(Debug)]
    event OrderSubmitted(
        uint128 indexed orderId,
        address indexed maker,
        address base,
        address quote,
        uint128 amount,
        uint128 price,
        bool isBid
    );

    #[derive(Debug)]
    event OrderPlaced(
        uint128 indexed orderId,
        address indexed maker,
        address base,
        address quote,
        uint128 amount,
        uint128 price,
        bool isBid
    );

    #[derive(Debug)]
    event OrderFilled(
        uint128 indexed orderId,
        address indexed maker,
        address indexed taker,
        uint128 amountFilled,
        uint128 price
    );

    #[derive(Debug)]
    event OrderMatched(
        uint128 indexed makerOrderId,
        uint128 indexed takerOrderId,
        address indexed maker,
        address taker,
        uint128 amountFilled,
        uint128 price
    );

    #[derive(Debug)]
    event OrderCancelled(
        uint128 indexed orderId,
        address indexed maker
    );

    #[derive(Debug)]
    struct OrderView {
        uint128 orderId;
        address maker;
        address base;
        address quote;
        bool isBid;
        uint128 price;
        uint128 quantity;
    }

    function place(address base, uint128 amount, uint128 price, bool isBid)
        external returns (uint128 orderId);
    function cancel(uint128 orderId) external;
    function getOrder(uint128 orderId) external view returns (OrderView memory);
    function deposit(address token, uint128 amount) external;
    function withdraw(address token, uint128 amount) external;
    function balanceOf(address user, address token) external view returns (uint128);
    function availableBalanceOf(address user, address token) external view returns (uint128);
    function pairKey(address base, address quote) external pure returns (bytes32);
    function createPair(address base) external returns (bytes32);
    function bestBid(address base) external view returns (uint128 price, uint128 quantity);
    function bestAsk(address base) external view returns (uint128 price, uint128 quantity);
    function MIN_ORDER_AMOUNT() external pure returns (uint128);
    function marketBuy(address base, uint128 amount, uint128 maxQuoteIn)
        external returns (uint128 quoteSpent);
    function marketSell(address base, uint128 amount, uint128 minQuoteOut)
        external returns (uint128 quoteReceived);

    error OrderDoesNotExist();
    error Unauthorized();
    error InsufficientLiquidity();
    error InvalidToken();
    error PairAlreadyExists();
    error PriceOutOfRange();
    error InsufficientBalance();
    error AmountBelowMinimum();
    error CannotMatchSelf();
}

/// One side limit order stored in the doubly-linked price-time queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Storable)]
pub struct Order {
    pub order_id: u128,
    pub maker: Address,
    pub book_key: B256,
    pub is_bid: bool,
    pub price: u128,
    pub quantity: u128,
    pub next: u128,
    pub prev: u128,
}

/// Per-pair orderbook metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Storable, Default)]
pub struct OrderBookMeta {
    pub base: Address,
    pub quote: Address,
    pub best_bid_id: u128,
    pub best_ask_id: u128,
}

/// Unwrap a storage operation or propagate the error via [`StorageCtx::error_result`].
macro_rules! try_storage {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return $crate::storage::StorageCtx.error_result(e),
        }
    };
}

/// Return a Solidity revert with the given custom error.
macro_rules! revert {
    ($err:expr) => {
        return Ok($crate::storage::StorageCtx.revert_output(SolError::abi_encode(&$err).into()))
    };
}

/// Darkpool orderbook precompile, registered at [`DARKPOOL_ADDRESS`].
#[contract(addr = DARKPOOL_ADDRESS)]
pub struct DarkpoolOrderbook {
    orders: Mapping<u128, Order>,
    books: Mapping<B256, OrderBookMeta>,
    balances: Mapping<Address, Mapping<Address, u128>>,
    next_order_id: u128,
    book_keys: Vec<B256>,
}

impl DarkpoolOrderbook {
    /// Sets the contract bytecode so the account is non-empty.
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
    }

    fn next_order_id_val(&self) -> tempo_precompiles::Result<u128> {
        Ok(self.next_order_id.read()?.max(1))
    }

    fn increment_next_order_id(&mut self) -> tempo_precompiles::Result<()> {
        let next = self.next_order_id_val()?;
        self.next_order_id.write(next + 1)
    }

    // ── internal balances ─────────────────────────────────────────────────

    pub fn balance_of(&self, user: Address, token: Address) -> tempo_precompiles::Result<u128> {
        self.balances[user][token].read()
    }

    pub fn available_balance_of(
        &self,
        user: Address,
        token: Address,
    ) -> tempo_precompiles::Result<u128> {
        let balance = self.balance_of(user, token)?;
        let reserved = self.reserved_balance_of(user, token)?;
        Ok(balance.saturating_sub(reserved))
    }

    fn reserved_balance_of(
        &self,
        user: Address,
        token: Address,
    ) -> tempo_precompiles::Result<u128> {
        let mut reserved = 0u128;
        let len = self.book_keys.len()?;

        for i in 0..len {
            let book_key = self.book_keys[i].read()?;
            let book = self.books[book_key].read()?;
            reserved = self.accumulate_reserved_side(
                book.best_bid_id,
                user,
                token,
                true,
                book.quote,
                reserved,
            )?;
            reserved = self.accumulate_reserved_side(
                book.best_ask_id,
                user,
                token,
                false,
                book.base,
                reserved,
            )?;
        }

        Ok(reserved)
    }

    fn accumulate_reserved_side(
        &self,
        mut order_id: u128,
        user: Address,
        token: Address,
        is_bid_side: bool,
        reserved_token: Address,
        mut reserved: u128,
    ) -> tempo_precompiles::Result<u128> {
        if token != reserved_token {
            return Ok(reserved);
        }

        while order_id != 0 {
            let order = self.orders[order_id].read()?;
            if order.maker == user {
                let amount = if is_bid_side {
                    order.quantity.saturating_mul(order.price)
                } else {
                    order.quantity
                };
                reserved = reserved.saturating_add(amount);
            }
            order_id = order.next;
        }

        Ok(reserved)
    }

    fn set_balance(
        &mut self,
        user: Address,
        token: Address,
        amount: u128,
    ) -> tempo_precompiles::Result<()> {
        self.balances[user][token].write(amount)
    }

    fn increment_balance(
        &mut self,
        user: Address,
        token: Address,
        amount: u128,
    ) -> tempo_precompiles::Result<()> {
        let cur = self.balance_of(user, token)?;
        self.set_balance(user, token, cur.saturating_add(amount))
    }

    fn decrement_balance(
        &mut self,
        user: Address,
        token: Address,
        amount: u128,
    ) -> tempo_precompiles::Result<()> {
        let cur = self.balance_of(user, token)?;
        self.set_balance(user, token, cur.saturating_sub(amount))
    }

    fn ensure_internal_balance(
        &mut self,
        user: Address,
        token: Address,
        required: u128,
    ) -> tempo_precompiles::Result<()> {
        let available = self.available_balance_of(user, token)?;
        if available >= required {
            return Ok(());
        }

        let missing = required - available;
        let mut tip20 = TIP20Token::from_address(token)?;
        tip20.system_transfer_from(user, self.address, U256::from(missing))?;
        self.increment_balance(user, token, missing)
    }

    // ── pair management ───────────────────────────────────────────────────

    fn validate_or_create_pair(&mut self, base: Address) -> tempo_precompiles::Result<B256> {
        let tip20 = TIP20Token::from_address(base)?;
        let quote = tip20.quote_token()?;
        let book_key = compute_book_key(base, quote);
        let book = self.books[book_key].read()?;
        if book.base == Address::ZERO {
            self.books[book_key].write(OrderBookMeta {
                base,
                quote,
                best_bid_id: 0,
                best_ask_id: 0,
            })?;
            self.book_keys.push(book_key)?;
        }
        Ok(book_key)
    }

    pub fn create_pair(&mut self, base: Address) -> PrecompileResult {
        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let book_key = compute_book_key(base, quote);
        let book = try_storage!(self.books[book_key].read());
        if book.base != Address::ZERO {
            revert!(PairAlreadyExists {});
        }
        try_storage!(self.books[book_key].write(OrderBookMeta {
            base,
            quote,
            best_bid_id: 0,
            best_ask_id: 0,
        }));
        try_storage!(self.book_keys.push(book_key));
        Ok(StorageCtx.success_output(book_key.abi_encode().into()))
    }

    // ── deposit / withdraw ────────────────────────────────────────────────

    pub fn deposit(&mut self, sender: Address, token: Address, amount: u128) -> PrecompileResult {
        let mut tip20 = try_storage!(TIP20Token::from_address(token));
        try_storage!(tip20.system_transfer_from(sender, self.address, U256::from(amount),));
        try_storage!(self.increment_balance(sender, token, amount));
        Ok(StorageCtx.success_output(Bytes::new()))
    }

    pub fn withdraw(&mut self, sender: Address, token: Address, amount: u128) -> PrecompileResult {
        let available = try_storage!(self.available_balance_of(sender, token));
        if available < amount {
            revert!(InsufficientBalance {});
        }
        try_storage!(self.decrement_balance(sender, token, amount));
        let mut tip20 = try_storage!(TIP20Token::from_address(token));
        try_storage!(tip20.transfer(
            self.address,
            ITIP20::transferCall {
                to: sender,
                amount: U256::from(amount)
            },
        ));
        Ok(StorageCtx.success_output(Bytes::new()))
    }

    // ── order placement ───────────────────────────────────────────────────

    pub fn place(
        &mut self,
        sender: Address,
        base: Address,
        amount: u128,
        price: u128,
        is_bid: bool,
    ) -> PrecompileResult {
        if amount < MIN_ORDER_AMOUNT {
            revert!(AmountBelowMinimum {});
        }
        if price == 0 {
            revert!(PriceOutOfRange {});
        }

        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let book_key = try_storage!(self.validate_or_create_pair(base));

        if is_bid {
            let escrow = match amount.checked_mul(price) {
                Some(v) => v,
                None => revert!(PriceOutOfRange {}),
            };
            try_storage!(self.ensure_internal_balance(sender, quote, escrow));
        } else {
            try_storage!(self.ensure_internal_balance(sender, base, amount));
        }

        let order_id = try_storage!(self.next_order_id_val());
        try_storage!(self.increment_next_order_id());
        try_storage!(self.emit_event(OrderSubmitted {
            orderId: order_id,
            maker: sender,
            base,
            quote,
            amount,
            price,
            isBid: is_bid,
        }));

        let mut remaining = amount;
        if is_bid {
            try_storage!(self.cross_asks(
                book_key,
                order_id,
                sender,
                price,
                &mut remaining,
                base,
                quote,
            ));
        } else {
            try_storage!(self.cross_bids(
                book_key,
                order_id,
                sender,
                price,
                &mut remaining,
                base,
                quote,
            ));
        }

        if remaining > 0 {
            let order = Order {
                order_id,
                maker: sender,
                book_key,
                is_bid,
                price,
                quantity: remaining,
                next: 0,
                prev: 0,
            };
            try_storage!(self.insert_order(book_key, order));
            try_storage!(self.emit_event(OrderPlaced {
                orderId: order_id,
                maker: sender,
                base,
                quote,
                amount: remaining,
                price,
                isBid: is_bid,
            }));
            Ok(StorageCtx.success_output(U256::from(order_id).abi_encode().into()))
        } else {
            Ok(StorageCtx.success_output(U256::from(order_id).abi_encode().into()))
        }
    }

    // ── matching ──────────────────────────────────────────────────────────

    fn cross_asks(
        &mut self,
        book_key: B256,
        taker_order_id: u128,
        taker: Address,
        bid_price: u128,
        remaining: &mut u128,
        base: Address,
        quote: Address,
    ) -> tempo_precompiles::Result<()> {
        loop {
            if *remaining == 0 {
                break;
            }
            let book = self.books[book_key].read()?;
            if book.best_ask_id == 0 {
                break;
            }
            let ask = self.orders[book.best_ask_id].read()?;
            if ask.price > bid_price {
                break;
            }
            let fill = (*remaining).min(ask.quantity);
            let quote_amount = U256::from(fill)
                .checked_mul(U256::from(ask.price))
                .and_then(|v| v.try_into().ok())
                .unwrap_or(0u128);

            // Taker (bidder) pays quote at maker's ask price.
            self.decrement_balance(taker, quote, quote_amount)?;
            // Taker receives base.
            self.increment_balance(taker, base, fill)?;
            // Maker (ask owner) releases base escrow.
            self.decrement_balance(ask.maker, base, fill)?;
            // Maker receives quote.
            self.increment_balance(ask.maker, quote, quote_amount)?;

            self.emit_event(OrderFilled {
                orderId: ask.order_id,
                maker: ask.maker,
                taker,
                amountFilled: fill,
                price: ask.price,
            })?;
            self.emit_event(OrderMatched {
                makerOrderId: ask.order_id,
                takerOrderId: taker_order_id,
                maker: ask.maker,
                taker,
                amountFilled: fill,
                price: ask.price,
            })?;

            *remaining -= fill;

            if fill == ask.quantity {
                self.remove_order(book_key, &ask)?;
            } else {
                self.orders[book.best_ask_id]
                    .quantity
                    .write(ask.quantity - fill)?;
            }
        }
        Ok(())
    }

    fn cross_bids(
        &mut self,
        book_key: B256,
        taker_order_id: u128,
        taker: Address,
        ask_price: u128,
        remaining: &mut u128,
        base: Address,
        quote: Address,
    ) -> tempo_precompiles::Result<()> {
        loop {
            if *remaining == 0 {
                break;
            }
            let book = self.books[book_key].read()?;
            if book.best_bid_id == 0 {
                break;
            }
            let bid = self.orders[book.best_bid_id].read()?;
            if bid.price < ask_price {
                break;
            }
            let fill = (*remaining).min(bid.quantity);
            let quote_amount = U256::from(fill)
                .checked_mul(U256::from(bid.price))
                .and_then(|v| v.try_into().ok())
                .unwrap_or(0u128);

            // Taker (asker) gives base.
            self.decrement_balance(taker, base, fill)?;
            // Taker receives quote at maker's bid price.
            self.increment_balance(taker, quote, quote_amount)?;
            // Maker (bid owner) pays quote.
            self.decrement_balance(bid.maker, quote, quote_amount)?;
            // Maker receives base.
            self.increment_balance(bid.maker, base, fill)?;

            self.emit_event(OrderFilled {
                orderId: bid.order_id,
                maker: bid.maker,
                taker,
                amountFilled: fill,
                price: bid.price,
            })?;
            self.emit_event(OrderMatched {
                makerOrderId: bid.order_id,
                takerOrderId: taker_order_id,
                maker: bid.maker,
                taker,
                amountFilled: fill,
                price: bid.price,
            })?;

            *remaining -= fill;

            if fill == bid.quantity {
                self.remove_order(book_key, &bid)?;
            } else {
                self.orders[book.best_bid_id]
                    .quantity
                    .write(bid.quantity - fill)?;
            }
        }
        Ok(())
    }

    // ── linked-list ───────────────────────────────────────────────────────

    fn insert_order(&mut self, book_key: B256, mut order: Order) -> tempo_precompiles::Result<()> {
        let book = self.books[book_key].read()?;
        let head_id = if order.is_bid {
            book.best_bid_id
        } else {
            book.best_ask_id
        };

        if head_id == 0 {
            self.orders[order.order_id].write(order)?;
            let mut meta = self.books[book_key].read()?;
            if order.is_bid {
                meta.best_bid_id = order.order_id;
            } else {
                meta.best_ask_id = order.order_id;
            }
            self.books[book_key].write(meta)?;
            return Ok(());
        }

        let mut current_id = head_id;
        let mut current = self.orders[current_id].read()?;

        // Walk to find insertion point.
        let (found, insert_before) = loop {
            let better_price = if order.is_bid {
                current.price < order.price
            } else {
                current.price > order.price
            };
            if better_price {
                break (true, current_id);
            }
            if current.next == 0 {
                break (false, current_id);
            }
            current_id = current.next;
            current = self.orders[current_id].read()?;
        };

        if found {
            // Insert before the found position.
            order.prev = current.prev;
            order.next = insert_before;
            self.orders[order.order_id].write(order)?;

            if current.prev == 0 {
                let mut meta = self.books[book_key].read()?;
                if order.is_bid {
                    meta.best_bid_id = order.order_id;
                } else {
                    meta.best_ask_id = order.order_id;
                }
                self.books[book_key].write(meta)?;
            } else {
                let mut prev = self.orders[current.prev].read()?;
                prev.next = order.order_id;
                self.orders[current.prev].write(prev)?;
            }
            let mut cur = self.orders[insert_before].read()?;
            cur.prev = order.order_id;
            self.orders[insert_before].write(cur)?;
        } else {
            // Append after the last position.
            order.prev = insert_before;
            order.next = 0;
            self.orders[order.order_id].write(order)?;

            let mut last = self.orders[insert_before].read()?;
            last.next = order.order_id;
            self.orders[insert_before].write(last)?;
        }

        Ok(())
    }

    fn remove_order(&mut self, book_key: B256, order: &Order) -> tempo_precompiles::Result<()> {
        if order.prev != 0 {
            let mut prev = self.orders[order.prev].read()?;
            prev.next = order.next;
            self.orders[order.prev].write(prev)?;
        } else {
            let mut meta = self.books[book_key].read()?;
            if order.is_bid {
                meta.best_bid_id = order.next;
            } else {
                meta.best_ask_id = order.next;
            }
            self.books[book_key].write(meta)?;
        }

        if order.next != 0 {
            let mut next = self.orders[order.next].read()?;
            next.prev = order.prev;
            self.orders[order.next].write(next)?;
        }

        self.orders[order.order_id].delete()
    }

    // ── cancel ────────────────────────────────────────────────────────────

    pub fn cancel(&mut self, sender: Address, order_id: u128) -> PrecompileResult {
        let order = try_storage!(self.orders[order_id].read());
        if order.maker == Address::ZERO || order.quantity == 0 {
            revert!(OrderDoesNotExist {});
        }
        if order.maker != sender {
            revert!(Unauthorized {});
        }

        try_storage!(self.remove_order(order.book_key, &order));

        try_storage!(self.emit_event(OrderCancelled {
            orderId: order_id,
            maker: sender,
        }));
        Ok(StorageCtx.success_output(Bytes::new()))
    }

    // ── market orders ────────────────────────────────────────────────────

    /// Market buy: purchase `amount` base at best available ask prices.
    /// Pulls `max_quote_in` from sender; unused quote stays in internal balance.
    /// Reverts if the book cannot fill the full `amount` or if it would cost
    /// more than `max_quote_in`.  Returns `quote_spent`.
    pub fn market_buy(
        &mut self,
        sender: Address,
        base: Address,
        amount: u128,
        max_quote_in: u128,
    ) -> PrecompileResult {
        if amount < MIN_ORDER_AMOUNT {
            revert!(AmountBelowMinimum {});
        }

        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let _book_key = try_storage!(self.validate_or_create_pair(base));

        try_storage!(self.ensure_internal_balance(sender, quote, max_quote_in));

        let (filled, spent) =
            try_storage!(self.fill_asks_market(sender, base, quote, amount, max_quote_in));

        if filled < amount {
            revert!(InsufficientLiquidity {});
        }

        Ok(StorageCtx.success_output(U256::from(spent).abi_encode().into()))
    }

    /// Market sell: sell `amount` base at best available bid prices.
    /// Pulls `amount` base tokens from sender; received quote stays in
    /// internal balance.  Reverts if the book cannot fill the full `amount`
    /// or if the received quote is below `min_quote_out`.
    /// Returns `quote_received`.
    pub fn market_sell(
        &mut self,
        sender: Address,
        base: Address,
        amount: u128,
        min_quote_out: u128,
    ) -> PrecompileResult {
        if amount < MIN_ORDER_AMOUNT {
            revert!(AmountBelowMinimum {});
        }

        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let _book_key = try_storage!(self.validate_or_create_pair(base));

        try_storage!(self.ensure_internal_balance(sender, base, amount));

        let (filled, received) = try_storage!(self.fill_bids_market(sender, base, quote, amount));

        if filled < amount {
            revert!(InsufficientLiquidity {});
        }
        if received < min_quote_out {
            revert!(InsufficientBalance {});
        }

        Ok(StorageCtx.success_output(U256::from(received).abi_encode().into()))
    }

    /// Cross asks without a price limit (market buy).  Returns `(filled, quote_spent)`.
    fn fill_asks_market(
        &mut self,
        taker: Address,
        base: Address,
        quote: Address,
        mut remaining: u128,
        max_quote: u128,
    ) -> tempo_precompiles::Result<(u128, u128)> {
        let book_key = compute_book_key(base, quote);
        let mut total_spent: u128 = 0;
        let mut total_filled: u128 = 0;

        loop {
            if remaining == 0 {
                break;
            }
            let book = self.books[book_key].read()?;
            if book.best_ask_id == 0 {
                break;
            }
            let ask = self.orders[book.best_ask_id].read()?;
            if ask.maker == taker {
                break;
            }

            let fill = remaining.min(ask.quantity);
            let quote_amount = U256::from(fill)
                .checked_mul(U256::from(ask.price))
                .and_then(|v| v.try_into().ok())
                .unwrap_or(0u128);

            if total_spent + quote_amount > max_quote {
                break;
            }

            self.decrement_balance(taker, quote, quote_amount)?;
            self.increment_balance(taker, base, fill)?;
            self.decrement_balance(ask.maker, base, fill)?;
            self.increment_balance(ask.maker, quote, quote_amount)?;

            self.emit_event(OrderFilled {
                orderId: ask.order_id,
                maker: ask.maker,
                taker,
                amountFilled: fill,
                price: ask.price,
            })?;

            total_spent += quote_amount;
            remaining -= fill;
            total_filled += fill;

            if fill == ask.quantity {
                self.remove_order(book_key, &ask)?;
            } else {
                self.orders[book.best_ask_id]
                    .quantity
                    .write(ask.quantity - fill)?;
            }
        }

        Ok((total_filled, total_spent))
    }

    /// Cross bids without a price limit (market sell).  Returns `(filled, quote_received)`.
    fn fill_bids_market(
        &mut self,
        taker: Address,
        base: Address,
        quote: Address,
        mut remaining: u128,
    ) -> tempo_precompiles::Result<(u128, u128)> {
        let book_key = compute_book_key(base, quote);
        let mut total_received: u128 = 0;
        let mut total_filled: u128 = 0;

        loop {
            if remaining == 0 {
                break;
            }
            let book = self.books[book_key].read()?;
            if book.best_bid_id == 0 {
                break;
            }
            let bid = self.orders[book.best_bid_id].read()?;
            if bid.maker == taker {
                break;
            }

            let fill = remaining.min(bid.quantity);
            let quote_amount = U256::from(fill)
                .checked_mul(U256::from(bid.price))
                .and_then(|v| v.try_into().ok())
                .unwrap_or(0u128);

            self.decrement_balance(taker, base, fill)?;
            self.increment_balance(taker, quote, quote_amount)?;
            self.decrement_balance(bid.maker, quote, quote_amount)?;
            self.increment_balance(bid.maker, base, fill)?;

            self.emit_event(OrderFilled {
                orderId: bid.order_id,
                maker: bid.maker,
                taker,
                amountFilled: fill,
                price: bid.price,
            })?;

            total_received += quote_amount;
            remaining -= fill;
            total_filled += fill;

            if fill == bid.quantity {
                self.remove_order(book_key, &bid)?;
            } else {
                self.orders[book.best_bid_id]
                    .quantity
                    .write(bid.quantity - fill)?;
            }
        }

        Ok((total_filled, total_received))
    }

    // ── views ─────────────────────────────────────────────────────────────

    pub fn get_order(&self, caller: Address, order_id: u128) -> PrecompileResult {
        let order = try_storage!(self.orders[order_id].read());
        if order.maker == Address::ZERO {
            revert!(OrderDoesNotExist {});
        }
        if order.maker != caller {
            revert!(Unauthorized {});
        }
        let book = try_storage!(self.books[order.book_key].read());
        let view = OrderView {
            orderId: order.order_id,
            maker: order.maker,
            base: book.base,
            quote: book.quote,
            isBid: order.is_bid,
            price: order.price,
            quantity: order.quantity,
        };
        Ok(StorageCtx.success_output(view.abi_encode().into()))
    }

    pub fn best_bid(&self, base: Address) -> PrecompileResult {
        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let book_key = compute_book_key(base, quote);
        let book = try_storage!(self.books[book_key].read());
        if book.best_bid_id == 0 {
            return Ok(StorageCtx.success_output((U256::ZERO, U256::ZERO).abi_encode().into()));
        }
        let order = try_storage!(self.orders[book.best_bid_id].read());
        Ok(StorageCtx.success_output(
            (U256::from(order.price), U256::from(order.quantity))
                .abi_encode()
                .into(),
        ))
    }

    pub fn best_ask(&self, base: Address) -> PrecompileResult {
        let tip20 = try_storage!(TIP20Token::from_address(base));
        let quote = try_storage!(tip20.quote_token());
        let book_key = compute_book_key(base, quote);
        let book = try_storage!(self.books[book_key].read());
        if book.best_ask_id == 0 {
            return Ok(StorageCtx.success_output((U256::ZERO, U256::ZERO).abi_encode().into()));
        }
        let order = try_storage!(self.orders[book.best_ask_id].read());
        Ok(StorageCtx.success_output(
            (U256::from(order.price), U256::from(order.quantity))
                .abi_encode()
                .into(),
        ))
    }

    // ── registration ──────────────────────────────────────────────────────

    pub fn create(
        cfg: &revm::context::CfgEnv<TempoHardfork>,
    ) -> alloy_evm::precompiles::DynPrecompile {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();
        alloy_evm::precompiles::DynPrecompile::new_stateful(
            PrecompileId::Custom("DarkpoolOrderbook".into()),
            move |input| {
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
                    let mut orderbook = Self::new();
                    if !input.is_static {
                        let initialized = try_storage!(orderbook.is_initialized());
                        if !initialized {
                            try_storage!(orderbook.initialize());
                        }
                    }
                    orderbook.call(input.data, input.caller)
                })
            },
        )
    }
}

impl TempoPrecompile for DarkpoolOrderbook {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        let Some((selector, _args)) = calldata.split_at_checked(4) else {
            return Ok(StorageCtx.revert_output(Bytes::new()));
        };

        match selector {
            s if s == placeCall::SELECTOR => {
                let Ok(call) = placeCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.place(msg_sender, call.base, call.amount, call.price, call.isBid)
            }
            s if s == cancelCall::SELECTOR => {
                let Ok(call) = cancelCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.cancel(msg_sender, call.orderId)
            }
            s if s == getOrderCall::SELECTOR => {
                let Ok(call) = getOrderCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.get_order(msg_sender, call.orderId)
            }
            s if s == depositCall::SELECTOR => {
                let Ok(call) = depositCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.deposit(msg_sender, call.token, call.amount)
            }
            s if s == withdrawCall::SELECTOR => {
                let Ok(call) = withdrawCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.withdraw(msg_sender, call.token, call.amount)
            }
            s if s == balanceOfCall::SELECTOR => {
                let Ok(call) = balanceOfCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                if call.user != msg_sender {
                    revert!(Unauthorized {});
                }
                let bal = try_storage!(self.balance_of(call.user, call.token));
                Ok(StorageCtx.success_output(U256::from(bal).abi_encode().into()))
            }
            s if s == availableBalanceOfCall::SELECTOR => {
                let Ok(call) = availableBalanceOfCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                if call.user != msg_sender {
                    revert!(Unauthorized {});
                }
                let bal = try_storage!(self.available_balance_of(call.user, call.token));
                Ok(StorageCtx.success_output(U256::from(bal).abi_encode().into()))
            }
            s if s == pairKeyCall::SELECTOR => {
                let Ok(call) = pairKeyCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                let key = compute_book_key(call.base, call.quote);
                Ok(StorageCtx.success_output(key.abi_encode().into()))
            }
            s if s == createPairCall::SELECTOR => {
                let Ok(call) = createPairCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.create_pair(call.base)
            }
            s if s == bestBidCall::SELECTOR => {
                let Ok(call) = bestBidCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.best_bid(call.base)
            }
            s if s == bestAskCall::SELECTOR => {
                let Ok(call) = bestAskCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.best_ask(call.base)
            }
            s if s == MIN_ORDER_AMOUNTCall::SELECTOR => {
                Ok(StorageCtx.success_output(U256::from(MIN_ORDER_AMOUNT).abi_encode().into()))
            }
            s if s == marketBuyCall::SELECTOR => {
                let Ok(call) = marketBuyCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.market_buy(msg_sender, call.base, call.amount, call.maxQuoteIn)
            }
            s if s == marketSellCall::SELECTOR => {
                let Ok(call) = marketSellCall::abi_decode(calldata) else {
                    return Ok(StorageCtx.revert_output(Bytes::new()));
                };
                self.market_sell(msg_sender, call.base, call.amount, call.minQuoteOut)
            }
            _ => Ok(StorageCtx.revert_output(Bytes::new())),
        }
    }
}

// ── utilities ────────────────────────────────────────────────────────────

/// Compute deterministic book key from (base, quote) pair.
pub fn compute_book_key(base: Address, quote: Address) -> B256 {
    let mut buf = [0u8; 40];
    buf[..20].copy_from_slice(base.as_slice());
    buf[20..].copy_from_slice(quote.as_slice());
    keccak256(buf)
}

#[cfg(test)]
pub(crate) const _DARKPOOL_ADDRESS_BYTES: [u8; 20] = [
    0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
];

#[cfg(test)]
mod tests {
    //! Reference-price guardrail tests for the darkpool orderbook.
    //!
    //! The guard itself lives in [`crate::refprice`]; these tests pin the
    //! behavior the orderbook-side caller relies on (in-range pass,
    //! out-of-range reject above/below, missing-provider reject, stale-provider
    //! reject, and the "never stale" shortcut for static providers).

    use alloc::string::ToString;

    use super::MIN_ORDER_AMOUNT;
    use crate::refprice::{GuardrailRejection, ReferencePrice, ReferencePriceGuard};

    fn alpha_reference(price: u128, as_of_timestamp: u64) -> ReferencePrice {
        ReferencePrice {
            price,
            source: "static:alpha".to_string(),
            as_of_block: 42,
            as_of_timestamp,
        }
    }

    fn alpha_guard(max_deviation_bps: u32, max_staleness_secs: u64) -> ReferencePriceGuard {
        ReferencePriceGuard {
            max_deviation_bps,
            max_staleness_secs,
        }
    }

    #[test]
    fn orderbook_min_order_amount_is_unchanged() {
        // Sanity guard against accidental tuning of the darkpool dust floor
        // while landing the reference-price guardrails.
        assert_eq!(MIN_ORDER_AMOUNT, 100);
    }

    #[test]
    fn orderbook_reference_price_guard_accepts_in_range_order() {
        let reference = alpha_reference(1_000_000, 1_700_000_000);
        let guard = alpha_guard(1_000, 0); // ±10%, no staleness check

        // Equal price → 0 bps deviation.
        guard
            .check_order_price(Some(&reference), 1_700_000_010, 1_000_000)
            .expect("equal price must pass");

        // +5% deviation.
        guard
            .check_order_price(Some(&reference), 1_700_000_010, 1_050_000)
            .expect("+5% must pass under a ±10% bound");

        // -5% deviation.
        guard
            .check_order_price(Some(&reference), 1_700_000_010, 950_000)
            .expect("-5% must pass under a ±10% bound");
    }

    #[test]
    fn orderbook_reference_price_guard_rejects_out_of_range_order_above() {
        let reference = alpha_reference(1_000_000, 1_700_000_000);
        let guard = alpha_guard(1_000, 0); // ±10%

        let err = guard
            .check_order_price(Some(&reference), 1_700_000_010, 1_200_000)
            .expect_err("+20% must be rejected under a ±10% bound");

        assert_eq!(
            err,
            GuardrailRejection::OutOfRange {
                reference_price: 1_000_000,
                order_price: 1_200_000,
                max_deviation_bps: 1_000,
                deviation_bps: 2_000,
            },
        );
    }

    #[test]
    fn orderbook_reference_price_guard_rejects_out_of_range_order_below() {
        let reference = alpha_reference(1_000_000, 1_700_000_000);
        let guard = alpha_guard(1_000, 0); // ±10%

        let err = guard
            .check_order_price(Some(&reference), 1_700_000_010, 800_000)
            .expect_err("-20% must be rejected under a ±10% bound");

        assert_eq!(
            err,
            GuardrailRejection::OutOfRange {
                reference_price: 1_000_000,
                order_price: 800_000,
                max_deviation_bps: 1_000,
                deviation_bps: 2_000,
            },
        );
    }

    #[test]
    fn orderbook_reference_price_guard_rejects_when_provider_disabled() {
        let guard = alpha_guard(1_000, 0);
        let err = guard
            .check_order_price(None, 1_700_000_000, 1_000_000)
            .expect_err("missing provider must reject");
        assert_eq!(err, GuardrailRejection::ProviderDisabled);
    }

    #[test]
    fn orderbook_reference_price_guard_rejects_stale_provider() {
        let reference = alpha_reference(1_000_000, 1_700_000_000);
        let guard = alpha_guard(1_000, 60); // ±10%, 60s max staleness

        // 120s after the snapshot — stale.
        let err = guard
            .check_order_price(Some(&reference), 1_700_000_120, 1_000_000)
            .expect_err("stale provider must reject");
        assert_eq!(
            err,
            GuardrailRejection::StaleReference {
                observed_age_secs: 120,
                max_age_secs: 60,
            },
        );

        // Exactly at the staleness boundary — still fresh.
        guard
            .check_order_price(Some(&reference), 1_700_000_060, 1_000_000)
            .expect("equal-to-bound age must still be fresh");
    }

    #[test]
    fn orderbook_reference_price_guard_treats_zero_staleness_window_as_never_stale() {
        let reference = alpha_reference(1_000_000, 1_000);
        let guard = alpha_guard(1_000, 0);

        // Snapshot is 1e9 seconds old but the guard is configured to never stale.
        guard
            .check_order_price(Some(&reference), 1_000_001_000, 1_000_000)
            .expect("zero staleness window must mean never stale");
    }

    #[test]
    fn orderbook_reference_price_guard_rejects_zero_reference_price() {
        let reference = alpha_reference(0, 1_700_000_000);
        let guard = alpha_guard(1_000, 0);
        let err = guard
            .check_order_price(Some(&reference), 1_700_000_010, 1_000_000)
            .expect_err("zero reference price must reject");
        assert_eq!(err, GuardrailRejection::ZeroReferencePrice);
    }

    #[test]
    fn orderbook_reference_price_is_fresh_respects_staleness_window() {
        let reference = alpha_reference(1_000_000, 1_700_000_000);

        let no_staleness = alpha_guard(1_000, 0);
        assert!(no_staleness.is_fresh(&reference, 1_700_999_999));

        let bounded = alpha_guard(1_000, 60);
        assert!(bounded.is_fresh(&reference, 1_700_000_060));
        assert!(!bounded.is_fresh(&reference, 1_700_000_061));
    }
}
