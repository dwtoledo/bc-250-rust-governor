use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PerformanceMode {
    Normal,
    MaxPerformance,
}

#[derive(Debug, Clone)]
pub enum GovCommand {
    SetFrequency(u16),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum SetterAck {
    Applied {
        freq: u16,
        #[allow(dead_code)]
        voltage: u16,
        latency_us: u64,
    },
    Failed {
        freq: u16,
        error: String,
    },
}

pub struct GovernorState {
    pub target_freq: f32,
    pub applied_freq: u16,
    pub pending_freq: Option<u16>,
    pub last_ack: Instant,
    pub performance_mode: PerformanceMode,
}

impl GovernorState {
    pub fn new(min_freq: u16) -> Self {
        Self {
            target_freq: f32::from(min_freq),
            applied_freq: min_freq,
            pending_freq: None,
            last_ack: Instant::now(),
            performance_mode: PerformanceMode::Normal,
        }
    }
}

#[derive(Default, Debug)]
pub struct GovernorStats {
    pub total_applies: u64,
    pub failed_applies: u64,
    pub burst_activations: u64,
    pub total_latency_us: u64,
    pub max_latency_us: u64,
    pub start_time: Option<Instant>,
}

impl GovernorStats {
    pub fn record_apply(&mut self, latency_us: u64) {
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }
        self.total_applies += 1;
        self.total_latency_us += latency_us;
        self.max_latency_us = self.max_latency_us.max(latency_us);
    }

    pub fn record_failure(&mut self) {
        self.failed_applies += 1;
    }

    pub fn record_burst(&mut self) {
        self.burst_activations += 1;
    }

    pub fn avg_latency_us(&self) -> u64 {
        if self.total_applies > 0 {
            self.total_latency_us / self.total_applies
        } else {
            0
        }
    }

    pub fn success_rate(&self) -> f32 {
        let total = self.total_applies + self.failed_applies;
        if total > 0 {
            (self.total_applies as f32) / (total as f32) * 100.0
        } else {
            0.0
        }
    }
}
