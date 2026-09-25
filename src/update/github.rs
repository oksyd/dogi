use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use ssh_key::{PublicKey, SshSig};

use super::UpdateError;
use crate::network::NetworkPolicy;

const API_ROOT: &str = "https://api.github.com/repos/oksyd/dogi";
const RELEASE_ROOT: &str = "https://github.com/oksyd/dogi/releases";
const API_VERSION: &str = "2026-03-10";
const USER_AGENT: &str = concat!("dogi/", env!("CARGO_PKG_VERSION"));
const API_BODY_LIMIT: u64 = 1024 * 1024;
pub(super) const MAX_ARTIFACT_SIZE: u64 = 128 * 1024 * 1024;
const TRUSTED_TAG_SIGNERS: &str = include_str!("../../packaging/release-signers");

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReleaseArtifact {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReleaseCandidate {
    pub(super) version: Version,
    pub(super) artifact: ReleaseArtifact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReleaseAssetKind {
    Debian { architecture: &'static str },
    Portable { target: &'static str },
}

impl ReleaseAssetKind {
    pub(super) fn expected_name(&self, version: &Version) -> String {
        match self {
            Self::Debian { architecture } => format!("dogi_{version}_{architecture}.deb"),
            Self::Portable { target } => format!("dogi-{version}-{target}.tar.zst"),
        }
    }
}

pub(super) struct GitHubReleaseClient<'a> {
    network: &'a NetworkPolicy,
}

impl<'a> GitHubReleaseClient<'a> {
    pub(super) fn new(network: &'a NetworkPolicy) -> Self {
        Self { network }
    }

    pub(super) fn latest(
        &self,
        current: &Version,
        kind: &ReleaseAssetKind,
    ) -> Result<Option<ReleaseCandidate>, UpdateError> {
        let release: ApiRelease = self.get_json(&format!("{API_ROOT}/releases/latest"))?;
        let candidate = validate_release(current, kind, release)?;
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        self.verify_release_tag(&candidate.version)?;
        Ok(Some(candidate))
    }

    pub(super) fn download(
        &self,
        candidate: &ReleaseCandidate,
        directory: &Path,
    ) -> Result<PathBuf, UpdateError> {
        let directory = directory.join(candidate.version.to_string());
        fs::create_dir_all(&directory).map_err(|error| {
            UpdateError::storage(format!("could not create the update cache: {error}"))
        })?;
        set_private_directory_permissions(&directory)?;

        self.download_asset(&candidate.artifact, &directory)
    }

    fn download_asset(
        &self,
        artifact: &ReleaseArtifact,
        directory: &Path,
    ) -> Result<PathBuf, UpdateError> {
        let destination = directory.join(&artifact.name);
        if destination.is_file() && verify_file(&destination, artifact)? {
            return Ok(destination);
        }

        let (temporary_path, mut output) = create_temporary_file(directory)?;
        let result = self.download_into(artifact, &mut output);
        if let Err(error) = result {
            drop(output);
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
        output.sync_all().map_err(|error| {
            UpdateError::storage(format!("could not commit the downloaded update: {error}"))
        })?;
        drop(output);
        fs::rename(&temporary_path, &destination).map_err(|error| {
            let _ = fs::remove_file(&temporary_path);
            UpdateError::storage(format!("could not store the downloaded update: {error}"))
        })?;
        sync_directory(directory);
        Ok(destination)
    }

    fn download_into(
        &self,
        artifact: &ReleaseArtifact,
        output: &mut File,
    ) -> Result<(), UpdateError> {
        let mut response = self.request(&artifact.url)?.0.call().map_err(|error| {
            UpdateError::network(format!("could not download the update: {error}"))
        })?;
        if let Some(length) = response.body().content_length()
            && length != artifact.size
        {
            return Err(UpdateError::verification(
                "the update download size does not match the release",
            ));
        }

        let mut reader = response
            .body_mut()
            .with_config()
            .limit(artifact.size.saturating_add(1))
            .reader();
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).map_err(|error| {
                UpdateError::network(format!("could not read the update download: {error}"))
            })?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| UpdateError::verification("the update download is too large"))?;
            if total > artifact.size {
                return Err(UpdateError::verification(
                    "the update download is larger than the release asset",
                ));
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count]).map_err(|error| {
                UpdateError::storage(format!("could not write the update download: {error}"))
            })?;
        }
        if total != artifact.size {
            return Err(UpdateError::verification(
                "the update download is incomplete",
            ));
        }
        let digest = lowercase_hex(&hasher.finalize());
        if digest != artifact.sha256 {
            return Err(UpdateError::verification(
                "the downloaded update failed SHA-256 verification",
            ));
        }
        Ok(())
    }

    fn verify_release_tag(&self, version: &Version) -> Result<(), UpdateError> {
        let reference: ApiTagReference =
            self.get_json(&format!("{API_ROOT}/git/ref/tags/{version}"))?;
        if reference.reference != format!("refs/tags/{version}")
            || reference.object.kind != "tag"
            || !is_git_sha(&reference.object.sha)
        {
            return Err(UpdateError::verification(
                "the release does not reference an annotated tag",
            ));
        }
        let tag: ApiTag =
            self.get_json(&format!("{API_ROOT}/git/tags/{}", reference.object.sha))?;
        verify_tag(version, &reference.object.sha, &tag)
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, UpdateError> {
        self.request(url)?
            .0
            .call()
            .map_err(|error| {
                UpdateError::network(format!("GitHub update request failed: {error}"))
            })?
            .body_mut()
            .with_config()
            .limit(API_BODY_LIMIT)
            .read_json::<T>()
            .map_err(|error| {
                UpdateError::verification(format!(
                    "GitHub returned invalid update metadata: {error}"
                ))
            })
    }

    fn request(
        &self,
        url: &str,
    ) -> Result<(ureq::RequestBuilder<ureq::typestate::WithoutBody>, String), UpdateError> {
        let routed = self.network.agent_for(url).map_err(UpdateError::network)?;
        let request = routed
            .agent
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", USER_AGENT);
        Ok((request, routed.route))
    }
}

pub(crate) fn test_connection(network: &NetworkPolicy) -> Result<String, String> {
    let client = GitHubReleaseClient::new(network);
    let (request, route) = client
        .request(&format!("{API_ROOT}/releases/latest"))
        .map_err(|error| error.to_string())?;
    request
        .call()
        .map_err(|error| format!("GitHub connection test failed: {error}"))?;
    Ok(route)
}

fn validate_release(
    current: &Version,
    kind: &ReleaseAssetKind,
    release: ApiRelease,
) -> Result<Option<ReleaseCandidate>, UpdateError> {
    if release.draft || release.prerelease {
        return Err(UpdateError::verification(
            "GitHub returned a non-stable release as latest",
        ));
    }
    let version = Version::parse(&release.tag_name).map_err(|_| {
        UpdateError::verification("the latest Dogi release tag is not a semantic version")
    })?;
    if version <= *current {
        return Ok(None);
    }
    if !release.immutable {
        return Err(UpdateError::verification(
            "the latest release is not immutable, so Dogi will not install it automatically",
        ));
    }
    let expected_release_url = format!("{RELEASE_ROOT}/tag/{version}");
    if release.html_url != expected_release_url {
        return Err(UpdateError::verification(
            "the latest release URL is not trusted",
        ));
    }

    let expected_name = kind.expected_name(&version);
    let artifact =
        validated_api_asset(&release.assets, &version, &expected_name, MAX_ARTIFACT_SIZE)?;
    Ok(Some(ReleaseCandidate { version, artifact }))
}

fn validated_api_asset(
    assets: &[ApiReleaseAsset],
    version: &Version,
    expected_name: &str,
    maximum_size: u64,
) -> Result<ReleaseArtifact, UpdateError> {
    let mut matches = assets.iter().filter(|asset| asset.name == expected_name);
    let asset = matches.next().ok_or_else(|| {
        UpdateError::verification(format!(
            "release {version} does not contain {expected_name}"
        ))
    })?;
    if matches.next().is_some()
        || asset.state != "uploaded"
        || asset.size == 0
        || asset.size > maximum_size
    {
        return Err(UpdateError::verification(format!(
            "release asset {expected_name} has an invalid size or state"
        )));
    }
    let expected_url = format!("{RELEASE_ROOT}/download/{version}/{expected_name}");
    if asset.browser_download_url != expected_url {
        return Err(UpdateError::verification(format!(
            "release asset {expected_name} has an untrusted URL"
        )));
    }
    let sha256 = parse_sha256(&asset.digest).ok_or_else(|| {
        UpdateError::verification(format!(
            "release asset {expected_name} does not include a valid SHA-256 digest"
        ))
    })?;
    Ok(ReleaseArtifact {
        name: expected_name.to_owned(),
        url: asset.browser_download_url.clone(),
        size: asset.size,
        sha256,
    })
}

fn verify_tag(version: &Version, reference_sha: &str, tag: &ApiTag) -> Result<(), UpdateError> {
    if tag.sha != reference_sha
        || tag.tag != version.to_string()
        || tag.object.kind != "commit"
        || !is_git_sha(&tag.object.sha)
        || !tag.verification.verified
        || tag.verification.reason != "valid"
    {
        return Err(UpdateError::verification(
            "the release tag is not a verified signed tag",
        ));
    }
    let signature = tag
        .verification
        .signature
        .as_deref()
        .ok_or_else(|| UpdateError::verification("the release tag signature is missing"))?
        .parse::<SshSig>()
        .map_err(|_| UpdateError::verification("the release tag SSH signature is invalid"))?;
    let payload =
        tag.verification.payload.as_deref().ok_or_else(|| {
            UpdateError::verification("the release tag signed payload is missing")
        })?;
    let public_key = trusted_tag_key()?;
    public_key
        .verify("git", payload.as_bytes(), &signature)
        .map_err(|_| {
            UpdateError::verification("the release tag was not signed by the Dogi release key")
        })?;

    let expected_prefix = format!("object {}\ntype commit\ntag {}\n", tag.object.sha, version);
    if !payload.starts_with(&expected_prefix) {
        return Err(UpdateError::verification(
            "the signed release tag payload does not match the release",
        ));
    }
    Ok(())
}

fn trusted_tag_key() -> Result<PublicKey, UpdateError> {
    let mut signers = TRUSTED_TAG_SIGNERS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let fields = signers
        .next()
        .ok_or_else(|| UpdateError::internal("the embedded Dogi release signer is missing"))?
        .split_whitespace()
        .collect::<Vec<_>>();
    if signers.next().is_some()
        || fields.len() != 4
        || fields[0] != "*"
        || fields[1] != "namespaces=\"git\""
        || fields[2] != "ssh-ed25519"
    {
        return Err(UpdateError::internal(
            "the embedded Dogi release signer is invalid",
        ));
    }
    format!("{} {} dogi-release", fields[2], fields[3])
        .parse::<PublicKey>()
        .map_err(|_| UpdateError::internal("the embedded Dogi tag key is invalid"))
}

fn parse_sha256(value: &str) -> Option<String> {
    let digest = value.strip_prefix("sha256:")?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

fn is_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn verify_file(path: &Path, artifact: &ReleaseArtifact) -> Result<bool, UpdateError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path).map_err(|error| {
        UpdateError::storage(format!("could not open the cached update: {error}"))
    })?;
    let metadata = file.metadata().map_err(|error| {
        UpdateError::storage(format!("could not inspect the cached update: {error}"))
    })?;
    if !metadata.is_file() || metadata.len() != artifact.size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| {
            UpdateError::storage(format!("could not verify the cached update: {error}"))
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(lowercase_hex(&hasher.finalize()) == artifact.sha256)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    output
}

fn create_temporary_file(directory: &Path) -> Result<(PathBuf, File), UpdateError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0_u8..16 {
        let path = directory.join(format!(
            ".download-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(UpdateError::storage(format!(
                    "could not create the update download: {error}"
                )));
            }
        }
    }
    Err(UpdateError::storage(
        "could not allocate a unique update download",
    ))
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        UpdateError::storage(format!("could not protect the update cache: {error}"))
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    immutable: bool,
    assets: Vec<ApiReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiReleaseAsset {
    name: String,
    browser_download_url: String,
    state: String,
    size: u64,
    digest: String,
}

