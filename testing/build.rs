extern crate skeptic;

fn main() {
    skeptic::generate_doc_tests(&[
        "../README.md",
        "../template-example.md",
        "tests/hashtag-test.md",
        "tests/rand-gen-range-test.md",
        "tests/cookbook-rand-test.md",
        "tests/rand-version-conflict-test.md",
        "tests/should-panic-test.md",
        "tests/section-names.md",
        "tests/two-rand-versions-real.md",
    ]);
}
