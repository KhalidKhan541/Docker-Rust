//! Image-to-rootfs provider used by the native OCI runtime.
//!
//! The provider owns the on-disk layout under `<data-root>/containers/<id>`:
//!
//! ```text
//! containers/<id>/
//!   rootfs/          complete, atomically renamed root filesystem
//!   rootfs.ready     marker written after the rename
//! ```
//!
//! A root filesystem is materialized into a sibling staging directory and
//! renamed into place only after every layer and the standard `/dev` nodes
//! have been applied. A crash therefore leaves either no `rootfs/` or a
//! complete one; a `rootfs/` without the marker is treated as incomplete and
//! replaced.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use docker_common::{ContainerId, Digest};
use docker_container_model::config::ContainerConfig;
use docker_errors::{Error, Result};
use docker_image_store::{materialize_rootfs, populate_standard_devices, ImageStore};
use docker_runtime::RootfsProvider;

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ImageRootfsProvider {
    images: Arc<ImageStore>,
    containers_root: PathBuf,
}

impl ImageRootfsProvider {
    pub(crate) fn new(images: Arc<ImageStore>, data_root: &Path) -> Self {
        Self {
            images,
            containers_root: data_root.join("containers"),
        }
    }

    fn container_dir(&self, id: &ContainerId) -> PathBuf {
        self.containers_root.join(id.as_str())
    }

    fn rootfs_path(&self, id: &ContainerId) -> PathBuf {
        self.container_dir(id).join("rootfs")
    }

    fn ready_path(&self, id: &ContainerId) -> PathBuf {
        self.container_dir(id).join("rootfs.ready")
    }
}

impl RootfsProvider for ImageRootfsProvider {
    fn prepare(
        &self,
        id: &ContainerId,
        config: &ContainerConfig,
        image_id: Option<&Digest>,
    ) -> Result<PathBuf> {
        let container_dir = self.container_dir(id);
        fs::create_dir_all(&container_dir).map_err(|error| {
            Error::internal(format!(
                "creating container directory `{}`: {error}",
                container_dir.display()
            ))
        })?;

        // Resolve the pinned image ID first. Falling back to the tag is only
        // for callers that created a record before image IDs were stored.
        let image_reference = image_id
            .map(Digest::full)
            .unwrap_or_else(|| config.image.to_string());
        let image = self.images.resolve(&image_reference)?;
        let expected = image.id.full();

        let rootfs = self.rootfs_path(id);
        let ready = self.ready_path(id);
        if is_complete_rootfs(&rootfs, &ready, &expected)? {
            return Ok(rootfs);
        }
        remove_incomplete_rootfs(&rootfs, &ready)?;

        let staging = container_dir.join(format!(
            ".rootfs-{}-{}",
            std::process::id(),
            STAGING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::internal(format!(
                    "removing stale staging rootfs `{}`: {error}",
                    staging.display()
                )))
            }
        }

        let result = (|| {
            materialize_rootfs(&self.images, &image, &staging)?;
            populate_standard_devices(&staging)?;
            fs::rename(&staging, &rootfs).map_err(|error| {
                Error::internal(format!(
                    "installing rootfs `{}` as `{}`: {error}",
                    staging.display(),
                    rootfs.display()
                ))
            })?;
            fs::write(&ready, format!("{expected}\n")).map_err(|error| {
                Error::internal(format!(
                    "writing rootfs completion marker `{}`: {error}",
                    ready.display()
                ))
            })?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        Ok(rootfs)
    }

    fn cleanup(&self, id: &ContainerId) -> Result<()> {
        let directory = self.container_dir(id);
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::internal(format!(
                "removing container rootfs `{}`: {error}",
                directory.display()
            ))),
        }
    }
}

fn is_complete_rootfs(rootfs: &Path, ready: &Path, expected_image_id: &str) -> Result<bool> {
    let metadata = match fs::symlink_metadata(rootfs) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(Error::internal(format!(
                "inspecting container rootfs `{}`: {error}",
                rootfs.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::conflict(format!(
            "container rootfs `{}` exists but is not a directory",
            rootfs.display()
        )));
    }
    let marker = match fs::read_to_string(ready) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(Error::internal(format!(
                "reading rootfs completion marker `{}`: {error}",
                ready.display()
            )))
        }
    };
    Ok(marker.trim() == expected_image_id)
}

fn remove_incomplete_rootfs(rootfs: &Path, ready: &Path) -> Result<()> {
    match fs::remove_file(ready) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::internal(format!(
                "removing stale rootfs marker `{}`: {error}",
                ready.display()
            )))
        }
    }
    match fs::remove_dir_all(rootfs) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::internal(format!(
            "removing incomplete container rootfs `{}`: {error}",
            rootfs.display()
        ))),
    }
}
