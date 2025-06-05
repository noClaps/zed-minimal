use std::sync::LazyLock;

use derive_more::Deref;
use regex::Regex;
use url::Url;

/// The URL to a Git remote.
#[derive(Debug, PartialEq, Eq, Clone, Deref)]
pub struct RemoteUrl(Url);

static USERNAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-zA-Z\-_]+@").expect("Failed to create USERNAME_REGEX"));

impl std::str::FromStr for RemoteUrl {
    type Err = url::ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if USERNAME_REGEX.is_match(input) {
            // Rewrite remote URLs like `git@github.com:user/repo.git` to `ssh://git@github.com/user/repo.git`
            let ssh_url = format!("ssh://{}", input.replacen(':', "/", 1));
            Ok(RemoteUrl(Url::parse(&ssh_url)?))
        } else {
            Ok(RemoteUrl(Url::parse(input)?))
        }
    }
}
