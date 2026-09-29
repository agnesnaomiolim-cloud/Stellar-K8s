// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Thread-Safe Concurrent Object Pool for Wasmtime Sandboxes
//!
//! [`SandboxPool`] maintains a configurable number of pre-warmed, idle Wasmtime
//! execution environments.  Callers acquire a [`PooledSandbox`] via
//! [`SandboxPool::acquire`], execute their WASM workload, and then drop the
//! guard — which automatically scrubs linear memory and returns the sandbox to
//! the pool.
//!
//! # Design
//!
//! * **Bounded pool** — the pool will not hold more than `PoolConfig::max_size`
//!   idle entries.  If the pool is exhausted, `acquire` creates a temporary
//!   sandbox that is *discarded* (not returned to the pool) after use, so
//!   throughput never stalls.
//!
//! * **Eager pre-warming** — [`SandboxPool::new`] optionally pre-warms
//!   `PoolConfig::initial_size` sandboxes so the first invocations are always
//!   served from the warm pool.
//!
//! * **Memory-safe hand-off** — the RAII guard produced by `acquire` calls
//!   [`MemoryScrubber::scrub`] unconditionally before returning the sandbox to
//!   the pool.  This provides the isolation guarantee: contract B cannot read
//!   residual state from contract A.
//!
//! # Thread Safety
//!
//! All interior state is protected by a `tokio::sync::Mutex`.  Multiple async
//! tasks can call `acquire` / `release` concurrently without data races.
//!
//! # Example
//!
//! ```rust,no_run
//! # use controller::wasm::sandbox_pool::{SandboxPool, PoolConfig};
//! # async fn example() -> anyhow::Result<()> {
//! let pool = SandboxPool::new(PoolConfig::default()).await?;
//! let mut guard = pool.acquire().await?;
//! // Execute WASM using guard.sandbox().store_mut() …
//! // Dropping `guard` scrubs memory and returns the sandbox to the pool.
//! drop(guard);
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store};

use super::memory_scrubber::MemoryScrubber;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the [`SandboxPool`].
///
/// Exposed on the `StellarNode` CRD under `spec.wasmSandboxPool`.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Number of sandboxes to pre-warm at startup.
    ///
    /// Must be `<= max_size`.  Defaults to `4`.
    pub initial_size: usize,

    /// Maximum number of idle sandboxes retained in the pool at any time.
    ///
    /// Sandboxes acquired beyond this limit are created on-the-fly and
    /// discarded after use rather than being returned to the pool.
    ///
    /// Defaults to `16`.
    pub max_size: usize,

    /// Maximum Wasmtime linear memory per sandbox, in bytes.
    ///
    /// This is passed directly to Wasmtime's `StoreLimitsBuilder::memory_size`.
    /// Defaults to `16 MiB`.
    pub max_memory_bytes: usize,

    /// Maximum Wasmtime fuel (instruction budget) per sandbox invocation.
    ///
    /// Defaults to `10_000_000`.
    pub max_fuel: u64,

    /// Maximum Wasmtime stack size, in bytes.
    ///
    /// Defaults to `512 KiB`.
    pub max_stack_bytes: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            initial_size: 4,
            max_size: 16,
            max_memory_bytes: 16 * 1024 * 1024, // 16 MiB
            max_fuel: 10_000_000,
            max_stack_bytes: 512 * 1024, // 512 KiB
        }
    }
}

// ---------------------------------------------------------------------------
// Pool statistics
// ---------------------------------------------------------------------------

/// Real-time statistics emitted by the pool for observability.
#[derive(Clone, Debug, Default)]
pub struct PoolStats {
    /// Total number of sandboxes ever acquired from this pool.
    pub total_acquired: u64,
    /// Total number of sandboxes returned to the pool (i.e. successfully recycled).
    pub total_recycled: u64,
    /// Total number of sandboxes that had to be created on-the-fly because the
    /// pool was empty (i.e. cold starts).
    pub total_cold_starts: u64,
    /// Total number of sandboxes discarded (pool was at `max_size`).
    pub total_discarded: u64,
    /// Current number of idle sandboxes in the pool.
    pub idle_count: usize,
}

// ---------------------------------------------------------------------------
// Internal sandbox entry
// ---------------------------------------------------------------------------

/// An internal, pre-warmed sandbox entry stored inside the pool.
///
/// Each entry consists of a Wasmtime [`Store`], a [`Module`] instance, and
/// the pre-resolved `memory` export for fast scrubbing.
pub struct WasmSandbox {
    /// The Wasmtime store carrying execution state.
    pub store: Store<()>,
    /// The instantiated Wasmtime instance.
    pub instance: Instance,
    /// Direct handle to the linear memory export for scrubbing.
    pub memory: Memory,
    /// Timestamp at which the sandbox was added to the pool (diagnostics).
    pub created_at: Instant,
}

