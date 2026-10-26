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
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DownloadStatus {
    pub ready: bool,
    pub downloading: bool,
    pub version: String,
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
        DownloadStatus {
            ready: executable_matches(&exe_path, EXE_SHA256),
            downloading: self.downloading.load(Ordering::Acquire),
            version: release().version.clone(),
            path: exe_path.to_string_lossy().into_owned(),
        }
    }

    pub fn download(&self) -> Result<DownloadStatus, String> {
        self.download_archive_with(|archive, spec| {
            let client = Client::builder()
                .redirect(Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(DOWNLOAD_TIMEOUT)
                .user_agent("print-bridge/0.1")
                .build()
                .map_err(display_error)?;
            let mut response = get_official_response(&client, &spec.url)?;
            if response
                .content_length()
                .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
            {
                return Err("Sumatra archive exceeds the 150 MiB limit".into());
            }
            receive_archive(&mut response, archive, &spec.sha256, MAX_ARCHIVE_BYTES)
        })
    }

    pub fn exe_path(&self) -> PathBuf {
        self.directory.join("SumatraPDF.exe")
    }

    fn download_archive_with(
        &self,
        acquire: impl FnOnce(&mut NamedTempFile, &BinaryLock) -> Result<(), String>,
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
            acquire(&mut archive, &release().binary)?;
            archive.as_file().sync_all().map_err(display_error)?;
            install_archive(
                archive.path(),
                &self.directory,
                &release().binary.member,
                EXE_SHA256,
                MAX_EXE_BYTES,
            )
        })();
        drop(guard);
        result?;
        Ok(self.status())
    }
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
    expected_sha256: &str,
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
    if format!("{:x}", digest.finalize()) != expected_sha256 {
        return Err("Sumatra archive SHA-256 does not match the pinned release".into());
    }
    archive.flush().map_err(display_error)
}

fn install_archive(
    archive_path: &Path,
    directory: &Path,
    member: &str,
    expected_sha256: &str,
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
    if format!("{:x}", digest.finalize()) != expected_sha256 {
        return Err("Sumatra executable SHA-256 does not match the pinned release".into());
    }
    staged_exe.as_file().sync_all().map_err(display_error)?;
    staged_exe
        .persist(directory.join("SumatraPDF.exe"))
        .map_err(|error| display_error(error.error))?;
    Ok(())
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
        let error = manager
            .download_archive_with(|archive, spec| {
                receive_archive(
                    &mut Cursor::new(bytes),
                    archive,
                    &spec.sha256,
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
            "unused",
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
        let error = manager
            .download_archive_with(|archive, spec| {
                receive_archive(
                    &mut Cursor::new(b"bad archive"),
                    archive,
                    &spec.sha256,
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
        let worker = thread::spawn(move || {
            worker_manager.download_archive_with(|_, _| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err("test download stopped".into())
            })
        });
        started_rx.recv().unwrap();
        let error = manager
            .download_archive_with(|_, _| panic!("second download must not start"))
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
}
