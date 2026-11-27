//! WOPI Discovery service.
//!
//! Fetches and caches the WOPI discovery XML from the editor (Collabora/OnlyOffice).
//! The discovery document describes which file types the editor supports and
//! provides the action URLs for view/edit operations.

use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::common::errors::{DomainError, ErrorKind};

/// A single WOPI action from the discovery XML.
#[derive(Clone, Debug)]
pub struct WopiAction {
    /// Action name: "view", "edit", "editnew", "convert", etc.
    pub name: String,
    /// File extension: "docx", "xlsx", etc.
    pub ext: String,
    /// Optional target extension for `"convert"` actions (e.g. `"docx"`, `"odt"`).
    pub targetext: Option<String>,
    /// Template URL with placeholders (WOPI_SOURCE, UI_LLCC)
    pub urlsrc: String,
}

/// Result of resolving a requested WOPI action against the discovery document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWopiAction {
    /// The actual WOPI action selected from discovery (e.g. `"edit"`, `"view"`, `"convert"`).
    pub action: String,
    /// Target extension when `action == "convert"` (e.g. `"docx"`, `"odt"`).
    pub target_ext: Option<String>,
    /// Fully built editor URL with `WOPISrc` populated and unused placeholders stripped.
    pub url: String,
}

/// Caches parsed WOPI discovery data from the editor.
pub struct WopiDiscoveryService {
    discovery_url: String,
    /// Map: extension -> Vec<WopiAction>
    actions: Arc<RwLock<HashMap<String, Vec<WopiAction>>>>,
    last_fetched: Arc<RwLock<Option<Instant>>>,
    cache_ttl: Duration,
    /// HTTP client with timeout (shared across requests).
    http_client: reqwest::Client,
    /// Mutex to prevent concurrent refresh stampede.
    refreshing: Arc<tokio::sync::Mutex<()>>,
}

impl WopiDiscoveryService {
    pub fn new(discovery_url: String, cache_ttl_secs: u64) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client for WOPI discovery");

