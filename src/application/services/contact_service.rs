use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::adapters::param_encoding::render_param_value;
use crate::application::dtos::address_book_dto::{
    AddressBookDto, CreateAddressBookDto, UpdateAddressBookDto,
};
use crate::application::dtos::contact_dto::{
    ContactDto, ContactGroupDto, CreateContactDto, CreateContactGroupDto, CreateContactVCardDto,
    GroupMembershipDto, UpdateContactDto, UpdateContactGroupDto,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::carddav_ports::{
    AddressBookUseCase, ContactStoragePort, ContactUseCase,
};
use crate::common::errors::DomainError;
use crate::common::text::ascii_ci_contains;
use crate::domain::entities::contact::{Address, AddressBook, Contact, ContactGroup, Email, Phone};
use crate::domain::services::authorization::{Permission, Resource, Role, Subject};
use crate::infrastructure::adapters::contact_storage_adapter::ContactStorageAdapter;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Contact service — the CardDAV / REST entry point for every
/// address-book or contact operation. Every method routes through
/// `AuthorizationEngine`; the pre-Round-3 `check_address_book_access`
/// / `check_address_book_write_access` bespoke helpers are gone.
///
/// Ownership + sharing live entirely in `storage.role_grants`
/// (`resource_type='address_book'`). `carddav.address_books.owner_id`
/// stays for provenance and legacy queries but is no longer consulted
/// for access decisions.
pub struct ContactService {
    /// Storage port — bundles the three CardDAV PG repositories
    /// (address_book, contact, contact_group) behind
    /// `ContactStoragePort`. Symmetric with `CalendarService`'s
    /// hold on `CalendarStorageAdapter`.
    contact_storage: Arc<ContactStorageAdapter>,
    /// ReBAC engine — every user-facing method calls `authz.require`
    /// with the appropriate `Permission`. `create_address_book` also
    /// uses it to seed an Owner grant for the caller so the common
    /// "owning my own address book" case takes a single indexed
    /// role_grants lookup.
    authz: Arc<PgAclEngine>,
}

impl ContactService {
    pub fn new(contact_storage: Arc<ContactStorageAdapter>, authz: Arc<PgAclEngine>) -> Self {
        Self {
            contact_storage,
            authz,
        }
    }

    /// Enforce `permission` on `Resource::AddressBook(uuid)` and
    /// return the hydrated entity. Denial routes through
    /// `authz.require` → `NotFound` (anti-enum, same shape as "no
    /// such address book") + `authz.denied` audit line. Used by
    /// every method that needs both the entity AND the authz gate.
    async fn require_address_book_perm(
        &self,
        address_book_id: &Uuid,
        caller_id: &Uuid,
        permission: Permission,
    ) -> Result<AddressBook, DomainError> {
        self.authz
            .require(
                Subject::User(*caller_id),
                permission,
                Resource::AddressBook(*address_book_id),
            )
            .await?;
        self.contact_storage
            .get_address_book_by_id(address_book_id)
            .await?
            .ok_or_else(|| DomainError::not_found("Address book", "not found"))
    }

    /// Read gate with the public-address-book bypass: any
    /// authenticated OxiCloud user can Read a book marked
    /// `is_public = true`, matching the pre-Round-3 behaviour and
    /// the calendar `is_public` semantics. Write paths never use
    /// this bypass — they go through `require_address_book_perm`
    /// with `Update` / `Delete` / `Create` directly.
    async fn require_address_book_read_or_public(
        &self,
        address_book_id: &Uuid,
        caller_id: &Uuid,
    ) -> Result<AddressBook, DomainError> {
        let book = self
            .contact_storage
            .get_address_book_by_id(address_book_id)
            .await?
            .ok_or_else(|| DomainError::not_found("Address book", "not found"))?;
        if book.is_public() {
            return Ok(book);
        }
        self.authz
            .require(
                Subject::User(*caller_id),
                Permission::Read,
                Resource::AddressBook(*address_book_id),
            )
            .await?;
        Ok(book)
    }

    // Associated function (no `&self`) so tests in this module
    // can call `ContactService::parse_vcard(&body)` directly
    // without instantiating a full service (which needs an
    // Arc<ContactStorageAdapter> and an Arc<PgAclEngine>).
    fn parse_vcard(vcard_data: &str) -> Result<Contact, DomainError> {
        // This is a simplified vCard parser - a real implementation would use a proper vCard library
        // For now, we'll create a basic contact with minimal data

        let mut contact = Contact::default();

        // Iterate lines() directly — the previous `Vec<&str>` collect was only
        // ever iterated once. Per EMAIL/TEL/ADR line the `TYPE=` routing uses
        // the allocation-free `ascii_ci_contains` instead of a throwaway
        // `line.to_ascii_uppercase()` copy (benches/ROUND20.md §A3).
        for line in vcard_data.lines() {
            let line = line.trim();

            if let Some(stripped) = line.strip_prefix("FN:") {
                contact.set_full_name(Some(stripped.to_string()));
            } else if let Some(stripped) = line.strip_prefix("N:") {
                let parts: Vec<&str> = stripped.split(';').collect();
                if parts.len() >= 2 {
                    contact.set_last_name(Some(parts[0].to_string()));
                    contact.set_first_name(Some(parts[1].to_string()));
                }
            } else if line.starts_with("EMAIL") {
                // Split on the FIRST colon — same rationale as the
                // TEL branch below; keeps parameter parsing separate
                // from value parsing.
                let value = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                if !value.is_empty() {
                    let lb = line.as_bytes();
                    let email_type = if ascii_ci_contains(lb, b"TYPE=HOME") {
                        "home"
                    } else if ascii_ci_contains(lb, b"TYPE=WORK") {
                        "work"
                    } else {
                        "other"
                    };

                    contact.push_email(Email {
                        email: value.to_string(),
                        r#type: email_type.to_string(),
                        is_primary: contact.email_is_empty(), // First one is primary
                    });
                }
            } else if line.starts_with("TEL") {
                // Split on the FIRST colon so URI-form values survive.
                // Apple Contacts / DAVx⁵ send:
                //   TEL;TYPE=cell;VALUE=uri:tel:+15551234567
                // The pre-fix `split(':').nth(1)` picked up "tel"
                // (the middle segment), silently losing the actual
                // phone number. `split_once(':')` splits ONCE at the
                // property-name/value boundary; we then strip the
                // `tel:` URI scheme if present.
                let value = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                let value = value.strip_prefix("tel:").unwrap_or(value);
                if !value.is_empty() {
                    // RFC 6350 §5.3: parameter values are
                    // case-insensitive. Match on the uppercase
                    // form of the whole property line so
                    // `TYPE=cell` and `TYPE=CELL` both route
                    // correctly. Pre-fix this was case-sensitive
                    // and dropped lowercase to "other" — matches
                    // the shape python-caldav / Apple Contacts
                    // emit.
                    let lb = line.as_bytes();
                    let phone_type = if ascii_ci_contains(lb, b"TYPE=CELL")
                        || ascii_ci_contains(lb, b"TYPE=MOBILE")
                    {
                        "mobile"
                    } else if ascii_ci_contains(lb, b"TYPE=HOME") {
                        "home"
                    } else if ascii_ci_contains(lb, b"TYPE=WORK") {
                        "work"
                    } else if ascii_ci_contains(lb, b"TYPE=FAX") {
                        "fax"
                    } else {
                        "other"
                    };

                    contact.push_phone(Phone {
                        number: value.to_string(),
                        r#type: phone_type.to_string(),
                        is_primary: contact.phone_is_empty(), // First one is primary
                    });
                }
            } else if line.starts_with("ADR") {
                // ADR (RFC 6350 §6.3.1). Structured value: 7 components
                // separated by `;` — (pobox, extended, street, city,
                // region, postal, country). Positions 0/1 are legacy
                // and typically empty; we preserve positions 2–6 as
                // (street, city, state, postal_code, country) which
                // matches the emitter format at
                // `carddav_adapter.rs::contact_to_vcard`.
                //
                // Pre-fix parse_vcard had NO ADR handler at all —
                // every ADR line sent by a client was silently dropped
                // at PUT time, so no address ever survived a
                // round-trip. See bug_carddav_parser_gaps.md.
                let value = line.split_once(':').map(|(_, v)| v).unwrap_or("");
                let parts: Vec<&str> = value.split(';').collect();
                let field = |i: usize| {
                    parts
                        .get(i)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                };
                let lb = line.as_bytes();
                let addr_type = if ascii_ci_contains(lb, b"TYPE=HOME") {
                    "home"
                } else if ascii_ci_contains(lb, b"TYPE=WORK") {
                    "work"
                } else {
                    "other"
                };
                // Only push if AT LEAST one of the useful fields
                // is populated — an all-empty ADR line is a
                // no-op sent by some clients that "clear" the
                // address; storing an empty row would confuse
                // downstream UIs.
                let street = field(2);
                let city = field(3);
                let state = field(4);
                let postal_code = field(5);
                let country = field(6);
                if street.is_some()
                    || city.is_some()
                    || state.is_some()
                    || postal_code.is_some()
                    || country.is_some()
                {
                    let is_primary = contact.address_is_empty();
                    contact.push_address(Address {
                        street,
                        city,
                        state,
                        postal_code,
                        country,
                        r#type: addr_type.to_string(),
                        is_primary,
                    });
                }
            } else if let Some(stripped) = line.strip_prefix("ORG:") {
                contact.set_organization(Some(stripped.to_string()));
            } else if let Some(stripped) = line.strip_prefix("TITLE:") {
                contact.set_title(Some(stripped.to_string()));
            } else if let Some(stripped) = line.strip_prefix("NOTE:") {
                contact.set_notes(Some(stripped.to_string()));
            } else if let Some(stripped) = line.strip_prefix("UID:") {
                contact.set_uid(stripped.to_string());
            }
        }

        // Store the original vCard data
        contact.set_vcard(vcard_data.to_string());
        contact.set_etag(Uuid::new_v4().to_string());

        Ok(contact)
    }

    fn generate_vcard(&self, contact: &Contact) -> String {
        // `write!` formats straight into `vcard`; the old
        // `push_str(&format!(…))` allocated a throwaway String per emitted
        // line (benches/ROUND11.md §10: 766 → 357 ns, 21 → 5 allocs).
        use std::fmt::Write as _;
        let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");

        // UID
        let _ = write!(vcard, "UID:{}\r\n", contact.uid());

        // Name fields
        if let Some(full_name) = contact.full_name() {
            let _ = write!(vcard, "FN:{}\r\n", full_name);
        }

        let last_name = contact.last_name().unwrap_or_default();
        let first_name = contact.first_name().unwrap_or_default();
        let _ = write!(vcard, "N:{};{};;;\r\n", last_name, first_name);

        // Email addresses
        for email in contact.email() {
            let mut ty = String::new();
            crate::common::fmt::push_upper(&mut ty, &email.r#type);
            vcard.push_str("EMAIL;TYPE=");
            vcard.push_str(&render_param_value(&ty));
            vcard.push(':');
            vcard.push_str(&email.email);
            vcard.push_str("\r\n");
        }

        // Phone numbers
        for phone in contact.phone() {
            let tel_type = match phone.r#type.as_str() {
                "mobile" => "CELL",
                "home" => "HOME",
                "work" => "WORK",
                "fax" => "FAX",
                _ => "OTHER",
            };
            let _ = write!(vcard, "TEL;TYPE={}:{}\r\n", tel_type, phone.number);
        }

        // Addresses
        for addr in contact.address() {
            let street = addr.street.as_deref().unwrap_or_default();
            let city = addr.city.as_deref().unwrap_or_default();
            let state = addr.state.as_deref().unwrap_or_default();
            let postal_code = addr.postal_code.as_deref().unwrap_or_default();
            let country = addr.country.as_deref().unwrap_or_default();

            let mut ty = String::new();
            crate::common::fmt::push_upper(&mut ty, &addr.r#type);
            vcard.push_str("ADR;TYPE=");
            vcard.push_str(&render_param_value(&ty));
            let _ = write!(
                vcard,
                ":;;{};{};{};{};{}\r\n",
                street, city, state, postal_code, country
            );
        }

        // Organization
        if let Some(org) = contact.organization() {
            let _ = write!(vcard, "ORG:{}\r\n", org);
        }

        // Title
        if let Some(title) = contact.title() {
            let _ = write!(vcard, "TITLE:{}\r\n", title);
        }

        // Notes
        if let Some(notes) = contact.notes() {
            let _ = write!(vcard, "NOTE:{}\r\n", notes);
        }

        // Birthday
        if let Some(birthday) = contact.birthday() {
            let _ = write!(vcard, "BDAY:{}\r\n", birthday.format("%Y%m%d"));
        }

        // Revision (last update) — stack renderer, see benches/ROUND19.md §V2.
        let mut rev_buf = [0u8; 16];
        match crate::common::fmt::compact_ical_utc(&mut rev_buf, contact.updated_at().timestamp()) {
            Some(rev) => {
                vcard.push_str("REV:");
                vcard.push_str(rev);
                vcard.push_str("\r\n");
            }
            None => {
                let _ = write!(
                    vcard,
                    "REV:{}\r\n",
                    contact.updated_at().format("%Y%m%dT%H%M%SZ")
                );
            }
        }

        vcard.push_str("END:VCARD\r\n");

        vcard
    }
}

impl AddressBookUseCase for ContactService {
    async fn create_address_book(
        &self,
        dto: CreateAddressBookDto,
    ) -> Result<AddressBookDto, DomainError> {
        // Legacy DTO carries the caller as `owner_id`. Parse it once
        // so the Owner-grant seed below can use the typed UUID; failed
        // parse maps to InvalidInput.
        let owner_id = Uuid::parse_str(&dto.owner_id)
            .map_err(|_| DomainError::validation_error("Invalid owner ID format"))?;
        let address_book = AddressBook::new(
            dto.name,
            dto.owner_id,
            dto.description,
            dto.color,
            dto.is_public.unwrap_or(false),
        );

        let created_address_book = self
            .contact_storage
            .create_address_book(address_book)
            .await?;
        // Seed the Owner role_grant so the engine's cache warms on
        // the caller's first read. `set_role` is idempotent on the
        // unique key — a re-run is a no-op.
        self.authz
            .set_role(
                owner_id,
                Subject::User(owner_id),
                Role::Owner,
                Resource::AddressBook(*created_address_book.id()),
                None,
            )
            .await?;
        Ok(AddressBookDto::from(created_address_book))
    }

    async fn update_address_book(
        &self,
        address_book_id: &str,
        update: UpdateAddressBookDto,
    ) -> Result<AddressBookDto, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // AuthZ: caller must have Update on the address book.
        // `update.user_id` in the DTO is the caller's own id — this
        // is legacy from the pre-Round-3 CardDAV flow. Post-Round-3
        // the caller is authoritative from the JWT extractor at the
        // handler; keeping the DTO field for wire compat.
        let caller_id = Uuid::parse_str(&update.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        let address_book = self
            .require_address_book_perm(&id, &caller_id, Permission::Update)
            .await?;

        // Apply updates
        let updated_address_book = AddressBook::from_raw(
            id,
            update
                .name
                .unwrap_or_else(|| address_book.name().to_string()),
            address_book.owner_id().to_string(),
            update
                .description
                .or_else(|| address_book.description().map(|s| s.to_string())),
            update
                .color
                .or_else(|| address_book.color().map(|s| s.to_string())),
            update.is_public.unwrap_or(address_book.is_public()),
            *address_book.created_at(),
            Utc::now(),
        );

        let result = self
            .contact_storage
            .update_address_book(updated_address_book)
            .await?;
        Ok(AddressBookDto::from(result))
    }

    async fn delete_address_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<(), DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // AuthZ: caller must have Delete on the address book. Only
        // Owner grants include Delete in their bundle today, matching
        // the pre-Round-3 owner-only rule; if `Contributor` ever grows
        // a Delete bundle it inherits the ability here for free.
        self.require_address_book_perm(&id, &user_id, Permission::Delete)
            .await?;

        self.contact_storage.delete_address_book(&id).await?;
        // Wipe every grant on this book so a re-used UUID doesn't
        // inherit stale ACLs. Storage DELETE won't cascade to
        // `storage.role_grants` — the legacy `carddav.address_book_shares`
        // had an FK, `role_grants` doesn't (cross-schema).
        let _ = self
            .authz
            .revoke_all_for_resource(Resource::AddressBook(id))
            .await;
        Ok(())
    }

    async fn get_address_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<AddressBookDto, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        let address_book = self
            .require_address_book_read_or_public(&id, &user_id)
            .await?;
        Ok(AddressBookDto::from(address_book))
    }

    async fn list_user_address_books(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<AddressBookDto>, DomainError> {
        // Post-Round-3: every address book the caller has any grant on
        // (owned + shared) comes from a single role_grants lookup.
        // Public address books stay a separate query — they don't
        // require a per-user grant, so a listing that ONLY filters on
        // grants would miss them.
        //
        // Duplicate suppression: a book that's public AND directly
        // granted to the caller shows up once. The HashMap keyed on
        // `book.id` handles this cheaply.
        let grants = self
            .authz
            .list_incoming_grants(Subject::User(user_id))
            .await?;
        let book_ids: std::collections::HashSet<Uuid> = grants
            .into_iter()
            .filter_map(|g| match g.resource {
                Resource::AddressBook(id) => Some(id),
                _ => None,
            })
            .collect();

        let mut address_book_map = std::collections::HashMap::new();

        // Hydrate in ONE `= ANY` round-trip (was one point SELECT per
        // accessible book — K serial round-trips on every CardDAV
        // discovery poll). Missing rows (deleted / trashed race) drop
        // out of the result set — matches the calendar-listing
        // carve-out.
        let ids: Vec<Uuid> = book_ids.into_iter().collect();
        for book in self.contact_storage.get_address_books_by_ids(&ids).await? {
            address_book_map.insert(*book.id(), book);
        }

        // Public address books surface for every authenticated caller
        // — same "internal-Read-for-everyone" semantics as
        // `is_public` on calendars.
        let public_address_books = self.contact_storage.get_public_address_books().await?;
        for book in public_address_books {
            if !address_book_map.contains_key(book.id()) {
                address_book_map.insert(*book.id(), book);
            }
        }

        Ok(address_book_map
            .into_values()
            .map(AddressBookDto::from)
            .collect())
    }

    async fn list_public_address_books(&self) -> Result<Vec<AddressBookDto>, DomainError> {
        let address_books = self.contact_storage.get_public_address_books().await?;
        let dtos: Vec<AddressBookDto> = address_books
            .into_iter()
            .map(AddressBookDto::from)
            .collect();
        Ok(dtos)
    }
}

impl ContactUseCase for ContactService {
    async fn create_contact(&self, dto: CreateContactDto) -> Result<ContactDto, DomainError> {
        let address_book_id = Uuid::parse_str(&dto.address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // AuthZ audit #19 (2026-07-12): previously required
        // `Permission::Update`, which is NOT in the Contributor bundle
        // (Read + Create) — Contributor grantees on a shared address
        // book couldn't add contacts via REST or CardDAV PUT despite
        // holding the intended Create permission. `Delete` uses Delete
        // (audit #13, above); creation must use Create. Same fix
        // applied to `create_contact_from_vcard` + `create_group`.
        let caller_id = Uuid::parse_str(&dto.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        self.require_address_book_perm(&address_book_id, &caller_id, Permission::Create)
            .await?;

        // Convert DTOs to domain entities
        let email: Vec<Email> = dto
            .email
            .into_iter()
            .map(|e| Email {
                email: e.email,
                r#type: e.r#type,
                is_primary: e.is_primary,
            })
            .collect();

        let phone: Vec<Phone> = dto
            .phone
            .into_iter()
            .map(|p| Phone {
                number: p.number,
                r#type: p.r#type,
                is_primary: p.is_primary,
            })
            .collect();

        let address: Vec<Address> = dto
            .address
            .into_iter()
            .map(|a| Address {
                street: a.street,
                city: a.city,
                state: a.state,
                postal_code: a.postal_code,
                country: a.country,
                r#type: a.r#type,
                is_primary: a.is_primary,
            })
            .collect();

        let mut contact = Contact::new(
            address_book_id,
            dto.full_name,
            dto.first_name,
            dto.last_name,
            dto.nickname,
            email,
            phone,
            address,
            dto.organization,
            dto.title,
            dto.notes,
            dto.photo_url,
            dto.birthday,
            dto.anniversary,
            String::new(), // Will be generated after creation
        );

        // Generate vCard data
        let vcard = self.generate_vcard(&contact);
        contact.set_vcard(vcard);
        let contact_with_vcard = contact;

        // Create the contact
        let created_contact = self
            .contact_storage
            .create_contact(contact_with_vcard)
            .await?;
        Ok(ContactDto::from(created_contact))
    }

    async fn create_contact_from_vcard(
        &self,
        dto: CreateContactVCardDto,
    ) -> Result<ContactDto, DomainError> {
        let address_book_id = Uuid::parse_str(&dto.address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // AuthZ audit #19 — see the sibling `create_contact` above.
        // This is the CardDAV `PUT contact.vcf` entry point; the fix
        // unblocks Contributor grantees creating contacts through the
        // CardDAV protocol as well as the REST surface.
        let caller_id = Uuid::parse_str(&dto.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        self.require_address_book_perm(&address_book_id, &caller_id, Permission::Create)
            .await?;

        // Parse vCard data
        let mut contact = Self::parse_vcard(&dto.vcard)?;

        // Set address book ID
        contact.set_address_book_id(address_book_id);

        // The contact was created with Contact::default() which generates a new ID
        // Set creation and update timestamps
        let now = Utc::now();
        contact.set_updated_at(now);

        // Create the contact
        let created_contact = self.contact_storage.create_contact(contact).await?;
        Ok(ContactDto::from(created_contact))
    }

    async fn update_contact(
        &self,
        contact_id: &str,
        update: UpdateContactDto,
    ) -> Result<ContactDto, DomainError> {
        let id = Uuid::parse_str(contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the current contact
        let contact = self
            .contact_storage
            .get_contact_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact", "not found"))?;

        // Check if user has write access to the address book
        let update_user_id = Uuid::parse_str(&update.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        self.require_address_book_perm(
            contact.address_book_id(),
            &update_user_id,
            Permission::Update,
        )
        .await?;

        // Destructure contact into owned parts for updates
        let parts = contact.into_parts();

        // Convert DTO fields to domain entities
        let email = if let Some(email_dtos) = update.email {
            email_dtos
                .into_iter()
                .map(|e| Email {
                    email: e.email,
                    r#type: e.r#type,
                    is_primary: e.is_primary,
                })
                .collect()
        } else {
            parts.email
        };

        let phone = if let Some(phone_dtos) = update.phone {
            phone_dtos
                .into_iter()
                .map(|p| Phone {
                    number: p.number,
                    r#type: p.r#type,
                    is_primary: p.is_primary,
                })
                .collect()
        } else {
            parts.phone
        };

        let address = if let Some(address_dtos) = update.address {
            address_dtos
                .into_iter()
                .map(|a| Address {
                    street: a.street,
                    city: a.city,
                    state: a.state,
                    postal_code: a.postal_code,
                    country: a.country,
                    r#type: a.r#type,
                    is_primary: a.is_primary,
                })
                .collect()
        } else {
            parts.address
        };

        // Update the contact object
        let mut updated_contact = Contact::from_raw(
            id,
            parts.address_book_id,
            parts.uid,
            update.full_name.or(parts.full_name),
            update.first_name.or(parts.first_name),
            update.last_name.or(parts.last_name),
            update.nickname.or(parts.nickname),
            email,
            phone,
            address,
            update.organization.or(parts.organization),
            update.title.or(parts.title),
            update.notes.or(parts.notes),
            update.photo_url.or(parts.photo_url),
            update.birthday.or(parts.birthday),
            update.anniversary.or(parts.anniversary),
            parts.vcard,                // Will be regenerated
            Uuid::new_v4().to_string(), // Generate new ETag
            parts.created_at,
            Utc::now(),
        );

        // Generate new vCard data
        let vcard = self.generate_vcard(&updated_contact);
        updated_contact.set_vcard(vcard);
        let contact_with_vcard = updated_contact;

        // Update the contact
        let result = self
            .contact_storage
            .update_contact(contact_with_vcard)
            .await?;
        Ok(ContactDto::from(result))
    }

    async fn delete_contact(&self, contact_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        let id = Uuid::parse_str(contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the current contact
        let contact = self
            .contact_storage
            .get_contact_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact", "not found"))?;

        // AuthZ audit #13 (2026-07-12): previously required
        // `Permission::Update`, which the Editor role bundle satisfies
        // (Read + Comment + Create + Update). Every Editor grantee on a
        // shared address book could delete individual contacts — a
        // silent privilege escalation because the intent for CardDAV
        // deletion is Delete, not Update. Sibling
        // `CalendarService::delete_event` was the ground-truth pattern.
        self.require_address_book_perm(contact.address_book_id(), &user_id, Permission::Delete)
            .await?;

        // Delete the contact
        self.contact_storage.delete_contact(&id).await?;
        Ok(())
    }

    async fn get_contact(
        &self,
        contact_id: &str,
        user_id: Uuid,
    ) -> Result<ContactDto, DomainError> {
        let id = Uuid::parse_str(contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the contact
        let contact = self
            .contact_storage
            .get_contact_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact", "not found"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(contact.address_book_id(), &user_id)
            .await?;

        Ok(ContactDto::from(contact))
    }

    async fn get_contact_by_uid(
        &self,
        address_book_id: &str,
        uid: &str,
        user_id: Uuid,
    ) -> Result<Option<ContactDto>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        let contact = self.contact_storage.get_contact_by_uid(&id, uid).await?;
        Ok(contact.map(ContactDto::from))
    }

    async fn get_contacts_by_uids(
        &self,
        address_book_id: &str,
        uids: &[String],
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let contacts = self.contact_storage.get_contacts_by_uids(&id, uids).await?;
        Ok(contacts.into_iter().map(ContactDto::from).collect())
    }

    async fn stream_contacts_by_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<futures::stream::BoxStream<'static, Result<ContactDto, DomainError>>, DomainError>
    {
        use futures::StreamExt;
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;
        // Same Read gate as `list_contacts`, once, before the cursor.
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;
        Ok(Box::pin(
            self.contact_storage
                .stream_contacts_by_book(id)
                .map(|r| r.map(ContactDto::from)),
        ))
    }

    async fn list_contacts(
        &self,
        address_book_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        // Get contacts
        let contacts = if limit.is_some() || offset.is_some() {
            let limit = limit.unwrap_or(100);
            let offset = offset.unwrap_or(0);
            self.contact_storage
                .get_contacts_by_address_book_paginated(&id, limit, offset)
                .await?
        } else {
            self.contact_storage
                .get_contacts_by_address_book(&id)
                .await?
        };
        let dtos = contacts.into_iter().map(ContactDto::from).collect();

        Ok(dtos)
    }

    async fn search_contacts(
        &self,
        address_book_id: &str,
        query: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        // Search contacts
        let contacts = self.contact_storage.search_contacts(&id, query).await?;
        let dtos = contacts.into_iter().map(ContactDto::from).collect();

        Ok(dtos)
    }

    async fn create_group(
        &self,
        dto: CreateContactGroupDto,
    ) -> Result<ContactGroupDto, DomainError> {
        let address_book_id = Uuid::parse_str(&dto.address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // AuthZ audit #19 — see the sibling `create_contact` above.
        let caller_id = Uuid::parse_str(&dto.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        self.require_address_book_perm(&address_book_id, &caller_id, Permission::Create)
            .await?;

        let group = ContactGroup::new(address_book_id, dto.name);

        let created_group = self.contact_storage.create_group(group).await?;
        Ok(ContactGroupDto::from(created_group))
    }

    async fn update_group(
        &self,
        group_id: &str,
        update: UpdateContactGroupDto,
    ) -> Result<ContactGroupDto, DomainError> {
        let id = Uuid::parse_str(group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        // Get the current group
        let group = self
            .contact_storage
            .get_group_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // Check if user has write access to the address book
        let caller_id = Uuid::parse_str(&update.user_id)
            .map_err(|_| DomainError::validation_error("Invalid user ID format"))?;
        self.require_address_book_perm(group.address_book_id(), &caller_id, Permission::Update)
            .await?;

        // Update the group
        let updated_group = ContactGroup::from_raw(
            id,
            *group.address_book_id(),
            update.name,
            *group.created_at(),
            Utc::now(),
        );

        let result = self.contact_storage.update_group(updated_group).await?;
        Ok(ContactGroupDto::from(result))
    }

    async fn delete_group(&self, group_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        let id = Uuid::parse_str(group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        // Get the current group
        let group = self
            .contact_storage
            .get_group_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // AuthZ audit #13 (2026-07-12): see the sibling `delete_contact`
        // above — required `Update` (in the Editor bundle) instead of
        // `Delete`, letting any Editor on a shared address book delete
        // groups they shouldn't.
        self.require_address_book_perm(group.address_book_id(), &user_id, Permission::Delete)
            .await?;

        // Delete the group
        self.contact_storage.delete_group(&id).await?;
        Ok(())
    }

    async fn get_group(
        &self,
        group_id: &str,
        user_id: Uuid,
    ) -> Result<ContactGroupDto, DomainError> {
        let id = Uuid::parse_str(group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        // Get the group
        let group = self
            .contact_storage
            .get_group_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(group.address_book_id(), &user_id)
            .await?;

        // Count-only read: the summary DTO never looks at the contacts, so
        // don't hydrate N full rows (vCard TEXT + 3 JSONB parses each).
        let members = self.contact_storage.count_contacts_in_group(&id).await?;

        let mut dto = ContactGroupDto::from(group);
        dto.members_count = Some(members as i32);

        Ok(dto)
    }

    async fn list_groups(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactGroupDto>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        // Get groups
        let groups = self.contact_storage.get_groups_by_address_book(&id).await?;
        let dtos = groups.into_iter().map(ContactGroupDto::from).collect();

        Ok(dtos)
    }

    async fn add_contact_to_group(
        &self,
        dto: GroupMembershipDto,
        user_id: Uuid,
    ) -> Result<(), DomainError> {
        let group_id = Uuid::parse_str(&dto.group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        let contact_id = Uuid::parse_str(&dto.contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the group
        let group = self
            .contact_storage
            .get_group_by_id(&group_id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // Check if user has write access to the address book
        self.require_address_book_perm(group.address_book_id(), &user_id, Permission::Update)
            .await?;

        // Add contact to group
        self.contact_storage
            .add_contact_to_group(&group_id, &contact_id)
            .await?;
        Ok(())
    }

    async fn remove_contact_from_group(
        &self,
        dto: GroupMembershipDto,
        user_id: Uuid,
    ) -> Result<(), DomainError> {
        let group_id = Uuid::parse_str(&dto.group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        let contact_id = Uuid::parse_str(&dto.contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the group
        let group = self
            .contact_storage
            .get_group_by_id(&group_id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // Check if user has write access to the address book
        self.require_address_book_perm(group.address_book_id(), &user_id, Permission::Update)
            .await?;

        // Remove contact from group
        self.contact_storage
            .remove_contact_from_group(&group_id, &contact_id)
            .await?;
        Ok(())
    }

    async fn list_contacts_in_group(
        &self,
        group_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError> {
        let id = Uuid::parse_str(group_id)
            .map_err(|_| DomainError::validation_error("Invalid group ID format"))?;

        // Get the group
        let group = self
            .contact_storage
            .get_group_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact group", "not found"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(group.address_book_id(), &user_id)
            .await?;

        // Get contacts in group
        let contacts = self.contact_storage.get_contacts_in_group(&id).await?;
        let dtos = contacts.into_iter().map(ContactDto::from).collect();

        Ok(dtos)
    }

    async fn list_groups_for_contact(
        &self,
        contact_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactGroupDto>, DomainError> {
        let id = Uuid::parse_str(contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the contact
        let contact = self
            .contact_storage
            .get_contact_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact", "not found"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(contact.address_book_id(), &user_id)
            .await?;

        // Get groups for contact
        let groups = self.contact_storage.get_groups_for_contact(&id).await?;
        let dtos = groups.into_iter().map(ContactGroupDto::from).collect();

        Ok(dtos)
    }

    async fn get_contact_vcard(
        &self,
        contact_id: &str,
        user_id: Uuid,
    ) -> Result<String, DomainError> {
        let id = Uuid::parse_str(contact_id)
            .map_err(|_| DomainError::validation_error("Invalid contact ID format"))?;

        // Get the contact
        let contact = self
            .contact_storage
            .get_contact_by_id(&id)
            .await?
            .ok_or_else(|| DomainError::not_found("Contact", "not found"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(contact.address_book_id(), &user_id)
            .await?;

        // Return the vCard data
        Ok(contact.vcard().to_string())
    }

    async fn get_contacts_as_vcards(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<(String, String)>, DomainError> {
        let id = Uuid::parse_str(address_book_id)
            .map_err(|_| DomainError::validation_error("Invalid address book ID format"))?;

        // Check if user has access to the address book
        self.require_address_book_read_or_public(&id, &user_id)
            .await?;

        // Get all contacts in the address book
        let contacts = self
            .contact_storage
            .get_contacts_by_address_book(&id)
            .await?;

        // Convert to Vec<(id, vcard)>
        let vcards = contacts
            .into_iter()
            .map(|contact| (contact.id().to_string(), contact.vcard().to_string()))
            .collect();

        Ok(vcards)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DefaultAddressBookLifecycleHook
//
// Ensures every internal user has at least one owned address book so
// CardDAV clients (Thunderbird, Apple Contacts, DAVx⁵) succeed at their
// PROPFIND-based address-book discovery on first connect. Without this,
// a fresh user's carddav home collection is empty and every mainstream
// client returns "no address books found" rather than offering to create
// one (see AtalayaLabs/OxiCloud#545 — same class of bug as CalDAV).
//
// Symmetric with `DefaultCalendarLifecycleHook`. See the calendar hook
// docstring for the design rationale (ownership-based idempotency, safety-
// net on login, external → internal upgrade, deletion behaviour).
// ─────────────────────────────────────────────────────────────────────────────

use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::domain::entities::user::User;
use crate::infrastructure::repositories::pg::AddressBookPgRepository;
use async_trait::async_trait;

pub struct DefaultAddressBookLifecycleHook {
    /// Owner-listing goes through the concrete repository (bypasses the
    /// storage port which doesn't expose owner-only enumeration —
    /// matching the pattern `PersonalDriveLifecycleHook` uses for
    /// `find_default_for_user`).
    address_book_repo: Arc<AddressBookPgRepository>,
    contact_storage: Arc<ContactStorageAdapter>,
    /// Concrete engine — `AuthorizationEngine` isn't dyn-compatible
    /// (native async-fn-in-trait), so we hold the concrete
    /// `PgAclEngine` matching the other lifecycle hooks.
    authorization: Arc<PgAclEngine>,
    /// Display name for the default address book. "Contacts" mirrors
    /// the Nextcloud convention CardDAV clients already recognise.
    default_name: String,
}

impl DefaultAddressBookLifecycleHook {
    pub fn new(
        address_book_repo: Arc<AddressBookPgRepository>,
        contact_storage: Arc<ContactStorageAdapter>,
        authorization: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            address_book_repo,
            contact_storage,
            authorization,
            default_name: "Contacts".to_string(),
        }
    }

    /// Idempotent provisioning. Shared by `on_user_created`,
    /// `on_user_login` (safety-net for pre-existing users), and
    /// `on_upgraded_to_internal` (external → internal promotion).
    async fn provision_if_needed(&self, user: &User) -> Result<(), DomainError> {
        if user.is_external() {
            return Ok(());
        }

        // Ownership-based idempotency check — same rationale as the
        // calendar hook. Any existing owned address book (auto-
        // provisioned earlier, user-created, migrated) is respected.
        // `EXISTS` short-circuits instead of hydrating every owned
        // address book to test emptiness, on EVERY login
        // (benches/ROUND13.md §Q2).
        let has_address_book = self
            .address_book_repo
            .has_owned_address_book(user.id())
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "DefaultAddressBookHook",
                    format!("has_owned_address_book: {e}"),
                )
            })?;
        if has_address_book {
            return Ok(());
        }

        // Provision. The address-book service constructs the entity
        // directly (no dedicated storage-adapter method), so we do the
        // same here: build the `AddressBook` domain type, persist via
        // the storage port, then seed the Owner role_grant.
        let address_book = AddressBook::new(
            self.default_name.clone(),
            user.id().to_string(),
            None,
            None,
            false,
        );
        let created = self
            .contact_storage
            .create_address_book(address_book)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "DefaultAddressBookHook",
                    format!("create_address_book: {e}"),
                )
            })?;
        self.authorization
            .set_role(
                user.id(),
                Subject::User(user.id()),
                Role::Owner,
                Resource::AddressBook(*created.id()),
                None,
            )
            .await?;

        tracing::info!(
            target: "user_lifecycle",
            hook = "default_address_book",
            user_id = %user.id(),
            address_book_id = %created.id(),
            "Default address book provisioned"
        );
        Ok(())
    }
}

#[async_trait]
impl UserLifecycleHook for DefaultAddressBookLifecycleHook {
    fn name(&self) -> &'static str {
        "default_address_book"
    }

    async fn on_user_created(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    async fn on_upgraded_to_internal(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        _user: &User,
        _mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        // `carddav.address_books.owner_id` has ON DELETE CASCADE on
        // `auth.users(id)`, and contacts cascade off address_book. The
        // trigger on `role_grants` reaps the token grants. No hook-side
        // cleanup needed.
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────
// Tests — parse_vcard property surface
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod parse_vcard_tests {
    use super::*;

    /// Wrap minimal vCard 3.0 header/footer around one or more
    /// property lines. CRLF-normalise, matching wire format.
    fn vcard(lines: &[&str]) -> String {
        let mut body =
            String::from("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:parse-test\r\nFN:Parse Test\r\n");
        for l in lines {
            body.push_str(l);
            body.push_str("\r\n");
        }
        body.push_str("END:VCARD\r\n");
        body
    }

    // ── TEL ───────────────────────────────────────────────────

    #[test]
    fn tel_plain_form_still_parses() {
        // Regression pin: pre-existing shape `TEL;TYPE=CELL:+1...`
        // (no VALUE=uri) must continue to parse cleanly.
        let body = vcard(&["TEL;TYPE=CELL:+15551234567"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.phone().len(), 1);
        assert_eq!(c.phone()[0].number, "+15551234567");
        assert_eq!(c.phone()[0].r#type, "mobile");
    }

    #[test]
    fn tel_uri_form_survives_first_colon_split() {
        // The #528-adjacent bug the fix targets. Pre-fix the
        // parser `split(':').nth(1)` would return "tel", losing
        // the actual number.
        let body = vcard(&["TEL;TYPE=cell;VALUE=uri:tel:+15551234567"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.phone().len(), 1);
        assert_eq!(
            c.phone()[0].number,
            "+15551234567",
            "URI-scheme prefix must be stripped so downstream UIs \
             show a clickable number, not `tel:+15551234567`."
        );
        assert_eq!(c.phone()[0].r#type, "mobile");
    }

    #[test]
    fn tel_uri_form_without_scheme_prefix_survives() {
        // Some clients emit VALUE=uri but no explicit `tel:` in
        // the value. Handle gracefully — we take everything after
        // the first colon and only strip `tel:` if present.
        let body = vcard(&["TEL;VALUE=uri:+15551234567"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.phone()[0].number, "+15551234567");
    }

    // ── ADR ───────────────────────────────────────────────────

    #[test]
    fn adr_full_structured_value_populates_all_fields() {
        // The reference shape from RFC 6350 §6.3.1:
        //   ADR;TYPE=HOME:pobox;ext;street;city;region;postal;country
        // Positions 0/1 (pobox, ext) are legacy and typically
        // empty on real client output; we skip them by design.
        let body = vcard(&["ADR;TYPE=HOME:;;42 Rue de Rivoli;Paris;Île-de-France;75001;France"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.address().len(), 1);
        let a = &c.address()[0];
        assert_eq!(a.street.as_deref(), Some("42 Rue de Rivoli"));
        assert_eq!(a.city.as_deref(), Some("Paris"));
        assert_eq!(a.state.as_deref(), Some("Île-de-France"));
        assert_eq!(a.postal_code.as_deref(), Some("75001"));
        assert_eq!(a.country.as_deref(), Some("France"));
        assert_eq!(a.r#type, "home");
        assert!(a.is_primary, "first ADR should be primary");
    }

    #[test]
    fn adr_partial_value_only_populates_present_fields() {
        // Client sends street + city only — the other structured
        // components stay None (not "" — that would confuse the
        // Address DTO's Option<String>-based null semantics).
        let body = vcard(&["ADR:;;42 Rue de Rivoli;Paris;;;"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.address().len(), 1);
        let a = &c.address()[0];
        assert_eq!(a.street.as_deref(), Some("42 Rue de Rivoli"));
        assert_eq!(a.city.as_deref(), Some("Paris"));
        assert!(a.state.is_none());
        assert!(a.postal_code.is_none());
        assert!(a.country.is_none());
        assert_eq!(a.r#type, "other", "no TYPE param → 'other'");
    }

    #[test]
    fn adr_all_empty_is_dropped() {
        // Some clients emit `ADR:;;;;;;` as a "clear this
        // address" operation. Storing an empty row would show as
        // a blank address slot in UIs. Skip it.
        let body = vcard(&["ADR:;;;;;;"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.address().len(), 0);
    }

    #[test]
    fn adr_type_work_recognized() {
        let body = vcard(&["ADR;TYPE=WORK:;;5 Wall St;NYC;NY;10005;USA"]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.address()[0].r#type, "work");
    }

    #[test]
    fn adr_and_tel_coexist() {
        // Two independent fixes on the same PUT should both
        // populate — proves neither branch consumes lines meant
        // for the other via prefix ambiguity.
        let body = vcard(&[
            "TEL;TYPE=CELL:+15551234567",
            "ADR;TYPE=HOME:;;42 Rue de Rivoli;Paris;;75001;France",
        ]);
        let c = ContactService::parse_vcard(&body).expect("valid vcard");
        assert_eq!(c.phone().len(), 1);
        assert_eq!(c.phone()[0].number, "+15551234567");
        assert_eq!(c.address().len(), 1);
        assert_eq!(c.address()[0].city.as_deref(), Some("Paris"));
    }
}
