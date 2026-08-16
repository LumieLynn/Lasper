//! Tar and raw disk image deployment implementations.

use async_trait::async_trait;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::image_operation::TarSourceOrigin;
use crate::nspawn::ops::provision::{
    process_state_unknown, send_deploy_log, send_deploy_progress, send_deploy_stream_log,
    AppliedResource, ApplyReport, DeployLogEvent, Deployer, DeploymentCancellation,
};
use crate::nspawn::sys::log_output;

const MAX_IMAGE_BYTES: u64 = crate::nspawn::models::config::MAX_DISK_IMAGE_SIZE_BYTES;
const CURL_FILESIZE_EXCEEDED_EXIT_CODE: i32 = 63;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSource {
    Local(String),
    Remote(String),
}

impl ImageSource {
    fn tar_origin(&self) -> TarSourceOrigin {
        match self {
            Self::Local(_) => TarSourceOrigin::Local,
            Self::Remote(_) => TarSourceOrigin::Remote,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Tar,
    Raw,
}

impl ImageFormat {
    pub fn from_artifact(artifact: &crate::nspawn::models::ArtifactSpec) -> Self {
        match artifact.resolved_format() {
            crate::nspawn::models::ArtifactFormat::Tar => Self::Tar,
            crate::nspawn::models::ArtifactFormat::Raw => Self::Raw,
            crate::nspawn::models::ArtifactFormat::Auto => unreachable!("format was resolved"),
        }
    }
}

pub struct ImageDeployer {
    pub source: ImageSource,
    pub format: ImageFormat,
    pub image_import: crate::nspawn::ops::provision::ImageImportStore,
    pub allow_unsafe_remote_tar: bool,
}

#[async_trait]
impl Deployer for ImageDeployer {
    fn is_external_storage_managed(&self) -> bool {
        self.format == ImageFormat::Raw
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()> {
        cancellation.checkpoint()?;
        let source = acquire_image_source(&self.source, &logs, cancellation).await?;
        let source = normalize_compression(source, &logs, cancellation).await?;
        cancellation.checkpoint()?;
        match self.format {
            ImageFormat::Raw => {
                send_deploy_log(&logs, "Importing typed RAW machine image...").await;
                let machine = crate::nspawn::models::MachineName::new(name)
                    .map_err(|error| NspawnError::Validation(error.to_string()))?;
                self.image_import.import_raw(machine, source).await?;
                report.record_created(AppliedResource::ExternalImage);
                cancellation.checkpoint()?;
            }
            ImageFormat::Tar => {
                send_deploy_log(&logs, "Extracting typed rootfs archive...").await;
                let target = crate::nspawn::adapters::rootfs::RootfsTarget::from_provisioned_path(
                    name, rootfs,
                )?;
                let report = self
                    .image_import
                    .import_tar(
                        target,
                        source,
                        self.source.tar_origin(),
                        self.allow_unsafe_remote_tar,
                    )
                    .await?;
                for warning in report.warnings {
                    send_deploy_log(&logs, format!("WARNING: {warning}")).await;
                }
                cancellation.checkpoint()?;
            }
        }
        Ok(())
    }
}

async fn acquire_image_source(
    source: &ImageSource,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
    cancellation: &DeploymentCancellation,
) -> Result<std::fs::File> {
    match source {
        ImageSource::Local(path) => std::fs::File::open(path)
            .map_err(|error| NspawnError::Io(std::path::PathBuf::from(path), error)),
        ImageSource::Remote(url) => download_image(url, logs, cancellation).await,
    }
}

async fn download_image(
    url: &str,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
    cancellation: &DeploymentCancellation,
) -> Result<std::fs::File> {
    check_tool("curl")?;
    let url = validate_download_url(url)?;
    send_deploy_log(logs, "Downloading container image...").await;

    let mut destination = tempfile::tempfile()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("temporary image"), error))?;
    let output = destination
        .try_clone()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("temporary image"), error))?;
    let redirect_protocols = if url.scheme() == "https" {
        "=https"
    } else {
        "=http,https"
    };
    let max_filesize = MAX_IMAGE_BYTES.to_string();
    let mut command = crate::nspawn::sys::new_command("curl");
    command.kill_on_drop(true);
    command.as_std_mut().process_group(0);
    crate::nspawn::sys::command::limit_file_size(command.as_std_mut(), MAX_IMAGE_BYTES);
    let mut child = command
        .args([
            "--progress-bar",
            "--location",
            "--fail",
            "--show-error",
            "--proto",
            "=http,https",
            "--proto-redir",
            redirect_protocols,
            "--max-filesize",
            &max_filesize,
            "--user-agent",
            "Lasper/0.3",
            "--",
        ])
        .arg(url.as_str())
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("curl"), error))?;
    let pid = child.id().expect("curl has pid");
    let mut stderr = child.stderr.take().expect("curl stderr piped");
    let stream_result = tokio::select! {
        result = stream_curl_output(&mut stderr, logs) => Some(result),
        _ = cancellation.cancelled() => None,
    };
    drop(stderr);
    let Some(stream_result) = stream_result else {
        terminate_child_group(&mut child, pid, "curl").await?;
        return Err(NspawnError::DeploymentCancelled);
    };
    if let Err(error) = stream_result {
        terminate_child_group(&mut child, pid, "curl").await?;
        return Err(error);
    }
    if cancellation.is_requested() {
        terminate_child_group(&mut child, pid, "curl").await?;
        return Err(NspawnError::DeploymentCancelled);
    }
    let status = child
        .wait()
        .await
        .map_err(|error| process_state_unknown("curl", error))?;
    if !status.success() {
        if status.code() == Some(CURL_FILESIZE_EXCEEDED_EXIT_CODE)
            || file_size(&destination).is_some_and(|size| size >= MAX_IMAGE_BYTES)
        {
            return Err(NspawnError::Validation(format!(
                "Downloaded image exceeds the {} byte limit",
                MAX_IMAGE_BYTES
            )));
        }
        return Err(NspawnError::DeployError(format!(
            "Network download failed: {status}"
        )));
    }
    validate_source_size(&destination, "Downloaded image")?;
    destination
        .seek(SeekFrom::Start(0))
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("temporary image"), error))?;
    Ok(destination)
}

