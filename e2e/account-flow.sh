#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ACCOUNT_FILE="${ACCOUNT_FILE:-$ROOT_DIR/e2e/.account.json}"

PATHUSD="${PATHUSD:-0x20C0000000000000000000000000000000000000}"
ALPHAUSD="${ALPHAUSD:-0x20C0000000000000000000000000000000000001}"
DARKPOOL="${DARKPOOL:-0x0B00000000000000000000000000000000000001}"
OUTBOX="${OUTBOX:-0x1c00000000000000000000000000000000000002}"
FEE_RECIPIENT="${FEE_RECIPIENT:-0xFeeC000000000000000000000000000000000000}"
TRANSFER_TOPIC="0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
WITHDRAWAL_PROCESSED_TOPIC="0x49ae2215ae0dc5cb44364a538a7007364db417143d69e45cf5247c53a7940bf2"

PATHUSD_AMOUNT="${PATHUSD_AMOUNT:-10000000}"
ALPHAUSD_AMOUNT="${ALPHAUSD_AMOUNT:-10000000}"
ORDER_AMOUNT="${ORDER_AMOUNT:-1000000}"
SELL_PRICE="${SELL_PRICE:-2}"
BUY_PRICE="${BUY_PRICE:-1}"
WITHDRAW_PATHUSD_AMOUNT="${WITHDRAW_PATHUSD_AMOUNT:-$((PATHUSD_AMOUNT / 2))}"
WITHDRAW_ALPHAUSD_AMOUNT="${WITHDRAW_ALPHAUSD_AMOUNT:-$((ALPHAUSD_AMOUNT / 2))}"

ZONE_NAME="${ZONE_NAME:-}"
L1_RPC_URL="${L1_RPC_URL:?Set L1_RPC_URL}"
ZONE_RPC_URL="${ZONE_RPC_URL:-http://localhost:8546}"
PRIVATE_ZONE_RPC_URL="${PRIVATE_ZONE_RPC_URL:-http://localhost:8544}"
WAIT_TIMEOUT_SECONDS="${WAIT_TIMEOUT_SECONDS:-180}"
VERIFY_L1_WITHDRAWAL_SETTLEMENT="${VERIFY_L1_WITHDRAWAL_SETTLEMENT:-1}"
APPROVE_GAS_FALLBACK="${APPROVE_GAS_FALLBACK:-500000}"
DEPOSIT_GAS_FALLBACK="${DEPOSIT_GAS_FALLBACK:-900000}"
ORDER_GAS_FALLBACK="${ORDER_GAS_FALLBACK:-4000000}"
WITHDRAW_GAS_FALLBACK="${WITHDRAW_GAS_FALLBACK:-2000000}"

HTTP_L1_RPC="$L1_RPC_URL"
HTTP_L1_RPC="${HTTP_L1_RPC/#wss:\/\//https://}"
HTTP_L1_RPC="${HTTP_L1_RPC/#ws:\/\//http://}"

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "ERROR: missing required command '$1'" >&2
        exit 1
    }
}

log() {
    printf '\n==> %s\n' "$*"
}

warn() {
    printf '\n==> %s\n' "$*" >&2
}

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

require_nonempty() {
    local name="$1"
    local value="$2"
    if [[ -z "$value" || "$value" == "null" ]]; then
        fail "$name is required"
    fi
}

require_uint() {
    local name="$1"
    local value="$2"
    require_nonempty "$name" "$value"
    if [[ ! "$value" =~ ^[0-9]+$ ]]; then
        fail "$name must be an unsigned integer, got '$value'"
    fi
}

rpc_hex_to_dec() {
    cast --to-dec "$1"
}

normalize_uint() {
    local value="$1"
    value="${value%% *}"
    value="${value//$'\n'/}"
    if [[ "$value" == 0x* ]]; then
        cast --to-dec "$value"
        return
    fi
    if [[ "$value" =~ ^[0-9]+$ ]]; then
        echo "$value"
        return
    fi
    fail "expected unsigned integer, got '$1'"
}

lower_hex() {
    echo "$1" | tr '[:upper:]' '[:lower:]'
}

