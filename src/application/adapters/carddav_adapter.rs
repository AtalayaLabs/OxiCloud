use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
/**
 * CardDAV Adapter Module
 *
 * This module provides conversion between CardDAV protocol XML structures and
 * OxiCloud domain objects. It handles parsing CardDAV request XML and generating
 * CardDAV response XML according to RFC 6352.
 */
use std::io::{BufReader, Read, Write};

use crate::application::adapters::webdav_adapter::{
    PropFindRequest, PropFindType, QualifiedName, Result, WebDavAdapter, WebDavError,
};
use crate::application::dtos::address_book_dto::AddressBookDto;
use crate::application::dtos::contact_dto::ContactDto;

/// Emit a WebDAV `getetag` value as `"…"` with the surrounding quotes written
/// as borrowed pre-escaped `&quot;` text events around the escaped etag body.
///
/// Byte-identical to escaping the whole `"{etag}"` String — `quick_xml` escapes
/// a literal `"` to `&quot;`, so the one-String form re-allocated an owned `Cow`
/// on write — but with **0 heap allocs per contact** (the NextCloud
/// `write_etag_element` pattern, benches/ROUND20.md §C1). Called per contact on
/// the CardDAV multiget/PROPFIND emit path.
fn write_quoted_etag<W: Write>(xml_writer: &mut Writer<W>, etag: &str) -> Result<()> {
    xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
    xml_writer.write_event(Event::Text(BytesText::new(etag)))?;
    xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
    Ok(())
}

/// Render a requested property as a namespaced response element name, mapping
/// the known namespaces to their response prefixes (`D:` for DAV, `CR:` for
/// CardDAV). Used for the catch-all arms of the requested-property writers so
/// the prefix mapping lives in exactly one place.
fn carddav_prop_name(prop: &QualifiedName) -> String {
    if prop.namespace == "urn:ietf:params:xml:ns:carddav" {
        format!("CR:{}", prop.name)
    } else if prop.namespace == "DAV:" {
        format!("D:{}", prop.name)
    } else {
        prop.name.clone()
    }
}

/// CardDAV report type
#[derive(Debug, PartialEq)]
pub enum CardDavReportType {
    /// Addressbook-query report
    AddressbookQuery { props: Vec<QualifiedName> },
    /// Addressbook-multiget report
    AddressbookMultiget {
        hrefs: Vec<String>,
        props: Vec<QualifiedName>,
    },
    /// Sync-collection report
    SyncCollection {
        sync_token: String,
        props: Vec<QualifiedName>,
    },
}

/// CardDAV adapter for XML parsing/generation
pub struct CardDavAdapter;

impl CardDavAdapter {
    /// Parse a REPORT XML request for CardDAV
    pub fn parse_report<R: Read>(reader: R) -> Result<CardDavReportType> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_addressbook_query = false;
        let mut in_addressbook_multiget = false;
        let mut in_sync_collection = false;
        let mut in_prop = false;
        let mut props = Vec::new();
        let mut hrefs = Vec::new();
        let mut sync_token = String::new();
        let mut in_href = false;
        let mut in_sync_token = false;
        let mut ns_map = std::collections::HashMap::<String, String>::new();

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    WebDavAdapter::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "addressbook-query" || s.ends_with(":addressbook-query") => {
                            in_addressbook_query = true
                        }
                        s if s == "addressbook-multiget"
                            || s.ends_with(":addressbook-multiget") =>
                        {
                            in_addressbook_multiget = true
                        }
                        s if s == "sync-collection" || s.ends_with(":sync-collection") => {
                            in_sync_collection = true
                        }
                        s if s == "prop" || s.ends_with(":prop") => in_prop = true,
                        s if s == "href" || s.ends_with(":href") => in_href = true,
                        s if s == "sync-token" || s.ends_with(":sync-token") => {
                            in_sync_token = true
                        }
                        _ if in_prop => {
                            let qname = WebDavAdapter::resolve_name(name_str, &ns_map);
                            props.push(qname);
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(e)) => {
                    let text = e.decode().unwrap_or_default();
                    if in_href {
                        hrefs.push(text.to_string());
                    } else if in_sync_token {
                        sync_token = text.to_string();
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "prop" || s.ends_with(":prop") => in_prop = false,
                        s if s == "href" || s.ends_with(":href") => in_href = false,
                        s if s == "sync-token" || s.ends_with(":sync-token") => {
                            in_sync_token = false
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(ref e)) if in_prop => {
                    WebDavAdapter::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    let qname = WebDavAdapter::resolve_name(name_str, &ns_map);
                    props.push(qname);
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }
            buffer.clear();
        }

        if in_addressbook_multiget || !hrefs.is_empty() {
            Ok(CardDavReportType::AddressbookMultiget { hrefs, props })
        } else if in_sync_collection {
            Ok(CardDavReportType::SyncCollection { sync_token, props })
        } else if in_addressbook_query {
            Ok(CardDavReportType::AddressbookQuery { props })
        } else {
            // Default
            Ok(CardDavReportType::AddressbookQuery { props })
        }
    }

