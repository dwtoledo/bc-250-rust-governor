#!/usr/bin/env python3
"""
BC-250 Rust Governor Simulation & Tuning Tool
Simulates the exact 1:1 governor core algorithm from src/governor_core.rs
against various real-world GPU workloads (Desktop, Heavy Gaming, Esports).
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import math
import random
import sys
import time
from typing import Callable, Deque, Dict, List, Optional, Tuple


@dataclasses.dataclass(frozen=True)
class GovernorConfig:
    # Timing
    burst_samples: int = 20
    ramp_up_samples: int = 64
    ramp_down_samples: int = 256
    sample_us: int = 2000
    adjust_us: int = 8000
    finetune_us: int = 50000

    # Ramp rates (MHz per millisecond)
    burst_rate: float = 1000.0
    up_rate: float = 50.0
    up_medium_rate: float = 25.0
    up_slow_rate: float = 10.0
    up_crawl_rate: float = 2.0
    down_rate: float = 0.2

    # Load target bands (0.0 to 1.0)
    band_upper: float = 0.90
    band_medium: float = 0.80
    band_slow: float = 0.70
    band_crawl: float = 0.60
    band_lower: float = 0.40

    # Frequency thresholds (MHz)
    threshold_adjust: int = 100
    threshold_finetune: int = 25

    # Hardware bounds (MHz)
    min_freq: int = 350
    max_freq: int = 2230

    def to_toml_snippet(self) -> str:
        return f"""[timing]
burst-samples = {self.burst_samples}
ramp-up-samples = {self.ramp_up_samples}
ramp-down-samples = {self.ramp_down_samples}
intervals = {{ sample = {self.sample_us}, adjust = {self.adjust_us}, finetune = {self.finetune_us} }}
ramp-rates = {{ burst = {self.burst_rate:.1f}, up = {self.up_rate:.1f}, up-medium = {self.up_medium_rate:.1f}, up-slow = {self.up_slow_rate:.1f}, up-crawl = {self.up_crawl_rate:.1f}, down = {self.down_rate:.2f} }}

[frequency-thresholds]
adjust = {self.threshold_adjust}
finetune = {self.threshold_finetune}

