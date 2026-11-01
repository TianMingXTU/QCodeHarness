use bench_greet_npe::greet;

#[test]
fn greets_with_capitalized_name() {
    assert_eq!(greet("qi"), "Hello, Qi");
}

#[test]
fn empty_name_gets_a_placeholder() {
    assert_eq!(greet(""), "Hello, stranger");
}