    /// Generate a PROPFIND response listing address books
    pub fn generate_addressbooks_propfind_response<W: Write>(
        writer: W,
        address_books: &[AddressBookDto],
        request: &PropFindRequest,
        base_href: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        for book in address_books {
            Self::write_addressbook_response(
                &mut xml_writer,
                book,
                request,
                &format!("{}{}/", base_href, book.id),
            )?;
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// Generate a PROPFIND response for the CardDAV root `/carddav/`.
    ///
    /// Mirrors the CalDAV root: emits a discovery entry for `/carddav/` itself
    /// advertising `current-user-principal` and `addressbook-home-set` (the
    /// properties DAVx5 / Apple Contacts read to locate address books), then —
    /// at Depth > 0 — one entry per address book. Without these discovery
    /// properties clients never find the address books at all.
    pub fn generate_root_propfind_response<W: Write>(
        writer: W,
        address_books: &[AddressBookDto],
        request: &PropFindRequest,
        base_href: &str,
        username: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        Self::write_carddav_root_response(&mut xml_writer, request, base_href, username)?;

        for book in address_books {
            Self::write_addressbook_response(
                &mut xml_writer,
                book,
                request,
                &format!("{}{}/", base_href, book.id),
            )?;
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// Generate a PROPFIND response for a CardDAV user principal resource at
    /// `/carddav/principals/{username}/`.
    ///
    /// Returns `addressbook-home-set` so clients can resolve the collection
    /// holding the user's address books, plus a self-referential
    /// `current-user-principal`.
    pub fn generate_principal_propfind_response<W: Write>(
        writer: W,
        request: &PropFindRequest,
        username: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        let href = format!("/carddav/principals/{}/", username);
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        match &request.prop_find_type {
            PropFindType::AllProp | PropFindType::PropName => {
                Self::write_carddav_principal_props(&mut xml_writer, username)?;
            }
            PropFindType::Prop(props) => {
                Self::write_carddav_principal_requested_props(&mut xml_writer, username, props)?;
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;

        Ok(())
    }

    /// Generate PROPFIND for a single address book collection + contacts
    pub fn generate_addressbook_collection_propfind<W: Write>(
        writer: W,
        address_book: &AddressBookDto,
        contacts: &[ContactDto],
        request: &PropFindRequest,
        base_href: &str,
        depth: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        Self::write_collection_head(&mut xml_writer, address_book, request, base_href)?;

        // Write contacts if depth > 0
        if depth != "0" {
            Self::write_collection_contact_page(&mut xml_writer, contacts, base_href)?;
        }

        Self::write_carddav_multistatus_end(&mut xml_writer)
    }

    /// Multistatus opening (DAV + CardDAV + CalendarServer namespaces)
    /// plus the address book's own `D:response` — the head of a depth-1
    /// collection PROPFIND. Streaming emitters call this once, then
    /// [`Self::write_collection_contact_page`] per cursor page, then
    /// [`Self::write_carddav_multistatus_end`].
    pub fn write_collection_head<W: Write>(
        xml_writer: &mut Writer<W>,
        address_book: &AddressBookDto,
        request: &PropFindRequest,
        base_href: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;
        Self::write_addressbook_response(xml_writer, address_book, request, base_href)
    }

    /// One depth-1 collection page of contact entries (standard props;
    /// href buffer reused across the page).
    pub fn write_collection_contact_page<W: Write>(
        xml_writer: &mut Writer<W>,
        contacts: &[ContactDto],
        base_href: &str,
    ) -> Result<()> {
        let mut href = String::with_capacity(base_href.len() + 48);
        for contact in contacts {
            href.clear();
            let _ = std::fmt::Write::write_fmt(
                &mut href,
                format_args!("{}{}.vcf", base_href, contact.uid),
            );
            Self::write_contact_response(xml_writer, contact, &[], &href)?;
        }
        Ok(())
    }

    /// Write address book properties
    fn write_addressbook_response<W: Write>(
        xml_writer: &mut Writer<W>,
        book: &AddressBookDto,
        request: &PropFindRequest,
        href: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        match &request.prop_find_type {
            PropFindType::AllProp => Self::write_addressbook_all_props(xml_writer, book)?,
            PropFindType::PropName => Self::write_addressbook_prop_names(xml_writer)?,
            PropFindType::Prop(props) => {
                Self::write_addressbook_requested_props(xml_writer, book, props)?
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        Ok(())
    }

    fn write_addressbook_all_props<W: Write>(
        xml_writer: &mut Writer<W>,
        book: &AddressBookDto,
    ) -> Result<()> {
        // resourcetype: collection + addressbook
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("CR:addressbook")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        // displayname
        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&book.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        // getlastmodified
        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&book.updated_at.to_rfc2822())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        // getetag
        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        write_quoted_etag(xml_writer, &book.id)?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

        // getcontenttype
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new("text/vcard")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        // supported-address-data
        xml_writer.write_event(Event::Start(BytesStart::new("CR:supported-address-data")))?;
        xml_writer.write_event(Event::Empty(
            BytesStart::new("CR:address-data-type")
                .with_attributes([("content-type", "text/vcard"), ("version", "3.0")]),
        ))?;
        xml_writer.write_event(Event::Empty(
            BytesStart::new("CR:address-data-type")
                .with_attributes([("content-type", "text/vcard"), ("version", "4.0")]),
        ))?;
        xml_writer.write_event(Event::End(BytesEnd::new("CR:supported-address-data")))?;

        // addressbook-description
        if let Some(ref desc) = book.description {
            xml_writer.write_event(Event::Start(BytesStart::new("CR:addressbook-description")))?;
            xml_writer.write_event(Event::Text(BytesText::new(desc)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("CR:addressbook-description")))?;
        }

        // current-user-privilege-set
        xml_writer.write_event(Event::Start(BytesStart::new(
            "D:current-user-privilege-set",
        )))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:read")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:write")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-privilege-set")))?;

        Ok(())
    }

    fn write_addressbook_prop_names<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getlastmodified")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getetag")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("CR:supported-address-data")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("CR:addressbook-description")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new(
            "D:current-user-privilege-set",
        )))?;
        Ok(())
    }

    fn write_addressbook_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        book: &AddressBookDto,
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("CR:addressbook")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                }
                ("DAV:", "displayname") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&book.name)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                }
                ("DAV:", "getlastmodified") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                    xml_writer
                        .write_event(Event::Text(BytesText::new(&book.updated_at.to_rfc2822())))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                }
                ("DAV:", "getetag") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                    write_quoted_etag(xml_writer, &book.id)?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                }
                ("DAV:", "getcontenttype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                    xml_writer.write_event(Event::Text(BytesText::new("text/vcard")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                }
                ("urn:ietf:params:xml:ns:carddav", "addressbook-description") => {
                    if let Some(ref desc) = book.description {
                        xml_writer.write_event(Event::Start(BytesStart::new(
                            "CR:addressbook-description",
                        )))?;
                        xml_writer.write_event(Event::Text(BytesText::new(desc)))?;
                        xml_writer
                            .write_event(Event::End(BytesEnd::new("CR:addressbook-description")))?;
                    } else {
                        xml_writer.write_event(Event::Empty(BytesStart::new(
                            "CR:addressbook-description",
                        )))?;
                    }
                }
                ("urn:ietf:params:xml:ns:carddav", "supported-address-data") => {
                    xml_writer
                        .write_event(Event::Start(BytesStart::new("CR:supported-address-data")))?;
                    xml_writer.write_event(Event::Empty(
                        BytesStart::new("CR:address-data-type")
                            .with_attributes([("content-type", "text/vcard"), ("version", "3.0")]),
                    ))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("CR:supported-address-data")))?;
                }
                ("DAV:", "current-user-privilege-set") => {
                    xml_writer.write_event(Event::Start(BytesStart::new(
                        "D:current-user-privilege-set",
                    )))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:read")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:write")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("D:current-user-privilege-set")))?;
                }
                _ => {
                    xml_writer
                        .write_event(Event::Empty(BytesStart::new(carddav_prop_name(prop))))?;
                }
            }
        }
        Ok(())
    }

    /// Write a populated `current-user-principal` element pointing at the user's
    /// CardDAV principal. Shared by the root and principal discovery responses.
    fn write_current_user_principal<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "/carddav/principals/{}/",
            username
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;
        Ok(())
    }

    /// Write a populated `addressbook-home-set` element pointing at the user's
    /// address-book home collection. Shared by the root and principal responses.
    fn write_addressbook_home_set<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("CR:addressbook-home-set")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "/carddav/{}/",
            username
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("CR:addressbook-home-set")))?;
        Ok(())
    }

    /// Write the root `/carddav/` discovery entry.
    fn write_carddav_root_response<W: Write>(
        xml_writer: &mut Writer<W>,
        request: &PropFindRequest,
        href: &str,
        username: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        match &request.prop_find_type {
            PropFindType::AllProp => {
                xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                Self::write_current_user_principal(xml_writer, username)?;
                Self::write_addressbook_home_set(xml_writer, username)?;
            }
            PropFindType::PropName => {
                xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
                xml_writer
                    .write_event(Event::Empty(BytesStart::new("D:current-user-principal")))?;
                xml_writer.write_event(Event::Empty(BytesStart::new("CR:addressbook-home-set")))?;
            }
            PropFindType::Prop(props) => {
                for prop in props {
                    match (prop.namespace.as_str(), prop.name.as_str()) {
                        ("DAV:", "resourcetype") => {
                            xml_writer
                                .write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                            xml_writer
                                .write_event(Event::Empty(BytesStart::new("D:collection")))?;
                            xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                        }
                        ("DAV:", "current-user-principal") => {
                            Self::write_current_user_principal(xml_writer, username)?;
                        }
                        ("urn:ietf:params:xml:ns:carddav", "addressbook-home-set") => {
                            Self::write_addressbook_home_set(xml_writer, username)?;
                        }
                        _ => {
                            xml_writer.write_event(Event::Empty(BytesStart::new(
                                carddav_prop_name(prop),
                            )))?;
                        }
                    }
                }
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    /// Write the standard properties for a CardDAV principal resource.
    fn write_carddav_principal_props<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:principal")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(username)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        Self::write_addressbook_home_set(xml_writer, username)?;
        Self::write_current_user_principal(xml_writer, username)?;
        Ok(())
    }

    /// Write the requested properties for a CardDAV principal resource.
    fn write_carddav_principal_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:principal")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                }
                ("DAV:", "displayname") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(username)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                }
                ("DAV:", "current-user-principal") => {
                    Self::write_current_user_principal(xml_writer, username)?;
                }
                ("urn:ietf:params:xml:ns:carddav", "addressbook-home-set") => {
                    Self::write_addressbook_home_set(xml_writer, username)?;
                }
                _ => {
                    xml_writer
                        .write_event(Event::Empty(BytesStart::new(carddav_prop_name(prop))))?;
                }
            }
        }
        Ok(())
    }

    /// REPORT `<D:multistatus>` opening tag (DAV + CardDAV namespaces).
    /// Streaming emitters call this once, then
    /// [`Self::write_contacts_report_page`] per cursor page, then
    /// [`Self::write_carddav_multistatus_end`].
    pub fn write_report_multistatus_start<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
            ]),
        ))?;
        Ok(())
    }

    /// Close a multistatus opened by either start writer.
    pub fn write_carddav_multistatus_end<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// One REPORT page of contact responses. Props are borrowed from
    /// the request; one href buffer is reused across the page.
    pub fn write_contacts_report_page<W: Write>(
        xml_writer: &mut Writer<W>,
        contacts: &[ContactDto],
        report: &CardDavReportType,
        base_href: &str,
    ) -> Result<()> {
        let props = match report {
            CardDavReportType::AddressbookQuery { props } => props,
            CardDavReportType::AddressbookMultiget { props, .. } => props,
            CardDavReportType::SyncCollection { props, .. } => props,
        };
        let mut href = String::with_capacity(base_href.len() + 48);
        for contact in contacts {
            href.clear();
            let _ = std::fmt::Write::write_fmt(
                &mut href,
                format_args!("{}{}.vcf", base_href, contact.uid),
            );
            // `write_contact_response` generates the vCard on demand when (and
            // only when) address-data is actually requested.
            Self::write_contact_response(xml_writer, contact, props, &href)?;
        }
        Ok(())
    }

    /// Generate response for contacts (for REPORT).
    ///
    /// `deleted_hrefs` renders each as an RFC 6578 §3.7
    /// `<D:status>HTTP/1.1 404 Not Found</D:status>` sub-response instead
    /// of a `<D:propstat>` block, and `sync_token` (when `Some`) renders
    /// as a trailing `<D:sync-token>` — both empty/`None` for the
    /// addressbook-query / addressbook-multiget report types, which
    /// don't use either.
    pub fn generate_contacts_response<W: Write>(
        writer: W,
        contacts: &[ContactDto],
        report: &CardDavReportType,
        base_href: &str,
        deleted_hrefs: &[String],
        sync_token: Option<&str>,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);
        Self::write_report_multistatus_start(&mut xml_writer)?;
        Self::write_contacts_report_page(&mut xml_writer, contacts, report, base_href)?;

        for href in deleted_hrefs {
            Self::write_deleted_contact_response(&mut xml_writer, href)?;
        }

        if let Some(token) = sync_token {
            xml_writer.write_event(Event::Start(BytesStart::new("D:sync-token")))?;
            xml_writer.write_event(Event::Text(BytesText::new(token)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:sync-token")))?;
        }

        Self::write_carddav_multistatus_end(&mut xml_writer)
    }

    /// RFC 6578 §3.7 removed-member sub-response: a `<D:response>` whose
    /// `<D:status>` is 404, with no `<D:propstat>` block.
    fn write_deleted_contact_response<W: Write>(
        xml_writer: &mut Writer<W>,
        href: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 404 Not Found")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    /// Write a single contact response element
    fn write_contact_response<W: Write>(
        xml_writer: &mut Writer<W>,
        contact: &ContactDto,
        props: &[QualifiedName],
        href: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        if props.is_empty() {
            // Return standard properties
            xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

            xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
            write_quoted_etag(xml_writer, &contact.etag)?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

            xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
            xml_writer.write_event(Event::Text(BytesText::new("text/vcard; charset=utf-8")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

            // Include vCard data
            let vcard = contact_to_vcard(contact);
            xml_writer.write_event(Event::Start(BytesStart::new("CR:address-data")))?;
            xml_writer.write_event(Event::Text(BytesText::new(&vcard)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("CR:address-data")))?;
        } else {
            for prop in props {
                match (prop.namespace.as_str(), prop.name.as_str()) {
                    ("DAV:", "resourcetype") => {
                        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
                    }
                    ("DAV:", "getetag") => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                        write_quoted_etag(xml_writer, &contact.etag)?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                    }
                    ("DAV:", "getcontenttype") => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(
                            "text/vcard; charset=utf-8",
                        )))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                    }
                    ("DAV:", "getlastmodified") => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                        // Stack render (ROUND10 §13, byte-identical to
                        // chrono) with the chrono fallback for
                        // out-of-range timestamps — per-contact on the
                        // multiget/PROPFIND path.
                        let mut lm_buf = [0u8; 31];
                        match crate::common::fmt::rfc2822_utc(
                            &mut lm_buf,
                            contact.updated_at.timestamp(),
                        ) {
                            Some(s) => {
                                xml_writer.write_event(Event::Text(BytesText::new(s)))?;
                            }
                            None => {
                                xml_writer.write_event(Event::Text(BytesText::new(
                                    &contact.updated_at.to_rfc2822(),
                                )))?;
                            }
                        }
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                    }
                    ("urn:ietf:params:xml:ns:carddav", "address-data") => {
                        let vcard = contact_to_vcard(contact);
                        xml_writer.write_event(Event::Start(BytesStart::new("CR:address-data")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&vcard)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("CR:address-data")))?;
                    }
                    _ => {
                        let prop_name = if prop.namespace == "urn:ietf:params:xml:ns:carddav" {
                            format!("CR:{}", prop.name)
                        } else if prop.namespace == "DAV:" {
                            format!("D:{}", prop.name)
                        } else {
                            prop.name.clone()
                        };
                        xml_writer.write_event(Event::Empty(BytesStart::new(&prop_name)))?;
                    }
                }
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        Ok(())
    }

    /// Parse a MKCOL XML request for making an address book
    pub fn parse_mkaddressbook<R: Read>(
        reader: R,
    ) -> Result<(String, Option<String>, Option<String>)> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_set = false;
        let mut in_prop = false;
        let mut in_displayname = false;
        let mut in_description = false;
        let mut in_color = false;

        let mut displayname = String::new();
        let mut description = None;
        let mut color = None;

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "set" || s.ends_with(":set") => in_set = true,
                        s if in_set && (s == "prop" || s.ends_with(":prop")) => in_prop = true,
                        s if in_prop && (s == "displayname" || s.ends_with(":displayname")) => {
                            in_displayname = true
                        }
                        s if in_prop
                            && (s == "addressbook-description"
                                || s.ends_with(":addressbook-description")) =>
                        {
                            in_description = true
                        }
                        s if in_prop && (s.contains("color")) => in_color = true,
                        _ => {}
                    }
                }
                Ok(Event::Text(e)) => {
                    let text = e.decode().unwrap_or_default();
                    if in_displayname {
                        displayname = text.to_string();
                    } else if in_description {
                        description = Some(text.to_string());
                    } else if in_color {
                        color = Some(text.to_string());
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    match name_str {
                        s if s == "set" || s.ends_with(":set") => in_set = false,
                        s if s == "prop" || s.ends_with(":prop") => in_prop = false,
                        s if s == "displayname" || s.ends_with(":displayname") => {
                            in_displayname = false
                        }
                        s if s.contains("description") => in_description = false,
                        s if s.contains("color") => in_color = false,
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }
            buffer.clear();
        }

        if displayname.is_empty() {
            displayname = format!("Address Book {}", uuid::Uuid::new_v4());
        }

        Ok((displayname, description, color))
    }
}

