//! Ctrl+C for the binaries under test.
//!
//! A Windows process started with Ctrl+C ignored (`SetConsoleCtrlHandler(NULL, TRUE)`, which
//! agent runners and `CREATE_NEW_PROCESS_GROUP` parents set) passes that to every child: the
//! C# REPL's `Console.CancelKeyPress` then never fires when the pseudo-terminal sends `^C`, and
//! a Ctrl+C scenario fails for reasons outside the binary. The harness clears the flag on
//! itself once, before it starts any child. (Raw-mode readers such as crossterm see `^C` as a
//! key either way.)

/// Stop ignoring Ctrl+C in this process, so the children inherit "handle Ctrl+C".
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn stop_ignoring() {
    // SAFETY: SetConsoleCtrlHandler with a null handler and FALSE only clears the process's
    // ignore-Ctrl+C attribute; it takes no pointers and has no other effect. Failure (no
    // console attached) is harmless and ignored.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
}

#[cfg(not(windows))]
pub fn stop_ignoring() {}
