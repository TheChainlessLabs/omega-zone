"use client";

import { useMemo } from "react";
import {
  useAccount,
  useConnect,
  useConnectors,
  useDisconnect,
  useSwitchChain,
} from "wagmi";
import { ChevronDown, Loader2, LogOut } from "lucide-react";
import { tempoChain, zoneChain } from "@/lib/config";

export function WalletConnect() {
  const { address, chainId, isConnected } = useAccount();
  const connectors = useConnectors();
  const connect = useConnect();
  const disconnect = useDisconnect();
  const switchChain = useSwitchChain();

  const connector = useMemo(
    () =>
      connectors.find(
        (item) => item.name === "Tempo Wallet" || item.id === "xyz.tempo",
      ) ?? connectors[0],
    [connectors],
  );

  const onConnect = async () => {
    if (!connector) return;
    await connect.connectAsync({ connector });
  };

  const shortAddress = address
    ? `${address.slice(0, 6)}...${address.slice(-4)}`
    : null;

  const activeChainLabel =
    chainId === tempoChain.id
      ? "Tempo"
      : chainId === zoneChain.id
        ? "Omega Zone"
        : chainId
          ? `Chain ${chainId}`
          : "No network";

  if (!isConnected) {
    return (
      <button
        onClick={() => void onConnect()}
        disabled={connect.isPending || !connector}
        className="inline-flex items-center gap-2 rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-800 disabled:opacity-50"
      >
        {connect.isPending ? (
          <Loader2 size={14} className="animate-spin" />
        ) : (
          <ChevronDown size={14} />
        )}
        {connect.isPending ? "Opening Tempo Wallet..." : "Open Tempo Wallet"}
      </button>
    );
  }

  return (
    <div className="flex items-center gap-2">
      <div className="hidden rounded-lg border border-zinc-200 bg-white px-3 py-2 text-right sm:block">
        <div className="text-[11px] font-medium uppercase tracking-wide text-zinc-500">
          {activeChainLabel}
        </div>
        <div className="text-sm font-medium text-zinc-800">{shortAddress}</div>
      </div>

      <div className="flex items-center gap-2">
        <button
          onClick={() =>
            void switchChain.switchChainAsync({
              chainId: tempoChain.id,
              addEthereumChainParameter: {
                nativeCurrency: {
                  name: "USD",
                  symbol: "USD",
                  decimals: 18,
                },
              },
            })
          }
          disabled={switchChain.isPending}
          className="rounded-lg border border-zinc-200 bg-white px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-50 disabled:opacity-50"
        >
          Tempo
        </button>
        <button
          onClick={() =>
            void switchChain.switchChainAsync({
              chainId: zoneChain.id,
              addEthereumChainParameter: {
                nativeCurrency: {
                  name: "USD",
                  symbol: "USD",
                  decimals: 18,
                },
              },
            })
          }
          disabled={switchChain.isPending}
          className="rounded-lg border border-zinc-200 bg-white px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-50 disabled:opacity-50"
        >
          Zone
        </button>
        <button
          onClick={() => disconnect.disconnect()}
          disabled={disconnect.isPending}
          className="inline-flex items-center gap-2 rounded-lg border border-zinc-200 bg-white px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-50 disabled:opacity-50"
        >
          {disconnect.isPending ? (
            <Loader2 size={14} className="animate-spin" />
          ) : (
            <LogOut size={14} />
          )}
          <span className="hidden sm:inline">Disconnect</span>
        </button>
      </div>
    </div>
  );
}
