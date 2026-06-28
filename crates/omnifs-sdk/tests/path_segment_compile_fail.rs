#[test]
fn path_segment_rejects_invalid_shapes() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/path_segment/*.rs");
}
