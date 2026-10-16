use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha512};

use crate::Result;

const DIRECTORY: &str = "https://cloud.debian.org/images/cloud/trixie/latest/";
const FILENAME: &str = "debian-13-genericcloud-amd64.qcow2";
const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

struct DownloadProgress {
    total: Option<u64>,
    downloaded: u64,
    started: Instant,
    last_update: Instant,
    terminal: bool,
    width: usize,
}

impl DownloadProgress {
    fn new(total: Option<u64>) -> Self {
        let mut progress = Self {
            total: total.filter(|total| *total > 0),
            downloaded: 0,
            started: Instant::now(),
            last_update: Instant::now(),
            terminal: std::io::stderr().is_terminal(),
            width: 0,
        };
        progress.draw();
        progress
    }

    fn update(&mut self, downloaded: u64) {
        self.downloaded = downloaded;
        let interval = if self.terminal {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        if self.last_update.elapsed() >= interval {
            self.draw();
        }
    }

    fn draw(&mut self) {
        let mib = self.downloaded as f64 / (1024.0 * 1024.0);
        let speed = mib / self.started.elapsed().as_secs_f64().max(0.001);
        let line = match self.total {
            Some(total) => {
                let fraction = (self.downloaded as f64 / total as f64).min(1.0);
                let filled = (fraction * 24.0) as usize;
                format!(
                    "Download [{}{}] {:5.1}% {mib:.1}/{:.1} MiB {speed:.1} MiB/s",
                    "=".repeat(filled),
                    ".".repeat(24 - filled),
                    fraction * 100.0,
                    total as f64 / (1024.0 * 1024.0),
                )
            }
            None => format!("Download {mib:.1} MiB {speed:.1} MiB/s (total size unknown)"),
        };
        let mut stderr = std::io::stderr().lock();
        if self.terminal {
            self.width = self.width.max(line.len());
            let _ = write!(stderr, "\r{line:width$}", width = self.width);
            let _ = stderr.flush();
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        self.last_update = Instant::now();
    }

    fn finish(mut self) {
        self.draw();
    }
}

impl Drop for DownloadProgress {
    fn drop(&mut self) {
        if self.terminal {
            let _ = writeln!(std::io::stderr());
        }
    }
}

#[derive(Serialize)]
pub struct Image {
    pub url: String,
    pub sha512: String,
    pub image_bytes: u64,
    pub downloaded_bytes: u64,
    pub cache_hit: bool,
}

pub fn checksum(manifest: &str, filename: &str) -> Result<String> {
    let mut matches = manifest.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        let digest = words.next()?;
        let name = words.next()?.trim_start_matches('*');
        (name == filename).then_some(digest)
    });
    let digest = matches.next().ok_or("image is absent from SHA512SUMS")?;
    if matches.next().is_some() || digest.len() != 128 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid or ambiguous image checksum".into());
    }
    Ok(digest.to_ascii_lowercase())
}

pub async fn download(path: &Path) -> Result<Image> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()?;
    let root = match std::env::var_os("XDG_CACHE_HOME") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".cache"),
    };
    cached_image(&client, path, &root.join("homestack-e2e/images"), DIRECTORY).await
}

