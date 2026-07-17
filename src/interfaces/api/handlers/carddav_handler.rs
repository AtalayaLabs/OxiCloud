/**
 * CardDAV Handler Module
 *
 * This module implements the CardDAV protocol (RFC 6352) endpoints for OxiCloud.
 * It provides contact/address book access and management through standard CardDAV
 * methods, allowing clients like Thunderbird, Apple Contacts, GNOME Contacts,
 * and DAVx⁵ to sync contacts.
 *
 * Supported methods:
 * - OPTIONS: Advertise CardDAV capabilities
 * - PROPFIND: List address books and their properties
 * - REPORT: Query contacts (addressbook-query, addressbook-multiget)
 * - MKCOL (ext): Create a new address book
 * - PUT: Create/update contacts (.vcf)
 * - GET: Retrieve contact vCard data
 * - DELETE: Remove address books or contacts
 * - PROPPATCH: Modify address book properties
 */
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderName, Request, StatusCode, header},
    response::Response,
};
use bytes::{Buf, Bytes};
use quick_xml::Writer;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::adapters::carddav_adapter::{
    CardDavAdapter, CardDavReportType, contact_to_vcard,
};
use crate::application::adapters::uid_from_multiget_href;
use crate::application::adapters::webdav_adapter::{PropFindRequest, PropFindType};
use crate::application::dtos::address_book_dto::{CreateAddressBookDto, UpdateAddressBookDto};
use crate::application::dtos::contact_dto::{ContactDto, CreateContactVCardDto};
use crate::application::ports::carddav_ports::{AddressBookUseCase, ContactUseCase};
use crate::application::ports::change_log_port::SyncChange;
use crate::application::services::carddav_sync_collection_service::CarddavSyncCollectionService;
use crate::application::services::contact_service::ContactService;
use crate::common::di::AppState;
use crate::domain::entities::sync_token::SyncToken;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::{AuthUser, CurrentUser};

const HEADER_DAV: HeaderName = HeaderName::from_static("dav");

/// Maximum allowed request body size for CardDAV XML/vCard endpoints (1 MB).
/// Prevents OOM/DoS via unbounded body buffering.
const MAX_CARDDAV_BODY: usize = 1_048_576;

/// Creates CardDAV routes with full path prefixes.
///
/// Uses `merge()` instead of `nest()` to avoid Axum's trailing-slash routing gap.
/// Registers `/carddav`, `/carddav/`, and `/carddav/{*path}` explicitly.
pub fn carddav_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/carddav/{*path}",
            axum::routing::any(handle_carddav_methods),
        )
        .route("/carddav/", axum::routing::any(handle_carddav_methods_root))
        .route("/carddav", axum::routing::any(handle_carddav_methods_root))
}

/// Creates the RFC 6764 well-known discovery route for CardDAV.
/// Public (no auth) — simply redirects to the CardDAV root so clients that
/// bootstrap from `/.well-known/carddav` can locate the service.
pub fn well_known_routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/.well-known/carddav",
        axum::routing::any(handle_well_known_carddav),
    )
}

async fn handle_well_known_carddav() -> Response<Body> {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, "/carddav/")
        .body(Body::empty())
        .unwrap()
}

async fn handle_carddav_methods_root(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    handle_carddav_methods_inner(state, req, String::new()).await
}

async fn handle_carddav_methods(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let uri = req.uri().clone();
    let path = extract_carddav_path(uri.path());
    reject_path_traversal(&path)?;
    handle_carddav_methods_inner(state, req, path).await
}

async fn handle_carddav_methods_inner(
    state: Arc<AppState>,
    req: Request<Body>,
    path: String,
) -> Result<Response<Body>, AppError> {
    let method = req.method().clone();

    match method.as_str() {
        "OPTIONS" => handle_options().await,
        "PROPFIND" => handle_propfind(state.clone(), req, &path).await,
        "REPORT" => handle_report(state.clone(), req, &path).await,
        "MKCOL" => handle_mkcol(state.clone(), req, &path).await,
        "PUT" => handle_put(state.clone(), req, &path).await,
        "GET" => handle_get(state.clone(), req, &path).await,
        "DELETE" => handle_delete(state.clone(), req, &path).await,
        "PROPPATCH" => handle_proppatch(state.clone(), req, &path).await,
        _ => Err(AppError::method_not_allowed(format!(
            "Method not allowed: {}",
            method
        ))),
    }
}

