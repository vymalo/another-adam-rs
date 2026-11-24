//! How a person writes a memory size and a CPU share, and the defaults of a command's limits
//! (ADR 0065 decision 11). Pure: the machine's RAM and core count are arguments.

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

/// The default memory of one command: a quarter of the RAM, at most 8 GB. `None` (the RAM is
/// unknown) is 2 GB.
#[must_use]
pub fn default_command_memory(total_ram: Option<u64>) -> u64 {
    total_ram.map_or(2 * GIB, |ram| (ram / 4).min(8 * GIB))
}

/// The default CPU of one command, in millicores: half the cores, at least one.
#[must_use]
pub fn default_command_cpu(cores: u32) -> u32 {
    (cores.max(1) / 2).max(1).saturating_mul(1000)
}

/// A size in bytes: a whole number, optionally followed by `B`, `K`, `KB`, `KiB`, `M`, `MB`, `MiB`,
/// `G`, `GB` or `GiB` (ASCII case ignored; every unit is binary, so `2GB` is `2 * 1024^3`).
///
/// # Errors
///
/// A message that says what is wrong.
pub fn parse_bytes(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| "not a size: a whole number, then B, KB, MB or GB".to_owned())?;
    let size = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => KIB,
        "m" | "mb" | "mib" => MIB,
        "g" | "gb" | "gib" => GIB,
        other => return Err(format!("unknown unit {other:?}: use B, KB, MB or GB")),
    };
    n.checked_mul(size)
        .ok_or_else(|| "too large to be a size".to_owned())
}

/// A CPU share in millicores: cores as a decimal (`2`, `1.5`), or millicores with an `m` (`500m`).
///
/// # Errors
///
/// A message that says what is wrong.
pub fn parse_millicores(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let bad = || "not a CPU share: cores (`2`, `1.5`) or millicores (`500m`)".to_owned();
    if let Some(milli) = text.strip_suffix('m') {
        return milli.trim().parse::<u32>().map_err(|_| bad());
    }
    let cores: f64 = text.parse().map_err(|_| bad())?;
    if !cores.is_finite() || cores < 0.0 || cores > f64::from(u32::MAX / 1000) {
        return Err(bad());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((cores * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_read_in_binary_units() {
        assert_eq!(parse_bytes("2GB"), Ok(2 * GIB));
        assert_eq!(parse_bytes(" 512 MiB "), Ok(512 * MIB));
        assert_eq!(parse_bytes("64k"), Ok(64 * KIB));
        assert_eq!(parse_bytes("1048576"), Ok(MIB));
        assert!(parse_bytes("lots").is_err());
        assert!(parse_bytes("2TB").is_err());
        assert!(parse_bytes("-1").is_err());
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("99999999999999GB").is_err());
    }

    #[test]
    fn shares_are_cores_or_millicores() {
        assert_eq!(parse_millicores("2"), Ok(2000));
        assert_eq!(parse_millicores("1.5"), Ok(1500));
        assert_eq!(parse_millicores("500m"), Ok(500));
        assert!(parse_millicores("fast").is_err());
        assert!(parse_millicores("-1").is_err());
        assert!(parse_millicores("NaN").is_err());
    }

    #[test]
    fn the_defaults_follow_the_machine() {
        assert_eq!(default_command_memory(Some(16 * GIB)), 4 * GIB);
        assert_eq!(
            default_command_memory(Some(128 * GIB)),
            8 * GIB,
            "at most 8 GB"
        );
        assert_eq!(default_command_memory(None), 2 * GIB);
        assert_eq!(default_command_cpu(8), 4000);
        assert_eq!(default_command_cpu(1), 1000, "at least one core");
        assert_eq!(default_command_cpu(0), 1000);
    }
}
