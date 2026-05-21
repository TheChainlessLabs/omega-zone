"use client";

import Link from "next/link";
import { useMemo, useRef, useState } from "react";
import {
  useAccount,
  useConnections,
  usePublicClient,
  useReadContract,
  useSwitchChain,
} from "wagmi";
import {
  AlertCircle,
  ArrowLeft,
  ArrowRightLeft,
  BookOpen,
  Check,
  Coins,
  Loader2,
  RefreshCw,
  Search,
  ShieldCheck,
} from "lucide-react";
import {
  decodeEventLog,
  decodeFunctionResult,
  encodeFunctionData,
  formatUnits,
  parseUnits,
  toFunctionSelector,
  type Address,
  type Hex,
  type Log,
  type TransactionReceipt,
} from "viem";
import { Account as TempoAccount, Transaction as TempoTransaction } from "viem/tempo";
import { zoneChain } from "@/lib/config";
import { BRIDGE_TOKENS, TIP20_ABI, type BridgeToken } from "@/lib/portal-abi";
import {
  DARKPOOL_ADDRESS,
  DARKPOOL_ORDERBOOK_ABI,
} from "@/lib/orderbook-abi";
import {
  type CachedZoneAuthToken,
  type ZoneAuthProvider,
  ZONE_ID,
  zonePrivateRpc,
} from "@/lib/zone-auth";
import { WalletConnect } from "@/components/wallet-connect";

type OrderView = {
  orderId: bigint;
  maker: Address;
  base: Address;
  quote: Address;
  isBid: boolean;
  price: bigint;
  quantity: bigint;
};

type ActivityItem = {
  id: string;
  label: string;
  detail: string;
  status: "success" | "pending" | "error";
};

type OrderFill = {
  amountFilled: bigint;
  price: bigint;
  maker: Address;
  taker: Address;
  transactionHash?: Hex;
  blockNumber?: bigint;
  logIndex?: number;
};

type OrderHistory = {
  orderId: bigint;
  maker?: Address;
  base?: Address;
  quote?: Address;
  isBid?: boolean;
  placedAmount?: bigint;
  placedPrice?: bigint;
  cancelled: boolean;
  fills: OrderFill[];
};

type UtilityTokenKey = "base" | "quote";
type StoredAccessKeyScope = {
  address: Address;
  selector?: Hex | string;
};
type StoredAccessKeyLimit = {
  token: Address;
  limit?: bigint | number | string;
  amount?: bigint | number | string;
};
type StoredDarkpoolAccessKey = NonNullable<
  NonNullable<ZoneAuthProvider["store"]>["getState"] extends () => infer State
    ? State extends { accessKeys?: readonly (infer Key)[] }
      ? Key
      : never
    : never
> & {
  keyPair: Parameters<typeof TempoAccount.fromWebCryptoP256>[0];
};

const BASE_TOKENS = BRIDGE_TOKENS.filter((token) => token.id !== "pathusd");
const DARKPOOL_ACCESS_KEY_TTL_SECS = 24 * 60 * 60;
const ACCESS_KEY_REFRESH_BUFFER_SECS = 60;
const ACCESS_KEY_AUTHORIZATION_GAS_OVERHEAD = 16_000_000n;
const UINT128_MAX = (1n << 128n) - 1n;
const GAS_ESTIMATE_BUFFER_BPS = 3_000n;
const GAS_ESTIMATE_FIXED_BUFFER = 50_000n;
const ORDER_DOES_NOT_EXIST_SELECTOR = "0x5dcaf2d7";
const UNAUTHORIZED_SELECTOR = "0x82b42900";
const DARKPOOL_ACCESS_KEY_SCOPES = [
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("deposit(address,uint128)") },
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("withdraw(address,uint128)") },
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("place(address,uint128,uint128,bool)") },
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("cancel(uint128)") },
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("marketBuy(address,uint128,uint128)") },
  { address: DARKPOOL_ADDRESS, selector: toFunctionSelector("marketSell(address,uint128,uint128)") },
] as const;

function emptyOrderHistory(orderId: bigint): OrderHistory {
  return {
    orderId,
    cancelled: false,
    fills: [],
  };
}

function mergeOrderHistory(
  current: OrderHistory | undefined,
  incoming: OrderHistory,
) {
  const merged: OrderHistory = {
    ...current,
    ...incoming,
    orderId: incoming.orderId,
    maker: current?.maker ?? incoming.maker,
    base: current?.base ?? incoming.base,
    quote: current?.quote ?? incoming.quote,
    isBid: current?.isBid ?? incoming.isBid,
    placedAmount: current?.placedAmount ?? incoming.placedAmount,
    placedPrice: current?.placedPrice ?? incoming.placedPrice,
    cancelled: Boolean(current?.cancelled || incoming.cancelled),
    fills: [],
  };
  const fillsByKey = new Map<string, OrderFill>();
  for (const fill of [...(current?.fills ?? []), ...incoming.fills]) {
    const key =
      fill.transactionHash && fill.logIndex != null
        ? `${fill.transactionHash}-${fill.logIndex}`
        : `${fill.maker}-${fill.taker}-${fill.amountFilled}-${fill.price}`;
    fillsByKey.set(key, fill);
  }
  merged.fills = [...fillsByKey.values()].sort((a, b) => {
    const aBlock = a.blockNumber ?? 0n;
    const bBlock = b.blockNumber ?? 0n;
    if (aBlock !== bBlock) return aBlock < bBlock ? -1 : 1;
    return (a.logIndex ?? 0) - (b.logIndex ?? 0);
  });
  return merged;
}

function parseOrderHistoriesFromLogs(logs: readonly Log[]) {
  const histories = new Map<bigint, OrderHistory>();

  for (const log of logs) {
    if (log.address.toLowerCase() !== DARKPOOL_ADDRESS.toLowerCase()) continue;

    try {
      const decoded = decodeEventLog({
        abi: DARKPOOL_ORDERBOOK_ABI,
        data: log.data,
        topics: log.topics,
      });

      if (
        decoded.eventName === "OrderSubmitted" ||
        decoded.eventName === "OrderPlaced"
      ) {
        const orderId = decoded.args.orderId;
        const history = mergeOrderHistory(histories.get(orderId), {
          orderId,
          maker: decoded.args.maker,
          base: decoded.args.base,
          quote: decoded.args.quote,
          isBid: decoded.args.isBid,
          placedAmount: decoded.args.amount,
          placedPrice: decoded.args.price,
          cancelled: false,
          fills: [],
        });
        histories.set(orderId, history);
      }

      if (decoded.eventName === "OrderFilled") {
        const orderId = decoded.args.orderId;
        const current = histories.get(orderId) ?? emptyOrderHistory(orderId);
        histories.set(
          orderId,
          mergeOrderHistory(current, {
            orderId,
            maker: decoded.args.maker,
            cancelled: false,
            fills: [
              {
                amountFilled: decoded.args.amountFilled,
                price: decoded.args.price,
                maker: decoded.args.maker,
                taker: decoded.args.taker,
                transactionHash: log.transactionHash ?? undefined,
                blockNumber: log.blockNumber ?? undefined,
                logIndex: log.logIndex ?? undefined,
              },
            ],
          }),
        );
      }

      if (decoded.eventName === "OrderMatched") {
        const orderId = decoded.args.takerOrderId;
        const current = histories.get(orderId) ?? emptyOrderHistory(orderId);
        histories.set(
          orderId,
          mergeOrderHistory(current, {
            orderId,
            maker: decoded.args.taker,
            cancelled: false,
            fills: [
              {
                amountFilled: decoded.args.amountFilled,
                price: decoded.args.price,
                maker: decoded.args.maker,
                taker: decoded.args.taker,
                transactionHash: log.transactionHash ?? undefined,
                blockNumber: log.blockNumber ?? undefined,
                logIndex: log.logIndex ?? undefined,
              },
            ],
          }),
        );
      }

      if (decoded.eventName === "OrderCancelled") {
        const orderId = decoded.args.orderId;
        const current = histories.get(orderId) ?? emptyOrderHistory(orderId);
        histories.set(
          orderId,
          mergeOrderHistory(current, {
            orderId,
            maker: decoded.args.maker,
            cancelled: true,
            fills: [],
          }),
        );
      }
    } catch {
      continue;
    }
  }

  return histories;
}

