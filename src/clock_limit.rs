use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const CLOCK_APPLY_ATTEMPTS: u32 = 3;
pub const CLOCK_SKIP: Duration = Duration::from_secs(1);

/// A frequency the kernel refused. It stays blocked until `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkippedFrequency {
    pub freq: u16,
    pub until: Instant,
}

impl SkippedFrequency {
    pub fn start(freq: u16, now: Instant) -> Self {
        Self {
            freq,
            until: now + CLOCK_SKIP,
        }
    }

    pub fn expired(self, now: Instant) -> bool {
        now >= self.until
    }

    /// Only the exact refused frequency is blocked, and only before it expires.
    pub fn blocks(self, target: u16, now: Instant) -> bool {
        !self.expired(now) && self.freq == target
    }
}

/// Order of exact safe-points to try after `requested` was refused.
/// The next higher point comes first, then the lower points from highest to lowest.
/// `requested` itself is not included.
pub fn recovery_candidates(requested: u16, points: &BTreeMap<u16, u16>) -> Vec<(u16, u16)> {
    let mut order = Vec::new();
    if let Some(above) = requested.checked_add(1) {
        if let Some((&freq, &voltage)) = points.range(above..).next() {
            order.push((freq, voltage));
        }
    }
    order.extend(
        points
            .range(..requested)
            .rev()
            .map(|(&freq, &voltage)| (freq, voltage)),
    );
    order
}

#[cfg(test)]
mod tests {
    use super::{recovery_candidates, SkippedFrequency, CLOCK_SKIP};
    use std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    };

    fn points() -> BTreeMap<u16, u16> {
        BTreeMap::from([(1620, 800), (1760, 850), (1890, 900), (2030, 950)])
    }

    #[test]
    fn refused_frequency_tries_the_next_point_then_descends() {
        assert_eq!(
            recovery_candidates(1800, &points()),
            vec![(1890, 900), (1760, 850), (1620, 800)]
        );
    }

    #[test]
    fn exact_point_is_not_retried() {
        assert_eq!(
            recovery_candidates(1890, &points()),
            vec![(2030, 950), (1760, 850), (1620, 800)]
        );
    }

    #[test]
    fn top_point_only_descends() {
        assert_eq!(
            recovery_candidates(2030, &points()),
            vec![(1890, 900), (1760, 850), (1620, 800)]
        );
    }

    #[test]
    fn bottom_point_only_steps_up() {
        assert_eq!(recovery_candidates(1620, &points()), vec![(1760, 850)]);
    }

    #[test]
    fn only_point_has_no_neighbor() {
        let only = BTreeMap::from([(1500, 775)]);
        assert!(recovery_candidates(1500, &only).is_empty());
    }

    #[test]
    fn skip_blocks_only_the_exact_frequency_and_then_expires() {
        let start = Instant::now();
        let skip = SkippedFrequency::start(1800, start);
        assert!(skip.blocks(1800, start));
        assert!(!skip.blocks(1801, start));
        assert!(!skip.blocks(1890, start));
        assert!(skip.blocks(1800, start + CLOCK_SKIP - Duration::from_millis(1)));
        assert!(!skip.blocks(1800, start + CLOCK_SKIP));
        assert!(skip.expired(start + CLOCK_SKIP));
    }
}
