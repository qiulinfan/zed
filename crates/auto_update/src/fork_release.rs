//! Updates from this personal fork's own GitHub Releases.
//!
//! The fork's CI (`.github/workflows/fork_bundle.yml`) publishes every push to
//! `stable` as a release tagged `fork-v<version>-<commit10>`. The updater reads
//! only these releases: zed.dev would install the official build over the
//! fork. Fork builds of one upstream version share its version number, so a
//! release is newer when GitHub says its commit is ahead of the commit this
//! build was made from.

use anyhow::{Context as _, Result};
use futures_lite::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, Request};
use semver::{BuildMetadata, Version};
use serde::Deserialize;
use serde::de::DeserializeOwned;

pub const FORK_REPO: &str = "qiulinfan/zed";
const API: &str = "https://api.github.com/repos/qiulinfan/zed";
const DOWNLOADS: &str = "https://github.com/qiulinfan/zed/releases/download/";
const RELEASES_URL: &str = "https://api.github.com/repos/qiulinfan/zed/releases?per_page=20";

/// The release asset holding this platform's update, or `None` where the fork
/// publishes none and the updater stays off.
pub fn fork_asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("Zed-aarch64.dmg"),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitHubRelease {
    tag_name: String,
    target_commitish: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForkRelease {
    /// The release's version with its commit's first ten digits as build
    /// metadata (`1.26.2+25cb848292`), so two fork builds of one version differ.
    pub version: Version,
    pub commit: String,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Relation {
    Newer,
    Same,
    Older,
    Diverged,
}

fn parse_tag(tag: &str) -> Option<(Version, &str)> {
    let rest = tag.strip_prefix("fork-v")?;
    let (version, commit) = rest.rsplit_once('-')?;
    if !(7..=40).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((version.parse().ok()?, commit))
}

/// The newest published fork release carrying `asset`.
pub(crate) fn pick_release(releases: Vec<GitHubRelease>, asset: &str) -> Option<ForkRelease> {
    releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease && release.published_at.is_some())
        .filter(|release| {
            release.target_commitish.len() == 40
                && release
                    .target_commitish
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
        })
        .filter_map(|release| {
            let (mut version, _) = parse_tag(&release.tag_name)?;
            let url = release
                .assets
                .iter()
                .find(|candidate| candidate.name == asset)?
                .browser_download_url
                .clone();
            if !url.starts_with(DOWNLOADS) {
                return None;
            }
            version.build = BuildMetadata::new(&release.target_commitish[..10]).ok()?;
            Some((
                release.published_at.clone(),
                ForkRelease {
                    version,
                    commit: release.target_commitish,
                    url,
                },
            ))
        })
        // RFC 3339 timestamps in one zone order as strings.
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, release)| release)
}

async fn get_json<T: DeserializeOwned>(client: &dyn HttpClient, url: &str) -> Result<T> {
    let request = Request::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "zed-fork-updater")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::empty())?;
    let mut response = client.send(request).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {} for {url}: {}",
        response.status(),
        String::from_utf8_lossy(&body),
    );
    serde_json::from_slice(&body).with_context(|| format!("unexpected answer from {url}"))
}

/// The newest fork release carrying `asset`.
pub(crate) async fn latest_release(client: &dyn HttpClient, asset: &str) -> Result<ForkRelease> {
    let releases: Vec<GitHubRelease> = get_json(client, RELEASES_URL).await?;
    pick_release(releases, asset)
        .with_context(|| format!("no {FORK_REPO} release carries {asset} yet"))
}