fn validate_download_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value.trim())
        .map_err(|error| NspawnError::Validation(format!("Invalid image URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NspawnError::Validation(
            "Image URL must use HTTP or HTTPS".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NspawnError::Validation(
            "Image URL must not contain embedded credentials".into(),
        ));
    }
    Ok(url)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageCompression {
    Gzip,
    Xz,
    Zstd,
    Bzip2,
}

impl ImageCompression {
    fn program(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Xz => "xz",
            Self::Zstd => "zstd",
            Self::Bzip2 => "bzip2",
        }
    }
}

fn detect_compression(source: &std::fs::File) -> Result<Option<ImageCompression>> {
    let mut header = [0u8; 6];
    let count = source
        .read_at(&mut header, 0)
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("image source fd"), error))?;
    let header = &header[..count];
    let compression = if header.starts_with(&[0x1f, 0x8b]) {
        Some(ImageCompression::Gzip)
    } else if header.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Some(ImageCompression::Xz)
    } else if header.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some(ImageCompression::Zstd)
    } else if header.starts_with(b"BZh") {
        Some(ImageCompression::Bzip2)
    } else {
        None
    };
    Ok(compression)
}

async fn normalize_compression(
    mut source: std::fs::File,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
    cancellation: &DeploymentCancellation,
) -> Result<std::fs::File> {
    validate_source_size(&source, "Image source")?;
    let Some(compression) = detect_compression(&source)? else {
        source
            .seek(SeekFrom::Start(0))
            .map_err(|error| NspawnError::Io(std::path::PathBuf::from("image source fd"), error))?;
        return Ok(source);
    };

    let program = compression.program();
    check_tool(program)?;
    send_deploy_log(logs, format!("Decompressing image with {program}...")).await;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("image source fd"), error))?;
    let mut destination = tempfile::tempfile()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("decompressed image"), error))?;
    let output = destination
        .try_clone()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("decompressed image"), error))?;
    let mut command = crate::nspawn::sys::new_command(program);
    command.kill_on_drop(true);
    command.as_std_mut().process_group(0);
    crate::nspawn::sys::command::limit_file_size(command.as_std_mut(), MAX_IMAGE_BYTES);
    let mut child = command
        .args(["-d", "-c"])
        .stdin(Stdio::from(source))
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from(program), error))?;
    let pid = child.id().expect("decompressor has pid");
    let mut stderr = child.stderr.take().expect("decompressor stderr piped");
    let mut stderr_bytes = Vec::new();
    let mut completed =
        Box::pin(async { tokio::join!(child.wait(), stderr.read_to_end(&mut stderr_bytes)) });
    let completed_result = tokio::select! {
        result = &mut completed => Some(result),
        _ = cancellation.cancelled() => None,
    };
    drop(completed);
    let Some((status, stderr_result)) = completed_result else {
        terminate_child_group(&mut child, pid, program).await?;
        return Err(NspawnError::DeploymentCancelled);
    };
    let status = status.map_err(|error| process_state_unknown(program, error))?;
    stderr_result.map_err(|error| NspawnError::Io(std::path::PathBuf::from(program), error))?;
    let result = std::process::Output {
        status,
        stdout: Vec::new(),
        stderr: stderr_bytes,
    };
    log_output(program, &result);
    if !result.status.success() {
        if file_size(&destination).is_some_and(|size| size >= MAX_IMAGE_BYTES) {
            return Err(NspawnError::Validation(format!(
                "Decompressed image exceeds the {} byte limit",
                MAX_IMAGE_BYTES
            )));
        }
        return Err(NspawnError::cmd_failed(
            "decompress image source",
            format!("{program} -d -c"),
            &result,
        ));
    }
    validate_source_size(&destination, "Decompressed image")?;
    destination
        .seek(SeekFrom::Start(0))
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("decompressed image"), error))?;
    Ok(destination)
}

