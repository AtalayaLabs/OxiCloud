//! PROPFIND per-row XML emit benchmark — Vec churn + format-interpreter
//! dates (ROUND4).
//!
//! For EVERY file/folder row of every PROPFIND page the old writers paid:
//!   • a `partition` into two throwaway `Vec<&QualifiedName>`s (+ a third
//!     for the 404 list) — even though the requested-props writer already
//!     skips unknown names itself;
//!   • `to_rfc3339()` + `to_rfc2822()` — chrono's format-spec interpreter
//!     plus a heap String each;
//!   • `size.to_string()` and a `format!("\"{etag}\"")`.
//!
//! AFTER: single-pass 404 computation (usually-empty Vec), stack-rendered
//! dates/sizes (`common::fmt`, byte-identical, chrono fallback for
//! out-of-range), exactly-sized etag quoting.
//!
//! The OLD writers are copied verbatim into `mod before`; the gate
//! asserts byte-identical multistatus XML for named-prop (typical sync
//! client set + unknown props), AllProp (with quota), and dead-prop
//! carrying rows. Exit 1 on any diff.
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_propfind_xml
//! Tunables (env): BENCH_ROWS (1000), BENCH_PASSES (200)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::application::adapters::webdav_adapter::{
    PropFindRequest, PropFindType, QualifiedName, bench as dav_bench,
};
use oxicloud::application::dtos::file_dto::FileDto;
use oxicloud::application::dtos::folder_dto::FolderDto;
use oxicloud::domain::entities::file::File;
use oxicloud::domain::entities::folder::Folder;
use uuid::Uuid;

// ─── Counting allocator ─────────────────────────────────────────────────────

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

// ─── BEFORE: verbatim copy of the old per-row writers ───────────────────────

#[allow(clippy::all)]
mod before {
    use oxicloud::application::adapters::webdav_adapter::{
        PropFindRequest, PropFindType, QualifiedName,
    };
    use oxicloud::application::dtos::file_dto::FileDto;
    use oxicloud::application::dtos::folder_dto::FolderDto;
    use quick_xml::Writer;
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
    use std::io::Write;

    type Result<T> = std::result::Result<T, quick_xml::Error>;

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
            write_qname_empty(xml_writer, prop)?;
        }
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 404 Not Found")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        Ok(())
    }

    fn write_dead_props_propstat<W: Write>(
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

    fn write_quota_props<W: Write>(
        xml_writer: &mut Writer<W>,
        used_bytes: i64,
        available_bytes: Option<i64>,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:quota-used-bytes")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&used_bytes.to_string())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:quota-used-bytes")))?;

        if let Some(available_bytes) = available_bytes {
            xml_writer.write_event(Event::Start(BytesStart::new("D:quota-available-bytes")))?;
            xml_writer.write_event(Event::Text(BytesText::new(&available_bytes.to_string())))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:quota-available-bytes")))?;
        }
        Ok(())
    }

    fn write_folder_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        quota: Option<(i64, Option<i64>)>,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&folder.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:creationdate")))?;
        let created_at = folder.created_at;
        xml_writer.write_event(Event::Text(BytesText::new(&created_at.to_rfc3339())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:creationdate")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        let modified_at = folder.modified_at;
        xml_writer.write_event(Event::Text(BytesText::new(&modified_at.to_rfc2822())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!("\"{}\"", folder.etag))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
        xml_writer.write_event(Event::Text(BytesText::new("0")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new("httpd/unix-directory")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        if let Some((used, available)) = quota {
            write_quota_props(xml_writer, used, available)?;
        }
        Ok(())
    }

    fn write_file_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
    ) -> Result<()> {
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&file.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&file.mime_type)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&file.size.to_string())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:creationdate")))?;
        let created_at = file.created_at;
        xml_writer.write_event(Event::Text(BytesText::new(&created_at.to_rfc3339())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:creationdate")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        let modified_at = file.modified_at;
        xml_writer.write_event(Event::Text(BytesText::new(&modified_at.to_rfc2822())))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!("\"{}\"", file.etag))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
        Ok(())
    }

    fn write_folder_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        folder: &FolderDto,
        props: &[&QualifiedName],
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
                        xml_writer.write_event(Event::Start(BytesStart::new("D:creationdate")))?;
                        let created_at = folder.created_at;
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&created_at.to_rfc3339())))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:creationdate")))?;
                    }
                    "getlastmodified" => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                        let modified_at = folder.modified_at;
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&modified_at.to_rfc2822())))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                    }
                    "getetag" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&format!(
                            "\"{}\"",
                            folder.etag
                        ))))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
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
                            xml_writer
                                .write_event(Event::Start(BytesStart::new("D:quota-used-bytes")))?;
                            xml_writer
                                .write_event(Event::Text(BytesText::new(&used.to_string())))?;
                            xml_writer
                                .write_event(Event::End(BytesEnd::new("D:quota-used-bytes")))?;
                        }
                    }
                    "quota-available-bytes" => {
                        if let Some((_, Some(available))) = quota {
                            xml_writer.write_event(Event::Start(BytesStart::new(
                                "D:quota-available-bytes",
                            )))?;
                            xml_writer
                                .write_event(Event::Text(BytesText::new(&available.to_string())))?;
                            xml_writer.write_event(Event::End(BytesEnd::new(
                                "D:quota-available-bytes",
                            )))?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn write_file_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        file: &FileDto,
        props: &[&QualifiedName],
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
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontentlength")))?;
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&file.size.to_string())))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontentlength")))?;
                    }
                    "creationdate" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:creationdate")))?;
                        let created_at = file.created_at;
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&created_at.to_rfc3339())))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:creationdate")))?;
                    }
                    "getlastmodified" => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                        let modified_at = file.modified_at;
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&modified_at.to_rfc2822())))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                    }
                    "getetag" => {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(&format!(
                            "\"{}\"",
                            file.etag
                        ))))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub fn write_file_response_with_dead_props<W: Write>(
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
                let (known, unknown): (Vec<_>, Vec<_>) =
                    props.iter().partition(|p| file_prop_is_known(p));
                let truly_unknown: Vec<_> = unknown
                    .into_iter()
                    .filter(|p| !dead_name_set.contains(*p))
                    .collect();

                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                write_file_requested_props(xml_writer, file, &known)?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

                write_unknown_props_404(xml_writer, &truly_unknown)?;
            }
            other => {
                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                match other {
                    PropFindType::AllProp => {
                        write_file_standard_props(xml_writer, file)?;
                    }
                    PropFindType::PropName => {
                        // not exercised in this bench
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

        write_dead_props_propstat(xml_writer, &relevant_dead)?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    pub fn write_folder_response_with_dead_props<W: Write>(
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
                let (known, unknown): (Vec<_>, Vec<_>) =
                    props.iter().partition(|p| folder_prop_is_known(p, quota));
                let truly_unknown: Vec<_> = unknown
                    .into_iter()
                    .filter(|p| !dead_name_set.contains(*p))
                    .collect();

                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                write_folder_requested_props(xml_writer, folder, &known, quota)?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

                write_unknown_props_404(xml_writer, &truly_unknown)?;
            }
            other => {
                xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;
                match other {
                    PropFindType::AllProp => {
                        write_folder_standard_props(xml_writer, folder, quota)?;
                    }
                    PropFindType::PropName => {}
                    PropFindType::Prop(_) => unreachable!(),
                }
                xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
                xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
            }
        }

        write_dead_props_propstat(xml_writer, &relevant_dead)?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }
}

