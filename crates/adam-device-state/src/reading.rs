//! The four readings a host pushes: power, thermal, network and memory pressure.

use std::fmt;

/// Where the device's power comes from (ADR 0065 decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Power {
    /// On mains: charging or full.
    OnMains,
    /// On its battery.
    OnBattery {
        /// The charge left, in percent (`0` to `100`).
        percent: u8,
        /// The device's own low-power mode is on.
        low_power: bool,
    },
}

impl Power {
    /// On the battery with `percent` left and low-power mode `low_power`, the charge clamped to
    /// `0..=100`.
    #[must_use]
    pub const fn on_battery(percent: u8, low_power: bool) -> Self {
        Self::OnBattery {
            percent: if percent > 100 { 100 } else { percent },
            low_power,
        }
    }

    /// The charge left, in percent; `None` on mains, where there is nothing to measure.
    #[must_use]
    pub const fn percent(self) -> Option<u8> {
        match self {
            Self::OnMains => None,
            Self::OnBattery { percent, .. } => Some(percent),
        }
    }

    /// Whether the device's low-power mode is on. Always `false` on mains.
    #[must_use]
    pub const fn low_power(self) -> bool {
        matches!(
            self,
            Self::OnBattery {
                low_power: true,
                ..
            }
        )
    }

    /// Whether the device is charging, which is what [`Self::OnMains`] says.
    #[must_use]
    pub const fn is_charging(self) -> bool {
        matches!(self, Self::OnMains)
    }

    /// Whether the battery is below `threshold` percent, or the device is in low-power mode.
    ///
    /// On mains it is never low: a plugged-in device has nothing to conserve.
    #[must_use]
    pub const fn is_low(self, threshold: u8) -> bool {
        match self {
            Self::OnMains => false,
            Self::OnBattery { percent, low_power } => low_power || percent < threshold,
        }
    }
}

impl fmt::Display for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OnMains => f.write_str("on mains"),
            Self::OnBattery { percent, low_power } => {
                write!(f, "on battery at {percent}%")?;
                if *low_power {
                    f.write_str(", low power")?;
                }
                Ok(())
            }
        }
    }
}

/// How hot the device is (ADR 0065 decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Thermal {
    /// No thermal pressure.
    Nominal,
    /// Warm, but working normally.
    Fair,
    /// Hot: the device is holding work back.
    Serious,
    /// At its limit: the device is throttling hard.
    Critical,
}

impl Thermal {
    /// The label of the state in a report or a log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Fair => "fair",
            Self::Serious => "serious",
            Self::Critical => "critical",
        }
    }

    /// Whether the device is hot enough to hold new work back (`Serious` or `Critical`).
    #[must_use]
    pub const fn is_hot(self) -> bool {
        matches!(self, Self::Serious | Self::Critical)
    }
}