/// Convert a ContactDto to vCard 3.0 format
pub fn contact_to_vcard(contact: &ContactDto) -> String {
    // `write!` into a String is infallible; `let _ =` discards the Ok(()).
    // Formatting straight into the buffer avoids one temporary String per
    // vCard line compared to `push_str(&format!(…))`.
    use std::fmt::Write as _;

    let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");

    let _ = write!(vcard, "UID:{}\r\n", contact.uid);

    if let (Some(last), Some(first)) = (&contact.last_name, &contact.first_name) {
        let _ = write!(vcard, "N:{};{};;;\r\n", last, first);
    } else if let Some(last) = &contact.last_name {
        let _ = write!(vcard, "N:{};;;;\r\n", last);
    } else if let Some(first) = &contact.first_name {
        let _ = write!(vcard, "N:;{};;;\r\n", first);
    }

    if let Some(fn_name) = &contact.full_name {
        let _ = write!(vcard, "FN:{}\r\n", fn_name);
    } else {
        // FN is mandatory in vCard 3.0. Write the borrowed trim slice directly
        // instead of copying it into a second owned String (benches/ROUND19.md §V1).
        let fn_name = format!(
            "{} {}",
            contact.first_name.as_deref().unwrap_or(""),
            contact.last_name.as_deref().unwrap_or(""),
        );
        let trimmed = fn_name.trim();
        if !trimmed.is_empty() {
            let _ = write!(vcard, "FN:{}\r\n", trimmed);
        } else {
            vcard.push_str("FN:Unknown\r\n");
        }
    }

    if let Some(nickname) = &contact.nickname {
        let _ = write!(vcard, "NICKNAME:{}\r\n", nickname);
    }

    for email in &contact.email {
        vcard.push_str("EMAIL;TYPE=");
        crate::common::fmt::push_upper(&mut vcard, &email.r#type);
        vcard.push(':');
        vcard.push_str(&email.email);
        vcard.push_str("\r\n");
    }

    for phone in &contact.phone {
        vcard.push_str("TEL;TYPE=");
        crate::common::fmt::push_upper(&mut vcard, &phone.r#type);
        vcard.push(':');
        vcard.push_str(&phone.number);
        vcard.push_str("\r\n");
    }

    for addr in &contact.address {
        vcard.push_str("ADR;TYPE=");
        crate::common::fmt::push_upper(&mut vcard, &addr.r#type);
        let _ = write!(
            vcard,
            ":;;{};{};{};{};{}\r\n",
            addr.street.as_deref().unwrap_or(""),
            addr.city.as_deref().unwrap_or(""),
            addr.state.as_deref().unwrap_or(""),
            addr.postal_code.as_deref().unwrap_or(""),
            addr.country.as_deref().unwrap_or(""),
        );
    }

    if let Some(org) = &contact.organization {
        let _ = write!(vcard, "ORG:{}\r\n", org);
    }
    if let Some(title) = &contact.title {
        let _ = write!(vcard, "TITLE:{}\r\n", title);
    }
    if let Some(notes) = &contact.notes {
        // Only a multi-line note needs the escaping copy; a note with no newline
        // writes its borrowed slice directly (benches/ROUND19.md §V1).
        if notes.contains('\n') {
            let _ = write!(vcard, "NOTE:{}\r\n", notes.replace('\n', "\\n"));
        } else {
            vcard.push_str("NOTE:");
            vcard.push_str(notes);
            vcard.push_str("\r\n");
        }
    }
    if let Some(bday) = &contact.birthday {
        // Stack render (byte-identical to chrono's `%Y-%m-%d`) with the chrono
        // fallback for out-of-range years — drops the strftime interpreter + a
        // heap alloc per contact-with-birthday (fmt::compact_date is the
        // date-only companion to the §V2 REV renderer above).
        use chrono::Datelike as _;
        let mut bday_buf = [0u8; 10];
        match crate::common::fmt::compact_date(&mut bday_buf, bday.year(), bday.month(), bday.day())
        {
            Some(s) => {
                vcard.push_str("BDAY:");
                vcard.push_str(s);
                vcard.push_str("\r\n");
            }
            None => {
                let _ = write!(vcard, "BDAY:{}\r\n", bday.format("%Y-%m-%d"));
            }
        }
    }
    if let Some(photo) = &contact.photo_url {
        let _ = write!(vcard, "PHOTO;VALUE=URI:{}\r\n", photo);
    }

    // REV via the stack renderer — chrono's `.format("%Y%m%dT%H%M%SZ")` runs the
    // strftime interpreter and allocates per contact (benches/ROUND19.md §V2:
    // 11.8× faster, 3→0 allocs). Out-of-range falls back to chrono.
    let mut rev_buf = [0u8; 16];
    match crate::common::fmt::compact_ical_utc(&mut rev_buf, contact.updated_at.timestamp()) {
        Some(rev) => {
            vcard.push_str("REV:");
            vcard.push_str(rev);
            vcard.push_str("\r\n");
        }
        None => {
            let _ = write!(
                vcard,
                "REV:{}\r\n",
                contact.updated_at.format("%Y%m%dT%H%M%SZ")
            );
        }
    }
    vcard.push_str("END:VCARD\r\n");

    vcard
}