// ─── Corpus ─────────────────────────────────────────────────────────────────

fn build_files(rows: usize) -> Vec<FileDto> {
    (0..rows)
        .map(|i| {
            // Timestamp mix: epoch edge, padded-day dates, recent, far future.
            let created = [0u64, 1_120_176_000, 1_700_000_000, 4_102_444_799][i % 4];
            let f = File::from_materialized_row(
                Uuid::from_u128(i as u128).to_string(),
                format!("informe-{i}.pdf"),
                Some("/Personal/Projects/2026"),
                (i as u64) * 3_517 + 42,
                "application/pdf".to_string(),
                Some(Uuid::nil().to_string()),
                created,
                created + 86_400 * (i as u64 % 300),
                format!("{:032x}", i * 2_654_435_761),
                None,
                None,
            )
            .expect("valid file");
            FileDto::from(f)
        })
        .collect()
}

fn build_folders(rows: usize) -> Vec<FolderDto> {
    (0..rows)
        .map(|i| {
            let created = [0u64, 1_120_176_000, 1_700_000_000, 4_102_444_799][i % 4];
            let f = Folder::from_materialized_row(
                Uuid::from_u128((1_000_000 + i) as u128).to_string(),
                format!("Carpeta {i}"),
                format!("/Personal/Carpeta {i}"),
                None,
                Uuid::nil(),
                created,
                created + 3_600,
                created + 7_200,
                None,
                None,
            )
            .expect("valid folder");
            FolderDto::from(f)
        })
        .collect()
}

/// The prop set DAVx⁵/rclone-style clients poll with, plus two unknown
/// names so the 404 path is exercised.
fn sync_request() -> PropFindRequest {
    PropFindRequest {
        prop_find_type: PropFindType::Prop(vec![
            QualifiedName::new("DAV:", "resourcetype"),
            QualifiedName::new("DAV:", "displayname"),
            QualifiedName::new("DAV:", "getcontenttype"),
            QualifiedName::new("DAV:", "getcontentlength"),
            QualifiedName::new("DAV:", "getlastmodified"),
            QualifiedName::new("DAV:", "getetag"),
            QualifiedName::new("DAV:", "lockdiscovery"),
            QualifiedName::new("http://owncloud.org/ns", "fileid"),
        ]),
    }
}

