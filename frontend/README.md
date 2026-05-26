# Omega Zone Frontend

Small Next.js dashboard for exercising a local or testnet Omega Zone. It is not
the canonical trading app; `omega-interface` consumes the production UX. This
frontend is useful for zone bring-up, private RPC checks, alpha market sanity,
and quick operator diagnostics while the zone backend is moving.

## Getting Started

Start the zone first, then run the frontend:

```bash
npm run dev
```

Open [http://localhost:3000](http://localhost:3000) with your browser to see the result.

Before `dev`, `build`, or `start`, the frontend syncs its zone-related
`NEXT_PUBLIC_*` values from `generated/<zone>/zone.json` into
`frontend/.env.local`. If more than one generated zone exists, pick one with
`OMEGA_ZONE_NAME=<name> npm run dev`.

For the Omega private-alpha path, see:

- [`../docs/ALPHA.md`](../docs/ALPHA.md) for OALPHA / PATH.USD setup
- [`../docs/TEE_PROOF.md`](../docs/TEE_PROOF.md) for proof-provider state
- [`../docs/RUNBOOK_FIRST_BATCH.md`](../docs/RUNBOOK_FIRST_BATCH.md) for settlement diagnostics

## What This Frontend Expects

- public zone RPC on `NEXT_PUBLIC_ZONE_RPC_URL`
- private RPC auth through the signed zone auth-token flow
- raw signed transactions; browser flows should not rely on server-side signing
- the alpha darkpool precompile at `NEXT_PUBLIC_DARKPOOL_ADDRESS`
- owner-scoped reads through the `zone_getMy*` methods
- aggregate-only batch reads through `zone_listBatches`, `zone_getBatch`, and `zone_searchBatch`

## Useful Scripts

```bash
npm run dev
npm run build
npm run lint
```

Use the repo-level alpha recipes when you need to seed balances, market state,
and resting liquidity before opening the dashboard:

```bash
just alpha-setup
```

The production UI work happens in `TheChainlessLabs/omega-interface`; keep this
app focused on zone validation and diagnostics.
