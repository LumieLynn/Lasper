use lasper_guest_protocol::{Architecture, RuntimeIdentity, IDENTITY_FRAME_BYTES};
use std::io::Read;

fn main() -> Result<(), String> {
    let target = std::env::args()
        .nth(1)
        .ok_or_else(|| "expected a guest runtime target".to_string())?;
    let architecture = match target.as_str() {
        "x86_64-unknown-linux-musl" => Architecture::X86_64,
        "aarch64-unknown-linux-musl" => Architecture::Aarch64,
        _ => return Err(format!("unsupported guest runtime target: {target}")),
    };

    let mut frame = [0; IDENTITY_FRAME_BYTES];
    let mut stdin = std::io::stdin().lock();
    stdin
        .read_exact(&mut frame)
        .map_err(|error| format!("read guest runtime identity: {error}"))?;
    let mut trailing = [0; 1];
    if stdin
        .read(&mut trailing)
        .map_err(|error| format!("check guest runtime identity length: {error}"))?
        != 0
    {
        return Err("guest runtime identity contains trailing bytes".to_string());
    }

    let actual = RuntimeIdentity::decode(&frame)
        .map_err(|error| format!("decode guest runtime identity: {error:?}"))?;
    let expected = RuntimeIdentity::current(architecture);
    if actual != expected {
        return Err(format!(
            "guest runtime identity mismatch: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}
