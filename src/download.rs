use fs2::FileExt;
use reqwest::{
    blocking::{Client, Response},
    header::LOCATION,
    redirect::Policy,
    Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Duration,
};
use tempfile::NamedTempFile;
use zip::ZipArchive;

const MAX_ARCHIVE_BYTES: u64 = 150 * 1024 * 1024;
const MAX_EXE_BYTES: u64 = 200 * 1024 * 1024;
const EXE_SHA256: &str = "719f689b34f47be8ca105ce8484948474dafde0e106bab599e4a89326070c3d0";
const UPDATE_CHECK_URL: &str = "https://www.sumatrapdfreader.org/update-check-rel.txt";
const MAX_UPDATE_METADATA_BYTES: u64 = 128 * 1024;
const EXPECTED_SIGNER_NAME: &str = "Krzysztof Kowalczyk";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DownloadStatus {
    pub ready: bool,
    pub downloading: bool,
    pub version: String,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub path: String,
}

pub struct SumatraManager {
    directory: PathBuf,
    downloading: AtomicBool,
}

#[derive(Deserialize)]
struct ReleaseLock {
    version: String,
    binary: BinaryLock,
}

#[derive(Deserialize)]
struct BinaryLock {
    url: String,
    sha256: String,
    member: String,
}

#[derive(Clone, Debug)]
struct ReleaseTarget {
    version: String,
    url: String,
    archive_sha256: Option<String>,
    exe_sha256: Option<String>,
    member: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstalledRelease {
    version: String,
    exe_sha256: String,
    publisher: String,
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
struct AuthenticodeResult {
    status: String,
    subject: String,
    product_version: String,
}

static RELEASE: OnceLock<ReleaseLock> = OnceLock::new();

fn release() -> &'static ReleaseLock {
    RELEASE.get_or_init(|| {
        serde_json::from_str(include_str!("../third_party/sumatra.lock.json"))
            .expect("embedded Sumatra release lock must be valid")
    })
}

impl SumatraManager {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            downloading: AtomicBool::new(false),
        }
    }

    pub fn status(&self) -> DownloadStatus {
        let exe_path = self.exe_path();
        let current_version = self.installed_release().map(|installed| installed.version);
        self.status_with(current_version, None, false, exe_path)
    }

    pub fn check_update(&self) -> Result<DownloadStatus, String> {
        let client = official_client()?;
        let current_version = self.installed_release().map(|installed| installed.version);
        let query_version = current_version.as_deref().unwrap_or(&release().version);
        let latest_version = fetch_latest_stable_version(&client, query_version)?;
        let update_available = current_version.as_deref().map_or(true, |current| {
            compare_versions(&latest_version, current).is_gt()
        });
        Ok(self.status_with(
            current_version,
            Some(latest_version),
            update_available,
            self.exe_path(),
        ))
    }

    pub fn download(&self) -> Result<DownloadStatus, String> {
        let checked = self.check_update()?;
        let Some(latest_version) = checked.latest_version.clone() else {
            return Err("Sumatra stable version metadata is missing".into());
        };
        if !checked.update_available {
            return Ok(checked);
        }

        let target = release_target(&latest_version)?;
        if target.exe_sha256.is_none() && !cfg!(windows) {
            return Err("动态 Sumatra 更新的签名验证仅支持 Windows".into());
        }

        self.download_target_with(&target, |archive, spec| {
            let client = official_client()?;
            let mut response = get_official_response(&client, &spec.url)?;
            if response
                .content_length()
                .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
            {
                return Err("Sumatra archive exceeds the 150 MiB limit".into());
            }
            receive_archive(
                &mut response,
                archive,
                spec.archive_sha256.as_deref(),
                MAX_ARCHIVE_BYTES,
            )
        })?;

        let current_version = self.installed_release().map(|installed| installed.version);
        Ok(self.status_with(
            current_version,
            Some(latest_version),
            false,
            self.exe_path(),
        ))
    }

    pub fn exe_path(&self) -> PathBuf {
        self.directory.join("SumatraPDF.exe")
    }

    fn installed_release(&self) -> Option<InstalledRelease> {
        let exe_path = self.exe_path();
        if let Ok(bytes) = fs::read(self.installed_release_path()) {
            if let Ok(installed) = serde_json::from_slice::<InstalledRelease>(&bytes) {
                if is_stable_version(&installed.version)
                    && is_sha256(&installed.exe_sha256)
                    && installed.publisher == EXPECTED_SIGNER_NAME
                    && executable_matches(&exe_path, &installed.exe_sha256)
                {
                    return Some(installed);
                }
            }
        }
        executable_matches(&exe_path, EXE_SHA256).then(|| InstalledRelease {
            version: release().version.clone(),
            exe_sha256: EXE_SHA256.into(),
            publisher: EXPECTED_SIGNER_NAME.into(),
        })
    }

    fn installed_release_path(&self) -> PathBuf {
        self.directory.join(".sumatra-installed.json")
    }

    fn status_with(
        &self,
        current_version: Option<String>,
        latest_version: Option<String>,
        update_available: bool,
        exe_path: PathBuf,
    ) -> DownloadStatus {
        let version = current_version
            .clone()
            .unwrap_or_else(|| release().version.clone());
        DownloadStatus {
            ready: current_version.is_some(),
            downloading: self.downloading.load(Ordering::Acquire),
            version,
            current_version: current_version.unwrap_or_default(),
            latest_version,
            update_available,
            path: exe_path.to_string_lossy().into_owned(),
        }
    }

    fn download_target_with(
        &self,
        target: &ReleaseTarget,
        acquire: impl FnOnce(&mut NamedTempFile, &ReleaseTarget) -> Result<(), String>,
    ) -> Result<DownloadStatus, String> {
        let guard = DownloadGuard::start(&self.downloading)?;
        let result = (|| {
            fs::create_dir_all(&self.directory).map_err(display_error)?;
            let lock_file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.directory.join(".sumatra-download.lock"))
                .map_err(display_error)?;
            lock_file
                .try_lock_exclusive()
                .map_err(|_| "Sumatra download is already in progress".to_string())?;

            let mut archive = NamedTempFile::new_in(&self.directory).map_err(display_error)?;
            acquire(&mut archive, target)?;
            archive.as_file().sync_all().map_err(display_error)?;
            install_target_archive(archive.path(), &self.directory, target)
        })();
        drop(guard);
        result?;
        Ok(self.status())
    }
}

