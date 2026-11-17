use lasper_guest_protocol::{Architecture, RuntimeIdentity, IDENTITY_FRAME_BYTES};
use std::io::Read;

fn main() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("--artifact-target") {
        return Err("expected --artifact-target <target>".into());
    }
    let target = arguments
        .next()
        .ok_or_else(|| "expected an artifact target".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected artifact verification argument".into());
    }
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
