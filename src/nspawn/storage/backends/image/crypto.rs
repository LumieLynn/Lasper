//! LUKS encryption logic for disk images.

use std::path::PathBuf;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::utils::new_command;
use super::DiskImageBackend;

impl DiskImageBackend {
    pub(super) async fn format_encrypted(&self, path: &std::path::Path, fs_type: &str, passphrase: &str) -> Result<()> {
        let root_uuid = super::utils::get_discoverable_root_uuid();
        let sfdisk_script = format!("label: gpt\ntype={}\n", root_uuid);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("raw");

        let (dev, loop_to_detach, nbd_to_detach) = if ext == "raw" || ext == "img" {
            // 1. Partition with sfdisk
            let mut child = new_command("sfdisk")
                .arg(path)
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(sfdisk_script.as_bytes()).await?;
            }
            let _ = child.wait().await;

            // 2. Setup loop
            let out = new_command("losetup").args(["--find", "--partition", "--show", &path.to_string_lossy()]).output().await?;
            if !out.status.success() {
                return Err(NspawnError::cmd_failed("losetup", "losetup --find -P --show", &out));
            }
            let loop_dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let part_dev = format!("{}p1", loop_dev);
            
            // Wait for devnodes deterministically
            for _ in 0..10 {
                if std::path::Path::new(&part_dev).exists() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            
            if !std::path::Path::new(&part_dev).exists() {
                if let Some(l) = Some(loop_dev.clone()) { let _ = new_command("losetup").arg("-d").arg(l).output().await; }
                return Err(NspawnError::Generic(format!("Timeout waiting for partition device {}. Ensure loop module supports partitions.", part_dev)));
            }
            
            (part_dev, Some(loop_dev), None)
        } else {
            // Virtual formats
            let _ = new_command("modprobe").args(["nbd", "max_part=16"]).output().await;
            
            // Find a free NBD device
            let nbd_dev = super::utils::find_free_nbd_device().await?
                .ok_or_else(|| NspawnError::Generic("No free NBD devices available".into()))?;

            let out = new_command("qemu-nbd").args(["-c", &nbd_dev, &path.to_string_lossy()]).output().await?;
            if !out.status.success() {
                return Err(NspawnError::cmd_failed("qemu-nbd connect", "qemu-nbd -c", &out));
            }
            
            // Partition
            let mut child = new_command("sfdisk")
                .arg(&nbd_dev)
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(sfdisk_script.as_bytes()).await?;
            }
            
            let out = child.wait_with_output().await?;
            if !out.status.success() {
                // Cleanup
                let _ = new_command("qemu-nbd").args(["-d", &nbd_dev]).output().await;
                return Err(NspawnError::cmd_failed("sfdisk", "sfdisk gpt partition on nbd", &out));
            }
            
            // Wait for partitions deterministically
            let part_dev = format!("{}p1", nbd_dev);
            for _ in 0..10 {
                if std::path::Path::new(&part_dev).exists() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            
            if !std::path::Path::new(&part_dev).exists() {
                let _ = new_command("qemu-nbd").args(["-d", &nbd_dev]).output().await;
                return Err(NspawnError::Generic(format!("Timeout waiting for partition {}. Ensure host 'nbd' module is loaded with max_part > 0.", part_dev)));
            }
            
            (part_dev, None, Some(nbd_dev))
        };

        // 3. luksFormat on the partition
        let mut child = new_command("cryptsetup")
            .args(["luksFormat", "--batch-mode", "--key-file", "-", &dev])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| NspawnError::Io(PathBuf::from("cryptsetup"), e))?;
        
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(passphrase.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        
        let out = child.wait_with_output().await.map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;
        if !out.status.success() {
            if let Some(l) = loop_to_detach { let _ = new_command("losetup").arg("-d").arg(l).output().await; }
            if let Some(n) = nbd_to_detach { let _ = new_command("qemu-nbd").args(["-d", &n]).output().await; }
            return Err(NspawnError::cmd_failed("luksFormat", "cryptsetup luksFormat", &out));
        }

        // 4. Open and format
        let map_name = format!("lasper-temp-{}", uuid::Uuid::new_v4());
        let mut child = new_command("cryptsetup")
            .args(["open", "--type", "luks", "--key-file", "-", &dev, &map_name])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| NspawnError::Io(PathBuf::from("cryptsetup"), e))?;
        
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(passphrase.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        
        let out = child.wait_with_output().await.map_err(|e| NspawnError::Io(path.to_path_buf(), e))?;
        if !out.status.success() {
            if let Some(l) = &loop_to_detach { let _ = new_command("losetup").arg("-d").arg(l).output().await; }
            if let Some(n) = &nbd_to_detach { let _ = new_command("qemu-nbd").args(["-d", &n]).output().await; }
            return Err(NspawnError::cmd_failed("cryptsetup open", "open luks device for formatting", &out));
        }
        
        let dev_path = format!("/dev/mapper/{}", map_name);
        let mkfs = format!("mkfs.{}", fs_type);
        let out = new_command(&mkfs).args(["-F", &dev_path]).output().await?;
        if !out.status.success() {
            let _ = new_command("cryptsetup").arg("close").arg(&map_name).output().await;
            if let Some(l) = &loop_to_detach { let _ = new_command("losetup").arg("-d").arg(l).output().await; }
            if let Some(n) = &nbd_to_detach { let _ = new_command("qemu-nbd").args(["-d", &n]).output().await; }
            return Err(NspawnError::cmd_failed("mkfs", "mkfs inside luks", &out));
        }
        
        let _ = new_command("cryptsetup").arg("close").arg(&map_name).output().await;
        
        // Final cleanup
        if let Some(l) = loop_to_detach { let _ = new_command("losetup").arg("-d").arg(l).output().await; }
        if let Some(n) = nbd_to_detach { let _ = new_command("qemu-nbd").args(["-d", &n]).output().await; }
        
        Ok(())
    }
}
