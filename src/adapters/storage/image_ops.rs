use crate::adapters::process::{log_output, CommandRunner};
use crate::adapters::storage::{ImageMountSource, ManagedImageKind};
use crate::domain::machine::MachineName;
use crate::domain::storage::{DiskImageFilesystem, DiskImagePartition, MAX_DISK_IMAGE_SIZE_BYTES};
use crate::nspawn::errors::{NspawnError, Result};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_IMAGE_BYTES: u64 = MAX_DISK_IMAGE_SIZE_BYTES;

fn filesystem_tool(filesystem: DiskImageFilesystem) -> &'static str {
    match filesystem {
        DiskImageFilesystem::Ext4 => "mkfs.ext4",
        DiskImageFilesystem::Xfs => "mkfs.xfs",
        DiskImageFilesystem::Btrfs => "mkfs.btrfs",
    }
}

#[derive(Debug)]
struct ImagePartitionTable {
    label: String,
    partitions: Vec<ImagePartitionLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImagePartitionProbe {
    pub label: String,
    pub partitions: Vec<ImagePartitionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImagePartitionInfo {
    pub number: DiskImagePartition,
    pub type_id: String,
}

#[derive(Debug)]
struct ImagePartitionLayout {
    number: DiskImagePartition,
    node: PathBuf,
    type_id: String,
}

#[derive(Debug)]
struct RootPartitionResolution {
    partition: Option<DiskImagePartition>,
    mark_selected_as_root: bool,
}

#[derive(Deserialize)]
struct SfdiskJson {
    partitiontable: SfdiskPartitionTable,
}

#[derive(Deserialize)]
struct SfdiskPartitionTable {
    label: String,
    device: String,
    #[serde(default)]
    partitions: Vec<SfdiskPartition>,
}

#[derive(Deserialize)]
struct SfdiskPartition {
    node: String,
    #[serde(rename = "type", default)]
    type_id: String,
}

#[derive(Deserialize)]
struct LosetupJson {
    #[serde(default)]
    loopdevices: Vec<LosetupDevice>,
}

#[derive(Deserialize)]
struct LosetupDevice {
    name: String,
}

pub(crate) async fn create_raw_image(
    machine: &MachineName,
    size_bytes: u64,
    filesystem: DiskImageFilesystem,
    partition_table: bool,
    runner: &dyn CommandRunner,
) -> Result<PathBuf> {
    validate_size_bytes(size_bytes)?;
    require_tool(filesystem_tool(filesystem))?;
    if partition_table {
        require_tool("sfdisk")?;
        require_tool("losetup")?;
        require_tool("udevadm")?;
    }

    let path = raw_image_path(machine);
    let conflicts = [
        crate::paths::machine_root(machine.as_str()),
        crate::paths::machine_image(machine.as_str(), "img"),
    ];
    create_raw_image_at(
        &path,
        &conflicts,
        size_bytes,
        filesystem,
        partition_table,
        runner,
    )
    .await?;
    Ok(path)
}

async fn create_raw_image_at(
    path: &Path,
    conflicts: &[PathBuf],
    size_bytes: u64,
    filesystem: DiskImageFilesystem,
    partition_table: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let path = reserve_raw_image(path, conflicts)?;
    let result = async {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .map_err(|error| NspawnError::Io(path.clone(), error))?
            .set_len(size_bytes)
            .await
            .map_err(|error| NspawnError::Io(path.clone(), error))?;

        if partition_table {
            format_partitioned_image(&path, filesystem, runner).await
        } else {
            format_whole_image(&path, filesystem, runner).await
        }
    }
    .await;

    if let Err(error) = result {
        remove_reserved_image(&path).await;
        return Err(error);
    }
    Ok(())
}

pub(crate) async fn import_raw_image(
    machine: &MachineName,
    source: std::fs::File,
) -> Result<PathBuf> {
    validate_import_source(&source)?;
    let path = raw_image_path(machine);
    let conflicts = [
        crate::paths::machine_root(machine.as_str()),
        crate::paths::machine_image(machine.as_str(), "img"),
    ];
    let import_path = path.clone();
    tokio::task::spawn_blocking(move || {
        import_raw_image_blocking(&import_path, &conflicts, source)
    })
    .await
    .map_err(|error| NspawnError::Runtime(format!("image import task failed: {error}")))??;
    Ok(path)
}

pub(crate) fn validate_import_source(source: &std::fs::File) -> Result<()> {
    validate_import_source_with_limit(source, MAX_IMAGE_BYTES)
}

fn validate_import_source_with_limit(source: &std::fs::File, limit: u64) -> Result<()> {
    let metadata = source
        .metadata()
        .map_err(|error| NspawnError::Io(PathBuf::from("import source fd"), error))?;
    let file_type = metadata.file_type();
    if !file_type.is_file() && !file_type.is_block_device() {
        return Err(NspawnError::Validation(
            "Source image descriptor is not a regular file or block device".into(),
        ));
    }
    if file_type.is_file() && metadata.len() == 0 {
        return Err(NspawnError::Validation(
            "Source image descriptor is empty".into(),
        ));
    }
    if file_type.is_file() && metadata.len() > limit {
        return Err(NspawnError::Validation(format!(
            "Source image descriptor exceeds the {} byte limit",
            limit
        )));
    }
    Ok(())
}

pub(crate) async fn mount_image(
    machine: &MachineName,
    source: ImageMountSource,
    root_partition: Option<DiskImagePartition>,
    runner: &dyn CommandRunner,
) -> Result<PathBuf> {
    let source_path = image_source_path(machine, source);
    validate_mount_source(&source_path, source).await?;

    if let Some(selected) = root_partition {
        if !matches!(source, ImageMountSource::Managed(_)) {
            return Err(NspawnError::Validation(
                "Manual root partition selection is only supported for managed image copies".into(),
            ));
        }
        prepare_selected_root_partition(&source_path, selected, runner).await?;
    }

    let mount_point = prepare_mount_point(machine).await?;

    let source_string = source_path.to_string_lossy().to_string();
    let mount_string = mount_point.to_string_lossy().to_string();
    let dissect = match runner
        .run(
            "systemd-dissect",
            vec![
                "--mount".into(),
                source_string.clone(),
                mount_string.clone(),
            ],
        )
        .await
    {
        Ok(output) => output,
        Err(error) => {
            remove_mount_point(&mount_point).await?;
            return Err(NspawnError::Io(PathBuf::from("systemd-dissect"), error));
        }
    };
    log_output("systemd-dissect --mount", &dissect);
    if dissect.status.success() {
        return Ok(mount_point);
    }

    log::warn!(
        "systemd-dissect failed for managed image {}: {}. Inspecting image layout before fallback.",
        machine,
        String::from_utf8_lossy(&dissect.stderr).trim()
    );

    // nspawn uses the same DDI dissection rules at startup. A raw loop
    // fallback is safe for a naked filesystem or a single-partition image,
    // but it can hide malformed secondary partitions in a multi-partition
    // image and produce a container that deploys but cannot boot.
    let table = inspect_partition_table(&source_path, runner).await?;
    if !allows_loop_fallback(table.as_ref(), root_partition)? {
        let _ = remove_mount_point(&mount_point).await;
        return Err(NspawnError::cmd_failed(
            "mount discoverable disk image",
            format!("systemd-dissect --mount {} {}", source_string, mount_string),
            &dissect,
        ));
    }

    log::warn!(
        "systemd-dissect failed for single-partition or filesystem image {}; attempting loop fallback.",
        machine
    );

    let result = match source {
        ImageMountSource::Managed(_) => {
            mount_managed_image_fallback(&source_path, &mount_point, root_partition, runner).await
        }
        ImageMountSource::BlockDevice => {
            mount_block_device_fallback(&source_path, &mount_point, runner).await
        }
    };
    if result.is_err() {
        let _ = remove_mount_point(&mount_point).await;
    }
    result.map(|()| mount_point)
}

fn allows_loop_fallback(
    table: Option<&ImagePartitionTable>,
    selected: Option<DiskImagePartition>,
) -> Result<bool> {
    if table.is_some_and(|table| table.partitions.len() > 1) {
        resolve_root_partition(table, selected)?;
        Ok(false)
    } else {
        Ok(true)
    }
}

pub(crate) async fn unmount_image(
    machine: &MachineName,
    source: ImageMountSource,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let mount_point = mount_point(machine);
    let mount_exists = validate_optional_mount_point(&mount_point).await?;
    let mut unmount_error = None;

    if mount_exists && is_mount_point(&mount_point).await? {
        let mount_string = mount_point.to_string_lossy().to_string();
        let mut unmount_attempt_error = match runner
            .run(
                "systemd-dissect",
                vec!["--umount".into(), mount_string.clone()],
            )
            .await
        {
            Ok(dissect) => {
                log_output("systemd-dissect --umount", &dissect);
                (!dissect.status.success()).then(|| {
                    NspawnError::cmd_failed(
                        "unmount managed image",
                        format!("systemd-dissect --umount {}", mount_point.display()),
                        &dissect,
                    )
                })
            }
            Err(error) => {
                log::warn!("systemd-dissect --umount unavailable: {}", error);
                Some(NspawnError::Io(PathBuf::from("systemd-dissect"), error))
            }
        };

        if is_mount_point(&mount_point).await? {
            match runner.run("umount", vec![mount_string]).await {
                Ok(fallback) => {
                    log_output("umount", &fallback);
                    unmount_attempt_error = (!fallback.status.success()).then(|| {
                        NspawnError::cmd_failed(
                            "unmount managed image",
                            format!("umount {}", mount_point.display()),
                            &fallback,
                        )
                    });
                }
                Err(error) => {
                    unmount_attempt_error = Some(NspawnError::Io(PathBuf::from("umount"), error));
                }
            }
        }

        if is_mount_point(&mount_point).await? {
            unmount_error = Some(unmount_attempt_error.unwrap_or_else(|| {
                NspawnError::Runtime(format!(
                    "Mount point remains mounted after unmount: {}",
                    mount_point.display()
                ))
            }));
        }
    }

    finish_successful_unmount(machine, source, unmount_error, runner).await?;
    remove_mount_point(&mount_point).await?;
    Ok(())
}

async fn finish_successful_unmount(
    machine: &MachineName,
    source: ImageMountSource,
    unmount_error: Option<NspawnError>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if let Some(error) = unmount_error {
        return Err(error);
    }
    if matches!(source, ImageMountSource::Managed(_)) {
        detach_image_loops(&image_source_path(machine, source), runner).await?;
    }
    Ok(())
}

pub(crate) fn validate_size_bytes(size_bytes: u64) -> Result<()> {
    if size_bytes == 0 || size_bytes > MAX_DISK_IMAGE_SIZE_BYTES {
        return Err(NspawnError::Validation(format!(
            "Disk image size must be between 1 byte and {} bytes",
            MAX_DISK_IMAGE_SIZE_BYTES
        )));
    }
    Ok(())
}

fn import_raw_image_blocking(
    path: &Path,
    conflicts: &[PathBuf],
    mut source: std::fs::File,
) -> Result<()> {
    validate_import_source(&source)?;
    let path = reserve_raw_image(path, conflicts)?;
    let result = (|| {
        let mut destination = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| NspawnError::Io(path.clone(), error))?;
        copy_source(&mut source, &mut destination)
            .map_err(|error| NspawnError::Io(path.clone(), error))?;
        destination
            .sync_all()
            .map_err(|error| NspawnError::Io(path.clone(), error))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result
}

fn copy_source(source: &mut std::fs::File, destination: &mut std::fs::File) -> std::io::Result<()> {
    copy_source_with_limit(source, destination, MAX_IMAGE_BYTES)
}

fn copy_source_with_limit(
    source: &mut std::fs::File,
    destination: &mut std::fs::File,
    limit: u64,
) -> std::io::Result<()> {
    let metadata = source.metadata()?;
    if metadata.is_file() && metadata.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("source image exceeds {limit} bytes"),
        ));
    }
    if metadata.is_file() && copy_sparse_regular_file(source, destination, metadata.len())? {
        return Ok(());
    }