fn validate_source_size(source: &std::fs::File, label: &str) -> Result<()> {
    validate_source_size_with_limit(source, label, MAX_IMAGE_BYTES)
}

fn validate_source_size_with_limit(source: &std::fs::File, label: &str, limit: u64) -> Result<()> {
    let metadata = source
        .metadata()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("image source fd"), error))?;
    if metadata.is_file() && metadata.len() > limit {
        return Err(NspawnError::Validation(format!(
            "{label} exceeds the {} byte limit",
            limit
        )));
    }
    Ok(())
}

fn file_size(file: &std::fs::File) -> Option<u64> {
    file.metadata().ok().map(|metadata| metadata.len())
}

async fn terminate_child_group(
    child: &mut tokio::process::Child,
    pid: u32,
    label: &str,
) -> Result<()> {
    const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    if let Err(error) = crate::nspawn::sys::command::signal_process_group(pid, libc::SIGTERM) {
        log::warn!("Failed to request termination of {label}: {error}; waiting for natural exit");
        tokio::time::timeout(STOP_TIMEOUT, child.wait())
            .await
            .map_err(|_| {
                process_state_unknown(
                    label,
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "termination request failed ({error}) and process exit was not confirmed"
                        ),
                    ),
                )
            })?
            .map_err(|wait_error| process_state_unknown(label, wait_error))?;
        return Ok(());
    }
    match tokio::time::timeout(STOP_TIMEOUT, child.wait()).await {
        Ok(status) => {
            status.map_err(|error| process_state_unknown(label, error))?;
        }
        Err(_) => {
            crate::nspawn::sys::command::signal_process_group(pid, libc::SIGKILL)
                .map_err(|error| process_state_unknown(label, error))?;
            tokio::time::timeout(STOP_TIMEOUT, child.wait())
                .await
                .map_err(|_| {
                    process_state_unknown(
                        label,
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "process exit was not confirmed after SIGKILL",
                        ),
                    )
                })?
                .map_err(|error| process_state_unknown(label, error))?;
        }
    }
    Ok(())
}

/// Parse curl's carriage-return progress frames and forward normal output.
async fn stream_curl_output(
    stderr: &mut tokio::process::ChildStderr,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
) -> Result<()> {
    const MAX_FRAME_BYTES: usize = 16 * 1024;
    let mut chunk = [0u8; 4096];
    let mut frame = Vec::new();
    {
        loop {
            let count = stderr
                .read(&mut chunk)
                .await
                .map_err(|error| NspawnError::Io(std::path::PathBuf::from("curl"), error))?;
            if count == 0 {
                break;
            }
            for byte in &chunk[..count] {
                if *byte == b'\r' || *byte == b'\n' {
                    forward_curl_frame(logs, &frame, *byte == b'\r').await;
                    frame.clear();
                } else if frame.len() < MAX_FRAME_BYTES {
                    frame.push(*byte);
                }
            }
        }
    }
    forward_curl_frame(logs, &frame, false).await;
    Ok(())
}

async fn forward_curl_frame(
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
    frame: &[u8],
    carriage_return: bool,
) {
    let text = String::from_utf8_lossy(frame);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(permille) = parse_curl_progress(trimmed) {
        send_deploy_progress(logs, "Downloading image", permille).await;
    } else if !carriage_return || !looks_like_curl_progress(trimmed) {
        send_deploy_stream_log(logs, trimmed).await;
    }
}

fn parse_curl_progress(frame: &str) -> Option<u16> {
    let percent = frame
        .split_ascii_whitespace()
        .rev()
        .find_map(|part| part.strip_suffix('%')?.parse::<f64>().ok())?;
    percent
        .is_finite()
        .then(|| (percent.clamp(0.0, 100.0) * 10.0).round() as u16)
}

fn looks_like_curl_progress(frame: &str) -> bool {
    frame
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'#' | b'=' | b'-' | b'.' | b'0'..=b'9' | b'%'))
}