fn official_client() -> Result<Client, String> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent("print-bridge/0.1")
        .build()
        .map_err(display_error)
}

fn fetch_latest_stable_version(client: &Client, current_version: &str) -> Result<String, String> {
    let mut url = Url::parse(UPDATE_CHECK_URL).map_err(display_error)?;
    url.query_pairs_mut()
        .append_pair("v", current_version)
        .append_pair("os", "windows")
        .append_pair("64bit", "yes")
        .append_pair("arm", "no")
        .append_pair("lang", "en")
        .append_pair("simd", "sse2")
        .append_pair("withPromo", "")
        .append_pair("force", "");
    let response = get_official_response(client, url.as_str())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_UPDATE_METADATA_BYTES)
    {
        return Err("Sumatra update metadata exceeds the 128 KiB limit".into());
    }
    let mut metadata = Vec::new();
    response
        .take(MAX_UPDATE_METADATA_BYTES + 1)
        .read_to_end(&mut metadata)
        .map_err(display_error)?;
    if metadata.len() as u64 > MAX_UPDATE_METADATA_BYTES {
        return Err("Sumatra update metadata exceeds the 128 KiB limit".into());
    }
    let metadata = std::str::from_utf8(&metadata)
        .map_err(|_| "Sumatra update metadata is not UTF-8".to_string())?;
    parse_latest_stable_version(metadata)
}

