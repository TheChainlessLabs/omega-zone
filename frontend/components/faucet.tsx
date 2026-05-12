"use client";

import { useState } from "react";
import { useAccount } from "wagmi";
import { Droplets, Loader2 } from "lucide-react";

interface FundResult {
  txHash: string;
  native?: string;
  pathUSD?: string;
}

export function Faucet() {
  const { address, isConnected } = useAccount();
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<FundResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const requestFunds = async () => {
    if (!address) return;
    setLoading(true);
    setError(null);
    setResult(null);

    try {
      const provider = (window as { ethereum?: unknown }).ethereum;
      if (!provider) throw new Error("No wallet provider found");

      const rpcUrl =
        process.env.NEXT_PUBLIC_TEMPO_RPC || "https://rpc.moderato.tempo.xyz";

      const res = await fetch(rpcUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          jsonrpc: "2.0",
          method: "tempo_fundAddress",
          params: [address],
          id: 1,
        }),
      });

      const json = await res.json();
      if (json.error) throw new Error(json.error.message || "RPC error");

      setResult({
        txHash: json.result?.txHash || "",
        native: json.result?.native
          ? `${BigInt(json.result.native) / 10n ** 18n} ETH`
          : undefined,
        pathUSD: json.result?.pathUSD ?? undefined,
      });
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to get funds");
    } finally {
      setLoading(false);
    }
  };

  if (!isConnected) {
    return (
      <div className="text-sm text-zinc-400">
        Connect your wallet to request test funds
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h3 className="font-semibold text-zinc-900">Testnet Faucet</h3>
          <p className="text-sm text-zinc-500">
            Get test USDC and ETH on Tempo Moderato
          </p>
        </div>
        <button
          onClick={requestFunds}
          disabled={loading}
          className="flex items-center gap-2 px-4 py-2 rounded-lg bg-zinc-900 text-white text-sm font-medium hover:bg-zinc-800 disabled:opacity-50 transition-colors"
        >
          {loading ? (
            <Loader2 size={16} className="animate-spin" />
          ) : (
            <Droplets size={16} />
          )}
          Request Funds
        </button>
      </div>

      {result && (
        <div className="p-3 rounded-lg bg-emerald-50 border border-emerald-200 text-sm text-emerald-800">
          <p className="font-medium">Funds sent</p>
          {result.native && <p>Native: {result.native}</p>}
          {result.pathUSD && <p>pathUSD: {result.pathUSD}</p>}
          {result.txHash && (
            <p className="text-xs mt-1 truncate text-emerald-600">
              tx: {result.txHash}
            </p>
          )}
        </div>
      )}

      {error && (
        <div className="p-3 rounded-lg bg-red-50 border border-red-200 text-sm text-red-700">
          {error}
        </div>
      )}
    </div>
  );
}