fn allprop_request() -> PropFindRequest {
    PropFindRequest {
        prop_find_type: PropFindType::AllProp,
    }
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

const QUOTA: Option<(i64, Option<i64>)> = Some((123_456_789, Some(9_876_543_210)));

fn render_before(
    files: &[FileDto],
    folders: &[FolderDto],
    request: &PropFindRequest,
    dead: &[(QualifiedName, Option<String>)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 << 20);
    let mut w = quick_xml::Writer::new(&mut out);
    for (i, folder) in folders.iter().enumerate() {
        let dead = if i % 7 == 0 { dead } else { &[] };
        before::write_folder_response_with_dead_props(
            &mut w,
            folder,
            request,
            "/webdav/Personal/",
            dead,
            QUOTA,
        )
        .expect("before folder row");
    }
    for (i, file) in files.iter().enumerate() {
        let dead = if i % 7 == 0 { dead } else { &[] };
        before::write_file_response_with_dead_props(
            &mut w,
            file,
            request,
            "/webdav/Personal/informe.pdf",
            dead,
        )
        .expect("before file row");
    }
    out
}

fn render_after(
    files: &[FileDto],
    folders: &[FolderDto],
    request: &PropFindRequest,
    dead: &[(QualifiedName, Option<String>)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 << 20);
    let mut w = quick_xml::Writer::new(&mut out);
    for (i, folder) in folders.iter().enumerate() {
        let dead = if i % 7 == 0 { dead } else { &[] };
        dav_bench::write_folder_propfind_row(
            &mut w,
            folder,
            request,
            "/webdav/Personal/",
            dead,
            QUOTA,
        )
        .expect("after folder row");
    }
    for (i, file) in files.iter().enumerate() {
        let dead = if i % 7 == 0 { dead } else { &[] };
        dav_bench::write_file_propfind_row(
            &mut w,
            file,
            request,
            "/webdav/Personal/informe.pdf",
            dead,
        )
        .expect("after file row");
    }
    out
}

fn main() {
    let rows: usize = env::var("BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let passes: usize = env::var("BENCH_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    let files = build_files(rows);
    let folders = build_folders(rows / 10);
    let total_rows = files.len() + folders.len();
    let dead: Vec<(QualifiedName, Option<String>)> = vec![(
        QualifiedName::new("http://example.com/ns", "color"),
        Some("azul".to_string()),
    )];

    let sync_req = sync_request();
    let all_req = allprop_request();

    println!(
        "bench_propfind_xml — {} files + {} folders/page, {passes} passes\n",
        files.len(),
        folders.len()
    );

    for (label, req) in [("named-prop (sync set)", &sync_req), ("allprop", &all_req)] {
        let mut lat_before = Vec::with_capacity(passes);
        let mut lat_after = Vec::with_capacity(passes);
        for _ in 0..passes {
            let t0 = Instant::now();
            black_box(render_before(&files, &folders, req, &dead));
            lat_before.push(t0.elapsed().as_secs_f64() * 1e6);
            let t0 = Instant::now();
            black_box(render_after(&files, &folders, req, &dead));
            lat_after.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        let b = p50(lat_before);
        let a = p50(lat_after);

        let s0 = ALLOC_CALLS.load(Ordering::Relaxed);
        black_box(render_before(&files, &folders, req, &dead));
        let ab = (ALLOC_CALLS.load(Ordering::Relaxed) - s0) as f64 / total_rows as f64;
        let s0 = ALLOC_CALLS.load(Ordering::Relaxed);
        black_box(render_after(&files, &folders, req, &dead));
        let aa = (ALLOC_CALLS.load(Ordering::Relaxed) - s0) as f64 / total_rows as f64;

        println!("[{label}] µs/page (p50) + allocs/row");
        println!("    BEFORE  {b:9.1} µs   {ab:6.2} allocs/row");
        println!(
            "    AFTER   {a:9.1} µs   {aa:6.2} allocs/row   {:.2}x",
            b / a
        );
    }

    // ── Equivalence gate: byte-identical multistatus XML ────────────────────
    let mut ok = true;
    for req in [&sync_req, &all_req] {
        let xb = render_before(&files, &folders, req, &dead);
        let xa = render_after(&files, &folders, req, &dead);
        if xb != xa {
            ok = false;
            let diff_at = xb.iter().zip(&xa).position(|(a, b)| a != b).unwrap_or(0);
            let lo = diff_at.saturating_sub(120);
            eprintln!(
                "GATE FAIL ({:?}): first diff at byte {diff_at}\n BEFORE: …{}…\n AFTER:  …{}…",
                match req.prop_find_type {
                    PropFindType::Prop(_) => "prop",
                    PropFindType::AllProp => "allprop",
                    PropFindType::PropName => "propname",
                },
                String::from_utf8_lossy(&xb[lo..(diff_at + 120).min(xb.len())]),
                String::from_utf8_lossy(&xa[lo..(diff_at + 120).min(xa.len())]),
            );
        }
    }
    println!(
        "\n[gate] multistatus XML: {}",
        if ok { "OK (byte-identical)" } else { "FAILED" }
    );
    if !ok {
        std::process::exit(1);
    }
}