fn parse_latest_stable_version(metadata: &str) -> Result<String, String> {
    let metadata = metadata.trim_start_matches('\u{feff}');
    let mut in_sumatra_section = false;
    let mut latest = None;
    for line in metadata.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_sumatra_section = line.eq_ignore_ascii_case("[SumatraPDF]");
            continue;
        }
        if !in_sumatra_section {
            continue;
        }
        let Some((key, value)) = split_metadata_pair(line) else {
            continue;
        };
        if key.eq_ignore_ascii_case("Latest") {
            let value = value.trim().to_owned();
            if latest.replace(value).is_some() {
                return Err("Sumatra update metadata contains multiple Latest values".into());
            }
        }
    }
    let version =
        latest.ok_or_else(|| "Sumatra update metadata has no Latest version".to_string())?;
    if !is_stable_version(&version) {
        return Err("Sumatra update metadata contains an invalid stable version".into());
    }
    Ok(version)
}

fn split_metadata_pair(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once(':') {
        return Some((key.trim(), value.trim()));
    }
    let split_at = line.find(char::is_whitespace)?;
    Some((&line[..split_at], line[split_at..].trim()))
}

fn is_stable_version(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part.parse::<u32>().is_ok()
        })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let left: Vec<u32> = left
        .split('.')
        .map(|part| part.parse().unwrap_or_default())
        .collect();
    let right: Vec<u32> = right
        .split('.')
        .map(|part| part.parse().unwrap_or_default())
        .collect();
    let part_count = left.len().max(right.len());
    (0..part_count)
        .map(|index| {
            left.get(index)
                .copied()
                .unwrap_or_default()
                .cmp(&right.get(index).copied().unwrap_or_default())
        })
        .find(|ordering| !ordering.is_eq())
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn release_target(version: &str) -> Result<ReleaseTarget, String> {
    if !is_stable_version(version) {
        return Err("Sumatra version is not a valid stable release".into());
    }
    if version == release().version {
        return Ok(ReleaseTarget {
            version: release().version.clone(),
            url: release().binary.url.clone(),
            archive_sha256: Some(release().binary.sha256.clone()),
            exe_sha256: Some(EXE_SHA256.into()),
            member: release().binary.member.clone(),
        });
    }
    Ok(ReleaseTarget {
        version: version.to_owned(),
        url: format!(
            "https://files.sumatrapdfreader.org/software/sumatrapdf/rel/{version}/SumatraPDF-{version}-64.zip"
        ),
        archive_sha256: None,
        exe_sha256: None,
        member: format!("SumatraPDF-{version}-64.exe"),
    })
}

struct DownloadGuard<'a>(&'a AtomicBool);

impl<'a> DownloadGuard<'a> {
    fn start(flag: &'a AtomicBool) -> Result<Self, String> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Sumatra download is already in progress".to_string())?;
        Ok(Self(flag))
    }
}

impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn get_official_response(client: &Client, initial_url: &str) -> Result<Response, String> {
    let mut url = Url::parse(initial_url).map_err(display_error)?;
    for _ in 0..=MAX_REDIRECTS {
        validate_official_url(&url)?;
        let response = client.get(url.clone()).send().map_err(display_error)?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| "Sumatra redirect did not include a location".to_string())?
                .to_str()
                .map_err(display_error)?;
            url = url.join(location).map_err(display_error)?;
            validate_official_url(&url)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "Sumatra download returned HTTP {}",
                response.status()
            ));
        }
        return Ok(response);
    }
    Err("Sumatra download exceeded the redirect limit".into())
}

fn validate_official_url(url: &Url) -> Result<(), String> {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let official_host = host == "sumatrapdfreader.org" || host.ends_with(".sumatrapdfreader.org");
    if url.scheme() != "https"
        || !official_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err("Sumatra download URL is outside the official HTTPS domain".into());
    }
    Ok(())
}