    source.seek(SeekFrom::Start(0))?;
    destination.set_len(0)?;
    destination.seek(SeekFrom::Start(0))?;
    let copied = std::io::copy(&mut source.take(limit.saturating_add(1)), destination)?;
    if copied > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("source image exceeds {limit} bytes"),
        ));
    }
    Ok(())
}

fn copy_sparse_regular_file(
    source: &mut std::fs::File,
    destination: &mut std::fs::File,
    size: u64,
) -> std::io::Result<bool> {
    let source_fd = source.as_raw_fd();
    let mut offset = 0u64;
    while offset < size {
        let data = unsafe { libc::lseek(source_fd, offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Ok(false);
            }
            return Err(error);
        }
        let hole = unsafe { libc::lseek(source_fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Ok(false);
            }
            return Err(error);
        }

        let data = data as u64;
        let hole = (hole as u64).min(size);
        source.seek(SeekFrom::Start(data))?;
        destination.seek(SeekFrom::Start(data))?;
        let mut extent = source.take(hole - data);
        let copied = std::io::copy(&mut extent, destination)?;
        if copied != hole - data {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "source image changed during import",
            ));
        }
        offset = hole;
    }
    destination.set_len(size)?;
    Ok(true)
}

fn reserve_raw_image(path: &Path, conflicts: &[PathBuf]) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        NspawnError::Validation("managed raw image path has no parent directory".into())
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    for conflict in conflicts {
        reject_existing(conflict)?;
    }

    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.sync_all()
                .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
            Ok(path.to_path_buf())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(NspawnError::Validation(format!(
                "Managed storage already exists: {}",
                path.display()
            )))
        }
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