/// Extract the CardDAV path from the full URI path, percent-decoding the result.
fn extract_carddav_path(uri_path: &str) -> String {
    let encoded = if let Some(pos) = uri_path.find("/carddav/") {
        let after = &uri_path[pos + 9..];
        after.trim_end_matches('/')
    } else if uri_path.ends_with("/carddav") {
        ""
    } else {
        uri_path.trim_start_matches('/').trim_end_matches('/')
    };
    percent_encoding::percent_decode_str(encoded)
        .decode_utf8_lossy()
        .into_owned()
}

/// Reject paths that contain path-traversal segments (`.` or `..`).
fn reject_path_traversal(path: &str) -> Result<(), AppError> {
    for segment in path.split('/') {
        if segment == ".." || segment == "." {
            return Err(AppError::bad_request(
                "Path must not contain '.' or '..' segments",
            ));
        }
    }
    Ok(())
}

// ─── Helper: strip optional username prefix from CardDAV path ────────
//
// The `addressbook-home-set` discovery property returns `/carddav/{username}/`,
// so standard clients will prefix all subsequent requests with the username segment.
// The handlers below expect paths of the form `{address_book_id}` or `{address_book_id}/{contact}.vcf`.
//
// Heuristic: if the first path segment is a valid UUID it is already an
// address book ID; otherwise treat it as a username and skip it.

fn strip_username_prefix(path: &str) -> &str {
    if let Some(pos) = path.find('/') {
        let first = &path[..pos];
        if uuid::Uuid::parse_str(first).is_ok() {
            path
        } else {
            &path[pos + 1..]
        }
    } else if uuid::Uuid::parse_str(path).is_ok() {
        path
    } else {
        ""
    }
}

// ─── Helper: extract user from request ───────────────────────────────

fn extract_user(req: &Request<Body>) -> Result<AuthUser, AppError> {
    req.extensions()
        .get::<Arc<CurrentUser>>()
        .cloned()
        .map(AuthUser)
        .ok_or_else(|| AppError::unauthorized("Authentication required"))
}

fn get_addressbook_service(state: &AppState) -> Result<&Arc<ContactService>, AppError> {
    state.addressbook_use_case.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "CardDAV address book service is not configured",
            "NotImplemented",
        )
    })
}

/// Rows per emitted page for the streaming CardDAV emitters — contacts
/// carry no master/exception bundling, so pages cut anywhere.
const CARDDAV_STREAM_PAGE_CONTACTS: usize = 500;

