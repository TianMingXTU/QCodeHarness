use bench_word_count::word_counts;

#[test]
fn counts_repeated_words_case_sensitively() {
    let counts = word_counts("a b A c a");
    assert_eq!(counts.get("a"), Some(&2));
    assert_eq!(counts.get("b"), Some(&1));
    assert_eq!(counts.get("A"), Some(&1));
    assert_eq!(counts.get("c"), Some(&1));
}

#[test]
fn whitespace_only_text_yields_no_counts() {
    assert!(word_counts("").is_empty());
    assert!(word_counts("   \t  ").is_empty());
}