fn reject_existing(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(NspawnError::Validation(format!(
            "Managed storage already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn format_whole_image(
    path: &Path,
    filesystem: DiskImageFilesystem,
    runner: &dyn CommandRunner,
) -> Result<()> {
    run_mkfs(path.to_string_lossy().to_string(), filesystem, runner).await
}

async fn format_partitioned_image(
    path: &Path,
    filesystem: DiskImageFilesystem,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let script = format!("label: gpt\ntype={}\n", discoverable_root_uuid()?);
    let sfdisk = run_command_with_stdin(
        "sfdisk",
        vec![path.to_string_lossy().to_string()],
        script.into_bytes(),
    )
    .await?;
    log_output("sfdisk", &sfdisk);
    if !sfdisk.status.success() {
        return Err(NspawnError::cmd_failed(
            "partition managed raw image",
            format!("sfdisk {}", path.display()),
            &sfdisk,
        ));
    }

    let loop_device = attach_loop(path, runner).await?;
    let result = async {
        settle_udev(runner).await?;
        let partition = PathBuf::from(format!("{}p1", loop_device.display()));
        validate_block_device(&partition).await?;
        run_mkfs(partition.to_string_lossy().to_string(), filesystem, runner).await
    }
    .await;
    let detach_result = detach_loop(&loop_device, runner).await;
    result.and(detach_result)
}

async fn run_mkfs(
    target: String,
    filesystem: DiskImageFilesystem,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let program = filesystem_tool(filesystem);
    let force = match filesystem {
        DiskImageFilesystem::Ext4 => "-F",
        DiskImageFilesystem::Xfs | DiskImageFilesystem::Btrfs => "-f",
    };
    let output = runner
        .run(program, vec![force.into(), target.clone()])
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))?;
    log_output(program, &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "format managed raw image",
            format!("{program} {target}"),
            &output,
        ))
    }
}

async fn run_command_with_stdin(
    program: &str,
    args: Vec<String>,
    input: Vec<u8>,
) -> Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut child = crate::adapters::process::new_command(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| NspawnError::Runtime(format!("{program} stdin was not piped")))?;
    stdin
        .write_all(&input)
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))?;
    drop(stdin);
    child
        .wait_with_output()
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))
}

async fn mount_managed_image_fallback(
    image: &Path,
    mount_point: &Path,
    selected: Option<DiskImagePartition>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let table = inspect_partition_table(image, runner).await?;
    let resolution = resolve_root_partition(table.as_ref(), selected)?;
    let loop_device = attach_loop(image, runner).await?;
    let result = async {
        settle_udev(runner).await?;
        let device = match resolution.partition {
            Some(partition) => {
                let path =
                    PathBuf::from(format!("{}p{}", loop_device.display(), partition.number()));
                validate_block_device(&path).await?;
                path
            }
            None => loop_device.clone(),
        };
        mount_device(&device, mount_point, runner).await
    }
    .await;
    if result.is_err() {
        let _ = detach_loop(&loop_device, runner).await;
    }
    result
}

async fn mount_block_device_fallback(
    device: &Path,
    mount_point: &Path,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let table = inspect_partition_table(device, runner).await?;
    let resolution = resolve_root_partition(table.as_ref(), None)?;
    let target = match resolution.partition {
        Some(partition) => table
            .as_ref()
            .and_then(|table| {
                table
                    .partitions
                    .iter()
                    .find(|candidate| candidate.number == partition)
            })
            .map(|partition| partition.node.clone())
            .ok_or_else(|| {
                NspawnError::Validation(format!(
                    "Selected root partition p{} is missing from {}",
                    partition.number(),
                    device.display()
                ))
            })?,
        None => device.to_path_buf(),
    };
    mount_device(&target, mount_point, runner).await
}