/// Streamed multistatus REPORT: header, one chunk per cursor page,
/// footer. Byte-compatible with the buffered
/// `generate_contacts_response` output; TTFB becomes the first page and
/// the whole-book DTO Vec is never materialised.
fn build_streaming_contacts_report(
    contact_svc: Arc<ContactService>,
    address_book_id: String,
    report: CardDavReportType,
    base_href: String,
    user_id: uuid::Uuid,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let mut buf = Vec::with_capacity(160);
        {
            let mut w = Writer::new(&mut buf);
            CardDavAdapter::write_report_multistatus_start(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);

        {
            use futures::TryStreamExt;
            let mut rows = contact_svc
                .stream_contacts_by_book(&address_book_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut page: Vec<ContactDto> =
                Vec::with_capacity(CARDDAV_STREAM_PAGE_CONTACTS);
            loop {
                let next = rows
                    .try_next()
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let flush = match &next {
                    Some(_) => page.len() >= CARDDAV_STREAM_PAGE_CONTACTS,
                    None => !page.is_empty(),
                };
                if flush {
                    let mut chunk = Vec::with_capacity(page.len() * 256 + 64);
                    {
                        let mut w = Writer::new(&mut chunk);
                        CardDavAdapter::write_contacts_report_page(
                            &mut w, &page, &report, &base_href,
                        )
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    }
                    page.clear();
                    yield Bytes::from(chunk);
                }
                match next {
                    Some(c) => page.push(c),
                    None => break,
                }
            }
        }

        let mut buf = Vec::with_capacity(32);
        {
            let mut w = Writer::new(&mut buf);
            CardDavAdapter::write_carddav_multistatus_end(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Streamed depth-1 address-book PROPFIND: head (multistatus + the
/// book's own response), one chunk per cursor page, footer.
fn build_streaming_book_propfind(
    contact_svc: Arc<ContactService>,
    address_book: crate::application::dtos::address_book_dto::AddressBookDto,
    propfind_request: PropFindRequest,
    address_book_id: String,
    base_href: String,
    user_id: uuid::Uuid,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let mut buf = Vec::with_capacity(2048);
        {
            let mut w = Writer::new(&mut buf);
            CardDavAdapter::write_collection_head(
                &mut w,
                &address_book,
                &propfind_request,
                &base_href,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);

        {
            use futures::TryStreamExt;
            let mut rows = contact_svc
                .stream_contacts_by_book(&address_book_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut page: Vec<ContactDto> =
                Vec::with_capacity(CARDDAV_STREAM_PAGE_CONTACTS);
            loop {
                let next = rows
                    .try_next()
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let flush = match &next {
                    Some(_) => page.len() >= CARDDAV_STREAM_PAGE_CONTACTS,
                    None => !page.is_empty(),
                };
                if flush {
                    let mut chunk = Vec::with_capacity(page.len() * 512 + 64);
                    {
                        let mut w = Writer::new(&mut chunk);
                        CardDavAdapter::write_collection_contact_page(&mut w, &page, &base_href)
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                    }
                    page.clear();
                    yield Bytes::from(chunk);
                }
                match next {
                    Some(c) => page.push(c),
                    None => break,
                }
            }
        }

        let mut buf = Vec::with_capacity(32);
        {
            let mut w = Writer::new(&mut buf);
            CardDavAdapter::write_carddav_multistatus_end(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn get_contact_service(state: &AppState) -> Result<&Arc<ContactService>, AppError> {
    state.contact_use_case.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "CardDAV contact service is not configured",
            "NotImplemented",
        )
    })
}

fn get_carddav_sync_collection_service(
    state: &AppState,
) -> Result<&Arc<CarddavSyncCollectionService>, AppError> {
    state
        .carddav_sync_collection_service
        .as_ref()
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "CardDAV sync-collection service is not configured",
                "NotImplemented",
            )
        })
}

// ─── OPTIONS ─────────────────────────────────────────────────────────

async fn handle_options() -> Result<Response<Body>, AppError> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_DAV, "1, 2, 3, addressbook")
        .header(
            header::ALLOW,
            "OPTIONS, GET, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCOL",
        )
        .body(Body::empty())
        .unwrap())
}

// ─── PROPFIND ────────────────────────────────────────────────────────

async fn handle_propfind(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1")
        .to_string();

    let user = extract_user(&req)?;
    let addressbook_service = get_addressbook_service(&state)?;
    let contact_svc = get_contact_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CARDDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let propfind_request = if body_bytes.is_empty() {
        PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        }
    } else {
        crate::application::adapters::webdav_adapter::WebDavAdapter::parse_propfind(
            body_bytes.reader(),
        )
        .map_err(|e| AppError::bad_request(format!("Failed to parse PROPFIND: {}", e)))?
    };

    // Discovery: the true root `/carddav/` advertises current-user-principal and
    // addressbook-home-set so clients (DAVx5, Apple Contacts) can locate the
    // address books. Depth 0 → only the root entry; Depth 1+ → also the books.
    if path.is_empty() {
        let address_books = if depth == "0" {
            vec![]
        } else {
            addressbook_service
                .list_user_address_books(user.id)
                .await
                .map_err(AppError::from)?
        };

        let mut response_body = Vec::new();
        CardDavAdapter::generate_root_propfind_response(
            &mut response_body,
            &address_books,
            &propfind_request,
            "/carddav/",
            &user.username,
        )
        .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(response_body))
            .unwrap());
    }

    // Discovery: principal resource `/carddav/principals/{username}/` returns the
    // addressbook-home-set the client should enumerate next.
    if path == "principals" || path.starts_with("principals/") {
        let username = path
            .strip_prefix("principals/")
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or(&user.username);

        let mut response_body = Vec::new();
        CardDavAdapter::generate_principal_propfind_response(
            &mut response_body,
            &propfind_request,
            username,
        )
        .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(response_body))
            .unwrap());
    }

    let effective_path = strip_username_prefix(path);

    if effective_path.is_empty() {
        // User address-book home `/carddav/{username}/` — list the user's books.
        let address_books = addressbook_service
            .list_user_address_books(user.id)
            .await
            .map_err(AppError::from)?;

        let user_part = path.split('/').next().unwrap_or(path);
        let base_href = format!("/carddav/{}/", user_part);
        let mut response_body = Vec::new();
        CardDavAdapter::generate_addressbooks_propfind_response(
            &mut response_body,
            &address_books,
            &propfind_request,
            &base_href,
        )
        .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

        Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(response_body))
            .unwrap())
    } else {
        let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
        let address_book_id = parts[0];

        if parts.len() == 1 {
            // Address book collection
            let address_book = addressbook_service
                .get_address_book(address_book_id, user.id)
                .await
                .map_err(|e| AppError::not_found(format!("Address book not found: {}", e)))?;

            // Depth-1 streams the contact listing page by page; depth-0
            // has no contact section and keeps the tiny buffered path.
            if depth != "0" {
                let base_href = format!("/carddav/{}/", address_book_id);
                return Ok(build_streaming_book_propfind(
                    contact_svc.clone(),
                    address_book,
                    propfind_request,
                    address_book_id.to_string(),
                    base_href,
                    user.id,
                ));
            }

            let base_href = &format!("/carddav/{}/", address_book_id);
            let mut response_body = Vec::new();

            CardDavAdapter::generate_addressbook_collection_propfind(
                &mut response_body,
                &address_book,
                &[],
                &propfind_request,
                base_href,
                &depth,
            )
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

            Ok(Response::builder()
                .status(StatusCode::MULTI_STATUS)
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(Body::from(response_body))
                .unwrap())
        } else {
            // Individual contact .vcf — indexed lookup by vCard UID.
            let contact_file = parts[1];
            let contact_uid = contact_file.trim_end_matches(".vcf");

            let contact = contact_svc
                .get_contact_by_uid(address_book_id, contact_uid, user.id)
                .await
                .map_err(AppError::from)?
                .ok_or_else(|| {
                    AppError::not_found(format!("Contact not found: {}", contact_uid))
                })?;

            // Build single-resource PROPFIND response
            let base_href = &format!("/carddav/{}/", address_book_id);
            let report = CardDavReportType::AddressbookMultiget {
                hrefs: vec![format!("{}{}.vcf", base_href, contact_uid)],
                props: vec![],
            };

            let mut response_body = Vec::new();
            CardDavAdapter::generate_contacts_response(
                &mut response_body,
                std::slice::from_ref(&contact),
                &report,
                base_href,
                &[],
                None,
            )
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

            Ok(Response::builder()
                .status(StatusCode::MULTI_STATUS)
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(Body::from(response_body))
                .unwrap())
        }
    }
}