address_topic() {
    local hex
    hex="$(lower_hex "${1#0x}")"
    printf '0x%064s\n' "$hex" | tr ' ' '0'
}

topic_uint() {
    normalize_uint "$1"
}

data_word() {
    local data="${1#0x}"
    local index="$2"
    echo "0x${data:$((index * 64)):64}"
}

word_address() {
    local word="${1#0x}"
    lower_hex "0x${word:24:40}"
}

word_bool() {
    local word
    word="$(normalize_uint "$1")"
    if (( word == 0 )); then
        echo "false"
    else
        echo "true"
    fi
}

wallet_private_key() {
    if [[ -n "${PRIVATE_KEY:-}" ]]; then
        echo "$PRIVATE_KEY"
        return
    fi

    if [[ -f "$ACCOUNT_FILE" ]]; then
        jq -r '.private_key' "$ACCOUNT_FILE"
        return
    fi

    warn "Creating account"
    mkdir -p "$(dirname "$ACCOUNT_FILE")"
    umask 077
    cast wallet new --json | jq '.[0]' > "$ACCOUNT_FILE"
    jq -r '.private_key' "$ACCOUNT_FILE"
}

load_zone_metadata() {
    if [[ -z "$ZONE_NAME" ]]; then
        L1_PORTAL_ADDRESS="${L1_PORTAL_ADDRESS:-}"
        ZONE_ID="${ZONE_ID:-}"
        ZONE_CHAIN_ID="${ZONE_CHAIN_ID:-}"
        require_nonempty "L1_PORTAL_ADDRESS (or ZONE_NAME)" "$L1_PORTAL_ADDRESS"
        require_uint "ZONE_ID (or ZONE_NAME)" "$ZONE_ID"
        require_uint "ZONE_CHAIN_ID (or ZONE_NAME)" "$ZONE_CHAIN_ID"
        return
    fi

    local zone_json="$ROOT_DIR/generated/$ZONE_NAME/zone.json"
    local genesis_json="$ROOT_DIR/generated/$ZONE_NAME/genesis.json"
    [[ -f "$zone_json" ]] || fail "$zone_json not found"
    [[ -f "$genesis_json" ]] || fail "$genesis_json not found"

    L1_PORTAL_ADDRESS="${L1_PORTAL_ADDRESS:-$(jq -r '.portal // empty' "$zone_json")}"
    ZONE_ID="$(jq -r '.zoneId // empty' "$zone_json")"
    ZONE_CHAIN_ID="$(jq -r '.config.chainId // empty' "$genesis_json")"
    require_nonempty "generated portal" "$L1_PORTAL_ADDRESS"
    require_uint "generated zoneId" "$ZONE_ID"
    require_uint "generated chainId" "$ZONE_CHAIN_ID"
}

tip20_balance() {
    local rpc="$1"
    local token="$2"
    local account="$3"
    local raw
    if ! raw="$(cast call "$token" "balanceOf(address)(uint256)" "$account" \
        --rpc-url "$rpc" \
        --from "$account")"; then
        return 1
    fi
    normalize_uint "$raw"
}

buffered_gas_limit() {
    local fallback="$1"
    local rpc="$2"
    local from="$3"
    shift 3

    local raw estimated buffered
    if ! raw="$(cast estimate "$@" --rpc-url "$rpc" --from "$from" 2>/dev/null)"; then
        echo "$fallback"
        return
    fi

    estimated="$(normalize_uint "$raw")"
    buffered=$(((estimated * 130 + 99) / 100 + 50000))
    if (( buffered < fallback )); then
        echo "$fallback"
    else
        echo "$buffered"
    fi
}

ensure_portal_token_enabled() {
    local token="$1"
    local label="$2"
    local enabled
    enabled="$(cast call "$L1_PORTAL_ADDRESS" "isTokenEnabled(address)(bool)" "$token" --rpc-url "$HTTP_L1_RPC")"
    if [[ "$enabled" != "true" ]]; then
        fail "$label ($token) is not enabled on portal $L1_PORTAL_ADDRESS"
    fi
}

wait_for_zone_balance_at_least() {
    local token="$1"
    local account="$2"
    local target="$3"
    local label="$4"
    local deadline=$((SECONDS + WAIT_TIMEOUT_SECONDS))

    while (( SECONDS < deadline )); do
        local balance
        balance="$(tip20_balance "$ZONE_RPC_URL" "$token" "$account" 2>/dev/null || echo 0)"
        printf '  %s zone balance: %s / %s\r' "$label" "$balance" "$target"
        if (( balance >= target )); then
            printf '\n'
            return
        fi
        sleep 2
    done

    printf '\n' >&2
    fail "timed out waiting for $label zone balance >= $target"
}

wait_for_l1_balance_at_least() {
    local token="$1"
    local account="$2"
    local target="$3"
    local label="$4"
    local deadline=$((SECONDS + WAIT_TIMEOUT_SECONDS))

    while (( SECONDS < deadline )); do
        local balance
        balance="$(tip20_balance "$HTTP_L1_RPC" "$token" "$account" 2>/dev/null || echo 0)"
        printf '  %s L1 balance: %s / %s\r' "$label" "$balance" "$target"
        if (( balance >= target )); then
            printf '\n'
            return
        fi
        sleep 2
    done

    printf '\n' >&2
    fail "timed out waiting for $label L1 balance >= $target"
}

wait_for_withdrawal_processed() {
    local from_block="$1"
    local token="$2"
    local to="$3"
    local amount="$4"
    local label="$5"
    local deadline=$((SECONDS + WAIT_TIMEOUT_SECONDS))
    local token_lower
    token_lower="$(lower_hex "$token")"

    while (( SECONDS < deadline )); do
        local logs
        logs="$(cast logs --address "$L1_PORTAL_ADDRESS" --from-block "$from_block" --rpc-url "$HTTP_L1_RPC" \
            "WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess)" \
            "$to" --json 2>/dev/null || echo "[]")"

        while IFS= read -r log_json; do
            local topic data event_token event_amount event_success tx_hash block_number
            [[ -n "$log_json" ]] || continue
            topic="$(echo "$log_json" | jq -r '.topics[0] // empty')"
            [[ "$topic" == "$WITHDRAWAL_PROCESSED_TOPIC" ]] || continue
            data="$(echo "$log_json" | jq -r '.data // empty')"
            [[ -n "$data" && "$data" != "0x" ]] || continue
            event_token="$(word_address "$(data_word "$data" 0)")"
            [[ "$event_token" == "$token_lower" ]] || continue
            event_amount="$(normalize_uint "$(data_word "$data" 1)")"
            [[ "$event_amount" == "$amount" ]] || continue
            event_success="$(word_bool "$(data_word "$data" 2)")"
            [[ "$event_success" == "true" ]] || continue
            tx_hash="$(echo "$log_json" | jq -r '.transactionHash')"
            block_number="$(normalize_uint "$(echo "$log_json" | jq -r '.blockNumber')")"
            echo "  $label WithdrawalProcessed: tx=$tx_hash block=$block_number amount=$event_amount"
            return
        done < <(echo "$logs" | jq -c '.[]' 2>/dev/null || true)

        printf '  waiting for %s WithdrawalProcessed from L1 block %s\r' "$label" "$from_block"
        sleep 2
    done

    printf '\n' >&2
    fail "timed out waiting for $label WithdrawalProcessed event from L1 block $from_block"
}

