//! Shared RFC 6578 §3.7 XML fragments used by every `sync-collection`
//! response writer — WebDAV, CalDAV, CardDAV adapters, plus the
//! NextCloud `report_handler.rs` surface. The only difference between
//! them is the DAV-namespace tag prefix (`"D:"` for the primary API
//! surfaces, `"d:"` for the lowercase-prefixed NC surface).

use quick_xml::{
    Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
use std::io::Write;

/// RFC 6578 §3.7 removed-member sub-response: a `<{tag_prefix}response>`
/// whose `<{tag_prefix}status>` is `HTTP/1.1 404 Not Found`, with no
/// `<{tag_prefix}propstat>` block — tells the client this href was a
/// member of the collection at the prior sync-token and no longer is.
pub fn write_deleted_response<W: Write>(
    xml_writer: &mut Writer<W>,
    tag_prefix: &str,
    href: &str,
) -> Result<(), quick_xml::Error> {
    xml_writer.write_event(Event::Start(BytesStart::new(format!(
        "{tag_prefix}response"
    ))))?;

    xml_writer.write_event(Event::Start(BytesStart::new(format!("{tag_prefix}href"))))?;
    xml_writer.write_event(Event::Text(BytesText::new(href)))?;
    xml_writer.write_event(Event::End(BytesEnd::new(format!("{tag_prefix}href"))))?;

    xml_writer.write_event(Event::Start(BytesStart::new(format!("{tag_prefix}status"))))?;
    xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 404 Not Found")))?;
    xml_writer.write_event(Event::End(BytesEnd::new(format!("{tag_prefix}status"))))?;

    xml_writer.write_event(Event::End(BytesEnd::new(format!("{tag_prefix}response"))))?;
    Ok(())
}

/// Trailing `<{tag_prefix}sync-token>{token}</{tag_prefix}sync-token>`
/// element closing out a `sync-collection` `<{tag_prefix}multistatus>` body.
pub fn write_sync_token<W: Write>(
    xml_writer: &mut Writer<W>,
    tag_prefix: &str,
    token: &str,
) -> Result<(), quick_xml::Error> {
    xml_writer.write_event(Event::Start(BytesStart::new(format!(
        "{tag_prefix}sync-token"
    ))))?;
    xml_writer.write_event(Event::Text(BytesText::new(token)))?;
    xml_writer.write_event(Event::End(BytesEnd::new(format!("{tag_prefix}sync-token"))))?;
    Ok(())
}