// ─── REPORT ──────────────────────────────────────────────────────────

async fn handle_report(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let contact_svc = get_contact_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CARDDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let report = CardDavAdapter::parse_report(body_bytes.reader())
        .map_err(|e| AppError::bad_request(format!("Failed to parse REPORT: {}", e)))?;

    let effective_path = strip_username_prefix(path);
    let address_book_id = effective_path.split('/').next().unwrap_or(effective_path);

    if address_book_id.is_empty() {
        return Err(AppError::bad_request("Address book ID required in path"));
    }

    // Whole-book queries stream; sync-collection needs incremental-delta
    // awareness (deleted_hrefs/sync_token) the streaming path doesn't
    // have, so it's excluded here — same as CalDAV's CalendarQuery-only
    // fast path. Bounded multiget also keeps the buffered path.
    if matches!(&report, CardDavReportType::AddressbookQuery { .. }) {
        let base_href = format!("/carddav/{}/", address_book_id);
        return Ok(build_streaming_contacts_report(
            contact_svc.clone(),
            address_book_id.to_string(),
            report,
            base_href,
            user.id,
        ));
    }

    let base_href = &format!("/carddav/{}/", address_book_id);

    let (contacts, deleted_hrefs, sync_token): (Vec<ContactDto>, Vec<String>, Option<String>) =
        match &report {
            CardDavReportType::AddressbookQuery { .. } => {
                unreachable!("addressbook-query streams above")
            }
            CardDavReportType::AddressbookMultiget { hrefs, .. } => {
                // Indexed batch lookup (`uid = ANY(...)`) — a multiget for a
                // handful of contacts must not pay for listing the whole
                // address book and filtering client-side.
                let uids: Vec<String> = hrefs
                    .iter()
                    .filter_map(|href| uid_from_multiget_href(href, ".vcf"))
                    .collect();

                let contacts = contact_svc
                    .get_contacts_by_uids(address_book_id, &uids, user.id)
                    .await
                    .map_err(AppError::from)?;
                (contacts, Vec::new(), None)
            }
            CardDavReportType::SyncCollection { sync_token, .. } => {
                let sync_service = get_carddav_sync_collection_service(&state)?;
                let address_book_uuid = Uuid::parse_str(address_book_id)
                    .map_err(|_| AppError::bad_request("Invalid address book ID"))?;

                if sync_token.is_empty() {
                    let contacts = contact_svc
                        .list_contacts(address_book_id, None, None, user.id)
                        .await
                        .map_err(AppError::from)?;
                    let new_token = sync_service
                        .mint_initial_token(address_book_uuid, user.id)
                        .await?;
                    (contacts, Vec::new(), Some(new_token.to_string()))
                } else {
                    let token = SyncToken::parse_for_collection(sync_token, address_book_uuid)
                        .map_err(|e| AppError::bad_request(format!("Invalid sync-token: {e}")))?;
                    let delta = sync_service
                        .list_changes_with_perms(address_book_uuid, Some(token), user.id)
                        .await?;
                    let mut contacts = Vec::new();
                    let mut deleted = Vec::new();
                    for change in delta.changes {
                        match change {
                            SyncChange::Upserted(contact) => contacts.push(contact),
                            SyncChange::Deleted { href_hint, .. } => {
                                deleted.push(format!("{}{}", base_href, href_hint));
                            }
                        }
                    }
                    (contacts, deleted, Some(delta.new_token.to_string()))
                }
            }
        };

    let mut response_body = Vec::new();
    CardDavAdapter::generate_contacts_response(
        &mut response_body,
        &contacts,
        &report,
        base_href,
        &deleted_hrefs,
        sync_token.as_deref(),
    )
    .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap())
}