        Self {
            discovery_url,
            actions: Arc::new(RwLock::new(HashMap::new())),
            last_fetched: Arc::new(RwLock::new(None)),
            cache_ttl: Duration::from_secs(cache_ttl_secs),
            http_client,
            refreshing: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Fetch and parse the discovery XML from the WOPI client.
    pub async fn refresh_discovery(&self) -> Result<(), DomainError> {
        tracing::info!("Fetching WOPI discovery from {}", self.discovery_url);

        let response = self
            .http_client
            .get(&self.discovery_url)
            .send()
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "WopiDiscovery",
                    format!("Failed to fetch discovery XML: {}", e),
                )
            })?;

        let response = response.error_for_status().map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "WopiDiscovery",
                format!("Discovery endpoint returned error: {}", e),
            )
        })?;

        let xml_text = response.text().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "WopiDiscovery",
                format!("Failed to read discovery response: {}", e),
            )
        })?;

        let actions = Self::parse_discovery_xml(&xml_text)?;

        tracing::info!(
            "WOPI discovery loaded: {} extensions supported",
            actions.len()
        );

        *self.actions.write().await = actions;
        *self.last_fetched.write().await = Some(Instant::now());

        Ok(())
    }

    /// Ensure the discovery cache is fresh, refreshing if needed.
    /// Uses a mutex so only one caller refreshes at a time (stampede prevention).
    async fn ensure_fresh(&self) -> Result<(), DomainError> {
        let needs_refresh = {
            let last = self.last_fetched.read().await;
            match *last {
                None => true,
                Some(t) => t.elapsed() > self.cache_ttl,
            }
        };
        if needs_refresh {
            let _guard = self.refreshing.lock().await;
            // Re-check after acquiring the lock (another caller may have refreshed)
            let still_stale = {
                let last = self.last_fetched.read().await;
                match *last {
                    None => true,
                    Some(t) => t.elapsed() > self.cache_ttl,
                }
            };
            if still_stale {
                self.refresh_discovery().await?;
            }
        }
        Ok(())
    }

    /// Build the concrete editor URL for a discovery action entry.
    fn build_action_url(wopi_action: &WopiAction, wopi_src: &str) -> String {
        let mut url = wopi_action
            .urlsrc
            .replace("WOPI_SOURCE", &urlencoding::encode(wopi_src))
            .replace("UI_LLCC", "en-US");

        // Clean up unused placeholder parameters
        url = Self::clean_placeholder_params(&url);

        // Some discovery documents return a bare `cool.html?` URL without
        // embedding WOPISrc in the template. Ensure WOPISrc is always present.
        if !Self::has_query_param(&url, "WOPISrc") {
            url = Self::append_query_param(&url, "WOPISrc", &urlencoding::encode(wopi_src));
        }

        url
    }

    /// Get the editor action URL for a given file extension and exact action.
    ///
    /// Replaces `WOPI_SOURCE` placeholder with the provided `wopi_src` URL.
    pub async fn get_action_url(
        &self,
        extension: &str,
        action: &str,
        wopi_src: &str,
    ) -> Result<Option<String>, DomainError> {
        self.ensure_fresh().await?;

        let actions = self.actions.read().await;
        let ext_lower = extension.to_lowercase();

        if let Some(ext_actions) = actions.get(&ext_lower)
            && let Some(wopi_action) = ext_actions.iter().find(|a| a.name == action)
        {
            return Ok(Some(Self::build_action_url(wopi_action, wopi_src)));
        }

        Ok(None)
    }

    /// Resolve a requested WOPI action against the discovery document, with
    /// bidirectional fallback for view-only or edit-only editor entries.
    ///
    /// - `"convert"` matches strictly against `name == "convert"` (and `target_ext`
    ///   when supplied) with no fallback to `"view"` or `"edit"`.
    /// - `"edit"` falls back to `"view"` when the editor only supports viewing
    ///   (e.g. OnlyOffice `.doc`/`.ppt` or Collabora `.pdf`).
    /// - `"view"` falls back to `"edit"` when the editor only advertises `"edit"`
    ///   and enforces read-only via `CheckFileInfo.UserCanWrite = false`.
    pub async fn resolve_action_url(
        &self,
        extension: &str,
        requested_action: &str,
        target_ext: Option<&str>,
        wopi_src: &str,
    ) -> Result<Option<ResolvedWopiAction>, DomainError> {
        self.ensure_fresh().await?;

        let actions = self.actions.read().await;
        let ext_lower = extension.to_lowercase();
        let Some(ext_actions) = actions.get(&ext_lower) else {
            return Ok(None);
        };

        let matched = if requested_action == "convert" {
            let target_lower = target_ext.map(str::to_lowercase);
            ext_actions.iter().find(|a| {
                a.name == "convert"
                    && match target_lower.as_deref() {
                        Some(t) => a.targetext.as_deref() == Some(t),
                        None => true,
                    }
            })
        } else {
            let fallback_action = match requested_action {
                "edit" => Some("view"),
                "view" => Some("edit"),
                _ => None,
            };
            ext_actions
                .iter()
                .find(|a| a.name == requested_action)
                .or_else(|| {
                    fallback_action.and_then(|fb| ext_actions.iter().find(|a| a.name == fb))
                })
        };

        Ok(matched.map(|wopi_action| ResolvedWopiAction {
            action: wopi_action.name.clone(),
            target_ext: wopi_action.targetext.clone(),
            url: Self::build_action_url(wopi_action, wopi_src),
        }))
    }

    /// Return deduplicated conversion target extensions advertised for `extension`
    /// (in discovery order), e.g. `["docx"]` for `.doc` in OnlyOffice.
    pub async fn get_convert_targets(&self, extension: &str) -> Result<Vec<String>, DomainError> {
        self.ensure_fresh().await?;

        let actions = self.actions.read().await;
        let ext_lower = extension.to_lowercase();
        let Some(ext_actions) = actions.get(&ext_lower) else {
            return Ok(Vec::new());
        };

        let mut targets = Vec::new();
        for action in ext_actions {
            if action.name == "convert"
                && let Some(ref target) = action.targetext
                && !targets.contains(target)
            {
                targets.push(target.clone());
            }
        }

        Ok(targets)
    }

    /// Check if an extension is supported for a given action.
    pub async fn supports_action(
        &self,
        extension: &str,
        action: &str,
    ) -> Result<bool, DomainError> {
        self.ensure_fresh().await?;
        let actions = self.actions.read().await;
        let ext_lower = extension.to_lowercase();
        Ok(actions
            .get(&ext_lower)
            .is_some_and(|acts| acts.iter().any(|a| a.name == action)))
    }

    /// Get list of all supported extensions.
    pub async fn get_supported_extensions(&self) -> Result<Vec<String>, DomainError> {
        self.ensure_fresh().await?;
        let actions = self.actions.read().await;
        Ok(actions.keys().cloned().collect())
    }

    /// Parse the WOPI discovery XML into a map of extension -> actions.
    fn parse_discovery_xml(xml: &str) -> Result<HashMap<String, Vec<WopiAction>>, DomainError> {
        let mut reader = Reader::from_str(xml);
        let mut actions: HashMap<String, Vec<WopiAction>> = HashMap::new();

        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                    if e.name().as_ref() == b"action" =>
                {
                    let mut name = String::new();
                    let mut ext = String::new();
                    let mut targetext: Option<String> = None;
                    let mut urlsrc = String::new();

                    for attr in e.attributes().flatten() {
                        let value = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .map(|value| value.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).to_string());

                        match attr.key.as_ref() {
                            b"name" => name = value,
                            b"ext" => ext = value,
                            b"targetext" => {
                                targetext = (!value.is_empty()).then(|| value.to_lowercase());
                            }
                            b"urlsrc" => urlsrc = value,
                            _ => {}
                        }
                    }

                    if !ext.is_empty() && !urlsrc.is_empty() {
                        actions
                            .entry(ext.to_lowercase())
                            .or_default()
                            .push(WopiAction {
                                name,
                                ext: ext.to_lowercase(),
                                targetext,
                                urlsrc,
                            });
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(DomainError::new(
                        ErrorKind::InternalError,
                        "WopiDiscovery",
                        format!("Failed to parse discovery XML: {}", e),
                    ));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(actions)
    }

    /// Remove unused placeholder parameters from the URL.
    fn clean_placeholder_params(url: &str) -> String {
        let mut result = url.to_string();
        while let Some(start) = result.find('<') {
            if let Some(end) = result[start..].find('>') {
                result = format!("{}{}", &result[..start], &result[start + end + 1..]);
            } else {
                break;
            }
        }
        result = result
            .trim_end_matches('&')
            .trim_end_matches('?')
            .to_string();
        result
    }

    fn has_query_param(url: &str, key: &str) -> bool {
        if let Some((_, query)) = url.split_once('?') {
            for part in query.split('&') {
                let name = part.split('=').next().unwrap_or("");
                if name == key {
                    return true;
                }
            }
        }
        false
    }

    fn append_query_param(url: &str, key: &str, value: &str) -> String {
        let separator = if url.contains('?') {
            if url.ends_with('?') || url.ends_with('&') {
                ""
            } else {
                "&"
            }
        } else {
            "?"
        };
        format!("{}{}{}={}", url, separator, key, value)
    }
}

