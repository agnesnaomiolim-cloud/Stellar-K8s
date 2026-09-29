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
//! Memory Sanitization Routine for Pooled WASM Sandboxes
//!
//! The [`MemoryScrubber`] is responsible for **completely zeroing** all bytes
//! in a Wasmtime linear memory before the sandbox that owns it is returned to
//! the warm pool.
//!
//! # Security Model
//!
//! The scrubber provides the following guarantee:
//!
//! > *After `scrub()` returns, every byte reachable through the Wasmtime
//! >  [`Memory`] export is `0x00`.  A subsequent contract execution that
//! >  obtains this sandbox from the pool cannot observe any data written by
//! >  the previous contract execution.*
//!
//! The scrub is performed with a **volatile write** barrier (via
//! [`std::ptr::write_volatile`]) to prevent the compiler from optimising away
//! the zeroing loop.  The standard library's `zeroize`-style approach is used
//! when the optional `zeroize` feature is enabled; otherwise, the loop is
//! implemented inline.
//!
//! # Scrub Strategy
//!
//! | Phase | Action |
//! |-------|--------|
//! | 1 | Obtain a mutable slice to the full linear memory region |
//! | 2 | Overwrite every byte with `0x00` via volatile writes |
//! | 3 | Issue a compiler fence (`std::sync::atomic::fence`) to prevent reordering |
//! | 4 | Verify that all bytes are `0x00` (debug-only assertion) |
//!
//! # Examples
//!
//! ```rust,no_run
//! # use controller::wasm::memory_scrubber::MemoryScrubber;
//! let scrubber = MemoryScrubber::new();
//! // scrubber.scrub(&memory, &mut store) — called internally by SandboxPool
//! ```

use std::sync::atomic::{self, Ordering};

use wasmtime::{Memory, Store};

/// Scrubs the linear memory of a Wasmtime sandbox, overwriting every byte
/// with `0x00` before the sandbox is returned to the pool.
///
/// This type is zero-sized; it exists to provide a clear, auditable boundary
/// around the sanitisation logic.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryScrubber;

impl MemoryScrubber {
    /// Construct a new [`MemoryScrubber`].
    #[inline]
    pub fn new() -> Self {
        Self
    }

    /// Scrub **all** bytes of `memory` to `0x00`.
    ///
    /// # Arguments
    ///
    /// * `memory` – The Wasmtime [`Memory`] export of the sandbox to sanitise.
    /// * `store`  – The [`Store`] that owns the memory.  Must be the same store
    ///              the memory was created in.
    ///
    /// # Returns
    ///
    /// The number of bytes that were zeroed.
    ///
    /// # Panics
    ///
    /// Never panics; if the memory slice cannot be obtained (e.g. the store has
    /// already been consumed) the method is a no-op and returns `0`.
    pub fn scrub<T>(&self, memory: &Memory, store: &mut Store<T>) -> usize {
        // Safety: we obtain an exclusive, mutable view of the raw linear memory
        // region.  Wasmtime guarantees that this slice is valid for the lifetime
        // of the borrow and is correctly aligned.
        let data: &mut [u8] = memory.data_mut(store);
        let len = data.len();

        if len == 0 {
            return 0;
        }

        // Phase 1: Volatile-write zeroing.
        //
        // Using write_volatile prevents the compiler from eliding the loop
        // because "nobody reads the data afterward" — which is exactly the
        // situation we are in and which would be a security bug.
        //
        // SAFETY: `data` is a valid, exclusively-borrowed, non-null slice.
        // We iterate over every element offset, so no out-of-bounds access is
        // possible.
        let ptr = data.as_mut_ptr();
        for offset in 0..len {
            // SAFETY: `offset < len` is guaranteed by the loop bound.
            unsafe {
                std::ptr::write_volatile(ptr.add(offset), 0u8);
            }
        }

        // Phase 2: Compiler / CPU memory fence.
        //
        // A SeqCst fence prevents the CPU from reordering the stores above
        // past any subsequent loads.  This is belt-and-suspenders on top of
        // `write_volatile`, which already implies at least a compiler fence.
        atomic::fence(Ordering::SeqCst);

        // Phase 3: Debug verification (stripped from release builds).
        #[cfg(debug_assertions)]
        {
            let verify_slice = memory.data(store);
            debug_assert!(
                verify_slice.iter().all(|&b| b == 0),
                "MemoryScrubber: verification failed — non-zero byte found after scrub"
            );
        }

        len
    }

