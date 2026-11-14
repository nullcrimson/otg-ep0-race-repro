//! Runs the rounds on a worker thread and watches for a round that never finishes.
//!
//! A serial call to a device whose USB has stopped answering can block forever, so the rounds
//! cannot run on the thread that has to notice the stall.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// A round takes a few milliseconds; one taking this long is not coming back.
pub const STALL: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(250);
const PROGRESS_EVERY: u32 = 500;

pub enum Outcome {
    Completed,
    /// `round` 0: stalled before the first round started.
    Stalled {
        round: u32,
    },
    Failed {
        round: u32,
        error: anyhow::Error,
    },
}

/// Shared between the worker, which advances it, and the watchdog, which reads it.
pub struct RoundCounter {
    current: AtomicU32,
    last: u32,
}

impl RoundCounter {
    fn new(last: u32) -> RoundCounter {
        RoundCounter {
            current: AtomicU32::new(0),
            last,
        }
    }

    /// Starts the next round and returns its number, or `None` once the last round has run.
    pub fn next(&self) -> Option<u32> {
        let next = self.current() + 1;
        if next > self.last {
            return None;
        }
        self.current.store(next, Ordering::Relaxed);
        Some(next)
    }

    fn current(&self) -> u32 {
        self.current.load(Ordering::Relaxed)
    }
}

pub fn run_rounds<F>(rounds: u32, drive: F) -> Outcome
where
    F: FnOnce(&RoundCounter) -> anyhow::Result<()> + Send + 'static,
{
    let counter = Arc::new(RoundCounter::new(rounds));
    let worker = {
        let counter = Arc::clone(&counter);
        thread::spawn(move || drive(&counter))
    };

    let mut last_seen = 0;
    let mut last_progress = Instant::now();
    while !worker.is_finished() {
        thread::sleep(POLL);
        let round = counter.current();
        if round != last_seen {
            if round / PROGRESS_EVERY != last_seen / PROGRESS_EVERY {
                println!("{} rounds done", round / PROGRESS_EVERY * PROGRESS_EVERY);
            }
            last_seen = round;
            last_progress = Instant::now();
        } else if last_progress.elapsed() > STALL {
            // The worker stays blocked in a serial call; it is left behind and ends with the
            // process.
            return Outcome::Stalled { round };
        }
    }

    let round = counter.current();
    match worker.join() {
        Ok(Ok(())) => Outcome::Completed,
        Ok(Err(error)) => Outcome::Failed { round, error },
        Err(_) => Outcome::Failed {
            round,
            error: anyhow::anyhow!("the worker thread panicked"),
        },
    }
}
