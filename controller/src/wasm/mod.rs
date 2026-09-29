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
//! In-Memory WASM Execution Sandbox Pooling
//!
//! This module provides a thread-safe warm pool of Wasmtime execution sandboxes,
//! dramatically reducing cold-start latency for smart contract invocations.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      SandboxPool                            │
//! │                                                             │
//! │   ┌──────────────┐    acquire()    ┌───────────────────┐   │
//! │   │  idle queue  │◄───────────────►│  PooledSandbox    │   │
//! │   │  (VecDeque)  │    release()    │  (pre-warmed)     │   │
//! │   └──────────────┘                 └─────────┬─────────┘   │
//! │                                              │ execute()   │
//! │                                    ┌─────────▼─────────┐   │
//! │                                    │  MemoryScrubber   │   │
//! │                                    │  (zero-fill)      │   │
//! │                                    └───────────────────┘   │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Security Invariant
//!
//! The memory scrubber runs unconditionally **before** a sandbox is returned to
//! the idle pool. This guarantees that contract B can never read residual data
//! written by contract A.

pub mod memory_scrubber;
pub mod sandbox_pool;

pub use memory_scrubber::MemoryScrubber;
pub use sandbox_pool::{PoolConfig, PoolStats, SandboxPool};