send_checked() {
    local description="$1"
    shift
    local output
    if ! output="$("$@" --json 2>&1)"; then
        echo "ERROR: $description command failed" >&2
        echo "$output" >&2
        exit 1
    fi
    local status
    status="$(echo "$output" | jq -r '.status // empty' 2>/dev/null || true)"
    local tx
    tx="$(echo "$output" | jq -r '.transactionHash // empty' 2>/dev/null || true)"
    if [[ "$status" != "0x1" && "$status" != "1" ]]; then
        echo "ERROR: $description failed" >&2
        echo "$output" | jq . >&2 2>/dev/null || echo "$output" >&2
        exit 1
    fi
    echo "$tx"
}

build_auth_token() {
    local private_key="$1"
    require_uint "ZONE_ID" "${ZONE_ID:-}"
    require_uint "ZONE_CHAIN_ID" "${ZONE_CHAIN_ID:-}"

    local now expires magic version fields digest sig sig_hex
    now="$(date +%s)"
    expires=$((now + 600))
    magic="54656d706f5a6f6e655250430000000000000000000000000000000000000000"
    version="00"
    fields="${version}$(printf '%08x' "$ZONE_ID")$(printf '%016x' "$ZONE_CHAIN_ID")$(printf '%016x' "$now")$(printf '%016x' "$expires")"
    digest="$(cast keccak "0x${magic}${fields}")"
    sig="$(cast wallet sign --no-hash "$digest" --private-key "$private_key")"
    sig_hex="${sig#0x}"
    echo "${sig_hex}${fields}"
}

private_rpc() {
    local token="$1"
    local payload="$2"
    curl -sS -X POST "$PRIVATE_ZONE_RPC_URL" \
        -H "Content-Type: application/json" \
        -H "X-Authorization-Token: $token" \
        -d "$payload"
}