impl fmt::Display for Thermal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the network costs (ADR 0065 decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Network {
    /// Unmetered: a large fetch costs nothing extra.
    Unmetered,
    /// Metered: the person pays for what is fetched.
    Metered,
    /// No network at all.
    Offline,
}

impl Network {
    /// The label of the state in a report or a log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmetered => "unmetered",
            Self::Metered => "metered",
            Self::Offline => "offline",
        }
    }

    /// Whether the link is metered.
    #[must_use]
    pub const fn is_metered(self) -> bool {
        matches!(self, Self::Metered)
    }

    /// Whether there is no link at all.
    #[must_use]
    pub const fn is_offline(self) -> bool {
        matches!(self, Self::Offline)
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How close the device is to running out of memory (ADR 0065 decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum MemoryPressure {
    /// Memory is plentiful.
    Normal,
    /// Memory is tight.
    Warn,
    /// The device is reclaiming or swapping hard.
    Critical,
}

impl MemoryPressure {
    /// The label of the state in a report or a log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }

    /// Whether the device is in critical memory pressure.
    #[must_use]
    pub const fn is_critical(self) -> bool {
        matches!(self, Self::Critical)
    }
}

impl fmt::Display for MemoryPressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One consistent set of the four readings, as a host pushes them.
///
/// The four travel together so that the engine decides on one moment, not on four reads that
/// another thread may have changed between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceReadings {
    /// Mains or battery.
    pub power: Power,
    /// How hot the device is.
    pub thermal: Thermal,
    /// What the network costs.
    pub network: Network,
    /// How close the device is to running out of memory.
    pub memory_pressure: MemoryPressure,
}

impl DeviceReadings {
    /// Nothing to conserve: on mains, cool, unmetered, memory normal.
    pub const NOMINAL: Self = Self {
        power: Power::OnMains,
        thermal: Thermal::Nominal,
        network: Network::Unmetered,
        memory_pressure: MemoryPressure::Normal,
    };

    /// The four readings, one of each.
    #[must_use]
    pub const fn new(
        power: Power,
        thermal: Thermal,
        network: Network,
        memory_pressure: MemoryPressure,
    ) -> Self {
        Self {
            power,
            thermal,
            network,
            memory_pressure,
        }
    }

    /// Whether every reading is [`Self::NOMINAL`], so nothing is held back.
    #[must_use]
    pub const fn is_nominal(&self) -> bool {
        matches!(self.power, Power::OnMains)
            && matches!(self.thermal, Thermal::Nominal)
            && matches!(self.network, Network::Unmetered)
            && matches!(self.memory_pressure, MemoryPressure::Normal)
    }
}

impl Default for DeviceReadings {
    fn default() -> Self {
        Self::NOMINAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_battery_is_low_below_its_threshold_or_in_low_power_mode() {
        assert!(Power::on_battery(19, false).is_low(20));
        assert!(!Power::on_battery(20, false).is_low(20));
        assert!(Power::on_battery(90, true).is_low(20));
        assert!(
            !Power::OnMains.is_low(20),
            "a plugged-in device conserves nothing"
        );
        assert_eq!(Power::on_battery(250, false).percent(), Some(100));
        assert_eq!(Power::OnMains.percent(), None);
        assert!(Power::OnMains.is_charging());
        assert!(!Power::on_battery(50, false).is_charging());
    }

    #[test]
    fn a_charge_is_clamped_to_a_hundred() {
        assert_eq!(Power::on_battery(0, false).percent(), Some(0));
        assert_eq!(Power::on_battery(100, true).percent(), Some(100));
        assert_eq!(Power::on_battery(101, false), Power::on_battery(100, false));
    }

    #[test]
    fn the_closed_values_carry_the_adr_labels() {
        assert_eq!(Thermal::Serious.as_str(), "serious");
        assert!(Thermal::Serious.is_hot() && Thermal::Critical.is_hot());
        assert!(!Thermal::Fair.is_hot() && !Thermal::Nominal.is_hot());
        assert!(Network::Metered.is_metered());
        assert!(Network::Offline.is_offline());
        assert!(!Network::Unmetered.is_metered());
        assert!(MemoryPressure::Critical.is_critical());
        assert_eq!(MemoryPressure::Warn.as_str(), "warn");
    }

    #[test]
    fn the_nominal_reading_holds_nothing_back() {
        assert!(DeviceReadings::NOMINAL.is_nominal());
        assert_eq!(DeviceReadings::default(), DeviceReadings::NOMINAL);
        let degraded = DeviceReadings::new(
            Power::on_battery(12, false),
            Thermal::Nominal,
            Network::Unmetered,
            MemoryPressure::Normal,
        );
        assert!(!degraded.is_nominal());
        assert_eq!(degraded.power.percent(), Some(12));
    }

    #[test]
    fn a_reading_names_itself_in_words() {
        assert_eq!(
            Power::on_battery(9, true).to_string(),
            "on battery at 9%, low power"
        );
        assert_eq!(Power::OnMains.to_string(), "on mains");
        assert_eq!(Thermal::Fair.to_string(), "fair");
        assert_eq!(Network::Metered.to_string(), "metered");
        assert_eq!(MemoryPressure::Normal.to_string(), "normal");
    }
}
