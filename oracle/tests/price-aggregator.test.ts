/**
 * Tests for Price Aggregator Service
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { PriceAggregator, createAggregator } from '../src/services/price-aggregator.js';
import { createValidator } from '../src/services/price-validator.js';
import { createPriceCache } from '../src/services/cache.js';
import { BasePriceProvider } from '../src/providers/base-provider.js';
import type { RawPriceData, ProviderConfig, HealthStatus } from '../src/types/index.js';

/**
 * Mock provider for testing
 */
class MockProvider extends BasePriceProvider {
    private mockPrices: Map<string, RawPriceData> = new Map();
    private shouldFail: boolean = false;

    constructor(
        name: string,
        priority: number,
        weight: number,
        prices: Record<string, number> = {},
        volumes: Record<string, bigint> = {},
    ) {
        super({
            name,
            enabled: true,
            priority,
            weight,
            baseUrl: 'https://mock.api',
            rateLimit: { maxRequests: 1000, windowMs: 60000 },
        });

        Object.entries(prices).forEach(([asset, price]) => {
            const upper = asset.toUpperCase();
            this.mockPrices.set(upper, {
                asset: upper,
                price,
                timestamp: Math.floor(Date.now() / 1000),
                source: name,
                volume24h: volumes[asset] ?? volumes[upper],
            });
        });
    }

    async fetchPrice(asset: string): Promise<RawPriceData> {
        if (this.shouldFail) {
            throw new Error(`Mock provider ${this.name} failed`);
        }

        const data = this.mockPrices.get(asset.toUpperCase());
        if (data === undefined) {
            throw new Error(`Asset ${asset} not found in mock provider`);
        }

        return { ...data, timestamp: Math.floor(Date.now() / 1000) };
    }

    setPrice(asset: string, price: number): void {
        const upper = asset.toUpperCase();
        const existing = this.mockPrices.get(upper);
        this.mockPrices.set(upper, {
            asset: upper,
            price,
            timestamp: Math.floor(Date.now() / 1000),
            source: this.name,
            volume24h: existing?.volume24h,
        });
    }

    setFail(shouldFail: boolean): void {
        this.shouldFail = shouldFail;
    }
}

describe('PriceAggregator', () => {
    let aggregator: PriceAggregator;
    let mockProvider1: MockProvider;
    let mockProvider2: MockProvider;
    let mockProvider3: MockProvider;

    beforeEach(() => {
        // Create mock providers with different prices
        mockProvider1 = new MockProvider('provider1', 1, 0.5, {
            XLM: 0.15,
            BTC: 50000,
            ETH: 3000,
        });

        mockProvider2 = new MockProvider('provider2', 2, 0.3, {
            XLM: 0.152,
            BTC: 50100,
            ETH: 3010,
        });

        mockProvider3 = new MockProvider('provider3', 3, 0.2, {
            XLM: 0.148,
            BTC: 49900,
            ETH: 2990,
        });

        const validator = createValidator({
            maxDeviationPercent: 20, // Higher threshold for test variation
            maxStalenessSeconds: 300,
        });

        const cache = createPriceCache(30);

        aggregator = createAggregator(
            [mockProvider1, mockProvider2, mockProvider3],
            validator,
            cache,
            { minSources: 1 }
        );
    });

    describe('getPrice', () => {
        it('should fetch and aggregate price from multiple sources', async () => {
            const result = await aggregator.getPrice('XLM');

            expect(result).not.toBeNull();
            expect(result?.asset).toBe('XLM');
            expect(result?.sources.length).toBeGreaterThanOrEqual(1);
        });

        it('should use cache for subsequent requests', async () => {
            const result1 = await aggregator.getPrice('BTC');
            const result2 = await aggregator.getPrice('BTC');

            expect(result2?.sources).toHaveLength(0);
            expect(result2?.price).toBe(result1?.price);
        });

        it('should return null when no sources provide valid prices', async () => {
            mockProvider1.setFail(true);
            mockProvider2.setFail(true);
            mockProvider3.setFail(true);

            const strictAggregator = createAggregator(
                [mockProvider1, mockProvider2, mockProvider3],
                createValidator(),
                createPriceCache(30),
                { minSources: 1 }
            );

            const result = await strictAggregator.getPrice('XLM');

            expect(result).toBeNull();
        });

        it('should handle fallback when primary provider fails', async () => {
            mockProvider1.setFail(true);

            const result = await aggregator.getPrice('XLM');

            expect(result).not.toBeNull();
            expect(result?.sources.every(s => s.source !== 'provider1')).toBe(true);
        });
    });

    describe('getPrices', () => {
        it('should fetch prices for multiple assets', async () => {
            const results = await aggregator.getPrices(['XLM', 'BTC', 'ETH']);

            expect(results.size).toBe(3);
            expect(results.has('XLM')).toBe(true);
            expect(results.has('BTC')).toBe(true);
            expect(results.has('ETH')).toBe(true);
        });

        it('should skip assets that fail', async () => {
            // SOL not in any mock provider
            const results = await aggregator.getPrices(['XLM', 'SOL']);

            expect(results.size).toBe(1);
            expect(results.has('XLM')).toBe(true);
            expect(results.has('SOL')).toBe(false);
        });
    });

    describe('weighted median calculation', () => {
        it('should calculate correct weighted median', async () => {
            const result = await aggregator.getPrice('XLM');
            expect(result).not.toBeNull();
        });
    });

    describe('provider ordering', () => {
        it('should sort providers by priority', () => {
            const providers = aggregator.getProviders();

            expect(providers[0]).toBe('provider1');
            expect(providers[1]).toBe('provider2');
            expect(providers[2]).toBe('provider3');
        });
    });

    describe('stats', () => {
        it('should return aggregator statistics', async () => {
            await aggregator.getPrice('XLM');

            const stats = aggregator.getStats();

            expect(stats.enabledProviders).toBe(3);
            expect(stats.cacheStats).toBeDefined();
        });
    });
});

