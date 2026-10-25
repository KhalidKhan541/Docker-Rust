//! Materialize a stored image into a container root filesystem.
//!
//! Docker and OCI image layers are tar streams, optionally compressed, that
//! are applied bottom-most first. This module implements the layer subset the
//! native runtime can execute:
//!
//! * regular files, directories, symlinks, hardlinks, FIFOs, and device nodes;
//! * OCI/Docker whiteouts (`.wh.<name>`) and opaque directories
//!   (`.wh..wh..opq`);
//! * gzip-compressed and uncompressed layers (zstd is rejected explicitly);
//! * POSIX ownership, mode, mtime, and `SCHILY.xattr.*` attributes.
//!
//! Security is part of the contract, not an afterthought: layer paths are
//! normalized without accepting absolute paths or `..`, extraction never
//! follows a symlink that was created by an earlier layer (an image that
//! requires that is rejected rather than silently extracted with different
//! semantics), whiteout markers cannot escape their directory, and whiteout
//! deletion does not follow symlinks. The caller is expected to extract into
//! a fresh temporary directory and rename it into place.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use docker_common::Digest;
use docker_errors::{Error, Result};
use flate2::read::GzDecoder;
use sha2::{Digest as _, Sha256};
use tar::{Archive, EntryType};

use crate::store::{ImageRecord, ImageStore};

/// Maximum number of entries accepted from a single layer.
const MAX_LAYER_ENTRIES: usize = 1_000_000;
/// Maximum number of entries accepted across every layer of one image.
const MAX_TOTAL_ENTRIES: usize = 2_000_000;
/// Maximum number of layers materialized for one container.
const MAX_LAYERS: usize = 512;
/// Maximum bytes written for file contents in one rootfs.
const MAX_ROOTFS_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Maximum bytes read from one compressed or uncompressed layer stream.
const MAX_LAYER_BYTES: u64 = 128 * 1024 * 1024 * 1024;

/// Materialize every layer in `record` under `destination`.
///
/// `destination` must be absolute, must not exist or be an empty directory,
/// and must not be a symlink. The function does not roll back partial output;
/// callers that need atomicity extract into a temporary directory.
pub fn materialize_rootfs(
    store: &ImageStore,
    record: &ImageRecord,
    destination: &Path,
) -> Result<()> {
    record.validate()?;
    if !destination.is_absolute() {
        return Err(Error::invalid_argument(format!(
            "rootfs destination `{}` must be an absolute path",
            destination.display()
        )));
    }
    prepare_destination(destination)?;
    if record.layers.len() > MAX_LAYERS {
        return Err(Error::invalid_argument(format!(
            "image {} has {} layers; the native runtime supports at most {MAX_LAYERS}",
            record.id.full(),
            record.layers.len()
        )));
    }

    let mut directories: BTreeMap<PathBuf, DirectoryMetadata> = BTreeMap::new();
    let mut totals = Totals::default();
    for (index, (layer, diff_id)) in record
        .layers
        .iter()
        .zip(record.config.rootfs.diff_ids.iter())
        .enumerate()
    {
        apply_layer(
            store,
            layer,
            diff_id,
            destination,
            &mut directories,
            &mut totals,
        )
        .map_err(|error| {
            Error::of_kind(
                error.kind(),
                format!(
                    "applying layer {} ({}) of image {}: {error}",
                    index + 1,
                    layer.digest.full(),
                    record.id.full()
                ),
            )
        })?;
    }

    // Apply directory metadata from the deepest path first. This mirrors
    // Docker's archive extraction: a read-only parent must not prevent files
    // in a child from being created.
    for (relative, metadata) in directories.into_iter().rev() {
        let path = join_relative(destination, &relative);
        let current = match fs::symlink_metadata(&path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("inspecting extracted directory", &path, error)),
        };
        if current.file_type().is_symlink() || !current.is_dir() {
            continue;
        }
        apply_metadata(&path, &metadata, false)?;
    }

    Ok(())
}

