use bench_report::aggregate;
use bench_report::observations;
use bench_report::rank;
use bench_report::render;

// ── observations ─────────────────────────────────────────────────────

#[test]
fn parse_line_reads_source_and_value() {
    let observation = observations::parse_line("sensor-a: 12.5").unwrap();
    assert_eq!(observation.source, "sensor-a");
    assert_eq!(observation.value, 12.5);
}

#[test]
fn parse_line_rejects_a_missing_colon() {
    assert!(observations::parse_line("sensor-a 12.5").is_err());
}

#[test]
fn group_by_source_collects_each_source() {
    let parsed: Vec<_> = ["a: 1", "b: 2", "a: 3"]
        .iter()
        .map(|line| observations::parse_line(line).unwrap())
        .collect();
    let groups = observations::group_by_source(&parsed);
    assert_eq!(groups["a"], vec![1.0, 3.0]);
    assert_eq!(groups["b"], vec![2.0]);
}

// ── aggregate ────────────────────────────────────────────────────────

#[test]
fn average_of_a_single_observation_is_itself() {
    let summary = aggregate::summarize("a", &[12.0]);
    assert!((summary.average - 12.0).abs() < 1e-9, "{summary:?}");
}

#[test]
fn average_of_two_observations_is_their_mean() {
    let summary = aggregate::summarize("a", &[10.0, 20.0]);
    assert!((summary.average - 15.0).abs() < 1e-9, "{summary:?}");
}

#[test]
fn max_is_the_largest_value() {
    let summary = aggregate::summarize("a", &[3.0, 9.0, 4.0]);
    assert!((summary.max - 9.0).abs() < 1e-9, "{summary:?}");
}

#[test]
fn count_is_the_number_of_observations() {
    let summary = aggregate::summarize("a", &[1.0, 2.0, 3.0]);
    assert_eq!(summary.count, 3);
}

// ── rank ─────────────────────────────────────────────────────────────

#[test]
fn the_loudest_source_ranks_first() {
    let mut summaries = vec![
        aggregate::summarize("quiet", &[1.0]),
        aggregate::summarize("loud", &[100.0]),
        aggregate::summarize("middle", &[10.0]),
    ];
    rank::by_average_descending(&mut summaries);
    assert_eq!(summaries[0].source, "loud");
    assert_eq!(summaries[2].source, "quiet");
}

// ── render ───────────────────────────────────────────────────────────

#[test]
fn the_title_stands_on_its_own_line() {
    let summaries = vec![aggregate::summarize("a", &[1.0])];
    let text = render::render(&summaries);
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line, "观测报表", "标题要独占一行: {text:?}");
}

#[test]
fn each_group_gets_its_own_line() {
    let summaries = vec![
        aggregate::summarize("a", &[1.0, 2.0]),
        aggregate::summarize("b", &[5.0]),
    ];
    let text = render::render(&summaries);
    assert_eq!(text.lines().count(), 3, "标题 + 两组: {text:?}");
}
