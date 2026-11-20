//! The program of a process.

/// The program that this process runs: the BLAKE3 digest, in hex, of the bytes
/// of its executable. A process takes it at its start, before a new build can
/// replace the file of its executable.
pub fn executable_program() -> std::io::Result<String> {
    let executable = std::env::current_exe()?;
    let bytes = std::fs::read(executable)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
