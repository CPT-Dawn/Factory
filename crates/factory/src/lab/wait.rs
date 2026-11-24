//! `factory wait lease`: blocks, silent, until a lab lease is held no more, on the daemon,
//! which keeps the lease store and frees an exited owner's leases.

use std::time::Duration;

use serde_json::Value;

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{WAIT_LEASE, WaitLeaseParams, WaitLeaseResult};

pub fn wait(lease: u32, timeout: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let waiter = client::waiter();
    let params = |resume, timeout_secs| WaitLeaseParams {
        lease: i64::from(lease),
        waiter: waiter.clone(),
        timeout_secs,
        resume,
    };
    let answer: WaitLeaseResult = client::hold(&paths, WAIT_LEASE, params, timeout)?;
    Ok(Done::new(
        format!("lease {} ended", answer.lease),
        serde_json::to_value(&answer).unwrap_or(Value::Null),
    ))
}
