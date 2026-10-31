use broken_repo::greet;

#[test]
fn greets_the_person_by_name() {
    assert_eq!(greet("Qi"), "Hello, Qi!");
}
