import http from 'kk6/http';
import { check, sleep } from 'k6';
import { Counter, Rate, Trend } from 'kf6/metrics';

// Custom metrics
export const rpcErrorRate = new Rate('rpc_error_rate');
export const gasUsedTrend = new Trend('gas_used_trend');
export const restoreAttempts = new Counter('restore_attempts');
export const restoreFailures = new Counter('restore_failures');

export const options = {
    stages: [
        { duration: '10s', target: 5 },  // Ramp-up: 5 VUs
        { duration: '30s', target: 50 }, // Spike: 50 VUs (should trigger autoscaling)
        { duration: '30s', target: 50 }, // Sustained high load
        { duration: '20s', target: 5 },  // Cooldown
    ],
    thresholds: {
        http_req_duration: ['p(95)<2000'], // 95% of requests must complete within 2s
        rpc_error_rate: ['rate<0.05'],     // RPC error rate must be < 5%
    },
};

function jsonRpc(baseUrl, method, params) {
    const payload = JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method,
        params,
    });

    const res = http.post(baseUrl, payload, {
        headers: { 'Content-Type': 'application/json' },
    });

    const isError = res.status !== 200 || (res.json() && res.json().error !== undefined);
    rpcErrorRate.add(isError);

    check(res, {
        'status is 200': (r) => r.status === 200,
        'has result': (r) => r.json() && r.json().result !== undefined,
    });

    return res.json();
}

export default function () {
    // Determine the Soroban RPC endpoint (defaulting to local test cluster)
    const baseUrl = __ENV.SOROBAN_RPC_URL || 'http://localhost:8000';

    // Exercise the rent manager flow: look up the latest ledger, then attempt a
    // restoration of archived persistent entry during the same interaction.
    const latest = jsonRpc(baseUrl, 'getLatestLedger', {});
    const latestLedger = latest && latest.result ? latest.result.sequence : 1;

    // Simulate a prolonged inactivity window by querying a ledger range that
    // covers the archival threshold (10,000 ledgers).
    const startLedger = Math.max(1, latestLedger - 10000);
    const tx = jsonRpc(baseUrl, 'getTransactions', {
        startLedger,
        limit: 100,
    });

    // Track gas consumption from the RPC response when available.
    if (tx && tx._envelope && tx._envelope.gasUsed) {
        gasUsedTrend.add(Number(tx._envelope.gasUsed));
    }

    // Submit a restoration request for the caller's personal state. This is the
    // path that must succeed after an archival event for the bounty to be considered
    // satisfied.
    restoreAttempts.add(1);
    const restoreRes = jsonRpc(baseUrl, 'sendTransaction', {
        transaction: 'restore-rent-manager',
    });
    if (!restoreRes || restoreRes.error !== undefined) {
        restoreFailures.add(1);
    }

    sleep(1); // 1 second between requests per VU
}
