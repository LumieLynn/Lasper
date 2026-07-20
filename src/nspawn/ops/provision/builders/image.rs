//! OCI and Disk Image deployment implementations.

use async_trait::async_trait;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::{
    send_deploy_log, send_deploy_progress, send_deploy_stream_log, DeployLogEvent, Deployer,
};
use crate::nspawn::sys::{log_output, CommandRunner};

pub struct OciDeployer {
    pub url: String,
    pub cmd_runner: Arc<dyn CommandRunner>,
    pub io: crate::nspawn::sys::ElevatedIo,
}

#[async_trait]
impl Deployer for OciDeployer {
    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    ) -> Result<()> {
        import_oci_image(
            self.cmd_runner.as_ref(),
            &self.io,
            &self.url,
            name,
            rootfs,
            &logs,
        )
        .await
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSource {
    Local(String),
    Remote(String),
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
    ) -> Result<()> {
        let source = acquire_image_source(&self.source, &logs).await?;
        let source = normalize_compression(source, &logs).await?;
        match self.format {
            ImageFormat::Raw => {
                send_deploy_log(&logs, "Importing typed RAW machine image...").await;
                let machine = crate::nspawn::models::MachineName::new(name)
                    .map_err(|error| NspawnError::Validation(error.to_string()))?;
                self.image_import.import_raw(machine, source).await
            }
            ImageFormat::Tar => {
                send_deploy_log(&logs, "Extracting typed rootfs archive...").await;
                let target = crate::nspawn::adapters::rootfs::RootfsTarget::from_provisioned_path(
                    name, rootfs,
                )?;
                self.image_import.import_tar(target, source).await
            }
        }
    }
}

async fn acquire_image_source(
    source: &ImageSource,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
) -> Result<std::fs::File> {
    match source {
        ImageSource::Local(path) => std::fs::File::open(path)
            .map_err(|error| NspawnError::Io(std::path::PathBuf::from(path), error)),
        ImageSource::Remote(url) => download_image(url, logs).await,
    }
}

async fn download_image(
    url: &str,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
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
    let mut command = crate::nspawn::sys::new_command("curl");
    command.kill_on_drop(true);
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
            "--user-agent",
            "Lasper/0.3",
            "--",
        ])
        .arg(url.as_str())
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("curl"), error))?;
    let mut stderr = child.stderr.take().expect("curl stderr piped");
    stream_curl_output(&mut stderr, logs).await?;
    let status = child
        .wait()
        .await
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("curl"), error))?;
    if !status.success() {
        return Err(NspawnError::DeployError(format!(
            "Network download failed: {status}"
        )));
    }
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
) -> Result<std::fs::File> {
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
    let child = command
        .args(["-d", "-c"])
        .stdin(Stdio::from(source))
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from(program), error))?;
    let result = child
        .wait_with_output()
        .await
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from(program), error))?;
    log_output(program, &result);
    if !result.status.success() {
        return Err(NspawnError::cmd_failed(
            "decompress image source",
            format!("{program} -d -c"),
            &result,
        ));
    }
    destination
        .seek(SeekFrom::Start(0))
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("decompressed image"), error))?;
    Ok(destination)
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

