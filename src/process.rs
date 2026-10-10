//! Source-process launch policy shared by metadata and audio downloads.

use std::process::{Command, Stdio};

pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(all(windows, not(debug_assertions)))]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: console tools must not open a window when the
        // release player has no console to inherit. Pipe I/O still works.
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn source_process_has_eof_stdin_and_working_output_pipes() {
        let output = command("cmd.exe")
            .args(["/D", "/C", "set /p input= & echo pipe-ready"])
            .output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("pipe-ready"));
    }
}
