//! Tests for zone-specific precompile availability.

use alloy::{
    primitives::{U128, U256, address},
    providers::{Provider, ProviderBuilder},
};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use alloy_sol_types::{SolEvent, sol};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_precompiles::{PATH_USD_ADDRESS, tip403_registry::ALLOW_ALL_POLICY_ID};
use zone_l1::EnabledToken;
use zone_precompiles::DARKPOOL_ADDRESS;

use crate::utils::{
    DEFAULT_TIMEOUT, STABLECOIN_DEX_ADDRESS, TEST_MNEMONIC, TestStablecoinDEX,
    start_local_zone_with_fixture,
};

const ALPHA_USD_ADDRESS: alloy::primitives::Address =
    address!("0x20C0000000000000000000000000000000000001");

fn signer_at(index: u32) -> eyre::Result<PrivateKeySigner> {
    let builder = MnemonicBuilder::<English>::default().phrase(TEST_MNEMONIC);
    if index == 0 {
        Ok(builder.build()?)
    } else {
        Ok(builder.index(index)?.build()?)
    }
}

fn alpha_usd_enabled_token() -> EnabledToken {
    EnabledToken {
        token: ALPHA_USD_ADDRESS,
        name: "Alpha USD".to_string(),
        symbol: "alphaUSD".to_string(),
        currency: "USD".to_string(),
    }
}

sol! {
    #[sol(rpc)]
    contract TestDarkpoolOrderbook {
        struct OrderView {
            uint128 orderId;
            address maker;
            address base;
            address quote;
            bool isBid;
            uint128 price;
            uint128 quantity;
        }

        function MIN_ORDER_AMOUNT() external pure returns (uint128);
        function place(address base, address quote, uint128 amount, uint128 price, bool isBid)
            external returns (uint128 orderId);
        function deposit(address token, uint128 amount) external;
        function cancel(uint128 orderId) external;
        function getOrder(uint128 orderId) external view returns (OrderView memory);
        function withdraw(address token, uint128 amount) external;
        function pairCount() external view returns (uint256);
        function pairAt(uint256 index) external view returns (address base, address quote);
        function pairExists(address base, address quote) external view returns (bool);
        function bestBid(address base, address quote) external view returns (uint128 price, uint128 quantity);
        function bestAsk(address base, address quote) external view returns (uint128 price, uint128 quantity);
        function balanceOf(address user, address token) external view returns (uint128);
        function availableBalanceOf(address user, address token) external view returns (uint128);
        function marketBuy(address base, address quote, uint128 amount, uint128 maxQuoteIn)
            external returns (uint128 quoteSpent);
        function marketSell(address base, address quote, uint128 amount, uint128 minQuoteOut)
            external returns (uint128 quoteReceived);

        event OrderSubmitted(
            uint128 indexed orderId,
            address indexed maker,
            address base,
            address quote,
            uint128 amount,
            uint128 price,
            bool isBid
        );
        event OrderPlaced(
            uint128 indexed orderId,
            address indexed maker,
            address base,
            address quote,
            uint128 amount,
            uint128 price,
            bool isBid
        );
        event OrderFilled(
            uint128 indexed orderId,
            address indexed maker,
            address indexed taker,
            uint128 amountFilled,
            uint128 price
        );
        event OrderMatched(
            uint128 indexed makerOrderId,
            uint128 indexed takerOrderId,
            address indexed maker,
            address taker,
            uint128 amountFilled,
            uint128 price
        );
        event OrderCancelled(
            uint128 indexed orderId,
            address indexed maker
        );
    }
}

/// The StablecoinDEX precompile should be disabled on zones — any call to
/// it must revert.
#[tokio::test(flavor = "multi_thread")]
async fn test_dex_disabled_on_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    // Inject an empty block so the zone is alive and processing.
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_tempo_block_number(1, DEFAULT_TIMEOUT).await?;

    // Attempt to call createPair on the DEX — should revert because the
    // precompile is not registered on the zone.
    let dex = TestStablecoinDEX::new(STABLECOIN_DEX_ADDRESS, zone.provider());
    let result = dex.createPair(PATH_USD_ADDRESS).call().await;

    assert!(
        result.is_err(),
        "StablecoinDEX should be disabled on zones — createPair must revert"
    );

    Ok(())
}