/// Whether `release` would move this build forward. A higher version is newer
/// and a lower one older; at the same version GitHub's comparison of the two
/// commits decides, so a local build of commits not pushed yet is never
/// replaced by an older release.
pub(crate) async fn relation(
    client: &dyn HttpClient,
    release: &ForkRelease,
    installed: &Version,
    installed_commit: Option<&str>,
) -> Result<Relation> {
    let core = |v: &Version| (v.major, v.minor, v.patch);
    match core(&release.version).cmp(&core(installed)) {
        std::cmp::Ordering::Greater => return Ok(Relation::Newer),
        std::cmp::Ordering::Less => return Ok(Relation::Older),
        std::cmp::Ordering::Equal => {}
    }
    let Some(installed_commit) = installed_commit else {
        return Ok(Relation::Newer);
    };
    if release.commit.starts_with(installed_commit) || installed_commit.starts_with(&release.commit)
    {
        return Ok(Relation::Same);
    }

    #[derive(Deserialize)]
    struct Comparison {
        status: String,
    }
    let url = format!("{API}/compare/{installed_commit}...{}", release.commit);
    let comparison: Comparison = get_json(client, &url).await?;
    Ok(match comparison.status.as_str() {
        "ahead" => Relation::Newer,
        "identical" => Relation::Same,
        "behind" => Relation::Older,
        _ => Relation::Diverged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn release(tag: &str, commit: &str, published_at: &str, assets: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "target_commitish": commit,
            "draft": false,
            "prerelease": false,
            "published_at": published_at,
            "assets": assets.iter().map(|name| serde_json::json!({
                "name": name,
                "browser_download_url": format!("{DOWNLOADS}{tag}/{name}"),
            })).collect::<Vec<_>>(),
        })
    }

    fn releases(values: Vec<serde_json::Value>) -> Vec<GitHubRelease> {
        serde_json::from_value(serde_json::Value::Array(values)).unwrap()
    }

    #[test]
    fn test_fork_asset_only_for_apple_silicon_macs() {
        assert_eq!(fork_asset_name("macos", "aarch64"), Some("Zed-aarch64.dmg"));
        assert_eq!(fork_asset_name("macos", "x86_64"), None);
        assert_eq!(fork_asset_name("windows", "x86_64"), None);
        assert_eq!(fork_asset_name("linux", "x86_64"), None);
    }

    #[test]
    fn test_pick_newest_release_with_the_asset() {
        let picked = pick_release(
            releases(vec![
                release(
                    "fork-v1.26.2-bbbbbbbbbb",
                    COMMIT_B,
                    "2026-11-01T08:00:00Z",
                    &["Zed-x86_64.zip"],
                ),
                release(
                    "fork-v1.26.2-aaaaaaaaaa",
                    COMMIT_A,
                    "2026-11-01T07:00:00Z",
                    &["Zed-aarch64.dmg"],
                ),
                release(
                    "fork-v1.25.3-aaaaaaaaaa",
                    COMMIT_A,
                    "2026-10-01T07:00:00Z",
                    &["Zed-aarch64.dmg"],
                ),
            ]),
            "Zed-aarch64.dmg",
        )
        .unwrap();
        assert_eq!(
            picked.version,
            "1.26.2+aaaaaaaaaa".parse::<Version>().unwrap()
        );
        assert_eq!(picked.commit, COMMIT_A);
        assert_eq!(
            picked.url,
            format!("{DOWNLOADS}fork-v1.26.2-aaaaaaaaaa/Zed-aarch64.dmg")
        );
    }

    #[test]
    fn test_pick_skips_drafts_other_tags_and_foreign_downloads() {
        let mut draft = release(
            "fork-v1.26.2-aaaaaaaaaa",
            COMMIT_A,
            "2026-11-01T07:00:00Z",
            &["Zed-aarch64.dmg"],
        );
        draft["draft"] = true.into();
        let mut foreign = release(
            "fork-v1.26.2-aaaaaaaaaa",
            COMMIT_A,
            "2026-11-01T07:00:00Z",
            &["Zed-aarch64.dmg"],
        );
        foreign["assets"][0]["browser_download_url"] =
            "https://zed.dev/api/releases/stable/latest/Zed-aarch64.dmg".into();
        let upstream_tag = release(
            "v1.26.2",
            COMMIT_A,
            "2026-11-01T07:00:00Z",
            &["Zed-aarch64.dmg"],
        );
        assert_eq!(
            pick_release(
                releases(vec![draft, foreign, upstream_tag]),
                "Zed-aarch64.dmg"
            ),
            None
        );
    }

    #[gpui::test]
    async fn test_relation_by_version_and_commit() {
        let client = FakeHttpClient::create(|req| async move {
            let path = req.uri().path().to_string();
            let status = if path.ends_with(&format!("{COMMIT_A}...{COMMIT_B}")) {
                "ahead"
            } else {
                "diverged"
            };
            Ok(Response::builder()
                .status(200)
                .body(format!(r#"{{"status":"{status}"}}"#).into())
                .unwrap())
        });
        let release = ForkRelease {
            version: "1.26.2+bbbbbbbbbb".parse().unwrap(),
            commit: COMMIT_B.to_string(),
            url: String::new(),
        };
        let at = |v: &str| v.parse::<Version>().unwrap();

        assert_eq!(
            relation(&*client, &release, &at("1.25.3"), Some(COMMIT_A))
                .await
                .unwrap(),
            Relation::Newer
        );
        assert_eq!(
            relation(&*client, &release, &at("1.27.0"), Some(COMMIT_A))
                .await
                .unwrap(),
            Relation::Older
        );
        assert_eq!(
            relation(&*client, &release, &at("1.26.2"), Some(COMMIT_B))
                .await
                .unwrap(),
            Relation::Same
        );
        assert_eq!(
            relation(&*client, &release, &at("1.26.2"), Some(COMMIT_A))
                .await
                .unwrap(),
            Relation::Newer
        );
        assert_eq!(
            relation(&*client, &release, &at("1.26.2"), Some(&"c".repeat(40)))
                .await
                .unwrap(),
            Relation::Diverged
        );
    }
}
