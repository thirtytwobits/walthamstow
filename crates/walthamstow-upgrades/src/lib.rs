//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's upgrade framework: staged files published through the kernel's
//! [`publication`](walthamstow_kernel::publication) under a journal that fences each by what it
//! held when staged, and recovery of an upgrade a journal authorised.

pub mod reason_codes;
pub mod workspace;
