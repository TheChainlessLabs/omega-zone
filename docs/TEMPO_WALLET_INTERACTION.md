# Tempo Wallet Direct Interaction

This document explains how a web app should interact directly with Tempo
Wallet for zone reads and writes.

There are two different paths:

- L1 portal/token writes can use normal wallet transaction methods.
- Zone writes should be locally signed and submitted to the private zone RPC as
  raw transactions.

The current frontend reference implementation is:

- `frontend/lib/config.ts` for chain registration and connectors
- `frontend/lib/zone-auth.ts` for private RPC authorization tokens
- `frontend/components/darkpool-dashboard.tsx` for darkpool access-key signing
  and raw transaction submission

## Register Tempo Wallet

The frontend uses wagmi with the Tempo Wallet connector:

```ts
import { createConfig, http } from "wagmi";
import { injected, tempoWallet } from "wagmi/connectors";
import { defineChain } from "viem";
import { tempoModerato } from "viem/chains";

export const zoneChain = defineChain({
  id: Number(process.env.NEXT_PUBLIC_ZONE_CHAIN_ID),
  name: "Omega Zone",
  nativeCurrency: { name: "USD", symbol: "USD", decimals: 18 },
  rpcUrls: {
    default: {
      http: [process.env.NEXT_PUBLIC_ZONE_RPC ?? "http://localhost:8546"],
    },
  },
});

export const config = createConfig({
  chains: [tempoModerato, zoneChain],
  connectors: [
    injected({ unstable_shimAsyncInject: true }),
    tempoWallet({ name: "Tempo Wallet" }),
  ],
  transports: {
    [tempoModerato.id]: http(process.env.NEXT_PUBLIC_TEMPO_RPC),
    [zoneChain.id]: http(process.env.NEXT_PUBLIC_ZONE_RPC),
  },
});
```

Before any zone action, ensure the connected wallet is on the zone chain:

```ts
await switchChainAsync({ chainId: zoneChain.id });
```

## L1 Portal Writes

For L1 approvals and portal deposits, use the connected wallet normally. These
transactions target Tempo L1, not the zone private RPC.

```ts
await switchChainAsync({ chainId: tempoModerato.id });

const approveHash = await walletClient.writeContract({
  account: address,
  chain: tempoModerato,
  address: pathUsdAddress,
  abi: tip20Abi,
  functionName: "approve",
  args: [portalAddress, amount],
  gas: 500_000n,
});

const depositHash = await walletClient.writeContract({
  account: address,
  chain: tempoModerato,
  address: portalAddress,
  abi: zonePortalAbi,
  functionName: "deposit",
  args: [pathUsdAddress, address, amount, zeroMemo],
  gas: 900_000n,
});
```

The app should check the approval receipt and confirm `allowance(owner, portal)
>= amount` before opening the deposit signature.

## Private RPC Authorization

The private zone RPC requires an `X-Authorization-Token` header. Build the token
by asking Tempo Wallet to `personal_sign` a zone-session digest.

The signed digest fields are:

```text
keccak256("TempoZoneRPC" padded to 32 bytes || fields)

fields =
  version:   1 byte
  zoneId:    4 bytes, big-endian
  chainId:   8 bytes, big-endian
  issuedAt:  8 bytes, big-endian unix seconds
  expiresAt: 8 bytes, big-endian unix seconds
```

The token sent to the RPC is:

```text
<signature bytes><fields bytes>
```

Minimal provider request:

```ts
const signature = await provider.request({
  method: "personal_sign",
  params: [digestHex, address],
});
```

Then call the private RPC with:

```ts
await fetch(zoneRpcUrl, {
  method: "POST",
  headers: {
    "Content-Type": "application/json",
    "X-Authorization-Token": token,
  },
  body: JSON.stringify({
    jsonrpc: "2.0",
    method: "zone_getAuthorizationTokenInfo",
    params: [],
    id: 1,
  }),
});
```

Cache the token until it is close to expiry. If the private RPC reports an
expired or invalid token, clear the cache and ask the wallet to sign a fresh
session token.

## Zone Reads

For owner-scoped reads, set `from` to the connected wallet address.

```ts
const data = encodeFunctionData({
  abi: tip20Abi,
  functionName: "balanceOf",
  args: [address],
});

const result = await privateRpc({
  method: "eth_call",
  params: [{ from: address, to: pathUsdAddress, data }, "latest"],
});
```

This matters because zone TIP-20 balances and darkpool balances are
caller-gated.

## Do Not Use Wallet `eth_sendTransaction` For Zone Writes

The private zone RPC does not hold user signing keys and rejects
`eth_sendTransaction` / `eth_signTransaction`. A zone write must be:

1. encoded by the app
2. signed locally by a user-authorized key
3. submitted with `eth_sendRawTransaction` or `eth_sendRawTransactionSync`

This avoids wallet-side transaction filling recursion on zone-only precompile
calls and gives the private RPC a signed raw transaction whose sender it can
verify against the authorization token.

## Authorize A Darkpool Access Key

