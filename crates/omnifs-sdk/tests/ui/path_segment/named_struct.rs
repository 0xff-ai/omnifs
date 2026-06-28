fn valid(_: &str) -> bool {
    true
}

#[omnifs_sdk::path_segment(validate = valid)]
struct Bad {
    value: String,
}

fn main() {}