/// The darkpool orderbook precompile should be available on zones.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_available_on_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_tempo_block_number(1, DEFAULT_TIMEOUT).await?;

    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, zone.provider());
    let result = darkpool.MIN_ORDER_AMOUNT().call().await;
    let code = zone.provider().get_code_at(DARKPOOL_ADDRESS).await?;

    assert!(
        result.is_ok(),
        "Darkpool should be available on zones — MIN_ORDER_AMOUNT must succeed"
    );
    assert!(
        !code.is_empty(),
        "Darkpool account must have marker bytecode so precompile storage writes persist"
    );

    Ok(())
}

/// A limit bid should pull missing quote escrow from the caller's zone wallet
/// into the darkpool's internal balance.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_place_pulls_zone_wallet_balance() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let dev_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let dev_address = dev_signer.address();
    let provider = ProviderBuilder::new()
        .wallet(dev_signer)
        .connect_http(zone.http_url().clone());
    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &provider);

    assert_eq!(darkpool.pairCount().call().await?, U256::ZERO);
    assert!(
        !darkpool
            .pairExists(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
            .call()
            .await?
    );

    let amount: u128 = 1_000_000;
    let price: u128 = 1;
    let initial_balance: u128 = 10_000_000;

    fixture.inject_enabled_tokens(zone.deposit_queue(), vec![alpha_usd_enabled_token()]);
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, initial_balance)],
    );
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(initial_balance),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let bid_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let bid_receipt = bid_pending.get_receipt().await?;
    assert!(
        bid_receipt.status(),
        "bid should pull quote escrow from the zone wallet"
    );

    assert_eq!(darkpool.pairCount().call().await?, U256::from(1));
    let pair = darkpool.pairAt(U256::ZERO).call().await?;
    assert_eq!(pair.base, ALPHA_USD_ADDRESS);
    assert_eq!(pair.quote, PATH_USD_ADDRESS);
    assert!(
        darkpool
            .pairExists(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
            .call()
            .await?
    );
    assert!(
        !darkpool
            .pairExists(PATH_USD_ADDRESS, ALPHA_USD_ADDRESS)
            .call()
            .await?
    );

    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        amount * price,
        "pulled quote should be credited to the caller's internal balance"
    );
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        0,
        "resting bid should reserve the pulled quote escrow"
    );

    Ok(())
}

/// Darkpool-internal available balances should fund new order escrow before the
/// precompile pulls more from the zone wallet.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_place_reuses_internal_available_balance() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let dev_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let dev_address = dev_signer.address();
    let provider = ProviderBuilder::new()
        .wallet(dev_signer)
        .connect_http(zone.http_url().clone());
    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &provider);

    let amount: u128 = 1_000_000;
    let price: u128 = 1;
    let escrow = amount * price;

    fixture.inject_enabled_tokens(zone.deposit_queue(), vec![alpha_usd_enabled_token()]);
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, 10_000_000)],
    );
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(10_000_000u128),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let deposit_pending = darkpool
        .deposit(PATH_USD_ADDRESS, escrow)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let deposit_receipt = deposit_pending.get_receipt().await?;
    assert!(deposit_receipt.status(), "darkpool deposit should succeed");
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        escrow,
        "darkpool deposit should create available internal balance"
    );

    let bid_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let bid_receipt = bid_pending.get_receipt().await?;
    assert!(bid_receipt.status(), "bid should reuse internal escrow");
    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        escrow,
        "place must not pull and double-credit escrow when internal balance covers it"
    );
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        0,
        "reused internal escrow should be reserved by the resting bid"
    );

    Ok(())
}

