//! CardDAV REPORT generation benchmark — dead double vCard generation +
//! O(N²) uid scan (BEFORE) vs single on-demand generation (AFTER).
//!
//! The old `handle_report` flow pre-generated a vCard for EVERY contact into a
//! `Vec<(uid, vcard)>`, then `generate_contacts_response` did a linear
//! `find(|(uid, _)| *uid == contact.uid)` per contact — O(N²) string compares
//! — and *discarded* the result (`let _ = vcard`), because
//! `write_contact_response` regenerates the vCard on demand anyway. The fix
//! deletes the pre-generation and the scan, and converts `contact_to_vcard`
//! from `push_str(&format!(…))` (one temp String per line) to
//! `write!(&mut String, …)`.
//!
//! `mod before` below is a verbatim copy of the OLD code (old
//! `contact_to_vcard`, old `generate_contacts_response` with the `vcards`
//! parameter, and the then-current `write_contact_response`), so one binary
//! measures both variants and byte-compares their output.
//!
//! Equivalence gate: BEFORE and AFTER XML must be byte-identical for every
//! (N, prop-set) combination, and the old/new `contact_to_vcard` must agree
//! byte-for-byte on every synthetic contact. Any mismatch exits 1 with the
//! first differing offset.
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_carddav_report
//! Tunables (env):
//!   BENCH_REPS (5)   median reported

use std::env;
use std::time::Instant;

use chrono::{NaiveDate, TimeZone, Utc};
use oxicloud::application::adapters::carddav_adapter::{
    CardDavAdapter, CardDavReportType, contact_to_vcard,
};
use oxicloud::application::adapters::webdav_adapter::QualifiedName;
use oxicloud::application::dtos::contact_dto::{AddressDto, ContactDto, EmailDto, PhoneDto};

/// Verbatim copy of the pre-fix production code (handler + adapter side),
/// kept here so the benchmark measures the real OLD flow, not a caricature.
mod before {
    use std::io::Write;

    use oxicloud::application::adapters::carddav_adapter::CardDavReportType;
    use oxicloud::application::adapters::webdav_adapter::QualifiedName;
    use oxicloud::application::dtos::contact_dto::ContactDto;
    use quick_xml::Writer;
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

