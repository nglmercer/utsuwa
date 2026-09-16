//! Injectable clock so scheduling, leases, and backoff are deterministic
//! under test.

use crate::model::Ms;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Ms;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Ms {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as Ms)
            .unwrap_or(0)
    }
}

#[derive(Debug)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    pub fn new(now: Ms) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(now),
        })
    }

    pub fn advance(&self, delta_ms: Ms) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }

    pub fn set(&self, now: Ms) {
        self.now.store(now, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> Ms {
        self.now.load(Ordering::SeqCst)
    }
}
