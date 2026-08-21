pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(ParsedVersion::Semver(l)), Some(ParsedVersion::Semver(c))) => Some(l > c),
        (Some(ParsedVersion::ForkRelease(l)), Some(ParsedVersion::ForkRelease(c))) => Some(l > c),
        _ => None,
    }
}

pub(crate) fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .strip_prefix("rust-v")
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

pub(crate) fn is_source_build_version(version: &str) -> bool {
    parse_version(version) == Some(ParsedVersion::Semver((0, 0, 0)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedVersion {
    Semver((u64, u64, u64)),
    ForkRelease(u64),
}

fn parse_version(v: &str) -> Option<ParsedVersion> {
    let version = v.trim();
    if version.len() == 12 && version.bytes().all(|byte| byte.is_ascii_digit()) {
        return version.parse().ok().map(ParsedVersion::ForkRelease);
    }

    let mut iter = version.split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    Some(ParsedVersion::Semver((maj, min, pat)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extracts_version_from_latest_tag() {
        assert_eq!(
            extract_version_from_latest_tag("rust-v1.5.0").expect("failed to parse version"),
            "1.5.0"
        );
    }

    #[test]
    fn latest_tag_without_prefix_is_invalid() {
        assert!(extract_version_from_latest_tag("v1.5.0").is_err());
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn fork_release_comparisons_work() {
        assert_eq!(is_newer("202608231531", "202608231530"), Some(true));
        assert_eq!(is_newer("202608231530", "202608231531"), Some(false));
        assert_eq!(is_newer("202608231530", "0.145.0"), None);
    }

    #[test]
    fn source_build_version_is_not_checked() {
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(
            parse_version(" 1.2.3 \n"),
            Some(ParsedVersion::Semver((1, 2, 3)))
        );
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }
}
