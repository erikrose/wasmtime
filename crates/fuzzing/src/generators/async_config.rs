use arbitrary::{Arbitrary, Unstructured};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use wasmtime::TimingWheelInterrupter;

/// Configuration for async support within a store.
///
/// Note that the `Arbitrary` implementation for this type always returns
/// `Disabled` because this is something that is statically chosen if the fuzzer
/// has support for async.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AsyncConfig {
    /// No async support enabled.
    Disabled,
    /// Async support is enabled, and cooperative yielding is done with fuel.
    YieldWithFuel(u64),
    /// Async support is enabled, and cooperative yielding is done with epochs.
    YieldWithEpochs {
        /// Duration between epoch ticks.
        dur: Duration,
        /// Number of ticks between yields.
        ticks: u64,
    },
    /// Async support is enabled, and cooperative yielding is done with MMU
    /// interruption.
    YieldWithMmu {
        /// How many timing-wheel ticks each store gets before being interrupted
        ticks_per_interrupt: u32,
        /// Number of trips around the timing wheel to wait to recycle an
        /// interrupt page
        cycles_of_cooldown: u32,
        /// Interval between ticks of the timing wheel
        dur: Duration,
        /// The MMU-based interrupter: always the default timing wheel. We wrap
        /// it in `EqualWheel` so Eq and Hash don't insist on penetrating
        /// uselessly into its uncomparable locks, interrupt page ptrs, etc. The
        /// only datum of the wheel we care about comparing, for the purposes of
        /// reproducing a fuzz case, is `ticks_per_interval`, so we surface that
        /// at the top level of `YieldWithMmu`.
        wheel: EqualWheel,
    },
}

impl AsyncConfig {
    /// Applies this async configuration to the `wasmtime::Config` provided to
    /// ensure it's ready to execute with the resulting modules.
    pub fn configure(&self, config: &mut wasmtime::Config) {
        match self {
            AsyncConfig::Disabled => {}
            AsyncConfig::YieldWithFuel(_) => {
                config.consume_fuel(true);
            }
            AsyncConfig::YieldWithEpochs { .. } => {
                config.epoch_interruption(true);
            }
            AsyncConfig::YieldWithMmu { wheel, .. } => {
                // TODO: Pass cycles_of_cooldown through to timing wheel once
                // the wheel implements it.
                config.with_mmu_interrupter(wheel.arc.clone());
                config.mmu_interruption(true);
            }
        }
    }
}

impl<'a> Arbitrary<'a> for AsyncConfig {
    fn arbitrary(_: &mut Unstructured<'a>) -> arbitrary::Result<AsyncConfig> {
        Ok(AsyncConfig::Disabled)
    }
}

/// A wrapper around the timing wheel that reports every instance as equal to
/// every other, ignoring their internal uncomparable ptrs and locks.
#[derive(Clone)]
pub struct EqualWheel {
    pub arc: Arc<TimingWheelInterrupter>,
}

impl PartialEq for EqualWheel {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for EqualWheel {}

impl Hash for EqualWheel {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

impl std::fmt::Debug for EqualWheel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EqualWheel")
    }
}
