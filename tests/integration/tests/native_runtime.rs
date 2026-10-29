//! Linux-only end-to-end execution test for the native runtime.
//!
//! This intentionally avoids Docker Hub and a real base image. It builds a
//! static syscall-only ELF probe with the host C compiler, packages it as an
//! OCI layer, materializes the rootfs through the production image-store path,
//! and executes it through the native namespace/chroot runtime.
//!
//! Run as root on Linux:
//!
//! ```text
//! cargo build --workspace
//! sudo env PATH="$PATH" cargo test -p docker-rs-integration \
//!   --test native_runtime -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use docker_common::Digest;
use docker_container_model::config::{ContainerConfig, HostConfig, NetworkMode, Signal};
use docker_image::{Descriptor, Image, Platform, RootFS};
use docker_image_store::{materialize_rootfs, populate_standard_devices, ImageRecord, ImageStore};
use docker_runtime::{ExecConfig, OciRuntime, Runtime, RuntimeConfig};
use tar::{Builder, EntryType, Header};

const PROBE_OUTPUT: &str = "native-runtime-ok";
const EXEC_OUTPUT: &str = "exec-runtime-ok";

#[test]
#[ignore = "requires root on Linux and a built docker-runtime-init"]
fn starts_a_real_process_in_a_materialized_rootfs() {
    if !nix::unistd::Uid::effective().is_root() {
        eprintln!("skipping native runtime test: not root");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let probe = build_static(temp.path(), "probe", PROBE_SOURCE);
    let probe_bytes = fs::read(&probe).unwrap();
    let exec_probe = build_static(temp.path(), "exec-probe", EXEC_PROBE_SOURCE);
    let exec_probe_bytes = fs::read(&exec_probe).unwrap();
    let sleeper = build_static(temp.path(), "sleeper", SLEEPER_SOURCE);
    let sleeper_bytes = fs::read(&sleeper).unwrap();

    let store = ImageStore::open(temp.path().join("images")).unwrap();
    let raw_layer = tar_with_files(&[
        ("probe", &probe_bytes, 0o755),
        ("exec-probe", &exec_probe_bytes, 0o755),
        ("sleeper", &sleeper_bytes, 0o755),
    ]);
    let diff_id = Digest::sha256(&raw_layer);
    let layer_digest = Digest::sha256(&raw_layer);
    store
        .blob_store()
        .put_verified(&layer_digest, &raw_layer)
        .unwrap();
    let record = ImageRecord {
        id: Digest::sha256(b"native-runtime-probe-config"),
        manifest_digest: Digest::sha256(b"native-runtime-probe-manifest"),
        manifest_digests: Vec::new(),
        tags: BTreeSet::new(),
        repo_digests: BTreeSet::new(),
        created: 1_700_000_000,
        pulled_at: 1_700_000_000,
        size: raw_layer.len() as i64,
        config_size: 0,
        platform: Platform {
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            ..Platform::default()
        },
        config: Image {
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            rootfs: RootFS::layers(vec![diff_id]),
            ..Image::default()
        },
        layers: vec![Descriptor {
            media_type: docker_image::MEDIA_TYPE_IMAGE_LAYER.to_string(),
            digest: layer_digest,
            size: raw_layer.len() as i64,
            ..Descriptor::default()
        }],
    };
    store.insert(record.clone()).unwrap();

    let rootfs = temp.path().join("rootfs");
    materialize_rootfs(&store, &record, &rootfs).unwrap();
    populate_standard_devices(&rootfs).unwrap();
    assert!(rootfs.join("probe").is_file());

    let init = init_program();
    let runtime = OciRuntime::new(temp.path().join("runtime"))
        .with_init_program(&init)
        .with_capture_stdio(true);
    let id = docker_common::ContainerId::generate().unwrap();
    let config = ContainerConfig {
        image: docker_common::ImageRef::parse("native-probe:latest").unwrap(),
        entrypoint: vec!["/probe".to_string()],
        cmd: Vec::new(),
        env: Vec::new(),
        labels: Default::default(),
        working_dir: None,
        user: None,
        hostname: None,
        tty: false,
        open_stdin: false,
        stop_signal: Signal::Sigterm,
        stop_timeout_secs: 10,
        exposed_ports: Vec::new(),
    };
    let host_config = HostConfig {
        network_mode: NetworkMode::None,
        ..HostConfig::default()
    };
    let task = runtime
        .create(
            &RuntimeConfig::new(id, config.clone(), host_config.clone())
                .with_rootfs(rootfs.as_path()),
        )
        .unwrap();
    task.start().unwrap();
    let mut stdio = task.take_stdio().unwrap().expect("captured stdio");
    let status = task.wait().unwrap();
    // Drain both pipes before asserting so the init helper's own panic
    // output reaches the test log when the container fails to start.
    let mut stdout = Vec::new();
    if let Some(pipe) = stdio.stdout.as_mut() {
        pipe.read_to_end(&mut stdout).unwrap();
    }
    let mut stderr = Vec::new();
    if let Some(pipe) = stdio.stderr.as_mut() {
        pipe.read_to_end(&mut stderr).unwrap();
    }
    task.delete().unwrap();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    assert!(
        status.success(),
        "probe exited with {status:?}\nstdout: {stdout:?}\nstderr: {stderr:?}"
    );
    assert!(stdout.contains(PROBE_OUTPUT), "probe output was {stdout:?}");

    // Start a long-lived task and run a second command inside its namespaces.
    // This is the runtime half of `docker exec`; it verifies namespace entry,
    // chroot, numeric user switching, and the helper's captured stdio.
    let runner_id = docker_common::ContainerId::generate().unwrap();
    let mut runner_config = config.clone();
    runner_config.entrypoint = vec!["/sleeper".to_string()];
    runner_config.user = Some("1000:1000".to_string());
    let runner = runtime
        .create(
            &RuntimeConfig::new(runner_id, runner_config, host_config)
                .with_rootfs(rootfs.as_path()),
        )
        .unwrap();
    runner.start().unwrap();
    wait_for_container_init(runner.pid().expect("runner pid"));
    let exec = runner
        .exec(&ExecConfig {
            argv: vec!["/exec-probe".to_string()],
            env: vec!["PATH=/bin".to_string()],
            cwd: "/".to_string(),
            user: Some("1000:1000".to_string()),
            capture_stdio: true,
        })
        .unwrap();
    exec.start().unwrap();
    let mut exec_stdio = exec.take_stdio().unwrap().expect("exec stdio");
    let exec_status = exec.wait().unwrap();
    let mut exec_stdout = Vec::new();
    if let Some(pipe) = exec_stdio.stdout.as_mut() {
        pipe.read_to_end(&mut exec_stdout).unwrap();
    }
    let mut exec_stderr = Vec::new();
    if let Some(pipe) = exec_stdio.stderr.as_mut() {
        pipe.read_to_end(&mut exec_stderr).unwrap();
    }
    exec.delete().unwrap();
    let exec_stdout = String::from_utf8_lossy(&exec_stdout).into_owned();
    let exec_stderr = String::from_utf8_lossy(&exec_stderr).into_owned();
    assert!(
        exec_status.success(),
        "exec exited with {exec_status:?}\nstdout: {exec_stdout:?}\nstderr: {exec_stderr:?}"
    );
    assert!(
        exec_stdout.contains(EXEC_OUTPUT),
        "exec output was {exec_stdout:?}"
    );

    runner.kill(Signal::Sigkill).unwrap();
    runner.delete().unwrap();
}

fn wait_for_container_init(pid: i32) {
    let children = PathBuf::from(format!("/proc/{pid}/task/{pid}/children"));
    for _ in 0..200 {
        let observed = fs::read_to_string(&children).unwrap_or_default();
        if !observed.trim().is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("container stage 1 (pid {pid}) did not fork its PID-namespace init");
}

fn init_program() -> PathBuf {
    if let Some(path) = std::env::var_os(docker_runtime::oci::RUNTIME_INIT_ENV) {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "{} points at missing file `{}`",
            docker_runtime::oci::RUNTIME_INIT_ENV,
            path.display()
        );
        return path;
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join("docker-runtime-init");
    assert!(
        path.is_file(),
        "runtime init helper `{}` is missing; run `cargo build --workspace` or set DRC_RUNTIME_INIT",
        path.display()
    );
    path
}

const PROBE_SOURCE: &str = r#"
.global _start
.section .text
_start:
    mov $1, %rax
    mov $1, %rdi
    lea message(%rip), %rsi
    mov $message_len, %rdx
    syscall
    mov $60, %rax
    xor %rdi, %rdi
    syscall
.section .rodata
message:
    .ascii "native-runtime-ok\n"
.set message_len, . - message
"#;

const EXEC_PROBE_SOURCE: &str = r#"
.global _start
.section .text
_start:
    mov $1, %rax
    mov $1, %rdi
    lea message(%rip), %rsi
    mov $message_len, %rdx
    syscall
    mov $60, %rax
    xor %rdi, %rdi
    syscall
.section .rodata
message:
    .ascii "exec-runtime-ok\n"
.set message_len, . - message
"#;

const SLEEPER_SOURCE: &str = r#"
.global _start
.section .text
_start:
    mov $34, %rax
    syscall
    jmp _start
"#;

fn build_static(directory: &Path, name: &str, source: &str) -> PathBuf {
    let source_path = directory.join(format!("{name}.S"));
    let binary = directory.join(name);
    fs::write(&source_path, source).unwrap();
    let output = Command::new("cc")
        .arg("-nostdlib")
        .arg("-static")
        .arg("-no-pie")
        .arg("-o")
        .arg(&binary)
        .arg(&source_path)
        .output()
        .expect("the Linux test requires a C compiler (`cc`)");
    assert!(
        output.status.success(),
        "compiling static probe {name} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

fn tar_with_files(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for (path, data, mode) in files {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder.append_data(&mut header, *path, *data).unwrap();
    }
    let mut bytes = builder.into_inner().unwrap();
    bytes.extend_from_slice(&[0u8; 1024]);
    bytes
}
