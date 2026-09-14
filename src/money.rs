//! YNAB speaks milliunits: 1000 milliunits = 1.00 in the plan's currency.
//! Convert exactly once at the edges — integers on the wire, decimals in tool output.

pub type Milliunits = i64;

/// Format milliunits as a decimal string with two places, e.g. -12340 -> "-12.34".
pub fn to_decimal(m: Milliunits) -> String {
    let sign = if m < 0 { "-" } else { "" };
    let cents = (m.abs() + 5) / 10;
    format!("{sign}{}.{:02}", cents / 100, cents % 100)
}

/// Convert a decimal currency amount (as supplied by a tool caller) to milliunits.
pub fn from_decimal(amount: f64) -> Milliunits {
    (amount * 1000.0).round() as Milliunits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_negative_amounts() {
        assert_eq!(to_decimal(-12340), "-12.34");
    }

    #[test]
    fn formats_whole_and_small_amounts() {
        assert_eq!(to_decimal(5000), "5.00");
        assert_eq!(to_decimal(10), "0.01");
        assert_eq!(to_decimal(0), "0.00");
    }

    #[test]
    fn rounds_sub_cent_milliunits() {
        assert_eq!(to_decimal(12345), "12.35");
        assert_eq!(to_decimal(12344), "12.34");
    }

    #[test]
    fn parses_decimal_to_milliunits() {
        assert_eq!(from_decimal(-12.34), -12340);
        assert_eq!(from_decimal(0.1 + 0.2), 300);
    }
}