async fn cached_image(client: &reqwest::Client, path: &Path, cache: &Path, directory: &str) -> Result<Image> {
    // Fetch the publisher's current checksum even when an image is already cached.
    let manifest = client
        .get(format!("{directory}SHA512SUMS"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = checksum(&manifest, FILENAME)?;
    std::fs::create_dir_all(cache)?;
    std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o700))?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(cache.join("image.lock"))?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(error) => return Err(error.into()),
        }
    }
    let cached = cache.join(FILENAME);
    let valid = match std::fs::symlink_metadata(&cached) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err("cached cloud image must be a regular file".into());
        }
        Ok(metadata) if metadata.len() > MAX_IMAGE_BYTES => None,
        Ok(_) => {
            eprintln!("Checking cached Debian image SHA-512...");
            let (actual, size) = file_digest(&cached).await?;
            (actual == expected).then_some(size)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let url = format!("{directory}{FILENAME}");
    let cache_hit = valid.is_some();
    let image_bytes = if let Some(size) = valid {
        eprintln!("Reusing verified cached Debian image: {}", cached.display());
        size
    } else {
        eprintln!("Downloading latest Debian image; cache is missing, outdated or corrupt...");
        fetch_image(client, &url, &expected, cache, &cached).await?
    };
    // Keep QEMU's backing file private to the run: replacing or clearing the
    // shared cache must never change a running VM's base image.
    let mut source = File::open(&cached)?;
    let mut destination = File::options().write(true).create_new(true).mode(0o600).open(path)?;
    if std::io::copy(&mut source, &mut destination)? != image_bytes {
        return Err("cached cloud image size changed while copying".into());
    }
    Ok(Image {
        url,
        sha512: expected,
        image_bytes,
        downloaded_bytes: if cache_hit { 0 } else { image_bytes },
        cache_hit,
    })
}

async fn file_digest(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut digest = Sha512::new();
    let mut bytes = 0;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        bytes += size as u64;
        if bytes > MAX_IMAGE_BYTES {
            return Err("cached cloud image exceeds the 2 GiB limit".into());
        }
        digest.update(&buffer[..size]);
        tokio::task::yield_now().await;
    }
    Ok((
        digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect(),
        bytes,
    ))
}

