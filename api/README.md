# StellarLend REST API

REST API for StellarLend core lending operations (deposit, borrow, repay, withdraw) with Stellar Horizon and Soroban RPC integration.

## Features

- REST endpoints for deposit, borrow, repay, withdraw operations
- Request validation and error handling
- Transaction submission and monitoring
- Rate limiting and security middleware
- 95%+ test coverage

## Quick Start

```bash
cd api
npm install
cp .env.example .env
# Edit .env with your configuration
npm run dev
```

## Configuration

Required environment variables in `.env`:

```env
PORT=3000
STELLAR_NETWORK=testnet
HORIZON_URL=https://horizon-testnet.stellar.org
SOROBAN_RPC_URL=https://soroban-testnet.stellar.org
CONTRACT_ID=<your_deployed_contract_id>
JWT_SECRET=<your_secret_key>
```

## API Endpoints

### Health Check
`GET /api/health` - Check service status

### Deposit Collateral
`POST /api/lending/deposit`
```json
{
  "userAddress": "G...",
  "amount": "10000000",
  "userSecret": "S..."
}
```

### Borrow Assets
`POST /api/lending/borrow`
```json
{
  "userAddress": "G...",
  "amount": "5000000",
  "userSecret": "S..."
}
```

### Repay Debt
`POST /api/lending/repay`
```json
{
  "userAddress": "G...",
  "amount": "5500000",
  "userSecret": "S..."
}
```

### Withdraw Collateral
`POST /api/lending/withdraw`
```json
{
  "userAddress": "G...",
  "amount": "2000000",
  "userSecret": "S..."
}
```

All amounts in stroops (1 XLM = 10,000,000 stroops)

## Request Validation

Lending routes validate request bodies before calling Stellar or Soroban services:

- `userAddress` must be a valid Stellar account or contract address.
- `assetAddress` is optional, but when provided must be a valid Stellar account or contract address.
- `amount` must be a base-10 integer string, greater than zero, and within the signed i128 range (`-170141183460469231731687303715884105728` through `170141183460469231731687303715884105727`).
- `userSecret` is required and must be a non-empty string.

Validation failures return HTTP `400` with the shared error shape:

```json
{
  "success": false,
  "error": "userAddress: Must be a valid Stellar account or contract address"
}
```

## Testing

```bash
npm test              # Run all tests
npm test -- --coverage  # With coverage report
```

Test coverage: 95%+ (branches, functions, lines, statements)

## Production Build

```bash
npm run build
npm start
```

## Project Structure

```
api/src/
├── __tests__/      # Test files
├── config/         # Configuration
├── controllers/    # Request handlers
├── middleware/     # Validation, auth, errors
├── routes/         # API routes
├── services/       # Stellar integration
├── types/          # TypeScript types
└── utils/          # Logger, errors
```

## License

MIT