private_zone_call() {
    local auth="$1"
    local method="$2"
    local params="$3"
    private_rpc "$auth" "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

require_json_rpc_result() {
    local method="$1"
    local response="$2"
    local error
    if ! error="$(echo "$response" | jq -r '.error.message // empty')"; then
        echo "$response" >&2
        fail "$method returned invalid JSON"
    fi
    if [[ -n "$error" ]]; then
        echo "$response" | jq . >&2 2>/dev/null || echo "$response" >&2
        fail "$method failed: $error"
    fi
    if ! echo "$response" | jq -e '.result != null' >/dev/null; then
        echo "$response" | jq . >&2 2>/dev/null || echo "$response" >&2
        fail "$method returned null result"
    fi
}

json_quantity() {
    local value="$1"
    if [[ "$value" == "null" || -z "$value" ]]; then
        echo "0"
        return
    fi
    normalize_uint "$value"
}

json_required_quantity() {
    local response="$1"
    local jq_expr="$2"
    local label="$3"
    local raw
    if ! raw="$(echo "$response" | jq -er "$jq_expr")"; then
        fail "$label is missing"
    fi
    json_quantity "$raw"
}

json_required_address() {
    local response="$1"
    local jq_expr="$2"
    local label="$3"
    local raw
    if ! raw="$(echo "$response" | jq -er "$jq_expr")"; then
        fail "$label is missing"
    fi
    require_nonempty "$label" "$raw"
    echo "$raw"
}

assert_address_equal() {
    local label="$1"
    local actual="$2"
    local expected="$3"
    local actual_lower expected_lower
    actual_lower="$(echo "$actual" | tr '[:upper:]' '[:lower:]')"
    expected_lower="$(echo "$expected" | tr '[:upper:]' '[:lower:]')"
    if [[ "$actual_lower" != "$expected_lower" ]]; then
        fail "$label mismatch: expected $expected, got $actual"
    fi
}

assert_uint_equal() {
    local label="$1"
    local actual="$2"
    local expected="$3"
    if (( actual != expected )); then
        fail "$label mismatch: expected $expected, got $actual"
    fi
}

private_tip20_balance() {
    local auth="$1"
    local account="$2"
    local token="$3"
    local data response raw
    data="$(cast calldata "balanceOf(address)" "$account")"
    response="$(private_rpc "$auth" "{\"jsonrpc\":\"2.0\",\"method\":\"eth_call\",\"params\":[{\"from\":\"$account\",\"to\":\"$token\",\"data\":\"$data\"},\"latest\"],\"id\":1}")"
    require_json_rpc_result "private balance eth_call" "$response"
    raw="$(echo "$response" | jq -r '.result')"
    rpc_hex_to_dec "$raw"
}

assert_public_balance() {
    local token="$1"
    local account="$2"
    local expected="$3"
    local label="$4"
    local actual
    actual="$(tip20_balance "$ZONE_RPC_URL" "$token" "$account")"
    assert_uint_equal "$label public balance" "$actual" "$expected"
    echo "  $label public balance:  $actual"
}

assert_private_balance() {
    local auth="$1"
    local account="$2"
    local token="$3"
    local expected="$4"
    local label="$5"
    local actual
    actual="$(private_tip20_balance "$auth" "$account" "$token")"
    assert_uint_equal "$label private balance" "$actual" "$expected"
    echo "  $label private balance: $actual"
}

zone_path_fee_paid() {
    local tx_hash="$1"
    local account="$2"
    local receipt from_topic fee_topic token total data amount
    receipt="$(cast receipt "$tx_hash" --rpc-url "$ZONE_RPC_URL" --json)"
    from_topic="$(address_topic "$account")"
    fee_topic="$(address_topic "$FEE_RECIPIENT")"
    token="$(lower_hex "$PATHUSD")"
    total=0
    while IFS= read -r data; do
        [[ -n "$data" ]] || continue
        amount="$(normalize_uint "$data")"
        total=$((total + amount))
    done < <(
        echo "$receipt" | jq -r \
            --arg token "$token" \
            --arg topic "$TRANSFER_TOPIC" \
            --arg from "$from_topic" \
            --arg fee "$fee_topic" \
            '.logs[]
             | select((.address | ascii_downcase) == $token)
             | select(.topics[0] == $topic)
             | select((.topics[1] | ascii_downcase) == $from)
             | select((.topics[2] | ascii_downcase) == $fee)
             | .data'
    )
    echo "$total"
}

validate_top_of_book_level() {
    local response="$1"
    local side="$2"
    local level_type price quantity
    level_type="$(echo "$response" | jq -r ".result.$side | type")"
    if [[ "$level_type" == "null" ]]; then
        echo "price=0 quantity=0"
        return
    fi
    if [[ "$level_type" != "object" ]]; then
        fail "zone_getTopOfBook $side must be an object or null, got $level_type"
    fi

    price="$(json_required_quantity "$response" ".result.$side.price" "zone_getTopOfBook $side.price")"
    quantity="$(json_required_quantity "$response" ".result.$side.quantity" "zone_getTopOfBook $side.quantity")"
    if (( price == 0 || quantity == 0 )); then
        fail "zone_getTopOfBook $side has non-null level with zero price or quantity"
    fi
    echo "price=$price quantity=$quantity"
}

print_order_status() {
    local label="$1"
    local account="$2"
    local auth="$3"
    local tx_hash="${4:-}"
    local top_of_book bid_level ask_level as_of_block min_block darkpool_path darkpool_alpha base quote
    local top_of_book_error="" top_of_book_skipped=false

    top_of_book="$(private_zone_call "$auth" "zone_getTopOfBook" "[{\"base\":\"$ALPHAUSD\",\"quote\":\"$PATHUSD\"}]")"
    top_of_book_error="$(echo "$top_of_book" | jq -r '.error.message // empty' 2>/dev/null || true)"
    if [[ -n "$top_of_book_error" ]]; then
        if [[ "$top_of_book_error" == unsupported\ pair* ]]; then
            top_of_book_skipped=true
        else
            echo "$top_of_book" | jq . >&2 2>/dev/null || echo "$top_of_book" >&2
            fail "zone_getTopOfBook failed: $top_of_book_error"
        fi
    else
        require_json_rpc_result "zone_getTopOfBook" "$top_of_book"
        if ! echo "$top_of_book" | jq -e '.result | has("bid") and has("ask") and has("midpoint") and has("spread")' >/dev/null; then
            fail "zone_getTopOfBook result is missing expected book fields"
        fi
        base="$(json_required_address "$top_of_book" ".result.base" "zone_getTopOfBook base")"
        quote="$(json_required_address "$top_of_book" ".result.quote" "zone_getTopOfBook quote")"
        assert_address_equal "zone_getTopOfBook base" "$base" "$ALPHAUSD"
        assert_address_equal "zone_getTopOfBook quote" "$quote" "$PATHUSD"
        as_of_block="$(json_required_quantity "$top_of_book" ".result.asOfBlock" "zone_getTopOfBook asOfBlock")"
        if (( as_of_block == 0 )); then
            fail "zone_getTopOfBook asOfBlock must be non-zero after $label"
        fi
        if [[ -n "$tx_hash" ]]; then
            min_block="$(json_required_quantity "$(cast receipt "$tx_hash" --rpc-url "$ZONE_RPC_URL" --json)" ".blockNumber" "$label blockNumber")"
            if (( as_of_block < min_block )); then
                fail "zone_getTopOfBook asOfBlock=$as_of_block is older than $label block=$min_block"
            fi
        fi
        bid_level="$(validate_top_of_book_level "$top_of_book" "bid")"
        ask_level="$(validate_top_of_book_level "$top_of_book" "ask")"
    fi
    darkpool_path="$(cast call "$DARKPOOL" "balanceOf(address,address)(uint128)" "$account" "$PATHUSD" \
        --rpc-url "$ZONE_RPC_URL" --from "$account")"
    darkpool_alpha="$(cast call "$DARKPOOL" "balanceOf(address,address)(uint128)" "$account" "$ALPHAUSD" \
        --rpc-url "$ZONE_RPC_URL" --from "$account")"

    echo "  status after $label:"
    if [[ "$top_of_book_skipped" == "true" ]]; then
        echo "    zone_getTopOfBook: skipped ($top_of_book_error)"
    else
        echo "    zone_getTopOfBook asOfBlock: $as_of_block"
        echo "    zone_getTopOfBook bid: $bid_level"
        echo "    zone_getTopOfBook ask: $ask_level"
    fi
    echo "    darkpool pathUSD balance:  $(normalize_uint "$darkpool_path")"
    echo "    darkpool alphaUSD balance: $(normalize_uint "$darkpool_alpha")"
}

print_specific_order_status() {
    local label="$1"
    local tx_hash="$2"
    local auth="$3"
    local expected_side="$4"
    local expected_amount="$5"
    local expected_price="$6"
    local receipt submitted_topic placed_topic matched_topic darkpool_lower
    local submitted_id="" submitted_amount="" submitted_price="" submitted_side=""
    local placed_amount="0" placed_price="" matched_amount="0" matched_price="" status=""

    submitted_topic="$(cast keccak "OrderSubmitted(uint128,address,address,address,uint128,uint128,bool)")"
    placed_topic="$(cast keccak "OrderPlaced(uint128,address,address,address,uint128,uint128,bool)")"
    matched_topic="$(cast keccak "OrderMatched(uint128,uint128,address,address,uint128,uint128)")"
    darkpool_lower="$(echo "$DARKPOOL" | tr '[:upper:]' '[:lower:]')"
    receipt="$(cast receipt "$tx_hash" --rpc-url "$ZONE_RPC_URL" --json)"

    while IFS= read -r log_entry; do
        local topic0 data order_id taker_order_id amount price is_bid
        topic0="$(echo "$log_entry" | jq -r '.topics[0]')"
        data="$(echo "$log_entry" | jq -r '.data')"

        case "$topic0" in
            "$submitted_topic")
                order_id="$(topic_uint "$(echo "$log_entry" | jq -r '.topics[1]')")"
                submitted_id="$order_id"
                submitted_amount="$(normalize_uint "$(data_word "$data" 2)")"
                submitted_price="$(normalize_uint "$(data_word "$data" 3)")"
                is_bid="$(word_bool "$(data_word "$data" 4)")"
                if [[ "$is_bid" == "true" ]]; then
                    submitted_side="buy"
                else
                    submitted_side="sell"
                fi
                ;;
            "$placed_topic")
                order_id="$(topic_uint "$(echo "$log_entry" | jq -r '.topics[1]')")"
                if [[ -n "$submitted_id" && "$order_id" == "$submitted_id" ]]; then
                    placed_amount="$(normalize_uint "$(data_word "$data" 2)")"
                    placed_price="$(normalize_uint "$(data_word "$data" 3)")"
                fi
                ;;
            "$matched_topic")
                taker_order_id="$(topic_uint "$(echo "$log_entry" | jq -r '.topics[2]')")"
                if [[ -n "$submitted_id" && "$taker_order_id" == "$submitted_id" ]]; then
                    amount="$(normalize_uint "$(data_word "$data" 1)")"
                    price="$(normalize_uint "$(data_word "$data" 2)")"
                    matched_amount=$((matched_amount + amount))
                    matched_price="$price"
                fi
                ;;
        esac
    done < <(echo "$receipt" | jq -c --arg addr "$darkpool_lower" '.logs[] | select((.address | ascii_downcase) == $addr)')

    if [[ -z "$submitted_id" ]]; then
        fail "specific $label status: no OrderSubmitted event found in $tx_hash"
    fi
    if [[ "$submitted_side" != "$expected_side" ]]; then
        fail "$label submitted side mismatch: expected $expected_side, got $submitted_side"
    fi
    if (( submitted_amount != expected_amount )); then
        fail "$label submitted amount mismatch: expected $expected_amount, got $submitted_amount"
    fi
    if (( submitted_price != expected_price )); then
        fail "$label submitted price mismatch: expected $expected_price, got $submitted_price"
    fi

    local expected_status expected_remaining expected_filled
    expected_filled="$matched_amount"
    expected_remaining="$placed_amount"
    if (( matched_amount + placed_amount != submitted_amount )); then
        fail "$label event accounting mismatch: submitted=$submitted_amount matched=$matched_amount resting=$placed_amount"
    fi

    if (( matched_amount > 0 && placed_amount > 0 )); then
        status="partially filled; residual resting"
        expected_status="partiallyFilled"
    elif (( matched_amount > 0 )); then
        status="filled"
        expected_status="filled"
    elif (( placed_amount > 0 )); then
        status="resting open"
        expected_status="open"
    else
        fail "$label emitted OrderSubmitted without OrderPlaced or OrderMatched"
    fi

    echo "  specific $label status:"
    echo "    tx:             $tx_hash"
    echo "    order id:       $submitted_id"
    echo "    side:           $submitted_side"
    echo "    submitted:      amount=$submitted_amount price=$submitted_price"
    echo "    matched amount: $matched_amount${matched_price:+ at price=$matched_price}"
    echo "    resting amount: $placed_amount${placed_price:+ at price=$placed_price}"
    echo "    status:         $status"

    local order_hex zone_order order_id order_side order_status order_amount order_price order_remaining order_filled order_base order_quote
    order_hex="$(printf '0x%x' "$submitted_id")"
    zone_order="$(private_zone_call "$auth" "zone_getOrder" "[\"$order_hex\"]")"
    require_json_rpc_result "zone_getOrder($order_hex)" "$zone_order"
    order_id="$(json_quantity "$(echo "$zone_order" | jq -r '.result.orderId')")"
    order_side="$(echo "$zone_order" | jq -r '.result.side')"
    order_status="$(echo "$zone_order" | jq -r '.result.status')"
    order_amount="$(json_required_quantity "$zone_order" ".result.amount" "zone_getOrder($order_hex).amount")"
    order_price="$(json_required_quantity "$zone_order" ".result.price" "zone_getOrder($order_hex).price")"
    order_remaining="$(json_required_quantity "$zone_order" ".result.remaining" "zone_getOrder($order_hex).remaining")"
    order_filled="$(json_required_quantity "$zone_order" ".result.filled" "zone_getOrder($order_hex).filled")"
    order_base="$(json_required_address "$zone_order" ".result.baseToken" "zone_getOrder($order_hex).baseToken")"
    order_quote="$(json_required_address "$zone_order" ".result.quoteToken" "zone_getOrder($order_hex).quoteToken")"
    assert_uint_equal "zone_getOrder($order_hex) orderId" "$order_id" "$submitted_id"
    if [[ "$expected_side" == "buy" && "$order_side" != "bid" ]]; then
        fail "zone_getOrder($order_hex) returned side=$order_side, expected bid"
    fi
    if [[ "$expected_side" == "sell" && "$order_side" != "ask" ]]; then
        fail "zone_getOrder($order_hex) returned side=$order_side, expected ask"
    fi
    if [[ "$order_status" != "$expected_status" ]]; then
        fail "zone_getOrder($order_hex) returned status=$order_status, expected $expected_status"
    fi
    assert_address_equal "zone_getOrder($order_hex) baseToken" "$order_base" "$ALPHAUSD"
    assert_address_equal "zone_getOrder($order_hex) quoteToken" "$order_quote" "$PATHUSD"
    assert_uint_equal "zone_getOrder($order_hex) amount" "$order_amount" "$submitted_amount"
    assert_uint_equal "zone_getOrder($order_hex) price" "$order_price" "$submitted_price"
    assert_uint_equal "zone_getOrder($order_hex) remaining" "$order_remaining" "$expected_remaining"
    assert_uint_equal "zone_getOrder($order_hex) filled" "$order_filled" "$expected_filled"
    echo "    zone_getOrder: status=$order_status remaining=$order_remaining filled=$order_filled"
}