fn receive_archive<R: Read>(
    reader: &mut R,
    archive: &mut NamedTempFile,
    expected_sha256: Option<&str>,
    max_bytes: u64,
) -> Result<(), String> {
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(display_error)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| "Sumatra archive size overflow".to_string())?;
        if received > max_bytes {
            return Err("Sumatra archive exceeds the 150 MiB limit".into());
        }
        archive.write_all(&buffer[..count]).map_err(display_error)?;
        digest.update(&buffer[..count]);
    }
    if expected_sha256.is_some_and(|expected| format!("{:x}", digest.finalize()) != expected) {
        return Err("Sumatra archive SHA-256 does not match the pinned release".into());
    }
    archive.flush().map_err(display_error)
}

fn install_target_archive(
    archive_path: &Path,
    directory: &Path,
    target: &ReleaseTarget,
) -> Result<(), String> {
    match target.exe_sha256.as_deref() {
        Some(expected_sha256) => install_archive(
            archive_path,
            directory,
            &target.member,
            expected_sha256,
            MAX_EXE_BYTES,
        ),
        None => install_dynamic_archive(archive_path, directory, target),
    }
}

fn install_archive(
    archive_path: &Path,
    directory: &Path,
    member: &str,
    expected_sha256: &str,
    max_exe_bytes: u64,
) -> Result<(), String> {
    install_archive_inner(
        archive_path,
        directory,
        member,
        Some(expected_sha256),
        None,
        None,
        max_exe_bytes,
    )
}

fn install_dynamic_archive(
    archive_path: &Path,
    directory: &Path,
    target: &ReleaseTarget,
) -> Result<(), String> {
    install_archive_inner(
        archive_path,
        directory,
        &target.member,
        None,
        Some(EXPECTED_SIGNER_NAME),
        Some(&target.version),
        MAX_EXE_BYTES,
    )
}

fn install_archive_inner(
    archive_path: &Path,
    directory: &Path,
    member: &str,
    expected_sha256: Option<&str>,
    expected_publisher: Option<&str>,
    install_version: Option<&str>,
    max_exe_bytes: u64,
) -> Result<(), String> {
    let file = File::open(archive_path).map_err(display_error)?;
    let mut zip = ZipArchive::new(file).map_err(display_error)?;
    let matching_entries = zip.file_names().filter(|name| *name == member).count();
    if matching_entries != 1 {
        return Err("Sumatra archive is missing the pinned executable".into());
    }
    let mut entry = zip.by_name(member).map_err(display_error)?;
    if entry.is_dir() || entry.size() > max_exe_bytes {
        return Err("Sumatra executable is missing or exceeds the 200 MiB limit".into());
    }

    let mut staged_exe = NamedTempFile::new_in(directory).map_err(display_error)?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut signature = [0u8; 2];
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = entry.read(&mut buffer).map_err(display_error)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| "Sumatra executable size overflow".to_string())?;
        if size > max_exe_bytes {
            return Err("Sumatra executable exceeds the 200 MiB limit".into());
        }
        for (index, byte) in buffer[..count].iter().enumerate() {
            if (size - count as u64 + index as u64) < signature.len() as u64 {
                signature[(size - count as u64 + index as u64) as usize] = *byte;
            }
        }
        staged_exe
            .write_all(&buffer[..count])
            .map_err(display_error)?;
        digest.update(&buffer[..count]);
    }
    if size < signature.len() as u64 || signature != *b"MZ" {
        return Err("Sumatra archive member is not a Windows executable".into());
    }
    let actual_sha256 = format!("{:x}", digest.finalize());
    if expected_sha256.is_some_and(|expected| expected != actual_sha256) {
        return Err("Sumatra executable SHA-256 does not match the pinned release".into());
    }
    staged_exe.as_file().sync_all().map_err(display_error)?;
    if let Some(expected_publisher) = expected_publisher {
        let version =
            install_version.ok_or_else(|| "Sumatra update version is missing".to_string())?;
        let subject = verify_authenticode_publisher(staged_exe.path(), version)?;
        if !publisher_subject_matches(&subject, expected_publisher) {
            return Err("Sumatra executable is signed by an unexpected publisher".into());
        }
    }

    let exe_path = directory.join("SumatraPDF.exe");
    if let Some(version) = install_version {
        persist_dynamic_executable(
            staged_exe,
            &exe_path,
            &directory.join(".sumatra-installed.json"),
            InstalledRelease {
                version: version.to_owned(),
                exe_sha256: actual_sha256,
                publisher: EXPECTED_SIGNER_NAME.to_owned(),
            },
        )?;
    } else {
        staged_exe
            .persist(exe_path)
            .map_err(|error| display_error(error.error))?;
    }
    Ok(())
}

