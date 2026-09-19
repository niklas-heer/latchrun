//! Safe terminal mode restoration for the foreground CLI.
use crate::protocol::Failure;
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use std::io;

pub struct RawMode(Termios);

impl RawMode {
    pub fn enable() -> Result<Self, Failure> {
        let original = tcgetattr(io::stdin())
            .map_err(|_| Failure::new("terminal", "TTY mode requires a terminal on stdin."))?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(io::stdin(), SetArg::TCSANOW, &raw)
            .map_err(|_| Failure::new("terminal", "Could not configure the terminal."))?;
        Ok(Self(original))
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &self.0);
    }
}

pub fn size() -> (u16, u16) {
    terminal_size::terminal_size().map_or(
        (24, 80),
        |(terminal_size::Width(cols), terminal_size::Height(rows))| (rows.max(1), cols.max(1)),
    )
}