/// Create the standard `/dev` layout inside a freshly materialized rootfs.
///
/// The native runtime currently does not mount a private `/dev` tmpfs, so
/// these nodes are persisted in the container rootfs. The daemon must run as
/// root (checked by `OciRuntime::preflight`).
#[cfg(target_os = "linux")]
pub fn populate_standard_devices(root: &Path) -> Result<()> {
    use nix::errno::Errno;
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};

    prepare_directory(root, Path::new("dev"), true)?;
    let dev = root.join("dev");

    let devices: &[(&str, u64, u64, u32)] = &[
        ("null", 1, 3, 0o666),
        ("zero", 1, 5, 0o666),
        ("full", 1, 7, 0o666),
        ("random", 1, 8, 0o666),
        ("urandom", 1, 9, 0o666),
        ("tty", 5, 0, 0o666),
        ("console", 5, 1, 0o600),
        ("ptmx", 5, 2, 0o666),
    ];
    for (name, major, minor, mode) in devices {
        let path = dev.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspecting device node", &path, error)),
        }
        mknod(
            &path,
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(*mode),
            makedev(*major, *minor),
        )
        .map_err(|error| {
            if error == Errno::EPERM {
                Error::permission_denied(format!(
                    "creating device node `{}` requires CAP_MKNOD",
                    path.display()
                ))
            } else {
                Error::internal(format!(
                    "creating device node `{}`: {error}",
                    path.display()
                ))
            }
        })?;
        set_mode(&path, *mode)?;
    }

    prepare_directory(root, Path::new("dev/pts"), true)?;
    set_mode(&dev.join("pts"), 0o755)?;
    prepare_directory(root, Path::new("dev/shm"), true)?;
    set_mode(&dev.join("shm"), 0o1777)?;

    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("core", "/proc/kcore"),
    ] {
        let path = dev.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(target, &path).map_err(|error| {
            Error::internal(format!(
                "creating device symlink `{}` -> `{target}`: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn populate_standard_devices(_root: &Path) -> Result<()> {
    Err(Error::unsupported(
        "device-node provisioning requires a Linux host",
    ))
}

fn prepare_destination(destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::conflict(format!(
                    "rootfs destination `{}` exists and is not a directory",
                    destination.display()
                )));
            }
            let mut entries = fs::read_dir(destination)
                .map_err(|error| io_error("reading rootfs destination", destination, error))?;
            if entries.next().is_some() {
                return Err(Error::conflict(format!(
                    "rootfs destination `{}` is not empty",
                    destination.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(destination)
                .map_err(|error| io_error("creating rootfs destination", destination, error))?;
        }
        Err(error) => {
            return Err(io_error(
                "inspecting rootfs destination",
                destination,
                error,
            ))
        }
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    entries: usize,
    file_bytes: u64,
}

#[derive(Debug, Clone)]
struct DirectoryMetadata {
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    xattrs: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    Plain,
    Gzip,
    Zstd,
}

fn apply_layer(
    store: &ImageStore,
    layer: &docker_image::Descriptor,
    diff_id: &Digest,
    root: &Path,
    directories: &mut BTreeMap<PathBuf, DirectoryMetadata>,
    totals: &mut Totals,
) -> Result<()> {
    layer
        .validate()
        .map_err(|error| Error::internal(format!("invalid layer descriptor: {error}")))?;
    let blob = store.blob_store().blob_path(&layer.digest);
    let metadata =
        fs::metadata(&blob).map_err(|error| io_error("opening layer blob", &blob, error))?;
    if metadata.len() != layer.size as u64 {
        return Err(Error::internal(format!(
            "layer blob `{}` is {} bytes but the manifest declares {}",
            blob.display(),
            metadata.len(),
            layer.size
        )));
    }
    let compression = compression_for(layer, &blob)?;
    if compression == Compression::Zstd {
        return Err(Error::unsupported(format!(
            "layer {} uses zstd compression, which the native rootfs materializer does not implement yet",
            layer.digest
        )));
    }

    let file = File::open(&blob).map_err(|error| io_error("opening layer blob", &blob, error))?;
    let compressed = HashingReader::new(file, layer.digest.clone(), MAX_LAYER_BYTES);
    let decoded = match compression {
        Compression::Plain => DecodedReader::Plain(compressed),
        Compression::Gzip => DecodedReader::Gzip(GzDecoder::new(compressed)),
        Compression::Zstd => unreachable!("zstd was rejected above"),
    };
    let mut uncompressed = HashingReader::new(decoded, diff_id.clone(), MAX_LAYER_BYTES);
    {
        let mut archive = Archive::new(&mut uncompressed);
        let mut layer_entries = 0usize;

        for entry in archive
            .entries()
            .map_err(|error| Error::internal(format!("reading layer tar: {error}")))?
        {
            let mut entry = entry
                .map_err(|error| Error::internal(format!("reading layer tar entry: {error}")))?;
            layer_entries += 1;
            totals.entries += 1;
            if layer_entries > MAX_LAYER_ENTRIES || totals.entries > MAX_TOTAL_ENTRIES {
                return Err(Error::invalid_argument(format!(
                "layer {} exceeds the maximum of {MAX_LAYER_ENTRIES} entries (or the image exceeds {MAX_TOTAL_ENTRIES})",
                layer.digest
            )));
            }

            let entry_type = entry.header().entry_type();
            if entry_type.is_pax_global_extensions() || entry_type.is_pax_local_extensions() {
                continue;
            }

            let raw_path = entry
                .path()
                .map_err(|error| Error::invalid_argument(format!("invalid layer path: {error}")))?;
            let relative = normalize_layer_path(raw_path.as_ref())?;
            let xattrs = read_pax_xattrs(&mut entry)?;

            if let Some(relative) = &relative {
                if handle_whiteout(root, relative)? {
                    continue;
                }
            }

            let mode =
                entry.header().mode().map_err(|error| {
                    Error::invalid_argument(format!("invalid layer mode: {error}"))
                })? & 0o7777;
            let uid = entry
                .header()
                .uid()
                .map_err(|error| Error::invalid_argument(format!("invalid layer uid: {error}")))?;
            let gid = entry
                .header()
                .gid()
                .map_err(|error| Error::invalid_argument(format!("invalid layer gid: {error}")))?;
            let mtime = entry.header().mtime().map_err(|error| {
                Error::invalid_argument(format!("invalid layer mtime: {error}"))
            })?;

            let Some(relative) = relative else {
                if !entry_type.is_dir() {
                    return Err(Error::invalid_argument(
                        "layer contains a non-directory entry for the rootfs root",
                    ));
                }
                directories.insert(
                    PathBuf::new(),
                    DirectoryMetadata {
                        mode,
                        uid,
                        gid,
                        mtime,
                        xattrs,
                    },
                );
                continue;
            };

            if entry_type.is_dir() {
                prepare_directory(root, parent_of(&relative), true)?;
                place_directory(root, &relative)?;
                directories.insert(
                    relative,
                    DirectoryMetadata {
                        mode,
                        uid,
                        gid,
                        mtime,
                        xattrs,
                    },
                );
            } else if entry_type.is_file() || entry_type.is_contiguous() {
                prepare_directory(root, parent_of(&relative), true)?;
                place_file(
                    root, &relative, &mut entry, mode, uid, gid, mtime, xattrs, totals,
                )?;
            } else if entry_type.is_symlink() {
                prepare_directory(root, parent_of(&relative), true)?;
                let target = entry
                    .link_name()
                    .map_err(|error| {
                        Error::invalid_argument(format!("invalid symlink target: {error}"))
                    })?
                    .ok_or_else(|| Error::invalid_argument("symlink entry has no target"))?;
                place_symlink(root, &relative, target.as_ref())?;
                apply_metadata(
                    &join_relative(root, &relative),
                    &DirectoryMetadata {
                        mode,
                        uid,
                        gid,
                        mtime,
                        xattrs,
                    },
                    true,
                )?;
            } else if entry_type.is_hard_link() {
                prepare_directory(root, parent_of(&relative), true)?;
                let target = entry
                    .link_name()
                    .map_err(|error| {
                        Error::invalid_argument(format!("invalid hardlink target: {error}"))
                    })?
                    .ok_or_else(|| Error::invalid_argument("hardlink entry has no target"))?;
                place_hardlink(root, &relative, target.as_ref())?;
            } else if entry_type.is_fifo()
                || entry_type.is_character_special()
                || entry_type.is_block_special()
            {
                prepare_directory(root, parent_of(&relative), true)?;
                let major = entry.header().device_major().map_err(|error| {
                    Error::invalid_argument(format!("invalid device major number: {error}"))
                })?;
                let minor = entry.header().device_minor().map_err(|error| {
                    Error::invalid_argument(format!("invalid device minor number: {error}"))
                })?;
                place_special(root, &relative, entry_type, mode, major, minor)?;
                apply_metadata(
                    &join_relative(root, &relative),
                    &DirectoryMetadata {
                        mode,
                        uid,
                        gid,
                        mtime,
                        xattrs,
                    },
                    false,
                )?;
            } else {
                return Err(Error::unsupported(format!(
                    "layer {} contains unsupported tar entry type {:?} at `{}`",
                    layer.digest,
                    entry_type,
                    relative.display()
                )));
            }
        }
    }

    io::copy(&mut uncompressed, &mut io::sink()).map_err(|error| {
        Error::internal(format!(
            "reading the remainder of layer {}: {error}",
            layer.digest
        ))
    })?;
    let decoded = uncompressed
        .finish()
        .map_err(|error| Error::internal(format!("verifying layer {}: {error}", layer.digest)))?;
    let mut compressed = decoded.into_inner();
    io::copy(&mut compressed, &mut io::sink()).map_err(|error| {
        Error::internal(format!(
            "reading the remainder of compressed layer {}: {error}",
            layer.digest
        ))
    })?;
    compressed.finish().map_err(|error| {
        Error::internal(format!("verifying layer blob {}: {error}", layer.digest))
    })?;
    Ok(())
}

enum DecodedReader<R> {
    Plain(R),
    Gzip(GzDecoder<R>),
}

impl<R: Read> Read for DecodedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            DecodedReader::Plain(reader) => reader.read(buffer),
            DecodedReader::Gzip(reader) => reader.read(buffer),
        }
    }
}

impl<R> DecodedReader<R> {
    fn into_inner(self) -> R {
        match self {
            DecodedReader::Plain(reader) => reader,
            DecodedReader::Gzip(reader) => reader.into_inner(),
        }
    }
}

fn compression_for(layer: &docker_image::Descriptor, blob: &Path) -> Result<Compression> {
    let media_type = layer.media_type.as_str();
    if media_type.ends_with("+zstd") || media_type.ends_with(".zstd") {
        return Ok(Compression::Zstd);
    }
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        return Ok(Compression::Gzip);
    }
    if media_type.ends_with("+tar")
        || media_type.ends_with(".tar")
        || media_type == docker_image::MEDIA_TYPE_IMAGE_LAYER
        || media_type.is_empty()
    {
        if media_type.is_empty() {
            return sniff_compression(blob);
        }
        return Ok(Compression::Plain);
    }
    Err(Error::unsupported(format!(
        "layer media type `{media_type}` is not implemented"
    )))
}

fn sniff_compression(blob: &Path) -> Result<Compression> {
    let mut file = File::open(blob).map_err(|error| io_error("opening layer blob", blob, error))?;
    let mut magic = [0u8; 4];
    let read = file
        .read(&mut magic)
        .map_err(|error| io_error("reading layer magic", blob, error))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("rewinding layer blob", blob, error))?;
    if read >= 2 && magic[..2] == [0x1f, 0x8b] {
        return Ok(Compression::Gzip);
    }
    if read >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(Compression::Zstd);
    }
    Ok(Compression::Plain)
}

