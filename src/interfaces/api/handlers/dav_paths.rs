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
    use super::collection_href_with_base;

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
}