function totalFilled(history: OrderHistory | null | undefined) {
  return history?.fills.reduce((total, fill) => total + fill.amountFilled, 0n) ?? 0n;
}

function orderHistoryStatus(
  history: OrderHistory | null | undefined,
  restingOrder: OrderView | null,
) {
  if (restingOrder) {
    return totalFilled(history) > 0n ? "Partially filled" : "Open";
  }
  if (!history) return "Not found";
  if (history.cancelled) return "Cancelled";
  if (
    history.placedAmount != null &&
    history.fills.length > 0 &&
    totalFilled(history) >= history.placedAmount
  ) {
    return "Filled";
  }
  if (history.fills.length > 0) return "Partially filled";
  return "No longer resting";
}

function orderHistorySummary(history: OrderHistory) {
  const side = history.isBid == null ? "order" : history.isBid ? "bid" : "ask";
  const filled = totalFilled(history);
  const amount =
    history.placedAmount == null
      ? `${filled.toString()} filled`
      : `${filled.toString()} / ${history.placedAmount.toString()} filled`;
  return `#${history.orderId.toString()} ${side}: ${amount} at raw ${history.placedPrice?.toString() ?? history.fills[0]?.price.toString() ?? "--"}`;
}

function formatOrderSide(isBid: boolean | undefined) {
  if (isBid == null) return "Order";
  return isBid ? "Bid" : "Ask";
}

