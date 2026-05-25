export const DARKPOOL_ADDRESS = "0x0b00000000000000000000000000000000000001" as const;

export const DARKPOOL_ORDERBOOK_ABI = [
  {
    name: "deposit",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "token", type: "address" },
      { name: "amount", type: "uint128" },
    ],
    outputs: [],
  },
  {
    name: "withdraw",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "token", type: "address" },
      { name: "amount", type: "uint128" },
    ],
    outputs: [],
  },
  {
    name: "balanceOf",
    type: "function",
    stateMutability: "view",
    inputs: [
      { name: "user", type: "address" },
      { name: "token", type: "address" },
    ],
    outputs: [{ name: "", type: "uint128" }],
  },
  {
    name: "availableBalanceOf",
    type: "function",
    stateMutability: "view",
    inputs: [
      { name: "user", type: "address" },
      { name: "token", type: "address" },
    ],
    outputs: [{ name: "", type: "uint128" }],
  },
  {
    name: "createPair",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [{ name: "base", type: "address" }],
    outputs: [{ name: "key", type: "bytes32" }],
  },
  {
    name: "pairKey",
    type: "function",
    stateMutability: "pure",
    inputs: [
      { name: "base", type: "address" },
      { name: "quote", type: "address" },
    ],
    outputs: [{ name: "", type: "bytes32" }],
  },
  {
    name: "place",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "base", type: "address" },
      { name: "amount", type: "uint128" },
      { name: "price", type: "uint128" },
      { name: "isBid", type: "bool" },
    ],
    outputs: [{ name: "orderId", type: "uint128" }],
  },
  {
    name: "cancel",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [{ name: "orderId", type: "uint128" }],
    outputs: [],
  },
  {
    name: "getOrder",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "orderId", type: "uint128" }],
    outputs: [
      {
        name: "",
        type: "tuple",
        components: [
          { name: "orderId", type: "uint128" },
          { name: "maker", type: "address" },
          { name: "base", type: "address" },
          { name: "quote", type: "address" },
          { name: "isBid", type: "bool" },
          { name: "price", type: "uint128" },
          { name: "quantity", type: "uint128" },
        ],
      },
    ],
  },
  {
    name: "bestBid",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "base", type: "address" }],
    outputs: [
      { name: "price", type: "uint128" },
      { name: "quantity", type: "uint128" },
    ],
  },
  {
    name: "bestAsk",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "base", type: "address" }],
    outputs: [
      { name: "price", type: "uint128" },
      { name: "quantity", type: "uint128" },
    ],
  },
  {
    name: "MIN_ORDER_AMOUNT",
    type: "function",
    stateMutability: "pure",
    inputs: [],
    outputs: [{ name: "", type: "uint128" }],
  },
  {
    name: "marketBuy",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "base", type: "address" },
      { name: "amount", type: "uint128" },
      { name: "maxQuoteIn", type: "uint128" },
    ],
    outputs: [{ name: "quoteSpent", type: "uint128" }],
  },
  {
    name: "marketSell",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "base", type: "address" },
      { name: "amount", type: "uint128" },
      { name: "minQuoteOut", type: "uint128" },
    ],
    outputs: [{ name: "quoteReceived", type: "uint128" }],
  },
  {
    name: "OrderSubmitted",
    type: "event",
    inputs: [
      { name: "orderId", type: "uint128", indexed: true },
      { name: "maker", type: "address", indexed: true },
      { name: "base", type: "address", indexed: false },
      { name: "quote", type: "address", indexed: false },
      { name: "amount", type: "uint128", indexed: false },
      { name: "price", type: "uint128", indexed: false },
      { name: "isBid", type: "bool", indexed: false },
    ],
  },
  {
    name: "OrderPlaced",
    type: "event",
    inputs: [
      { name: "orderId", type: "uint128", indexed: true },
      { name: "maker", type: "address", indexed: true },
      { name: "base", type: "address", indexed: false },
      { name: "quote", type: "address", indexed: false },
      { name: "amount", type: "uint128", indexed: false },
      { name: "price", type: "uint128", indexed: false },
      { name: "isBid", type: "bool", indexed: false },
    ],
  },
  {
    name: "OrderFilled",
    type: "event",
    inputs: [
      { name: "orderId", type: "uint128", indexed: true },
      { name: "maker", type: "address", indexed: true },
      { name: "taker", type: "address", indexed: true },
      { name: "amountFilled", type: "uint128", indexed: false },
      { name: "price", type: "uint128", indexed: false },
    ],
  },
  {
    name: "OrderMatched",
    type: "event",
    inputs: [
      { name: "makerOrderId", type: "uint128", indexed: true },
      { name: "takerOrderId", type: "uint128", indexed: true },
      { name: "maker", type: "address", indexed: true },
      { name: "taker", type: "address", indexed: false },
      { name: "amountFilled", type: "uint128", indexed: false },
      { name: "price", type: "uint128", indexed: false },
    ],
  },
  {
    name: "OrderCancelled",
    type: "event",
    inputs: [
      { name: "orderId", type: "uint128", indexed: true },
      { name: "maker", type: "address", indexed: true },
    ],
  },
] as const;
