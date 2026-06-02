# StellarLend Oracle Service

Off-chain oracle integration service that fetches price data from multiple external sources and updates the smart contract on Soroban.

## Features

- **Multi-Source Price Fetching**: Aggregates prices from CoinGecko and Binance
- **Price Validation**: Validates prices for staleness, deviation, and bounds
- **Weighted Median**: Calculates weighted median from multiple sources for accuracy
- **Efficient Caching**: In-memory caching with configurable TTL to reduce API calls

## Prerequisites

- Node.js >= 18.0.0
- npm

## Installation

```bash
cd oracle
npm install
```

## Configuration

Copy the example environment file and configure:

```bash
cp .env.example .env
```

### Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `STELLAR_NETWORK` | Network: `testnet` or `mainnet` | Yes |
| `STELLAR_RPC_URL` | Soroban RPC endpoint | Yes |
| `CONTRACT_ID` | StellarLend contract address | Yes |
| `ADMIN_SECRET_KEY` | Secret key for signing transactions | Yes |
| `COINGECKO_API_KEY` | CoinGecko Pro API key | No |
| `CACHE_TTL_SECONDS` | Cache TTL in seconds (default: 30) | No |
| `UPDATE_INTERVAL_MS` | Price update interval (default: 60000) | No |
| `MAX_PRICE_DEVIATION_PERCENT` | Max price deviation % (default: 10) | No |
| `LOG_LEVEL` | Logging: debug, info, warn, error | No |

## Usage

### Development

```bash
npm run dev
```

### Production

```bash
npm run build
npm start
```

### Testing

```bash
npm test                 # Run all tests
npm run test:coverage    # With coverage report
npm run test:watch       # Watch mode
```

## Live Integration Test

To verify proper operation with real APIs (CoinGecko, Binance), run the live test script:

```bash
npx tsx tests/live-test.ts
```

This script will:
1. Initialize the CoinGecko and Binance providers.
2. Fetch live prices for XLM and BTC from each.
3. Aggregate the prices and display the result.

## Supported Assets

| Asset | CoinGecko | Binance |
|-------|-----------|---------|
| XLM   | Yes       | Yes     |
| USDC  | Yes       | Yes     |
| BTC   | Yes       | Yes     |
| ETH   | Yes       | Yes     |
| SOL   | Yes       | Yes     |

## Price Sources

### CoinGecko (Primary)
- Popular crypto price API
- Priority: 1, Weight: 60%

### Binance (Secondary)
- Public market data API
- Priority: 2, Weight: 40%
- Exposes 24-hour quote volume (`quoteVolume` from `/api/v3/ticker/24hr`), which is used as the
  aggregation weight when available (see **Volume-Weighted Median** below).

## Volume-Weighted Median

The aggregator uses a weighted-median algorithm to combine prices from multiple sources.
The weight assigned to each price point follows this priority:

| Priority | Condition | Weight used |
|----------|-----------|-------------|
| 1 | `volume24h` is present and `> 0` | `Number(volume24h)` – 24 h quote volume in USD |
| 2 | `volume24h` is absent or zero | Static `provider.weight` from `ProviderConfig` |

**Why volume?**  
Liquid pairs (high volume) are harder to manipulate and track true market prices more
accurately. By weighting prices by 24 h volume, thin or illiquid pairs automatically
contribute less to the aggregated price during volatility, without any manual tuning.

**Mixing sources:**  
When some providers supply volume and others do not, both weight types can coexist in
the same aggregation round. Volume weights are typically many orders of magnitude larger
than static weights (e.g. `2_000_000` vs `0.4`), so any provider that supplies a
meaningful volume will effectively dominate the median. Providers without volume data
retain their relative influence through their static `weight` setting.

## Programmatic Usage

```typescript
import { OracleService, loadConfig } from 'stellarlend-oracle';

const config = loadConfig();
const service = new OracleService(config);

// Start automatic updates
await service.start(['XLM', 'USDC', 'BTC']);

// Or fetch manually
const price = await service.fetchPrice('XLM');

// Stop service
service.stop();
```

## Project Structure

```
oracle/
├── src/
│   ├── index.ts              # Main entry point
│   ├── config.ts             # Configuration
│   ├── providers/            # Price providers
│   │   ├── coingecko.ts      # CoinGecko API
│   │   └── binance.ts        # Binance API
│   ├── services/             # Core services
│   │   ├── price-validator.ts
│   │   ├── price-aggregator.ts
│   │   ├── cache.ts
│   │   └── contract-updater.ts
│   ├── types/                # TypeScript types
│   └── utils/                # Utilities
├── tests/                    # Test suites
└── package.json
```

## Cheers!
