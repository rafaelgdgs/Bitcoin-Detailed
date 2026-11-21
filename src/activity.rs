use std::collections::VecDeque;
use std::time::Instant;

use crate::mining::Hash256;

pub const HISTORY_SECONDS: usize = 30;
pub const RECENT_SECONDS: usize = 10;
const EVENT_LIMIT: usize = 6;

#[derive(Clone, Copy)]
struct Sample {
    second: u64,
    hash: Hash256,
}

#[derive(Clone)]
pub struct HashHistory {
    started: Instant,
    samples: [Option<Sample>; HISTORY_SECONDS],
}

impl Default for HashHistory {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            samples: [None; HISTORY_SECONDS],
        }
    }
}

impl HashHistory {
    pub fn observe(&mut self, at: Instant, hash: Hash256) {
        let second = at.saturating_duration_since(self.started).as_secs();
        let slot = &mut self.samples[(second % HISTORY_SECONDS as u64) as usize];
        if slot.is_none_or(|sample| {
            second > sample.second || (second == sample.second && hash < sample.hash)
        }) {
            *slot = Some(Sample { second, hash });
        }
    }

    pub fn series(&self, now: Instant) -> [Option<Hash256>; HISTORY_SECONDS] {
        let current = now.saturating_duration_since(self.started).as_secs();
        std::array::from_fn(|index| {
            let second = current.checked_sub((HISTORY_SECONDS - 1 - index) as u64)?;
            self.samples[(second % HISTORY_SECONDS as u64) as usize]
                .filter(|sample| sample.second == second)
                .map(|sample| sample.hash)
        })
    }
}

#[derive(Clone)]
pub struct MiningEvent {
    pub at: Instant,
    pub text: String,
    pub count: u64,
}

pub fn record_event(events: &mut VecDeque<MiningEvent>, text: &str) {
    let at = Instant::now();
    if let Some(latest) = events.back_mut().filter(|event| event.text == text) {
        latest.at = at;
        latest.count += 1;
    } else {
        if events.len() == EVENT_LIMIT {
            events.pop_front();
        }
        events.push_back(MiningEvent {
            at,
            text: text.to_owned(),
            count: 1,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn history_keeps_the_lowest_sample_per_second_and_expires_it() {
        let mut history = HashHistory::default();
        let start = history.started;
        let second = |n| start + Duration::from_secs(n);
        history.observe(second(0), [4; 32]);
        history.observe(second(0), [3; 32]);
        history.observe(second(0), [5; 32]);
        history.observe(second(2), [6; 32]);
        assert_eq!(
            &history.series(second(2))[27..],
            &[Some([3; 32]), None, Some([6; 32])]
        );
        assert_eq!(
            history.series(second(9))[HISTORY_SECONDS - RECENT_SECONDS],
            Some([3; 32])
        );
        assert!(
            history.series(second(10))[HISTORY_SECONDS - RECENT_SECONDS..]
                .iter()
                .all(|sample| *sample != Some([3; 32]))
        );
        history.observe(second(30), [8; 32]);
        history.observe(second(0), [0; 32]);
        assert_eq!(history.series(second(30))[29], Some([8; 32]));
        assert_eq!(history.series(second(60)), [None; HISTORY_SECONDS]);
    }
}