async fn prepare_selected_root_partition(
    image: &Path,
    selected: DiskImagePartition,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let table = inspect_partition_table(image, runner)
        .await?
        .ok_or_else(|| {
            NspawnError::Validation(format!(
                "Cannot select p{} because {} has no partition table",
                selected.number(),
                image.display()
            ))
        })?;
    let resolution = resolve_root_partition(Some(&table), Some(selected))?;
    if !resolution.mark_selected_as_root {
        return Ok(());
    }

    let output = runner
        .run(
            "sfdisk",
            vec![
                "--part-type".into(),
                image.to_string_lossy().into_owned(),
                selected.number().to_string(),
                discoverable_root_uuid()?.into(),
            ],
        )
        .await
        .map_err(|error| command_io_error("sfdisk", error))?;
    log_output("sfdisk --part-type", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "mark selected image root partition",
            format!(
                "sfdisk --part-type {} {} <root-guid>",
                image.display(),
                selected.number()
            ),
            &output,
        ))
    }
}

async fn inspect_partition_table(
    image: &Path,
    runner: &dyn CommandRunner,
) -> Result<Option<ImagePartitionTable>> {
    let output = runner
        .run(
            "sfdisk",
            vec!["--json".into(), image.to_string_lossy().into_owned()],
        )
        .await
        .map_err(|error| command_io_error("sfdisk", error))?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_sfdisk_partition_table(&output.stdout).map(Some)
}

pub(crate) fn probe_image_partitions(image: &Path) -> Result<Option<ImagePartitionProbe>> {
    let output = crate::adapters::process::new_sync_command("sfdisk")
        .arg("--json")
        .arg(image)
        .output()
        .map_err(|error| command_io_error("sfdisk", error))?;
    if !output.status.success() {
        return Ok(None);
    }

    let table = parse_sfdisk_partition_table(&output.stdout)?;
    Ok(Some(ImagePartitionProbe {
        label: table.label,
        partitions: table
            .partitions
            .into_iter()
            .map(|partition| ImagePartitionInfo {
                number: partition.number,
                type_id: partition.type_id,
            })
            .collect(),
    }))
}

pub(crate) fn partition_type_label(type_id: &str) -> String {
    if type_id.eq_ignore_ascii_case("C12A7328-F81F-11D2-BA4B-00A0C93EC93B") {
        "EFI System".into()
    } else if type_id.eq_ignore_ascii_case("0FC63DAF-8483-4772-8E79-3D69D8477DE4") {
        "Linux filesystem".into()
    } else if is_current_architecture_root_type(type_id).unwrap_or(false) {
        "Root filesystem (current architecture)".into()
    } else if type_id.is_empty() {
        "Unknown partition type".into()
    } else {
        type_id.to_string()
    }
}

pub(crate) fn is_current_architecture_root_type(type_id: &str) -> Result<bool> {
    Ok(type_id.eq_ignore_ascii_case(discoverable_root_uuid()?))
}

fn parse_sfdisk_partition_table(output: &[u8]) -> Result<ImagePartitionTable> {
    let parsed: SfdiskJson = serde_json::from_slice(output).map_err(|error| {
        NspawnError::Runtime(format!(
            "failed to parse sfdisk partition metadata: {error}"
        ))
    })?;
    let device = parsed.partitiontable.device;
    let mut partitions = Vec::with_capacity(parsed.partitiontable.partitions.len());
    for partition in parsed.partitiontable.partitions {
        let number = partition_number_from_node(&device, &partition.node)?;
        partitions.push(ImagePartitionLayout {
            number,
            node: PathBuf::from(partition.node),
            type_id: partition.type_id,
        });
    }
    partitions.sort_by_key(|partition| partition.number.number());
    Ok(ImagePartitionTable {
        label: parsed.partitiontable.label,
        partitions,
    })
}

fn resolve_root_partition(
    table: Option<&ImagePartitionTable>,
    selected: Option<DiskImagePartition>,
) -> Result<RootPartitionResolution> {
    let Some(table) = table else {
        return if let Some(selected) = selected {
            Err(NspawnError::Validation(format!(
                "Cannot select p{} from an image without a partition table",
                selected.number()
            )))
        } else {
            Ok(RootPartitionResolution {
                partition: None,
                mark_selected_as_root: false,
            })
        };
    };

    if table.partitions.is_empty() {
        return Err(NspawnError::Validation(
            "Disk image partition table contains no partitions".into(),
        ));
    }

    if let Some(selected) = selected {
        if !table
            .partitions
            .iter()
            .any(|partition| partition.number == selected)
        {
            return Err(NspawnError::Validation(format!(
                "Selected root partition p{} does not exist; available partitions: {}",
                selected.number(),
                partition_summary(table)
            )));
        }
        if table.partitions.len() == 1 {
            return Ok(RootPartitionResolution {
                partition: Some(selected),
                mark_selected_as_root: false,
            });
        }
        if !table.label.eq_ignore_ascii_case("gpt") {
            return Err(NspawnError::Validation(
                "Manual root selection for a multi-partition image requires GPT so systemd-nspawn can discover it"
                    .into(),
            ));
        }

        let roots = discoverable_root_partitions(table)?;
        return match roots.as_slice() {
            [] => Ok(RootPartitionResolution {
                partition: Some(selected),
                mark_selected_as_root: true,
            }),
            [root] if *root == selected => Ok(RootPartitionResolution {
                partition: Some(selected),
                mark_selected_as_root: false,
            }),
            [root] => Err(NspawnError::Validation(format!(
                "p{} is already marked as the current architecture root partition; refusing to replace it with p{}",
                root.number(),
                selected.number()
            ))),
            _ => Err(NspawnError::Validation(format!(
                "Image contains multiple current architecture root partitions: {}",
                partition_list(&roots)
            ))),
        };
    }

    if table.partitions.len() == 1 {
        return Ok(RootPartitionResolution {
            partition: Some(table.partitions[0].number),
            mark_selected_as_root: false,
        });
    }

    let roots = discoverable_root_partitions(table)?;
    match roots.as_slice() {
        [root] => Ok(RootPartitionResolution {
            partition: Some(*root),
            mark_selected_as_root: false,
        }),
        [] => Err(NspawnError::Validation(format!(
            "Multi-partition image has no root partition for this architecture; select one in the storage step. Available partitions: {}",
            partition_summary(table)
        ))),
        _ => Err(NspawnError::Validation(format!(
            "Image contains multiple current architecture root partitions: {}",
            partition_list(&roots)
        ))),
    }
}