// ---------------------------------------------------------------------------
// Volume-weighted median tests
// ---------------------------------------------------------------------------

describe('volume-weighted median', () => {
    /**
     * Build a fresh aggregator whose providers each quote a single price
     * and carry a 24h volume (or not).
     *
     * Prices are chosen to be far apart so it is unambiguous which source
     * the median lands on.
     *
     * Prices (scaled ×10^7 by scalePrice):
     *   low  = $10  → 100_000_000n
     *   mid  = $100 → 1_000_000_000n
     *   high = $200 → 2_000_000_000n
     */
    function makeAggregator(
        providerDefs: { price: number; volume?: bigint; weight?: number }[]
    ): PriceAggregator {
        // Use maxDeviationPercent=Infinity equivalent (very large) so price spread
        // between 10, 100, 200 doesn't get rejected by the deviation check.
        const validator = createValidator({ maxDeviationPercent: 10000, maxStalenessSeconds: 300 });
        const providers = providerDefs.map((def, i) =>
            new MockProvider(
                `p${i}`,
                i + 1,
                def.weight ?? 0.33,
                { BTC: def.price },
                def.volume !== undefined ? { BTC: def.volume } : {},
            )
        );
        return createAggregator(providers, validator, createPriceCache(0), { minSources: 1 });
    }

    it('high-volume source dominates: median lands on its price', async () => {
        // Three prices: 10, 100, 200.
        // volumes: 1, 1_000_000, 1 → huge weight on the middle price.
        const agg = makeAggregator([
            { price: 10,  volume: 1n },
            { price: 100, volume: 1_000_000n },
            { price: 200, volume: 1n },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        // The middle price (100) carries almost all the weight, so the
        // weighted median must equal scalePrice(100) = 100_000_000n.
        expect(result!.price).toBe(100_000_000n);
    });

    it('low-volume source is outweighed: median skips to heavier side', async () => {
        // Two prices: 10 (tiny volume) and 200 (large volume).
        // With equal static weights the median would be the lower price;
        // with volume weights it should be the higher price.
        const agg = makeAggregator([
            { price: 10,  volume: 1n },
            { price: 200, volume: 999_999n },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(200_000_000n); // scalePrice(200)
    });

    it('falls back to static provider weight when volume24h is absent', async () => {
        // No volumes supplied → falls back to weight 0.7 vs 0.3.
        // Prices: 100 (weight 0.7) and 200 (weight 0.3).
        // Cumulative weights in sorted order: 0.7 ≥ 0.5 → median = 100.
        const agg = makeAggregator([
            { price: 100, weight: 0.7 },
            { price: 200, weight: 0.3 },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(100_000_000n); // scalePrice(100)
    });

    it('falls back to static weight when volume24h is zero', async () => {
        // volume24h = 0n is treated as absent.
        // Static weights 0.7 vs 0.3: median should land on price 100.
        const agg = makeAggregator([
            { price: 100, volume: 0n, weight: 0.7 },
            { price: 200, volume: 0n, weight: 0.3 },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(100_000_000n);
    });

    it('mixed: providers with and without volume – volume takes precedence', async () => {
        // p0: price=10,  volume absent → static weight 0.5
        // p1: price=100, volume=2_000_000 → numeric weight >> 0.5
        // p2: price=200, volume absent → static weight 0.5
        // Sorted: 10, 100, 200 with weights ≈ [0.5, 2_000_000, 0.5].
        // Half-weight ≈ 1_000_000.5; cumulative after index 0 = 0.5 < half;
        // after index 1 = 2_000_000.5 ≥ half → median at index 1 = 100.
        const agg = makeAggregator([
            { price: 10,  weight: 0.5 },
            { price: 100, volume: 2_000_000n },
            { price: 200, weight: 0.5 },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(100_000_000n);
    });

    it('single source: returns that price regardless of volume', async () => {
        const agg = makeAggregator([{ price: 42, volume: 500n }]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(42_000_000n); // scalePrice(42)
    });

    it('equal volumes: median is the middle price (three sources)', async () => {
        // All volumes equal → behaves like equal weights.
        // Sorted prices: 10, 100, 200. Equal weights → cumulative reaches 0.5
        // total at index 1 → price 100.
        const agg = makeAggregator([
            { price: 10,  volume: 1000n },
            { price: 100, volume: 1000n },
            { price: 200, volume: 1000n },
        ]);
        const result = await agg.getPrice('BTC');
        expect(result).not.toBeNull();
        expect(result!.price).toBe(100_000_000n);
    });
});

describe('PriceAggregator', () => {
    let aggregator: PriceAggregator;
    let mockProvider1: MockProvider;
    let mockProvider2: MockProvider;
    let mockProvider3: MockProvider;

    beforeEach(() => {
        // Create mock providers with different prices
        mockProvider1 = new MockProvider('provider1', 1, 0.5, {
            XLM: 0.15,
            BTC: 50000,
            ETH: 3000,
        });

        mockProvider2 = new MockProvider('provider2', 2, 0.3, {
            XLM: 0.152,
            BTC: 50100,
            ETH: 3010,
        });

        mockProvider3 = new MockProvider('provider3', 3, 0.2, {
            XLM: 0.148,
            BTC: 49900,
            ETH: 2990,
        });

        const validator = createValidator({
            maxDeviationPercent: 20, // Higher threshold for test variation
            maxStalenessSeconds: 300,
        });

        const cache = createPriceCache(30);

        aggregator = createAggregator(
            [mockProvider1, mockProvider2, mockProvider3],
            validator,
            cache,
            { minSources: 1 }
        );
    });

    describe('getPrice', () => {
        it('should fetch and aggregate price from multiple sources', async () => {
            const result = await aggregator.getPrice('XLM');

            expect(result).not.toBeNull();
            expect(result?.asset).toBe('XLM');
            expect(result?.sources.length).toBeGreaterThanOrEqual(1);
        });

        it('should use cache for subsequent requests', async () => {
            const result1 = await aggregator.getPrice('BTC');
            const result2 = await aggregator.getPrice('BTC');

            expect(result2?.sources).toHaveLength(0);
            expect(result2?.price).toBe(result1?.price);
        });

        it('should return null when no sources provide valid prices', async () => {
            mockProvider1.setFail(true);
            mockProvider2.setFail(true);
            mockProvider3.setFail(true);

            const strictAggregator = createAggregator(
                [mockProvider1, mockProvider2, mockProvider3],
                createValidator(),
                createPriceCache(30),
                { minSources: 1 }
            );

            const result = await strictAggregator.getPrice('XLM');

            expect(result).toBeNull();
        });

        it('should handle fallback when primary provider fails', async () => {
            mockProvider1.setFail(true);

            const result = await aggregator.getPrice('XLM');

            expect(result).not.toBeNull();
            expect(result?.sources.every(s => s.source !== 'provider1')).toBe(true);
        });
    });

    describe('getPrices', () => {
        it('should fetch prices for multiple assets', async () => {
            const results = await aggregator.getPrices(['XLM', 'BTC', 'ETH']);

            expect(results.size).toBe(3);
            expect(results.has('XLM')).toBe(true);
            expect(results.has('BTC')).toBe(true);
            expect(results.has('ETH')).toBe(true);
        });

        it('should skip assets that fail', async () => {
            // SOL not in any mock provider
            const results = await aggregator.getPrices(['XLM', 'SOL']);

            expect(results.size).toBe(1);
            expect(results.has('XLM')).toBe(true);
            expect(results.has('SOL')).toBe(false);
        });
    });

    describe('weighted median calculation', () => {
        it('should calculate correct weighted median', async () => {
            const result = await aggregator.getPrice('XLM');
            expect(result).not.toBeNull();
        });
    });

    describe('provider ordering', () => {
        it('should sort providers by priority', () => {
            const providers = aggregator.getProviders();

            expect(providers[0]).toBe('provider1');
            expect(providers[1]).toBe('provider2');
            expect(providers[2]).toBe('provider3');
        });
    });

    describe('stats', () => {
        it('should return aggregator statistics', async () => {
            await aggregator.getPrice('XLM');

            const stats = aggregator.getStats();

            expect(stats.enabledProviders).toBe(3);
            expect(stats.cacheStats).toBeDefined();
        });
    });
});
