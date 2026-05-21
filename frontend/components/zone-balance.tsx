"use client";

import { useState, useCallback } from "react";
import { useAccount, useConnections } from "wagmi";
import { zoneChain } from "@/lib/config";
import { BRIDGE_TOKENS, TIP20_ABI } from "@/lib/portal-abi";
import { formatUnits, encodeFunctionData } from "viem";
import { RefreshCw, Loader2, ShieldCheck } from "lucide-react";
import {
  type CachedZoneAuthToken,
  type ZoneAuthProvider,
  ZONE_ID,
  zonePrivateRpc,
} from "@/lib/zone-auth";

export function ZoneBalance() {
  const { address, isConnected } = useAccount();
  const connections = useConnections();
  const [selectedTokenAddress, setSelectedTokenAddress] =
    useState<(typeof BRIDGE_TOKENS)[number]["address"]>(BRIDGE_TOKENS[0].address);
  const [balance, setBalance] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [authToken, setAuthToken] = useState<CachedZoneAuthToken | null>(null);
  const selectedToken =
    BRIDGE_TOKENS.find((token) => token.address === selectedTokenAddress) ?? BRIDGE_TOKENS[0];

  const checkBalance = useCallback(async () => {
    if (!address) return;
    setLoading(true);
    setError(null);

    try {
      const activeConnector = connections[0]?.connector;
      const provider = activeConnector
        ? ((await activeConnector.getProvider({
            chainId: zoneChain.id,
          })) as ZoneAuthProvider | undefined)
        : undefined;
      if (!provider) throw new Error("No connected wallet provider");

      // eth_call balanceOf with auth token and explicit from
      const callData = encodeFunctionData({
        abi: TIP20_ABI,
        functionName: "balanceOf",
        args: [address],
      });
      const { result, token } = await zonePrivateRpc<string>({
        address,
        chainId: zoneChain.id,
        provider,
        currentToken: authToken,
        body: {
          jsonrpc: "2.0",
          method: "eth_call",
          params: [
            { to: selectedToken.address, from: address, data: callData },
            "latest",
          ],
          id: 1,
        },
      });
      setAuthToken(token);

      const raw = result;
      if (raw === "0x" || raw === "0x0") {
        setBalance("0");
      } else {
        setBalance(formatUnits(BigInt(raw), selectedToken.decimals));
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally {
      setLoading(false);
    }
  }, [address, authToken, connections, selectedToken.address, selectedToken.decimals]);

  if (!isConnected) return null;

  const activeAuthToken =
    authToken && authToken.account === address ? authToken : null;

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

      <div className="mb-3">
        <label className="text-xs text-zinc-500 font-medium">Token</label>
        <select
          value={selectedToken.address}
          onChange={(e) => {
            setSelectedTokenAddress(e.target.value as typeof selectedTokenAddress);
            setBalance(null);
            setError(null);
          }}
          disabled={loading}
          className="mt-1 w-full rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-900 outline-none disabled:opacity-50"
        >
          {BRIDGE_TOKENS.map((token) => (
            <option key={token.id} value={token.address}>
              {token.symbol} - {token.address}
            </option>
          ))}
        </select>
      </div>

      {balance !== null ? (
        <div>
          <div className="flex items-baseline gap-2">
            <span className="text-2xl font-mono font-semibold text-zinc-900">
              {Number(balance).toFixed(2)}
            </span>
            <span className="text-sm text-zinc-500">{selectedToken.symbol} on zone</span>
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
            Click Refresh to sign a private RPC session token and check your zone balance.
          </p>
          <p className="text-xs text-zinc-400 mt-1">
            Zone #{ZONE_ID} · Chain {zoneChain.id}
          </p>
        </div>
      )}

      {activeAuthToken && !error && (
        <p className="text-xs text-zinc-400 mt-2">
          Auth session cached until {new Date(activeAuthToken.expiresAt * 1000).toLocaleTimeString()}.
        </p>
      )}

      {error && <p className="text-xs text-red-600 mt-2">{error}</p>}
    </div>
  );
}