fn publisher_subject_matches(subject: &str, expected_name: &str) -> bool {
    let mut common_name_matches = false;
    let mut organization_matches = false;
    for attribute in subject.split(',') {
        let Some((name, value)) = attribute.trim().split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("CN") && value.trim() == expected_name {
            common_name_matches = true;
        }
        if name.trim().eq_ignore_ascii_case("O") && value.trim() == expected_name {
            organization_matches = true;
        }
    }
    common_name_matches && organization_matches
}

fn persist_dynamic_executable(
    staged_exe: NamedTempFile,
    exe_path: &Path,
    installed_release_path: &Path,
    installed_release: InstalledRelease,
) -> Result<(), String> {
    let mut backup = if exe_path.exists() {
        let backup = NamedTempFile::new_in(
            exe_path
                .parent()
                .ok_or_else(|| "Sumatra directory is invalid".to_string())?,
        )
        .map_err(display_error)?;
        fs::copy(exe_path, backup.path()).map_err(display_error)?;
        backup.as_file().sync_all().map_err(display_error)?;
        Some(backup)
    } else {
        None
    };
    staged_exe
        .persist(exe_path)
        .map_err(|error| display_error(error.error))?;

    let state_result = write_installed_release(installed_release_path, &installed_release);
    if let Err(error) = state_result {
        let rollback_result = match backup.take() {
            Some(backup) => backup
                .persist(exe_path)
                .map(|_| ())
                .map_err(|error| display_error(error.error)),
            None => fs::remove_file(exe_path).map_err(display_error),
        };
        return match rollback_result {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; failed to restore the previous Sumatra executable: {rollback_error}"
            )),
        };
    }
    Ok(())
}

fn write_installed_release(
    path: &Path,
    installed_release: &InstalledRelease,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Sumatra directory is invalid".to_string())?;
    let mut temp = NamedTempFile::new_in(parent).map_err(display_error)?;
    serde_json::to_writer(temp.as_file_mut(), installed_release).map_err(display_error)?;
    temp.as_file().sync_all().map_err(display_error)?;
    temp.persist(path)
        .map_err(|error| display_error(error.error))?;
    Ok(())
}

#[cfg(windows)]
fn verify_authenticode_publisher(path: &Path, expected_version: &str) -> Result<String, String> {
    use std::{os::windows::process::CommandExt, process::Command};

    let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
        "Windows SystemRoot is unavailable for signature verification".to_string()
    })?;
    let powershell = PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let script = r#"$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $path = $env:SUMATRA_SIGNATURE_TARGET; $sig = Get-AuthenticodeSignature -LiteralPath $path; $version = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($path).ProductVersion; $result = [ordered]@{ status = [string]$sig.Status; subject = [string]$sig.SignerCertificate.Subject; product_version = [string]$version }; ConvertTo-Json -InputObject $result -Compress"#;
    let output = Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("SUMATRA_SIGNATURE_TARGET", path)
        .creation_flags(0x08000000)
        .output()
        .map_err(|_| "could not run Windows Authenticode verification".to_string())?;
    if !output.status.success() {
        return Err("Windows Authenticode verification failed".into());
    }
    let result: AuthenticodeResult = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Windows Authenticode verification returned invalid data".to_string())?;
    if !result.status.eq_ignore_ascii_case("Valid") || result.subject.is_empty() {
        return Err("Sumatra executable does not have a valid Authenticode signature".into());
    }
    if !is_stable_version(&result.product_version)
        || !compare_versions(&result.product_version, expected_version).is_eq()
    {
        return Err("Sumatra executable version does not match the requested update".into());
    }
    Ok(result.subject)
}