#[derive(Debug, Deserialize)]
struct ApiTagReference {
    #[serde(rename = "ref")]
    reference: String,
    object: ApiGitObject,
}

#[derive(Debug, Deserialize)]
struct ApiTag {
    sha: String,
    tag: String,
    object: ApiGitObject,
    verification: ApiVerification,
}

#[derive(Debug, Deserialize)]
struct ApiGitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct ApiVerification {
    verified: bool,
    reason: String,
    signature: Option<String>,
    payload: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAg9ZhtgtPQFFhAUrD0tidVQ/KRvL\nSFAevlJZaDoL6YK7kAAAADZ2l0AAAAAAAAAAZzaGE1MTIAAABTAAAAC3NzaC1lZDI1NTE5\nAAAAQFu/v4xW2z4AkNAVR34w75nS6SSOKi/juyf8ck0CiAXOZVW7csZXQIOOMosHXenyZJ\nvxlYAiyUWCL7mXi0/M3QQ=\n-----END SSH SIGNATURE-----\n";
    const TAG_PAYLOAD: &str = "object b1936c1e30dbf4b9cc2797b8887d06f2ace7b655\ntype commit\ntag 0.1.3\ntagger forust <150141651+forust@users.noreply.github.com> 1785939881 +0800\n\nchore: release 0.1.3\n";

