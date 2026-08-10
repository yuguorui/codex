pub(crate) fn get(name: &str) -> Option<&'static str> {
    match name {
        "code-review" => Some(include_str!("bundled/code-review.js")),
        "deep-research" => Some(include_str!("bundled/deep-research.js")),
        _ => None,
    }
}

#[cfg(test)]
#[path = "bundled_tests.rs"]
mod tests;