fn normalize_layer_path(path: &Path) -> Result<Option<PathBuf>> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::invalid_argument(format!(
                    "layer path `{}` contains `..`",
                    path.display()
                )))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::invalid_argument(format!(
                    "layer path `{}` is absolute",
                    path.display()
                )))
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(normalized))
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new(""))
}

fn join_relative(root: &Path, relative: &Path) -> PathBuf {
    if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    }
}

/// Handle OCI whiteout entries. Returns true when the entry was a marker and
/// must not be created as a regular file.
fn handle_whiteout(root: &Path, relative: &Path) -> Result<bool> {
    let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if name == ".wh..wh..opq" {
        let parent = parent_of(relative);
        if prepare_directory(root, parent, true)? {
            clear_directory(&join_relative(root, parent))?;
        }
        return Ok(true);
    }
    let Some(target) = name.strip_prefix(".wh.") else {
        return Ok(false);
    };
    if target.is_empty() {
        return Err(Error::invalid_argument(
            "whiteout marker `.wh.` has an empty target",
        ));
    }
    if target == "." || target == ".." || target.contains('/') || target.contains('\\') {
        return Err(Error::invalid_argument(format!(
            "whiteout marker `{name}` has an invalid target"
        )));
    }
    if target.starts_with(".wh.") {
        return Err(Error::invalid_argument(format!(
            "unknown reserved whiteout marker `{name}`"
        )));
    }
    let parent = parent_of(relative);
    if prepare_directory(root, parent, false)? {
        remove_path(&join_relative(root, parent).join(target))?;
    }
    Ok(true)
}