/// Resting limit orders reserve their escrow and cannot be withdrawn until
/// filled or cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_resting_bid_escrow_is_not_withdrawable() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let dev_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let dev_address = dev_signer.address();
    let provider = ProviderBuilder::new()
        .wallet(dev_signer)
        .connect_http(zone.http_url().clone());
    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &provider);

    let amount: u128 = 1_000_000;
    let price: u128 = 1;
    let escrow = amount * price;

    fixture.inject_enabled_tokens(
        zone.deposit_queue(),
        vec![EnabledToken {
            token: ALPHA_USD_ADDRESS,
            name: "Alpha USD".to_string(),
            symbol: "alphaUSD".to_string(),
            currency: "USD".to_string(),
        }],
    );
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, 10_000_000)],
    );
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(10_000_000u128),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let bid_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let bid_receipt = bid_pending.get_receipt().await?;
    assert!(bid_receipt.status(), "bid placement should succeed");

    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        escrow,
        "resting bid escrow is part of the total internal balance"
    );
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        0,
        "resting bid escrow should not be withdrawable"
    );

    let withdraw_result = darkpool
        .withdraw(PATH_USD_ADDRESS, escrow)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await;
    if let Ok(pending) = withdraw_result {
        fixture.inject_empty_block(zone.deposit_queue());
        let receipt = pending.get_receipt().await?;
        assert!(
            !receipt.status(),
            "withdrawing reserved bid escrow should revert"
        );
    }

    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        0,
        "failed withdrawal must leave escrow reserved"
    );

    let cancel_pending = darkpool
        .cancel(1)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let cancel_receipt = cancel_pending.get_receipt().await?;
    assert!(cancel_receipt.status(), "cancel should release escrow");
    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        escrow,
        "cancel must not double-credit escrow"
    );
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        escrow,
        "cancelled escrow becomes withdrawable"
    );

    let withdraw_pending = darkpool
        .withdraw(PATH_USD_ADDRESS, escrow)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let withdraw_receipt = withdraw_pending.get_receipt().await?;
    assert!(withdraw_receipt.status(), "released escrow should withdraw");
    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        0,
        "withdraw should consume the released internal balance"
    );

    Ok(())
}