    /// OLD `generate_contacts_response` — takes the pre-generated `vcards`,
    /// does the O(N²) linear uid scan per contact, then throws the hit away.
    pub fn generate_contacts_response<W: Write>(
        writer: W,
        contacts: &[ContactDto],
        vcards: &[(String, String)], // (uid, vcard_data)
        report: &CardDavReportType,
        base_href: &str,
    ) -> std::io::Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
            ]),
        ))?;

        let props = match report {
            CardDavReportType::AddressbookQuery { props } => props.clone(),
            CardDavReportType::AddressbookMultiget { props, .. } => props.clone(),
            CardDavReportType::SyncCollection { props, .. } => props.clone(),
        };

        for contact in contacts {
            let href = format!("{}{}.vcf", base_href, contact.uid);
            let vcard = vcards
                .iter()
                .find(|(uid, _)| *uid == contact.uid)
                .map(|(_, data)| data.as_str())
                .unwrap_or("");
            write_contact_response(&mut xml_writer, contact, &props, &href)?;
            // If address-data is requested, include vcard
            if props.iter().any(|p| p.name == "address-data") || props.is_empty() {
                // Already handled in write_contact_response
            }
            let _ = vcard; // suppress warning - used via contact_to_vcard fallback
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// Copy of the (unchanged) private `write_contact_response`, wired to the
    /// OLD `contact_to_vcard` so the BEFORE variant is fully self-contained.
    fn write_contact_response<W: Write>(
        xml_writer: &mut Writer<W>,
        contact: &ContactDto,
        props: &[QualifiedName],
        href: &str,
    ) -> std::io::Result<()> {
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
            xml_writer.write_event(Event::Text(BytesText::new(&format!(
                "\"{}\"",
                contact.etag
            ))))?;
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
                        xml_writer.write_event(Event::Text(BytesText::new(&format!(
                            "\"{}\"",
                            contact.etag
                        ))))?;
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
                        xml_writer.write_event(Event::Text(BytesText::new(
                            &contact.updated_at.to_rfc2822(),
                        )))?;
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

    /// OLD `contact_to_vcard` — one `push_str(&format!(…))` temp String per line.
    pub fn contact_to_vcard(contact: &ContactDto) -> String {
        let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");

        vcard.push_str(&format!("UID:{}\r\n", contact.uid));

        if let (Some(last), Some(first)) = (&contact.last_name, &contact.first_name) {
            vcard.push_str(&format!("N:{};{};;;\r\n", last, first));
        } else if let Some(last) = &contact.last_name {
            vcard.push_str(&format!("N:{};;;;\r\n", last));
        } else if let Some(first) = &contact.first_name {
            vcard.push_str(&format!("N:;{};;;\r\n", first));
        }

        if let Some(fn_name) = &contact.full_name {
            vcard.push_str(&format!("FN:{}\r\n", fn_name));
        } else {
            // FN is mandatory in vCard 3.0
            let fn_name = format!(
                "{} {}",
                contact.first_name.as_deref().unwrap_or(""),
                contact.last_name.as_deref().unwrap_or(""),
            )
            .trim()
            .to_string();
            if !fn_name.is_empty() {
                vcard.push_str(&format!("FN:{}\r\n", fn_name));
            } else {
                vcard.push_str("FN:Unknown\r\n");
            }
        }

        if let Some(nickname) = &contact.nickname {
            vcard.push_str(&format!("NICKNAME:{}\r\n", nickname));
        }

        for email in &contact.email {
            vcard.push_str(&format!(
                "EMAIL;TYPE={}:{}\r\n",
                email.r#type.to_uppercase(),
                email.email
            ));
        }

        for phone in &contact.phone {
            vcard.push_str(&format!(
                "TEL;TYPE={}:{}\r\n",
                phone.r#type.to_uppercase(),
                phone.number
            ));
        }

        for addr in &contact.address {
            let adr = format!(
                ";;{};{};{};{};{}",
                addr.street.as_deref().unwrap_or(""),
                addr.city.as_deref().unwrap_or(""),
                addr.state.as_deref().unwrap_or(""),
                addr.postal_code.as_deref().unwrap_or(""),
                addr.country.as_deref().unwrap_or(""),
            );
            vcard.push_str(&format!(
                "ADR;TYPE={}:{}\r\n",
                addr.r#type.to_uppercase(),
                adr
            ));
        }

        if let Some(org) = &contact.organization {
            vcard.push_str(&format!("ORG:{}\r\n", org));
        }
        if let Some(title) = &contact.title {
            vcard.push_str(&format!("TITLE:{}\r\n", title));
        }
        if let Some(notes) = &contact.notes {
            vcard.push_str(&format!("NOTE:{}\r\n", notes.replace('\n', "\\n")));
        }
        if let Some(bday) = &contact.birthday {
            vcard.push_str(&format!("BDAY:{}\r\n", bday.format("%Y-%m-%d")));
        }
        if let Some(photo) = &contact.photo_url {
            vcard.push_str(&format!("PHOTO;VALUE=URI:{}\r\n", photo));
        }

        vcard.push_str(&format!(
            "REV:{}\r\n",
            contact.updated_at.format("%Y%m%dT%H%M%SZ")
        ));
        vcard.push_str("END:VCARD\r\n");

        vcard
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Deterministic synthetic address book: every contact has 2 emails, 1 phone
/// and 1 address; optional fields (nickname, notes-with-newline, birthday,
/// photo, missing names → FN fallback) are cycled so the byte-equality gate
/// exercises every `contact_to_vcard` branch, not just the happy path.
fn make_contacts(n: usize) -> Vec<ContactDto> {
    let created = Utc.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap();
    let updated = Utc.with_ymd_and_hms(2026, 6, 30, 18, 45, 12).unwrap();

    (0..n)
        .map(|i| {
            let (full_name, first_name, last_name) = match i % 5 {
                0 => (
                    Some(format!("Contact {i:05} Example")),
                    Some(format!("Contact{i:05}")),
                    Some("Example".to_string()),
                ),
                1 => (
                    None,
                    Some(format!("Contact{i:05}")),
                    Some("Example".to_string()),
                ),
                2 => (None, None, Some("Example".to_string())),
                3 => (None, Some(format!("Contact{i:05}")), None),
                _ => (None, None, None), // FN:Unknown fallback
            };
            ContactDto {
                id: format!("id-{i:05}"),
                address_book_id: "bench-book".to_string(),
                uid: format!("bench-contact-{i:05}@oxicloud"),
                full_name,
                first_name,
                last_name,
                nickname: (i % 7 == 0).then(|| format!("nick{i}")),
                email: vec![
                    EmailDto {
                        email: format!("contact{i:05}@example.com"),
                        r#type: "work".to_string(),
                        is_primary: true,
                    },
                    EmailDto {
                        email: format!("contact{i:05}@home.example.org"),
                        r#type: "home".to_string(),
                        is_primary: false,
                    },
                ],
                phone: vec![PhoneDto {
                    number: format!("+1-555-{:04}", i % 10_000),
                    r#type: "cell".to_string(),
                    is_primary: true,
                }],
                address: vec![AddressDto {
                    street: Some(format!("{} Main Street", i + 1)),
                    city: Some("Springfield".to_string()),
                    state: Some("IL".to_string()),
                    postal_code: Some(format!("{:05}", 60_000 + (i % 1_000))),
                    country: Some("USA".to_string()),
                    r#type: "home".to_string(),
                    is_primary: true,
                }],
                organization: Some("OxiCloud Benchmarks Inc.".to_string()),
                title: Some("Engineer".to_string()),
                notes: (i % 11 == 0).then(|| "line one\nline two & <specials>".to_string()),
                photo_url: (i % 13 == 0).then(|| format!("https://example.com/avatars/{i}.jpg")),
                birthday: (i % 3 == 0).then(|| NaiveDate::from_ymd_opt(1990, 5, 17).unwrap()),
                anniversary: None,
                created_at: created,
                updated_at: updated,
                etag: format!("etag-{i:05}"),
            }
        })
        .collect()
}

fn dav(name: &str) -> QualifiedName {
    QualifiedName {
        namespace: "DAV:".to_string(),
        name: name.to_string(),
    }
}

fn carddav(name: &str) -> QualifiedName {
    QualifiedName {
        namespace: "urn:ietf:params:xml:ns:carddav".to_string(),
        name: name.to_string(),
    }
}

/// OLD handler flow: pre-generate a vCard per contact, then generate the XML
/// (which re-generates every vCard on demand and never reads the pre-made ones).
fn run_before(contacts: &[ContactDto], report: &CardDavReportType, base_href: &str) -> Vec<u8> {
    // Generate vCards (verbatim old handle_report pre-generation)
    let vcards: Vec<(String, String)> = contacts
        .iter()
        .map(|c| (c.uid.clone(), before::contact_to_vcard(c)))
        .collect();

    let mut out = Vec::new();
    before::generate_contacts_response(&mut out, contacts, &vcards, report, base_href)
        .expect("BEFORE XML generation failed");
    out
}

/// NEW production path.
fn run_after(contacts: &[ContactDto], report: &CardDavReportType, base_href: &str) -> Vec<u8> {
    let mut out = Vec::new();
    CardDavAdapter::generate_contacts_response(&mut out, contacts, report, base_href, &[], None)
        .expect("AFTER XML generation failed");
    out
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b {
        return None;
    }
    Some(
        a.iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| a.len().min(b.len())),
    )
}

fn context_snippet(bytes: &[u8], at: usize) -> String {
    let start = at.saturating_sub(40);
    let end = (at + 40).min(bytes.len());
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

fn main() {
    let reps: usize = env_or("BENCH_REPS", 5);
    let base_href = "/carddav/bench-book/";

    let prop_sets: Vec<(&str, Vec<QualifiedName>)> = vec![
        ("getetag", vec![dav("getetag")]),
        (
            "getetag + address-data",
            vec![dav("getetag"), carddav("address-data")],
        ),
        // Not part of the timing table, but gated too: the empty-props
        // default path also embeds address-data.
        ("(empty = allprop default)", vec![]),
    ];
    let sizes = [500usize, 5_000];

    // ── Equivalence gate ────────────────────────────────────────────────
    let gate_contacts = make_contacts(*sizes.iter().max().unwrap());
    for c in &gate_contacts {
        let old = before::contact_to_vcard(c);
        let new = contact_to_vcard(c);
        if old != new {
            let at = first_diff(old.as_bytes(), new.as_bytes()).unwrap();
            eprintln!(
                "EQUIVALENCE FAILURE: contact_to_vcard differs for uid={} at byte {}\n  old: …{}…\n  new: …{}…",
                c.uid,
                at,
                context_snippet(old.as_bytes(), at),
                context_snippet(new.as_bytes(), at),
            );
            std::process::exit(1);
        }
    }
    for &n in &sizes {
        let contacts = &gate_contacts[..n];
        for (label, props) in &prop_sets {
            let report = CardDavReportType::AddressbookQuery {
                props: props.clone(),
            };
            let old_xml = run_before(contacts, &report, base_href);
            let new_xml = run_after(contacts, &report, base_href);
            if let Some(at) = first_diff(&old_xml, &new_xml) {
                eprintln!(
                    "EQUIVALENCE FAILURE: REPORT XML differs (N={}, props={}) at byte {} (before {} B, after {} B)\n  before: …{}…\n  after:  …{}…",
                    n,
                    label,
                    at,
                    old_xml.len(),
                    new_xml.len(),
                    context_snippet(&old_xml, at),
                    context_snippet(&new_xml, at),
                );
                std::process::exit(1);
            }
        }
    }
    println!(
        "equivalence gate: BEFORE == AFTER byte-identical for all prop sets at N = {:?} (and all {} vCards match)\n",
        sizes,
        gate_contacts.len()
    );

    // ── Timing ──────────────────────────────────────────────────────────
    println!("| N     | props                  | BEFORE ms | AFTER ms | speedup |");
    println!("|------:|------------------------|----------:|---------:|--------:|");
    for &n in &sizes {
        let contacts = &gate_contacts[..n];
        for (label, props) in prop_sets.iter().take(2) {
            let report = CardDavReportType::AddressbookQuery {
                props: props.clone(),
            };

            // Warm-up (allocator, caches) — result discarded.
            let _ = run_before(contacts, &report, base_href);
            let _ = run_after(contacts, &report, base_href);

            let mut before_ms = Vec::with_capacity(reps);
            let mut after_ms = Vec::with_capacity(reps);
            for _ in 0..reps {
                let t0 = Instant::now();
                let out = run_before(contacts, &report, base_href);
                before_ms.push(t0.elapsed().as_secs_f64() * 1_000.0);
                std::hint::black_box(&out);

                let t1 = Instant::now();
                let out = run_after(contacts, &report, base_href);
                after_ms.push(t1.elapsed().as_secs_f64() * 1_000.0);
                std::hint::black_box(&out);
            }
            let b = median(before_ms);
            let a = median(after_ms);
            println!(
                "| {:>5} | {:<22} | {:>9.3} | {:>8.3} | {:>6.2}x |",
                n,
                label,
                b,
                a,
                b / a
            );
        }
    }
    println!(
        "\n(median of {} reps; BEFORE includes the old handler's vCard pre-generation loop,",
        reps
    );
    println!(" which the old code then discarded — the O(N²) uid scan dominates at large N)");
}
