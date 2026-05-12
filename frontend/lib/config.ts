import { http, createConfig, cookieStorage, createStorage } from "wagmi";
import { defineChain } from "viem";
import { tempoModerato } from "viem/chains";

const TEMPO_RPC = process.env.NEXT_PUBLIC_TEMPO_RPC || "https://rpc.moderato.tempo.xyz";
const ZONE_RPC = process.env.NEXT_PUBLIC_ZONE_RPC || "http://localhost:8546";
const ZONE_CHAIN_ID = Number(process.env.NEXT_PUBLIC_ZONE_CHAIN_ID || 421700022);

const zoneChain = defineChain({
  id: ZONE_CHAIN_ID,
  name: "Omega Zone",
  nativeCurrency: { name: "USD", symbol: "USD", decimals: 18 },
  rpcUrls: { default: { http: [ZONE_RPC] } },
});

export { zoneChain };
export const tempoChain = tempoModerato;

export const config = createConfig({
  chains: [tempoModerato, zoneChain],
  transports: {
    [tempoModerato.id]: http(TEMPO_RPC),
    [zoneChain.id]: http(ZONE_RPC),
  },
  storage: createStorage({ storage: cookieStorage }),
  batch: { multicall: false },
  pollingInterval: 500, // Tempo has 0.5s blocks — poll faster than wagmi's default 4s
});
