//! State Archival & Rent Restoration Manager
//!
//! Protocol 20 evicts inactive persistent entries once their TTL reaches zero.
//! This contract acts as a rent manager that:
//!
//! 1. Tracks the TTL of critical protocol state (total supply, user deposits,
//!    and the global configuration).
//! 2. Extends only the specific keys touched by a given entrypoint (avoiding
//!  indiscriminate extension of every key in the contract, which inflates
//!  instruction fees).
//! 3. Provides a restoration proxy that allows a user (acting as their own
"//    keeper) to pay rent and bring their archived deposit back to the live
//