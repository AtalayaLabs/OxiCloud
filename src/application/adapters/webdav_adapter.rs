use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use chrono::Utc;
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
/**
 * WebDAV Adapter Module
 *
 * This module provides conversion between WebDAV protocol XML structures and OxiCloud domain objects.
 * It handles parsing WebDAV request XML and generating WebDAV response XML according to RFC 4918.
 */
use std::io::{BufReader, Read, Write};

/// Result type for WebDAV operations
pub type Result<T> = std::result::Result<T, WebDavError>;

/// Error type for WebDAV operations
#[derive(Debug)]
pub enum WebDavError {
    XmlError(quick_xml::Error),
    IoError(std::io::Error),
    ParseError(String),
}

impl From<quick_xml::Error> for WebDavError {
    fn from(err: quick_xml::Error) -> Self {
        WebDavError::XmlError(err)
    }
}

impl From<std::io::Error> for WebDavError {
    fn from(err: std::io::Error) -> Self {
        WebDavError::IoError(err)
    }
}

impl std::fmt::Display for WebDavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebDavError::XmlError(e) => write!(f, "XML error: {}", e),
            WebDavError::IoError(e) => write!(f, "IO error: {}", e),
            WebDavError::ParseError(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

/// Qualified name with namespace and local name
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct QualifiedName {
    pub namespace: String,
    pub name: String,
}

impl QualifiedName {
    pub fn new<S: Into<String>>(namespace: S, name: S) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.namespace.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{{{}}}{}", self.namespace, self.name)
        }
    }
}

/// Whether PROPPATCH must refuse to set/remove this property as a dead
/// property (RFC 4918 §9.2 — server MAY reject a PROPPATCH attempt on a
/// live property; DeadPropertyStore has no business holding a value that
/// PROPFIND / REPORT already emit from live server state).
pub fn is_protected_property(qn: &QualifiedName) -> bool {
    match qn.namespace.as_str() {
        // RFC 4918 §15 — the DAV: namespace is server-owned in its
        // entirety. Any PROPPATCH into it either forges a live
        // property (dual-emission) or accumulates unread garbage
        // (silent litter).
        "DAV:" => true,

        // Every name below appears verbatim in write_folder_response
        // / write_file_response in the NC handler. Adding a new
        // live emitter → add its name here.
        "http://owncloud.org/ns" => matches!(
            qn.name.as_str(),
            "favorite"
                | "fileid"
                | "id"
                | "owner-id"
                | "owner-display-name"
                | "permissions"
                | "share-types"
                | "size"
        ),

        "http://nextcloud.org/ns" => matches!(
            qn.name.as_str(),
            "has-preview" | "is-encrypted" | "mount-type" | "creation_time" | "upload_time"
        ),

        "http://open-collaboration-services.org/ns" => {
            matches!(qn.name.as_str(), "share-permissions")
        }

        _ => false,
    }
}

/// PROPFIND request type
#[derive(Debug, PartialEq)]
pub enum PropFindType {
    /// Request all properties
    AllProp,
    /// Request property names only
    PropName,
    /// Request specific properties
    Prop(Vec<QualifiedName>),
}

/// PROPFIND request
#[derive(Debug)]
pub struct PropFindRequest {
    pub prop_find_type: PropFindType,
}

impl PropFindRequest {
    /// Whether answering this PROPFIND requires resolving the account /
    /// drive quota at all.
    ///
    /// `resolve_webdav_quota` costs two DB round-trips per request; sync
    /// clients poll folders with an explicit `<D:prop>` list that most of
    /// the time names only etag/length/type props — computing quota there
    /// is pure waste (the response never mentions it). `AllProp` and
    /// `PropName` keep quota: the writers emit RFC 4331 props for both.
    /// Measured in `benches/QUOTA-PATH.md`.
    pub fn wants_quota(&self) -> bool {
        match &self.prop_find_type {
            PropFindType::AllProp | PropFindType::PropName => true,
            PropFindType::Prop(props) => props.iter().any(|p| {
                p.namespace == "DAV:"
                    && matches!(
                        p.name.as_str(),
                        "quota-used-bytes" | "quota-available-bytes"
                    )
            }),
        }
    }
}

/// Parsed RFC 6578 `<D:sync-collection>` REPORT request.
#[derive(Debug)]
pub struct SyncCollectionRequest {
    /// `None` for an initial sync (empty/absent `<D:sync-token/>`).
    pub sync_token: Option<String>,
    /// Reuses `PropFindRequest`'s prop-selection semantics (allprop /
    /// propname / prop) — sync-collection's `<D:prop>` block means the
    /// same thing PROPFIND's does.
    pub request: PropFindRequest,
}

/// WebDAV property value
#[derive(Debug, Clone)]
pub struct PropValue {
    pub name: QualifiedName,
    pub value: Option<String>,
}

/// A single PROPPATCH operation (preserves document order per RFC 4918 §9.2).
#[derive(Debug, Clone)]
pub enum PropPatchOp {
    Set(PropValue),
    Remove(QualifiedName),
}

/// WebDAV lock information
#[derive(Debug, Clone)]
pub struct LockInfo {
    pub token: String,
    pub owner: Option<String>,
    pub depth: String,
    pub timeout: Option<String>,
    pub scope: LockScope,
    pub type_: LockType,
}

/// Lock scope (exclusive or shared)
#[derive(Debug, Clone, PartialEq)]
pub enum LockScope {
    Exclusive,
    Shared,
}

/// Lock type (currently only write)
#[derive(Debug, Clone, PartialEq)]
pub enum LockType {
    Write,
}

/// Extra property context for Nextcloud/ownCloud WebDAV extensions.
#[derive(Debug, Clone)]
pub struct NextcloudPropContext {
    pub file_id: Option<i64>,
    pub oc_id: Option<String>,
    pub owner_id: Option<String>,
    pub owner_display_name: Option<String>,
    pub permissions: String,
    pub size: u64,
    pub has_preview: bool,
    pub is_encrypted: bool,
    pub mount_type: String,
    pub contained_file_count: u64,
    pub contained_folder_count: u64,
}

