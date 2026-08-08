#[cfg(test)]
mod tests {
    use crate::application::adapters::carddav_adapter::{
        CardDavAdapter, CardDavReportType, contact_to_vcard,
    };
    use crate::application::adapters::webdav_adapter::{
        PropFindRequest, PropFindType, QualifiedName,
    };
    use crate::application::dtos::address_book_dto::AddressBookDto;
    use crate::application::dtos::contact_dto::{AddressDto, ContactDto, EmailDto, PhoneDto};
    use chrono::{NaiveDate, TimeZone, Utc};
    use std::io::Cursor;

    fn sample_address_book() -> AddressBookDto {
        AddressBookDto {
            id: "ab-001".to_string(),
            name: "My Contacts".to_string(),
            owner_id: "user-001".to_string(),
            description: Some("Personal address book".to_string()),
            color: Some("#00FF00".to_string()),
            is_public: false,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
        }
    }

    fn sample_contact() -> ContactDto {
        ContactDto {
            id: "contact-001".to_string(),
            address_book_id: "ab-001".to_string(),
            uid: "uid-contact-001@oxicloud".to_string(),
            full_name: Some("John Doe".to_string()),
            first_name: Some("John".to_string()),
            last_name: Some("Doe".to_string()),
            nickname: Some("Johnny".to_string()),
            email: vec![
                EmailDto {
                    email: "john@example.com".to_string(),
                    r#type: "work".to_string(),
                    is_primary: true,
                },
                EmailDto {
                    email: "john.doe@personal.com".to_string(),
                    r#type: "home".to_string(),
                    is_primary: false,
                },
            ],
            phone: vec![PhoneDto {
                number: "+1-555-0100".to_string(),
                r#type: "cell".to_string(),
                is_primary: true,
            }],
            address: vec![AddressDto {
                street: Some("123 Main St".to_string()),
                city: Some("Springfield".to_string()),
                state: Some("IL".to_string()),
                postal_code: Some("62701".to_string()),
                country: Some("US".to_string()),
                r#type: "home".to_string(),
                is_primary: true,
            }],
            organization: Some("Acme Corp".to_string()),
            title: Some("Software Engineer".to_string()),
            notes: Some("Met at conference".to_string()),
            photo_url: None,
            birthday: Some(NaiveDate::from_ymd_opt(1990, 5, 15).unwrap()),
            anniversary: None,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2025, 3, 10, 8, 30, 0).unwrap(),
            etag: "etag-abc123".to_string(),
        }
    }

    fn sample_contact_minimal() -> ContactDto {
        ContactDto {
            id: "contact-002".to_string(),
            address_book_id: "ab-001".to_string(),
            uid: "uid-contact-002@oxicloud".to_string(),
            full_name: Some("Jane Smith".to_string()),
            first_name: None,
            last_name: None,
            nickname: None,
            email: vec![],
            phone: vec![],
            address: vec![],
            organization: None,
            title: None,
            notes: None,
            photo_url: None,
            birthday: None,
            anniversary: None,
            created_at: Utc.with_ymd_and_hms(2025, 2, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2025, 2, 1, 0, 0, 0).unwrap(),
            etag: "etag-def456".to_string(),
        }
    }

    // ========================
    // vCard generation tests
    // ========================

    #[test]
    fn test_contact_to_vcard_full() {
        let contact = sample_contact();
        let vcard = contact_to_vcard(&contact);

        assert!(
            vcard.starts_with("BEGIN:VCARD"),
            "vCard should start with BEGIN:VCARD"
        );
        assert!(vcard.contains("VERSION:3.0"), "Should be vCard 3.0");
        assert!(vcard.contains("FN:John Doe"), "Should contain full name");
        assert!(
            vcard.contains("N:Doe;John"),
            "Should contain structured name"
        );
        assert!(vcard.contains("NICKNAME:Johnny"), "Should contain nickname");
        assert!(vcard.contains("john@example.com"), "Should contain email");
        assert!(vcard.contains("+1-555-0100"), "Should contain phone number");
        assert!(
            vcard.contains("ORG:Acme Corp"),
            "Should contain organization"
        );
        assert!(
            vcard.contains("TITLE:Software Engineer"),
            "Should contain title"
        );
        assert!(
            vcard.contains("NOTE:Met at conference"),
            "Should contain notes"
        );
        assert!(vcard.contains("BDAY:1990-05-15"), "Should contain birthday");
        assert!(
            vcard.contains("UID:uid-contact-001@oxicloud"),
            "Should contain UID"
        );
        assert!(
            vcard.ends_with("END:VCARD\r\n") || vcard.trim_end().ends_with("END:VCARD"),
            "vCard should end with END:VCARD"
        );
    }

    #[test]
    fn test_contact_to_vcard_minimal() {
        let contact = sample_contact_minimal();
        let vcard = contact_to_vcard(&contact);

        assert!(vcard.contains("BEGIN:VCARD"), "Should start correctly");
        assert!(vcard.contains("VERSION:3.0"), "Should be vCard 3.0");
        assert!(vcard.contains("FN:Jane Smith"), "Should have full name");
        assert!(
            vcard.contains("UID:uid-contact-002@oxicloud"),
            "Should have UID"
        );
        assert!(vcard.contains("END:VCARD"), "Should end correctly");
        // Should NOT contain optional fields
        assert!(!vcard.contains("NICKNAME:"), "Should not have nickname");
        assert!(!vcard.contains("ORG:"), "Should not have org");
        assert!(!vcard.contains("TITLE:"), "Should not have title");
        assert!(!vcard.contains("BDAY:"), "Should not have birthday");
    }

    // ========================
    // MKADDRESSBOOK parsing tests
    // ========================

    #[test]
    fn test_parse_mkaddressbook_full() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <D:mkcol xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
            <D:set>
                <D:prop>
                    <D:displayname>Work Contacts</D:displayname>
                    <CR:addressbook-description>Colleagues and clients</CR:addressbook-description>
                </D:prop>
            </D:set>
        </D:mkcol>"#;

        let result = CardDavAdapter::parse_mkaddressbook(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse mkaddressbook: {:?}",
            result.err()
        );
        let (name, desc, color) = result.unwrap();
        assert_eq!(name, "Work Contacts");
        assert_eq!(desc, Some("Colleagues and clients".to_string()));
        assert!(color.is_none());
    }

    #[test]
    fn test_parse_mkaddressbook_name_only() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <D:mkcol xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
            <D:set>
                <D:prop>
                    <D:displayname>Simple Book</D:displayname>
                </D:prop>
            </D:set>
        </D:mkcol>"#;

        let result = CardDavAdapter::parse_mkaddressbook(Cursor::new(xml));
        assert!(result.is_ok());
        let (name, desc, color) = result.unwrap();
        assert_eq!(name, "Simple Book");
        assert!(desc.is_none());
        assert!(color.is_none());
    }

    // ========================
    // REPORT parsing tests
    // ========================

    #[test]
    fn test_parse_addressbook_query_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <CR:addressbook-query xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
            <D:prop>
                <D:getetag/>
                <CR:address-data/>
            </D:prop>
        </CR:addressbook-query>"#;

        let result = CardDavAdapter::parse_report(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse addressbook-query: {:?}",
            result.err()
        );

        match result.unwrap() {
            CardDavReportType::AddressbookQuery { props } => {
                assert!(!props.is_empty(), "Props should not be empty");
            }
            other => panic!("Expected AddressbookQuery, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_addressbook_multiget_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <CR:addressbook-multiget xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
            <D:prop>
                <D:getetag/>
                <CR:address-data/>
            </D:prop>
            <D:href>/carddav/ab-001/contact-001.vcf</D:href>
            <D:href>/carddav/ab-001/contact-002.vcf</D:href>
            <D:href>/carddav/ab-001/contact-003.vcf</D:href>
        </CR:addressbook-multiget>"#;

        let result = CardDavAdapter::parse_report(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse multiget: {:?}",
            result.err()
        );

        match result.unwrap() {
            CardDavReportType::AddressbookMultiget { hrefs, props } => {
                assert_eq!(hrefs.len(), 3, "Should have 3 hrefs");
                assert_eq!(hrefs[0], "/carddav/ab-001/contact-001.vcf");
                assert_eq!(hrefs[2], "/carddav/ab-001/contact-003.vcf");
                assert!(!props.is_empty());
            }
            other => panic!("Expected AddressbookMultiget, got {:?}", other),
        }
    }

    // ========================
    // PROPFIND response tests
    // ========================

    #[test]
    fn test_generate_addressbooks_propfind_response() {
        let addressbooks = vec![sample_address_book()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_addressbooks_propfind_response(
            &mut output,
            &addressbooks,
            &request,
            "/carddav",
        );

        assert!(
            result.is_ok(),
            "Failed to generate propfind response: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("multistatus"),
            "Should contain multistatus"
        );
        assert!(
            xml_str.contains("My Contacts"),
            "Should contain address book name"
        );
        assert!(
            xml_str.contains("ab-001"),
            "Should contain address book ID in href"
        );
    }

    #[test]
    fn test_root_propfind_advertises_principal_and_home_set() {
        // Regression for #480: without these discovery properties DAVx5 / Apple
        // Contacts never locate the user's address books.
        let books = vec![sample_address_book()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        CardDavAdapter::generate_root_propfind_response(
            &mut output,
            &books,
            &request,
            "/carddav/",
            "testuser",
        )
        .expect("root propfind");

        let xml = String::from_utf8(output).expect("utf8");
        assert!(
            xml.contains("/carddav/principals/testuser/"),
            "Root must expose current-user-principal href, got: {xml}"
        );
        assert!(
            xml.contains("/carddav/testuser/"),
            "Root must expose addressbook-home-set href, got: {xml}"
        );
        // Depth 1 also enumerates the books.
        assert!(xml.contains("ab-001"), "Should list address book");
    }

    #[test]
    fn test_root_propfind_prop_request_returns_populated_discovery() {
        // A DAVx5-style targeted request for the two discovery properties.
        let request = PropFindRequest {
            prop_find_type: PropFindType::Prop(vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "current-user-principal".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:carddav".to_string(),
                    name: "addressbook-home-set".to_string(),
                },
            ]),
        };

        let mut output = Vec::new();
        CardDavAdapter::generate_root_propfind_response(
            &mut output,
            &[],
            &request,
            "/carddav/",
            "testuser",
        )
        .expect("root propfind");

        let xml = String::from_utf8(output).expect("utf8");
        assert!(xml.contains("/carddav/principals/testuser/"));
        assert!(xml.contains("/carddav/testuser/"));
        // Properties must be populated, not empty self-closing placeholders.
        assert!(!xml.contains("<D:current-user-principal/>"));
        assert!(!xml.contains("<CR:addressbook-home-set/>"));
    }

    #[test]
    fn test_principal_propfind_returns_home_set() {
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        CardDavAdapter::generate_principal_propfind_response(&mut output, &request, "testuser")
            .expect("principal propfind");

        let xml = String::from_utf8(output).expect("utf8");
        assert!(
            xml.contains("/carddav/principals/testuser/"),
            "Principal href should be present"
        );
        assert!(
            xml.contains("/carddav/testuser/"),
            "addressbook-home-set should be present"
        );
        assert!(
            xml.contains("D:principal"),
            "resourcetype should include principal"
        );
    }

    #[test]
    fn test_generate_addressbook_collection_propfind_depth_0() {
        let addressbook = sample_address_book();
        let contacts = vec![sample_contact()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_addressbook_collection_propfind(
            &mut output,
            &addressbook,
            &contacts,
            &request,
            "/carddav/ab-001",
            "0",
        );

        assert!(
            result.is_ok(),
            "Failed to generate depth-0 propfind: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("multistatus"),
            "Should contain multistatus"
        );
        assert!(
            xml_str.contains("My Contacts"),
            "Should contain address book name"
        );
    }

    #[test]
    fn test_generate_addressbook_collection_propfind_depth_1() {
        let addressbook = sample_address_book();
        let contacts = vec![sample_contact(), sample_contact_minimal()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_addressbook_collection_propfind(
            &mut output,
            &addressbook,
            &contacts,
            &request,
            "/carddav/ab-001",
            "1",
        );

        assert!(
            result.is_ok(),
            "Failed to generate depth-1 propfind: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("multistatus"),
            "Should contain multistatus"
        );
        assert!(
            xml_str.contains("My Contacts"),
            "Should contain address book name"
        );
        // Depth 1 should include contact resources
        assert!(
            xml_str.contains("contact-001"),
            "Should include contact-001"
        );
        assert!(
            xml_str.contains("contact-002"),
            "Should include contact-002"
        );
    }

    // ========================
    // Contacts response tests
    // ========================

    #[test]
    fn test_generate_contacts_response() {
        let contacts = vec![sample_contact()];
        let report = CardDavReportType::AddressbookQuery {
            props: vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "getetag".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:carddav".to_string(),
                    name: "address-data".to_string(),
                },
            ],
        };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_contacts_response(
            &mut output,
            &contacts,
            &report,
            "/carddav/ab-001",
            &[],
            None,
        );

        assert!(
            result.is_ok(),
            "Failed to generate contacts response: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("multistatus"),
            "Should contain multistatus"
        );
        assert!(xml_str.contains("contact-001"), "Should reference contact");
        assert!(xml_str.contains("etag-abc123"), "Should contain etag");
    }

    #[test]
    fn test_generate_empty_contacts_response() {
        let contacts: Vec<ContactDto> = vec![];
        let report = CardDavReportType::AddressbookQuery { props: vec![] };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_contacts_response(
            &mut output,
            &contacts,
            &report,
            "/carddav/ab-001",
            &[],
            None,
        );

        assert!(
            result.is_ok(),
            "Empty contacts should produce valid response"
        );
        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(xml_str.contains("multistatus"), "Should have multistatus");
    }

    // ========================
    // Multiple address books test
    // ========================

    #[test]
    fn test_generate_multiple_addressbooks() {
        let mut ab2 = sample_address_book();
        ab2.id = "ab-002".to_string();
        ab2.name = "Work Contacts".to_string();

        let addressbooks = vec![sample_address_book(), ab2];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CardDavAdapter::generate_addressbooks_propfind_response(
            &mut output,
            &addressbooks,
            &request,
            "/carddav/",
        );

        assert!(result.is_ok());
        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("My Contacts"),
            "Should contain first address book"
        );
        assert!(
            xml_str.contains("Work Contacts"),
            "Should contain second address book"
        );
        assert!(xml_str.contains("ab-001"), "Should have first ID");
        assert!(xml_str.contains("ab-002"), "Should have second ID");
    }

    // ========================
    // vCard edge cases
    // ========================

    #[test]
    fn test_contact_to_vcard_with_multiple_emails() {
        let contact = sample_contact();
        let vcard = contact_to_vcard(&contact);

        // Should contain both emails
        assert!(vcard.contains("john@example.com"), "Should have work email");
        assert!(
            vcard.contains("john.doe@personal.com"),
            "Should have personal email"
        );
    }

    #[test]
    fn test_contact_to_vcard_address_formatting() {
        let contact = sample_contact();
        let vcard = contact_to_vcard(&contact);

        // vCard ADR format: ;;street;city;state;postal;country
        assert!(vcard.contains("123 Main St"), "Should have street");
        assert!(vcard.contains("Springfield"), "Should have city");
        assert!(vcard.contains("62701"), "Should have postal code");
    }
}