fn clear_directory(path: &Path) -> Result<()> {
    let entries = fs::read_dir(path).map_err(|error| io_error("reading directory", path, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error("reading directory entry", path, error))?;
        remove_path(&entry.path())?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                fs::remove_file(path).map_err(|error| io_error("removing path", path, error))
            } else {
                fs::remove_dir_all(path)
                    .map_err(|error| io_error("removing directory", path, error))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspecting path", path, error)),
    }
}

/// Ensure `relative` is a directory beneath `root` without following a
/// symlink. When `create` is false, a missing component makes the function
/// return `false`; an existing non-directory is an error.
fn prepare_directory(root: &Path, relative: &Path, create: bool) -> Result<bool> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(Error::invalid_argument(format!(
                "invalid directory component in `{}`",
                relative.display()
            )));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::unsupported(format!(
                    "image extraction would traverse symlink `{}`; refusing to follow it outside the rootfs",
                    current.display()
                )))
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) if create => {
                remove_path(&current)?;
                fs::create_dir(&current)
                    .map_err(|error| io_error("creating directory", &current, error))?;
            }
            Ok(_) => {
                return Err(Error::invalid_argument(format!(
                    "layer path `{}` traverses non-directory `{}`",
                    relative.display(),
                    current.display()
                )))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !create {
                    return Ok(false);
                }
                fs::create_dir(&current)
                    .map_err(|error| io_error("creating directory", &current, error))?;
            }
            Err(error) => return Err(io_error("inspecting directory", &current, error)),
        }
    }
    Ok(true)
}