[load-target]
upper = {self.band_upper:.2f}
medium = {self.band_medium:.2f}
slow = {self.band_slow:.2f}
crawl = {self.band_crawl:.2f}
lower = {self.band_lower:.2f}"""


# ---------------------------------------------------------------------------
# 1:1 Algorithmic Model matching src/governor_core.rs
# ---------------------------------------------------------------------------

def busy_ratio(history: Deque[bool], window: int) -> float:
    if not history or window == 0:
        return 0.0
    if len(history) >= window:
        # Take the newest `window` samples
        count = sum(1 for i in range(1, window + 1) if history[-i])
        return count / window
    else:
        return sum(history) / len(history)


def is_burst(history: Deque[bool], burst_samples: int) -> bool:
    if burst_samples <= 0 or len(history) < burst_samples:
        return False
    for i in range(1, burst_samples + 1):
        if not history[-i]:
            return False
    return True


def step_target(
    target: float,
    burst: bool,
    busy_up: float,
    busy_down: float,
    cfg: GovernorConfig,
    delta_ms: float,
) -> float:
    if burst:
        nxt = target + cfg.burst_rate * delta_ms
    elif busy_up > cfg.band_upper:
        nxt = target + cfg.up_rate * delta_ms
    elif busy_up > cfg.band_medium:
        nxt = target + cfg.up_medium_rate * delta_ms
    elif busy_up > cfg.band_slow:
        nxt = target + cfg.up_slow_rate * delta_ms
    elif busy_up > cfg.band_crawl:
        nxt = target + cfg.up_crawl_rate * delta_ms
    elif busy_down < cfg.band_lower:
        nxt = target - cfg.down_rate * delta_ms
    else:
        nxt = target

    return max(float(cfg.min_freq), min(float(cfg.max_freq), nxt))


def should_request_clock(
    pending: bool,
    burst: bool,
    adjust_due: bool,
    finetune_due: bool,
    diff: int,
    cfg: GovernorConfig,
) -> bool:
    if pending or diff == 0:
        return False
    return (
        burst
        or (adjust_due and diff >= cfg.threshold_adjust)
        or (finetune_due and diff >= cfg.threshold_finetune)
    )


# ---------------------------------------------------------------------------
# Workload Scenarios
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class WorkloadScenario:
    name: str
    description: str
    duration_s: float
    # Function returning target load in [0.0, 1.0] at time t (seconds)
    load_func: Callable[[float], float]


def scenario_desktop_idle(duration_s: float = 10.0) -> WorkloadScenario:
    def load(t: float) -> float:
        # Idle at ~2% with brief 150ms spikes every 3 seconds (opening windows)
        cycle = t % 3.0
        if 1.0 <= cycle <= 1.15:
            return 0.85
        if 2.2 <= cycle <= 2.30:
            return 0.65
        return 0.02

    return WorkloadScenario(
        name="Desktop Idle with Occasional UI Spikes",
        description="Low background activity with brief UI spikes. Expects low clock at floor and minimal fan noise.",
        duration_s=duration_s,
        load_func=load,
    )


def scenario_heavy_gaming(duration_s: float = 10.0) -> WorkloadScenario:
    def load(t: float) -> float:
        # Sustained heavy game: starts after 0.5s loading, stays at 95%-100%
        if t < 0.5:
            return 0.20
        return 0.95 + 0.05 * math.sin(t * 10.0)

    return WorkloadScenario(
        name="Heavy Sustained Gaming (e.g. Cyberpunk / Witcher 3)",
        description="Continuous 95-100% load. Expects immediate burst to max clock and 0 downward oscillations.",
        duration_s=duration_s,
        load_func=load,
    )


def scenario_oscillating_esports(duration_s: float = 12.0) -> WorkloadScenario:
    def load(t: float) -> float:
        # Fast fluctuating workload between 45% and 85%
        base = 0.65
        wave1 = 0.20 * math.sin(t * 4.0)
        wave2 = 0.10 * math.cos(t * 9.0)
        return max(0.20, min(0.95, base + wave1 + wave2))

    return WorkloadScenario(
        name="Dynamic / Oscillating Esports (e.g. CS2 / Dota 2)",
        description="Rapidly fluctuating load. Tests anti-hunting / anti-jitter to prevent frame stutters.",
        duration_s=duration_s,
        load_func=load,
    )


def scenario_scene_transitions(duration_s: float = 16.0) -> WorkloadScenario:
    def load(t: float) -> float:
        # Alternating: Menu (15%) -> Heavy Fight (98%) -> Inventory (35%) -> Boss (100%)
        if t < 3.0:
            return 0.15
        elif t < 8.0:
            return 0.98
        elif t < 11.0:
            return 0.35
        else:
            return 1.00

    return WorkloadScenario(
        name="Scene Transitions (Menu -> Fight -> Inventory -> Boss)",
        description="Realistic gameplay transitions with sharp load shifts.",
        duration_s=duration_s,
        load_func=load,
    )


# ---------------------------------------------------------------------------
# Simulation Engine
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class SimulationResult:
    scenario_name: str
    duration_s: float
    total_ticks: int
    driver_writes: int
    writes_per_second: float
    avg_applied_clock: float
    time_to_max_ms: Optional[float]
    jitter_reversals: int
    time_near_floor_pct: float
    time_near_ceiling_pct: float
    underclock_penalty: float
    overclock_penalty: float
    fitness_score: float


def run_simulation(cfg: GovernorConfig, scenario: WorkloadScenario) -> SimulationResult:
    delta_s = cfg.sample_us / 1_000_000.0
    delta_ms = cfg.sample_us / 1000.0
    total_ticks = int(scenario.duration_s / delta_s)

    max_samples = max(cfg.ramp_up_samples, cfg.ramp_down_samples, cfg.burst_samples)
    history: Deque[bool] = collections.deque(maxlen=max_samples)

    applied_freq = cfg.min_freq
    target_freq = float(cfg.min_freq)
    pending_apply_until_us = 0

    last_adjust_us = 0
    last_finetune_us = 0
    driver_writes = 0
    clock_history: List[int] = []

    time_to_max_ms: Optional[float] = None
    jitter_reversals = 0
    last_direction = 0  # +1 up, -1 down, 0 neutral
    last_dir_change_time = 0.0

    underclock_penalty = 0.0
    overclock_penalty = 0.0

    # Deterministic pseudo-random seed per scenario for exact reproducibility
    rng = random.Random(42)

    for tick in range(total_ticks):
        current_time_s = tick * delta_s
        current_time_us = int(current_time_s * 1_000_000)

        # 1. Hardware load at this instant
        instant_load = scenario.load_func(current_time_s)
        # Sample probability: instantaneous load determines if register reads busy
        # Sub-sample noise to emulate real DRM register polling
        is_busy = rng.random() < instant_load
        history.append(is_busy)

        # 2. Complete in-flight async driver writes
        pending = current_time_us < pending_apply_until_us

        # 3. Governor core calculation
        burst = is_burst(history, cfg.burst_samples)
        busy_up = busy_ratio(history, cfg.ramp_up_samples)
        busy_down = busy_ratio(history, cfg.ramp_down_samples)

        target_freq = step_target(
            target_freq,
            burst,
            busy_up,
            busy_down,
            cfg,
            delta_ms,
        )
        target_u16 = int(round(target_freq))

        # Check time-to-max under heavy demand
        if instant_load > 0.90 and time_to_max_ms is None:
            if applied_freq >= cfg.max_freq - 50:
                time_to_max_ms = current_time_s * 1000.0

        # 4. Driver apply decision
        diff = abs(applied_freq - target_u16)
        adjust_due = (current_time_us - last_adjust_us) >= cfg.adjust_us
        finetune_due = (current_time_us - last_finetune_us) >= cfg.finetune_us

        if should_request_clock(pending, burst, adjust_due, finetune_due, diff, cfg):
            driver_writes += 1
            direction = 1 if target_u16 > applied_freq else -1
            if direction != last_direction and (current_time_s - last_dir_change_time) < 0.15:
                jitter_reversals += 1
            last_direction = direction
            last_dir_change_time = current_time_s

            applied_freq = target_u16
            # Emulate async setter latency (~800µs)
            pending_apply_until_us = current_time_us + 800

            if diff >= cfg.threshold_adjust:
                last_adjust_us = current_time_us
            if diff >= cfg.threshold_finetune:
                last_finetune_us = current_time_us

        clock_history.append(applied_freq)

        # 5. Evaluate quality metrics
        # Ideal clock proportional to load
        ideal_clock = cfg.min_freq + instant_load * (cfg.max_freq - cfg.min_freq)
        clock_deficit = ideal_clock - applied_freq
        if clock_deficit > 100.0 and instant_load > 0.60:
            underclock_penalty += (clock_deficit / 1000.0) * delta_s
        elif applied_freq > ideal_clock + 400.0 and instant_load < 0.15:
            overclock_penalty += ((applied_freq - ideal_clock) / 1000.0) * delta_s

    writes_per_sec = driver_writes / scenario.duration_s
    avg_clock = sum(clock_history) / len(clock_history)
    near_floor = sum(1 for c in clock_history if c <= cfg.min_freq + 100) / len(clock_history) * 100.0
    near_ceil = sum(1 for c in clock_history if c >= cfg.max_freq - 100) / len(clock_history) * 100.0

    # Composite Fitness Score (0 - 100, higher is better)
    # - Penalize underclocking (game starvation / drop in fps) heavily
    # - Penalize erratic jitter (microstutters)
    # - Penalize excessive driver writes (> 25/s wastes CPU cycles)
    # - Penalize overclocking during idle (fan noise / heat)
    score = 100.0
    score -= underclock_penalty * 35.0
    score -= overclock_penalty * 15.0
    score -= jitter_reversals * 1.5
    if writes_per_sec > 25.0:
        score -= (writes_per_sec - 25.0) * 1.2

    fitness_score = max(0.0, min(100.0, score))

    return SimulationResult(
        scenario_name=scenario.name,
        duration_s=scenario.duration_s,
        total_ticks=total_ticks,
        driver_writes=driver_writes,
        writes_per_second=writes_per_sec,
        avg_applied_clock=avg_clock,
        time_to_max_ms=time_to_max_ms,
        jitter_reversals=jitter_reversals,
        time_near_floor_pct=near_floor,
        time_near_ceiling_pct=near_ceil,
        underclock_penalty=underclock_penalty,
        overclock_penalty=overclock_penalty,
        fitness_score=fitness_score,
    )


def evaluate_suite(cfg: GovernorConfig) -> Tuple[float, List[SimulationResult]]:
    scenarios = [
        scenario_desktop_idle(),
        scenario_heavy_gaming(),
        scenario_oscillating_esports(),
        scenario_scene_transitions(),
    ]
    results = [run_simulation(cfg, sc) for sc in scenarios]
    avg_score = sum(r.fitness_score for r in results) / len(results)
    return avg_score, results


# ---------------------------------------------------------------------------
# Parameter Optimizer
# ---------------------------------------------------------------------------

def mutate_config(base: GovernorConfig, temperature: float = 1.0) -> GovernorConfig:
    """Produces a realistic mutation of governor parameters within safe bounds."""
    factor = temperature * 0.25

    def perturb_int(val: int, low: int, high: int) -> int:
        delta = int(round(random.gauss(0, max(1.0, (high - low) * factor * 0.2))))
        return max(low, min(high, val + delta))

    def perturb_float(val: float, low: float, high: float, step: float = 0.05) -> float:
        delta = random.gauss(0, (high - low) * factor * 0.15)
        raw = max(low, min(high, val + delta))
        return round(raw / step) * step

    burst_samples = perturb_int(base.burst_samples, 10, 40)
    ramp_up = perturb_int(base.ramp_up_samples, 32, 128)
    ramp_down = perturb_int(base.ramp_down_samples, 128, 512)

    # Intervals (µs)
    sample_us = perturb_int(base.sample_us, 1000, 4000)
    adjust_us = perturb_int(base.adjust_us, 4000, 16000)
    finetune_us = perturb_int(base.finetune_us, 20000, 80000)

    # Rates
    burst_rate = perturb_float(base.burst_rate, 600.0, 1800.0, 50.0)
    up_rate = perturb_float(base.up_rate, 30.0, 90.0, 5.0)
    up_med = perturb_float(base.up_medium_rate, 15.0, 45.0, 5.0)
    up_slow = perturb_float(base.up_slow_rate, 5.0, 20.0, 1.0)
    up_crawl = perturb_float(base.up_crawl_rate, 1.0, 5.0, 0.5)
    down_rate = perturb_float(base.down_rate, 0.10, 2.00, 0.05)

    # Bands must stay strictly monotonic: upper > medium > slow > crawl > lower
    b_upper = perturb_float(base.band_upper, 0.85, 0.95, 0.02)
    b_med = perturb_float(min(b_upper - 0.05, base.band_medium), 0.75, b_upper - 0.02, 0.02)
    b_slow = perturb_float(min(b_med - 0.05, base.band_slow), 0.60, b_med - 0.02, 0.02)
    b_crawl = perturb_float(min(b_slow - 0.05, base.band_crawl), 0.45, b_slow - 0.02, 0.02)
    b_lower = perturb_float(min(b_crawl - 0.05, base.band_lower), 0.25, b_crawl - 0.02, 0.02)

    thresh_adj = perturb_int(base.threshold_adjust, 60, 150)
    thresh_fine = perturb_int(base.threshold_finetune, 15, 40)

    return GovernorConfig(
        burst_samples=burst_samples,
        ramp_up_samples=ramp_up,
        ramp_down_samples=ramp_down,
        sample_us=sample_us,
        adjust_us=adjust_us,
        finetune_us=finetune_us,
        burst_rate=burst_rate,
        up_rate=up_rate,
        up_medium_rate=up_med,
        up_slow_rate=up_slow,
        up_crawl_rate=up_crawl,
        down_rate=down_rate,
        band_upper=b_upper,
        band_medium=b_med,
        band_slow=b_slow,
        band_crawl=b_crawl,
        band_lower=b_lower,
        threshold_adjust=thresh_adj,
        threshold_finetune=thresh_fine,
        min_freq=base.min_freq,
        max_freq=base.max_freq,
    )


def optimize_governor(
    baseline: GovernorConfig,
    iterations: int = 150,
) -> Tuple[GovernorConfig, float, List[SimulationResult]]:
    best_cfg = baseline
    best_score, best_results = evaluate_suite(baseline)

    print(f"[*] Starting parameter search across {iterations} iterations...")
    print(f"[*] Baseline Quality Score: {best_score:.2f} / 100.0\n")

    start_time = time.time()
    improvements = 0

    for i in range(1, iterations + 1):
        temp = max(0.1, 1.0 - (i / iterations))
        candidate = mutate_config(best_cfg, temperature=temp)
        cand_score, cand_results = evaluate_suite(candidate)

        if cand_score > best_score:
            improvements += 1
            diff = cand_score - best_score
            best_score = cand_score
            best_cfg = candidate
            best_results = cand_results
            print(f"  [Iter {i:3d}/{iterations}] New Best Score: {best_score:5.2f} (+{diff:4.2f})")

    elapsed = time.time() - start_time
    print(f"\n[+] Optimization completed in {elapsed:.2f}s ({improvements} improvements found).")
    return best_cfg, best_score, best_results


# ---------------------------------------------------------------------------
# CLI & Formatting
# ---------------------------------------------------------------------------

def print_results_table(title: str, results: List[SimulationResult], score: float) -> None:
    print(f"\n{'='*78}")
    print(f"  {title.upper()} (Overall Score: {score:.2f}/100)")
    print(f"{'='*78}")
    print(f"{'Scenario':<34} | {'Writes/s':<9} | {'AvgClock':<9} | {'Jitter':<7} | {'Score':<6}")
    print(f"{'-'*34}-+-{'-'*9}-+-{'-'*9}-+-{'-'*7}-+-{'-'*6}")
    for r in results:
        name_short = r.scenario_name[:34]
        print(f"{name_short:<34} | {r.writes_per_second:7.1f}/s | {r.avg_applied_clock:6.0f}MHz | {r.jitter_reversals:7d} | {r.fitness_score:5.1f}")
    print(f"{'='*78}\n")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="BC-250 Rust Governor Simulation & Tuning Tool",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--benchmark",
        action="store_true",
        help="Run simulation on default configuration across all workload scenarios",
    )
    parser.add_argument(
        "--tune",
        action="store_true",
        help="Run parameter optimizer to find the best TOML configuration",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=120,
        help="Number of iterations for the optimizer (default: 120)",
    )

    args = parser.parse_args()

    baseline = GovernorConfig()

    if args.tune:
        base_score, base_results = evaluate_suite(baseline)
        print_results_table("Baseline Configuration", base_results, base_score)

        optimal_cfg, optimal_score, optimal_results = optimize_governor(
            baseline, iterations=args.iterations
        )
        print_results_table("Tuned Configuration", optimal_results, optimal_score)

        print("\n" + "=" * 78)
        print("  RECOMMENDED OPTIMIZED TOML CONFIGURATION")
        print("  (Copy and paste into /etc/bc-250-rust-governor/config.toml)")
        print("=" * 78)
        print(optimal_cfg.to_toml_snippet())
        print("=" * 78 + "\n")
    else:
        # Default: benchmark mode
        score, results = evaluate_suite(baseline)
        print_results_table("Default Configuration Benchmark", results, score)
        print("Tip: Run with '--tune' to automatically find the highest-performing config parameters:")
        print("     python3 tools/simulate.py --tune\n")


if __name__ == "__main__":
    main()
