"use client";

import { useState, useCallback } from "react";
import { useAccount, useReadContract, useSwitchChain, useWalletClient } from "wagmi";
import {
  decodeFunctionResult,
  encodeFunctionData,
  parseUnits,
  formatUnits,
  zeroAddress,
  zeroHash,
  type Hex,
  type TransactionReceipt,
} from "viem";
import { BRIDGE_TOKENS, ZONE_PORTAL_ABI, TIP20_ABI } from "@/lib/portal-abi";
import { tempoChain } from "@/lib/config";
import { ArrowRight, Loader2, Check, AlertCircle } from "lucide-react";

const PORTAL = process.env.NEXT_PUBLIC_ZONE_PORTAL as `0x${string}` | undefined;
const ZONE_ID = Number(process.env.NEXT_PUBLIC_ZONE_ID || 1);
const APPROVE_GAS_LIMIT = 500_000n;
const DEPOSIT_GAS_LIMIT = 900_000n;

export function Bridge() {
  const { address, chainId, isConnected } = useAccount();
  const { data: walletClient } = useWalletClient();
  const switchChain = useSwitchChain();
  const [amount, setAmount] = useState("");
  const [selectedTokenAddress, setSelectedTokenAddress] =
    useState<(typeof BRIDGE_TOKENS)[number]["address"]>(BRIDGE_TOKENS[0].address);
  const [step, setStep] = useState<"idle" | "approving" | "depositing" | "done">("idle");
  const [error, setError] = useState<string | null>(null);
  const [approveHash, setApproveHash] = useState<string | null>(null);
  const [depositHash, setDepositHash] = useState<string | null>(null);
  const selectedToken =
    BRIDGE_TOKENS.find((token) => token.address === selectedTokenAddress) ?? BRIDGE_TOKENS[0];
  const hasPortal = !!PORTAL && PORTAL !== zeroAddress;

  const { data: symbol } = useReadContract({
    abi: TIP20_ABI,
    address: selectedToken.address,
    functionName: "symbol",
    chainId: tempoChain.id,
    query: { enabled: isConnected },
  });
  const { data: decimals } = useReadContract({
    abi: TIP20_ABI,
    address: selectedToken.address,
    functionName: "decimals",
    chainId: tempoChain.id,
    query: { enabled: isConnected },
  });
  const { data: balance, refetch: refetchBalance } = useReadContract({
    abi: TIP20_ABI,
    address: selectedToken.address,
    functionName: "balanceOf",
    args: [address!],
    account: address,
    chainId: tempoChain.id,
    query: { enabled: !!address },
  });
  const { refetch: refetchAllowance } = useReadContract({
    abi: TIP20_ABI,
    address: selectedToken.address,
    functionName: "allowance",
    args: [address!, PORTAL ?? zeroAddress],
    account: address,
    chainId: tempoChain.id,
    query: { enabled: !!address && hasPortal },
  });

  const handleTokenChange = (tokenAddress: (typeof BRIDGE_TOKENS)[number]["address"]) => {
    setSelectedTokenAddress(tokenAddress);
    setStep("idle");
    setError(null);
    setApproveHash(null);
    setDepositHash(null);
  };

  const tokenDecimals = (decimals ?? selectedToken.decimals) as number;
  const tokenSymbol = (symbol ?? selectedToken.symbol) as string;
  const balanceStr = balance != null ? formatUnits(balance as bigint, tokenDecimals) : "0";

  const parsedAmount = useCallback(() => {
    try { return amount ? parseUnits(amount, tokenDecimals) : BigInt(0); }
    catch { return BigInt(0); }
  }, [amount, tokenDecimals]);

  const tempoRpcRequest = async <T,>(method: string, params: unknown[]): Promise<T> => {
    const res = await fetch(tempoChain.rpcUrls.default.http[0], {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method,
        params,
        id: 1,
      }),
    });
    const json = await res.json();
    if (json.error) {
      throw new Error(json.error.message || `${method} failed`);
    }
    return json.result as T;
  };

  const waitForReceipt = async (hash: `0x${string}`): Promise<TransactionReceipt> => {
    return tempoRpcRequest<TransactionReceipt>("eth_getTransactionReceipt", [hash]);
  };

  const pollReceipt = async (hash: `0x${string}`, timeoutMs = 120_000): Promise<TransactionReceipt> => {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      const receipt = await waitForReceipt(hash);
      if (receipt && receipt.blockNumber) return receipt;
      await new Promise((r) => setTimeout(r, 600));
    }
    throw new Error("Transaction receipt not found within timeout");
  };

  const receiptSucceeded = (receipt: TransactionReceipt) => {
    const status = String(receipt.status ?? "");
    return status === "success" || status === "0x1";
  };

  const readPortalAllowance = async () => {
    if (!address || !PORTAL) throw new Error("Connect your wallet first");
    const data = encodeFunctionData({
      abi: TIP20_ABI,
      functionName: "allowance",
      args: [address, PORTAL],
    });
    const result = await tempoRpcRequest<Hex>("eth_call", [
      { from: address, to: selectedToken.address, data },
      "latest",
    ]);
    return decodeFunctionResult({
      abi: TIP20_ABI,
      functionName: "allowance",
      data: result,
    }) as bigint;
  };

  const waitForPortalAllowance = async (required: bigint, timeoutMs = 30_000) => {
    const start = Date.now();
    let latest = BigInt(0);
    while (Date.now() - start < timeoutMs) {
      latest = await readPortalAllowance();
      if (latest >= required) return latest;
      await new Promise((r) => setTimeout(r, 800));
    }
    return latest;
  };

  const handleBridge = async () => {
    if (!walletClient || !address || !PORTAL) return;
    setError(null);
    const amt = parsedAmount();
    if (amt === BigInt(0)) return;

    try {
      if (chainId !== tempoChain.id) {
        await switchChain.switchChainAsync({
          chainId: tempoChain.id,
          addEthereumChainParameter: {
            nativeCurrency: {
              name: "USD",
              symbol: "USD",
              decimals: 18,
            },
          },
        });
      }

      if (balance != null && (balance as bigint) < amt) {
        throw new Error(`Insufficient ${tokenSymbol} balance`);
      }

      let currentAllowance = await readPortalAllowance();
      if (currentAllowance < amt) {
        setStep("approving");
        const approveTx = await walletClient.writeContract({
          chain: tempoChain,
          account: address,
          abi: TIP20_ABI,
          address: selectedToken.address,
          functionName: "approve",
          args: [PORTAL, amt],
          gas: APPROVE_GAS_LIMIT,
        });
        setApproveHash(approveTx);
        const approveReceipt = await pollReceipt(approveTx);
        if (!receiptSucceeded(approveReceipt)) {
          throw new Error("Approval transaction reverted. Deposit was not submitted.");
        }

        currentAllowance = await waitForPortalAllowance(amt);
        await Promise.all([refetchAllowance(), refetchBalance()]);
        if (currentAllowance < amt) {
          throw new Error(
            `Approval confirmed, but portal allowance is still ${formatUnits(
              currentAllowance,
              tokenDecimals,
            )} ${tokenSymbol}. Wait a few seconds and retry.`,
          );
        }
      }

      // Step 2: Deposit
      setStep("depositing");
      const depositTx = await walletClient.writeContract({
        chain: tempoChain,
        account: address,
        abi: ZONE_PORTAL_ABI,
        address: PORTAL,
        functionName: "deposit",
        args: [selectedToken.address, address, amt, zeroHash],
        gas: DEPOSIT_GAS_LIMIT,
      });
      setDepositHash(depositTx);
      const depositReceipt = await pollReceipt(depositTx);
      if (!receiptSucceeded(depositReceipt)) {
        throw new Error("Deposit transaction reverted");
      }
      await Promise.all([refetchAllowance(), refetchBalance()]);

      setStep("done");
    } catch (err: unknown) {
      setStep("idle");
      setError(err instanceof Error ? err.message : "Transaction failed");
    }
  };

  if (!isConnected) {
    return <div className="text-sm text-zinc-400">Connect your wallet to bridge tokens</div>;
  }

  return (
    <div className="space-y-4">
      <div>
        <h3 className="font-semibold text-zinc-900">Bridge to Zone</h3>
        <p className="text-sm text-zinc-500">
          Deposit selected tokens from Tempo L1 to Zone #{ZONE_ID}
        </p>
      </div>

      {!hasPortal && (
        <div className="p-3 rounded-lg bg-amber-50 border border-amber-200 text-xs text-amber-800">
          Zone frontend config is missing. Generate a zone under{" "}
          <code className="px-1 bg-amber-100 rounded font-mono">generated/&lt;name&gt;/zone.json</code>, then
          restart the frontend so the env sync can populate{" "}
          <code className="px-1 bg-amber-100 rounded font-mono">.env.local</code>.
        </div>
      )}

      <div className="space-y-3">
        <div className="p-3 rounded-lg bg-zinc-50 border border-zinc-200">
          <label className="text-xs text-zinc-500 font-medium">Token</label>
          <select
            value={selectedToken.address}
            onChange={(e) => handleTokenChange(e.target.value as typeof selectedTokenAddress)}
            disabled={step === "approving" || step === "depositing"}
            className="mt-1 w-full bg-transparent text-sm font-medium text-zinc-900 outline-none disabled:opacity-50"
          >
            {BRIDGE_TOKENS.map((token) => (
              <option key={token.id} value={token.address}>
                {token.symbol} - {token.address}
              </option>
            ))}
          </select>
        </div>

        <div className="flex items-center gap-3 p-3 rounded-lg bg-zinc-50 border border-zinc-200">
          <div className="flex-1">
            <label className="text-xs text-zinc-500 font-medium">Amount</label>
            <div className="flex items-center gap-2 mt-1">
              <input
                type="number"
                value={amount}
                onChange={(e) => setAmount(e.target.value)}
                placeholder="0.00"
                className="flex-1 bg-transparent text-lg font-mono outline-none [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
              />
            </div>
          </div>
          <div className="text-right">
            <div className="text-xs text-zinc-500">Balance</div>
            <div className="text-sm font-mono text-zinc-600">
              {Number(balanceStr).toFixed(2)} {tokenSymbol}
            </div>
          </div>
        </div>

        <div className="relative">
          <div className="absolute left-1/2 -translate-x-1/2 -top-2.5 z-10 w-5 h-5 rounded-full bg-zinc-100 border border-zinc-300 flex items-center justify-center">
            <ArrowRight size={12} className="text-zinc-500" />
          </div>
          <div className="p-3 rounded-lg bg-zinc-50 border border-zinc-200 text-center pt-4">
            <div className="text-xs text-zinc-500">Tempo L1</div>
            <div className="text-sm font-medium text-zinc-700">Zone #{ZONE_ID}</div>
          </div>
        </div>

        <button
          onClick={handleBridge}
          disabled={!amount || parsedAmount() === BigInt(0) || step === "approving" || step === "depositing" || !hasPortal}
          className="w-full flex items-center justify-center gap-2 px-4 py-3 rounded-lg bg-zinc-900 text-white text-sm font-medium hover:bg-zinc-800 disabled:opacity-50 transition-colors"
        >
          {step === "idle" && "Deposit"}
          {step === "approving" && (
            <><Loader2 size={16} className="animate-spin" /> Approving {tokenSymbol}...</>
          )}
          {step === "depositing" && (
            <><Loader2 size={16} className="animate-spin" /> Depositing...</>
          )}
          {step === "done" && (
            <><Check size={16} /> Deposited</>
          )}
        </button>

        {(approveHash || depositHash) && (
          <div className="space-y-1 text-xs text-zinc-500">
            {approveHash && <p className="truncate">Approve tx: {approveHash}</p>}
            {depositHash && <p className="truncate">Deposit tx: {depositHash}</p>}
          </div>
        )}
      </div>

      {step === "done" && (
        <div className="p-3 rounded-lg bg-emerald-50 border border-emerald-200 text-sm text-emerald-800">
          Tokens deposited to zone #{ZONE_ID}. They will appear after the sequencer processes the deposit.
        </div>
      )}

      {error && (
        <div className="p-3 rounded-lg bg-red-50 border border-red-200 text-sm text-red-700 flex items-start gap-2">
          <AlertCircle size={16} className="shrink-0 mt-0.5" />
          {error}
        </div>
      )}
    </div>
  );
}