// ─── MKCOL (create address book) ─────────────────────────────────────

async fn handle_mkcol(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let addressbook_service = get_addressbook_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CARDDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let (name, description, color) = if body_bytes.is_empty() {
        let name = path
            .split('/')
            .next_back()
            .unwrap_or("New Address Book")
            .to_string();
        (name, None, None)
    } else {
        CardDavAdapter::parse_mkaddressbook(body_bytes.reader())
            .map_err(|e| AppError::bad_request(format!("Failed to parse MKCOL: {}", e)))?
    };

    let create_dto = CreateAddressBookDto {
        name,
        owner_id: user.id.to_string(),
        description,
        color,
        is_public: Some(false),
    };

    // See the comment on the vCard PUT path — kind-aware error mapping
    // so a client MKCOL body with a bad name / duplicate returns
    // 400 / 409 instead of an opaque 500.
    addressbook_service
        .create_address_book(create_dto)
        .await
        .map_err(AppError::from)?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

// ─── PUT (.vcf) ──────────────────────────────────────────────────────

async fn handle_put(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let contact_svc = get_contact_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    if parts.len() < 2 {
        return Err(AppError::bad_request(
            "Path must be {address_book_id}/{uid}.vcf",
        ));
    }

    let address_book_id = parts[0];

    let body_bytes = body::to_bytes(req.into_body(), MAX_CARDDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let vcard_data = String::from_utf8(body_bytes.to_vec())
        .map_err(|e| AppError::bad_request(format!("Invalid UTF-8 in vCard data: {}", e)))?;

    // Extract UID from vCard
    let vcard_uid = extract_uid_from_vcard(&vcard_data);

    // Check if contact already exists — indexed single-row lookup
    // (listing the whole address book made imports O(N²)).
    let existing = if let Some(ref uid) = vcard_uid {
        contact_svc
            .get_contact_by_uid(address_book_id, uid, user.id)
            .await
            .unwrap_or_default()
    } else {
        None
    };

    if let Some(existing_contact) = existing {
        // Update: delete + recreate from vCard. `AppError::from` maps
        // the domain-error ErrorKind onto the right status code:
        // NotFound → 404 (contact/address-book gone), AccessDenied →
        // 403, InvalidInput → 400 (malformed vCard PUT from the
        // client). Naive `internal_error(...)` wrapping used to hide
        // all client-input bugs as 500 — same class of bug as the
        // CalDAV `create_event_from_ical` path (see #545).
        contact_svc
            .delete_contact(&existing_contact.id, user.id)
            .await
            .map_err(AppError::from)?;

        let create_dto = CreateContactVCardDto {
            address_book_id: address_book_id.to_string(),
            vcard: vcard_data,
            user_id: user.id.to_string(),
        };
        let contact = contact_svc
            .create_contact_from_vcard(create_dto)
            .await
            .map_err(AppError::from)?;

        Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ETAG, format!("\"{}\"", contact.etag))
            .body(Body::empty())
            .unwrap())
    } else {
        let create_dto = CreateContactVCardDto {
            address_book_id: address_book_id.to_string(),
            vcard: vcard_data,
            user_id: user.id.to_string(),
        };

        // See the comment above the update branch — same rationale for
        // preferring `AppError::from` over blanket 500.
        let contact = contact_svc
            .create_contact_from_vcard(create_dto)
            .await
            .map_err(AppError::from)?;

        Ok(Response::builder()
            .status(StatusCode::CREATED)
            .header(header::ETAG, format!("\"{}\"", contact.etag))
            .body(Body::empty())
            .unwrap())
    }
}