// Minimal inline URL encoding implementation (no external crate dependency).
// Matches the pattern used by oidc_service.rs in this codebase.
mod urlencoding {
    pub fn encode(input: &str) -> String {
        let mut result = String::with_capacity(input.len() * 3);
        for byte in input.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    result.push(byte as char);
                }
                _ => {
                    result.push('%');
                    result.push_str(&format!("{:02X}", byte));
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DISCOVERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<wopi-discovery>
  <net-zone name="external-https">
    <app name="Word">
      <action name="view" ext="docx" urlsrc="https://collabora/cool/word/view?WOPISrc=WOPI_SOURCE&amp;lang=UI_LLCC"/>
      <action name="edit" ext="docx" urlsrc="https://collabora/cool/word/edit?WOPISrc=WOPI_SOURCE&amp;lang=UI_LLCC"/>
    </app>
    <app name="Excel">
      <action name="edit" ext="xlsx" urlsrc="https://collabora/cool/calc/edit?WOPISrc=WOPI_SOURCE"/>
    </app>
    <app name="Impress">
      <action name="view" ext="pptx" urlsrc="https://collabora/cool/impress/view?WOPISrc=WOPI_SOURCE"/>
    </app>
  </net-zone>
</wopi-discovery>"#;

    #[test]
    fn test_parse_discovery_xml() {
        let actions =
            WopiDiscoveryService::parse_discovery_xml(SAMPLE_DISCOVERY).expect("Should parse");

        assert!(actions.contains_key("docx"));
        assert!(actions.contains_key("xlsx"));
        assert!(actions.contains_key("pptx"));

        let docx_actions = &actions["docx"];
        assert_eq!(docx_actions.len(), 2);
        assert!(docx_actions.iter().any(|a| a.name == "view"));
        assert!(docx_actions.iter().any(|a| a.name == "edit"));
        assert!(
            docx_actions
                .iter()
                .all(|action| !action.urlsrc.contains("&amp;"))
        );
    }

    #[tokio::test]
    async fn test_get_action_url_decodes_xml_escaped_ampersands() {
        let service = WopiDiscoveryService::new("http://example.test/discovery".to_string(), 60);
        *service.actions.write().await =
            WopiDiscoveryService::parse_discovery_xml(SAMPLE_DISCOVERY).expect("Should parse");
        *service.last_fetched.write().await = Some(Instant::now());

        let url = service
            .get_action_url("docx", "edit", "https://oxicloud.test/wopi/files/abc")
            .await
            .expect("Should build URL")
            .expect("Action URL should exist");

        assert!(url.contains("lang=en-US"));
        assert!(!url.contains("&amp;"));
    }

    #[test]
    fn test_clean_placeholder_params() {
        let url =
            "https://example.com/edit?WOPISrc=http%3A%2F%2Flocalhost&<lang=UI_LLCC&><ui=UI_LLCC&>";
        let cleaned = WopiDiscoveryService::clean_placeholder_params(url);
        assert!(!cleaned.contains('<'));
        assert!(!cleaned.contains('>'));
        assert!(cleaned.contains("WOPISrc="));
    }

    #[test]
    fn test_append_wopisrc_when_missing() {
        let base = "http://127.0.0.1:9980/browser/hash/cool.html?";
        assert!(!WopiDiscoveryService::has_query_param(base, "WOPISrc"));

        let appended = WopiDiscoveryService::append_query_param(
            base,
            "WOPISrc",
            "http%3A%2F%2F127.0.0.1%3A8086%2Fwopi%2Ffiles%2Fabc",
        );

        assert!(WopiDiscoveryService::has_query_param(&appended, "WOPISrc"));
        assert!(appended.contains("WOPISrc=http%3A%2F%2F127.0.0.1%3A8086%2Fwopi%2Ffiles%2Fabc"));
    }

    const ONLYOFFICE_DOC_DISCOVERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<wopi-discovery>
  <net-zone name="external-http">
    <app name="Word">
      <action name="view" ext="doc" default="true" urlsrc="https://onlyoffice.test/hosting/wopi/word/view?&lt;rs=DC_LLCC&amp;&gt;&lt;ui=UI_LLCC&amp;&gt;&lt;wopisrc=WOPI_SOURCE&amp;&gt;&amp;"/>
      <action name="convert" ext="doc" targetext="docx" requires="update" urlsrc="https://onlyoffice.test/hosting/wopi/convert-and-edit/doc/docx?&lt;ui=UI_LLCC&amp;&gt;&lt;wopisrc=WOPI_SOURCE&amp;&gt;&amp;"/>
      <action name="view" ext="docx" urlsrc="https://onlyoffice.test/hosting/wopi/word/view?&lt;wopisrc=WOPI_SOURCE&amp;&gt;"/>
      <action name="edit" ext="docx" default="true" urlsrc="https://onlyoffice.test/hosting/wopi/word/edit?&lt;wopisrc=WOPI_SOURCE&amp;&gt;"/>
    </app>
    <app name="Excel">
      <action name="edit" ext="xlsx" default="true" urlsrc="https://collabora.test/cool/calc/edit?WOPISrc=WOPI_SOURCE"/>
    </app>
  </net-zone>
</wopi-discovery>"#;

    const MULTI_TARGET_DISCOVERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<wopi-discovery>
  <net-zone name="external-http">
    <app name="Word">
      <action name="view" ext="doc" default="true" urlsrc="https://editor.test/word/view?WOPISrc=WOPI_SOURCE"/>
      <action name="convert" ext="doc" targetext="docx" urlsrc="https://editor.test/convert/doc/docx?WOPISrc=WOPI_SOURCE"/>
      <action name="convert" ext="doc" targetext="odt" urlsrc="https://editor.test/convert/doc/odt?WOPISrc=WOPI_SOURCE"/>
      <action name="convert" ext="doc" targetext="DOCX" urlsrc="https://editor.test/convert/doc/docx-dup?WOPISrc=WOPI_SOURCE"/>
    </app>
  </net-zone>
</wopi-discovery>"#;

    async fn seeded_service(xml: &str) -> WopiDiscoveryService {
        let service = WopiDiscoveryService::new("http://example.test/discovery".to_string(), 60);
        *service.actions.write().await =
            WopiDiscoveryService::parse_discovery_xml(xml).expect("Should parse");
        *service.last_fetched.write().await = Some(Instant::now());
        service
    }

    // Regression pin: OnlyOffice advertises `view` + `convert` (no `edit`) for
    // legacy `.doc`; requesting `edit` used to return None -> HTTP 422 on open.
    #[tokio::test]
    async fn test_resolve_action_url_onlyoffice_doc_falls_back_to_view() {
        let service = seeded_service(ONLYOFFICE_DOC_DISCOVERY).await;

        assert_eq!(
            service
                .get_action_url("doc", "edit", "https://oxicloud.test/wopi/files/abc")
                .await
                .expect("exact lookup ok"),
            None
        );

        let resolved = service
            .resolve_action_url("DOC", "edit", None, "https://oxicloud.test/wopi/files/abc")
            .await
            .expect("resolve should succeed")
            .expect("should fall back to view");

        assert_eq!(resolved.action, "view");
        assert_eq!(resolved.target_ext, None);
        assert_eq!(
            resolved.url,
            "https://onlyoffice.test/hosting/wopi/word/view?WOPISrc=https%3A%2F%2Foxicloud.test%2Fwopi%2Ffiles%2Fabc"
        );
    }

    #[tokio::test]
    async fn test_resolve_action_url_convert_and_targets() {
        let service = seeded_service(ONLYOFFICE_DOC_DISCOVERY).await;

        let targets = service
            .get_convert_targets("doc")
            .await
            .expect("targets lookup ok");
        assert_eq!(targets, vec!["docx"]);

        let resolved = service
            .resolve_action_url(
                "doc",
                "convert",
                Some("docx"),
                "https://oxicloud.test/wopi/files/abc",
            )
            .await
            .expect("resolve ok")
            .expect("convert action should exist");

        assert_eq!(resolved.action, "convert");
        assert_eq!(resolved.target_ext.as_deref(), Some("docx"));
        assert!(resolved.url.starts_with(
            "https://onlyoffice.test/hosting/wopi/convert-and-edit/doc/docx?WOPISrc="
        ));

        // Explicit convert request must not silently fall back to view/edit
        // when the requested target format is not advertised.
        assert_eq!(
            service
                .resolve_action_url(
                    "doc",
                    "convert",
                    Some("odt"),
                    "https://oxicloud.test/wopi/files/abc",
                )
                .await
                .expect("resolve ok"),
            None
        );
    }

    #[tokio::test]
    async fn test_resolve_multi_target_convert_preserves_order_and_dedups() {
        let service = seeded_service(MULTI_TARGET_DISCOVERY).await;

        let targets = service
            .get_convert_targets("doc")
            .await
            .expect("targets lookup ok");
        assert_eq!(targets, vec!["docx", "odt"]);

        let odt = service
            .resolve_action_url(
                "doc",
                "convert",
                Some("ODT"),
                "https://oxicloud.test/wopi/files/abc",
            )
            .await
            .expect("resolve ok")
            .expect("odt target should exist");
        assert_eq!(odt.action, "convert");
        assert_eq!(odt.target_ext.as_deref(), Some("odt"));
        assert!(odt.url.contains("/convert/doc/odt?"));

        assert_eq!(
            service
                .resolve_action_url(
                    "doc",
                    "convert",
                    Some("pdf"),
                    "https://oxicloud.test/wopi/files/abc",
                )
                .await
                .expect("resolve ok"),
            None
        );
    }

    // Regression pin: Collabora advertises only `edit` for `.xlsx`; requesting
    // `view` (e.g. from host_page for a Viewer) must fall back to `edit` instead
    // of failing with 500, while the handler keeps `UserCanWrite = false`.
    #[tokio::test]
    async fn test_resolve_action_url_collabora_edit_only_falls_back_to_edit() {
        let service = seeded_service(ONLYOFFICE_DOC_DISCOVERY).await;

        let resolved = service
            .resolve_action_url("xlsx", "view", None, "https://oxicloud.test/wopi/files/abc")
            .await
            .expect("resolve ok")
            .expect("should fall back to edit");
        assert_eq!(resolved.action, "edit");
        assert!(resolved.url.contains("/calc/edit?"));

        // Exact matches still win when both actions exist.
        let docx_edit = service
            .resolve_action_url("docx", "edit", None, "https://oxicloud.test/wopi/files/abc")
            .await
            .expect("resolve ok")
            .expect("docx edit");
        assert_eq!(docx_edit.action, "edit");
        let docx_view = service
            .resolve_action_url("docx", "view", None, "https://oxicloud.test/wopi/files/abc")
            .await
            .expect("resolve ok")
            .expect("docx view");
        assert_eq!(docx_view.action, "view");

        // Unsupported extensions and unknown actions still return None -> 422.
        assert_eq!(
            service
                .resolve_action_url("zip", "edit", None, "https://oxicloud.test/wopi/files/abc")
                .await
                .expect("resolve ok"),
            None
        );
        assert_eq!(
            service
                .resolve_action_url(
                    "docx",
                    "bogus",
                    None,
                    "https://oxicloud.test/wopi/files/abc"
                )
                .await
                .expect("resolve ok"),
            None
        );
    }
}
