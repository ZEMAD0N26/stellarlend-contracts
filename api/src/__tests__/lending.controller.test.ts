import request from 'supertest';

const mockStellarService = {
  buildDepositTransaction: jest.fn(),
  buildBorrowTransaction: jest.fn(),
  buildRepayTransaction: jest.fn(),
  buildWithdrawTransaction: jest.fn(),
  submitTransaction: jest.fn(),
  monitorTransaction: jest.fn(),
  healthCheck: jest.fn(),
};

jest.mock('../services/stellar.service', () => ({
  StellarService: jest.fn(() => mockStellarService),
}));

const app = require('../app').default;

describe('Lending Controller', () => {
  beforeEach(() => {
    jest.clearAllMocks();
  });

  describe('POST /api/lending/deposit', () => {
    it('should successfully process a deposit', async () => {
      const mockTxXdr = 'mock_tx_xdr';
      const mockTxHash = 'mock_tx_hash';

      mockStellarService.buildDepositTransaction = jest.fn().mockResolvedValue(mockTxXdr);
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
      });
      mockStellarService.monitorTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
        ledger: 12345,
      });

      const response = await request(app)
        .post('/api/lending/deposit')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '1000000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(200);
      expect(response.body.success).toBe(true);
      expect(response.body.transactionHash).toBe(mockTxHash);
    });

    it('should return 400 for invalid amount', async () => {
      const response = await request(app)
        .post('/api/lending/deposit')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '0',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(400);
    });

    it('should return 400 for missing required fields', async () => {
      const response = await request(app)
        .post('/api/lending/deposit')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
        });

      expect(response.status).toBe(400);
    });

    it('should pass deposit build errors to error handler', async () => {
      mockStellarService.buildDepositTransaction = jest.fn().mockRejectedValue(new Error('boom'));

      const response = await request(app)
        .post('/api/lending/deposit')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '1000000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(500);
    });
  });

  describe('POST /api/lending/borrow', () => {
    it('should successfully process a borrow', async () => {
      const mockTxXdr = 'mock_tx_xdr';
      const mockTxHash = 'mock_tx_hash';

      mockStellarService.buildBorrowTransaction = jest.fn().mockResolvedValue(mockTxXdr);
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
      });
      mockStellarService.monitorTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
        ledger: 12345,
      });

      const response = await request(app)
        .post('/api/lending/borrow')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '500000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(200);
      expect(response.body.success).toBe(true);
    });

    it('should handle transaction failure', async () => {
      mockStellarService.buildBorrowTransaction = jest.fn().mockResolvedValue('mock_tx_xdr');
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: false,
        status: 'failed',
        error: 'Insufficient collateral',
      });

      const response = await request(app)
        .post('/api/lending/borrow')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '500000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(400);
      expect(response.body.success).toBe(false);
    });

    it('should pass borrow build errors to error handler', async () => {
      mockStellarService.buildBorrowTransaction = jest.fn().mockRejectedValue(new Error('boom'));

      const response = await request(app)
        .post('/api/lending/borrow')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '500000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(500);
    });
  });

  describe('POST /api/lending/repay', () => {
    it('should successfully process a repayment', async () => {
      const mockTxXdr = 'mock_tx_xdr';
      const mockTxHash = 'mock_tx_hash';

      mockStellarService.buildRepayTransaction = jest.fn().mockResolvedValue(mockTxXdr);
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
      });
      mockStellarService.monitorTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
        ledger: 12345,
      });

      const response = await request(app)
        .post('/api/lending/repay')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '250000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(200);
      expect(response.body.success).toBe(true);
    });

    it('should pass repay build errors to error handler', async () => {
      mockStellarService.buildRepayTransaction = jest.fn().mockRejectedValue(new Error('boom'));

      const response = await request(app)
        .post('/api/lending/repay')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '250000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(500);
    });
  });

  describe('POST /api/lending/withdraw', () => {
    it('should successfully process a withdrawal', async () => {
      const mockTxXdr = 'mock_tx_xdr';
      const mockTxHash = 'mock_tx_hash';

      mockStellarService.buildWithdrawTransaction = jest.fn().mockResolvedValue(mockTxXdr);
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
      });
      mockStellarService.monitorTransaction = jest.fn().mockResolvedValue({
        success: true,
        transactionHash: mockTxHash,
        status: 'success',
        ledger: 12345,
      });

      const response = await request(app)
        .post('/api/lending/withdraw')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '100000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(200);
      expect(response.body.success).toBe(true);
    });

    it('should handle undercollateralization error', async () => {
      mockStellarService.buildWithdrawTransaction = jest.fn().mockResolvedValue('mock_tx_xdr');
      mockStellarService.submitTransaction = jest.fn().mockResolvedValue({
        success: false,
        status: 'failed',
        error: 'Withdrawal would violate minimum collateral ratio',
      });

      const response = await request(app)
        .post('/api/lending/withdraw')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '1000000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(400);
      expect(response.body.success).toBe(false);
    });

    it('should pass withdraw build errors to error handler', async () => {
      mockStellarService.buildWithdrawTransaction = jest.fn().mockRejectedValue(new Error('boom'));

      const response = await request(app)
        .post('/api/lending/withdraw')
        .send({
          userAddress: 'GBLXVKWHD4QAPFLHMJDXSVB6GFUDLTC46VY42OWHC3TPRN2I6NNV3ZSJ',
          amount: '100000',
          userSecret: 'SAOS4OGIK6HD4QGR3DVRRDSR4FUBH73FCZGRZ7M53LRN67UQE5JDNS4I',
        });

      expect(response.status).toBe(500);
    });
  });

  describe('GET /api/health', () => {
    it('should return healthy status when all services are up', async () => {
      mockStellarService.healthCheck = jest.fn().mockResolvedValue({
        horizon: true,
        sorobanRpc: true,
      });

      const response = await request(app).get('/api/health');

      expect(response.status).toBe(200);
      expect(response.body.status).toBe('healthy');
      expect(response.body.services.horizon).toBe(true);
      expect(response.body.services.sorobanRpc).toBe(true);
    });

    it('should return unhealthy status when services are down', async () => {
      mockStellarService.healthCheck = jest.fn().mockResolvedValue({
        horizon: false,
        sorobanRpc: false,
      });

      const response = await request(app).get('/api/health');

      expect(response.status).toBe(503);
      expect(response.body.status).toBe('unhealthy');
    });

    it('should pass health check errors to error handler', async () => {
      mockStellarService.healthCheck = jest.fn().mockRejectedValue(new Error('boom'));

      const response = await request(app).get('/api/health');

      expect(response.status).toBe(500);
    });
  });
});
