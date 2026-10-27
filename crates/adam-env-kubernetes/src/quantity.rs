//! Kubernetes quantities (`250m`, `1.5`, `512Mi`, `2Gi`, `1G`, `129e6`) as whole numbers, to weight a
//! command's time by the requests of the pod it ran in.
//!
//! The grammar is `apimachinery`'s `resource.Quantity`: an optional `+`, a decimal number (`5`, `5.`,
//! `.5`, `1.25`), and one suffix: binary (`Ki Mi Gi Ti Pi Ei`, powers of 1024), decimal (`n u m k M G T P
//! E`, powers of 1000, `m` is a thousandth) or an exponent (`e3`, `E-3`, `e+3`). A negative number is not a
//! request. Arithmetic is exact (integers, no floating point) and a fraction of a unit is rounded **up**,
//! as Kubernetes does (`MilliValue`, `Value`).

/// CPU in thousandths of a core: `250m` is 250, `1.5` is 1500, `100u` is 1.
pub(crate) fn cpu_millis(quantity: &str) -> Option<u64> {
    whole(quantity, 3)
}

/// A size in bytes: `512Mi` is 536870912, `1G` is 1000000000.
pub(crate) fn bytes(quantity: &str) -> Option<u64> {
    whole(quantity, 0)
}

/// The quantity times `10^unit`, rounded up, when it is one and the result fits a `u64`.
fn whole(quantity: &str, unit: i32) -> Option<u64> {
    let parsed = parse(quantity)?;
    // value = digits * 10^(decimal) * 1024^binary; the unit moves the decimal exponent.
    let decimal = parsed.decimal.checked_add(unit)?;
    let mut numerator = parsed.digits;
    for _ in 0..parsed.binary {
        numerator = numerator.checked_mul(1024)?;
    }
    let rounded = if decimal >= 0 {
        numerator.checked_mul(pow10(u32::try_from(decimal).ok()?)?)?
    } else {
        let divisor = pow10(u32::try_from(-decimal).ok()?)?;
        numerator / divisor + u128::from(numerator % divisor != 0)
    };
    u64::try_from(rounded).ok()
}

/// A quantity taken apart: `digits * 10^decimal * 1024^binary`.
struct Parsed {
    digits: u128,
    decimal: i32,
    binary: u32,
}

fn parse(quantity: &str) -> Option<Parsed> {
    let text = quantity.strip_prefix('+').unwrap_or(quantity);
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(end);
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (number, ""),
    };
    if whole.len() + fraction.len() == 0 || fraction.contains('.') {
        return None;
    }
    let digits: u128 = format!("{whole}{fraction}").parse().ok()?;
    let places = i32::try_from(fraction.len()).ok()?;

    let (decimal, binary) = match suffix {
        "" => (0, 0),
        "Ki" => (0, 1),
        "Mi" => (0, 2),
        "Gi" => (0, 3),
        "Ti" => (0, 4),
        "Pi" => (0, 5),
        "Ei" => (0, 6),
        "n" => (-9, 0),
        "u" => (-6, 0),
        "m" => (-3, 0),
        "k" => (3, 0),
        "M" => (6, 0),
        "G" => (9, 0),
        "T" => (12, 0),
        "P" => (15, 0),
        "E" => (18, 0),
        other => (exponent(other)?, 0),
    };
    Some(Parsed {
        digits,
        decimal: decimal.checked_sub(places)?,
        binary,
    })
}

/// `e3`, `E-3`, `e+3`: the exponent of a decimal number.
fn exponent(suffix: &str) -> Option<i32> {
    let digits = suffix.strip_prefix(['e', 'E'])?;
    let (negative, digits) = match digits.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, digits.strip_prefix('+').unwrap_or(digits)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let exponent: i32 = digits.parse().ok()?;
    Some(if negative { -exponent } else { exponent })
}

/// `10^power` when it fits a `u128`.
fn pow10(power: u32) -> Option<u128> {
    10u128.checked_pow(power)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_in_thousandths_of_a_core_rounded_up() {
        for (quantity, millis) in [
            ("250m", 250),
            ("1", 1000),
            ("2", 2000),
            ("1.5", 1500),
            ("0.1", 100),
            ("0.5", 500),
            (".5", 500),
            ("5.", 5000),
            ("+1", 1000),
            ("2000m", 2000),
            ("0", 0),
            ("0m", 0),
            ("100u", 1),
            ("1500u", 2),
            ("0.0005", 1),
            ("1.2345678", 1235),
            ("1k", 1_000_000),
            ("1e3", 1_000_000),
            ("1E3", 1_000_000),
            ("1e-3", 1),
            ("500e-3", 500),
        ] {
            assert_eq!(cpu_millis(quantity), Some(millis), "{quantity}");
        }
        // An exa of cores is out of range for a u64 of thousandths: refused, not wrapped.
        assert_eq!(cpu_millis("1E"), None);
    }

    #[test]
    fn memory_is_in_bytes_with_binary_and_decimal_suffixes() {
        for (quantity, bytes_) in [
            ("512Mi", 536_870_912),
            ("2Gi", 2_147_483_648),
            ("1Ki", 1024),
            ("1.5Gi", 1_610_612_736),
            ("1G", 1_000_000_000),
            ("1000k", 1_000_000),
            ("1M", 1_000_000),
            ("129e6", 129_000_000),
            ("129E6", 129_000_000),
            ("128974848", 128_974_848),
            ("1Ti", 1 << 40),
            ("1Pi", 1 << 50),
            ("1Ei", 1 << 60),
            ("1E", 1_000_000_000_000_000_000),
            ("123456789m", 123_457),
            ("0", 0),
            ("0Gi", 0),
            ("1500m", 2),
        ] {
            assert_eq!(bytes(quantity), Some(bytes_), "{quantity}");
        }
    }

    #[test]
    fn what_is_not_a_quantity_is_none() {
        for bad in [
            "", " ", "m", "Gi", "1x", "1 Gi", " 1", "1 ", "-1", "-0", "--1", "++1", "1..2",
            "1.2.3", ".", "+", "1Gi2", "1e", "1e+", "1E+", "1ki", "1K", "1gi", "0x10", "NaN",
            "inf", "1_000", "1E1E1", "1e3e3", "1.5.5Gi", "１", "1mi",
        ] {
            assert_eq!(cpu_millis(bad), None, "{bad:?}");
            assert_eq!(bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_number_too_large_to_hold_is_refused_not_wrapped() {
        for huge in [
            "99999999999999999999Ei",
            "18446744073709551616",
            "1e40",
            "1e9999999999",
            "123456789012345678901234567890123456789",
        ] {
            assert_eq!(bytes(huge), None, "{huge}");
        }
        assert_eq!(bytes("18446744073709551615"), Some(u64::MAX));
        // A fraction of the smallest unit is a whole one, however far down it is written.
        assert_eq!(bytes("1e-30"), Some(1));
        assert_eq!(bytes("0e-30"), Some(0));
    }
}