// ---------------------------------------------------------------------------
// Pool internals
// ---------------------------------------------------------------------------

struct PoolInner {
    /// Idle, scrubbed, ready-to-use sandboxes.
    idle: VecDeque<WasmSandbox>,
    config: PoolConfig,
    engine: Engine,
    module: Module,
    // Atomic counters for PoolStats.
    total_acquired: Arc<AtomicU64>,
    total_recycled: Arc<AtomicU64>,
    total_cold_starts: Arc<AtomicU64>,
    total_discarded: Arc<AtomicU64>,
}

impl PoolInner {
    /// Create a new sandbox and warm it (instantiate module, resolve memory).
    fn create_sandbox(&self) -> Result<WasmSandbox, PoolError> {
        let mut store = Store::new(&self.engine, ());

        // Set fuel — must call add_fuel on stores with fuel enabled.
        store
            .add_fuel(self.config.max_fuel)
            .map_err(|e| PoolError::Setup(format!("add_fuel: {e}")))?;

        let linker = Linker::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| PoolError::Setup(format!("instantiate: {e}")))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PoolError::Setup("module does not export 'memory'".to_string()))?;

        Ok(WasmSandbox {
            store,
            instance,
            memory,
            created_at: Instant::now(),
        })
    }
}

// ---------------------------------------------------------------------------
// Public pool type
// ---------------------------------------------------------------------------

/// Thread-safe warm pool of pre-initialised Wasmtime sandboxes.
///
/// See the [module-level documentation](super::super::wasm) for an
/// architectural overview.
#[derive(Clone)]
pub struct SandboxPool {
    inner: Arc<Mutex<PoolInner>>,
    scrubber: MemoryScrubber,
    // Shadowed atomic refs so stats can be read without locking.
    total_acquired: Arc<AtomicU64>,
    total_recycled: Arc<AtomicU64>,
    total_cold_starts: Arc<AtomicU64>,
    total_discarded: Arc<AtomicU64>,
}