/// Self-crossing limit orders are valid and should execute when prices cross.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_self_crossing_limit_orders_fill() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let dev_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let dev_address = dev_signer.address();
    let provider = ProviderBuilder::new()
        .wallet(dev_signer)
        .connect_http(zone.http_url().clone());
    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &provider);

    let amount: u128 = 1_000_000;
    let price: u128 = 1;

    fixture.inject_enabled_tokens(
        zone.deposit_queue(),
        vec![EnabledToken {
            token: ALPHA_USD_ADDRESS,
            name: "Alpha USD".to_string(),
            symbol: "alphaUSD".to_string(),
            currency: "USD".to_string(),
        }],
    );
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![
            fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, 10_000_000),
            fixture.make_deposit(ALPHA_USD_ADDRESS, dev_address, dev_address, amount),
        ],
    );
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(10_000_000u128),
        DEFAULT_TIMEOUT,
    )
    .await?;
    zone.wait_for_balance(
        ALPHA_USD_ADDRESS,
        dev_address,
        U256::from(amount),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let bid_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let bid_receipt = bid_pending.get_receipt().await?;
    assert!(bid_receipt.status(), "bid placement should succeed");
    let bid_submitted = bid_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderSubmitted::decode_log(&log.inner).ok())
        .expect("bid should emit OrderSubmitted");
    let bid_placed = bid_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderPlaced::decode_log(&log.inner).ok())
        .expect("resting bid should emit OrderPlaced");
    assert_eq!(bid_submitted.orderId, 1, "first submission id");
    assert_eq!(
        bid_submitted.amount, amount,
        "submitted amount is original amount"
    );
    assert!(bid_submitted.isBid, "first submission should be a bid");
    assert_eq!(bid_placed.orderId, 1, "resting order keeps submission id");

    let best_bid = darkpool
        .bestBid(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    assert_eq!(best_bid.price, price, "bid should rest before the ask");
    assert_eq!(best_bid.quantity, amount, "full bid should be resting");

    let ask_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, false)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let ask_receipt = ask_pending.get_receipt().await?;
    assert!(ask_receipt.status(), "self-crossing ask should fill");
    let ask_submitted = ask_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderSubmitted::decode_log(&log.inner).ok())
        .expect("fully filled ask should still emit OrderSubmitted");
    let ask_matched = ask_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderMatched::decode_log(&log.inner).ok())
        .expect("self-crossing ask should emit OrderMatched");
    assert_eq!(
        ask_submitted.orderId, 2,
        "fully filled ask gets a stable id"
    );
    assert_eq!(ask_submitted.amount, amount, "submitted ask amount");
    assert!(!ask_submitted.isBid, "second submission should be an ask");
    assert_eq!(ask_matched.makerOrderId, 1, "resting maker order id");
    assert_eq!(ask_matched.takerOrderId, 2, "incoming taker order id");
    assert_eq!(ask_matched.amountFilled, amount, "full ask amount filled");

    let best_bid = darkpool
        .bestBid(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    let best_ask = darkpool
        .bestAsk(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    assert_eq!(best_bid.price, 0, "self-crossed bid should be removed");
    assert_eq!(
        best_bid.quantity, 0,
        "self-crossed bid quantity should be zero"
    );
    assert_eq!(best_ask.price, 0, "self-crossing ask should not rest");
    assert_eq!(
        best_ask.quantity, 0,
        "self-crossing ask quantity should be zero"
    );
    assert_eq!(
        darkpool
            .balanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        amount,
        "quote from the filled self-cross should remain available internally"
    );
    assert_eq!(
        darkpool
            .balanceOf(dev_address, ALPHA_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        amount,
        "base from the filled self-cross should remain available internally"
    );

    Ok(())
}

/// Market orders should follow the same self-crossing semantics as limit
/// orders: an owner may buy from their own ask or sell into their own bid.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_self_crossing_market_orders_fill() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let dev_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let dev_address = dev_signer.address();
    let provider = ProviderBuilder::new()
        .wallet(dev_signer)
        .connect_http(zone.http_url().clone());
    let darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &provider);

    let amount: u128 = 1_000_000;
    let price: u128 = 1;
    let path_balance: u128 = 10_000_000;
    let alpha_balance: u128 = 2_000_000;

    fixture.inject_enabled_tokens(zone.deposit_queue(), vec![alpha_usd_enabled_token()]);
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![
            fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, path_balance),
            fixture.make_deposit(ALPHA_USD_ADDRESS, dev_address, dev_address, alpha_balance),
        ],
    );
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(path_balance),
        DEFAULT_TIMEOUT,
    )
    .await?;
    zone.wait_for_balance(
        ALPHA_USD_ADDRESS,
        dev_address,
        U256::from(alpha_balance),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let ask_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, false)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    assert!(
        ask_pending.get_receipt().await?.status(),
        "self-owned ask should rest"
    );

    let buy_pending = darkpool
        .marketBuy(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, amount * price)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let buy_receipt = buy_pending.get_receipt().await?;
    assert!(
        buy_receipt.status(),
        "market buy should fill the caller's own ask"
    );
    let buy_fill = buy_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderFilled::decode_log(&log.inner).ok())
        .expect("self market buy should emit OrderFilled");
    assert_eq!(buy_fill.orderId, 1);
    assert_eq!(buy_fill.maker, dev_address);
    assert_eq!(buy_fill.taker, dev_address);
    assert_eq!(buy_fill.amountFilled, amount);
    assert_eq!(buy_fill.price, price);
    assert_eq!(
        darkpool
            .bestAsk(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
            .call()
            .await?
            .price,
        0
    );

    let bid_pending = darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    assert!(
        bid_pending.get_receipt().await?.status(),
        "self-owned bid should rest"
    );

    let sell_pending = darkpool
        .marketSell(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, amount * price)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let sell_receipt = sell_pending.get_receipt().await?;
    assert!(
        sell_receipt.status(),
        "market sell should fill the caller's own bid"
    );
    let sell_fill = sell_receipt
        .logs()
        .iter()
        .find_map(|log| TestDarkpoolOrderbook::OrderFilled::decode_log(&log.inner).ok())
        .expect("self market sell should emit OrderFilled");
    assert_eq!(sell_fill.orderId, 2);
    assert_eq!(sell_fill.maker, dev_address);
    assert_eq!(sell_fill.taker, dev_address);
    assert_eq!(sell_fill.amountFilled, amount);
    assert_eq!(sell_fill.price, price);
    assert_eq!(
        darkpool
            .bestBid(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
            .call()
            .await?
            .price,
        0
    );

    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, PATH_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        amount,
        "self market trades should leave quote available internally"
    );
    assert_eq!(
        darkpool
            .availableBalanceOf(dev_address, ALPHA_USD_ADDRESS)
            .from(dev_address)
            .call()
            .await?,
        amount,
        "self market trades should leave base available internally"
    );

    Ok(())
}

