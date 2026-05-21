"use client";

import Link from "next/link";
import { useAccount } from "wagmi";
import { WalletConnect } from "@/components/wallet-connect";
import { Faucet } from "@/components/faucet";
import { Bridge } from "@/components/bridge";
import { ZoneBalance } from "@/components/zone-balance";
import { ArrowRight } from "lucide-react";

export default function Home() {
  const { isConnected, chainId, address } = useAccount();

  return (
    <div className="min-h-screen bg-zinc-50">
      <header className="border-b border-zinc-200 bg-white">
        <div className="max-w-3xl mx-auto px-6 h-16 flex items-center justify-between">
          <div className="flex items-center gap-4">
            <h1 className="text-lg font-semibold text-zinc-900 tracking-tight">
              Omega Zone
            </h1>
            <Link
              href="/darkpool"
              className="text-sm font-medium text-zinc-500 transition-colors hover:text-zinc-900"
            >
              Darkpool
            </Link>
          </div>
          <WalletConnect />
        </div>
      </header>

      <main className="max-w-3xl mx-auto px-6 py-12">
        {!isConnected ? (
          <div className="text-center py-24">
            <h2 className="text-2xl font-semibold text-zinc-900 mb-2">
              Connect your wallet
            </h2>
            <p className="text-zinc-500 mb-4">
              Connect with an EVM wallet to get test funds and bridge to the zone.
            </p>
            <div className="max-w-md mx-auto p-4 rounded-lg bg-blue-50 border border-blue-200 text-sm text-blue-800 text-left">
              <p className="font-medium mb-2">Tempo gas model</p>
              <ul className="list-disc pl-4 space-y-1 text-blue-700">
                <li>Tempo has <strong>no native gas token</strong> — fees are paid in stablecoins (pathUSD)</li>
                <li>MetaMask may show &quot;insufficient balance&quot; errors due to native balance checks</li>
                <li>We recommend <strong>Rabby</strong> or <strong>Rainbow</strong> which handle this correctly</li>
                <li>The faucet provides both testnet USD and pathUSD for fees</li>
              </ul>
            </div>
          </div>
        ) : (
          <div className="space-y-8">
            <div className="flex items-center gap-3 p-4 rounded-xl bg-white border border-zinc-200">
              <div className="flex-1">
                <div className="text-xs text-zinc-500 font-medium uppercase tracking-wide">
                  Connected
                </div>
                <div className="text-sm font-mono text-zinc-700 mt-0.5">
                  {address}
                </div>
              </div>
              <div className="px-3 py-1 rounded-full bg-zinc-100 text-xs font-medium text-zinc-600">
                Chain {chainId}
              </div>
            </div>

            <div className="grid gap-6 md:grid-cols-2">
              <div className="p-6 rounded-xl bg-white border border-zinc-200">
                <Faucet />
              </div>
              <div className="p-6 rounded-xl bg-white border border-zinc-200">
                <Bridge />
              </div>
            </div>

            <ZoneBalance />

            <div className="p-6 rounded-xl bg-white border border-zinc-200">
              <h3 className="font-semibold text-zinc-900 mb-2">Flow</h3>
              <div className="flex items-center gap-4 text-sm text-zinc-600">
                <span className="px-3 py-1 rounded-full bg-zinc-100 font-medium">
                  1. Connect wallet
                </span>
                <ArrowRight size={14} />
                <span className="px-3 py-1 rounded-full bg-zinc-100 font-medium">
                  2. Request faucet funds
                </span>
                <ArrowRight size={14} />
                <span className="px-3 py-1 rounded-full bg-zinc-100 font-medium">
                  3. Bridge to zone
                </span>
                <ArrowRight size={14} />
                <span className="px-3 py-1 rounded-full bg-zinc-100 font-medium">
                  4. Trade on zone
                </span>
              </div>
            </div>
          </div>
        )}
      </main>
    </div>
  );
}