fn place_directory(root: &Path, relative: &Path) -> Result<()> {
    let path = join_relative(root, relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => {
            remove_path(&path)?;
            fs::create_dir(&path).map_err(|error| io_error("creating directory", &path, error))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&path).map_err(|error| io_error("creating directory", &path, error))
        }
        Err(error) => Err(io_error("inspecting directory", &path, error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn place_file<R: Read>(
    root: &Path,
    relative: &Path,
    entry: &mut tar::Entry<'_, R>,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    xattrs: Vec<(String, Vec<u8>)>,
    totals: &mut Totals,
) -> Result<()> {
    let path = join_relative(root, relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&path)
                .map_err(|error| io_error("removing directory", &path, error))?;
        }
        Ok(_) => {
            fs::remove_file(&path).map_err(|error| io_error("removing path", &path, error))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspecting file", &path, error)),
    }

    let declared = entry
        .header()
        .size()
        .map_err(|error| Error::invalid_argument(format!("invalid file size: {error}")))?;
    let next_total = totals
        .file_bytes
        .checked_add(declared)
        .ok_or_else(|| Error::invalid_argument("rootfs size overflow"))?;
    if next_total > MAX_ROOTFS_BYTES {
        return Err(Error::invalid_argument(format!(
            "rootfs exceeds the maximum extracted size of {MAX_ROOTFS_BYTES} bytes"
        )));
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| io_error("creating file", &path, error))?;
    let written = io::copy(entry, &mut file)
        .map_err(|error| io_error("extracting file contents", &path, error))?;
    if written != declared {
        return Err(Error::invalid_argument(format!(
            "layer entry `{}` declares {declared} bytes but contained {written}",
            relative.display()
        )));
    }
    totals.file_bytes = next_total;
    drop(file);

    apply_metadata(
        &path,
        &DirectoryMetadata {
            mode,
            uid,
            gid,
            mtime,
            xattrs,
        },
        false,
    )
}

#[cfg(unix)]
fn place_symlink(root: &Path, relative: &Path, target: &Path) -> Result<()> {
    if target.as_os_str().is_empty() {
        return Err(Error::invalid_argument("symlink target must not be empty"));
    }
    let path = join_relative(root, relative);
    remove_path(&path)?;
    std::os::unix::fs::symlink(target, &path).map_err(|error| {
        Error::internal(format!(
            "creating symlink `{}` -> `{}`: {error}",
            path.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn place_symlink(_root: &Path, relative: &Path, _target: &Path) -> Result<()> {
    Err(Error::unsupported(format!(
        "image layer contains symlink `{}`, which cannot be materialized on {}",
        relative.display(),
        std::env::consts::OS
    )))
}

fn place_hardlink(root: &Path, relative: &Path, target: &Path) -> Result<()> {
    let Some(target) = normalize_layer_path(target)? else {
        return Err(Error::invalid_argument(
            "hardlink target refers to the rootfs root",
        ));
    };
    if !prepare_directory(root, parent_of(&target), false)? {
        return Err(Error::invalid_argument(format!(
            "hardlink target `{}` does not exist",
            target.display()
        )));
    }
    let target_path = join_relative(root, &target);
    let metadata = fs::symlink_metadata(&target_path)
        .map_err(|error| io_error("inspecting hardlink target", &target_path, error))?;
    if metadata.file_type().is_symlink() || metadata.is_dir() {
        return Err(Error::invalid_argument(format!(
            "hardlink target `{}` is not a regular file",
            target.display()
        )));
    }

    let path = join_relative(root, relative);
    remove_path(&path)?;
    fs::hard_link(&target_path, &path).map_err(|error| {
        Error::internal(format!(
            "creating hardlink `{}` -> `{}`: {error}",
            path.display(),
            target_path.display()
        ))
    })
}

fn place_special(
    root: &Path,
    relative: &Path,
    entry_type: EntryType,
    mode: u32,
    major: Option<u32>,
    minor: Option<u32>,
) -> Result<()> {
    let path = join_relative(root, relative);
    remove_path(&path)?;
    place_special_at(&path, entry_type, mode, major, minor)
}

#[cfg(target_os = "linux")]
fn place_special_at(
    path: &Path,
    entry_type: EntryType,
    mode: u32,
    major: Option<u32>,
    minor: Option<u32>,
) -> Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};

    let mode = Mode::from_bits_truncate(mode);
    if entry_type.is_fifo() {
        nix::unistd::mkfifo(path, mode).map_err(|error| {
            Error::internal(format!("creating FIFO `{}`: {error}", path.display()))
        })
    } else {
        let device = makedev(
            u64::from(major.ok_or_else(|| {
                Error::invalid_argument(format!(
                    "device entry `{}` has no major number",
                    path.display()
                ))
            })?),
            u64::from(minor.ok_or_else(|| {
                Error::invalid_argument(format!(
                    "device entry `{}` has no minor number",
                    path.display()
                ))
            })?),
        );
        let kind = if entry_type.is_character_special() {
            SFlag::S_IFCHR
        } else {
            SFlag::S_IFBLK
        };
        mknod(path, kind, mode, device).map_err(|error| {
            Error::internal(format!("creating device `{}`: {error}", path.display()))
        })
    }
}

#[cfg(not(target_os = "linux"))]
fn place_special_at(
    path: &Path,
    _entry_type: EntryType,
    _mode: u32,
    _major: Option<u32>,
    _minor: Option<u32>,
) -> Result<()> {
    Err(Error::unsupported(format!(
        "image layer contains special file `{}`, which requires a Linux host",
        path.display()
    )))
}

fn read_pax_xattrs<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Vec<(String, Vec<u8>)>> {
    let Some(extensions) = entry
        .pax_extensions()
        .map_err(|error| Error::invalid_argument(format!("invalid PAX headers: {error}")))?
    else {
        return Ok(Vec::new());
    };
    let mut xattrs = Vec::new();
    for extension in extensions {
        let extension = extension
            .map_err(|error| Error::invalid_argument(format!("invalid PAX header: {error}")))?;
        let key = extension
            .key()
            .map_err(|error| Error::invalid_argument(format!("PAX key is not UTF-8: {error}")))?;
        if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
            if name.is_empty() || name.contains('\0') {
                return Err(Error::invalid_argument(
                    "PAX extended attribute has an invalid name",
                ));
            }
            xattrs.push((name.to_string(), extension.value_bytes().to_vec()));
        } else if key.starts_with("LIBARCHIVE.xattr.") {
            return Err(Error::unsupported(
                "LIBARCHIVE.xattr PAX attributes are not implemented; refusing to ignore extended attributes",
            ));
        } else if key.starts_with("SCHILY.acl.") || key == "SCHILY.fflags" {
            return Err(Error::unsupported(format!(
                "PAX attribute `{key}` is not implemented; refusing to ignore filesystem metadata"
            )));
        }
    }
    Ok(xattrs)
}

fn apply_metadata(path: &Path, metadata: &DirectoryMetadata, symlink: bool) -> Result<()> {
    apply_ownership(path, metadata.uid, metadata.gid, symlink)?;
    apply_xattrs(path, &metadata.xattrs)?;
    if !symlink {
        set_mode(path, metadata.mode)?;
    }
    let seconds = i64::try_from(metadata.mtime).map_err(|_| {
        Error::invalid_argument(format!(
            "mtime {} of `{}` is outside the supported range",
            metadata.mtime,
            path.display()
        ))
    })?;
    let time = filetime::FileTime::from_unix_time(seconds, 0);
    #[cfg(unix)]
    if symlink {
        filetime::set_symlink_file_times(path, time, time).map_err(|error| {
            Error::internal(format!(
                "setting symlink times on `{}`: {error}",
                path.display()
            ))
        })?;
        return Ok(());
    }
    filetime::set_file_times(path, time, time)
        .map_err(|error| Error::internal(format!("setting times on `{}`: {error}", path.display())))
}

#[cfg(target_os = "linux")]
fn apply_ownership(path: &Path, uid: u64, gid: u64, symlink: bool) -> Result<()> {
    use std::os::unix::fs::{chown, lchown};

    let uid = u32::try_from(uid)
        .map_err(|_| Error::invalid_argument(format!("uid {uid} is out of range")))?;
    let gid = u32::try_from(gid)
        .map_err(|_| Error::invalid_argument(format!("gid {gid} is out of range")))?;
    let effective_uid = nix::unistd::geteuid();
    let effective_gid = nix::unistd::getegid();
    if !effective_uid.is_root() {
        if uid != effective_uid.as_raw() || gid != effective_gid.as_raw() {
            return Err(Error::permission_denied(format!(
                "materializing rootfs ownership {uid}:{gid} at `{}` requires root (running as {}:{})",
                path.display(),
                effective_uid,
                effective_gid
            )));
        }
        return Ok(());
    }
    let result = if symlink {
        lchown(path, Some(uid), Some(gid))
    } else {
        chown(path, Some(uid), Some(gid))
    };
    result.map_err(|error| {
        Error::internal(format!(
            "setting ownership {uid}:{gid} on `{}`: {error}",
            path.display()
        ))
    })
}

#[cfg(not(target_os = "linux"))]
fn apply_ownership(_path: &Path, _uid: u64, _gid: u64, _symlink: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn apply_xattrs(path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
    for (name, value) in xattrs {
        xattr::set(path, name, value).map_err(|error| {
            if error.kind() == io::ErrorKind::Unsupported {
                Error::unsupported(format!(
                    "filesystem does not support extended attribute `{name}` on `{}`",
                    path.display()
                ))
            } else {
                Error::internal(format!(
                    "setting extended attribute `{name}` on `{}`: {error}",
                    path.display()
                ))
            }
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_xattrs(path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
    if xattrs.is_empty() {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "image layer sets extended attributes on `{}`, which cannot be materialized on {}",
            path.display(),
            std::env::consts::OS
        )))
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|error| io_error("setting file mode", path, error))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn io_error(action: &str, path: &Path, error: io::Error) -> Error {
    Error::internal(format!("{action} `{}`: {error}", path.display()))
}

/// Streaming reader that verifies a digest when EOF is reached.
struct HashingReader<R> {
    inner: R,
    hasher: Option<Sha256>,
    expected: Digest,
    read: u64,
    limit: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R, expected: Digest, limit: u64) -> Self {
        Self {
            inner,
            hasher: Some(Sha256::new()),
            expected,
            read: 0,
            limit,
        }
    }

    fn verify(&mut self) -> io::Result<()> {
        let Some(hasher) = self.hasher.take() else {
            return Ok(());
        };
        let actual = format!("{:x}", hasher.finalize());
        if actual != self.expected.hex() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "digest mismatch: expected {}, got sha256:{actual}",
                    self.expected.full()
                ),
            ));
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<R> {
        self.verify()?;
        Ok(self.inner)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 {
            self.verify()?;
            return Ok(0);
        }
        self.read = self.read.saturating_add(read as u64);
        if self.read > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("layer stream exceeds {} bytes", self.limit),
            ));
        }
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&buffer[..read]);
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use docker_image::{Descriptor, Image, Platform, RootFS};
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tempfile::TempDir;

    use super::*;

    struct TestImage {
        temp: TempDir,
        store: ImageStore,
        record: ImageRecord,
    }

    fn ids() -> (u64, u64) {
        #[cfg(target_os = "linux")]
        {
            (
                u64::from(nix::unistd::geteuid().as_raw()),
                u64::from(nix::unistd::getegid().as_raw()),
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            (0, 0)
        }
    }

    fn tar_header(entry_type: EntryType, size: u64, mode: u32) -> tar::Header {
        let (uid, gid) = ids();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(size);
        header.set_mode(mode);
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_mtime(1_700_000_000);
        header
    }

    fn layer(entries: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        entries(&mut builder);
        let mut bytes = builder.into_inner().unwrap();
        bytes.extend_from_slice(&[0u8; 1024]);
        bytes
    }

    fn add_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
        let mut header = tar_header(EntryType::Regular, data.len() as u64, 0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    }

    fn add_dir(builder: &mut tar::Builder<Vec<u8>>, path: &str) {
        let mut header = tar_header(EntryType::Directory, 0, 0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }

    #[cfg(unix)]
    fn add_symlink(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
        let mut header = tar_header(EntryType::Symlink, 0, 0o777);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }

    fn add_hardlink(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
        let mut header = tar_header(EntryType::Link, 0, 0o644);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append_data(&mut header, path, io::empty()).unwrap();
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn test_image(layers: Vec<Vec<u8>>, compressed: bool) -> TestImage {
        let temp = tempfile::tempdir().unwrap();
        let store = ImageStore::open(temp.path().join("images")).unwrap();
        let mut descriptors = Vec::new();
        let mut diff_ids = Vec::new();
        let mut total = 0i64;
        for raw in layers {
            let diff_id = Digest::sha256(&raw);
            let (media_type, payload) = if compressed {
                (docker_image::MEDIA_TYPE_IMAGE_LAYER_GZIP, gzip(&raw))
            } else {
                (docker_image::MEDIA_TYPE_IMAGE_LAYER, raw)
            };
            let digest = Digest::sha256(&payload);
            store.blob_store().put_verified(&digest, &payload).unwrap();
            total = total.saturating_add(payload.len() as i64);
            descriptors.push(Descriptor {
                media_type: media_type.to_string(),
                digest,
                size: payload.len() as i64,
                ..Descriptor::default()
            });
            diff_ids.push(diff_id);
        }

        let config = Image {
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            rootfs: RootFS::layers(diff_ids),
            ..Image::default()
        };
        let record = ImageRecord {
            id: Digest::sha256(b"test-image-config"),
            manifest_digest: Digest::sha256(b"test-image-manifest"),
            manifest_digests: Vec::new(),
            tags: Default::default(),
            repo_digests: Default::default(),
            created: 1_700_000_000,
            pulled_at: 1_700_000_000,
            size: total,
            config_size: 0,
            platform: Platform {
                architecture: "amd64".to_string(),
                os: "linux".to_string(),
                ..Platform::default()
            },
            config,
            layers: descriptors,
        };
        store.insert(record.clone()).unwrap();
        TestImage {
            temp,
            store,
            record,
        }
    }

    #[test]
    fn materializes_layers_and_whiteouts() {
        let image = test_image(
            vec![
                layer(|builder| {
                    add_dir(builder, "etc/");
                    add_file(builder, "etc/hello", b"old");
                    add_file(builder, "etc/keep", b"keep");
                }),
                layer(|builder| {
                    add_file(builder, "etc/.wh.hello", b"");
                    add_file(builder, "etc/new", b"new");
                }),
            ],
            false,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();

        assert!(!root.join("etc/hello").exists());
        assert_eq!(fs::read(root.join("etc/keep")).unwrap(), b"keep");
        assert_eq!(fs::read(root.join("etc/new")).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(root.join("etc")).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn applies_opaque_directory_whiteout() {
        let image = test_image(
            vec![
                layer(|builder| {
                    add_dir(builder, "etc/");
                    add_file(builder, "etc/old", b"old");
                }),
                layer(|builder| {
                    add_file(builder, "etc/.wh..wh..opq", b"");
                    add_file(builder, "etc/new", b"new");
                }),
            ],
            false,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();
        assert!(!root.join("etc/old").exists());
        assert_eq!(fs::read(root.join("etc/new")).unwrap(), b"new");
    }

    #[test]
    fn decompresses_gzip_layers_and_verifies_diff_ids() {
        let image = test_image(
            vec![layer(|builder| {
                add_dir(builder, "usr/");
                add_file(builder, "usr/hello", b"compressed");
            })],
            true,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();
        assert_eq!(fs::read(root.join("usr/hello")).unwrap(), b"compressed");
    }

    #[test]
    fn creates_hardlinks_inside_the_rootfs() {
        let image = test_image(
            vec![layer(|builder| {
                add_file(builder, "a", b"same");
                add_hardlink(builder, "b", "a");
            })],
            false,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();
        assert_eq!(fs::read(root.join("b")).unwrap(), b"same");
    }

    #[test]
    fn rejects_destination_that_is_not_empty() {
        let image = test_image(
            vec![layer(|builder| {
                add_file(builder, "file", b"x");
            })],
            false,
        );
        let root = image.temp.path().join("rootfs");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("already-here"), b"x").unwrap();
        let error = materialize_rootfs(&image.store, &image.record, &root).unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Conflict);
    }

    #[test]
    fn rejects_reserved_and_escaping_whiteout_markers() {
        let image = test_image(
            vec![layer(|builder| {
                add_file(builder, "dir/.wh..wh.unsupported", b"");
            })],
            false,
        );
        let root = image.temp.path().join("rootfs");
        let error = materialize_rootfs(&image.store, &image.record, &root).unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::InvalidArgument);
    }

    #[test]
    fn normalizes_layer_paths_without_accepting_escapes() {
        assert_eq!(
            normalize_layer_path(Path::new("./etc//hello")).unwrap(),
            Some(PathBuf::from("etc/hello"))
        );
        assert_eq!(normalize_layer_path(Path::new(".")).unwrap(), None);
        assert!(normalize_layer_path(Path::new("../escape")).is_err());
        assert!(normalize_layer_path(Path::new("/absolute")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn materializes_symlinks_without_following_them() {
        let image = test_image(
            vec![layer(|builder| {
                add_file(builder, "etc/hello", b"target");
                add_symlink(builder, "etc/link", "hello");
            })],
            false,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();
        assert_eq!(
            fs::read_link(root.join("etc/link")).unwrap(),
            PathBuf::from("hello")
        );
        assert_eq!(fs::read(root.join("etc/link")).unwrap(), b"target");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_layers_that_write_through_a_symlink_ancestor() {
        let image = test_image(
            vec![
                layer(|builder| {
                    add_dir(builder, "target/");
                    add_symlink(builder, "link", "target");
                }),
                layer(|builder| {
                    add_file(builder, "link/pwned", b"escape");
                }),
            ],
            false,
        );
        let root = image.temp.path().join("rootfs");
        let error = materialize_rootfs(&image.store, &image.record, &root).unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(!root.join("target/pwned").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn creates_standard_devices_when_root() {
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        let image = test_image(
            vec![layer(|builder| {
                add_dir(builder, "etc/");
            })],
            false,
        );
        let root = image.temp.path().join("rootfs");
        materialize_rootfs(&image.store, &image.record, &root).unwrap();
        populate_standard_devices(&root).unwrap();
        use std::os::unix::fs::FileTypeExt as _;
        for name in ["null", "zero", "full", "random", "urandom", "tty"] {
            let metadata = fs::symlink_metadata(root.join("dev").join(name)).unwrap();
            assert!(
                metadata.file_type().is_char_device(),
                "/dev/{name} must be a character device"
            );
        }
        assert_eq!(
            fs::read_link(root.join("dev/fd")).unwrap(),
            PathBuf::from("/proc/self/fd")
        );
    }
}