For darkpool actions, ask Tempo Wallet to authorize a local access key scoped to
the darkpool precompile.

Current darkpool selectors:

```ts
const darkpoolScopes = [
  { address: DARKPOOL, selector: toFunctionSelector("deposit(address,uint128)") },
  { address: DARKPOOL, selector: toFunctionSelector("withdraw(address,uint128)") },
  { address: DARKPOOL, selector: toFunctionSelector("place(address,uint128,uint128,bool)") },
  { address: DARKPOOL, selector: toFunctionSelector("cancel(uint128)") },
  { address: DARKPOOL, selector: toFunctionSelector("marketBuy(address,uint128,uint128)") },
  { address: DARKPOOL, selector: toFunctionSelector("marketSell(address,uint128,uint128)") },
];
```

Request shape:

```ts
const UINT128_MAX = (1n << 128n) - 1n;

await provider.request({
  method: "wallet_authorizeAccessKey",
  params: [
    {
      chainId: BigInt(zoneChain.id),
      expiry: Math.floor(Date.now() / 1000) + 24 * 60 * 60,
      limits: bridgeTokens.map((token) => ({
        token: token.address,
        limit: UINT128_MAX,
      })),
      scopes: darkpoolScopes,
    },
  ],
});
```

After authorization, read the local access key from the provider store. The
current frontend expects an entry with:

- `access` equal to the connected root wallet
- a local `keyPair`
- matching `limits`
- matching `scopes`
- optional pending `keyAuthorization` for first use

## Sign A Zone Transaction Locally

Build nonce and EIP-1559 fee defaults from the zone public client before
signing:

```ts
const [nonce, fees] = await Promise.all([
  publicClient.getTransactionCount({ address, blockTag: "pending" }),
  publicClient.estimateFeesPerGas(),
]);
```

Hydrate the authorized local key and sign a Tempo typed transaction:

```ts
import { Account as TempoAccount, Transaction as TempoTransaction } from "viem/tempo";

const accessKeyAccount = TempoAccount.fromWebCryptoP256(accessKey.keyPair, {
  access: address,
  internal_version: "v2",
});

const signed = await accessKeyAccount.signTransaction(
  {
    type: "tempo",
    chainId: zoneChain.id,
    nonce,
    maxFeePerGas: fees.maxFeePerGas,
    maxPriorityFeePerGas: fees.maxPriorityFeePerGas,
    gas,
    calls: [{ to, data }],
    keyAuthorization: accessKey.keyAuthorization,
  },
  { serializer: TempoTransaction.serialize },
);
```

If `keyAuthorization` is present, add extra gas for the first transaction that
installs the access key. The current frontend uses a `16_000_000` gas overhead
for that first submission, then clears `keyAuthorization` from the cached key.

## Submit The Raw Transaction

Submit through the private RPC with the zone auth token:

```ts
const response = await privateRpc({
  method: "eth_sendRawTransaction",
  params: [signed],
});
```

`eth_sendRawTransactionSync` is also available when the caller wants the RPC to
wait for inclusion and return the receipt.

The private RPC decodes the raw transaction, recovers the sender, and checks it
matches the account that signed the `X-Authorization-Token`.

## Common Errors

| Error | Meaning | Fix |
|---|---|---|
| `Unauthorized` / HTTP 401 or 403 | Missing, expired, or wrong auth token | Ask Tempo Wallet for a fresh `personal_sign` session token. |
| `Transaction rejected` | Raw transaction sender does not match the auth-token account | Sign the transaction with the same root/access account for the connected wallet. |
| `does not hold caller signing keys` | App called `eth_sendTransaction` or `eth_signTransaction` on the private RPC | Locally sign and submit `eth_sendRawTransaction`. |
| `Account mismatch` | `eth_call` / `eth_estimateGas` used the wrong `from` | Set `from` to the connected account or omit it where allowed. |
| `KeyAuthorization chain_id mismatch` | Access key was authorized for the wrong chain | Include `chainId: BigInt(zoneChain.id)` and clear stale cached keys. |
| `InvalidSpendingLimit` | Access key limit exceeds the TIP-20 `uint128` range | Use `uint128::MAX`, not `uint256::MAX`. |
| `call scopes are not active before T3` | Zone genesis lacks T3 activation | Restart from a genesis with Tempo fork fields enabled, or use a limits-only flow for pre-T3 zones. |

## Minimal Flow

1. Connect Tempo Wallet.
2. Register and switch to the zone chain.
3. Ask Tempo Wallet to `personal_sign` a private RPC session token.
4. For darkpool writes, call `wallet_authorizeAccessKey` with darkpool scopes.
5. Encode the darkpool call.
6. Estimate gas against the zone public client and add a buffer.
7. Sign a Tempo typed transaction locally with the authorized access key.
8. Submit the signed raw transaction to the private RPC with
   `X-Authorization-Token`.

For L1 portal approvals and deposits, use normal wallet `writeContract` calls
on Tempo L1 instead of this raw zone transaction path.
