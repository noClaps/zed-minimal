use std::time;
use std::{sync::Arc, time::Instant};

use clock::SystemClock;

const COALESCE_TIMEOUT: time::Duration = time::Duration::from_secs(20);
const SIMULATED_DURATION_FOR_SINGLE_EVENT: time::Duration = time::Duration::from_millis(1);

#[derive(Debug, PartialEq)]
struct PeriodData {
    environment: &'static str,
    start: Instant,
    end: Option<Instant>,
}

pub struct EventCoalescer {
    clock: Arc<dyn SystemClock>,
    state: Option<PeriodData>,
}

impl EventCoalescer {
    pub fn new(clock: Arc<dyn SystemClock>) -> Self {
        Self { clock, state: None }
    }

    pub fn log_event(
        &mut self,
        environment: &'static str,
    ) -> Option<(Instant, Instant, &'static str)> {
        let log_time = self.clock.utc_now();

        let Some(state) = &mut self.state else {
            self.state = Some(PeriodData {
                start: log_time,
                end: None,
                environment,
            });
            return None;
        };

        let period_end = state
            .end
            .unwrap_or(state.start + SIMULATED_DURATION_FOR_SINGLE_EVENT);
        let within_timeout = log_time - period_end < COALESCE_TIMEOUT;
        let environment_is_same = state.environment == environment;
        let should_coaelesce = !within_timeout || !environment_is_same;

        if should_coaelesce {
            let previous_environment = state.environment;
            let original_start = state.start;

            state.start = log_time;
            state.end = None;
            state.environment = environment;

            return Some((
                original_start,
                if within_timeout { log_time } else { period_end },
                previous_environment,
            ));
        }

        state.end = Some(log_time);

        None
    }
}
