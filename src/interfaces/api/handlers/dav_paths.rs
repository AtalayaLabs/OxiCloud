/// Percent-decode a DAV path segment by segment (#315).
///
/// Decoding the whole path before splitting turned `a%2Fb` into the nested
/// path `a/b`. Names cannot contain `/` or `\` anyway
/// (`path_service::validate_storage_name`), so a segment that decodes to
/// either is refused; callers map the error to 400. Invalid UTF-8 is
/// refused too instead of being decoded lossily.
pub(crate) fn decode_and_validate_path(encoded_path: &str) -> Result<String, &'static str> {
    encoded_path
        .split('/')
        .map(|seg| {
            let d = percent_encoding::percent_decode_str(seg)
                .decode_utf8()
                .map_err(|_| "invalid UTF-8 in path")?;
            if d.contains(['/', '\\']) {
                return Err("path segment decodes to '/' or '\\'");
            }
            Ok(d.into_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.join("/"))
}

/// Keep collection/member hrefs in the alias the client used. Discovery may
/// lead clients through `/{username}/{collection_id}/`; dropping that prefix
/// makes a successfully uploaded member disappear from a URL-based sync view.
/// `path` is the original encoded URI path and `collection_id` is the collection
/// already resolved by the handler, including for member requests. Retain the
/// requested encoding as well: `user%40example.com` must not become another URL.
pub(super) fn collection_href(protocol: &str, path: &str, collection_id: &str) -> String {
    collection_href_with_base(
        crate::common::config::server_base_path(),
        protocol,
        path,
        collection_id,
    )
}

fn collection_href_with_base(
    base: &str,
    protocol: &str,
    path: &str,
    collection_id: &str,
) -> String {
    let prefix = format!("/{protocol}/");
    // The protocol router and path parser have already accepted this path.
    // Axum may have stripped a deployment prefix; the configured base below
    // supplies it once in either case.
    let relative = path.split_once(&prefix).map_or(path, |(_, rest)| rest);
    let mut segments = relative.split('/');
    let first = segments.next().unwrap_or(relative);
    let collection_path =
        if percent_encoding::percent_decode_str(first).decode_utf8_lossy() == collection_id {
            first.to_string()
        } else {
            format!("{first}/{}", segments.next().unwrap_or(collection_id))
        };
    format!("{base}/{protocol}/{collection_path}/")
}

#[cfg(test)]
mod tests {
    use super::{collection_href_with_base, decode_and_validate_path};

    #[test]
    fn collection_alias_survives_subpath_and_member_requests() {
        let id = "7c1341b2-e785-42b9-b2bd-919d87a539c2";
        for protocol in ["caldav", "carddav"] {
            for suffix in ["", "/", "/object.ics", "/object.vcf"] {
                assert_eq!(
                    collection_href_with_base(
                        "/cloud",
                        protocol,
                        &format!("/{protocol}/{id}{suffix}"),
                        id
                    ),
                    format!("/cloud/{protocol}/{id}/")
                );
                assert_eq!(
                    collection_href_with_base(
                        "/cloud",
                        protocol,
                        &format!("/cloud/{protocol}/admin/{id}{suffix}"),
                        id
                    ),
                    format!("/cloud/{protocol}/admin/{id}/")
                );
            }
        }
    }

    #[test]
    fn request_path_encoding_is_preserved() {
        let id = "7c1341b2-e785-42b9-b2bd-919d87a539c2";
        assert_eq!(
            collection_href_with_base(
                "",
                "caldav",
                &format!("/caldav/user%40example.com/{id}/event.ics"),
                id
            ),
            format!("/caldav/user%40example.com/{id}/")
        );
    }

    // ── Issue #315: WebDAV percent-decoding security tests ──
    // Regression pin: a%2Fb must NOT silently become a nested path a/b.
    // Paths containing / are NOT allowed in storage (path_service::validate_storage_name).

    #[test]
    fn plain_path_unchanged() {
        assert_eq!(decode_and_validate_path("simple").unwrap(), "simple");
        assert_eq!(
            decode_and_validate_path("folder/file.txt").unwrap(),
            "folder/file.txt"
        );
    }

    #[test]
    fn space_and_unicode_decoded() {
        assert_eq!(
            decode_and_validate_path("hello%20world").unwrap(),
            "hello world"
        );
        assert_eq!(
            decode_and_validate_path("caf%C3%A9").unwrap(), // UTF-8 for é
            "café"
        );
    }

    #[test]
    fn percent_2f_uppercase_rejected() {
        // %2F is the uppercase encoding of /
        assert!(decode_and_validate_path("a%2Fb").is_err());
    }

    #[test]
    fn percent_2f_lowercase_rejected() {
        // %2f is the lowercase encoding of / — both must be rejected
        assert!(decode_and_validate_path("a%2fb").is_err());
    }

    #[test]
    fn percent_5c_uppercase_rejected() {
        // %5C is the uppercase encoding of \
        assert!(decode_and_validate_path("a%5Cb").is_err());
    }

    #[test]
    fn percent_5c_lowercase_rejected() {
        // %5c is the lowercase encoding of \
        assert!(decode_and_validate_path("a%5cb").is_err());
    }

    #[test]
    fn real_slash_still_splits() {
        // A real slash in the input is preserved (it's not encoded)
        assert_eq!(
            decode_and_validate_path("folder/file").unwrap(),
            "folder/file"
        );
    }

    #[test]
    fn destination_header_with_percent_2f_rejected() {
        // Simulate a Destination header like /webdav/folder/a%2Fb (should become a/b on decode, fail)
        assert!(decode_and_validate_path("folder/a%2Fb").is_err());
    }

    #[test]
    fn traversal_still_rejected_by_caller() {
        // . and .. are handled by the caller (reject_path_traversal), not here
        // but the helper itself doesn't decode them specially
        assert_eq!(decode_and_validate_path("..").unwrap(), "..");
        assert_eq!(decode_and_validate_path(".").unwrap(), ".");
    }
}
