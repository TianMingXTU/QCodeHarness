use bench_utils::format;
use bench_utils::normalize;
use bench_utils::parse;
use bench_utils::stats;
use bench_utils::validators;

// ── normalize ────────────────────────────────────────────────────────

#[test]
fn min_max_peaks_at_one() {
    let scaled = normalize::min_max(&[2.0, 4.0, 8.0]);
    assert!((scaled[2] - 1.0).abs() < 1e-9, "峰值要规范到 1: {scaled:?}");
    assert!((scaled[0] - 0.0).abs() < 1e-9, "谷值要规范到 0: {scaled:?}");
}

#[test]
fn min_max_is_monotonic() {
    let scaled = normalize::min_max(&[1.0, 2.0, 3.0]);
    assert!(scaled[0] < scaled[1] && scaled[1] < scaled[2], "{scaled:?}");
}

#[test]
fn zero_mean_centers_the_series() {
    let centered = normalize::zero_mean(&[1.0, 2.0, 3.0]);
    let mean = centered.iter().sum::<f64>() / centered.len() as f64;
    assert!(mean.abs() < 1e-9, "均值要为 0: {centered:?}");
}

// ── stats ────────────────────────────────────────────────────────────

#[test]
fn median_of_an_odd_series() {
    assert_eq!(stats::median(&[3.0, 1.0, 2.0]), 2.0);
}

#[test]
fn median_of_an_even_series_is_the_middle_average() {
    assert_eq!(stats::median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
}

#[test]
fn normalized_mean_of_uniform_values_is_half() {
    let values = [0.0, 10.0];
    // 0-1 区间上均匀分布的两个点的均值是 0.5。
    assert!((stats::normalized_mean(&values) - 0.5).abs() < 1e-9);
}

// ── format ───────────────────────────────────────────────────────────

#[test]
fn summary_line_reports_both_statistics() {
    let line = format::summary_line(&[1.0, 2.0, 3.0]);
    assert!(line.starts_with("median=2.00"), "{line}");
    assert!(line.contains("mean="), "{line}");
}

#[test]
fn percent_multiplies_by_one_hundred() {
    assert_eq!(format::percent(0.42), "42.0%");
}

// ── parse ────────────────────────────────────────────────────────────

#[test]
fn parse_numbers_accepts_comma_separated_values() {
    assert_eq!(parse::parse_numbers("1, 2, 3").unwrap(), vec![1.0, 2.0, 3.0]);
}

#[test]
fn parse_numbers_rejects_garbage() {
    assert!(parse::parse_numbers("1, x, 3").is_err());
}

#[test]
fn parse_setting_reads_key_and_value() {
    let (key, value) = parse::parse_setting("timeout = 30").unwrap();
    assert_eq!(key, "timeout");
    assert_eq!(value, 30.0);
}

// ── validators ───────────────────────────────────────────────────────

#[test]
fn four_digit_ids_are_valid() {
    assert!(validators::is_valid_id("1234"));
}

#[test]
fn two_digit_ids_are_invalid() {
    assert!(!validators::is_valid_id("12"));
}

#[test]
fn ids_with_letters_are_invalid() {
    assert!(!validators::is_valid_id("12a4"));
}

#[test]
fn ratios_within_zero_to_one_are_valid() {
    assert!(validators::is_valid_ratio("0.5"));
    assert!(!validators::is_valid_ratio("1.5"));
}
