//! Milestone B vertical slice: compose an image config → store it in the
//! content-addressed blob store with digest verification → reference it
//! from a manifest → export everything as an OCI image layout directory
//! with an index entry — then reload and re-verify the whole thing.
//!
//! Mirrors the real builder flow: config JSON and layers land in the CAS
//! first, descriptors are composed afterwards, and the layout on disk is
//! byte-compatible with the OCI image-spec layout (§layout: `oci-layout`
//! marker, `blobs/sha256/<hex>`, `index.json`).

use docker_common::Digest;
use docker_errors::ErrorKind;
use docker_image::{
    Descriptor, History, Image, ImageConfig, Index, Manifest, Platform, RootFS,
    MEDIA_TYPE_IMAGE_CONFIG, MEDIA_TYPE_IMAGE_LAYER_GZIP, MEDIA_TYPE_IMAGE_MANIFEST,
};
use docker_storage::{init_image_layout, BlobStore};

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "drc-slice-{tag}-{}",
        docker_common::rng::random_hex(12).unwrap()
    ))
}

#[test]
fn image_config_to_verified_cas_to_layout() {
    // 1. Compose an image config like a builder would: one layer diff, an
    //    entrypoint, and a matching history entry.
    let diff_id = Digest::sha256(b"layer-1-uncompressed-content");
    let image = Image {
        architecture: "amd64".into(),
        os: "linux".into(),
        config: ImageConfig {
            env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/bin".into()],
            entrypoint: vec!["/app/server".into()],
            ..ImageConfig::default()
        },
        rootfs: RootFS::layers(vec![diff_id]),
        history: vec![History {
            created_by: Some("COPY . /app".into()),
            ..History::default()
        }],
        ..Image::default()
    };
    image.validate().unwrap();
    let config_bytes = serde_json::to_vec(&image).unwrap();

    // 2. Store the config with verification. A wrong expected digest must
    //    be rejected before anything is written.
    let cas_dir = tmp_dir("cas");
    let store = BlobStore::open(cas_dir.clone()).unwrap();
    let wrong = Digest::sha256(b"not-the-config");
    let err = store.put_verified(&wrong, &config_bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    assert!(err.message().contains("digest mismatch"));
    assert_eq!(
        store.list().unwrap().len(),
        0,
        "rejected write left no blob"
    );

    let expected = Digest::sha256(&config_bytes);
    let (config_digest, config_size) = store.put_verified(&expected, &config_bytes).unwrap();
    assert_eq!(store.get_verified(&config_digest).unwrap(), config_bytes);
    assert_eq!(store.stat(&config_digest).unwrap(), config_size);

    // 3. Store a (pretend-compressed) layer and reference both blobs from
    //    a manifest descriptor set, exactly like the builder would.
    let layer_bytes = b"compressed-layer-payload".to_vec();
    let (layer_digest, layer_size) = store.put(&layer_bytes).unwrap();
    let manifest = Manifest::new(
        Descriptor {
            media_type: MEDIA_TYPE_IMAGE_CONFIG.into(),
            digest: config_digest.clone(),
            size: config_size as i64,
            ..Descriptor::default()
        },
        vec![Descriptor {
            media_type: MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: layer_digest.clone(),
            size: layer_size as i64,
            ..Descriptor::default()
        }],
    );
    manifest.validate().unwrap();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let (manifest_digest, manifest_size) = store.put(&manifest_bytes).unwrap();

    // 4. Export as an OCI image layout: marker + blobs + index.json.
    let layout_dir = tmp_dir("layout");
    let layout_store = init_image_layout(&layout_dir).unwrap();
    let (put_digest, put_size) = layout_store.put(&manifest_bytes).unwrap();
    assert_eq!(put_digest, manifest_digest, "digests are store-independent");
    assert_eq!(put_size, manifest_size);
    layout_store.put(&config_bytes).unwrap();
    layout_store.put(&layer_bytes).unwrap();

    let mut index = Index::new();
    index.add(Descriptor {
        media_type: MEDIA_TYPE_IMAGE_MANIFEST.into(),
        digest: manifest_digest.clone(),
        size: manifest_size as i64,
        platform: Some(Platform {
            architecture: "amd64".into(),
            os: "linux".into(),
            ..Platform::default()
        }),
        annotations: [(
            "org.opencontainers.image.ref.name".to_string(),
            "docker-rs/test".to_string(),
        )]
        .into_iter()
        .collect(),
        ..Descriptor::default()
    });
    index.validate().unwrap();
    let index_bytes = serde_json::to_vec_pretty(&index).unwrap();
    std::fs::write(layout_dir.join("index.json"), &index_bytes).unwrap();

    // 5. Reload everything from disk and verify integrity end to end.
    docker_storage::check_image_layout(&layout_dir).unwrap();
    let reloaded = BlobStore::open(layout_dir.join("blobs")).unwrap();
    assert_eq!(
        reloaded.get_verified(&manifest_digest).unwrap(),
        manifest_bytes
    );
    assert_eq!(reloaded.get_verified(&config_digest).unwrap(), config_bytes);
    assert_eq!(reloaded.get_verified(&layer_digest).unwrap(), layer_bytes);
    assert_eq!(reloaded.list().unwrap().len(), 3);

    let index_on_disk: Index =
        serde_json::from_slice(&std::fs::read(layout_dir.join("index.json")).unwrap()).unwrap();
    assert_eq!(index_on_disk, index);
    assert_eq!(index_on_disk.manifests[0].digest, manifest_digest);
    assert_eq!(
        index_on_disk.manifests[0]
            .platform
            .as_ref()
            .unwrap()
            .architecture,
        "amd64"
    );

    // Cleanup (best effort; temp dirs are uniquely named anyway).
    let _ = std::fs::remove_dir_all(&cas_dir);
    let _ = std::fs::remove_dir_all(&layout_dir);
}