#[cfg(not(windows))]
fn verify_authenticode_publisher(_path: &Path, _expected_version: &str) -> Result<String, String> {
    Err("动态 Sumatra 更新的签名验证仅支持 Windows".into())
}

fn executable_matches(path: &Path, expected_sha256: &str) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut signature = [0u8; 2];
    if file.read_exact(&mut signature).is_err() || signature != *b"MZ" {
        return false;
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    hash_reader(&mut file)
        .map(|hash| hash == expected_sha256)
        .unwrap_or(false)
}

fn hash_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{mpsc, Arc},
        thread,
    };
    use zip::write::SimpleFileOptions;

    const TEST_MEMBER: &str = "SumatraPDF-3.6.1-64.exe";

    fn make_archive(member_name: &str, bytes: &[u8]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file(member_name, SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn install_test_archive(
        dir: &Path,
        archive_bytes: &[u8],
        member: &str,
        expected_sha256: &str,
        max_exe_bytes: u64,
    ) -> Result<(), String> {
        fs::create_dir_all(dir).unwrap();
        let archive_path = dir.join("test.zip");
        fs::write(&archive_path, archive_bytes).unwrap();
        install_archive(&archive_path, dir, member, expected_sha256, max_exe_bytes)
    }

    #[test]
    fn status_requires_the_pinned_executable_hash_and_mz_header() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SumatraManager::new(dir.path().to_path_buf());
        assert!(!manager.status().ready);

        fs::write(manager.exe_path(), b"not an executable").unwrap();
        assert!(!manager.status().ready);

        fs::write(manager.exe_path(), b"MZdifferent executable").unwrap();
        assert!(!manager.status().ready);
    }

    #[test]
    fn archive_hash_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SumatraManager::new(dir.path().to_path_buf());
        let bytes = make_archive(TEST_MEMBER, b"MZfixture");
        let target = release_target("3.6.1").unwrap();
        let error = manager
            .download_target_with(&target, |archive, spec| {
                receive_archive(
                    &mut Cursor::new(bytes),
                    archive,
                    Some(spec.archive_sha256.as_deref().unwrap()),
                    MAX_ARCHIVE_BYTES,
                )
            })
            .unwrap_err();
        assert!(error.contains("archive SHA-256"));
        assert!(!manager.exe_path().exists());
    }

    #[test]
    fn oversized_archive_is_rejected_while_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = NamedTempFile::new_in(dir.path()).unwrap();
        let error = receive_archive(
            &mut Cursor::new(b"archive bytes"),
            &mut archive,
            Some("unused"),
            4,
        )
        .unwrap_err();
        assert!(error.contains("archive exceeds the 150 MiB limit"));
    }

    #[test]
    fn missing_executable_member_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_archive("some-other-file.exe", b"MZfixture");
        let error = install_test_archive(dir.path(), &bytes, TEST_MEMBER, "unused", MAX_EXE_BYTES)
            .unwrap_err();
        assert!(error.contains("missing the pinned executable"));
        assert!(!dir.path().join("SumatraPDF.exe").exists());
    }

    #[test]
    fn oversized_executable_member_is_rejected_before_extracting() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_archive(TEST_MEMBER, b"MZ12345");
        let expected = format!("{:x}", Sha256::digest(b"MZ12345"));
        let error =
            install_test_archive(dir.path(), &bytes, TEST_MEMBER, &expected, 6).unwrap_err();
        assert!(error.contains("exceeds the 200 MiB limit"));
        assert!(!dir.path().join("SumatraPDF.exe").exists());
    }

    #[test]
    fn pseudo_executable_member_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_archive(TEST_MEMBER, b"not an exe");
        let expected = format!("{:x}", Sha256::digest(b"not an exe"));
        let error = install_test_archive(dir.path(), &bytes, TEST_MEMBER, &expected, MAX_EXE_BYTES)
            .unwrap_err();
        assert!(error.contains("not a Windows executable"));
        assert!(!dir.path().join("SumatraPDF.exe").exists());
    }

    #[test]
    fn executable_hash_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_archive(TEST_MEMBER, b"MZfixture");
        let error =
            install_test_archive(dir.path(), &bytes, TEST_MEMBER, "wrong-hash", MAX_EXE_BYTES)
                .unwrap_err();
        assert!(error.contains("executable SHA-256"));
        assert!(!dir.path().join("SumatraPDF.exe").exists());
    }

    #[test]
    fn verified_executable_installs_and_replaces_without_running() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = b"MZverified fixture";
        let expected = format!("{:x}", Sha256::digest(fixture));
        let bytes = make_archive(TEST_MEMBER, fixture);
        fs::write(dir.path().join("SumatraPDF.exe"), b"old contents").unwrap();
        install_test_archive(dir.path(), &bytes, TEST_MEMBER, &expected, MAX_EXE_BYTES).unwrap();
        assert_eq!(
            fs::read(dir.path().join("SumatraPDF.exe")).unwrap(),
            fixture
        );
        assert!(executable_matches(
            &dir.path().join("SumatraPDF.exe"),
            &expected
        ));
    }

    #[test]
    fn failed_download_preserves_an_existing_executable() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SumatraManager::new(dir.path().to_path_buf());
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(manager.exe_path(), b"existing executable contents").unwrap();
        let target = release_target("3.6.1").unwrap();
        let error = manager
            .download_target_with(&target, |archive, spec| {
                receive_archive(
                    &mut Cursor::new(b"bad archive"),
                    archive,
                    Some(spec.archive_sha256.as_deref().unwrap()),
                    MAX_ARCHIVE_BYTES,
                )
            })
            .unwrap_err();
        assert!(error.contains("archive SHA-256"));
        assert_eq!(
            fs::read(manager.exe_path()).unwrap(),
            b"existing executable contents".to_vec()
        );
    }

    #[test]
    fn concurrent_download_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SumatraManager::new(dir.path().to_path_buf()));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_manager = Arc::clone(&manager);
        let target = release_target("3.6.1").unwrap();
        let worker = thread::spawn(move || {
            worker_manager.download_target_with(&target, |_, _| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err("test download stopped".into())
            })
        });
        started_rx.recv().unwrap();
        let target = release_target("3.6.1").unwrap();
        let error = manager
            .download_target_with(&target, |_, _| panic!("second download must not start"))
            .unwrap_err();
        assert!(error.contains("already in progress"));
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(!manager.status().downloading);
    }

    #[test]
    fn manager_rejects_redirect_urls_outside_official_https_domain() {
        let external =
            Url::parse("https://sumatrapdfreader.org.attacker.example/file.zip").unwrap();
        let http = Url::parse("http://www.sumatrapdfreader.org/file.zip").unwrap();
        let official = Url::parse("https://downloads.sumatrapdfreader.org/file.zip").unwrap();
        assert!(validate_official_url(&external).is_err());
        assert!(validate_official_url(&http).is_err());
        assert!(validate_official_url(&official).is_ok());
    }

    #[test]
    fn dynamic_release_target_uses_only_official_x64_zip_path() {
        let target = release_target("3.7.0").unwrap();
        let url = Url::parse(&target.url).unwrap();
        assert!(validate_official_url(&url).is_ok());
        assert_eq!(
            url.path(),
            "/software/sumatrapdf/rel/3.7.0/SumatraPDF-3.7.0-64.zip"
        );
        assert_eq!(target.member, "SumatraPDF-3.7.0-64.exe");
        assert_eq!(target.archive_sha256, None);
        assert_eq!(target.exe_sha256, None);
    }

    #[test]
    fn parses_official_stable_update_metadata() {
        let metadata = "\u{feff}[SumatraPDF]\nLatest 3.7.0\n[Promo]\nName = Test\n";
        assert_eq!(parse_latest_stable_version(metadata).unwrap(), "3.7.0");
        assert_eq!(
            parse_latest_stable_version("[SumatraPDF]\nLatest: 3.6.1\n").unwrap(),
            "3.6.1"
        );
    }

    #[test]
    fn rejects_missing_duplicate_or_non_stable_versions() {
        assert!(parse_latest_stable_version("[SumatraPDF]\n").is_err());
        assert!(parse_latest_stable_version("[SumatraPDF]\nLatest 3.6.1\nLatest 3.7.0\n").is_err());
        assert!(parse_latest_stable_version("[SumatraPDF]\nLatest 3.7.0-rc.1\n").is_err());
        assert!(release_target("../3.7.0").is_err());
    }

    #[test]
    fn stable_versions_compare_by_numeric_parts() {
        assert!(compare_versions("3.10.0", "3.9.9").is_gt());
        assert!(compare_versions("3.6", "3.6.0").is_eq());
        assert!(compare_versions("3.6.1", "3.6.2").is_lt());
    }

    #[test]
    fn publisher_check_requires_expected_cn_and_organization() {
        assert!(publisher_subject_matches(
            "C=PL, ST=małopolskie, O=Krzysztof Kowalczyk, CN=Krzysztof Kowalczyk",
            EXPECTED_SIGNER_NAME
        ));
        assert!(!publisher_subject_matches(
            "C=US, O=Other Publisher, CN=Krzysztof Kowalczyk",
            EXPECTED_SIGNER_NAME
        ));
        assert!(!publisher_subject_matches(
            "C=PL, O=Krzysztof Kowalczyk, CN=Different Person",
            EXPECTED_SIGNER_NAME
        ));
    }

    #[test]
    fn status_reads_verified_version_state_offline() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SumatraManager::new(dir.path().to_path_buf());
        let exe = b"MZverified dynamic fixture";
        fs::write(manager.exe_path(), exe).unwrap();
        fs::write(
            manager.installed_release_path(),
            serde_json::to_vec(&InstalledRelease {
                version: "3.7.0".into(),
                exe_sha256: format!("{:x}", Sha256::digest(exe)),
                publisher: EXPECTED_SIGNER_NAME.into(),
            })
            .unwrap(),
        )
        .unwrap();

        let status = manager.status();
        assert!(status.ready);
        assert_eq!(status.version, "3.7.0");
        assert_eq!(status.current_version, "3.7.0");
        assert_eq!(status.latest_version, None);
        assert!(!status.update_available);
    }

    #[test]
    fn failed_version_state_write_restores_previous_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe_path = dir.path().join("SumatraPDF.exe");
        let old_exe = b"old verified executable";
        fs::write(&exe_path, old_exe).unwrap();
        let state_path = dir.path().join(".sumatra-installed.json");
        fs::create_dir(&state_path).unwrap();
        let mut staged = NamedTempFile::new_in(dir.path()).unwrap();
        staged.write_all(b"MZnew verified executable").unwrap();

        assert!(persist_dynamic_executable(
            staged,
            &exe_path,
            &state_path,
            InstalledRelease {
                version: "3.7.0".into(),
                exe_sha256: "0".repeat(64),
                publisher: EXPECTED_SIGNER_NAME.into(),
            },
        )
        .is_err());
        assert_eq!(fs::read(exe_path).unwrap(), old_exe);
    }

    #[test]
    fn download_status_serializes_update_fields() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SumatraManager::new(dir.path().to_path_buf());
        let status = manager.status_with(
            Some("3.6.1".into()),
            Some("3.7.0".into()),
            true,
            manager.exe_path(),
        );
        let serialized = serde_json::to_value(status).unwrap();
        assert_eq!(serialized["current_version"], "3.6.1");
        assert_eq!(serialized["latest_version"], "3.7.0");
        assert_eq!(serialized["update_available"], true);
    }
}
