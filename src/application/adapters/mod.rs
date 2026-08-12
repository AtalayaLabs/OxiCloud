//! Adapters module for translating between external protocols and internal models

pub mod caldav_adapter;
pub mod carddav_adapter;
pub mod param_encoding;
pub mod plugin_lifecycle_hook;
pub mod plugin_user_lifecycle_hook;
pub mod webdav_adapter;

#[cfg(test)]
mod caldav_adapter_test;
#[cfg(test)]
mod carddav_adapter_test;

/// Extract the resource UID from a DAV multiget `href`.
///
/// CalDAV/CardDAV multiget REPORTs address object resources by full href
/// (e.g. `/caldav/{calendar_id}/{uid}.ics`, possibly with a username
/// segment). The UID is the last path segment with the protocol
/// `extension` (`.ics` / `.vcf`, matched case-insensitively) stripped,
/// then percent-decoded — hrefs arrive in the XML body, so they have not
/// gone through URL-path decoding, and clients may re-encode hrefs they
/// previously read from the server.
///
/// Returns `None` for collection hrefs (empty last segment) or segments
/// that are not valid UTF-8 after decoding.
pub fn uid_from_multiget_href(href: &str, extension: &str) -> Option<String> {
    // A trailing slash denotes a collection, not an object resource.
    if href.ends_with('/') {
        return None;
    }
    let segment = href.rsplit('/').next()?;

    // Case-insensitive ASCII extension strip; the matched tail is ASCII,
    // so the byte cut is guaranteed to land on a char boundary.
    let bytes = segment.as_bytes();
    let ext = extension.as_bytes();
    let segment =
        if bytes.len() >= ext.len() && bytes[bytes.len() - ext.len()..].eq_ignore_ascii_case(ext) {
            &segment[..segment.len() - ext.len()]
        } else {
            segment
        };

    let decoded = percent_encoding::percent_decode_str(segment)
        .decode_utf8()
        .ok()?;
    let uid = decoded.trim();
    (!uid.is_empty()).then(|| uid.to_string())
}

#[cfg(test)]
mod multiget_href_tests {
    use super::uid_from_multiget_href;

    #[test]
    fn plain_caldav_href() {
        assert_eq!(
            uid_from_multiget_href("/caldav/abc-123/event-uid.ics", ".ics"),
            Some("event-uid".to_string())
        );
    }

    #[test]
    fn href_with_username_prefix() {
        assert_eq!(
            uid_from_multiget_href("/carddav/alice/book-1/uid-42.vcf", ".vcf"),
            Some("uid-42".to_string())
        );
    }

    #[test]
    fn uppercase_extension() {
        assert_eq!(
            uid_from_multiget_href("/caldav/abc/EVENT.ICS", ".ics"),
            Some("EVENT".to_string())
        );
    }

    #[test]
    fn percent_encoded_uid() {
        assert_eq!(
            uid_from_multiget_href("/caldav/abc/uid%40example.com.ics", ".ics"),
            Some("uid@example.com".to_string())
        );
    }

    #[test]
    fn missing_extension_uses_whole_segment() {
        assert_eq!(
            uid_from_multiget_href("/caldav/abc/bare-uid", ".ics"),
            Some("bare-uid".to_string())
        );
    }

    #[test]
    fn collection_href_yields_none() {
        assert_eq!(uid_from_multiget_href("/caldav/abc/", ".ics"), None);
        assert_eq!(uid_from_multiget_href("", ".ics"), None);
    }
}