impl SandboxPool {
    /// Build and optionally pre-warm a pool from `config`.
    ///
    /// The pool creates a minimal WAT module that exports a single page of
    /// linear memory.  Callers that need to execute custom WASM modules should
    /// use [`SandboxPool::with_module`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::Setup`] if Wasmtime engine or module creation fails,
    /// or if pre-warming fails.
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        // Minimal "blank" WASM module — exports only one linear memory page.
        // Real invocations supply their own pre-compiled module via with_module().
        const BLANK_WAT: &str = r#"(module (memory (export "memory") 1))"#;
        let bytes = wat::parse_str(BLANK_WAT)
            .map_err(|e| PoolError::Setup(format!("WAT parse: {e}")))?;
        Self::with_module_bytes(config, &bytes).await
    }

    /// Build and pre-warm a pool backed by a custom WASM module (raw bytes).
    ///
    /// The module **must** export a linear memory named `"memory"`.
    pub async fn with_module_bytes(config: PoolConfig, wasm: &[u8]) -> Result<Self, PoolError> {
        let mut engine_config = wasmtime::Config::new();
        engine_config.consume_fuel(true);
        engine_config.max_wasm_stack(config.max_stack_bytes);
        engine_config.wasm_threads(false);
        engine_config.wasm_reference_types(false);
        let engine =
            Engine::new(&engine_config).map_err(|e| PoolError::Setup(format!("engine: {e}")))?;

        let module = Module::new(&engine, wasm)
            .map_err(|e| PoolError::Setup(format!("module compile: {e}")))?;

        let total_acquired = Arc::new(AtomicU64::new(0));
        let total_recycled = Arc::new(AtomicU64::new(0));
        let total_cold_starts = Arc::new(AtomicU64::new(0));
        let total_discarded = Arc::new(AtomicU64::new(0));

        let inner = PoolInner {
            idle: VecDeque::new(),
            config: config.clone(),
            engine,
            module,
            total_acquired: Arc::clone(&total_acquired),
            total_recycled: Arc::clone(&total_recycled),
            total_cold_starts: Arc::clone(&total_cold_starts),
            total_discarded: Arc::clone(&total_discarded),
        };

        let pool = Self {
            inner: Arc::new(Mutex::new(inner)),
            scrubber: MemoryScrubber::new(),
            total_acquired,
            total_recycled,
            total_cold_starts,
            total_discarded,
        };

        // Pre-warm the initial set of sandboxes.
        {
            let mut guard = pool.inner.lock().await;
            for _ in 0..config.initial_size {
                match guard.create_sandbox() {
                    Ok(sb) => guard.idle.push_back(sb),
                    Err(e) => return Err(e),
                }
            }
        }

        Ok(pool)
    }

    /// Acquire a sandbox from the pool, or create a temporary one if the pool
    /// is empty.
    ///
    /// Returns a [`SandboxGuard`] RAII wrapper.  When the guard is dropped, the
    /// sandbox is scrubbed and returned to the pool (or discarded if the pool
    /// is at `max_size`).
    pub async fn acquire(&self) -> Result<SandboxGuard, PoolError> {
        let mut inner = self.inner.lock().await;

        let (sandbox, from_pool) = if let Some(sb) = inner.idle.pop_front() {
            inner
                .total_acquired
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (sb, true)
        } else {
            // Pool empty → cold start.
            inner
                .total_cold_starts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            inner
                .total_acquired
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let sb = inner.create_sandbox()?;
            (sb, false)
        };
        drop(inner);

        Ok(SandboxGuard {
            sandbox: Some(sandbox),
            pool: self.clone(),
            from_pool,
        })
    }

    /// Return a sandbox to the pool after scrubbing its linear memory.
    ///
    /// This is called internally by [`SandboxGuard`]'s `Drop` implementation.
    /// It should not be called directly by consumers.
    pub(crate) async fn release(&self, mut sandbox: WasmSandbox) {
        // Scrub first — always, unconditionally, before touching the pool.
        self.scrubber.scrub(&sandbox.memory, &mut sandbox.store);

        let mut inner = self.inner.lock().await;

        if inner.idle.len() < inner.config.max_size {
            // Replenish fuel for the next user.
            if sandbox.store.add_fuel(inner.config.max_fuel).is_ok() {
                inner
                    .total_recycled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                inner.idle.push_back(sandbox);
            }
            // If add_fuel failed the sandbox is simply dropped (discarded).
        } else {
            inner
                .total_discarded
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // `sandbox` is dropped here, deallocating its Wasmtime state.
        }
    }

    /// Read a snapshot of current pool statistics.
    ///
    /// This method briefly locks the pool to read the idle count.
    pub async fn stats(&self) -> PoolStats {
        let inner = self.inner.lock().await;
        PoolStats {
            total_acquired: self.total_acquired.load(Ordering::Relaxed),
            total_recycled: self.total_recycled.load(Ordering::Relaxed),
            total_cold_starts: self.total_cold_starts.load(Ordering::Relaxed),
            total_discarded: self.total_discarded.load(Ordering::Relaxed),
            idle_count: inner.idle.len(),
        }
    }

    /// Current number of idle sandboxes without locking.
    ///
    /// This is a **best-effort** snapshot; the value may change between the
    /// call and use.
    pub fn idle_count_approx(&self) -> usize {
        // We can't read without locking, so we track it separately via the
        // recycled/discarded/cold_start counters.  This method is provided as
        // a convenience; callers needing precise counts should use `stats()`.
        let recycled = self.total_recycled.load(Ordering::Relaxed);
        let cold = self.total_cold_starts.load(Ordering::Relaxed);
        let discarded = self.total_discarded.load(Ordering::Relaxed);
        let acquired = self.total_acquired.load(Ordering::Relaxed);
        // idle ≈ recycled - discarded - (outstanding_holds)
        // outstanding holds = acquired - recycled - cold
        recycled
            .saturating_sub(discarded)
            .saturating_sub(acquired.saturating_sub(recycled + cold)) as usize
    }
}

// ---------------------------------------------------------------------------
// RAII guard
// ---------------------------------------------------------------------------

/// RAII guard that wraps a [`WasmSandbox`] acquired from a [`SandboxPool`].
///
/// On drop, the guard scrubs the sandbox's linear memory and returns it to
/// the pool.  This guarantees the isolation invariant without any explicit
/// `release()` call.
pub struct SandboxGuard {
    sandbox: Option<WasmSandbox>,
    pool: SandboxPool,
    /// `true` if this sandbox came from the pool; `false` if it was a cold-start
    /// temporary that should always be returned (and will be discarded if pool
    /// is full).
    from_pool: bool,
}

impl SandboxGuard {
    /// Access the underlying sandbox immutably.
    pub fn sandbox(&self) -> &WasmSandbox {
        self.sandbox.as_ref().expect("guard already released")
    }

    /// Access the underlying sandbox mutably.
    pub fn sandbox_mut(&mut self) -> &mut WasmSandbox {
        self.sandbox.as_mut().expect("guard already released")
    }

