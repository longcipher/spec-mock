use std::{
    net::TcpStream,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

/// A running Prism mock server. Killed on drop.
pub(crate) struct PrismServer {
    child: Child,
    port: u16,
}

/// Locate the Prism CLI. Returns `(program, args_prefix)` where the caller
/// appends `["mock", spec, "--port", port, "--host", "127.0.0.1"]`.
///
/// Search order:
/// 1. `SPECMOCK_PRISM_CMD` env var (treated as a bare executable name / path).
/// 2. `npx @stoplight/prism-cli` (exits 0 when the package is resolvable).
/// 3. `prism` on `$PATH`.
pub(crate) fn find_prism_command() -> Option<(String, Vec<String>)> {
    // 1. Explicit override.
    if let Ok(cmd) = std::env::var("SPECMOCK_PRISM_CMD") &&
        !cmd.is_empty()
    {
        return Some((cmd, vec![]));
    }

    // 2. npx @stoplight/prism-cli
    if Command::new("npx")
        .args(["@stoplight/prism-cli", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Some(("npx".to_owned(), vec!["@stoplight/prism-cli".to_owned()]));
    }

    // 3. bare `prism` on PATH
    if Command::new("prism").arg("--version").output().is_ok_and(|o| o.status.success()) {
        return Some(("prism".to_owned(), vec![]));
    }

    None
}

/// Bind to `127.0.0.1:0` to let the OS pick a free port, then immediately
/// close the socket and return that port number.  There is an inherent TOCTOU
/// race but it is acceptable for test infrastructure.
pub(crate) fn find_available_port() -> Option<u16> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

impl PrismServer {
    /// Start Prism against `spec_path` and block until it is ready (TCP open)
    /// or until 10 seconds elapse.  Returns `None` when Prism is not installed
    /// or the server fails to become ready in time.
    pub(crate) fn start(spec_path: &Path) -> Option<Self> {
        let (program, mut prefix_args) = find_prism_command()?;
        let port = find_available_port()?;

        prefix_args.extend([
            "mock".to_owned(),
            spec_path.to_string_lossy().into_owned(),
            "--port".to_owned(),
            port.to_string(),
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
        ]);

        let child = Command::new(&program)
            .args(&prefix_args)
            // Suppress Prism's stdout/stderr so test output stays clean.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        // Poll TCP until the port is open (max 10 s).
        let deadline = Instant::now() + Duration::from_secs(10);
        let addr = format!("127.0.0.1:{port}");
        let ready = loop {
            if TcpStream::connect(&addr).is_ok() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(100));
        };

        if !ready {
            #[expect(clippy::print_stderr, reason = "test infrastructure logging")]
            {
                eprintln!("[prism] TCP port {port} never became ready within 10 s");
            }
            return None;
        }

        // Extra settle delay so HTTP routing initialises.
        thread::sleep(Duration::from_millis(300));

        Some(Self { child, port })
    }

    /// Base URL of the running server, e.g. `http://127.0.0.1:4010`.
    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The TCP port Prism is listening on.
    #[expect(dead_code, reason = "used in Tasks 2.x and 3.1")]
    pub(crate) const fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PrismServer {
    fn drop(&mut self) {
        // Best-effort kill; ignore errors.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
