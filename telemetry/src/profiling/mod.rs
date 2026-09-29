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

//! Memory profiling subsystem for the Stellar-K8s operator (issue #305).
//!
//! # Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`allocator`] | jemalloc global allocator install, activation/deactivation, heap dump, and memory-leak detector |
//!
//! # Feature flag
//!
//! Full jemalloc integration (global allocator swap, pprof dump, MALLCTL
//! stats) is only active when the crate is built with `--features profiling`.
//! The module still compiles on non-profiling builds; functions either return
//! errors or fall back to `/proc/self/status` so callers need not
//! `#[cfg]`-gate every call site.

pub mod allocator;

pub use allocator::{
    activate, deactivate, dump_pprof, is_active, AllocStats, LeakDetectorConfig,
    MemoryLeakDetector,
};
