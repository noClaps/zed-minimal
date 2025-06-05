use std::str::FromStr;

use url::Url;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostingProvider, ParsedGitRemote,
    RemoteUrl,
};

pub struct Sourcehut;

impl GitHostingProvider for Sourcehut {
    fn name(&self) -> String {
        "SourceHut".to_string()
    }

    fn base_url(&self) -> Url {
        Url::parse("https://git.sr.ht").unwrap()
    }

    fn supports_avatars(&self) -> bool {
        false
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != "git.sr.ht" {
            return None;
        }

        let mut path_segments = url.path_segments()?;
        let owner = path_segments.next()?.trim_start_matches('~');
        // We don't trim the `.git` suffix here like we do elsewhere, as
        // sourcehut treats a repo with `.git` suffix as a separate repo.
        //
        // For example, `git@git.sr.ht:~username/repo` and `git@git.sr.ht:~username/repo.git`
        // are two distinct repositories.
        let repo = path_segments.next()?;

        Some(ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner, repo } = remote;

        self.base_url()
            .join(&format!("~{owner}/{repo}/commit/{sha}"))
            .unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self
            .base_url()
            .join(&format!("~{owner}/{repo}/tree/{sha}/item/{path}"))
            .unwrap();
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }
}
