//! Tests for zone-specific precompile availability.

use alloy::primitives::{U256, address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use alloy_sol_types::{SolEvent, sol};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_precompiles::{PATH_USD_ADDRESS, tip403_registry::ALLOW_ALL_POLICY_ID};
use zone::precompiles::DARKPOOL_ADDRESS;

use crate::utils::{
    DEFAULT_TIMEOUT, STABLECOIN_DEX_ADDRESS, TEST_MNEMONIC, TestStablecoinDEX,
    start_local_zone_with_fixture,
};

const ALPHA_USD_ADDRESS: alloy::primitives::Address =
    address!("0x20C0000000000000000000000000000000000001");

sol! {
    #[sol(rpc)]
    contract TestDarkpoolOrderbook {
        function MIN_ORDER_AMOUNT() external pure returns (uint128);
        function place(address base, uint128 amount, uint128 price, bool isBid)
            external returns (uint128 orderId);
        function cancel(uint128 orderId) external;
        function withdraw(address token, uint128 amount) external;
        function bestBid(address base) external view returns (uint128 price, uint128 quantity);
        function bestAsk(address base) external view returns (uint128 price, uint128 quantity);
        function balanceOf(address user, address token) external view returns (uint128);
        function availableBalanceOf(address user, address token) external view returns (uint128);

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
        event OrderMatched(
            uint128 indexed makerOrderId,
            uint128 indexed takerOrderId,
            address indexed maker,
            address taker,
            uint128 amountFilled,
            uint128 price
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
        vec![zone::EnabledToken {
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
        .place(ALPHA_USD_ADDRESS, amount, price, true)
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
        vec![zone::EnabledToken {
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
        .place(ALPHA_USD_ADDRESS, amount, price, true)
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

    let best_bid = darkpool.bestBid(ALPHA_USD_ADDRESS).call().await?;
    assert_eq!(best_bid.price, price, "bid should rest before the ask");
    assert_eq!(best_bid.quantity, amount, "full bid should be resting");

    let ask_pending = darkpool
        .place(ALPHA_USD_ADDRESS, amount, price, false)
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

    let best_bid = darkpool.bestBid(ALPHA_USD_ADDRESS).call().await?;
    let best_ask = darkpool.bestAsk(ALPHA_USD_ADDRESS).call().await?;
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
