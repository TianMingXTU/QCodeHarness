use bench_sum::top_n_sum;

#[test]
fn champion_counts_alone() {
    assert_eq!(top_n_sum(&[5, 3, 1], 1), 5);
}

#[test]
fn top_two_sum() {
    assert_eq!(top_n_sum(&[5, 3], 2), 8);
}

#[test]
fn full_leaderboard_sums_every_score() {
    assert_eq!(top_n_sum(&[5, 3, 1], 3), 9);
}