main() {
    require_cmd cast
    require_cmd jq
    require_cmd curl
    load_zone_metadata

    local private_key account
    private_key="$(wallet_private_key)"
    account="$(cast wallet address "$private_key")"
    local auth
    auth="$(build_auth_token "$private_key")"

    log "Configuration"
    echo "  account:        $account"
    echo "  account file:   $ACCOUNT_FILE"
    echo "  L1 RPC:         $HTTP_L1_RPC"
    echo "  zone RPC:       $ZONE_RPC_URL"
    echo "  private RPC:    $PRIVATE_ZONE_RPC_URL"
    echo "  portal:         $L1_PORTAL_ADDRESS"
    echo "  pathUSD:        $PATHUSD amount=$PATHUSD_AMOUNT"
    echo "  alphaUSD:       $ALPHAUSD amount=$ALPHAUSD_AMOUNT"
    echo "  orders:         sell $ORDER_AMOUNT @ $SELL_PRICE, buy $ORDER_AMOUNT @ $BUY_PRICE"
    echo "  L1 settlement:  $VERIFY_L1_WITHDRAWAL_SETTLEMENT"
    echo "  gas fallbacks:  approve=$APPROVE_GAS_FALLBACK deposit=$DEPOSIT_GAS_FALLBACK order=$ORDER_GAS_FALLBACK withdraw=$WITHDRAW_GAS_FALLBACK"

    local required_path_available required_alpha_available
    required_path_available=$((ORDER_AMOUNT * BUY_PRICE + WITHDRAW_PATHUSD_AMOUNT))
    required_alpha_available=$((ORDER_AMOUNT + WITHDRAW_ALPHAUSD_AMOUNT))
    if (( PATHUSD_AMOUNT < required_path_available )); then
        echo "ERROR: PATHUSD_AMOUNT must cover buy escrow plus withdrawal." >&2
        echo "       Need at least $required_path_available, got $PATHUSD_AMOUNT." >&2
        exit 1
    fi
    if (( ALPHAUSD_AMOUNT < required_alpha_available )); then
        echo "ERROR: ALPHAUSD_AMOUNT must cover sell escrow plus withdrawal." >&2
        echo "       Need at least $required_alpha_available, got $ALPHAUSD_AMOUNT." >&2
        exit 1
    fi

    ensure_portal_token_enabled "$PATHUSD" "pathUSD"
    ensure_portal_token_enabled "$ALPHAUSD" "alphaUSD"

    log "Requesting faucet funds"
    cast rpc tempo_fundAddress "$account" --rpc-url "$HTTP_L1_RPC" || true

    local l1_path l1_alpha
    l1_path="$(tip20_balance "$HTTP_L1_RPC" "$PATHUSD" "$account")"
    l1_alpha="$(tip20_balance "$HTTP_L1_RPC" "$ALPHAUSD" "$account")"
    echo "  L1 pathUSD:  $l1_path"
    echo "  L1 alphaUSD: $l1_alpha"
    if (( l1_path < PATHUSD_AMOUNT || l1_alpha < ALPHAUSD_AMOUNT )); then
        echo "ERROR: account lacks deposit funds after faucet request." >&2
        echo "       Need pathUSD=$PATHUSD_AMOUNT and alphaUSD=$ALPHAUSD_AMOUNT." >&2
        echo "       If this faucet does not mint alphaUSD, pre-fund $account and rerun." >&2
        exit 1
    fi

    local zone_path_before zone_alpha_before target_path target_alpha
    zone_path_before="$(tip20_balance "$ZONE_RPC_URL" "$PATHUSD" "$account" 2>/dev/null || echo 0)"
    zone_alpha_before="$(tip20_balance "$ZONE_RPC_URL" "$ALPHAUSD" "$account" 2>/dev/null || echo 0)"
    target_path=$((zone_path_before + PATHUSD_AMOUNT))
    target_alpha=$((zone_alpha_before + ALPHAUSD_AMOUNT))

    log "Approving portal"
    local path_approval_tx alpha_approval_tx
    local path_approval_gas alpha_approval_gas
    path_approval_gas="$(buffered_gas_limit "$APPROVE_GAS_FALLBACK" "$HTTP_L1_RPC" "$account" \
        "$PATHUSD" "approve(address,uint256)" "$L1_PORTAL_ADDRESS" "$(cast max-uint)")"
    alpha_approval_gas="$(buffered_gas_limit "$APPROVE_GAS_FALLBACK" "$HTTP_L1_RPC" "$account" \
        "$ALPHAUSD" "approve(address,uint256)" "$L1_PORTAL_ADDRESS" "$(cast max-uint)")"
    path_approval_tx="$(send_checked "pathUSD portal approval" \
        cast send "$PATHUSD" "approve(address,uint256)" "$L1_PORTAL_ADDRESS" "$(cast max-uint)" \
        --rpc-url "$HTTP_L1_RPC" --private-key "$private_key" --gas-limit "$path_approval_gas")"
    alpha_approval_tx="$(send_checked "alphaUSD portal approval" \
        cast send "$ALPHAUSD" "approve(address,uint256)" "$L1_PORTAL_ADDRESS" "$(cast max-uint)" \
        --rpc-url "$HTTP_L1_RPC" --private-key "$private_key" --gas-limit "$alpha_approval_gas")"
    echo "  pathUSD approval tx:  $path_approval_tx (gas $path_approval_gas)"
    echo "  alphaUSD approval tx: $alpha_approval_tx (gas $alpha_approval_gas)"

    log "Depositing to zone"
    local memo="0x0000000000000000000000000000000000000000000000000000000000000000"
    local path_deposit_tx alpha_deposit_tx
    local path_deposit_gas alpha_deposit_gas
    path_deposit_gas="$(buffered_gas_limit "$DEPOSIT_GAS_FALLBACK" "$HTTP_L1_RPC" "$account" \
        "$L1_PORTAL_ADDRESS" "deposit(address,address,uint128,bytes32)" "$PATHUSD" "$account" "$PATHUSD_AMOUNT" "$memo")"
    alpha_deposit_gas="$(buffered_gas_limit "$DEPOSIT_GAS_FALLBACK" "$HTTP_L1_RPC" "$account" \
        "$L1_PORTAL_ADDRESS" "deposit(address,address,uint128,bytes32)" "$ALPHAUSD" "$account" "$ALPHAUSD_AMOUNT" "$memo")"
    path_deposit_tx="$(send_checked "pathUSD deposit" \
        cast send "$L1_PORTAL_ADDRESS" "deposit(address,address,uint128,bytes32)" "$PATHUSD" "$account" "$PATHUSD_AMOUNT" "$memo" \
        --rpc-url "$HTTP_L1_RPC" --private-key "$private_key" --gas-limit "$path_deposit_gas")"
    alpha_deposit_tx="$(send_checked "alphaUSD deposit" \
        cast send "$L1_PORTAL_ADDRESS" "deposit(address,address,uint128,bytes32)" "$ALPHAUSD" "$account" "$ALPHAUSD_AMOUNT" "$memo" \
        --rpc-url "$HTTP_L1_RPC" --private-key "$private_key" --gas-limit "$alpha_deposit_gas")"
    echo "  pathUSD deposit tx:  $path_deposit_tx (gas $path_deposit_gas)"
    echo "  alphaUSD deposit tx: $alpha_deposit_tx (gas $alpha_deposit_gas)"

    log "Waiting for zone deposit balances"
    wait_for_zone_balance_at_least "$PATHUSD" "$account" "$target_path" "pathUSD"
    wait_for_zone_balance_at_least "$ALPHAUSD" "$account" "$target_alpha" "alphaUSD"

    log "Placing darkpool orders"
    local sell_tx buy_tx
    local sell_gas buy_gas
    local zone_path_fees=0
    sell_gas="$(buffered_gas_limit "$ORDER_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$DARKPOOL" "place(address,uint128,uint128,bool)" "$ALPHAUSD" "$ORDER_AMOUNT" "$SELL_PRICE" false)"
    sell_tx="$(send_checked "sell order" \
        cast send "$DARKPOOL" "place(address,uint128,uint128,bool)" "$ALPHAUSD" "$ORDER_AMOUNT" "$SELL_PRICE" false \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$sell_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$sell_tx" "$account")))
    echo "  sell tx: $sell_tx (gas $sell_gas)"
    print_specific_order_status "sell order" "$sell_tx" "$auth" "sell" "$ORDER_AMOUNT" "$SELL_PRICE"
    print_order_status "sell order" "$account" "$auth" "$sell_tx"

    buy_gas="$(buffered_gas_limit "$ORDER_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$DARKPOOL" "place(address,uint128,uint128,bool)" "$ALPHAUSD" "$ORDER_AMOUNT" "$BUY_PRICE" true)"
    buy_tx="$(send_checked "buy order" \
        cast send "$DARKPOOL" "place(address,uint128,uint128,bool)" "$ALPHAUSD" "$ORDER_AMOUNT" "$BUY_PRICE" true \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$buy_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$buy_tx" "$account")))
    echo "  buy tx:  $buy_tx (gas $buy_gas)"
    print_specific_order_status "buy order" "$buy_tx" "$auth" "buy" "$ORDER_AMOUNT" "$BUY_PRICE"
    print_order_status "buy order" "$account" "$auth" "$buy_tx"

    log "Approving outbox and withdrawing half of deposited amounts"
    local path_outbox_approval_tx alpha_outbox_approval_tx
    local path_outbox_approval_gas alpha_outbox_approval_gas
    path_outbox_approval_gas="$(buffered_gas_limit "$APPROVE_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$PATHUSD" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)")"
    alpha_outbox_approval_gas="$(buffered_gas_limit "$APPROVE_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$ALPHAUSD" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)")"
    path_outbox_approval_tx="$(send_checked "pathUSD outbox approval" \
        cast send "$PATHUSD" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)" \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$path_outbox_approval_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$path_outbox_approval_tx" "$account")))
    alpha_outbox_approval_tx="$(send_checked "alphaUSD outbox approval" \
        cast send "$ALPHAUSD" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)" \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$alpha_outbox_approval_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$alpha_outbox_approval_tx" "$account")))
    echo "  pathUSD outbox approval tx:  $path_outbox_approval_tx (gas $path_outbox_approval_gas)"
    echo "  alphaUSD outbox approval tx: $alpha_outbox_approval_tx (gas $alpha_outbox_approval_gas)"

    local withdraw_l1_from_block l1_path_before_withdraw l1_alpha_before_withdraw
    withdraw_l1_from_block="$(cast block-number --rpc-url "$HTTP_L1_RPC")"
    l1_path_before_withdraw="$(tip20_balance "$HTTP_L1_RPC" "$PATHUSD" "$account")"
    l1_alpha_before_withdraw="$(tip20_balance "$HTTP_L1_RPC" "$ALPHAUSD" "$account")"

    local path_withdraw_tx alpha_withdraw_tx
    local path_withdraw_gas alpha_withdraw_gas
    path_withdraw_gas="$(buffered_gas_limit "$WITHDRAW_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$OUTBOX" "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)" \
        "$PATHUSD" "$account" "$WITHDRAW_PATHUSD_AMOUNT" "$memo" 0 "$account" "0x" "0x")"
    alpha_withdraw_gas="$(buffered_gas_limit "$WITHDRAW_GAS_FALLBACK" "$ZONE_RPC_URL" "$account" \
        "$OUTBOX" "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)" \
        "$ALPHAUSD" "$account" "$WITHDRAW_ALPHAUSD_AMOUNT" "$memo" 0 "$account" "0x" "0x")"
    path_withdraw_tx="$(send_checked "pathUSD withdrawal" \
        cast send "$OUTBOX" "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)" \
        "$PATHUSD" "$account" "$WITHDRAW_PATHUSD_AMOUNT" "$memo" 0 "$account" "0x" "0x" \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$path_withdraw_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$path_withdraw_tx" "$account")))
    alpha_withdraw_tx="$(send_checked "alphaUSD withdrawal" \
        cast send "$OUTBOX" "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)" \
        "$ALPHAUSD" "$account" "$WITHDRAW_ALPHAUSD_AMOUNT" "$memo" 0 "$account" "0x" "0x" \
        --rpc-url "$ZONE_RPC_URL" --private-key "$private_key" --gas-limit "$alpha_withdraw_gas")"
    zone_path_fees=$((zone_path_fees + $(zone_path_fee_paid "$alpha_withdraw_tx" "$account")))
    echo "  pathUSD withdrawal tx:  $path_withdraw_tx (gas $path_withdraw_gas)"
    echo "  alphaUSD withdrawal tx: $alpha_withdraw_tx (gas $alpha_withdraw_gas)"
    echo "  pathUSD zone tx fees:  $zone_path_fees"

    if [[ "$VERIFY_L1_WITHDRAWAL_SETTLEMENT" != "0" ]]; then
        log "Waiting for L1 withdrawal settlement"
        local expected_l1_path expected_l1_alpha
        expected_l1_path=$((l1_path_before_withdraw + WITHDRAW_PATHUSD_AMOUNT))
        expected_l1_alpha=$((l1_alpha_before_withdraw + WITHDRAW_ALPHAUSD_AMOUNT))
        echo "  monitoring portal from L1 block: $withdraw_l1_from_block"
        wait_for_l1_balance_at_least "$PATHUSD" "$account" "$expected_l1_path" "pathUSD"
        wait_for_l1_balance_at_least "$ALPHAUSD" "$account" "$expected_l1_alpha" "alphaUSD"
        wait_for_withdrawal_processed "$withdraw_l1_from_block" "$PATHUSD" "$account" "$WITHDRAW_PATHUSD_AMOUNT" "pathUSD"
        wait_for_withdrawal_processed "$withdraw_l1_from_block" "$ALPHAUSD" "$account" "$WITHDRAW_ALPHAUSD_AMOUNT" "alphaUSD"
    fi

    log "Final public zone status"
    local expected_path_final expected_alpha_final
    expected_path_final=$((target_path - ORDER_AMOUNT * BUY_PRICE - WITHDRAW_PATHUSD_AMOUNT - zone_path_fees))
    expected_alpha_final=$((target_alpha - ORDER_AMOUNT - WITHDRAW_ALPHAUSD_AMOUNT))
    assert_public_balance "$PATHUSD" "$account" "$expected_path_final" "pathUSD"
    assert_public_balance "$ALPHAUSD" "$account" "$expected_alpha_final" "alphaUSD"

    log "Private RPC account data"
    local info
    info="$(private_rpc "$auth" '{"jsonrpc":"2.0","method":"zone_getAuthorizationTokenInfo","params":[],"id":1}')"
    require_json_rpc_result "zone_getAuthorizationTokenInfo" "$info"
    echo "$info" | jq .
    assert_private_balance "$auth" "$account" "$PATHUSD" "$expected_path_final" "pathUSD"
    assert_private_balance "$auth" "$account" "$ALPHAUSD" "$expected_alpha_final" "alphaUSD"

    log "Done"
}

main "$@"