impl NextcloudPropContext {
    pub fn for_folder(
        file_id: Option<i64>,
        oc_id: Option<String>,
        owner: &str,
        contained_files: u64,
        contained_folders: u64,
    ) -> Self {
        Self {
            file_id,
            oc_id,
            owner_id: Some(owner.to_string()),
            owner_display_name: Some(owner.to_string()),
            permissions: "RGDNVCK".to_string(),
            size: 0,
            has_preview: false,
            is_encrypted: false,
            mount_type: "dir".to_string(),
            contained_file_count: contained_files,
            contained_folder_count: contained_folders,
        }
    }

    pub fn for_file(file_id: Option<i64>, oc_id: Option<String>, owner: &str, size: u64) -> Self {
        Self {
            file_id,
            oc_id,
            owner_id: Some(owner.to_string()),
            owner_display_name: Some(owner.to_string()),
            permissions: "RGDNVW".to_string(),
            size,
            has_preview: false,
            is_encrypted: false,
            mount_type: "file".to_string(),
            contained_file_count: 0,
            contained_folder_count: 0,
        }
    }
}

/// Defense-in-depth cap on attributes per XML element in WebDAV request
/// bodies. Legitimate PROPFIND / PROPPATCH elements carry a handful of
/// `xmlns:*` declarations and, occasionally, per-property namespace
/// bindings — a dozen is already a lot. 100 is generous headroom and
/// three orders of magnitude below what an attacker would need to
/// exploit a quadratic parser bug (see quick-xml #969, fixed in 0.41;
/// this cap fences the same threat model for any future analogous bug
/// in whatever parser we swap to).
///
/// A rejected element yields 400 Bad Request via the ParseError path.
pub const MAX_ATTRIBUTES_PER_ELEMENT: usize = 100;

/// WebDAV adapter for converting between XML and domain objects
pub struct WebDavAdapter;

impl WebDavAdapter {
    /// Refuse elements carrying an unreasonable attribute count.
    /// See [`MAX_ATTRIBUTES_PER_ELEMENT`] for the reasoning.
    ///
    /// `Attributes::count()` is O(N) in the number of attributes (each
    /// attribute is parsed once), so this check itself is safe even
    /// against very large elements. The parser may still have paid a
    /// quadratic cost by the time we get here on a vulnerable version
    /// of the underlying library — the bump to quick-xml 0.41 closes
    /// that specific bug; this cap is defense-in-depth against future
    /// analogous bugs and against adversarially large XML that would
    /// otherwise reach our downstream code.
    fn check_attribute_cap(e: &BytesStart) -> Result<()> {
        if e.attributes().count() > MAX_ATTRIBUTES_PER_ELEMENT {
            return Err(WebDavError::ParseError(format!(
                "Element carries more than {MAX_ATTRIBUTES_PER_ELEMENT} attributes"
            )));
        }
        Ok(())
    }

    /// Collect namespace prefix → URI mappings from element attributes.
    /// E.g. `xmlns:D="DAV:"` maps prefix `"D"` to `"DAV:"`.
    pub fn collect_ns_decls(
        e: &BytesStart,
        ns_map: &mut std::collections::HashMap<String, String>,
    ) {
        for attr in e.attributes().flatten() {
            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
            if let Some(prefix) = key.strip_prefix("xmlns:") {
                let uri = attr
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .unwrap_or_default()
                    .to_string();
                ns_map.insert(prefix.to_string(), uri);
            } else if key == "xmlns" {
                // Default namespace declaration: xmlns="uri"
                let uri = attr
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .unwrap_or_default()
                    .to_string();
                ns_map.insert(String::new(), uri);
            }
        }
    }

