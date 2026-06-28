use std::str::FromStr;

use omnifs_sdk::captures::{Captures, FromCaptures, PathSegment};
use omnifs_sdk::identity::Facet;
use omnifs_sdk::object::FacetMetadata;

#[omnifs_sdk::path_segment]
#[derive(Debug, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
enum StateFilter {
    OpenNow,
    All,
}

#[omnifs_sdk::path_segment]
#[derive(Debug, PartialEq, Eq)]
enum ItemKind {
    #[strum(serialize = "issues")]
    Issues,
    #[strum(serialize = "pulls")]
    Pulls,
}

fn is_valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[omnifs_sdk::path_segment(validate = is_valid_slug)]
#[derive(Debug, PartialEq, Eq)]
struct Slug(String);

fn is_valid_owner(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[omnifs_sdk::path_segment(validate = is_valid_owner, normalize = str::to_ascii_lowercase)]
#[derive(Debug, PartialEq, Eq)]
struct Owner(String);

#[omnifs_sdk::path_captures]
struct FacetedKey {
    filter: Facet<StateFilter>,
}

#[test]
fn enum_segment_parses_renders_and_exposes_choices() {
    assert_eq!(StateFilter::from_str("open_now"), Ok(StateFilter::OpenNow));
    assert_eq!(StateFilter::OpenNow.to_string(), "open_now");
    assert_eq!(StateFilter::All.as_ref(), "all");
    assert_eq!(
        StateFilter::choices().expect("StateFilter has finite choices"),
        ["open_now", "all"]
    );
    assert!(StateFilter::from_str("closed").is_err());
}

#[test]
fn enum_segment_honors_per_variant_serialized_names() {
    assert_eq!(ItemKind::from_str("issues"), Ok(ItemKind::Issues));
    assert_eq!(ItemKind::Pulls.to_string(), "pulls");
    assert_eq!(
        ItemKind::choices().expect("ItemKind has finite choices"),
        ["issues", "pulls"]
    );
}

#[test]
fn string_segment_validates_and_renders() {
    let slug = Slug::from_str("abc-123").expect("valid slug");
    assert_eq!(slug.as_str(), "abc-123");
    assert_eq!(slug.as_ref(), "abc-123");
    assert_eq!(slug.to_string(), "abc-123");
    assert!(Slug::from_str("bad/slash").is_err());
    assert_eq!(Slug::choices(), None);
}

#[test]
fn string_segment_normalizes_after_validation() {
    let owner = Owner::from_str("Octo_Cat").expect("valid owner");
    assert_eq!(owner.as_str(), "octo_cat");
    assert!(Owner::from_str(".github").is_err());
}

#[test]
fn path_captures_facet_axes_use_generated_choices() {
    let axes = FacetedKey::facet_axes();
    assert_eq!(axes.len(), 1);
    assert_eq!(axes[0].capture_name, "filter");
    assert_eq!(axes[0].choices, ["open_now", "all"]);

    let key = FacetedKey::from_captures(&Captures::new(vec![omnifs_sdk::captures::Capture {
        name: "filter".to_string(),
        value: "open_now".to_string(),
    }]))
    .expect("valid capture");
    assert_eq!(key.filter.0, StateFilter::OpenNow);
}
