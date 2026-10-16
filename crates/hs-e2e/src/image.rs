use std::io::{IsTerminal, Write};
use std::path::Path;
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
    pub downloaded_bytes: u64,
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
    let manifest = client
        .get(format!("{DIRECTORY}SHA512SUMS"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = checksum(&manifest, FILENAME)?;
    let url = format!("{DIRECTORY}{FILENAME}");
    let mut response = client.get(&url).send().await?.error_for_status()?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
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
    progress.finish();
    eprintln!("Debian image SHA-512 verified.");
    Ok(Image {
        url,
        sha512: actual,
        downloaded_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_requires_one_exact_filename_and_a_valid_digest() {
        let digest = "a".repeat(128);
        let manifest = format!("{digest}  image.qcow2.old\n{digest} *image.qcow2\n");
        assert_eq!(checksum(&manifest, "image.qcow2").ok(), Some(digest.clone()));
        assert!(checksum(&manifest, "missing.qcow2").is_err());
        assert!(checksum("bad image.qcow2", "image.qcow2").is_err());
        assert!(checksum(&format!("{manifest}{digest} image.qcow2\n"), "image.qcow2").is_err());
    }
}