    /// Reject `xmlns:prefix=""` declarations — binding a prefix to an empty URI
    /// is forbidden by the XML Namespaces 1.0 spec (RFC 4918 §8.1 requires 400).
    fn check_ns_decls_valid(e: &BytesStart) -> Result<()> {
        for attr in e.attributes().flatten() {
            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
            if key.starts_with("xmlns:") {
                let uri = attr
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .unwrap_or_default();
                if uri.is_empty() {
                    return Err(WebDavError::ParseError(
                        "Invalid namespace declaration: prefix bound to empty URI".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolve a prefixed element name (e.g. `D:resourcetype`) to a
    /// `QualifiedName` using the accumulated namespace declarations.
    pub fn resolve_name(
        name_str: &str,
        ns_map: &std::collections::HashMap<String, String>,
    ) -> QualifiedName {
        if let Some(idx) = name_str.find(':') {
            let prefix = &name_str[..idx];
            let local = &name_str[idx + 1..];
            if let Some(uri) = ns_map.get(prefix) {
                return QualifiedName::new(uri.clone(), local.to_string());
            }
        }
        // No prefix: check for a default namespace (xmlns="...").
        // An empty string means xmlns="" — null namespace override, which is valid.
        if let Some(default_ns) = ns_map.get("") {
            return QualifiedName::new(default_ns.clone(), name_str.to_string());
        }
        // Fallback: no prefix or unknown prefix → use legacy extraction
        QualifiedName::new(
            Self::extract_namespace(name_str),
            Self::extract_local_name(name_str),
        )
    }

    /// Parse a PROPFIND XML request
    pub fn parse_propfind<R: Read>(reader: R) -> Result<PropFindRequest> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_propfind = false;
        let mut saw_propfind_close = false;
        let mut in_prop = false;
        let mut in_allprop = false;
        let mut in_propname = false;
        let mut props = Vec::new();
        let mut ns_map = std::collections::HashMap::<String, String>::new();

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    Self::check_ns_decls_valid(e)?;
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if name_str == "propfind" || name_str.ends_with(":propfind") {
                        in_propfind = true;
                    } else if in_propfind && (name_str == "prop" || name_str.ends_with(":prop")) {
                        in_prop = true;
                    } else if in_propfind
                        && (name_str == "allprop" || name_str.ends_with(":allprop"))
                    {
                        in_allprop = true;
                    } else if in_propfind
                        && (name_str == "propname" || name_str.ends_with(":propname"))
                    {
                        in_propname = true;
                    } else if in_prop {
                        let qname = Self::resolve_name(name_str, &ns_map);
                        props.push(qname);
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if name_str == "propfind" || name_str.ends_with(":propfind") {
                        in_propfind = false;
                        saw_propfind_close = true;
                    } else if name_str == "prop" || name_str.ends_with(":prop") {
                        in_prop = false;
                    } else if name_str == "allprop" || name_str.ends_with(":allprop") {
                        in_allprop = false;
                    } else if name_str == "propname" || name_str.ends_with(":propname") {
                        in_propname = false;
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    Self::check_ns_decls_valid(e)?;
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if in_propfind && (name_str == "allprop" || name_str.ends_with(":allprop")) {
                        in_allprop = true;
                    } else if in_propfind
                        && (name_str == "propname" || name_str.ends_with(":propname"))
                    {
                        in_propname = true;
                    } else if in_prop {
                        let qname = Self::resolve_name(name_str, &ns_map);
                        props.push(qname);
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }

            buffer.clear();
        }

        // RFC 4918 §8.1: non-well-formed XML MUST produce 400. quick-xml is
        // lenient about EOF-inside-element (no XmlError on unclosed tags), so
        // check explicitly: body must contain a complete <propfind>…</propfind>.
        if !saw_propfind_close {
            return Err(WebDavError::ParseError(
                "PROPFIND body is not well-formed XML: missing or unclosed <propfind> element"
                    .to_string(),
            ));
        }

        let prop_find_type = if in_allprop {
            PropFindType::AllProp
        } else if in_propname {
            PropFindType::PropName
        } else {
            PropFindType::Prop(props)
        };

        Ok(PropFindRequest { prop_find_type })
    }

    /// `quota` reflects whether the caller could resolve the account's
    /// storage quota for this request (the quota service is optional —
    /// `OXICLOUD_ENABLE_*` feature flags can disable it) and, independently,
    /// whether the account has a finite available-bytes figure to report.
    /// RFC 4331's `quota-used-bytes` / `quota-available-bytes` are each only
    /// reported as known properties when a value actually exists —
    /// otherwise they fall through to the standard 404 propstat like any
    /// other property this server doesn't support. Unlimited accounts have
    /// `quota-used-bytes` known but `quota-available-bytes` unknown (see
    /// `resolve_quota` in `webdav_handler.rs`).
    fn folder_prop_is_known(prop: &QualifiedName, quota: Option<(i64, Option<i64>)>) -> bool {
        if prop.namespace != "DAV:" {
            return false;
        }
        match prop.name.as_str() {
            "resourcetype" | "displayname" | "creationdate" | "getlastmodified" | "getetag"
            | "getcontentlength" | "getcontenttype" => true,
            "quota-used-bytes" => quota.is_some(),
            "quota-available-bytes" => quota.is_some_and(|(_, available)| available.is_some()),
            _ => false,
        }
    }

    /// Parse an RFC 6578 `<D:sync-collection>` REPORT request.
    ///
    /// `sync_token` is `None` for an initial sync (empty or absent
    /// `<D:sync-token/>`) — callers treat both the initial case and any
    /// non-empty token identically, since this server always returns a
    /// full listing (see [`WebDavAdapter::generate_sync_collection_response`]
    /// docs for why: there's no change-tracking store backing incremental
    /// sync here, matching the CalDAV/CardDAV sync-collection REPORTs,
    /// which have the same limitation).
    pub fn parse_sync_collection<R: Read>(reader: R) -> Result<SyncCollectionRequest> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_prop = false;
        let mut in_sync_token = false;
        let mut in_allprop = false;
        let mut in_propname = false;
        let mut props = Vec::new();
        let mut sync_token = String::new();
        let mut ns_map = std::collections::HashMap::<String, String>::new();

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if name_str == "prop" || name_str.ends_with(":prop") {
                        in_prop = true;
                    } else if name_str == "sync-token" || name_str.ends_with(":sync-token") {
                        in_sync_token = true;
                    } else if name_str == "allprop" || name_str.ends_with(":allprop") {
                        in_allprop = true;
                    } else if name_str == "propname" || name_str.ends_with(":propname") {
                        in_propname = true;
                    } else if in_prop {
                        let qname = Self::resolve_name(name_str, &ns_map);
                        props.push(qname);
                    }
                }
                Ok(Event::Text(e)) if in_sync_token => {
                    sync_token.push_str(&e.decode().unwrap_or_default());
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if name_str == "prop" || name_str.ends_with(":prop") {
                        in_prop = false;
                    } else if name_str == "sync-token" || name_str.ends_with(":sync-token") {
                        in_sync_token = false;
                    } else if name_str == "allprop" || name_str.ends_with(":allprop") {
                        in_allprop = false;
                    } else if name_str == "propname" || name_str.ends_with(":propname") {
                        in_propname = false;
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if name_str == "allprop" || name_str.ends_with(":allprop") {
                        in_allprop = true;
                    } else if name_str == "propname" || name_str.ends_with(":propname") {
                        in_propname = true;
                    } else if in_prop {
                        let qname = Self::resolve_name(name_str, &ns_map);
                        props.push(qname);
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }
            buffer.clear();
        }

        let prop_find_type = if in_allprop {
            PropFindType::AllProp
        } else if in_propname {
            PropFindType::PropName
        } else {
            PropFindType::Prop(props)
        };

        Ok(SyncCollectionRequest {
            sync_token: if sync_token.is_empty() {
                None
            } else {
                Some(sync_token)
            },
            request: PropFindRequest { prop_find_type },
        })
    }

    fn file_prop_is_known(prop: &QualifiedName) -> bool {
        prop.namespace == "DAV:"
            && matches!(
                prop.name.as_str(),
                "resourcetype"
                    | "displayname"
                    | "getcontenttype"
                    | "getcontentlength"
                    | "creationdate"
                    | "getlastmodified"
                    | "getetag"
            )
    }

    /// Write a single qualified name as an empty XML element with proper namespace declaration.
    ///
    /// DAV: props use the `D:` prefix (already declared on the root element).
    /// All other namespaces get a local `xmlns:X` declaration on the element itself.
    fn write_qname_empty<W: Write>(xml_writer: &mut Writer<W>, prop: &QualifiedName) -> Result<()> {
        if prop.namespace.is_empty() {
            xml_writer.write_event(Event::Empty(BytesStart::new(prop.name.as_str())))?;
        } else if prop.namespace == "DAV:" {
            xml_writer.write_event(Event::Empty(BytesStart::new(format!("D:{}", prop.name))))?;
        } else {
            let tag = format!("X:{}", prop.name);
            let mut start = BytesStart::new(tag.as_str());
            start.push_attribute(("xmlns:X", prop.namespace.as_str()));
            xml_writer.write_event(Event::Empty(start))?;
        }
        Ok(())
    }

    /// Write a 404 propstat block for unknown properties (RFC 4918 §9.2).
    fn write_unknown_props_404<W: Write>(
        xml_writer: &mut Writer<W>,
        unknown: &[&QualifiedName],
    ) -> Result<()> {
        if unknown.is_empty() {
            return Ok(());
        }
        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
        for prop in unknown {
            Self::write_qname_empty(xml_writer, prop)?;
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 404 Not Found")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        Ok(())
    }

    /// Write a dead-property propstat block (RFC 4918 §4.2).
    ///
    /// Written AFTER the live-property propstats inside a `<D:response>`.
    /// Only emitted when `dead_props` is non-empty.
    ///
    /// `pub(crate)` so the NextCloud-compatible handler
    /// (`interfaces::nextcloud::webdav_handler`) can append the same
    /// dead-property block to its own bespoke PROPFIND writers instead
    /// of duplicating this XML shape.
    pub(crate) fn write_dead_props_propstat<W: Write>(
        xml_writer: &mut Writer<W>,
        dead_props: &[(QualifiedName, Option<String>)],
    ) -> Result<()> {
        if dead_props.is_empty() {
            return Ok(());
        }
        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
        for (name, value) in dead_props {
            let tag = if name.namespace.is_empty() {
                name.name.clone()
            } else {
                format!("X:{}", name.name)
            };
            let mut start = BytesStart::new(tag.as_str());
            if !name.namespace.is_empty() {
                start.push_attribute(("xmlns:X", name.namespace.as_str()));
            }
            match value {
                Some(v) if !v.is_empty() => {
                    xml_writer.write_event(Event::Start(start))?;
                    xml_writer.write_event(Event::Text(BytesText::new(v)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new(tag.as_str())))?;
                }
                _ => {
                    xml_writer.write_event(Event::Empty(start))?;
                }
            }
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        Ok(())
    }

    /// Write folder properties as a response
    fn write_folder_response<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        request: &PropFindRequest,
        href: &str,
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        Self::write_folder_response_with_dead_props(xml_writer, folder, request, href, &[], quota)
    }

    /// `quota` is `Some((used_bytes, available_bytes))` for the caller's
    /// account when the quota subsystem is enabled and reachable —
    /// `available_bytes` is itself `None` for unlimited accounts, which
    /// omits `quota-available-bytes` from the response entirely (see
    /// [`Self::folder_prop_is_known`]). It's the same value regardless of
    /// which folder is being described (quota is account-wide, not
    /// per-folder), so callers resolve it once per PROPFIND request.
    fn write_folder_response_with_dead_props<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        // Compute dead props first so we can exclude them from the 404 propstat.
        let relevant_dead: Vec<_> = match &request.prop_find_type {
            PropFindType::Prop(requested) => dead_props
                .iter()
                .filter(|(name, _)| requested.iter().any(|r| r == name))
                .cloned()
                .collect(),
            PropFindType::AllProp => dead_props.to_vec(),
            PropFindType::PropName => vec![],
        };
        let dead_name_set: std::collections::HashSet<&QualifiedName> =
            relevant_dead.iter().map(|(n, _)| n).collect();

        match &request.prop_find_type {
            PropFindType::Prop(props) => {
                // RFC 4918 §9.2: known props → 200 propstat; unknown → 404 propstat.
                // Props found in the dead store are returned in the dead 200 propstat,
                // so exclude them from the 404 propstat to avoid duplicate reporting.
                // Single pass: the requested-props writer skips unknown
                // names itself (its match arms mirror
                // `folder_prop_is_known` exactly), so only the usually
                // empty 404 list needs materialising — the old
                // `partition` built two throwaway Vecs per row.
                let truly_unknown: Vec<_> = props
                    .iter()
                    .filter(|p| !Self::folder_prop_is_known(p, quota) && !dead_name_set.contains(p))
                    .collect();

                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                Self::write_folder_requested_props(xml_writer, folder, props, quota)?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

                Self::write_unknown_props_404(xml_writer, &truly_unknown)?;
            }
            other => {
                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                match other {
                    PropFindType::AllProp => {
                        Self::write_folder_standard_props(xml_writer, folder, quota)?;
                    }
                    PropFindType::PropName => {
                        Self::write_folder_prop_names(xml_writer, quota)?;
                    }
                    PropFindType::Prop(_) => unreachable!(),
                }
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
            }
        }

        // Dead properties — written as a separate 200 propstat (RFC 4918 §4.2).
        Self::write_dead_props_propstat(xml_writer, &relevant_dead)?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    /// Write file properties as a response
    fn write_file_response<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
        request: &PropFindRequest,
        href: &str,
    ) -> Result<()> {
        Self::write_file_response_with_dead_props(xml_writer, file, request, href, &[])
    }

    fn write_file_response_with_dead_props<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        // Compute dead props first so we can exclude them from the 404 propstat.
        let relevant_dead: Vec<_> = match &request.prop_find_type {
            PropFindType::Prop(requested) => dead_props
                .iter()
                .filter(|(name, _)| requested.iter().any(|r| r == name))
                .cloned()
                .collect(),
            PropFindType::AllProp => dead_props.to_vec(),
            PropFindType::PropName => vec![],
        };
        let dead_name_set: std::collections::HashSet<&QualifiedName> =
            relevant_dead.iter().map(|(n, _)| n).collect();

        match &request.prop_find_type {
            PropFindType::Prop(props) => {
                // RFC 4918 §9.2: known props → 200 propstat; unknown → 404 propstat.
                // Props found in the dead store are returned in the dead 200 propstat,
                // so exclude them from the 404 propstat to avoid duplicate reporting.
                // Single pass: the requested-props writer skips unknown
                // names itself (its match arms mirror `file_prop_is_known`
                // exactly), so only the usually empty 404 list needs
                // materialising — the old `partition` built two throwaway
                // Vecs per row.
                let truly_unknown: Vec<_> = props
                    .iter()
                    .filter(|p| !Self::file_prop_is_known(p) && !dead_name_set.contains(p))
                    .collect();

                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                Self::write_file_requested_props(xml_writer, file, props)?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

                Self::write_unknown_props_404(xml_writer, &truly_unknown)?;
            }
            other => {
                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                match other {
                    PropFindType::AllProp => {
                        Self::write_file_standard_props(xml_writer, file)?;
                    }
                    PropFindType::PropName => {
                        Self::write_file_prop_names(xml_writer)?;
                    }
                    PropFindType::Prop(_) => unreachable!(),
                }
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
            }
        }

        // Dead properties (RFC 4918 §4.2).
        Self::write_dead_props_propstat(xml_writer, &relevant_dead)?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    // ── Per-row formatted-value writers (stack-rendered) ─────────────
    //
    // PROPFIND emits two formatted dates, a size and a quoted etag for
    // EVERY row of every listing. `to_rfc3339()`/`to_rfc2822()` ran
    // chrono's format-spec interpreter and allocated a String each;
    // `to_string()`/`format!` added two more. These render the same
    // bytes from stack buffers (`common::fmt`); out-of-range timestamps
    // keep the old chrono path as a byte-identical fallback.

    fn write_creationdate<W: Write>(xml_writer: &mut Writer<W>, secs: u64) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:creationdate")))?;
        let secs = secs as i64;
        let mut buf = [0u8; 25];
        match crate::common::fmt::rfc3339_utc(&mut buf, secs) {
            Some(s) => xml_writer.write_event(Event::Text(BytesText::new(s)))?,
            None => {
                let s = chrono::DateTime::<Utc>::from_timestamp(secs, 0)
                    .unwrap_or_else(Utc::now)
                    .to_rfc3339();
                xml_writer.write_event(Event::Text(BytesText::new(&s)))?;
            }
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:creationdate")))?;
        Ok(())
    }

    fn write_lastmodified<W: Write>(xml_writer: &mut Writer<W>, secs: u64) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        let secs = secs as i64;
        let mut buf = [0u8; 31];
        match crate::common::fmt::rfc2822_utc(&mut buf, secs) {
            Some(s) => xml_writer.write_event(Event::Text(BytesText::new(s)))?,
            None => {
                let s = chrono::DateTime::<Utc>::from_timestamp(secs, 0)
                    .unwrap_or_else(Utc::now)
                    .to_rfc2822();
                xml_writer.write_event(Event::Text(BytesText::new(&s)))?;
            }
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
        Ok(())
    }

    fn write_etag_quoted<W: Write>(xml_writer: &mut Writer<W>, etag: &str) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        // Borrowed pre-escaped quotes (the ROUND20 §C1 NextCloud / ROUND21 §R4
        // CardDAV pattern the native WebDAV adapter never got): `BytesText::new`
        // escapes a literal `"` → `&quot;`, re-allocating an owned `Cow`, so the
        // old `"{etag}"` String paid TWO allocs/row (the sized buffer + the
        // escape). Emit the two quotes as borrowed pre-escaped `&quot;` text
        // events around the escaped body — byte-identical output, 0 allocs/row
        // on the hottest native-WebDAV PROPFIND path (per file AND per folder,
        // up to PROPFIND_BATCH_SIZE=500 rows/page).
        xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
        xml_writer.write_event(Event::Text(BytesText::new(etag)))?;
        xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
        Ok(())
    }

    fn write_contentlength<W: Write>(xml_writer: &mut Writer<W>, size: u64) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
        let mut buf = [0u8; 20];
        xml_writer.write_event(Event::Text(BytesText::new(crate::common::fmt::u64_str(
            &mut buf, size,
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;
        Ok(())
    }

    /// Write standard folder properties
    fn write_folder_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        // Resource type (collection)
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        // Display name
        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&folder.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        // Creation date
        Self::write_creationdate(xml_writer, folder.created_at)?;

        // Last modified
        Self::write_lastmodified(xml_writer, folder.modified_at)?;

        // ETag — routes through `FolderDto::etag` (= `Folder::etag()`)
        // so every WebDAV emitter and HEAD response agree on a single
        // value for the same folder.
        Self::write_etag_quoted(xml_writer, &folder.etag)?;

        // Content length (0 for directories)
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
        xml_writer.write_event(Event::Text(BytesText::new("0")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;

        // Content type for directories
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new("httpd/unix-directory")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        if let Some((used, available)) = quota {
            Self::write_quota_props(xml_writer, used, available)?;
        }

        Ok(())
    }

    /// Write RFC 4331 `quota-used-bytes` / `quota-available-bytes`. Shared
    /// by the allprop and named-prop paths so the element shape only
    /// lives in one place. `available_bytes` is `None` for unlimited
    /// accounts — RFC 4331 §3 lets a server omit `quota-available-bytes`
    /// rather than disclose a made-up value, so the element is skipped.
    fn write_quota_props<W: Write>(
        xml_writer: &mut Writer<W>,
        used_bytes: i64,
        available_bytes: Option<i64>,
    ) -> Result<()> {
        let mut buf = [0u8; 21];
        xml_writer.write_event(Event::Start(BytesStart::new("D:quota-used-bytes")))?;
        xml_writer.write_event(Event::Text(BytesText::new(crate::common::fmt::i64_str(
            &mut buf, used_bytes,
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:quota-used-bytes")))?;

        if let Some(available_bytes) = available_bytes {
            xml_writer.write_event(Event::Start(BytesStart::new("D:quota-available-bytes")))?;
            xml_writer.write_event(Event::Text(BytesText::new(crate::common::fmt::i64_str(
                &mut buf,
                available_bytes,
            ))))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:quota-available-bytes")))?;
        }

        Ok(())
    }

    /// Write standard file properties
    fn write_file_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
    ) -> Result<()> {
        // Resource type (empty for files)
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

        // Display name
        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&file.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        // Content type
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&file.mime_type)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        // Content length
        Self::write_contentlength(xml_writer, file.size)?;

        // Creation date
        Self::write_creationdate(xml_writer, file.created_at)?;

        // Last modified
        Self::write_lastmodified(xml_writer, file.modified_at)?;

        // ETag — routes through `FileDto::etag` (= `File::etag()`) so
        // PROPFIND, GET, HEAD, PUT-response, and MOVE all emit
        // byte-identical values for the same file.
        Self::write_etag_quoted(xml_writer, &file.etag)?;

        Ok(())
    }

    /// Write folder property names
    fn write_folder_prop_names<W: Write>(
        xml_writer: &mut Writer<W>,
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        // Write empty property elements for folders
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:creationdate")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getlastmodified")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getetag")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontentlength")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontenttype")))?;
        if let Some((_, available)) = quota {
            xml_writer.write_event(Event::Empty(BytesStart::new("D:quota-used-bytes")))?;
            if available.is_some() {
                xml_writer.write_event(Event::Empty(BytesStart::new("D:quota-available-bytes")))?;
            }
        }

        Ok(())
    }

    /// Write file property names
    fn write_file_prop_names<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        // Write empty property elements for files
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontentlength")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:creationdate")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getlastmodified")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getetag")))?;

        Ok(())
    }

    /// Write requested folder properties
    fn write_folder_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        props: &[QualifiedName],
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        for prop in props {
            if prop.namespace == "DAV:" {
                match prop.name.as_str() {
                    "resourcetype" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                    }
                    "displayname" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&folder.name)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                    }
                    "creationdate" => {
                        Self::write_creationdate(xml_writer, folder.created_at)?;
                    }
                    "getlastmodified" => {
                        Self::write_lastmodified(xml_writer, folder.modified_at)?;
                    }
                    "getetag" => {
                        Self::write_etag_quoted(xml_writer, &folder.etag)?;
                    }
                    "getcontentlength" => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
                        xml_writer.write_event(Event::Text(BytesText::new("0")))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;
                    }
                    "getcontenttype" => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                        xml_writer
                            .write_event(Event::Text(BytesText::new("httpd/unix-directory")))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                    }
                    "quota-used-bytes" => {
                        if let Some((used, _)) = quota {
                            let mut buf = [0u8; 21];
                            xml_writer
                                .write_event(Event::Start(BytesStart::new("D:quota-used-bytes")))?;
                            xml_writer.write_event(Event::Text(BytesText::new(
                                crate::common::fmt::i64_str(&mut buf, used),
                            )))?;
                            xml_writer
                                .write_event(Event::End(BytesEnd::new("D:quota-used-bytes")))?;
                        }
                    }
                    "quota-available-bytes" => {
                        if let Some((_, Some(available))) = quota {
                            let mut buf = [0u8; 21];
                            xml_writer.write_event(Event::Start(BytesStart::new(
                                "D:quota-available-bytes",
                            )))?;
                            xml_writer.write_event(Event::Text(BytesText::new(
                                crate::common::fmt::i64_str(&mut buf, available),
                            )))?;
                            xml_writer.write_event(Event::End(BytesEnd::new(
                                "D:quota-available-bytes",
                            )))?;
                        }
                    }
                    _ => {
                        // Unknown prop — skipped here; caller writes 404 propstat.
                    }
                }
            }
            // Non-DAV namespace props are unknown — skipped; caller writes 404 propstat.
        }

        Ok(())
    }

    /// Write requested file properties
    fn write_file_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            if prop.namespace == "DAV:" {
                match prop.name.as_str() {
                    "resourcetype" => {
                        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
                    }
                    "displayname" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&file.name)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                    }
                    "getcontenttype" => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&file.mime_type)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                    }
                    "getcontentlength" => {
                        Self::write_contentlength(xml_writer, file.size)?;
                    }
                    "creationdate" => {
                        Self::write_creationdate(xml_writer, file.created_at)?;
                    }
                    "getlastmodified" => {
                        Self::write_lastmodified(xml_writer, file.modified_at)?;
                    }
                    "getetag" => {
                        Self::write_etag_quoted(xml_writer, &file.etag)?;
                    }
                    _ => {
                        // Unknown prop — skipped here; caller writes 404 propstat.
                    }
                }
            }
            // Non-DAV namespace props are unknown — skipped; caller writes 404 propstat.
        }

        Ok(())
    }

    /// Parse a PROPPATCH XML request.
    ///
    /// Returns operations in document order (RFC 4918 §9.2 requires document-order
    /// processing so that remove-then-set and set-then-remove yield different results).
    pub fn parse_proppatch<R: Read>(reader: R) -> Result<Vec<PropPatchOp>> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_propertyupdate = false;
        let mut in_set = false;
        let mut in_remove = false;
        let mut in_prop = false;
        let mut current_prop: Option<QualifiedName> = None;
        let mut ops: Vec<PropPatchOp> = Vec::new();
        let mut current_text = String::new();
        let mut ns_map = std::collections::HashMap::<String, String>::new();

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "propertyupdate" || s.ends_with(":propertyupdate") => {
                            in_propertyupdate = true
                        }
                        s if (in_propertyupdate && (s == "set" || s.ends_with(":set"))) => {
                            in_set = true
                        }
                        s if (in_propertyupdate && (s == "remove" || s.ends_with(":remove"))) => {
                            in_remove = true
                        }
                        s if ((in_set || in_remove) && (s == "prop" || s.ends_with(":prop"))) => {
                            in_prop = true
                        }
                        _ if in_prop => {
                            current_prop = Some(Self::resolve_name(name_str, &ns_map));
                            current_text.clear();
                        }
                        _ => (),
                    }
                }
                Ok(Event::Text(e)) if current_prop.is_some() => {
                    let raw = e.decode().unwrap_or_default();
                    let unescaped =
                        quick_xml::escape::unescape(&raw).unwrap_or_else(|_| raw.clone());
                    current_text.push_str(&unescaped);
                }
                Ok(Event::GeneralRef(ref e)) if current_prop.is_some() => {
                    // quick-xml 0.39 emits GeneralRef for character references like &#65536;
                    // and named entity references like &amp;. Resolve them to actual chars.
                    match e.resolve_char_ref() {
                        Ok(Some(ch)) => current_text.push(ch),
                        Ok(None) => {
                            if let Ok(name) = e.decode() {
                                match name.as_ref() {
                                    "amp" => current_text.push('&'),
                                    "lt" => current_text.push('<'),
                                    "gt" => current_text.push('>'),
                                    "apos" => current_text.push('\''),
                                    "quot" => current_text.push('"'),
                                    _ => {}
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "propertyupdate" || s.ends_with(":propertyupdate") => {
                            in_propertyupdate = false
                        }
                        s if s == "set" || s.ends_with(":set") => in_set = false,
                        s if s == "remove" || s.ends_with(":remove") => in_remove = false,
                        s if s == "prop" || s.ends_with(":prop") => in_prop = false,
                        _ if in_prop => {
                            if let Some(prop_name) = current_prop.take() {
                                if in_set {
                                    ops.push(PropPatchOp::Set(PropValue {
                                        name: prop_name,
                                        value: if current_text.is_empty() {
                                            None
                                        } else {
                                            Some(current_text.clone())
                                        },
                                    }));
                                } else if in_remove {
                                    ops.push(PropPatchOp::Remove(prop_name));
                                }
                            }
                            current_text.clear();
                        }
                        _ => (),
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    Self::check_attribute_cap(e)?;
                    Self::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if in_prop {
                        let qname = Self::resolve_name(name_str, &ns_map);

                        if in_set {
                            ops.push(PropPatchOp::Set(PropValue {
                                name: qname,
                                value: None,
                            }));
                        } else if in_remove {
                            ops.push(PropPatchOp::Remove(qname));
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }

            buffer.clear();
        }

        Ok(ops)
    }

    /// Generate a PROPPATCH response
    pub fn generate_proppatch_response<W: Write>(
        writer: W,
        href: &str,
        results: &[(&QualifiedName, bool)],
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        // Start multistatus response
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([("xmlns:D", "DAV:")]),
        ))?;

        // Start response element
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        // Write href
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        // Group results by status
        let mut success_props = Vec::new();
        let mut failed_props = Vec::new();

        for (prop, success) in results {
            if *success {
                success_props.push(prop);
            } else {
                failed_props.push(prop);
            }
        }

        // Write successful properties
        if !success_props.is_empty() {
            xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;

            // Start prop
            xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

            // Write property names
            for prop in success_props {
                Self::write_qname_empty(&mut xml_writer, prop)?;
            }

            // End prop
            xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

            // Write status
            xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
            xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

            // End propstat
            xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        }

        // Write failed properties
        if !failed_props.is_empty() {
            xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;

            // Start prop
            xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

            // Write property names
            for prop in failed_props {
                Self::write_qname_empty(&mut xml_writer, prop)?;
            }

            // End prop
            xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

            // Write status
            xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
            xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 403 Forbidden")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

            // End propstat
            xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        }

        // End response
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        // End multistatus
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;

        Ok(())
    }

    /// Parse a LOCK XML request
    pub fn parse_lockinfo<R: Read>(reader: R) -> Result<(LockScope, LockType, Option<String>)> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_lockinfo = false;
        let mut in_lockscope = false;
        let mut in_locktype = false;
        let mut in_owner = false;
        let mut owner_text = String::new();
        let mut scope = LockScope::Exclusive; // Default to exclusive
        let mut type_ = LockType::Write; // Default to write (only supported type)

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "lockinfo" || s.ends_with(":lockinfo") => in_lockinfo = true,
                        s if in_lockinfo && (s == "lockscope" || s.ends_with(":lockscope")) => {
                            in_lockscope = true
                        }
                        s if in_lockinfo && (s == "locktype" || s.ends_with(":locktype")) => {
                            in_locktype = true
                        }
                        s if in_lockinfo && (s == "owner" || s.ends_with(":owner")) => {
                            in_owner = true
                        }
                        s if in_lockscope && (s == "exclusive" || s.ends_with(":exclusive")) => {
                            scope = LockScope::Exclusive
                        }
                        s if in_lockscope && (s == "shared" || s.ends_with(":shared")) => {
                            scope = LockScope::Shared
                        }
                        s if in_locktype && (s == "write" || s.ends_with(":write")) => {
                            type_ = LockType::Write
                        }
                        _ => (),
                    }
                }
                Ok(Event::Text(e)) if in_owner => {
                    owner_text.push_str(&e.decode().unwrap_or_default());
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "lockinfo" || s.ends_with(":lockinfo") => in_lockinfo = false,
                        s if s == "lockscope" || s.ends_with(":lockscope") => in_lockscope = false,
                        s if s == "locktype" || s.ends_with(":locktype") => in_locktype = false,
                        s if s == "owner" || s.ends_with(":owner") => in_owner = false,
                        _ => (),
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if in_lockscope && (s == "exclusive" || s.ends_with(":exclusive")) => {
                            scope = LockScope::Exclusive
                        }
                        s if in_lockscope && (s == "shared" || s.ends_with(":shared")) => {
                            scope = LockScope::Shared
                        }
                        s if in_locktype && (s == "write" || s.ends_with(":write")) => {
                            type_ = LockType::Write
                        }
                        _ => (),
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }

            buffer.clear();
        }

        let owner = if owner_text.is_empty() {
            None
        } else {
            Some(owner_text)
        };

        Ok((scope, type_, owner))
    }

    /// Generate a LOCK response (lockdiscovery)
    pub fn generate_lock_response<W: Write>(
        writer: W,
        lock_info: &LockInfo,
        href: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        // Start prop element (direct response, not multistatus)
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:prop").with_attributes([("xmlns:D", "DAV:")]),
        ))?;

        // Start lockdiscovery
        xml_writer.write_event(Event::Start(BytesStart::new("D:lockdiscovery")))?;

        // Start activelock
        xml_writer.write_event(Event::Start(BytesStart::new("D:activelock")))?;

        // Write locktype
        xml_writer.write_event(Event::Start(BytesStart::new("D:locktype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:write")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:locktype")))?;

        // Write lockscope
        xml_writer.write_event(Event::Start(BytesStart::new("D:lockscope")))?;
        match lock_info.scope {
            LockScope::Exclusive => {
                xml_writer.write_event(Event::Empty(BytesStart::new("D:exclusive")))?;
            }
            LockScope::Shared => {
                xml_writer.write_event(Event::Empty(BytesStart::new("D:shared")))?;
            }
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:lockscope")))?;

        // Write depth
        xml_writer.write_event(Event::Start(BytesStart::new("D:depth")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&lock_info.depth)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:depth")))?;

        // Write owner (if provided)
        if let Some(owner) = &lock_info.owner {
            xml_writer.write_event(Event::Start(BytesStart::new("D:owner")))?;
            xml_writer.write_event(Event::Text(BytesText::new(owner)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:owner")))?;
        }

        // Write timeout (if provided)
        if let Some(timeout) = &lock_info.timeout {
            xml_writer.write_event(Event::Start(BytesStart::new("D:timeout")))?;
            xml_writer.write_event(Event::Text(BytesText::new(timeout)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:timeout")))?;
        }

        // Write locktoken
        xml_writer.write_event(Event::Start(BytesStart::new("D:locktoken")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&lock_info.token)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:locktoken")))?;

        // Write lockroot
        xml_writer.write_event(Event::Start(BytesStart::new("D:lockroot")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:lockroot")))?;

        // End activelock, lockdiscovery, and prop
        xml_writer.write_event(Event::End(BytesEnd::new("D:activelock")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:lockdiscovery")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        Ok(())
    }

    /// Helper method to extract namespace from tag name
    pub fn extract_namespace(name: &str) -> String {
        if let Some(idx) = name.rfind(':')
            && idx > 0
        {
            return name[..idx].to_string();
        }
        // Default namespace for WebDAV
        "DAV:".to_string()
    }

    /// Helper method to extract local name from tag name
    pub fn extract_local_name(name: &str) -> String {
        if let Some(idx) = name.rfind(':')
            && idx > 0
            && idx < name.len() - 1
        {
            return name[idx + 1..].to_string();
        }
        name.to_string()
    }

    // ─────────────────────────────────────────────────────────────
    // Streaming PROPFIND helpers
    //
    // These methods write incremental XML fragments so the caller
    // can flush chunks to the HTTP body without buffering the whole
    // response in memory.
    // ─────────────────────────────────────────────────────────────

    /// Writes the opening `<D:multistatus>` tag.
    pub fn write_multistatus_start<W: Write>(writer: &mut Writer<W>) -> Result<()> {
        writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([("xmlns:D", "DAV:")]),
        ))?;
        Ok(())
    }

    /// Writes the closing `</D:multistatus>` tag.
    pub fn write_multistatus_end<W: Write>(writer: &mut Writer<W>) -> Result<()> {
        writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// Writes a single `<D:response>` element for a folder.
    pub fn write_folder_entry<W: Write>(
        writer: &mut Writer<W>,
        folder: &FolderDto,
        request: &PropFindRequest,
        href: &str,
    ) -> Result<()> {
        Self::write_folder_response(writer, folder, request, href, None)
    }

    /// Writes a single `<D:response>` element for a file, including dead properties.
    pub fn write_file_entry<W: Write>(
        writer: &mut Writer<W>,
        file: &FileDto,
        request: &PropFindRequest,
        href: &str,
    ) -> Result<()> {
        Self::write_file_response(writer, file, request, href)
    }

    /// Writes a folder entry including dead (custom) properties.
    pub fn write_folder_entry_with_dead_props<W: Write>(
        writer: &mut Writer<W>,
        folder: &FolderDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        Self::write_folder_response_with_dead_props(
            writer, folder, request, href, dead_props, quota,
        )
    }

    /// Writes a file entry including dead (custom) properties.
    pub fn write_file_entry_with_dead_props<W: Write>(
        writer: &mut Writer<W>,
        file: &FileDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
    ) -> Result<()> {
        Self::write_file_response_with_dead_props(writer, file, request, href, dead_props)
    }

    /// Generate the RFC 6578 §3.7 sync-collection response for the
    /// plain-file WebDAV surface: a `<D:multistatus>` listing every
    /// current member of the collection (subfolders + files), followed
    /// by a `<D:sync-token>`.
    ///
    /// **This is a full re-sync every time, not incremental** — there is
    /// no change-tracking store behind it (same limitation the existing
    /// CalDAV/CardDAV sync-collection REPORTs have: they parse a
    /// `sync-token` but never act on it either). `sync_token` here is
    /// freshly minted per response (current timestamp) so clients get a
    /// spec-shaped round-trip and can detect that a resync occurred, but
    /// every request — initial or with a prior token — returns the
    /// complete current member list. A real incremental implementation
    /// would need a persistent change log keyed by folder, which is a
    /// separate, larger effort than this gap-fill.
    /// `subfolders`/`files` pair each resource with its already-encoded
    /// href — encoding is the handler's job (see `encode_path_segment` in
    /// `webdav_handler.rs`), matching how every other `write_*_entry*`
    /// caller in this module is fed pre-built hrefs.
    pub fn generate_sync_collection_response<W: Write>(
        writer: W,
        subfolders: &[(FolderDto, String)],
        files: &[(FileDto, String)],
        request: &PropFindRequest,
        sync_token: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([("xmlns:D", "DAV:")]),
        ))?;

        for (subfolder, href) in subfolders {
            Self::write_folder_entry(&mut xml_writer, subfolder, request, href)?;
        }
        for (file, href) in files {
            Self::write_file_entry(&mut xml_writer, file, request, href)?;
        }

        xml_writer.write_event(Event::Start(BytesStart::new("D:sync-token")))?;
        xml_writer.write_event(Event::Text(BytesText::new(sync_token)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:sync-token")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }
}

#[cfg(test)]
mod sync_collection_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_sync_collection_initial_request_has_no_token() {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:sync-level>1</D:sync-level>
  <D:prop>
    <D:getetag/>
    <D:getcontentlength/>
  </D:prop>
</D:sync-collection>"#;

        let parsed = WebDavAdapter::parse_sync_collection(Cursor::new(xml)).expect("parse");
        assert!(parsed.sync_token.is_none());
        match parsed.request.prop_find_type {
            PropFindType::Prop(props) => assert_eq!(props.len(), 2),
            other => panic!("Expected Prop, got {:?}", other),
        }
    }

    #[test]
    fn parse_sync_collection_with_prior_token() {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>http://oxicloud.local/ns/sync/12345</D:sync-token>
  <D:prop><D:getetag/></D:prop>
</D:sync-collection>"#;

        let parsed = WebDavAdapter::parse_sync_collection(Cursor::new(xml)).expect("parse");
        assert_eq!(
            parsed.sync_token.as_deref(),
            Some("http://oxicloud.local/ns/sync/12345")
        );
    }

    #[test]
    fn parse_sync_collection_allprop() {
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:allprop/>
</D:sync-collection>"#;

        let parsed = WebDavAdapter::parse_sync_collection(Cursor::new(xml)).expect("parse");
        assert!(matches!(
            parsed.request.prop_find_type,
            PropFindType::AllProp
        ));
    }
}

/// Thin public wrappers over the private per-row PROPFIND writers so
/// `examples/bench_propfind_xml.rs` can measure them. Gated behind the
/// `bench` feature — adds nothing to prod builds.
#[cfg(feature = "bench")]
pub mod bench {
    use super::*;

    pub fn write_file_propfind_row<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
    ) -> Result<()> {
        WebDavAdapter::write_file_response_with_dead_props(
            xml_writer, file, request, href, dead_props,
        )
    }

    pub fn write_folder_propfind_row<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        request: &PropFindRequest,
        href: &str,
        dead_props: &[(QualifiedName, Option<String>)],
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        WebDavAdapter::write_folder_response_with_dead_props(
            xml_writer, folder, request, href, dead_props, quota,
        )
    }
}