/// Extract UID from vCard data
fn extract_uid_from_vcard(vcard_data: &str) -> Option<String> {
    for line in vcard_data.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("UID:") {
            return Some(stripped.trim().to_string());
        }
    }
    None
}

// ─── GET (.vcf) ──────────────────────────────────────────────────────

async fn handle_get(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let contact_svc = get_contact_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    let address_book_id = parts[0];

    if parts.len() < 2 {
        // GET on address book collection — return all contacts as vcf
        let contacts = contact_svc
            .list_contacts(address_book_id, None, None, user.id)
            .await
            .map_err(AppError::from)?;

        let mut vcf_data = String::new();
        for contact in &contacts {
            vcf_data.push_str(&contact_to_vcard(contact));
        }

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/vcard; charset=utf-8")
            .body(Body::from(vcf_data))
            .unwrap())
    } else {
        // GET on individual contact — indexed lookup by vCard UID.
        let contact_file = parts[1];
        let contact_uid = contact_file.trim_end_matches(".vcf");

        let contact = contact_svc
            .get_contact_by_uid(address_book_id, contact_uid, user.id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::not_found(format!("Contact not found: {}", contact_uid)))?;

        let vcard = contact_to_vcard(&contact);

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/vcard; charset=utf-8")
            .header(header::ETAG, format!("\"{}\"", contact.etag))
            .body(Body::from(vcard))
            .unwrap())
    }
}

// ─── DELETE ──────────────────────────────────────────────────────────