fn discoverable_root_partitions(table: &ImagePartitionTable) -> Result<Vec<DiskImagePartition>> {
    let root_type = discoverable_root_uuid()?;
    Ok(table
        .partitions
        .iter()
        .filter(|partition| partition.type_id.eq_ignore_ascii_case(root_type))
        .map(|partition| partition.number)
        .collect())
}

fn partition_summary(table: &ImagePartitionTable) -> String {
    table
        .partitions
        .iter()
        .map(|partition| {
            let type_id = if partition.type_id.is_empty() {
                "unknown"
            } else {
                partition.type_id.as_str()
            };
            format!("p{} ({type_id})", partition.number.number())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn partition_list(partitions: &[DiskImagePartition]) -> String {
    partitions
        .iter()
        .map(|partition| format!("p{}", partition.number()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn partition_number_from_node(device: &str, node: &str) -> Result<DiskImagePartition> {
    let suffix = node
        .strip_prefix(device)
        .and_then(|suffix| suffix.strip_prefix('p').or(Some(suffix)))
        .filter(|suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| {
            NspawnError::Validation(format!(
                "Invalid partition node returned by sfdisk: {node} for {device}"
            ))
        })?;
    let number = suffix.parse::<u32>().map_err(|_| {
        NspawnError::Validation(format!(
            "Invalid partition number returned by sfdisk: {node}"
        ))
    })?;
    DiskImagePartition::new(number).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn command_io_error(program: &str, error: std::io::Error) -> NspawnError {
    if error.kind() == std::io::ErrorKind::NotFound {
        NspawnError::ToolNotFound(program.into())
    } else {
        NspawnError::Io(PathBuf::from(program), error)
    }
}

async fn mount_device(device: &Path, mount_point: &Path, runner: &dyn CommandRunner) -> Result<()> {
    validate_block_device(device).await?;
    let output = runner
        .run(
            "mount",
            vec![
                device.to_string_lossy().to_string(),
                mount_point.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("mount"), error))?;
    log_output("mount", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "mount managed image",
            format!("mount {} {}", device.display(), mount_point.display()),
            &output,
        ))
    }
}

async fn attach_loop(image: &Path, runner: &dyn CommandRunner) -> Result<PathBuf> {
    let output = runner
        .run(
            "losetup",
            vec![
                "--find".into(),
                "--partscan".into(),
                "--show".into(),
                image.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("losetup"), error))?;
    log_output("losetup", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "attach managed image loop device",
            format!("losetup --find --partscan --show {}", image.display()),
            &output,
        ));
    }
    let loop_device = parse_loop_device(String::from_utf8_lossy(&output.stdout).trim())?;
    validate_block_device(&loop_device).await?;
    Ok(loop_device)
}

async fn detach_loop(loop_device: &Path, runner: &dyn CommandRunner) -> Result<()> {
    validate_loop_device_path(loop_device)?;
    let output = runner
        .run(
            "losetup",
            vec!["-d".into(), loop_device.to_string_lossy().to_string()],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("losetup"), error))?;
    log_output("losetup -d", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "detach managed image loop device",
            format!("losetup -d {}", loop_device.display()),
            &output,
        ))
    }
}

async fn detach_image_loops(image: &Path, runner: &dyn CommandRunner) -> Result<()> {
    let output = runner
        .run(
            "losetup",
            vec![
                "--json".into(),
                "--list".into(),
                "--associated".into(),
                image.to_string_lossy().to_string(),
                "--output".into(),
                "NAME".into(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("losetup"), error))?;
    log_output("losetup --json --list --associated", &output);
    if !output.status.success() {
        return Ok(());
    }

    for device in parse_losetup_devices(&output.stdout)? {
        detach_loop(&device, runner).await?;
    }
    Ok(())
}

fn parse_losetup_devices(content: &[u8]) -> Result<Vec<PathBuf>> {
    let parsed: LosetupJson = serde_json::from_slice(content)
        .map_err(|error| NspawnError::Runtime(format!("Failed to parse losetup JSON: {error}")))?;
    parsed
        .loopdevices
        .into_iter()
        .map(|device| parse_loop_device(&device.name))
        .collect()
}

async fn settle_udev(runner: &dyn CommandRunner) -> Result<()> {
    let output = runner
        .run("udevadm", vec!["settle".into(), "--timeout=5".into()])
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("udevadm"), error))?;
    log_output("udevadm settle", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "wait for managed image device",
            "udevadm settle --timeout=5",
            &output,
        ))
    }
}

fn parse_loop_device(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    validate_loop_device_path(&path)?;
    Ok(path)
}