/// Normalize an OCI image reference for use with skopeo.
fn normalize_oci_image_ref(image_ref: &str) -> String {
    let transports = [
        "docker://",
        "oci:",
        "dir:",
        "docker-archive:",
        "docker-daemon:",
        "ostree:",
        "containers-storage:",
    ];
    if transports.iter().any(|t| image_ref.starts_with(t)) || image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

fn skopeo_copy_args(
    policy_path: &std::path::Path,
    image_ref: &str,
    tmp_oci: &std::path::Path,
) -> Vec<String> {
    vec![
        "--policy".into(),
        policy_path.to_string_lossy().to_string(),
        "copy".into(),
        image_ref.into(),
        format!("oci:{}:latest", tmp_oci.display()),
    ]
}

/// Import an OCI registry image as a nspawn rootfs directory.
pub async fn import_oci_image(
    cmd_runner: &dyn CommandRunner,
    io: &crate::nspawn::sys::ElevatedIo,
    image_ref: &str,
    local_name: &str,
    dest: &std::path::Path,
    logs: &tokio::sync::mpsc::Sender<DeployLogEvent>,
) -> Result<()> {
    check_tool("skopeo")?;
    check_tool("umoci")?;

    let normalized_ref = normalize_oci_image_ref(image_ref);
    let staging_id = uuid::Uuid::new_v4();
    let staging_root = std::path::PathBuf::from(format!(
        "/var/cache/lasper/oci-staging/oci-deploy-{}-{}",
        local_name, staging_id
    ));
    io.create_dir_all(&staging_root).await?;
    let tmp_oci = staging_root.join("oci-repo");
    let bundle_dir = staging_root.join("bundle");
    let policy_path = staging_root.join("policy.json");

    let import_result: Result<()> = async {
        // Lasper accepts unsigned images for this import only. Never modify the
        // host-wide containers policy as a side effect of an application run.
        let import_policy = r#"{ "default": [ { "type": "insecureAcceptAnything" } ] }"#;
        io.write(&policy_path, import_policy).await?;

        send_deploy_log(
            logs,
            format!("Pulling OCI image '{}' via skopeo...", normalized_ref),
        )
        .await;

        // skopeo copy — streamed
        {
            let mut spawned = cmd_runner
                .spawn(
                    "skopeo",
                    skopeo_copy_args(&policy_path, &normalized_ref, &tmp_oci),
                )
                .await
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;
            {
                let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        send_deploy_stream_log(logs, trimmed).await;
                    }
                }
            }
            let status = spawned
                .wait()
                .await
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;
            if !status.success() {
                return Err(NspawnError::DeployError(format!(
                    "skopeo copy failed (exit code: {})",
                    status
                )));
            }
        }

        // Ensure dest parent exists
        if let Some(parent) = dest.parent() {
            io.create_dir_all(parent).await?;
        }

        send_deploy_log(logs, "Extracting rootfs with umoci...").await;

        let umoci_raw = cmd_runner
            .run(
                "umoci",
                vec![
                    "raw-unpack".into(),
                    "--image".into(),
                    format!("{}:latest", tmp_oci.display()),
                    dest.to_string_lossy().to_string(),
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;
        log_output("umoci", &umoci_raw);

        if umoci_raw.status.success() {
            send_deploy_log(logs, "OCI image imported via raw-unpack.").await;
            return Ok(());
        }

        // Fallback: umoci unpack
        send_deploy_log(logs, "umoci raw-unpack failed, falling back to unpack...").await;
        let umoci = cmd_runner
            .run(
                "umoci",
                vec![
                    "unpack".into(),
                    "--image".into(),
                    format!("{}:latest", tmp_oci.display()),
                    bundle_dir.to_string_lossy().to_string(),
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;
        log_output("umoci", &umoci);

        if !umoci.status.success() {
            return Err(NspawnError::cmd_failed(
                "umoci unpack",
                format!(
                    "umoci unpack --image {}:latest {}",
                    tmp_oci.display(),
                    bundle_dir.display()
                ),
                &umoci,
            ));
        }

        let rootfs_source = bundle_dir.join("rootfs");
        let rootfs_check = cmd_runner
            .run(
                "test",
                vec!["-d".into(), rootfs_source.to_string_lossy().to_string()],
            )
            .await
            .map_err(|e| NspawnError::Io(rootfs_source.clone(), e))?;
        if !rootfs_check.status.success() {
            return Err(NspawnError::DeployError(
                "umoci unpack did not create rootfs directory".into(),
            ));
        }

        send_deploy_log(logs, "Copying rootfs to destination...").await;
        let copy_out = cmd_runner
            .run(
                "cp",
                vec![
                    "-a".into(),
                    format!("{}/.", rootfs_source.to_string_lossy()),
                    dest.to_string_lossy().to_string(),
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;
        log_output("cp", &copy_out);

        if !copy_out.status.success() {
            return Err(NspawnError::cmd_failed(
                "cp rootfs content",
                format!("cp -a {}/. {}", rootfs_source.display(), dest.display()),
                &copy_out,
            ));
        }

        send_deploy_log(logs, "OCI image imported successfully.").await;
        Ok(())
    }
    .await;

    if let Err(error) = io.remove_dir_all(&staging_root).await {
        log::warn!(
            "Failed to clean OCI staging directory {}: {}",
            staging_root.display(),
            error
        );
    }

    import_result
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
    fn test_normalize_oci_image_ref() {
        assert_eq!(normalize_oci_image_ref("ubuntu"), "docker://ubuntu");
        assert_eq!(
            normalize_oci_image_ref("docker://ubuntu"),
            "docker://ubuntu"
        );
        assert_eq!(
            normalize_oci_image_ref("nvcr.io/nvidia/cuda:12.0"),
            "docker://nvcr.io/nvidia/cuda:12.0"
        );
        assert_eq!(
            normalize_oci_image_ref("oci:/tmp/myimage:latest"),
            "oci:/tmp/myimage:latest"
        );
    }

    #[test]
    fn test_skopeo_copy_uses_scoped_policy() {
        let args = skopeo_copy_args(
            std::path::Path::new("/var/cache/lasper/staging/policy.json"),
            "docker://ubuntu:latest",
            std::path::Path::new("/var/cache/lasper/staging/oci-repo"),
        );

        assert_eq!(args[0], "--policy");
        assert_eq!(args[1], "/var/cache/lasper/staging/policy.json");
        assert_eq!(args[2], "copy");
        assert!(!args.iter().any(|arg| arg == "/etc/containers/policy.json"));
    }

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
    fn download_urls_are_limited_to_http_without_embedded_credentials() {
        assert!(validate_download_url("https://example.test/rootfs.tar.xz").is_ok());
        assert!(validate_download_url("http://example.test/rootfs.raw").is_ok());
        assert!(validate_download_url("file:///etc/shadow").is_err());
        assert!(validate_download_url("https://user:secret@example.test/rootfs.raw").is_err());
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
        let mut decoded = normalize_compression(compressed, &logs).await.unwrap();
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
        assert!(normalize_compression(compressed, &logs).await.is_err());
    }
}
