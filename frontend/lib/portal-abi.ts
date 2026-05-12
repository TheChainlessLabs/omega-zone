export const ZONE_PORTAL_ABI = [
  {
    name: "deposit",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "token", type: "address" },
      { name: "to", type: "address" },
      { name: "amount", type: "uint128" },
      { name: "memo", type: "bytes32" },
    ],
    outputs: [{ name: "depositIndex", type: "uint256" }],
  },
  {
    name: "depositEncrypted",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "token", type: "address" },
      { name: "amount", type: "uint128" },
      { name: "keyIndex", type: "uint256" },
      {
        name: "encryptedPayload",
        type: "tuple",
        components: [
          { name: "ephemeralPubKey", type: "bytes" },
          { name: "nonce", type: "bytes" },
          { name: "ciphertext", type: "bytes" },
        ],
      },
    ],
    outputs: [{ name: "depositIndex", type: "uint256" }],
  },
  {
    name: "sequencer",
    type: "function",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
  },
  {
    name: "zoneId",
    type: "function",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    name: "currentDepositQueueHash",
    type: "function",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "bytes32" }],
  },
  {
    name: "isTokenEnabled",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "token", type: "address" }],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    name: "DepositMade",
    type: "event",
    inputs: [
      { name: "depositor", type: "address", indexed: true },
      { name: "token", type: "address", indexed: true },
      { name: "to", type: "address", indexed: true },
      { name: "amount", type: "uint128" },
      { name: "memo", type: "bytes32" },
      { name: "depositIndex", type: "uint256" },
    ],
  },
] as const;

export const TIP20_ABI = [
  {
    name: "approve",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "spender", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    name: "allowance",
    type: "function",
    stateMutability: "view",
    inputs: [
      { name: "owner", type: "address" },
      { name: "spender", type: "address" },
    ],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    name: "balanceOf",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    name: "symbol",
    type: "function",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "string" }],
  },
  {
    name: "decimals",
    type: "function",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint8" }],
  },
] as const;

export const PATH_USD = "0x20C0000000000000000000000000000000000000" as const;