pub fn check_tool(name: &str) -> Result<()> {
    let found = std::env::var_os("PATH")
        .unwrap_or_default()
        .to_string_lossy()
        .split(':')
        .map(|d| std::path::PathBuf::from(d).join(name))
        .any(|p| p.is_file());
    if found {
        Ok(())
    } else {
        Err(NspawnError::ToolNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn curl_progress_parser_reads_percentage_without_counting_bar_cells() {
        assert_eq!(
            parse_curl_progress("####################  37.4%"),
            Some(374)
        );
        assert_eq!(parse_curl_progress("100.0%"), Some(1000));
        assert_eq!(parse_curl_progress("curl: (22) HTTP error"), None);
    }

    #[test]
    fn curl_progress_detection_does_not_hide_error_messages() {
        assert!(looks_like_curl_progress("#######====  42.0%"));
        assert!(!looks_like_curl_progress("curl: connection reset"));
    }

    #[test]
    fn artifact_format_is_resolved_before_privileged_import() {
        assert_eq!(
            ImageFormat::from_artifact(&crate::nspawn::models::ArtifactSpec {
                path: "rootfs.raw.xz".into(),
                format: crate::nspawn::models::ArtifactFormat::Auto,
            }),
            ImageFormat::Raw
        );
        assert_eq!(
            ImageFormat::from_artifact(&crate::nspawn::models::ArtifactSpec {
                path: "rootfs.tar.xz".into(),
                format: crate::nspawn::models::ArtifactFormat::Auto,
            }),
            ImageFormat::Tar
        );
    }

    #[test]
    fn image_source_origin_is_preserved_for_tar_policy() {
        assert_eq!(
            ImageSource::Local("/tmp/rootfs.tar".into()).tar_origin(),
            TarSourceOrigin::Local
        );
        assert_eq!(
            ImageSource::Remote("https://example.test/rootfs.tar".into()).tar_origin(),
            TarSourceOrigin::Remote
        );
    }

    #[test]
    fn download_urls_are_limited_to_http_without_embedded_credentials() {
        assert!(validate_download_url("https://example.test/rootfs.tar.xz").is_ok());
        assert!(validate_download_url("http://example.test/rootfs.raw").is_ok());
        assert!(validate_download_url("file:///etc/shadow").is_err());
        assert!(validate_download_url("https://user:secret@example.test/rootfs.raw").is_err());
    }

    #[test]
    fn source_size_limit_rejects_oversized_files_before_processing() {
        let source = tempfile::tempfile().unwrap();
        source.set_len(5).unwrap();

        let error = validate_source_size_with_limit(&source, "Image source", 4).unwrap_err();

        assert!(error.to_string().contains("4 byte limit"));
    }

    #[test]
    fn compression_is_detected_from_content_instead_of_source_name() {
        let cases: &[(&[u8], Option<ImageCompression>)] = &[
            (&[0x1f, 0x8b, 0x08], Some(ImageCompression::Gzip)),
            (
                &[0xfd, b'7', b'z', b'X', b'Z', 0x00],
                Some(ImageCompression::Xz),
            ),
            (&[0x28, 0xb5, 0x2f, 0xfd], Some(ImageCompression::Zstd)),
            (b"BZh9", Some(ImageCompression::Bzip2)),
            (b"ustar", None),
        ];
        for (content, expected) in cases {
            let mut source = tempfile::tempfile().unwrap();
            source.write_all(content).unwrap();
            assert_eq!(detect_compression(&source).unwrap(), *expected);
        }
    }

    #[tokio::test]
    async fn compressed_source_is_decoded_and_rewound() {
        let mut plain = tempfile::tempfile().unwrap();
        plain.write_all(b"typed image payload").unwrap();
        plain.seek(SeekFrom::Start(0)).unwrap();

        let mut compressed = tempfile::tempfile().unwrap();
        let output = crate::nspawn::sys::new_sync_command("gzip")
            .args(["-c"])
            .stdin(Stdio::from(plain))
            .stdout(Stdio::from(compressed.try_clone().unwrap()))
            .output()
            .unwrap();
        assert!(output.status.success());
        compressed.seek(SeekFrom::Start(0)).unwrap();

        let (logs, _receiver) = tokio::sync::mpsc::channel(4);
        let mut decoded =
            normalize_compression(compressed, &logs, &DeploymentCancellation::default())
                .await
                .unwrap();
        let mut payload = Vec::new();
        decoded.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"typed image payload");
    }

    #[tokio::test]
    async fn corrupt_compressed_source_is_rejected() {
        let mut compressed = tempfile::tempfile().unwrap();
        compressed.write_all(&[0x1f, 0x8b, 0x08, 0x00]).unwrap();
        compressed.seek(SeekFrom::Start(0)).unwrap();

        let (logs, _receiver) = tokio::sync::mpsc::channel(4);
        assert!(
            normalize_compression(compressed, &logs, &DeploymentCancellation::default())
                .await
                .is_err()
        );
    }
}