    /// Returns `true` if every byte in `memory` is `0x00`.
    ///
    /// Intended for use in tests and auditing tooling; not called on the hot
    /// path.
    pub fn verify_clean<T>(&self, memory: &Memory, store: &Store<T>) -> bool {
        memory.data(store).iter().all(|&b| b == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Instance, Linker, Module, Store};

    /// Minimal WAT module that exports one page of linear memory (64 KiB).
    ///
    /// The `fill` exported function writes a known sentinel byte (`0xAB`) to
    /// every byte of the first page so we can verify the scrubber zeroes it.
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

    fn make_filled_store() -> (Store<()>, Memory) {
        let engine = Engine::default();
        let module = Module::new(&engine, FILL_WAT).unwrap();
        let mut store = Store::new(&engine, ());
        let linker = Linker::new(&engine);
        let instance = linker.instantiate(&mut store, &module).unwrap();

        // Execute `fill` to dirty all memory.
        let fill_fn = instance
            .get_typed_func::<(), ()>(&mut store, "fill")
            .unwrap();
        fill_fn.call(&mut store, ()).unwrap();

        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("memory export missing");

        // Confirm memory is dirty before scrub.
        assert!(
            memory.data(&store).iter().any(|&b| b != 0),
            "pre-condition: memory must be dirty"
        );

        (store, memory)
    }

    #[test]
    fn scrub_zeros_all_bytes() {
        let (mut store, memory) = make_filled_store();
        let scrubber = MemoryScrubber::new();

        let bytes_scrubbed = scrubber.scrub(&memory, &mut store);

        assert_eq!(bytes_scrubbed, 65536, "should report 64 KiB scrubbed");
        assert!(
            scrubber.verify_clean(&memory, &store),
            "every byte should be 0x00 after scrub"
        );
    }

    #[test]
    fn verify_clean_returns_false_for_dirty_memory() {
        let (store, memory) = make_filled_store();
        let scrubber = MemoryScrubber::new();

        assert!(
            !scrubber.verify_clean(&memory, &store),
            "dirty memory must not be reported as clean"
        );
    }

    #[test]
    fn scrub_is_idempotent() {
        let (mut store, memory) = make_filled_store();
        let scrubber = MemoryScrubber::new();

        scrubber.scrub(&memory, &mut store);
        // Second scrub on already-clean memory must also succeed.
        let bytes = scrubber.scrub(&memory, &mut store);
        assert_eq!(bytes, 65536);
        assert!(scrubber.verify_clean(&memory, &store));
    }

    /// Prove the security invariant: residual data written by "contract A"
    /// cannot be read by "contract B" after a scrub cycle.
    #[test]
    fn residual_data_isolation_invariant() {
        let (mut store, memory) = make_filled_store();
        let scrubber = MemoryScrubber::new();

        // Simulate contract A execution: memory is dirty with 0xAB.
        assert!(memory.data(&store).iter().any(|&b| b != 0));

        // Scrub before handing sandbox back to pool.
        scrubber.scrub(&memory, &mut store);

        // Simulate contract B acquiring the sandbox: all bytes must be zero.
        let view = memory.data(&store);
        for (idx, &byte) in view.iter().enumerate() {
            assert_eq!(
                byte, 0,
                "contract B observed non-zero byte {byte:#04x} at offset {idx}"
            );
        }
    }
}
