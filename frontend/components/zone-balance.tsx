"use client";

import { useState, useCallback } from "react";
import { useAccount } from "wagmi";
import { zoneChain } from "@/lib/config";
import { TIP20_ABI, PATH_USD } from "@/lib/portal-abi";
import { keccak256, toHex, formatUnits, encodeFunctionData } from "viem";
import { RefreshCw, Loader2, ShieldCheck } from "lucide-react";

const ZONE_RPC = process.env.NEXT_PUBLIC_ZONE_RPC || "http://localhost:8546";
const ZONE_ID = Number(process.env.NEXT_PUBLIC_ZONE_ID || 25);

const TEMPO_ZONE_RPC_MAGIC = new TextEncoder().encode("TempoZoneRPC");
const MAGIC_PADDED = new Uint8Array(32);
MAGIC_PADDED.set(TEMPO_ZONE_RPC_MAGIC);

function buildAuthDigest(chainId: number, now: number, expiresAt: number): Uint8Array {
  const msg = new Uint8Array(32 + 1 + 4 + 8 + 8 + 8);
  msg.set(MAGIC_PADDED, 0);

  const dv = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  msg[32] = 0x00;                          // version
  dv.setUint32(33, ZONE_ID, false);        // zoneId
  dv.setBigUint64(37, BigInt(chainId), false); // chainId
  dv.setBigUint64(45, BigInt(now), false);     // issuedAt
  dv.setBigUint64(53, BigInt(expiresAt), false); // expiresAt

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

export function ZoneBalance() {
  const { address, isConnected, chainId } = useAccount();
  const [balance, setBalance] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onZone = chainId === zoneChain.id;

  const checkBalance = useCallback(async () => {
    if (!address) return;
    setLoading(true);
    setError(null);

    try {
      const ethereum = (window as { ethereum?: { request: (args: { method: string; params: unknown[] }) => Promise<unknown> } }).ethereum;
      if (!ethereum) throw new Error("No wallet provider");

      const chainId = zoneChain.id;
      const now = Math.floor(Date.now() / 1000);
      const expiresAt = now + 3600;
      const msg = buildAuthDigest(chainId, now, expiresAt);
      const digest = keccak256(msg);

      // Sign the 32-byte digest with personal_sign.
      // personal_sign interprets it as a hash and prepends
      // "\x19Ethereum Signed Message:\n32" before re-hashing.
      const digestHex = toHex(msg); // keccak256 of magic + fields
      const sigHex = await ethereum.request({
        method: "personal_sign",
        params: [digestHex, address],
      }) as string;

      const sigBytes = new Uint8Array(sigHex.length / 2 - 1);
      for (let i = 2; i < sigHex.length; i += 2) {
        sigBytes[(i - 2) / 2] = parseInt(sigHex.slice(i, i + 2), 16);
      }

      const tokenHex = buildToken(sigBytes, chainId, now, expiresAt);

      // eth_call balanceOf with auth token
      const callData = encodeFunctionData({
        abi: TIP20_ABI,
        functionName: "balanceOf",
        args: [address],
      });

      const res = await fetch(ZONE_RPC, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "x-authorization-token": tokenHex,
        },
        body: JSON.stringify({
          jsonrpc: "2.0",
          method: "eth_call",
          params: [
            { to: PATH_USD, data: callData },
            "latest",
          ],
          id: 1,
        }),
      });

      const json = await res.json();
      if (json.error) throw new Error(json.error.message || JSON.stringify(json.error));

      const raw = json.result as string;
      if (raw === "0x" || raw === "0x0") {
        setBalance("0");
      } else {
        setBalance(formatUnits(BigInt(raw), 6));
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally {
      setLoading(false);
    }
  }, [address]);

  if (!isConnected) return null;

  return (
    <div className="p-6 rounded-xl bg-white border border-zinc-200">
      <div className="flex items-center justify-between mb-3">
        <h3 className="font-semibold text-zinc-900">Zone Balance</h3>
        <button
          onClick={checkBalance}
          disabled={loading}
          className="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-zinc-900 text-white text-xs font-medium hover:bg-zinc-800 disabled:opacity-50 transition-colors"
        >
          {loading ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}
          Refresh
        </button>
      </div>

      {balance !== null ? (
        <div>
          <div className="flex items-baseline gap-2">
            <span className="text-2xl font-mono font-semibold text-zinc-900">
              {Number(balance).toFixed(2)}
            </span>
            <span className="text-sm text-zinc-500">pathUSD on zone</span>
          </div>
          {Number(balance) > 0 && (
            <div className="flex items-center gap-1.5 mt-1 text-xs text-emerald-600">
              <ShieldCheck size={12} />
              Deposit confirmed
            </div>
          )}
        </div>
      ) : (
        <div>
          <p className="text-sm text-zinc-500">
            Click Refresh to sign an auth token and check your private zone balance.
          </p>
          <p className="text-xs text-zinc-400 mt-1">
            Zone #{ZONE_ID} · Chain {zoneChain.id}
          </p>
        </div>
      )}

      {error && <p className="text-xs text-red-600 mt-2">{error}</p>}
    </div>
  );
}
