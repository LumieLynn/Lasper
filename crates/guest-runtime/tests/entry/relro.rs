use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn reject_unaligned_relro(binary: &str) {
    let mut artifact = Artifact::new(binary);
    let size = artifact.relro_u64(40);
    artifact.set_relro_u64(40, size - 1);
    artifact.assert_rejected("RELRO end is not aligned");
}

fn reject_writable_prefix(binary: &str) {
    let mut artifact = Artifact::new(binary);
    let address = artifact.relro_u64(16);
    let size = artifact.relro_u64(40);
    // Keep the aligned end, but exclude the first byte of the writable LOAD
    // from RELRO. Protecting its first page would also cover that byte.
    artifact.set_relro_u64(16, address + 1);
    artifact.set_relro_u64(40, size - 1);
    artifact.assert_rejected("RELRO page contains writable data outside RELRO");
}

fn runtime_rejects_unaligned_relro(binary: &str) {
    let mut artifact = Artifact::new(binary);
    let size = artifact.relro_u64(40);
    artifact.set_relro_u64(40, size - 1);
    // Execute directly so the runtime check cannot be supplied by the CI script.
    let output = Command::new(artifact.write())
        .arg("--identify")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "artifact check rejects incomplete RELRO page padding",
        reject_unaligned_relro,
    ),
    (
        "artifact check rejects writable data sharing the RELRO page",
        reject_writable_prefix,
    ),
    (
        "standalone entry refuses an unaligned RELRO end before processing arguments",
        runtime_rejects_unaligned_relro,
    ),
];

struct Artifact {
    directory: PathBuf,
    target: &'static str,
    bytes: Vec<u8>,
    relro: usize,
}

impl Artifact {
    fn new(binary: &str) -> Self {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(binary);
        let bytes = fs::read(source).unwrap();
        assert_eq!(&bytes[..6], b"\x7fELF\x02\x01");
        let target = match u16::from_le_bytes(bytes[18..20].try_into().unwrap()) {
            62 => "x86_64-unknown-linux-musl",
            183 => "aarch64-unknown-linux-musl",
            machine => panic!("unexpected ELF machine {machine}"),
        };
        let table = u64_at(&bytes, 32) as usize;
        let stride = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
        assert_eq!(stride, 56);
        let relro = (0..count)
            .map(|index| table + index * stride)
            .find(|offset| {
                u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap()) == 0x6474_e552
            })
            .unwrap();
        let directory = std::env::temp_dir().join(format!(
            "lasper-relro-check-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        Self {
            directory,
            target,
            bytes,
            relro,
        }
    }

    fn relro_u64(&self, field: usize) -> u64 {
        u64_at(&self.bytes, self.relro + field)
    }

    fn set_relro_u64(&mut self, field: usize, value: u64) {
        let offset = self.relro + field;
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn write(&self) -> PathBuf {
        let path = self.directory.join(format!(
            "target/{}/guest-runtime/lasper-guest-runtime",
            self.target
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &self.bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn assert_rejected(&self, reason: &str) {
        self.write();
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/scripts/verify-guest-runtime.sh");
        let output = Command::new(script)
            .arg(self.target)
            .current_dir(&self.directory)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "{stderr}");
    }
}

impl Drop for Artifact {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
