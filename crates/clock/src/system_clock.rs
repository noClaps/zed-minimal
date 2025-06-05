use std::time::Instant;

pub trait SystemClock: Send + Sync {
    /// Returns the current date and time in UTC.
    fn utc_now(&self) -> Instant;
}

pub struct RealSystemClock;

impl SystemClock for RealSystemClock {
    fn utc_now(&self) -> Instant {
        Instant::now()
    }
}
