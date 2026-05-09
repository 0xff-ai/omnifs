use std::path::Path;

pub(crate) fn is_valid_component(name: &str) -> bool {
    let has_forbidden_byte = name.is_empty()
        || name.as_bytes().contains(&0)
        || name.contains('/')
        || name.contains('\\');
    if has_forbidden_byte {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}
