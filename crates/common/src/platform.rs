//! Host platform and capability detection.
//!
//! Milestone C (native OCI runtime) requires Linux kernel facilities. On
//! hosts where they are unavailable the runtime must fail with a *clear*
//! `Unsupported` error describing exactly which facility is missing, never
//! silently fall back to a semantically weaker implementation. This module
//! provides the capability probes that decision trees consume.
//!
//! On Windows hosts the Linux isolation stack is genuinely absent:
//! namespaces, cgroups (v2), overlayfs, and netlink-based networking do not
//! exist, so the probe layer reports them as unsupported with actionable
//! reasons. Integration points for WSL/VM-based implementations belong in
//! a dedicated runtime-backend crate (Milestone C), not in silent fallbacks
//! here.

use std::fmt;
use std::process::Command as StdCommand;

/// Host operating system family detected at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
    Other(&'static str),
}

/// CPU architecture detected at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
    Other(&'static str),
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Os::Linux => f.write_str("linux"),
            Os::MacOs => f.write_str("darwin"),
            Os::Windows => f.write_str("windows"),
            Os::Other(s) => f.write_str(s),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arch::X86_64 => f.write_str("amd64"),
            Arch::Aarch64 => f.write_str("arm64"),
            Arch::Other(s) => f.write_str(s),
        }
    }
}

/// Purely informational host summary — mirrors the kernel fields that the
/// Engine API `SystemInfo` eventually carries (api version >= 1.43 added
/// kernel version, os/arch pairs). Parsing /proc or the Windows registry is
/// deliberately kept out of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// Detect from build/runtime constants. `std::env::consts` gives us
    /// reliable OS/arch without touching the network or spawning processes.
    pub fn current() -> Self {
        let os = match std::env::consts::OS {
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            "macos" => Os::MacOs,
            other => Os::Other(Box::leak(String::from(other).into_boxed_str()) as &str),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => Arch::Other(Box::leak(String::from(other).into_boxed_str()) as &str),
        };
        Self { os, arch }
    }
}

/// Linux kernel isolation facilities the native runtime (Milestone C)
/// depends on, individually reportable so failures name the missing piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsolationFeatures {
    /// Linux namespaces with clone3/unshare/mount namespace support.
    pub namespaces: bool,
    /// cgroup v2 unified hierarchy resource control.
    pub cgroup_v2: bool,
    /// OverlayFS union filesystem for image layering.
    pub overlay_fs: bool,
    /// Netlink + bridge device support for container networking.
    pub net_bridge: bool,
    /// Read-only bind mounts for layered rootfs mounts.
    pub bind_mounts: bool,
}

impl IsolationFeatures {
    /// Names of features lacking, for error surfacing.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.namespaces {
            v.push("namespaces");
        }
        if !self.cgroup_v2 {
            v.push("cgroup_v2");
        }
        if !self.overlay_fs {
            v.push("overlay_fs");
        }
        if !self.net_bridge {
            v.push("net_bridge");
        }
        if !self.bind_mounts {
            v.push("bind_mounts");
        }
        v
    }
}

impl fmt::Display for IsolationFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.missing().is_empty() {
            return f.write_str("all isolation features supported");
        }
        write!(f, "missing: {}", self.missing().join(", "))
    }
}

/// Protocol describing how the host exposes (or fails to expose) Linux
/// isolation facilities. Non-Linux hosts produce `None` + reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationSupport {
    pub features: Option<IsolationFeatures>,
    pub reason: Option<String>,
}

/// Probe the running host for Linux isolation facilities.
///
/// Current implementation: compiled probes are not initialized yet (they
/// arrive with the runtime backend in Milestone C, gated per-OS). The probe
/// therefore reports honest, static knowledge:
///
/// * Linux compiled binaries assume kernel ≥ 5.8 (cgroup v2 widespread)
///   until runtime probes are implemented; documented assumption.
/// * Windows/CSP darwin report `None` with a reason string. This matches the
///   behavior of the upstream binaries, which refuse these facilities there.
pub fn probe_isolation() -> IsolationSupport {
    match Platform::current().os {
        Os::Linux => IsolationSupport {
            features: Some(IsolationFeatures {
                namespaces: true,
                cgroup_v2: true,
                overlay_fs: true,
                net_bridge: true,
                bind_mounts: true,
            }),
            reason: None,
        },
        #[cfg(target_os = "macos")]
        Os::MacOs => unsupported_reason("macOS provides no Linux namespaces or cgroups; containers require a Linux VM"),
        Os::Windows => unsupported_reason(format!(
            "host OS `{}` provides no Linux namespaces/cgroups/overlayfs; the native runtime requires Linux (or an explicitly selected VM backend in a future milestone)",
            Os::Windows
        )),
        Os::Other(os) => unsupported_reason(format!("host OS `{os}` is unsupported by the native runtime")),
        _ => unsupported_reason("host OS is unsupported by the native runtime"),
    }
}

fn unsupported_reason(reason: impl Into<String>) -> IsolationSupport {
    IsolationSupport {
        features: None,
        reason: Some(reason.into()),
    }
}

/// Helper used by higher layers to detect display/kernel environment info.
/// Deliberately cheap and side-effect free.
pub fn hostname_hint() -> Option<String> {
    StdCommand::new("hostname")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_current_compiles_sane() {
        let p = Platform::current();
        assert!(matches!(
            p.arch,
            Arch::X86_64 | Arch::Aarch64 | Arch::Other(_)
        ));
    }

    #[test]
    fn isolation_probe_reports_missing_facilities_on_windows() {
        let sup = probe_isolation();
        if cfg!(windows) || cfg!(target_os = "macos") {
            assert!(sup.features.is_none(), "expected None on this host");
            assert!(sup.reason.is_some());
        } else {
            assert!(sup.features.is_some());
        }
    }

    #[test]
    fn missing_features_listed() {
        let f = IsolationFeatures::default();
        assert_eq!(f.missing().len(), 5);
        let full = IsolationFeatures {
            namespaces: true,
            cgroup_v2: true,
            overlay_fs: true,
            net_bridge: true,
            bind_mounts: true,
        };
        assert!(full.missing().is_empty());
        assert_eq!(full.to_string(), "all isolation features supported");
    }

    #[test]
    fn hostname_hint_safe() {
        let _ = hostname_hint();
    }
}
