//! Derived values for rendering: numbers the panels show that aren't stored
//! on `AppState` directly.

/// Tokens per second, guarded against non-positive time and zero tokens.
/// Returns 0.0 when a rate can't be computed.
pub fn tokens_per_sec(tokens: u32, secs: f64) -> f64 {
    if secs > 0.0 && tokens > 0 {
        tokens as f64 / secs
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tps_guards_zero_and_negative() {
        assert_eq!(tokens_per_sec(0, 5.0), 0.0);
        assert_eq!(tokens_per_sec(100, 0.0), 0.0);
        assert_eq!(tokens_per_sec(100, -1.0), 0.0);
        assert!((tokens_per_sec(100, 4.0) - 25.0).abs() < f64::EPSILON);
    }
}
