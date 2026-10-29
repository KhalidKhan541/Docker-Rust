//! Linux exec helper entry point.
//!
//! The JSON payload is read from stdin. The native runtime writes it to a pipe
//! so command arguments and environment values are not exposed in the helper's
//! process command line.

fn main() {
    #[cfg(target_os = "linux")]
    {
        match run() {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("docker-runtime-exec: {error}");
                std::process::exit(125);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "docker-runtime-exec: native exec requires Linux; this host is {}",
            std::env::consts::OS
        );
        std::process::exit(125);
    }
}

#[cfg(target_os = "linux")]
fn run() -> docker_errors::Result<i32> {
    use std::io::Read as _;

    const MAX_SPEC_BYTES: u64 = 4 * 1024 * 1024;
    let mut spec = String::new();
    std::io::stdin()
        .take(MAX_SPEC_BYTES + 1)
        .read_to_string(&mut spec)
        .map_err(|error| {
            docker_errors::Error::invalid_argument(format!("reading exec spec from stdin: {error}"))
        })?;
    if spec.len() as u64 > MAX_SPEC_BYTES {
        return Err(docker_errors::Error::invalid_argument(format!(
            "exec spec exceeds {MAX_SPEC_BYTES} bytes"
        )));
    }
    docker_runtime::exec::run(&spec)
}