function formatTokenAmount(value: bigint | undefined, decimals: number) {
  if (value == null) return "--";
  return Number(formatUnits(value, decimals)).toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

function formatRawPrice(value: bigint | undefined) {
  if (value == null) return "--";
  return value.toString();
}

function parseAmountInput(value: string, decimals: number) {
  if (!value) return 0n;
  return parseUnits(value, decimals);
}

function parseUint128Integer(value: string) {
  if (!value) return 0n;
  if (!/^\d+$/.test(value)) {
    throw new Error("Enter a whole-number uint128 value");
  }
  return BigInt(value);
}

function ensureSufficientAmount(
  required: bigint,
  available: bigint | undefined,
  label: string,
) {
  if (available == null) {
    throw new Error(`${label} is still loading. Wait for balances to finish loading and try again.`);
  }
  if (available < required) {
    throw new Error(`Insufficient ${label.toLowerCase()}`);
  }
}

function darkpoolGasLimit(
  functionName:
    | "deposit"
    | "withdraw"
    | "createPair"
    | "place"
    | "cancel"
    | "marketBuy"
    | "marketSell",
) {
  switch (functionName) {
    case "createPair":
      return 250_000n;
    case "cancel":
      return 350_000n;
    case "deposit":
    case "withdraw":
      return 600_000n;
    case "place":
      return 4_000_000n;
    case "marketBuy":
    case "marketSell":
      return 5_000_000n;
  }
}

function gasWithBuffer(estimated: bigint) {
  return estimated + (estimated * GAS_ESTIMATE_BUFFER_BPS) / 10_000n + GAS_ESTIMATE_FIXED_BUFFER;
}

function findToken(address: Address | undefined) {
  if (!address) return null;
  return (
    BRIDGE_TOKENS.find(
      (token) => token.address.toLowerCase() === address.toLowerCase(),
    ) ?? null
  );
}

function shortHex(value: string | undefined) {
  if (!value) return "--";
  return `${value.slice(0, 6)}...${value.slice(-4)}`;
}

function formatHistoryAmount(
  value: bigint | undefined,
  tokenAddress: Address | undefined,
  fallbackDecimals: number,
) {
  const token = findToken(tokenAddress);
  const suffix = token?.symbol ? ` ${token.symbol}` : "";
  return `${formatTokenAmount(value, token?.decimals ?? fallbackDecimals)}${suffix}`;
}

function getOwnStringProperty(value: unknown, key: string) {
  if (!value || (typeof value !== "object" && typeof value !== "function")) return null;

  let current: object | null = value as object;
  while (current) {
    const descriptor = Object.getOwnPropertyDescriptor(current, key);
    if (descriptor && "value" in descriptor && typeof descriptor.value === "string") {
      return descriptor.value;
    }
    current = Object.getPrototypeOf(current);
  }
  return null;
}

function describeDarkpoolError(cause: unknown, fallback: string) {
  const shortMessage = getOwnStringProperty(cause, "shortMessage");
  const message = getOwnStringProperty(cause, "message");
  const data = getOwnStringProperty(cause, "data");
  const details = [shortMessage, message, data].filter(Boolean).join(" ");

  if (details.includes(ORDER_DOES_NOT_EXIST_SELECTOR)) {
    return "Order not found or no longer resting in the darkpool.";
  }
  if (details.includes(UNAUTHORIZED_SELECTOR)) {
    return "Order is not owned by the connected account.";
  }

  if (shortMessage && !/^execution reverted$/i.test(shortMessage)) return shortMessage;
  if (message && !/^execution reverted$/i.test(message)) return message;

  return fallback;
}

function scopesCoverDarkpool(scopes: readonly StoredAccessKeyScope[] | undefined) {
  if (!scopes) return false;

  return DARKPOOL_ACCESS_KEY_SCOPES.every((required) =>
    scopes.some((scope) => {
      const sameAddress = scope.address.toLowerCase() === required.address.toLowerCase();
      const sameSelector =
        !scope.selector || scope.selector.toLowerCase() === required.selector.toLowerCase();
      return sameAddress && sameSelector;
    }),
  );
}

function getAccessKeyLimitAmount(limit: StoredAccessKeyLimit) {
  const value = limit.limit ?? limit.amount;
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return BigInt(value);
  if (typeof value === "string") return BigInt(value);
  return null;
}

function limitsCoverDarkpool(limits: readonly StoredAccessKeyLimit[] | undefined) {
  if (!limits) return false;

  return BRIDGE_TOKENS.every((token) =>
    limits.some((limit) => {
      const amount = getAccessKeyLimitAmount(limit);
      return (
        limit.token.toLowerCase() === token.address.toLowerCase() &&
        amount === UINT128_MAX
      );
    }),
  );
}

function findDarkpoolAccessKey(provider: ZoneAuthProvider, owner: Address) {
  const accessKeys = provider.store?.getState().accessKeys ?? [];
  const now = Math.floor(Date.now() / 1000);

  return accessKeys.find((key): key is StoredDarkpoolAccessKey => {
    const usableLocalKey = key.keyPair != null;
    const sameOwner = key.access.toLowerCase() === owner.toLowerCase();
    const notExpiring = !key.expiry || key.expiry > now + ACCESS_KEY_REFRESH_BUFFER_SECS;
    const matchingPendingAuthorization =
      !key.keyAuthorization ||
      getKeyAuthorizationChainId(key.keyAuthorization) === BigInt(zoneChain.id);
    return (
      usableLocalKey &&
      sameOwner &&
      notExpiring &&
      matchingPendingAuthorization &&
      limitsCoverDarkpool(key.limits) &&
      scopesCoverDarkpool(key.scopes)
    );
  });
}

function getKeyAuthorizationChainId(keyAuthorization: unknown) {
  if (!keyAuthorization || typeof keyAuthorization !== "object") return null;
  const value = (keyAuthorization as { chainId?: unknown }).chainId;
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return BigInt(value);
  if (typeof value === "string") return BigInt(value);
  return null;
}

function clearLocalAccessKeys(provider: ZoneAuthProvider, owner: Address) {
  const accessKeys = provider.store?.getState().accessKeys ?? [];
  if (!accessKeys.length || !provider.store?.setState) return;

  const filtered = accessKeys.filter((key) => {
    const localKey = key.keyPair != null || key.privateKey != null;
    const sameOwner = key.access.toLowerCase() === owner.toLowerCase();
    return !(localKey && sameOwner);
  });

  if (filtered.length !== accessKeys.length) {
    provider.store.setState({ accessKeys: filtered });
  }
}

function markAccessKeyAuthorizationUsed(provider: ZoneAuthProvider, accessKey: StoredDarkpoolAccessKey) {
  const accessKeys = provider.store?.getState().accessKeys ?? [];
  if (!accessKeys.length || !provider.store?.setState || !accessKey.address) return;

  provider.store.setState({
    accessKeys: accessKeys.map((key) =>
      key.address?.toLowerCase() === accessKey.address?.toLowerCase()
        ? { ...key, keyAuthorization: undefined }
        : key,
    ),
  });
}

function darkpoolAccessKeyRequest() {
  return {
    chainId: BigInt(zoneChain.id),
    expiry: Math.floor(Date.now() / 1000) + DARKPOOL_ACCESS_KEY_TTL_SECS,
    limits: BRIDGE_TOKENS.map((token) => ({
      token: token.address,
      limit: UINT128_MAX,
    })),
    scopes: DARKPOOL_ACCESS_KEY_SCOPES,
  };
}

function isAccessKeyAuthError(cause: unknown) {
  const message = describeDarkpoolError(cause, "");
  return /keyauthorization|keychain key|access key|chain.?id mismatch|call scopes|before T3|spending.?limit|InvalidSpendingLimit|not authorized|revoked|expired/i.test(
    message,
  );
}

function TokenUtilityRow({
  label,
  token,
  walletBalance,
  darkpoolBalance,
  availableDarkpoolBalance,
  amount,
  onAmountChange,
  onDeposit,
  onWithdraw,
  pendingAction,
  disabled,
}: {
  label: string;
  token: BridgeToken;
  walletBalance?: bigint;
  darkpoolBalance?: bigint;
  availableDarkpoolBalance?: bigint;
  amount: string;
  onAmountChange: (value: string) => void;
  onDeposit: () => void;
  onWithdraw: () => void;
  pendingAction: string | null;
  disabled: boolean;
}) {
  return (
    <div className="grid gap-3 border-t border-zinc-200 py-4 first:border-t-0 first:pt-0 last:pb-0 md:grid-cols-[1.4fr_1fr_1fr_1.4fr] md:items-center">
      <div>
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium text-zinc-900">{label}</span>
          <span className="rounded bg-zinc-100 px-2 py-0.5 text-[11px] font-medium text-zinc-600">
            {token.symbol}
          </span>
        </div>
        <div className="mt-1 text-xs text-zinc-500">{token.address}</div>
      </div>

      <div className="text-sm text-zinc-600">
        <div>Wallet: {formatTokenAmount(walletBalance, token.decimals)}</div>
        <div>Darkpool: {formatTokenAmount(darkpoolBalance, token.decimals)}</div>
        <div>Available: {formatTokenAmount(availableDarkpoolBalance, token.decimals)}</div>
      </div>

      <div className="text-sm text-zinc-600">
        <div>Transfer path</div>
        <div className="text-xs text-zinc-500">No TIP20 approval required</div>
      </div>

      <div className="flex flex-col gap-2 sm:flex-row">
        <input
          type="number"
          step="0.01"
          value={amount}
          onChange={(event) => onAmountChange(event.target.value)}
          placeholder="0.00"
          className="min-w-0 flex-1 rounded-lg border border-zinc-200 bg-white px-3 py-2 text-sm text-zinc-900 outline-none"
        />
        <button
          onClick={onDeposit}
          disabled={pendingAction !== null || disabled}
          className="rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-800 disabled:opacity-50"
        >
          {pendingAction === `deposit:${token.id}` ? "Depositing..." : "Deposit"}
        </button>
        <button
          onClick={onWithdraw}
          disabled={pendingAction !== null || disabled}
          className="rounded-lg border border-zinc-300 px-3 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 disabled:opacity-50"
        >
          {pendingAction === `withdraw:${token.id}` ? "Withdrawing..." : "Withdraw"}
        </button>
      </div>
    </div>
  );
}

export function DarkpoolDashboard() {
  const { address, isConnected, chainId } = useAccount();
  const connections = useConnections();
  const { switchChainAsync } = useSwitchChain();
  const publicClient = usePublicClient({ chainId: zoneChain.id });

  const [baseAddress, setBaseAddress] =
    useState<(typeof BASE_TOKENS)[number]["address"]>(BASE_TOKENS[0].address);
  const [utilityAmounts, setUtilityAmounts] = useState<Record<UtilityTokenKey, string>>({
    base: "",
    quote: "",
  });
  const [limitSide, setLimitSide] = useState<"bid" | "ask">("bid");
  const [limitAmount, setLimitAmount] = useState("");
  const [limitPrice, setLimitPrice] = useState("1");
  const [marketSide, setMarketSide] = useState<"buy" | "sell">("buy");
  const [marketAmount, setMarketAmount] = useState("");
  const [marketGuard, setMarketGuard] = useState("");
  const [orderLookup, setOrderLookup] = useState("");
  const [loadedOrder, setLoadedOrder] = useState<OrderView | null>(null);
  const [loadedOrderHistory, setLoadedOrderHistory] = useState<OrderHistory | null>(null);
  const [recentOrderHistories, setRecentOrderHistories] = useState<OrderHistory[]>([]);
  const [authToken, setAuthToken] = useState<CachedZoneAuthToken | null>(null);
  const [knownOrderIds, setKnownOrderIds] = useState<bigint[]>([]);
  const [activity, setActivity] = useState<ActivityItem[]>([]);
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const activityCounterRef = useRef(0);
  const isOnZoneChain = chainId === zoneChain.id;

  const selectedBase = useMemo(
    () =>
      BASE_TOKENS.find((token) => token.address === baseAddress) ?? BASE_TOKENS[0],
    [baseAddress],
  );

  const quoteTokenQuery = useReadContract({
    abi: TIP20_ABI,
    address: selectedBase.address,
    functionName: "quoteToken",
    chainId: zoneChain.id,
    query: { enabled: isConnected },
  });
  const quoteToken = findToken((quoteTokenQuery.data as Address | undefined) ?? undefined);
  const selectedBaseEnabled = quoteTokenQuery.isSuccess && quoteToken != null;
  const quoteAddress = quoteToken?.address ?? BRIDGE_TOKENS[0].address;
  const activeQuoteToken = quoteToken ?? BRIDGE_TOKENS[0];

  const minOrderAmountQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "MIN_ORDER_AMOUNT",
    chainId: zoneChain.id,
    query: { enabled: true },
  });
  const pairKeyQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "pairKey",
    args: [selectedBase.address, quoteAddress],
    chainId: zoneChain.id,
    query: { enabled: selectedBaseEnabled && !!quoteAddress },
  });
  const bestBidQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "bestBid",
    args: [selectedBase.address],
    chainId: zoneChain.id,
    query: { enabled: isConnected && selectedBaseEnabled },
  });
  const bestAskQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "bestAsk",
    args: [selectedBase.address],
    chainId: zoneChain.id,
    query: { enabled: isConnected && selectedBaseEnabled },
  });

  const baseWalletQuery = useReadContract({
    abi: TIP20_ABI,
    address: selectedBase.address,
    functionName: "balanceOf",
    args: address ? [address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address && selectedBaseEnabled },
  });
  const quoteWalletQuery = useReadContract({
    abi: TIP20_ABI,
    address: activeQuoteToken.address,
    functionName: "balanceOf",
    args: address ? [address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address },
  });
  const baseDarkpoolBalanceQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "balanceOf",
    args: address ? [address, selectedBase.address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address && selectedBaseEnabled },
  });
  const baseAvailableDarkpoolBalanceQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "availableBalanceOf",
    args: address ? [address, selectedBase.address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address && selectedBaseEnabled },
  });
  const quoteDarkpoolBalanceQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "balanceOf",
    args: address ? [address, activeQuoteToken.address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address },
  });
  const quoteAvailableDarkpoolBalanceQuery = useReadContract({
    abi: DARKPOOL_ORDERBOOK_ABI,
    address: DARKPOOL_ADDRESS,
    functionName: "availableBalanceOf",
    args: address ? [address, activeQuoteToken.address] : undefined,
    account: address,
    chainId: zoneChain.id,
    query: { enabled: !!address },
  });

  const addActivity = (item: ActivityItem) => {
    setActivity((current) => [item, ...current].slice(0, 8));
  };

  const ensureZoneChain = async () => {
    if (isOnZoneChain) return;
    if (!switchChainAsync) {
      throw new Error(`Switch your wallet to ${zoneChain.name} before using darkpool actions`);
    }
    await switchChainAsync({ chainId: zoneChain.id });
  };

  const rememberOrderId = (orderId: bigint) => {
    if (orderId === 0n) return;
    setKnownOrderIds((current) => {
      if (current.includes(orderId)) return current;
      return [orderId, ...current].slice(0, 12);
    });
  };

  const rememberOrderHistory = (history: OrderHistory) => {
    rememberOrderId(history.orderId);
    setRecentOrderHistories((current) => {
      const existing = current.find((item) => item.orderId === history.orderId);
      const merged = mergeOrderHistory(existing, history);
      return [
        merged,
        ...current.filter((item) => item.orderId !== history.orderId),
      ].slice(0, 8);
    });
  };

  const recordOrderReceipt = (receipt: TransactionReceipt) => {
    const histories = [...parseOrderHistoriesFromLogs(receipt.logs).values()];
    if (!histories.length) return;

    for (const history of histories) {
      rememberOrderHistory(history);
    }
    addActivity({
      id: `events-${receipt.transactionHash}`,
      label: "Order events",
      detail: histories.map(orderHistorySummary).join(" · "),
      status: "success",
    });
  };

  const refetchAll = async () => {
    const refetches: Promise<unknown>[] = [
      quoteWalletQuery.refetch(),
      quoteDarkpoolBalanceQuery.refetch(),
      quoteAvailableDarkpoolBalanceQuery.refetch(),
    ];
    if (selectedBaseEnabled) {
      refetches.push(
        bestBidQuery.refetch(),
        bestAskQuery.refetch(),
        baseWalletQuery.refetch(),
        baseDarkpoolBalanceQuery.refetch(),
        baseAvailableDarkpoolBalanceQuery.refetch(),
      );
    }
    await Promise.all(refetches);
  };

  const getZoneTransactionDefaults = async () => {
    if (!address) throw new Error("Connect a wallet first");
    if (!publicClient) throw new Error("Zone public client unavailable");

    // Avoid wallet-side eth_fillTransaction recursion for zone precompile writes.
    const [nonce, fees] = await Promise.all([
      publicClient.getTransactionCount({ address, blockTag: "pending" }),
      publicClient.estimateFeesPerGas(),
    ]);

    if (
      !("maxFeePerGas" in fees) ||
      fees.maxFeePerGas == null ||
      !("maxPriorityFeePerGas" in fees) ||
      fees.maxPriorityFeePerGas == null
    ) {
      throw new Error("Zone EIP-1559 fee data unavailable");
    }

    return {
      chainId: zoneChain.id,
      nonce,
      maxFeePerGas: fees.maxFeePerGas,
      maxPriorityFeePerGas: fees.maxPriorityFeePerGas,
    };
  };

  const ensureDarkpoolAccessKey = async (provider: ZoneAuthProvider) => {
    if (!address) throw new Error("Connect a wallet first");

    const existing = findDarkpoolAccessKey(provider, address);
    if (existing) return existing;

    clearLocalAccessKeys(provider, address);
    await provider.request({
      method: "wallet_authorizeAccessKey",
      params: [darkpoolAccessKeyRequest()],
    });

    const next = findDarkpoolAccessKey(provider, address);
    if (!next) throw new Error("Wallet did not return a usable darkpool access key");
    return next;
  };

  const signAndSubmitWithAccessKey = async ({
    provider,
    to,
    data,
    gas,
  }: {
    provider: ZoneAuthProvider;
    to: Address;
    data: Hex;
    gas: bigint;
  }) => {
    if (!address) throw new Error("Connect a wallet first");

    const accessKey = await ensureDarkpoolAccessKey(provider);
    const accessKeyAccount = TempoAccount.fromWebCryptoP256(accessKey.keyPair, {
      access: address,
      internal_version: "v2",
    });
    const defaults = await getZoneTransactionDefaults();
    const hasPendingKeyAuthorization = accessKey.keyAuthorization != null;
    const transactionGas = hasPendingKeyAuthorization
      ? gas + ACCESS_KEY_AUTHORIZATION_GAS_OVERHEAD
      : gas;
    const signed = await accessKeyAccount.signTransaction(
      {
        type: "tempo",
        chainId: defaults.chainId,
        nonce: defaults.nonce,
        maxFeePerGas: defaults.maxFeePerGas,
        maxPriorityFeePerGas: defaults.maxPriorityFeePerGas,
        calls: [{ to, data }],
        gas: transactionGas,
        keyAuthorization: accessKey.keyAuthorization as never,
      } as never,
      { serializer: TempoTransaction.serialize as never },
    );

    const { result, token } = await zonePrivateRpc<`0x${string}`>({
      address,
      chainId: zoneChain.id,
      provider,
      currentToken: authToken,
      body: {
        jsonrpc: "2.0",
        method: "eth_sendRawTransaction",
        params: [signed],
        id: 1,
      },
    });
    setAuthToken(token);
    markAccessKeyAuthorizationUsed(provider, accessKey);
    return result;
  };

  const sendZoneContractCall = async ({
    to,
    data,
    gas,
  }: {
    to: Address;
    data: Hex;
    gas: bigint;
  }) => {
    if (!address) throw new Error("Connect a wallet first");
    const activeConnector = connections[0]?.connector;
    const provider = activeConnector
      ? ((await activeConnector.getProvider({
          chainId: zoneChain.id,
    })) as ZoneAuthProvider | undefined)
      : undefined;
    if (!provider) throw new Error("No connected wallet provider");

    try {
      return await signAndSubmitWithAccessKey({ provider, to, data, gas });
    } catch (cause) {
      if (!isAccessKeyAuthError(cause)) throw cause;
      clearLocalAccessKeys(provider, address);
      return await signAndSubmitWithAccessKey({ provider, to, data, gas });
    }
  };

  const estimateZoneContractCallGas = async ({
    to,
    data,
    fallbackGas,
  }: {
    to: Address;
    data: Hex;
    fallbackGas: bigint;
  }) => {
    if (!address) throw new Error("Connect a wallet first");
    if (!publicClient) throw new Error("Zone public client unavailable");

    const estimate = await publicClient.estimateGas({
      account: address,
      to,
      data,
    });
    const bufferedEstimate = gasWithBuffer(estimate);
    return bufferedEstimate > fallbackGas ? bufferedEstimate : fallbackGas;
  };

  const runWrite = async (
    actionKey: string,
    label: string,
    write: () => Promise<`0x${string}`>,
  ): Promise<TransactionReceipt> => {
    if (!publicClient) throw new Error("Zone public client unavailable");
    setPendingAction(actionKey);
    setError(null);
    activityCounterRef.current += 1;
    addActivity({
      id: `${activityCounterRef.current}-${actionKey}`,
      label,
      detail: "Transaction submitted",
      status: "pending",
    });
    try {
      await ensureZoneChain();
      const hash = await write();
      const receipt = await publicClient.waitForTransactionReceipt({ hash });
      await refetchAll();
      addActivity({
        id: hash,
        label,
        detail: hash,
        status: receipt.status === "success" ? "success" : "error",
      });
      if (receipt.status !== "success") {
        throw new Error(`${label} reverted`);
      }
      return receipt;
    } finally {
      setPendingAction(null);
    }
  };

  const writeDarkpoolContract = async (config: {
    functionName:
      | "deposit"
      | "withdraw"
      | "place"
      | "cancel"
      | "marketBuy"
      | "marketSell";
    args: readonly unknown[];
  }) => {
    const data = encodeFunctionData({
      abi: DARKPOOL_ORDERBOOK_ABI,
      functionName: config.functionName,
      args: config.args as never,
    });
    const fallbackGas = darkpoolGasLimit(config.functionName);
    const gas = await estimateZoneContractCallGas({
      to: DARKPOOL_ADDRESS,
      data,
      fallbackGas,
    });
    return sendZoneContractCall({
      to: DARKPOOL_ADDRESS,
      data,
      gas,
    });
  };

  const tokenForUtility = (key: UtilityTokenKey) =>
    key === "base" ? selectedBase : activeQuoteToken;
  const availableDarkpoolBalanceForUtility = (key: UtilityTokenKey) =>
    key === "base"
      ? (baseAvailableDarkpoolBalanceQuery.data as bigint | undefined)
      : (quoteAvailableDarkpoolBalanceQuery.data as bigint | undefined);

  const ensureSelectedBaseEnabled = () => {
    if (selectedBaseEnabled) return;
    throw new Error(
      `${selectedBase.symbol} is not enabled on ${zoneChain.name}. Enable it through the portal and wait for zone sync before using it in the darkpool.`,
    );
  };

  const handleDepositWithdraw = async (key: UtilityTokenKey, mode: "deposit" | "withdraw") => {
    const token = tokenForUtility(key);
    try {
      if (key === "base") ensureSelectedBaseEnabled();
      const amount = parseAmountInput(utilityAmounts[key], token.decimals);
      if (amount <= 0n) throw new Error("Enter an amount");
      if (mode === "withdraw") {
        ensureSufficientAmount(
          amount,
          availableDarkpoolBalanceForUtility(key),
          `${token.symbol} available darkpool balance`,
        );
      }
      await runWrite(`${mode}:${token.id}`, `${mode} ${token.symbol}`, () =>
        writeDarkpoolContract({
          functionName: mode,
          args: [token.address, amount],
        }),
      );
      setUtilityAmounts((current) => ({ ...current, [key]: "" }));
    } catch (cause) {
      setError(describeDarkpoolError(cause, `${mode} failed`));
    }
  };

  const handlePlaceOrder = async () => {
    try {
      ensureSelectedBaseEnabled();
      const amount = parseAmountInput(limitAmount, selectedBase.decimals);
      const price = parseUint128Integer(limitPrice);
      if (amount <= 0n) throw new Error("Enter an order amount");
      if (price <= 0n) throw new Error("Enter a raw integer price");
      if (minOrderAmount == null) {
        throw new Error("Minimum order amount unavailable. Refresh the page and try again.");
      }
      if (amount < minOrderAmount) {
        throw new Error(
          `Order amount must be at least ${formatTokenAmount(
            minOrderAmount,
            selectedBase.decimals,
          )} ${selectedBase.symbol}`,
        );
      }

      if (limitSide === "bid") {
        const escrow = amount * price;
        ensureSufficientAmount(
          escrow,
          quoteWallet,
          `${activeQuoteToken.symbol} wallet balance`,
        );
      } else {
        ensureSufficientAmount(
          amount,
          baseWallet,
          `${selectedBase.symbol} wallet balance`,
        );
      }

      const receipt = await runWrite(
        `place:${limitSide}`,
        `Place ${limitSide} order`,
        () =>
          writeDarkpoolContract({
            functionName: "place",
            args: [selectedBase.address, amount, price, limitSide === "bid"],
          }),
      );

      recordOrderReceipt(receipt);

      setLimitAmount("");
    } catch (cause) {
      setError(describeDarkpoolError(cause, "Order placement failed"));
    }
  };

  const handleMarketOrder = async () => {
    try {
      ensureSelectedBaseEnabled();
      const amount = parseAmountInput(marketAmount, selectedBase.decimals);
      if (amount <= 0n) throw new Error("Enter a market amount");

      const guard =
        marketGuard.trim() === ""
          ? 0n
          : parseAmountInput(marketGuard, activeQuoteToken.decimals);

      const receipt = await runWrite(`market:${marketSide}`, `Market ${marketSide}`, () =>
        writeDarkpoolContract({
          functionName: marketSide === "buy" ? "marketBuy" : "marketSell",
          args:
            marketSide === "buy"
              ? [selectedBase.address, amount, guard]
              : [selectedBase.address, amount, guard],
        }),
      );
      recordOrderReceipt(receipt);
      setMarketAmount("");
    } catch (cause) {
      setError(describeDarkpoolError(cause, "Market order failed"));
    }
  };

  const fetchRestingOrder = async (orderId: bigint) => {
    if (!address) throw new Error("Connect a wallet first");
    const activeConnector = connections[0]?.connector;
    const provider = activeConnector
      ? ((await activeConnector.getProvider({
          chainId: zoneChain.id,
        })) as ZoneAuthProvider | undefined)
      : undefined;
    if (!provider) throw new Error("No connected wallet provider");
    const callData = encodeFunctionData({
      abi: DARKPOOL_ORDERBOOK_ABI,
      functionName: "getOrder",
      args: [orderId],
    });

    const { result, token } = await zonePrivateRpc<`0x${string}`>({
      address,
      chainId: zoneChain.id,
      provider,
      currentToken: authToken,
      body: {
        jsonrpc: "2.0",
        method: "eth_call",
        params: [
          { to: DARKPOOL_ADDRESS, from: address, data: callData },
          "latest",
        ],
        id: 1,
      },
    });
    setAuthToken(token);

    if (!result || result === "0x") {
      throw new Error("Order not found or not owned by the connected account");
    }

    try {
      return decodeFunctionResult({
        abi: DARKPOOL_ORDERBOOK_ABI,
        functionName: "getOrder",
        data: result as Hex,
      }) as OrderView;
    } catch {
      throw new Error("Order lookup returned invalid data");
    }
  };

  const fetchOrderHistory = async (orderId: bigint) => {
    if (!publicClient) throw new Error("Zone public client unavailable");
    const logs = await publicClient.getLogs({
      address: DARKPOOL_ADDRESS,
      fromBlock: 0n,
      toBlock: "latest",
    });
    return parseOrderHistoriesFromLogs(logs).get(orderId) ?? null;
  };

  const loadOrderDetails = async (orderId: bigint) => {
    await ensureZoneChain();

    const [restingResult, history] = await Promise.allSettled([
      fetchRestingOrder(orderId),
      fetchOrderHistory(orderId),
    ]);
    const restingOrder =
      restingResult.status === "fulfilled" ? restingResult.value : null;
    const eventHistory =
      history.status === "fulfilled" ? history.value : null;

    if (!restingOrder && !eventHistory) {
      if (restingResult.status === "rejected") throw restingResult.reason;
      if (history.status === "rejected") throw history.reason;
      throw new Error("Order not found or not owned by the connected account");
    }

    const mergedHistory =
      eventHistory ??
      (restingOrder
        ? {
            orderId: restingOrder.orderId,
            maker: restingOrder.maker,
            base: restingOrder.base,
            quote: restingOrder.quote,
            isBid: restingOrder.isBid,
            placedPrice: restingOrder.price,
            cancelled: false,
            fills: [],
          }
        : null);

    setLoadedOrder(restingOrder);
    setLoadedOrderHistory(mergedHistory);
    rememberOrderId(orderId);
    if (mergedHistory) rememberOrderHistory(mergedHistory);
    addActivity({
      id: `read-${orderId.toString()}`,
      label: restingOrder ? "Read open order" : "Read order history",
      detail: mergedHistory ? orderHistorySummary(mergedHistory) : `Order #${orderId.toString()}`,
      status: "success",
    });
  };

  const handleLoadOrder = async (orderIdValue?: bigint) => {
    try {
      const orderId =
        orderIdValue ?? parseUint128Integer(orderLookup.trim() || "0");
      if (orderId <= 0n) throw new Error("Enter an order id");
      setPendingAction("loadOrder");
      setError(null);
      await loadOrderDetails(orderId);
    } catch (cause) {
      setError(describeDarkpoolError(cause, "Order not found or not owned by the connected account"));
      setLoadedOrder(null);
      setLoadedOrderHistory(null);
    } finally {
      setPendingAction(null);
    }
  };

  const handleCancelOrder = async () => {
    try {
      const orderId = loadedOrder?.orderId ?? parseUint128Integer(orderLookup.trim() || "0");
      if (orderId <= 0n) throw new Error("Load an order first");
      const receipt = await runWrite("cancel", `Cancel order #${orderId.toString()}`, () =>
        writeDarkpoolContract({
          functionName: "cancel",
          args: [orderId],
        }),
      );
      recordOrderReceipt(receipt);
      setLoadedOrder(null);
      setLoadedOrderHistory((current) =>
        current && current.orderId === orderId
          ? { ...current, cancelled: true }
          : current,
      );
    } catch (cause) {
      setError(describeDarkpoolError(cause, "Cancel failed"));
    }
  };

  const bestBid = bestBidQuery.data as readonly [bigint, bigint] | undefined;
  const bestAsk = bestAskQuery.data as readonly [bigint, bigint] | undefined;
  const minOrderAmount = minOrderAmountQuery.data as bigint | undefined;
  const baseWallet = baseWalletQuery.data as bigint | undefined;
  const quoteWallet = quoteWalletQuery.data as bigint | undefined;
  const baseDarkpoolBalance = baseDarkpoolBalanceQuery.data as bigint | undefined;
  const quoteDarkpoolBalance = quoteDarkpoolBalanceQuery.data as bigint | undefined;
  const baseAvailableDarkpoolBalance = baseAvailableDarkpoolBalanceQuery.data as
    | bigint
    | undefined;
  const quoteAvailableDarkpoolBalance = quoteAvailableDarkpoolBalanceQuery.data as
    | bigint
    | undefined;
  const limitFundingSymbol =
    limitSide === "bid" ? activeQuoteToken.symbol : selectedBase.symbol;
  const limitFundingBalanceLoaded =
    limitSide === "bid" ? quoteWallet != null : baseWallet != null;
  const limitFundingBalanceFailed =
    limitSide === "bid" ? quoteWalletQuery.isError : baseWalletQuery.isError;
  const marketFundingSymbol =
    marketSide === "buy" ? activeQuoteToken.symbol : selectedBase.symbol;
  const marketFundingBalanceLoaded =
    marketSide === "buy" ? quoteWallet != null : baseWallet != null;
  const marketFundingBalanceFailed =
    marketSide === "buy" ? quoteWalletQuery.isError : baseWalletQuery.isError;

  if (!isConnected) {
    return (
      <div className="min-h-screen bg-zinc-50">
        <header className="border-b border-zinc-200 bg-white">
          <div className="mx-auto flex h-16 max-w-6xl items-center justify-between px-6">
            <div className="flex items-center gap-3">
              <Link
                href="/"
                className="inline-flex items-center gap-2 text-sm font-medium text-zinc-600 transition-colors hover:text-zinc-900"
              >
                <ArrowLeft size={14} />
                Back
              </Link>
              <div className="text-lg font-semibold text-zinc-900">Darkpool Orderbook</div>
            </div>
            <WalletConnect />
          </div>
        </header>

        <main className="mx-auto max-w-4xl px-6 py-20">
          <div className="rounded-xl border border-zinc-200 bg-white p-10 text-center">
            <h1 className="text-2xl font-semibold text-zinc-900">
              Connect a wallet to open the darkpool demo
            </h1>
            <p className="mt-3 text-sm text-zinc-500">
              This page is built for internal ops and demo flows: funding, approvals,
              limit and market orders, and maker-scoped order reads.
            </p>
          </div>
        </main>
      </div>
    );
  }

  return (
    <div className="min-h-screen bg-zinc-50">
      <header className="border-b border-zinc-200 bg-white">
        <div className="mx-auto flex h-16 max-w-6xl items-center justify-between px-6">
          <div className="flex items-center gap-3">
            <Link
              href="/"
              className="inline-flex items-center gap-2 text-sm font-medium text-zinc-600 transition-colors hover:text-zinc-900"
            >
              <ArrowLeft size={14} />
              Home
            </Link>
            <div>
              <div className="text-lg font-semibold text-zinc-900">Darkpool Orderbook</div>
              <div className="text-xs text-zinc-500">
                Internal ops / demo dashboard for Zone #{ZONE_ID}
              </div>
            </div>
          </div>
          <WalletConnect />
        </div>
      </header>

      <main className="mx-auto max-w-6xl px-6 py-8">
        {!isOnZoneChain && (
          <section className="mb-6 rounded-xl border border-amber-200 bg-amber-50 px-4 py-4 text-sm text-amber-900">
            <div className="flex flex-col gap-3 md:flex-row md:items-center md:justify-between">
              <div>
                <div className="font-medium">Wallet is on the wrong network</div>
                <div className="mt-1 text-amber-800">
                  Darkpool actions run on {zoneChain.name} (chain {zoneChain.id}). Your wallet is
                  currently on chain {chainId}.
                </div>
              </div>
              <button
                onClick={() => void ensureZoneChain().catch((cause) => {
                  setError(describeDarkpoolError(cause, "Failed to switch network"));
                })}
                disabled={pendingAction !== null}
                className="rounded-lg bg-amber-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-amber-800 disabled:opacity-50"
              >
                Switch to {zoneChain.name}
              </button>
            </div>
          </section>
        )}

        <section className="grid gap-4 lg:grid-cols-[1.6fr_1fr_1fr_1fr]">
          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
              Pair
            </div>
            <div className="mt-3 flex flex-col gap-3 md:flex-row md:items-center">
              <select
                value={selectedBase.address}
                onChange={(event) => {
                  setBaseAddress(event.target.value as (typeof selectedBase)["address"]);
                  setLoadedOrder(null);
                  setLoadedOrderHistory(null);
                  setError(null);
                }}
                className="rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
              >
                {BASE_TOKENS.map((token) => (
                  <option key={token.id} value={token.address}>
                    {token.symbol} / pathUSD
                  </option>
                ))}
              </select>
            </div>
            <div className="mt-3 text-xs text-zinc-500">
              Base {selectedBase.address} · Quote {activeQuoteToken.address}
            </div>
            <div className="mt-2 text-xs text-zinc-500">
              Pairs are created lazily on the first deposit, limit order, or market order.
            </div>
          </div>

          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
              Best Bid
            </div>
            <div className="mt-3 text-2xl font-semibold text-zinc-900">
              {formatRawPrice(bestBid?.[0])}
            </div>
            <div className="mt-1 text-sm text-zinc-500">
              {formatTokenAmount(bestBid?.[1], selectedBase.decimals)} {selectedBase.symbol}
            </div>
          </div>

          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
              Best Ask
            </div>
            <div className="mt-3 text-2xl font-semibold text-zinc-900">
              {formatRawPrice(bestAsk?.[0])}
            </div>
            <div className="mt-1 text-sm text-zinc-500">
              {formatTokenAmount(bestAsk?.[1], selectedBase.decimals)} {selectedBase.symbol}
            </div>
          </div>

          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
              Pair State
            </div>
            <div className="mt-3 text-sm text-zinc-700">
              Min order:{" "}
              <span className="font-medium">
                {formatTokenAmount(minOrderAmount, selectedBase.decimals)}
              </span>
            </div>
            <div className="mt-1 break-all font-mono text-[11px] text-zinc-500">
              {pairKeyQuery.data as string | undefined}
            </div>
          </div>
        </section>

        <section className="mt-6 rounded-xl border border-zinc-200 bg-white p-5">
          <div className="flex items-center gap-2">
            <Coins size={16} className="text-zinc-500" />
            <h2 className="text-sm font-semibold text-zinc-900">Funding</h2>
          </div>
          {!selectedBaseEnabled && (
            <div className="mt-4 rounded-lg border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-900">
              {selectedBase.symbol} is not enabled on {zoneChain.name} yet. Enable it through the
              portal and wait for zone sync before depositing or trading it.
            </div>
          )}
          <div className="mt-4">
            <TokenUtilityRow
              label="Base Token"
              token={selectedBase}
              walletBalance={baseWallet}
              darkpoolBalance={baseDarkpoolBalance}
              availableDarkpoolBalance={baseAvailableDarkpoolBalance}
              amount={utilityAmounts.base}
              onAmountChange={(value) =>
                setUtilityAmounts((current) => ({ ...current, base: value }))
              }
              onDeposit={() => void handleDepositWithdraw("base", "deposit")}
              onWithdraw={() => void handleDepositWithdraw("base", "withdraw")}
              pendingAction={pendingAction}
              disabled={!isOnZoneChain || !selectedBaseEnabled}
            />
            <TokenUtilityRow
              label="Quote Token"
              token={activeQuoteToken}
              walletBalance={quoteWallet}
              darkpoolBalance={quoteDarkpoolBalance}
              availableDarkpoolBalance={quoteAvailableDarkpoolBalance}
              amount={utilityAmounts.quote}
              onAmountChange={(value) =>
                setUtilityAmounts((current) => ({ ...current, quote: value }))
              }
              onDeposit={() => void handleDepositWithdraw("quote", "deposit")}
              onWithdraw={() => void handleDepositWithdraw("quote", "withdraw")}
              pendingAction={pendingAction}
              disabled={!isOnZoneChain}
            />
          </div>
        </section>

        <section className="mt-6 grid gap-6 lg:grid-cols-2">
          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="flex items-center gap-2">
              <BookOpen size={16} className="text-zinc-500" />
              <h2 className="text-sm font-semibold text-zinc-900">Limit Order Ticket</h2>
            </div>

            <div className="mt-4 grid gap-4">
              <div className="inline-flex rounded-lg border border-zinc-200 bg-zinc-50 p-1">
                {(["bid", "ask"] as const).map((side) => (
                  <button
                    key={side}
                    onClick={() => setLimitSide(side)}
                    className={`rounded-md px-3 py-2 text-sm font-medium transition-colors ${
                      limitSide === side
                        ? "bg-white text-zinc-900 shadow-sm"
                        : "text-zinc-500 hover:text-zinc-900"
                    }`}
                  >
                    {side === "bid" ? "Bid" : "Ask"}
                  </button>
                ))}
              </div>

              <div className="grid gap-4 md:grid-cols-2">
                <label className="text-sm text-zinc-600">
                  Amount ({selectedBase.symbol})
                  <input
                    type="number"
                    step="0.01"
                    value={limitAmount}
                    onChange={(event) => setLimitAmount(event.target.value)}
                    className="mt-1 w-full rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
                  />
                </label>
                <label className="text-sm text-zinc-600">
                  Raw Price
                  <input
                    type="text"
                    inputMode="numeric"
                    value={limitPrice}
                    onChange={(event) => setLimitPrice(event.target.value)}
                    className="mt-1 w-full rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
                  />
                </label>
              </div>

              <div className="rounded-lg bg-zinc-50 px-3 py-3 text-xs text-zinc-500">
                <div>
                  {limitSide === "bid"
                    ? `Bids escrow ${activeQuoteToken.symbol} directly from the wallet into the darkpool.`
                    : `Asks escrow ${selectedBase.symbol} directly from the wallet into the darkpool.`}
                </div>
                <div className="mt-1">
                  This precompile uses an integer raw price. For same-decimal stablecoin pairs,
                  `1` is the natural 1:1 demo price.
                </div>
              </div>

              {selectedBaseEnabled && !limitFundingBalanceLoaded && (
                <div
                  className={`rounded-lg border px-3 py-2 text-xs ${
                    limitFundingBalanceFailed
                      ? "border-red-200 bg-red-50 text-red-700"
                      : "border-zinc-200 bg-zinc-50 text-zinc-500"
                  }`}
                >
                  {limitFundingBalanceFailed
                    ? `${limitFundingSymbol} wallet balance read failed. Use Refresh to retry.`
                    : `Loading ${limitFundingSymbol} wallet balance...`}
                </div>
              )}

              <button
                onClick={() => void handlePlaceOrder()}
                disabled={
                  pendingAction !== null ||
                  !isOnZoneChain ||
                  !selectedBaseEnabled ||
                  !limitFundingBalanceLoaded
                }
                className="rounded-lg bg-zinc-900 px-4 py-3 text-sm font-medium text-white transition-colors hover:bg-zinc-800 disabled:opacity-50"
              >
                {pendingAction?.startsWith("place:")
                  ? "Submitting..."
                  : `Place ${limitSide === "bid" ? "Bid" : "Ask"}`}
              </button>
            </div>
          </div>

          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="flex items-center gap-2">
              <ArrowRightLeft size={16} className="text-zinc-500" />
              <h2 className="text-sm font-semibold text-zinc-900">Market Order Ticket</h2>
            </div>

            <div className="mt-4 grid gap-4">
              <div className="inline-flex rounded-lg border border-zinc-200 bg-zinc-50 p-1">
                {(["buy", "sell"] as const).map((side) => (
                  <button
                    key={side}
                    onClick={() => setMarketSide(side)}
                    className={`rounded-md px-3 py-2 text-sm font-medium transition-colors ${
                      marketSide === side
                        ? "bg-white text-zinc-900 shadow-sm"
                        : "text-zinc-500 hover:text-zinc-900"
                    }`}
                  >
                    {side === "buy" ? "Buy" : "Sell"}
                  </button>
                ))}
              </div>

              <label className="text-sm text-zinc-600">
                Amount ({selectedBase.symbol})
                <input
                  type="number"
                  step="0.01"
                  value={marketAmount}
                  onChange={(event) => setMarketAmount(event.target.value)}
                  className="mt-1 w-full rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
                />
              </label>

              <label className="text-sm text-zinc-600">
                {marketSide === "buy"
                  ? `Max ${activeQuoteToken.symbol} In`
                  : `Min ${activeQuoteToken.symbol} Out`}
                <input
                  type="number"
                  step="0.01"
                  value={marketGuard}
                  onChange={(event) => setMarketGuard(event.target.value)}
                  placeholder="Optional guard"
                  className="mt-1 w-full rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
                />
              </label>

              <div className="rounded-lg bg-zinc-50 px-3 py-3 text-xs text-zinc-500">
                Market orders revert if the book cannot fill the full requested size. Unused
                quote from market buys stays in your darkpool internal balance.
              </div>

              {selectedBaseEnabled && !marketFundingBalanceLoaded && (
                <div
                  className={`rounded-lg border px-3 py-2 text-xs ${
                    marketFundingBalanceFailed
                      ? "border-red-200 bg-red-50 text-red-700"
                      : "border-zinc-200 bg-zinc-50 text-zinc-500"
                  }`}
                >
                  {marketFundingBalanceFailed
                    ? `${marketFundingSymbol} wallet balance read failed. Use Refresh to retry.`
                    : `Loading ${marketFundingSymbol} wallet balance...`}
                </div>
              )}

              <button
                onClick={() => void handleMarketOrder()}
                disabled={
                  pendingAction !== null ||
                  !isOnZoneChain ||
                  !selectedBaseEnabled ||
                  !marketFundingBalanceLoaded
                }
                className="rounded-lg bg-zinc-900 px-4 py-3 text-sm font-medium text-white transition-colors hover:bg-zinc-800 disabled:opacity-50"
              >
                {pendingAction?.startsWith("market:")
                  ? "Submitting..."
                  : `Run Market ${marketSide === "buy" ? "Buy" : "Sell"}`}
              </button>
            </div>
          </div>
        </section>

        <section className="mt-6 grid gap-6 lg:grid-cols-[1.2fr_0.8fr]">
          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="flex items-center gap-2">
              <Search size={16} className="text-zinc-500" />
              <h2 className="text-sm font-semibold text-zinc-900">Private Order Inspector</h2>
            </div>

            <div className="mt-4 flex flex-col gap-3 md:flex-row">
              <input
                type="text"
                inputMode="numeric"
                value={orderLookup}
                onChange={(event) => setOrderLookup(event.target.value)}
                placeholder="Order id"
                className="min-w-0 flex-1 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none"
              />
              <button
                onClick={() => void handleLoadOrder()}
                disabled={pendingAction !== null || !isOnZoneChain}
                className="rounded-lg border border-zinc-300 px-3 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 disabled:opacity-50"
              >
                {pendingAction === "loadOrder" ? "Loading..." : "Load Order"}
              </button>
              <button
                onClick={() => void handleCancelOrder()}
                disabled={pendingAction !== null || !isOnZoneChain}
                className="rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-800 disabled:opacity-50"
              >
                {pendingAction === "cancel" ? "Cancelling..." : "Cancel Order"}
              </button>
            </div>

            {knownOrderIds.length > 0 && (
              <div className="mt-4 flex flex-wrap gap-2">
                {knownOrderIds.map((orderId) => (
                  <button
                    key={orderId.toString()}
                    onClick={() => void handleLoadOrder(orderId)}
                    className="rounded-full border border-zinc-200 bg-zinc-50 px-3 py-1 text-xs font-medium text-zinc-600 transition-colors hover:border-zinc-300 hover:text-zinc-900"
                  >
                    #{orderId.toString()}
                  </button>
                ))}
              </div>
            )}

            {(loadedOrder || loadedOrderHistory) && (
              <div className="mt-4 rounded-lg border border-zinc-200 bg-zinc-50 p-4 text-sm text-zinc-700">
                <div className="flex items-center justify-between">
                  <div className="font-medium text-zinc-900">
                    Order #{(loadedOrder?.orderId ?? loadedOrderHistory?.orderId)?.toString()}
                  </div>
                  <div className="flex gap-2">
                    <span className="rounded bg-white px-2 py-1 text-xs font-medium text-zinc-600">
                      {formatOrderSide(loadedOrder?.isBid ?? loadedOrderHistory?.isBid)}
                    </span>
                    <span className="rounded bg-white px-2 py-1 text-xs font-medium text-zinc-600">
                      {orderHistoryStatus(loadedOrderHistory, loadedOrder)}
                    </span>
                  </div>
                </div>
                <div className="mt-3 grid gap-2 md:grid-cols-2">
                  <div>
                    Base:{" "}
                    {findToken(loadedOrder?.base ?? loadedOrderHistory?.base)?.symbol ??
                      loadedOrder?.base ??
                      loadedOrderHistory?.base ??
                      "--"}
                  </div>
                  <div>
                    Quote:{" "}
                    {findToken(loadedOrder?.quote ?? loadedOrderHistory?.quote)?.symbol ??
                      loadedOrder?.quote ??
                      loadedOrderHistory?.quote ??
                      "--"}
                  </div>
                  <div>
                    Placed:{" "}
                    {formatHistoryAmount(
                      loadedOrderHistory?.placedAmount,
                      loadedOrder?.base ?? loadedOrderHistory?.base,
                      selectedBase.decimals,
                    )}
                  </div>
                  <div>
                    Filled:{" "}
                    {formatHistoryAmount(
                      totalFilled(loadedOrderHistory),
                      loadedOrder?.base ?? loadedOrderHistory?.base,
                      selectedBase.decimals,
                    )}
                  </div>
                  <div>
                    Remaining:{" "}
                    {loadedOrder
                      ? formatHistoryAmount(
                          loadedOrder.quantity,
                          loadedOrder.base,
                          selectedBase.decimals,
                        )
                      : "--"}
                  </div>
                  <div>
                    Raw price:{" "}
                    {(loadedOrder?.price ?? loadedOrderHistory?.placedPrice)?.toString() ??
                      loadedOrderHistory?.fills[0]?.price.toString() ??
                      "--"}
                  </div>
                </div>
                <div className="mt-3 break-all font-mono text-[11px] text-zinc-500">
                  Maker {loadedOrder?.maker ?? loadedOrderHistory?.maker ?? "--"}
                </div>
                {loadedOrderHistory && loadedOrderHistory.fills.length > 0 && (
                  <div className="mt-4 border-t border-zinc-200 pt-3">
                    <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
                      Fills
                    </div>
                    <div className="mt-2 space-y-2">
                      {loadedOrderHistory.fills.map((fill, index) => (
                        <div
                          key={`${fill.transactionHash ?? "fill"}-${fill.logIndex ?? index}`}
                          className="rounded border border-zinc-200 bg-white px-3 py-2 text-xs text-zinc-600"
                        >
                          <div className="flex flex-wrap justify-between gap-2">
                            <span>
                              {formatHistoryAmount(
                                fill.amountFilled,
                                loadedOrder?.base ?? loadedOrderHistory.base,
                                selectedBase.decimals,
                              )}{" "}
                              at raw {fill.price.toString()}
                            </span>
                            <span>Block {fill.blockNumber?.toString() ?? "--"}</span>
                          </div>
                          <div className="mt-1 break-all font-mono text-[11px] text-zinc-500">
                            Maker {shortHex(fill.maker)} · Taker {shortHex(fill.taker)} · Tx{" "}
                            {shortHex(fill.transactionHash)}
                          </div>
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </div>
            )}

            {recentOrderHistories.length > 0 && (
              <div className="mt-4 rounded-lg border border-zinc-200 bg-white p-3">
                <div className="text-xs font-medium uppercase tracking-wide text-zinc-500">
                  Recent Order History
                </div>
                <div className="mt-2 space-y-2">
                  {recentOrderHistories.map((history) => (
                    <button
                      key={history.orderId.toString()}
                      onClick={() => void handleLoadOrder(history.orderId)}
                      className="block w-full rounded border border-zinc-200 px-3 py-2 text-left text-xs text-zinc-600 transition-colors hover:bg-zinc-50"
                    >
                      <div className="font-medium text-zinc-800">
                        {orderHistorySummary(history)}
                      </div>
                      <div className="mt-1 text-zinc-500">
                        {orderHistoryStatus(
                          history,
                          loadedOrder?.orderId === history.orderId ? loadedOrder : null,
                        )}
                      </div>
                    </button>
                  ))}
                </div>
              </div>
            )}

            <div className="mt-4 flex items-start gap-2 rounded-lg bg-amber-50 px-3 py-3 text-xs text-amber-800">
              <ShieldCheck size={14} className="mt-0.5 shrink-0" />
              <div>
                Open orders are read with maker-scoped `getOrder`. Filled or cancelled orders are
                reconstructed from darkpool events because they no longer exist in order storage.
              </div>
            </div>
          </div>

          <div className="rounded-xl border border-zinc-200 bg-white p-5">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-semibold text-zinc-900">Session & Activity</div>
                <div className="text-xs text-zinc-500">
                  Connected {address} · Chain {chainId}
                </div>
              </div>
              <button
                onClick={() => void refetchAll()}
                className="rounded-lg border border-zinc-300 px-2.5 py-2 text-zinc-600 transition-colors hover:bg-zinc-100"
              >
                <RefreshCw size={14} />
              </button>
            </div>

            {authToken && (
              <div className="mt-4 rounded-lg bg-zinc-50 px-3 py-3 text-xs text-zinc-500">
                Private RPC session cached until{" "}
                <span className="font-medium text-zinc-700">
                  {new Date(authToken.expiresAt * 1000).toLocaleTimeString()}
                </span>
                .
              </div>
            )}

            <div className="mt-4 space-y-3">
              {activity.length === 0 ? (
                <div className="rounded-lg bg-zinc-50 px-3 py-4 text-sm text-zinc-500">
                  No actions yet. Funding, orders, and private reads will appear here.
                </div>
              ) : (
                activity.map((item) => (
                  <div
                    key={item.id}
                    className="rounded-lg border border-zinc-200 px-3 py-3 text-sm text-zinc-700"
                  >
                    <div className="flex items-center justify-between gap-3">
                      <div className="font-medium text-zinc-900">{item.label}</div>
                      <div>
                        {item.status === "success" && (
                          <Check size={14} className="text-emerald-600" />
                        )}
                        {item.status === "pending" && (
                          <Loader2 size={14} className="animate-spin text-zinc-500" />
                        )}
                        {item.status === "error" && (
                          <AlertCircle size={14} className="text-red-600" />
                        )}
                      </div>
                    </div>
                    <div className="mt-1 break-all text-xs text-zinc-500">{item.detail}</div>
                  </div>
                ))
              )}
            </div>
          </div>
        </section>

        {error && (
          <div className="mt-6 flex items-start gap-2 rounded-xl border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700">
            <AlertCircle size={16} className="mt-0.5 shrink-0" />
            {error}
          </div>
        )}
      </main>
    </div>
  );
}