    #[test]
    fn release_validation_accepts_the_original_assets_without_extra_signatures() {
        let candidate = validate_release(
            &Version::new(0, 1, 2),
            &ReleaseAssetKind::Debian {
                architecture: "amd64",
            },
            release_fixture(true),
        )
        .unwrap()
        .unwrap();

        assert_eq!(candidate.version, Version::new(0, 1, 3));
        assert_eq!(candidate.artifact.name, "dogi_0.1.3_amd64.deb");
        assert_eq!(candidate.artifact.sha256, "a".repeat(64));
    }

    #[test]
    fn current_and_older_releases_do_not_prepare_an_update() {
        assert!(
            validate_release(
                &Version::new(0, 1, 3),
                &ReleaseAssetKind::Debian {
                    architecture: "amd64",
                },
                release_fixture(true),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn mutable_releases_are_never_accepted_for_installation() {
        let error = validate_release(
            &Version::new(0, 1, 2),
            &ReleaseAssetKind::Debian {
                architecture: "amd64",
            },
            release_fixture(false),
        )
        .unwrap_err();

        assert!(error.to_string().contains("not immutable"));
    }

    #[test]
    fn pinned_release_key_verifies_the_real_signed_tag_fixture() {
        let tag = ApiTag {
            sha: "8bc4b909d3e6bfcfe58fc1246eafda1e985c42e9".to_owned(),
            tag: "0.1.3".to_owned(),
            object: ApiGitObject {
                sha: "b1936c1e30dbf4b9cc2797b8887d06f2ace7b655".to_owned(),
                kind: "commit".to_owned(),
            },
            verification: ApiVerification {
                verified: true,
                reason: "valid".to_owned(),
                signature: Some(TAG_SIGNATURE.to_owned()),
                payload: Some(TAG_PAYLOAD.to_owned()),
            },
        };

        verify_tag(
            &Version::new(0, 1, 3),
            "8bc4b909d3e6bfcfe58fc1246eafda1e985c42e9",
            &tag,
        )
        .unwrap();
    }

    #[test]
    fn signed_tag_payload_cannot_be_relabelled() {
        let tag = ApiTag {
            sha: "8bc4b909d3e6bfcfe58fc1246eafda1e985c42e9".to_owned(),
            tag: "0.1.4".to_owned(),
            object: ApiGitObject {
                sha: "b1936c1e30dbf4b9cc2797b8887d06f2ace7b655".to_owned(),
                kind: "commit".to_owned(),
            },
            verification: ApiVerification {
                verified: true,
                reason: "valid".to_owned(),
                signature: Some(TAG_SIGNATURE.to_owned()),
                payload: Some(TAG_PAYLOAD.to_owned()),
            },
        };

        assert!(
            verify_tag(
                &Version::new(0, 1, 4),
                "8bc4b909d3e6bfcfe58fc1246eafda1e985c42e9",
                &tag,
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_release_assets_are_rejected() {
        let mut release = release_fixture(true);
        release.assets.push(ApiReleaseAsset {
            name: release.assets[0].name.clone(),
            browser_download_url: release.assets[0].browser_download_url.clone(),
            state: "uploaded".to_owned(),
            size: 1024,
            digest: format!("sha256:{}", "a".repeat(64)),
        });
        assert!(
            validate_release(
                &Version::new(0, 1, 2),
                &ReleaseAssetKind::Debian {
                    architecture: "amd64"
                },
                release,
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_release_asset_metadata_is_rejected() {
        let mutations: [fn(&mut ApiReleaseAsset); 5] = [
            |asset| asset.digest.clear(),
            |asset| asset.size = 0,
            |asset| asset.size = MAX_ARTIFACT_SIZE + 1,
            |asset| asset.state = "new".to_owned(),
            |asset| asset.browser_download_url = "https://example.com/dogi.deb".to_owned(),
        ];
        for mutate in mutations {
            let mut release = release_fixture(true);
            mutate(&mut release.assets[0]);
            assert!(
                validate_release(
                    &Version::new(0, 1, 2),
                    &ReleaseAssetKind::Debian {
                        architecture: "amd64"
                    },
                    release,
                )
                .is_err()
            );
        }
    }

    fn release_fixture(immutable: bool) -> ApiRelease {
        ApiRelease {
            tag_name: "0.1.3".to_owned(),
            html_url: "https://github.com/oksyd/dogi/releases/tag/0.1.3".to_owned(),
            draft: false,
            prerelease: false,
            immutable,
            assets: vec![ApiReleaseAsset {
                name: "dogi_0.1.3_amd64.deb".to_owned(),
                browser_download_url:
                    "https://github.com/oksyd/dogi/releases/download/0.1.3/dogi_0.1.3_amd64.deb"
                        .to_owned(),
                state: "uploaded".to_owned(),
                size: 1024,
                digest: format!("sha256:{}", "a".repeat(64)),
            }],
        }
    }
}
