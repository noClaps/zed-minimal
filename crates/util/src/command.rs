use std::ffi::OsStr;

pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}

pub fn new_smol_command(program: impl AsRef<OsStr>) -> smol::process::Command {
    smol::process::Command::new(program)
}
