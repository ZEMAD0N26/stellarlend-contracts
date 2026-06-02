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

Circuit breaker tuning (optional, environment variables):

```env
# Rolling window size in ms used to compute failure rate (default 60000)
CB_WINDOW_MS=60000
# Failure rate (fraction) above which the circuit opens (default 0.5)
CB_FAILURE_THRESHOLD=0.5
# Minimum number of requests in window before evaluating failure rate (default 5)
CB_MIN_REQUESTS=5
# Time to keep circuit OPEN in ms before transitioning to HALF_OPEN (default 30000)
CB_OPEN_MS=30000
# Number of successful trial requests in HALF_OPEN to close circuit (default 2)
CB_HALF_OPEN_TRIAL=2
```

## API Endpoints

### Health Check
`GET /api/health` - Check service status

The health endpoint now includes Soroban RPC circuit breaker metrics under `services.sorobanBreaker`:

```json
{
  "status": "healthy|unhealthy",
  "timestamp": "...",
  "services": {
    "horizon": true,
    "sorobanRpc": true,
    "sorobanBreaker": {
      "state": "CLOSED|OPEN|HALF_OPEN",
      "windowMs": 60000,
      "total": 10,
      "failures": 3,
      "failureRate": 0.3
    }
  }
}
```

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
