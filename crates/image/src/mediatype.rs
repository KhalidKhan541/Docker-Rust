//! Media types ported from `upstream/image-spec/specs-go/v1/mediatype.go`
//! (string values verbatim against the pinned upstream revision).

pub const MEDIA_TYPE_DESCRIPTOR: &str = "application/vnd.oci.descriptor.v1+json";

pub const MEDIA_TYPE_LAYOUT_HEADER: &str = "application/vnd.oci.layout.header.v1+json";

pub const MEDIA_TYPE_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

pub const MEDIA_TYPE_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

pub const MEDIA_TYPE_IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";

/// Media type for an unused blob containing the literal value `{}`.
pub const MEDIA_TYPE_EMPTY_JSON: &str = "application/vnd.oci.empty.v1+json";

/// Media type used for layers referenced by the manifest.
pub const MEDIA_TYPE_IMAGE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";

pub const MEDIA_TYPE_IMAGE_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

pub const MEDIA_TYPE_IMAGE_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// Non-distributable layer media types remain recognized, but per the
/// upstream deprecation they are produced only for round-tripping of
/// existing content; docker-rs builders never emit them.
pub const MEDIA_TYPE_IMAGE_LAYER_NON_DISTRIBUTABLE: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar";
pub const MEDIA_TYPE_IMAGE_LAYER_NON_DISTRIBUTABLE_GZIP: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip";
pub const MEDIA_TYPE_IMAGE_LAYER_NON_DISTRIBUTABLE_ZSTD: &str =
    "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_match_upstream_strings() {
        assert_eq!(
            MEDIA_TYPE_IMAGE_MANIFEST,
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            MEDIA_TYPE_IMAGE_INDEX,
            "application/vnd.oci.image.index.v1+json"
        );
        assert_eq!(
            MEDIA_TYPE_IMAGE_CONFIG,
            "application/vnd.oci.image.config.v1+json"
        );
        assert_eq!(MEDIA_TYPE_EMPTY_JSON, "application/vnd.oci.empty.v1+json");
        assert_eq!(
            MEDIA_TYPE_IMAGE_LAYER,
            "application/vnd.oci.image.layer.v1.tar"
        );
        assert_eq!(
            MEDIA_TYPE_IMAGE_LAYER_GZIP,
            "application/vnd.oci.image.layer.v1.tar+gzip"
        );
        assert_eq!(
            MEDIA_TYPE_IMAGE_LAYER_ZSTD,
            "application/vnd.oci.image.layer.v1.tar+zstd"
        );
    }
}