fn validate_loop_device_path(path: &Path) -> Result<()> {
    let name = path
        .strip_prefix("/dev")
        .ok()
        .and_then(|relative| relative.to_str())
        .filter(|relative| !relative.contains('/'))
        .ok_or_else(|| {
            NspawnError::Validation(format!(
                "Invalid loop device returned by losetup: {}",
                path.display()
            ))
        })?;
    let digits = name.strip_prefix("loop").ok_or_else(|| {
        NspawnError::Validation(format!(
            "Invalid loop device returned by losetup: {}",
            path.display()
        ))
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(NspawnError::Validation(format!(
            "Invalid loop device returned by losetup: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn validate_mount_source(path: &Path, source: ImageMountSource) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if metadata.file_type().is_symlink() {
        return Err(NspawnError::Validation(format!(
            "Refusing symlink image source: {}",
            path.display()
        )));
    }
    let valid = match source {
        ImageMountSource::Managed(_) => metadata.is_file(),
        ImageMountSource::BlockDevice => metadata.file_type().is_block_device(),
    };
    if !valid {
        return Err(NspawnError::Validation(format!(
            "Invalid typed image source: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn validate_block_device(path: &Path) -> Result<()> {
    if validate_optional_block_device(path).await? {
        Ok(())
    } else {
        Err(NspawnError::Validation(format!(
            "Expected block device does not exist: {}",
            path.display()
        )))
    }
}

async fn validate_optional_block_device(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            Ok(!metadata.file_type().is_symlink() && metadata.file_type().is_block_device())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn prepare_mount_point(machine: &MachineName) -> Result<PathBuf> {
    let path = mount_point(machine);
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(NspawnError::Validation(format!(
                    "Refusing unsafe managed image mount point: {}",
                    path.display()
                )));
            }
            if tokio::fs::read_dir(&path)
                .await
                .map_err(|error| NspawnError::Io(path.clone(), error))?
                .next_entry()
                .await
                .map_err(|error| NspawnError::Io(path.clone(), error))?
                .is_some()
            {
                return Err(NspawnError::Validation(format!(
                    "Managed image mount point is not empty: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(&path)
                .await
                .map_err(|error| NspawnError::Io(path.clone(), error))?;
        }
        Err(error) => return Err(NspawnError::Io(path, error)),
    }
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| NspawnError::Io(path.clone(), error))?;
    Ok(path)
}

async fn validate_optional_mount_point(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(true),
        Ok(_) => Err(NspawnError::Validation(format!(
            "Refusing unsafe managed image mount point: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_mount_point(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

async fn remove_reserved_image(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "Failed to remove partial managed raw image {}: {}",
                path.display(),
                error
            );
        }
    }
}

async fn is_mount_point(path: &Path) -> Result<bool> {
    let mountinfo_path = Path::new("/proc/self/mountinfo");
    let content = tokio::fs::read(mountinfo_path)
        .await
        .map_err(|error| NspawnError::Io(mountinfo_path.to_path_buf(), error))?;
    Ok(mountinfo_contains_path(&content, path))
}

fn mountinfo_contains_path(content: &[u8], path: &Path) -> bool {
    let expected = path.as_os_str().as_bytes();
    content.split(|byte| *byte == b'\n').any(|line| {
        let Some(encoded) = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .nth(4)
        else {
            return false;
        };
        decode_mountinfo_field(encoded).is_some_and(|decoded| decoded == expected)
    })
}

fn decode_mountinfo_field(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] != b'\\' {
            decoded.push(encoded[index]);
            index += 1;
            continue;
        }
        if index + 3 >= encoded.len() {
            return None;
        }
        let digits = &encoded[index + 1..=index + 3];
        if !digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
            return None;
        }
        let value = (digits[0] - b'0') as u16 * 64
            + (digits[1] - b'0') as u16 * 8
            + (digits[2] - b'0') as u16;
        if value > u8::MAX as u16 {
            return None;
        }
        decoded.push(value as u8);
        index += 4;
    }
    Some(decoded)
}

fn raw_image_path(machine: &MachineName) -> PathBuf {
    crate::paths::machine_raw_image(machine.as_str())
}

fn image_source_path(machine: &MachineName, source: ImageMountSource) -> PathBuf {
    match source {
        ImageMountSource::Managed(ManagedImageKind::Raw) => raw_image_path(machine),
        ImageMountSource::Managed(ManagedImageKind::LegacyImg) => {
            crate::paths::machine_image(machine.as_str(), "img")
        }
        ImageMountSource::BlockDevice => PathBuf::from("/dev").join(machine.as_str()),
    }
}

fn mount_point(machine: &MachineName) -> PathBuf {
    crate::paths::machine_image_mount(machine.as_str())
}

fn require_tool(name: &str) -> Result<()> {
    which::which(name)
        .map(|_| ())
        .map_err(|_| NspawnError::ToolNotFound(name.to_string()))
}

fn discoverable_root_uuid() -> Result<&'static str> {
    let uuid = match std::env::consts::ARCH {
        "aarch64" => "B921B045-1DF0-41C3-AF44-4C6F280D3FAE",
        "x86_64" => "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709",
        "x86" => "44479540-F297-41B2-9AF7-D131D5F0458A",
        "arm" => "69DAD710-2CE4-4E3C-B16C-21A1D49ABED3",
        "riscv64" => "1AE5EE25-DDF4-4BD0-8459-24AC0BBE1559",
        architecture => {
            return Err(NspawnError::Validation(format!(
                "No discoverable root partition type is configured for architecture {architecture}"
            )));
        }
    };
    Ok(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;

    fn failed_output(message: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: message.as_bytes().to_vec(),
        }
    }

    fn successful_output(stdout: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn partition(number: u32, type_id: &str) -> ImagePartitionLayout {
        ImagePartitionLayout {
            number: DiskImagePartition::new(number).unwrap(),
            node: PathBuf::from(format!("/dev/loop0p{number}")),
            type_id: type_id.into(),
        }
    }

    fn table(label: &str, partitions: Vec<ImagePartitionLayout>) -> ImagePartitionTable {
        ImagePartitionTable {
            label: label.into(),
            partitions,
        }
    }

    #[test]
    fn loop_device_parser_rejects_command_output_injection() {
        assert_eq!(
            parse_loop_device("/dev/loop12").unwrap(),
            PathBuf::from("/dev/loop12")
        );
        for value in [
            "/dev/loop0p1",
            "/dev/mapper/root",
            "/tmp/loop0",
            "/dev/loop0 --help",
            "loop0",
        ] {
            assert!(parse_loop_device(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn losetup_json_parser_extracts_and_validates_loop_devices() {
        let content = br#"{
            "loopdevices": [
                {"name": "/dev/loop7"},
                {"name": "/dev/loop12"}
            ]
        }"#;
        assert_eq!(
            parse_losetup_devices(content).unwrap(),
            vec![PathBuf::from("/dev/loop7"), PathBuf::from("/dev/loop12")]
        );

        let injected = br#"{
            "loopdevices": [{"name": "/dev/loop0;touch /tmp/pwned"}]
        }"#;
        assert!(parse_losetup_devices(injected).is_err());
        assert!(parse_losetup_devices(br"{}").unwrap().is_empty());
    }

    #[tokio::test]
    async fn detach_image_loops_requests_json_name_output() {
        let image = Path::new("/var/lib/machines/test.raw");
        let mut runner = crate::adapters::process::MockCommandRunner::new();
        let mut sequence = mockall::Sequence::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "losetup"
                    && args.iter().map(String::as_str).eq([
                        "--json",
                        "--list",
                        "--associated",
                        "/var/lib/machines/test.raw",
                        "--output",
                        "NAME",
                    ])
            })
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|_, _| {
                Ok(successful_output(
                    r#"{"loopdevices":[{"name":"/dev/loop7"}]}"#,
                ))
            });
        runner
            .expect_run()
            .withf(|program, args| {
                program == "losetup" && args.iter().map(String::as_str).eq(["-d", "/dev/loop7"])
            })
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|_, _| Ok(successful_output("")));

        detach_image_loops(image, &runner).await.unwrap();
    }

    #[tokio::test]
    async fn failed_unmount_never_detaches_managed_image_loops() {
        let machine = MachineName::new("test").unwrap();
        let mut runner = crate::adapters::process::MockCommandRunner::new();
        runner.expect_run().times(0);

        let error = finish_successful_unmount(
            &machine,
            ImageMountSource::Managed(ManagedImageKind::Raw),
            Some(NspawnError::Runtime("mount remains active".into())),
            &runner,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("mount remains active"));
    }

    #[test]
    fn mountinfo_parser_decodes_escaped_mount_points() {
        let content = b"42 1 0:42 / /run/lasper/mounts/with\\040space\\134suffix rw,relatime - tmpfs tmpfs rw\n43 1 0:43 / /run/lasper/mounts/other rw,relatime - tmpfs tmpfs rw\n";
        assert!(mountinfo_contains_path(
            content,
            Path::new("/run/lasper/mounts/with space\\suffix")
        ));
        assert!(!mountinfo_contains_path(
            content,
            Path::new("/run/lasper/mounts/missing")
        ));
    }

    #[test]
    fn mountinfo_parser_ignores_malformed_fields() {
        let content = b"42 1 0:42 / /run/lasper/mounts/bad\\04 rw,relatime - tmpfs tmpfs rw\n";
        assert!(!mountinfo_contains_path(
            content,
            Path::new("/run/lasper/mounts/bad")
        ));
    }

    #[test]
    fn partition_node_parser_accepts_regular_files_and_partitioned_devices() {
        assert_eq!(
            partition_number_from_node("/tmp/image.raw", "/tmp/image.raw2")
                .unwrap()
                .number(),
            2
        );
        assert_eq!(
            partition_number_from_node("/dev/loop12", "/dev/loop12p7")
                .unwrap()
                .number(),
            7
        );
        for node in [
            "/tmp/image.raw2/escape",
            "/tmp/other.raw2",
            "/tmp/image.raw0",
            "/tmp/image.raw129",
        ] {
            assert!(
                partition_number_from_node("/tmp/image.raw", node).is_err(),
                "accepted {node:?}"
            );
        }
    }

    #[test]
    fn sfdisk_json_is_parsed_without_trusting_partition_nodes() {
        let json = br#"{
            "partitiontable": {
                "label": "gpt",
                "device": "/tmp/image.raw",
                "partitions": [
                    {"node": "/tmp/image.raw1", "type": "generic"},
                    {"node": "/tmp/image.raw2", "type": "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709"}
                ]
            }
        }"#;
        let parsed = parse_sfdisk_partition_table(json).unwrap();
        assert_eq!(parsed.label, "gpt");
        assert_eq!(parsed.partitions[0].number.number(), 1);
        assert_eq!(parsed.partitions[1].number.number(), 2);

        let injected = br#"{
            "partitiontable": {
                "label": "gpt",
                "device": "/tmp/image.raw",
                "partitions": [
                    {"node": "/dev/mapper/host-root", "type": "generic"}
                ]
            }
        }"#;
        assert!(parse_sfdisk_partition_table(injected).is_err());
    }

    #[test]
    fn automatic_root_resolution_uses_single_or_typed_partition() {
        let single = table("gpt", vec![partition(3, "generic")]);
        assert_eq!(
            resolve_root_partition(Some(&single), None)
                .unwrap()
                .partition
                .unwrap()
                .number(),
            3
        );

        let typed = table(
            "gpt",
            vec![
                partition(1, "generic"),
                partition(2, discoverable_root_uuid().unwrap()),
            ],
        );
        assert_eq!(
            resolve_root_partition(Some(&typed), None)
                .unwrap()
                .partition
                .unwrap()
                .number(),
            2
        );
    }

    #[test]
    fn ambiguous_multi_partition_image_requires_manual_selection() {
        let image = table(
            "gpt",
            vec![partition(1, "generic"), partition(2, "generic")],
        );
        let error = resolve_root_partition(Some(&image), None).unwrap_err();
        assert!(error.to_string().contains("select one in the storage step"));

        let selected =
            resolve_root_partition(Some(&image), Some(DiskImagePartition::new(2).unwrap()))
                .unwrap();
        assert_eq!(selected.partition.unwrap().number(), 2);
        assert!(selected.mark_selected_as_root);
    }

    #[test]
    fn manual_selection_does_not_replace_existing_root_partition() {
        let image = table(
            "gpt",
            vec![
                partition(1, discoverable_root_uuid().unwrap()),
                partition(2, "generic"),
            ],
        );
        let error = resolve_root_partition(Some(&image), Some(DiskImagePartition::new(2).unwrap()))
            .unwrap_err();
        assert!(error.to_string().contains("p1 is already marked"));
    }

    #[test]
    fn multi_partition_mbr_cannot_persist_manual_root_selection() {
        let image = table("dos", vec![partition(1, "83"), partition(2, "83")]);
        let error = resolve_root_partition(Some(&image), Some(DiskImagePartition::new(2).unwrap()))
            .unwrap_err();
        assert!(error.to_string().contains("requires GPT"));
    }

    #[test]
    fn loop_fallback_never_masks_multi_partition_dissect_failures() {
        let valid_multi = table(
            "gpt",
            vec![
                partition(1, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
                partition(2, discoverable_root_uuid().unwrap()),
            ],
        );
        assert!(!allows_loop_fallback(Some(&valid_multi), None).unwrap());

        let ambiguous = table(
            "gpt",
            vec![partition(1, "generic"), partition(2, "generic")],
        );
        assert!(allows_loop_fallback(Some(&ambiguous), None).is_err());

        let single = table("gpt", vec![partition(2, "generic")]);
        assert!(allows_loop_fallback(Some(&single), None).unwrap());
        assert!(allows_loop_fallback(None, None).unwrap());
    }

    #[tokio::test]
    async fn manual_selection_marks_only_the_typed_managed_partition() {
        let image = "/var/lib/machines/test.raw";
        let metadata = format!(
            r#"{{
                "partitiontable": {{
                    "label": "gpt",
                    "device": "{image}",
                    "partitions": [
                        {{"node": "{image}1", "type": "generic"}},
                        {{"node": "{image}2", "type": "generic"}}
                    ]
                }}
            }}"#
        );
        let mut runner = crate::adapters::process::MockCommandRunner::new();
        let mut sequence = mockall::Sequence::new();
        let metadata_for_result = metadata.clone();
        runner
            .expect_run()
            .withf(move |program, args| {
                program == "sfdisk" && args.iter().map(String::as_str).eq(["--json", image])
            })
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(move |_, _| Ok(successful_output(&metadata_for_result)));
        runner
            .expect_run()
            .withf(move |program, args| {
                program == "sfdisk"
                    && args.iter().map(String::as_str).eq([
                        "--part-type",
                        image,
                        "2",
                        discoverable_root_uuid().unwrap(),
                    ])
            })
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|_, _| Ok(successful_output("")));

        prepare_selected_root_partition(
            Path::new(image),
            DiskImagePartition::new(2).unwrap(),
            &runner,
        )
        .await
        .unwrap();
    }

    #[test]
    fn sparse_import_preserves_content_and_logical_size() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.raw");
        let destination_path = directory.path().join("destination.raw");
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&source_path)
            .unwrap();
        source.write_all(b"start").unwrap();
        source.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        source.write_all(b"end").unwrap();
        source.seek(SeekFrom::Start(0)).unwrap();
        let mut destination = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .unwrap();

        copy_source(&mut source, &mut destination).unwrap();

        assert_eq!(destination.metadata().unwrap().len(), 1024 * 1024 + 3);
        destination.seek(SeekFrom::Start(0)).unwrap();
        let mut start = [0u8; 5];
        destination.read_exact(&mut start).unwrap();
        assert_eq!(&start, b"start");
        destination.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        let mut end = [0u8; 3];
        destination.read_exact(&mut end).unwrap();
        assert_eq!(&end, b"end");
    }

    #[test]
    fn source_validation_rejects_oversized_regular_images() {
        let source = tempfile::tempfile().unwrap();
        source.set_len(5).unwrap();

        let error = validate_import_source_with_limit(&source, 4).unwrap_err();

        assert!(error.to_string().contains("4 byte limit"));
    }

    #[test]
    fn raw_copy_enforces_the_limit_for_non_sparse_input() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.raw");
        let destination_path = directory.path().join("destination.raw");
        std::fs::write(&source_path, b"0123456789").unwrap();
        let mut source = std::fs::File::open(&source_path).unwrap();
        let mut destination = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .unwrap();

        let error = copy_source_with_limit(&mut source, &mut destination, 4).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        assert_eq!(destination.metadata().unwrap().len(), 0);
    }

    #[test]
    fn reservation_is_exclusive_and_rejects_managed_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("machine.raw");
        let machine = directory.path().join("machine");
        let legacy = directory.path().join("machine.img");

        let reserved = reserve_raw_image(&raw, &[machine.clone(), legacy.clone()]).unwrap();
        assert_eq!(reserved, raw);
        assert!(raw.is_file());
        assert!(reserve_raw_image(&raw, &[machine.clone(), legacy.clone()]).is_err());

        std::fs::remove_file(&raw).unwrap();
        std::fs::create_dir(&machine).unwrap();
        assert!(reserve_raw_image(&raw, &[machine.clone(), legacy.clone()]).is_err());
        std::fs::remove_dir(&machine).unwrap();

        std::fs::write(&legacy, b"legacy").unwrap();
        assert!(reserve_raw_image(&raw, &[machine, legacy]).is_err());
        assert!(!raw.exists());
    }

    #[tokio::test]
    async fn failed_format_removes_partial_managed_image() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.raw");
        let mut runner = crate::adapters::process::MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "mkfs.ext4"
                    && args.first().map(String::as_str) == Some("-F")
                    && args
                        .last()
                        .map(String::as_str)
                        .is_some_and(|target| target.ends_with("test.raw"))
            })
            .times(1)
            .return_once(|_, _| Ok(failed_output("format failed")));

        let result = create_raw_image_at(
            &path,
            &[],
            16 * 1024 * 1024,
            DiskImageFilesystem::Ext4,
            false,
            &runner,
        )
        .await;

        assert!(result.is_err());
        assert!(!path.exists());
    }
}
