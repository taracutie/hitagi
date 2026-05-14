use std::io::Write;

unsafe extern "C" {
    /// libc `_exit(2)` ~ terminates the process without running any
    /// `atexit` handlers, glibc destructors, or stdio flushers. We do our
    /// own stdout/stderr flush before calling this; everything else
    /// (rayon thread-pool teardown, allocator destructors, Rust statics)
    /// is intentionally skipped because the OS reclaims the same
    /// resources on process exit and the cleanup otherwise adds ~20-30 ms
    /// per CLI invocation.
    fn _exit(status: i32) -> !;
}

fn main() -> ! {
    let code = match mimi::cli::run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    };
    // Flush stdio explicitly ~ `_exit` skips libc's stdio flush, so we
    // would otherwise drop the trailing characters of the response.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Safety: we're at the tail of `main`, no further Rust code runs.
    // Library callers never go through this `main`, so the CLI's
    // skipped-cleanup policy doesn't affect embedded use.
    unsafe { _exit(code) }
}
