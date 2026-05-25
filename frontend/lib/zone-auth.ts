import { toHex, type Address, type Hex } from "viem";

const ZONE_RPC = process.env.NEXT_PUBLIC_ZONE_RPC || "http://localhost:8546";
const ZONE_ID = Number(process.env.NEXT_PUBLIC_ZONE_ID || 25);
const AUTH_TOKEN_TTL_SECS = 3600;
const AUTH_TOKEN_REFRESH_BUFFER_SECS = 30;

const TEMPO_ZONE_RPC_MAGIC = new TextEncoder().encode("TempoZoneRPC");
const MAGIC_PADDED = new Uint8Array(32);
MAGIC_PADDED.set(TEMPO_ZONE_RPC_MAGIC);

export type CachedZoneAuthToken = {
  account: `0x${string}`;
  chainId: number;
  zoneId: number;
  token: string;
  expiresAt: number;
};

export type ZoneAuthProvider = {
  request: (args: { method: string; params: unknown[] }) => Promise<unknown>;
  store?: {
    getState: () => {
      accessKeys?: readonly {
        address?: Address;
        access: Address;
        expiry?: number;
        keyAuthorization?: unknown;
        keyPair?: unknown;
        limits?: readonly { token: Address; limit: bigint; period?: number }[];
        privateKey?: Hex;
        scopes?: readonly {
          address: Address;
          selector?: Hex | string;
        }[];
      }[];
    };
    setState?: (state: {
      accessKeys?: readonly {
        address?: Address;
        access: Address;
        expiry?: number;
        keyAuthorization?: unknown;
        keyPair?: unknown;
        limits?: readonly { token: Address; limit: bigint; period?: number }[];
        privateKey?: Hex;
        scopes?: readonly {
          address: Address;
          selector?: Hex | string;
        }[];
      }[];
    }) => void;
  };
};

function buildAuthDigest(chainId: number, now: number, expiresAt: number): Uint8Array {
  const msg = new Uint8Array(32 + 1 + 4 + 8 + 8 + 8);
  msg.set(MAGIC_PADDED, 0);

  const dv = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  msg[32] = 0x00;
  dv.setUint32(33, ZONE_ID, false);
  dv.setBigUint64(37, BigInt(chainId), false);
  dv.setBigUint64(45, BigInt(now), false);
  dv.setBigUint64(53, BigInt(expiresAt), false);

  return msg;
}

function buildToken(sigBytes: Uint8Array, chainId: number, now: number, expiresAt: number): string {
  const fields = new Uint8Array(1 + 4 + 8 + 8 + 8);
  fields[0] = 0x00;
  const dv = new DataView(fields.buffer, fields.byteOffset, fields.byteLength);
  dv.setUint32(1, ZONE_ID, false);
  dv.setBigUint64(5, BigInt(chainId), false);
  dv.setBigUint64(13, BigInt(now), false);
  dv.setBigUint64(21, BigInt(expiresAt), false);

  const token = new Uint8Array(sigBytes.length + fields.length);
  token.set(sigBytes, 0);
  token.set(fields, sigBytes.length);

  return toHex(token);
}

function getStorageKey(chainId: number) {
  return `omega-zone-auth:${ZONE_ID}:${chainId}`;
}

export function readCachedZoneAuthToken(
  account: `0x${string}`,
  chainId: number,
): CachedZoneAuthToken | null {
  const raw = window.sessionStorage.getItem(getStorageKey(chainId));
  if (!raw) return null;

  try {
    const parsed = JSON.parse(raw) as CachedZoneAuthToken;
    if (
      parsed.account !== account ||
      parsed.chainId !== chainId ||
      parsed.zoneId !== ZONE_ID ||
      typeof parsed.token !== "string" ||
      typeof parsed.expiresAt !== "number"
    ) {
      window.sessionStorage.removeItem(getStorageKey(chainId));
      return null;
    }

    return parsed;
  } catch {
    window.sessionStorage.removeItem(getStorageKey(chainId));
    return null;
  }
}

export function persistCachedZoneAuthToken(chainId: number, token: CachedZoneAuthToken | null) {
  const storageKey = getStorageKey(chainId);
  if (!token) {
    window.sessionStorage.removeItem(storageKey);
    return;
  }

  window.sessionStorage.setItem(storageKey, JSON.stringify(token));
}

export function zoneAuthTokenNearExpiry(expiresAt: number) {
  const now = Math.floor(Date.now() / 1000);
  return expiresAt <= now + AUTH_TOKEN_REFRESH_BUFFER_SECS;
}

export async function getZoneSessionToken({
  address,
  chainId,
  provider,
  currentToken,
  forceRefresh = false,
}: {
  address: `0x${string}`;
  chainId: number;
  provider: ZoneAuthProvider;
  currentToken?: CachedZoneAuthToken | null;
  forceRefresh?: boolean;
}): Promise<CachedZoneAuthToken> {
  if (!forceRefresh) {
    const cached = currentToken ?? readCachedZoneAuthToken(address, chainId);
    if (cached && !zoneAuthTokenNearExpiry(cached.expiresAt)) {
      return cached;
    }
  }

  const now = Math.floor(Date.now() / 1000);
  const expiresAt = now + AUTH_TOKEN_TTL_SECS;
  const digestHex = toHex(buildAuthDigest(chainId, now, expiresAt));
  const sigHex = (await provider.request({
    method: "personal_sign",
    params: [digestHex, address],
  })) as string;

  const sigBytes = new Uint8Array(sigHex.length / 2 - 1);
  for (let i = 2; i < sigHex.length; i += 2) {
    sigBytes[(i - 2) / 2] = Number.parseInt(sigHex.slice(i, i + 2), 16);
  }

  const nextToken = {
    account: address,
    chainId,
    zoneId: ZONE_ID,
    token: buildToken(sigBytes, chainId, now, expiresAt),
    expiresAt,
  } satisfies CachedZoneAuthToken;

  persistCachedZoneAuthToken(chainId, nextToken);
  return nextToken;
}

function shouldRefreshAuthToken(message: string) {
  return /expired|invalid signature|missing|zone id|chain id|issued/i.test(message);
}

export async function zonePrivateRpc<T>({
  address,
  chainId,
  provider,
  currentToken,
  body,
}: {
  address: `0x${string}`;
  chainId: number;
  provider: ZoneAuthProvider;
  currentToken?: CachedZoneAuthToken | null;
  body: Record<string, unknown>;
}): Promise<{ result: T; token: CachedZoneAuthToken }> {
  const doFetch = async (token: string) => {
    const res = await fetch(ZONE_RPC, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-authorization-token": token,
      },
      body: JSON.stringify(body),
    });
    return res.json();
  };

  let token = await getZoneSessionToken({
    address,
    chainId,
    provider,
    currentToken,
  });
  let json = await doFetch(token.token);
  if (json.error) {
    const message =
      typeof json.error.message === "string"
        ? json.error.message
        : JSON.stringify(json.error);
    if (!shouldRefreshAuthToken(message)) {
      throw new Error(message);
    }

    persistCachedZoneAuthToken(chainId, null);
    token = await getZoneSessionToken({
      address,
      chainId,
      provider,
      forceRefresh: true,
    });
    json = await doFetch(token.token);
    if (json.error) {
      throw new Error(json.error.message || JSON.stringify(json.error));
    }
  }

  return { result: json.result as T, token };
}

export { ZONE_ID, ZONE_RPC };