/// Limit-order matching must preserve price-time priority across multiple
/// makers and multiple taker submissions.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_multi_maker_multi_taker_fill_ordering() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let maker_one_signer = signer_at(0)?;
    let maker_two_signer = signer_at(1)?;
    let taker_one_signer = signer_at(2)?;
    let taker_two_signer = signer_at(3)?;

    let maker_one = maker_one_signer.address();
    let maker_two = maker_two_signer.address();
    let taker_one = taker_one_signer.address();
    let taker_two = taker_two_signer.address();

    let path_deposit: u128 = 10_000_000;
    let maker_one_amount: u128 = 300_000;
    let maker_two_amount: u128 = 400_000;
    let price: u128 = 2;

    fixture.inject_enabled_tokens(zone.deposit_queue(), vec![alpha_usd_enabled_token()]);
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![
            fixture.make_deposit(PATH_USD_ADDRESS, maker_one, maker_one, path_deposit),
            fixture.make_deposit(PATH_USD_ADDRESS, maker_two, maker_two, path_deposit),
            fixture.make_deposit(PATH_USD_ADDRESS, taker_one, taker_one, path_deposit),
            fixture.make_deposit(PATH_USD_ADDRESS, taker_two, taker_two, path_deposit),
            fixture.make_deposit(ALPHA_USD_ADDRESS, maker_one, maker_one, maker_one_amount),
            fixture.make_deposit(ALPHA_USD_ADDRESS, maker_two, maker_two, maker_two_amount),
        ],
    );
    for account in [maker_one, maker_two, taker_one, taker_two] {
        zone.wait_for_balance(
            PATH_USD_ADDRESS,
            account,
            U256::from(path_deposit),
            DEFAULT_TIMEOUT,
        )
        .await?;
    }
    for (account, amount) in [(maker_one, maker_one_amount), (maker_two, maker_two_amount)] {
        zone.wait_for_balance(
            ALPHA_USD_ADDRESS,
            account,
            U256::from(amount),
            DEFAULT_TIMEOUT,
        )
        .await?;
    }

    let maker_one_provider = ProviderBuilder::new()
        .wallet(maker_one_signer)
        .connect_http(zone.http_url().clone());
    let maker_two_provider = ProviderBuilder::new()
        .wallet(maker_two_signer)
        .connect_http(zone.http_url().clone());
    let taker_one_provider = ProviderBuilder::new()
        .wallet(taker_one_signer)
        .connect_http(zone.http_url().clone());
    let taker_two_provider = ProviderBuilder::new()
        .wallet(taker_two_signer)
        .connect_http(zone.http_url().clone());

    let maker_one_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &maker_one_provider);
    let maker_two_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &maker_two_provider);
    let taker_one_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &taker_one_provider);
    let taker_two_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &taker_two_provider);

    let maker_one_pending = maker_one_darkpool
        .place(
            ALPHA_USD_ADDRESS,
            PATH_USD_ADDRESS,
            maker_one_amount,
            price,
            false,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let maker_one_receipt = maker_one_pending.get_receipt().await?;
    assert!(maker_one_receipt.status(), "maker one ask should rest");

    let maker_two_pending = maker_two_darkpool
        .place(
            ALPHA_USD_ADDRESS,
            PATH_USD_ADDRESS,
            maker_two_amount,
            price,
            false,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let maker_two_receipt = maker_two_pending.get_receipt().await?;
    assert!(maker_two_receipt.status(), "maker two ask should rest");

    let taker_one_fill: u128 = 500_000;
    let taker_one_pending = taker_one_darkpool
        .place(
            ALPHA_USD_ADDRESS,
            PATH_USD_ADDRESS,
            taker_one_fill,
            price,
            true,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let taker_one_receipt = taker_one_pending.get_receipt().await?;
    assert!(
        taker_one_receipt.status(),
        "first taker bid should fill across both makers"
    );

    let taker_one_matches = taker_one_receipt
        .logs()
        .iter()
        .filter_map(|log| TestDarkpoolOrderbook::OrderMatched::decode_log(&log.inner).ok())
        .collect::<Vec<_>>();
    assert_eq!(
        taker_one_matches.len(),
        2,
        "first taker should consume maker one, then maker two"
    );
    assert_eq!(taker_one_matches[0].makerOrderId, 1, "oldest ask first");
    assert_eq!(taker_one_matches[0].takerOrderId, 3);
    assert_eq!(taker_one_matches[0].maker, maker_one);
    assert_eq!(taker_one_matches[0].taker, taker_one);
    assert_eq!(taker_one_matches[0].amountFilled, maker_one_amount);
    assert_eq!(
        taker_one_matches[1].makerOrderId, 2,
        "second ask supplies the remainder"
    );
    assert_eq!(taker_one_matches[1].takerOrderId, 3);
    assert_eq!(taker_one_matches[1].maker, maker_two);
    assert_eq!(taker_one_matches[1].taker, taker_one);
    assert_eq!(
        taker_one_matches[1].amountFilled,
        taker_one_fill - maker_one_amount
    );

    let remaining_maker_two = maker_two_amount - (taker_one_fill - maker_one_amount);
    let best_ask = taker_one_darkpool
        .bestAsk(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    assert_eq!(best_ask.price, price, "maker two residual should remain");
    assert_eq!(best_ask.quantity, remaining_maker_two);

    let taker_two_pending = taker_two_darkpool
        .place(
            ALPHA_USD_ADDRESS,
            PATH_USD_ADDRESS,
            remaining_maker_two,
            price,
            true,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let taker_two_receipt = taker_two_pending.get_receipt().await?;
    assert!(
        taker_two_receipt.status(),
        "second taker bid should consume maker two residual"
    );

    let taker_two_matches = taker_two_receipt
        .logs()
        .iter()
        .filter_map(|log| TestDarkpoolOrderbook::OrderMatched::decode_log(&log.inner).ok())
        .collect::<Vec<_>>();
    assert_eq!(taker_two_matches.len(), 1);
    assert_eq!(taker_two_matches[0].makerOrderId, 2);
    assert_eq!(taker_two_matches[0].takerOrderId, 4);
    assert_eq!(taker_two_matches[0].maker, maker_two);
    assert_eq!(taker_two_matches[0].taker, taker_two);
    assert_eq!(taker_two_matches[0].amountFilled, remaining_maker_two);

    let best_bid = taker_two_darkpool
        .bestBid(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    let best_ask = taker_two_darkpool
        .bestAsk(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS)
        .call()
        .await?;
    assert_eq!(best_bid.quantity, 0, "no taker bid should rest");
    assert_eq!(best_ask.quantity, 0, "all maker asks should be filled");

    Ok(())
}

/// A partially-filled order reconstructed from emitted events should preserve
/// fill state across multiple taker `place` calls, then record the cancel tx.
#[tokio::test(flavor = "multi_thread")]
async fn test_darkpool_partial_fill_then_cancel_reconstructs_from_events() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(20).await?;
    zone.policy_cache()
        .write()
        .set_token_policy(ALPHA_USD_ADDRESS, 0, ALLOW_ALL_POLICY_ID);

    let maker_signer = signer_at(0)?;
    let taker_one_signer = signer_at(1)?;
    let taker_two_signer = signer_at(2)?;
    let maker = maker_signer.address();
    let taker_one = taker_one_signer.address();
    let taker_two = taker_two_signer.address();

    let amount: u128 = 1_000_000;
    let first_fill: u128 = 300_000;
    let second_fill: u128 = 400_000;
    let price: u128 = 2;
    let path_deposit: u128 = 10_000_000;

    fixture.inject_enabled_tokens(zone.deposit_queue(), vec![alpha_usd_enabled_token()]);
    fixture.inject_deposits(
        zone.deposit_queue(),
        vec![
            fixture.make_deposit(PATH_USD_ADDRESS, maker, maker, path_deposit),
            fixture.make_deposit(PATH_USD_ADDRESS, taker_one, taker_one, path_deposit),
            fixture.make_deposit(PATH_USD_ADDRESS, taker_two, taker_two, path_deposit),
            fixture.make_deposit(ALPHA_USD_ADDRESS, maker, maker, amount),
        ],
    );
    for account in [maker, taker_one, taker_two] {
        zone.wait_for_balance(
            PATH_USD_ADDRESS,
            account,
            U256::from(path_deposit),
            DEFAULT_TIMEOUT,
        )
        .await?;
    }
    zone.wait_for_balance(
        ALPHA_USD_ADDRESS,
        maker,
        U256::from(amount),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let maker_provider = ProviderBuilder::new()
        .wallet(maker_signer)
        .connect_http(zone.http_url().clone());
    let taker_one_provider = ProviderBuilder::new()
        .wallet(taker_one_signer)
        .connect_http(zone.http_url().clone());
    let taker_two_provider = ProviderBuilder::new()
        .wallet(taker_two_signer)
        .connect_http(zone.http_url().clone());
    let maker_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &maker_provider);
    let taker_one_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &taker_one_provider);
    let taker_two_darkpool = TestDarkpoolOrderbook::new(DARKPOOL_ADDRESS, &taker_two_provider);

    let ask_pending = maker_darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, amount, price, false)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let ask_receipt = ask_pending.get_receipt().await?;
    assert!(ask_receipt.status(), "maker ask should rest");

    let first_bid_pending = taker_one_darkpool
        .place(ALPHA_USD_ADDRESS, PATH_USD_ADDRESS, first_fill, price, true)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let first_bid_receipt = first_bid_pending.get_receipt().await?;
    assert!(
        first_bid_receipt.status(),
        "first partial fill should succeed"
    );

    let second_bid_pending = taker_two_darkpool
        .place(
            ALPHA_USD_ADDRESS,
            PATH_USD_ADDRESS,
            second_fill,
            price,
            true,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(4_000_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let second_bid_receipt = second_bid_pending.get_receipt().await?;
    assert!(
        second_bid_receipt.status(),
        "second partial fill should succeed"
    );

    let mut logs = Vec::new();
    logs.extend(ask_receipt.logs().iter().cloned());
    logs.extend(first_bid_receipt.logs().iter().cloned());
    logs.extend(second_bid_receipt.logs().iter().cloned());

    let mut orders = zone_rpc::darkpool::reconstruct_orders(
        logs.iter()
            .filter(|log| zone_rpc::darkpool::caller_is_maker(log, &maker)),
    );
    assert_eq!(orders.len(), 1, "maker should have one reconstructed order");
    let order = orders.pop().expect("one reconstructed order");
    assert_eq!(order.order_id, U128::from(1u128));
    assert_eq!(order.side, zone_rpc::darkpool::Side::Ask);
    assert_eq!(
        order.status,
        zone_rpc::darkpool::OrderStatus::PartiallyFilled
    );
    assert_eq!(order.amount, U128::from(amount));
    assert_eq!(order.filled, U128::from(first_fill + second_fill));
    assert_eq!(
        order.remaining,
        U128::from(amount - first_fill - second_fill)
    );
    assert_eq!(order.price, U128::from(price));
    assert_eq!(order.cancel_tx_hash, None);

    let live_order = maker_darkpool.getOrder(1).from(maker).call().await?;
    assert_eq!(
        live_order.quantity,
        amount - first_fill - second_fill,
        "live resting quantity should match reconstructed remaining"
    );

    let cancel_pending = maker_darkpool
        .cancel(1)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let cancel_receipt = cancel_pending.get_receipt().await?;
    assert!(
        cancel_receipt.status(),
        "cancel should succeed after partial fill"
    );

    let cancel_tx_hash = cancel_receipt
        .logs()
        .iter()
        .find_map(|log| {
            TestDarkpoolOrderbook::OrderCancelled::decode_log(&log.inner)
                .ok()
                .and(log.transaction_hash)
        })
        .expect("cancel should emit OrderCancelled with tx hash");
    logs.extend(cancel_receipt.logs().iter().cloned());

    let mut orders = zone_rpc::darkpool::reconstruct_orders(
        logs.iter()
            .filter(|log| zone_rpc::darkpool::caller_is_maker(log, &maker)),
    );
    assert_eq!(orders.len(), 1, "cancel should update the existing order");
    let order = orders.pop().expect("one reconstructed order");
    assert_eq!(order.status, zone_rpc::darkpool::OrderStatus::Cancelled);
    assert_eq!(
        order.cancel_tx_hash,
        Some(cancel_tx_hash),
        "cancel tx hash must propagate into reconstructed order state"
    );
    assert_eq!(
        order.remaining,
        U128::ZERO,
        "cancelled orders should not report withdrawable residual as resting"
    );
    assert_eq!(
        order.filled,
        U128::from(first_fill + second_fill),
        "cancel should preserve the previously filled amount"
    );

    assert_eq!(
        maker_darkpool
            .availableBalanceOf(maker, ALPHA_USD_ADDRESS)
            .from(maker)
            .call()
            .await?,
        amount - first_fill - second_fill,
        "cancel should release the residual base balance"
    );

    Ok(())
}
