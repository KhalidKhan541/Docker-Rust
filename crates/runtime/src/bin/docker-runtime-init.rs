//! Linux OCI init helper entry point.
//!
//! Usage: `docker-runtime-init --bundle <directory>`.
//! On non-Linux hosts this binary reports an explicit unsupported error; the
//! parent runtime never launches it there.

fn main() {
    #[cfg(target_os = "linux")]
    {
        match run() {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("docker-runtime-init: {err}");
                std::process::exit(125);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "docker-runtime-init: native OCI execution requires Linux; this host is {}",
            std::env::consts::OS
        );
        std::process::exit(125);
    }
}

#[cfg(target_os = "linux")]
fn run() -> docker_errors::Result<i32> {
    use std::path::PathBuf;

    let mut bundle: Option<PathBuf> = None;
    let mut stage2 = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--stage2") => stage2 = true,
            Some("--bundle") => {
                let Some(value) = args.next() else {
                    return Err(docker_errors::Error::invalid_argument(
                        "--bundle requires a directory argument",
                    ));
                };
                bundle = Some(PathBuf::from(value));
            }
            Some("-h" | "--help") => {
                println!("usage: docker-runtime-init [--stage2] --bundle <directory>");
                return Ok(0);
            }
            _ => {
                return Err(docker_errors::Error::invalid_argument(format!(
                    "unknown argument `{}`",
                    arg.to_string_lossy()
                )));
            }
        }
    }

    let bundle = bundle.ok_or_else(|| {
        docker_errors::Error::invalid_argument("missing required --bundle <directory>")
    })?;
    if stage2 {
        docker_runtime::init::run_stage2(&bundle)
    } else {
        docker_runtime::init::run_stage1(&bundle)
    }
}