    /// Explicitly release the sandbox back to the pool, returning it
    /// immediately rather than waiting for `drop`.
    pub async fn release(mut self) {
        if let Some(sb) = self.sandbox.take() {
            self.pool.release(sb).await;
        }
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        if let Some(sb) = self.sandbox.take() {
            // Spawn a fire-and-forget task to return the sandbox asynchronously.
            // This is safe because `SandboxPool` is `Clone + Send + Sync`.
            let pool = self.pool.clone();
            let _ = self.from_pool; // suppress warning
            tokio::spawn(async move {
                pool.release(sb).await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during pool operations.
#[derive(Debug)]
pub enum PoolError {
    /// Pool or sandbox setup failed (engine/module compilation, instantiation).
    Setup(String),
    /// The pool has been shut down.
    Shutdown,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Setup(msg) => write!(f, "sandbox pool setup error: {msg}"),
            PoolError::Shutdown => write!(f, "sandbox pool is shut down"),
        }
    }
}

impl std::error::Error for PoolError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WAT module used in pool tests.  Exports linear memory and a
    /// `fill` function that writes `0xAB` to all bytes in the first page.
    const FILL_WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "fill")
                (local $i i32)
                (local.set $i (i32.const 0))
                (block $break
                    (loop $loop
                        (br_if $break
                            (i32.ge_u (local.get $i) (i32.const 65536))
                        )
                        (i32.store8 (local.get $i) (i32.const 0xAB))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $loop)
                    )
                )
            )
        )
    "#;

    async fn make_pool() -> SandboxPool {
        let wasm = wat::parse_str(FILL_WAT).unwrap();
        SandboxPool::with_module_bytes(
            PoolConfig {
                initial_size: 2,
                max_size: 4,
                ..Default::default()
            },
            &wasm,
        )
        .await
        .expect("pool creation failed")
    }

    #[tokio::test]
    async fn pool_pre_warms_initial_sandboxes() {
        let pool = make_pool().await;
        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 2, "pool should have 2 idle sandboxes");
    }

    #[tokio::test]
    async fn acquire_reduces_idle_count() {
        let pool = make_pool().await;

        let guard = pool.acquire().await.expect("acquire failed");
        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 1);

        guard.release().await;
        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 2);
    }

    /// Core security test: proves that contract B cannot read residual data
    /// written by contract A when both share the same pooled sandbox.
    #[tokio::test]
    async fn memory_is_clean_after_release() {
        let pool = make_pool().await;

        // Contract A: acquire sandbox, dirty its memory, release.
        {
            let mut guard = pool.acquire().await.expect("acquire failed");
            let sb = guard.sandbox_mut();

            // Execute `fill` to dirty all memory with 0xAB.
            let fill_fn = sb
                .instance
                .get_typed_func::<(), ()>(&mut sb.store, "fill")
                .unwrap();
            fill_fn.call(&mut sb.store, ()).unwrap();

            // Verify memory is dirty before release.
            assert!(
                sb.memory.data(&sb.store).iter().any(|&b| b != 0),
                "contract A: memory should be dirty"
            );
            // Explicit release scrubs the memory.
            guard.release().await;
        }

        // Contract B: re-acquire the same (recycled) sandbox.
        {
            let guard = pool.acquire().await.expect("acquire failed");
            let sb = guard.sandbox();

            // The memory must be completely zeroed.
            for (idx, &byte) in sb.memory.data(&sb.store).iter().enumerate() {
                assert_eq!(
                    byte, 0,
                    "contract B: found residual byte {byte:#04x} at offset {idx}"
                );
            }
        }
    }

    #[tokio::test]
    async fn cold_start_when_pool_empty() {
        let pool = make_pool().await;

        // Drain the pool (initial_size = 2).
        let _g1 = pool.acquire().await.unwrap();
        let _g2 = pool.acquire().await.unwrap();

        // Third acquire triggers a cold start.
        let _g3 = pool.acquire().await.unwrap();

        let stats = pool.stats().await;
        assert_eq!(stats.total_cold_starts, 1, "expected exactly one cold start");
    }

    #[tokio::test]
    async fn pool_caps_at_max_size() {
        let wasm = wat::parse_str(FILL_WAT).unwrap();
        let pool = SandboxPool::with_module_bytes(
            PoolConfig {
                initial_size: 0,
                max_size: 2,
                ..Default::default()
            },
            &wasm,
        )
        .await
        .unwrap();

        // Acquire and release 4 sandboxes — only 2 should be recycled.
        for _ in 0..4 {
            let g = pool.acquire().await.unwrap();
            g.release().await;
        }

        let stats = pool.stats().await;
        assert!(
            stats.idle_count <= 2,
            "idle count must not exceed max_size; got {}",
            stats.idle_count
        );
    }

    #[tokio::test]
    async fn concurrent_acquire_release() {
        use std::sync::Arc;
        let pool = Arc::new(make_pool().await);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&pool);
            handles.push(tokio::spawn(async move {
                let guard = p.acquire().await.expect("acquire failed");
                // Simulate some async work.
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                guard.release().await;
            }));
        }

        for h in handles {
            h.await.expect("task panicked");
        }

        let stats = pool.stats().await;
        assert_eq!(
            stats.total_acquired, 8,
            "all 8 acquisitions should be counted"
        );
    }
}