async fn handle_delete(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let addressbook_service = get_addressbook_service(&state)?;
    let contact_svc = get_contact_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    let address_book_id = parts[0];

    if address_book_id.is_empty() {
        return Err(AppError::bad_request("Address book ID required"));
    }

    if parts.len() < 2 {
        // Delete address book
        addressbook_service
            .delete_address_book(address_book_id, user.id)
            .await
            .map_err(AppError::from)?;
    } else {
        // Delete contact — indexed lookup by vCard UID.
        let contact_file = parts[1];
        let contact_uid = contact_file.trim_end_matches(".vcf");

        let contact = contact_svc
            .get_contact_by_uid(address_book_id, contact_uid, user.id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::not_found(format!("Contact not found: {}", contact_uid)))?;

        contact_svc
            .delete_contact(&contact.id, user.id)
            .await
            .map_err(AppError::from)?;
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

// ─── PROPPATCH ───────────────────────────────────────────────────────

async fn handle_proppatch(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let addressbook_service = get_addressbook_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CARDDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let ops = crate::application::adapters::webdav_adapter::WebDavAdapter::parse_proppatch(
        body_bytes.reader(),
    )
    .map_err(|e| AppError::bad_request(format!("Failed to parse PROPPATCH: {}", e)))?;

    let effective_path = strip_username_prefix(path);
    let address_book_id = effective_path.split('/').next().unwrap_or(effective_path);

    if address_book_id.is_empty() {
        return Err(AppError::bad_request("Address book ID required"));
    }

    let mut update = UpdateAddressBookDto {
        name: None,
        description: None,
        color: None,
        is_public: None,
        user_id: user.id.to_string(),
    };

    for op in &ops {
        if let crate::application::adapters::webdav_adapter::PropPatchOp::Set(prop) = op {
            match prop.name.name.as_str() {
                "displayname" => update.name = Some(prop.value.clone().unwrap_or_default()),
                "addressbook-description" => update.description = prop.value.clone(),
                "calendar-color" | "addressbook-color" => update.color = prop.value.clone(),
                _ => {}
            }
        }
    }

    if update.name.is_some() || update.description.is_some() || update.color.is_some() {
        addressbook_service
            .update_address_book(address_book_id, update)
            .await
            .map_err(AppError::from)?;
    }

    let mut results = Vec::new();
    for op in &ops {
        match op {
            crate::application::adapters::webdav_adapter::PropPatchOp::Set(prop) => {
                results.push((&prop.name, true));
            }
            crate::application::adapters::webdav_adapter::PropPatchOp::Remove(name) => {
                results.push((name, true));
            }
        }
    }

    let href = format!("/carddav/{}", path);
    let mut response_body = Vec::new();
    crate::application::adapters::webdav_adapter::WebDavAdapter::generate_proppatch_response(
        &mut response_body,
        &href,
        &results,
    )
    .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::strip_username_prefix;

    #[test]
    fn test_strip_username_prefix_uuid_only() {
        let uuid = "ae8ae236-709f-4939-b766-37ad589ac7f2";
        assert_eq!(strip_username_prefix(uuid), uuid);
    }

    #[test]
    fn test_strip_username_prefix_uuid_with_contact() {
        let path = "ae8ae236-709f-4939-b766-37ad589ac7f2/contact.vcf";
        assert_eq!(strip_username_prefix(path), path);
    }

    #[test]
    fn test_strip_username_prefix_username_and_uuid() {
        let path = "timm/ae8ae236-709f-4939-b766-37ad589ac7f2";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2"
        );
    }

    #[test]
    fn test_strip_username_prefix_username_uuid_and_contact() {
        let path = "timm/ae8ae236-709f-4939-b766-37ad589ac7f2/contact.vcf";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2/contact.vcf"
        );
    }

    #[test]
    fn test_strip_username_prefix_bare_username() {
        assert_eq!(strip_username_prefix("timm"), "");
    }

    #[test]
    fn test_strip_username_prefix_empty() {
        assert_eq!(strip_username_prefix(""), "");
    }

    #[test]
    fn test_strip_username_prefix_email_style_username() {
        let path = "user@example.com/ae8ae236-709f-4939-b766-37ad589ac7f2/contact.vcf";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2/contact.vcf"
        );
    }
}
