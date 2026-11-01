//! Opens the code index file of this computer with the worker that reads repositories, as a tool call and
//! `harness-code build` both need it.

use std::time::Duration;

use super::supervise::Worker;
use super::{CodeIndex, worker};
use crate::support::Env;

/// The code index of this computer (`code.sqlite3` in the data folder), created when missing.
pub fn open_index(env: &Env) -> Result<CodeIndex, String> {
    let path = crate::location::database_path(env).ok_or("the data folder is unknown")?;

    let mut index = CodeIndex::open(&path)?;

    // the index is read in a process of its own that is killed past its limits, unless a unit test
    // asks for the old way
    // (only a debug build honours that: a release build always reads the repository in a worker)
    if !(cfg!(debug_assertions) && env.var(worker::INPROCESS_VAR) == Some("1")) {
        index.set_worker(Worker::new(env)?);
    }

    // a debug build lets the tests shorten the time one call may take
    if cfg!(debug_assertions)
        && let Some(ms) = env.var(worker::BUDGET_VAR).and_then(|value| value.parse::<u64>().ok())
    {
        index.set_budget(Duration::from_millis(ms));
    }

    Ok(index)
}