async fn fetch_image(client: &reqwest::Client, url: &str, expected: &str, cache: &Path, cached: &Path) -> Result<u64> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut file = tempfile::NamedTempFile::new_in(cache)?;
    let mut progress = DownloadProgress::new(response.content_length());
    let mut digest = Sha512::new();
    let mut downloaded_bytes = 0;
    while let Some(chunk) = response.chunk().await? {
        downloaded_bytes += chunk.len() as u64;
        if downloaded_bytes > MAX_IMAGE_BYTES {
            return Err("cloud image exceeds the 2 GiB download limit".into());
        }
        digest.update(&chunk);
        file.write_all(&chunk)?;
        progress.update(downloaded_bytes);
    }
    let actual: String = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != expected {
        return Err("cloud image SHA-512 mismatch; latest may have changed during download".into());
    }
    file.as_file().sync_all()?;
    file.as_file().set_permissions(std::fs::Permissions::from_mode(0o400))?;
    file.persist(cached)?;
    File::open(cache)?.sync_all()?;
    progress.finish();
    eprintln!("Debian image SHA-512 verified.");
    Ok(downloaded_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{Router, extract::State, routing::get};
    use std::sync::{Arc, Mutex};

    struct Published {
        body: String,
        manifest: String,
        manifest_requests: usize,
        image_requests: usize,
    }

    impl Published {
        fn publish(&mut self, body: &str) {
            self.body = body.into();
            let digest: String = Sha512::digest(body).iter().map(|byte| format!("{byte:02x}")).collect();
            self.manifest = format!("{digest}  {FILENAME}\n");
        }
    }

    struct Publisher {
        data: Arc<Mutex<Published>>,
        directory: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Publisher {
        async fn start() -> Result<Self> {
            let mut data = Published {
                body: String::new(),
                manifest: String::new(),
                manifest_requests: 0,
                image_requests: 0,
            };
            data.publish("first image");
            let data = Arc::new(Mutex::new(data));
            let router = Router::new()
                .route(
                    "/SHA512SUMS",
                    get(async |State(data): State<Arc<Mutex<Published>>>| {
                        let mut data = data.lock().unwrap_or_else(|error| error.into_inner());
                        data.manifest_requests += 1;
                        data.manifest.clone()
                    }),
                )
                .route(
                    &format!("/{FILENAME}"),
                    get(async |State(data): State<Arc<Mutex<Published>>>| {
                        let mut data = data.lock().unwrap_or_else(|error| error.into_inner());
                        data.image_requests += 1;
                        data.body.clone()
                    }),
                )
                .with_state(data.clone());
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let directory = format!("http://{}/", listener.local_addr()?);
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Ok(Self { data, directory, task })
        }
    }

    impl Drop for Publisher {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[test]
    fn checksum_requires_one_exact_filename_and_a_valid_digest() {
        let digest = "a".repeat(128);
        let manifest = format!("{digest}  image.qcow2.old\n{digest} *image.qcow2\n");
        assert_eq!(checksum(&manifest, "image.qcow2").ok(), Some(digest.clone()));
        assert!(checksum(&manifest, "missing.qcow2").is_err());
        assert!(checksum("bad image.qcow2", "image.qcow2").is_err());
        assert!(checksum(&format!("{manifest}{digest} image.qcow2\n"), "image.qcow2").is_err());
    }

    #[tokio::test]
    async fn reuse_rechecks_the_manifest_and_repairs_corrupt_or_outdated_images() -> Result<()> {
        let publisher = Publisher::start().await?;
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        let client = reqwest::Client::builder().no_proxy().build()?;
        let first = cached_image(&client, &root.path().join("first"), &cache, &publisher.directory).await?;
        assert!(!first.cache_hit);
        assert_eq!(first.downloaded_bytes, 11);
        std::fs::remove_file(root.path().join("first"))?;
        let second = cached_image(&client, &root.path().join("second"), &cache, &publisher.directory).await?;
        assert!(second.cache_hit);
        assert_eq!((second.downloaded_bytes, second.image_bytes), (0, 11));
        {
            let data = publisher.data.lock().map_err(|_| "publisher lock poisoned")?;
            assert_eq!((data.manifest_requests, data.image_requests), (2, 1));
        }
        let image = cache.join(FILENAME);
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(&image, "corrupt")?;
        let repaired = cached_image(&client, &root.path().join("repaired"), &cache, &publisher.directory).await?;
        assert!(!repaired.cache_hit);
        assert_eq!(std::fs::read_to_string(&image)?, "first image");
        publisher
            .data
            .lock()
            .map_err(|_| "publisher lock poisoned")?
            .publish("new image");
        let updated = cached_image(&client, &root.path().join("updated"), &cache, &publisher.directory).await?;
        assert!(!updated.cache_hit);
        assert_ne!(first.sha512, updated.sha512);
        assert_eq!(std::fs::read_to_string(&image)?, "new image");
        assert_eq!(std::fs::read_to_string(root.path().join("second"))?, "first image");
        let data = publisher.data.lock().map_err(|_| "publisher lock poisoned")?;
        assert_eq!((data.manifest_requests, data.image_requests), (4, 3));
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_download_keeps_the_previous_cache_and_removes_partial_files() -> Result<()> {
        let publisher = Publisher::start().await?;
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        let client = reqwest::Client::builder().no_proxy().build()?;
        cached_image(&client, &root.path().join("first"), &cache, &publisher.directory).await?;
        {
            let mut data = publisher.data.lock().map_err(|_| "publisher lock poisoned")?;
            data.publish("new image");
            data.body = "wrong content".into();
        }
        assert!(
            cached_image(&client, &root.path().join("failed"), &cache, &publisher.directory)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(cache.join(FILENAME))?, "first image");
        assert!(!root.path().join("failed").exists());
        assert_eq!(std::fs::read_dir(&cache)?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "downloads the current Debian image and updates the persistent image cache"]
    async fn live_debian_image_is_reused_after_checksum_verification() -> Result<()> {
        let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&work)?;
        let run = tempfile::tempdir_in(work)?;
        let first = download(&run.path().join("first.qcow2")).await?;
        let second = download(&run.path().join("second.qcow2")).await?;
        assert_eq!(first.sha512, second.sha512);
        assert_eq!(first.image_bytes, second.image_bytes);
        assert!(second.cache_hit);
        assert_eq!(second.downloaded_bytes, 0);
        eprintln!(
            "Live cache verified: first download {} bytes, second download {} bytes, SHA-512 {}",
            first.downloaded_bytes, second.downloaded_bytes, second.sha512
        );
        Ok(())
    }
}
