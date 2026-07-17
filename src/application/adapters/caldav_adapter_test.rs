#[cfg(test)]
mod tests {
    use crate::application::adapters::caldav_adapter::{CalDavAdapter, CalDavReportType};
    use crate::application::adapters::webdav_adapter::{
        PropFindRequest, PropFindType, QualifiedName,
    };
    use crate::application::dtos::calendar_dto::{CalendarDto, CalendarEventDto};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use std::io::Cursor;

    fn sample_calendar() -> CalendarDto {
        CalendarDto {
            id: "cal-001".to_string(),
            name: "Personal".to_string(),
            owner_id: "user-001".to_string(),
            description: Some("My personal calendar".to_string()),
            color: Some("#FF0000".to_string()),
            is_public: false,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
            custom_properties: HashMap::new(),
        }
    }

    fn sample_event() -> CalendarEventDto {
        CalendarEventDto {
            id: "evt-001".to_string(),
            calendar_id: "cal-001".to_string(),
            summary: "Team Meeting".to_string(),
            description: Some("Weekly team sync".to_string()),
            location: Some("Conference Room A".to_string()),
            start_time: Utc.with_ymd_and_hms(2025, 6, 15, 10, 0, 0).unwrap(),
            end_time: Utc.with_ymd_and_hms(2025, 6, 15, 11, 0, 0).unwrap(),
            all_day: false,
            rrule: None,
            ical_uid: "uid-evt-001@oxicloud".to_string(),
            recurrence_id: None,
            // Post-phase-4 the emitter serves stored ical_data
            // verbatim (folded per UID) instead of regenerating
            // from DTO fields. The fixture must therefore carry
            // a valid single-VEVENT VCALENDAR body — this is what
            // create_event_from_ical stores per row.
            ical_data: "BEGIN:VCALENDAR\r\n\
                VERSION:2.0\r\n\
                PRODID:-//OxiCloud test//EN\r\n\
                BEGIN:VEVENT\r\n\
                UID:uid-evt-001@oxicloud\r\n\
                DTSTAMP:20250601T090000Z\r\n\
                DTSTART:20250615T100000Z\r\n\
                DTEND:20250615T110000Z\r\n\
                SUMMARY:Team Meeting\r\n\
                DESCRIPTION:Weekly team sync\r\n\
                LOCATION:Conference Room A\r\n\
                END:VEVENT\r\n\
                END:VCALENDAR\r\n"
                .to_string(),
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    // ========================
    // MKCALENDAR parsing tests
    // ========================

    #[test]
    fn test_parse_mkcalendar_full() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:set>
                <D:prop>
                    <D:displayname>Work Calendar</D:displayname>
                    <C:calendar-description>Work related events</C:calendar-description>
                    <A:calendar-color xmlns:A="http://apple.com/ns/ical/">#0000FF</A:calendar-color>
                </D:prop>
            </D:set>
        </C:mkcalendar>"#;

        let result = CalDavAdapter::parse_mkcalendar(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse MKCALENDAR: {:?}",
            result.err()
        );
        let (name, desc, color) = result.unwrap();
        assert_eq!(name, "Work Calendar");
        assert_eq!(desc, Some("Work related events".to_string()));
        assert_eq!(color, Some("#0000FF".to_string()));
    }

    #[test]
    fn test_parse_mkcalendar_name_only() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:set>
                <D:prop>
                    <D:displayname>Minimal Calendar</D:displayname>
                </D:prop>
            </D:set>
        </C:mkcalendar>"#;

        let result = CalDavAdapter::parse_mkcalendar(Cursor::new(xml));
        assert!(result.is_ok());
        let (name, desc, color) = result.unwrap();
        assert_eq!(name, "Minimal Calendar");
        assert!(desc.is_none());
        assert!(color.is_none());
    }

    // ========================
    // REPORT parsing tests
    // ========================

    #[test]
    fn test_parse_calendar_query_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:prop>
                <D:getetag/>
                <C:calendar-data/>
            </D:prop>
            <C:filter>
                <C:comp-filter name="VCALENDAR">
                    <C:comp-filter name="VEVENT">
                        <C:time-range start="2025-06-01T00:00:00Z" end="2025-07-01T00:00:00Z"/>
                    </C:comp-filter>
                </C:comp-filter>
            </C:filter>
        </C:calendar-query>"#;

        let result = CalDavAdapter::parse_report(Cursor::new(xml));
        assert!(result.is_ok(), "Failed to parse report: {:?}", result.err());

        match result.unwrap() {
            CalDavReportType::CalendarQuery { time_range, props } => {
                assert!(time_range.is_some(), "Time range should be parsed");
                let (start, end) = time_range.unwrap();
                assert_eq!(start, Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap());
                assert_eq!(end, Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap());
                assert!(!props.is_empty(), "Props should not be empty");
            }
            other => panic!("Expected CalendarQuery, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_calendar_multiget_report() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:prop>
                <D:getetag/>
                <C:calendar-data/>
            </D:prop>
            <D:href>/caldav/cal-001/evt-001.ics</D:href>
            <D:href>/caldav/cal-001/evt-002.ics</D:href>
        </C:calendar-multiget>"#;

        let result = CalDavAdapter::parse_report(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse multiget: {:?}",
            result.err()
        );

        match result.unwrap() {
            CalDavReportType::CalendarMultiget { hrefs, props } => {
                assert_eq!(hrefs.len(), 2);
                assert_eq!(hrefs[0], "/caldav/cal-001/evt-001.ics");
                assert_eq!(hrefs[1], "/caldav/cal-001/evt-002.ics");
                assert!(!props.is_empty());
            }
            other => panic!("Expected CalendarMultiget, got {:?}", other),
        }
    }

    // ========================
    // PROPFIND response tests
    // ========================

    #[test]
    fn test_generate_calendars_propfind_response() {
        let calendars = vec![sample_calendar()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendars_propfind_response(
            &mut output,
            &calendars,
            &request,
            "/caldav/",
            "user-001",
        );

        assert!(
            result.is_ok(),
            "Failed to generate propfind response: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8 in response");
        assert!(
            xml_str.contains("multistatus"),
            "Response should contain multistatus element"
        );
        assert!(
            xml_str.contains("Personal"),
            "Response should contain calendar name"
        );
        assert!(
            xml_str.contains("cal-001"),
            "Response should contain calendar ID in href"
        );
    }

    #[test]
    fn test_generate_calendar_collection_propfind_depth_0() {
        let calendar = sample_calendar();
        let events = vec![sample_event()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendar_collection_propfind(
            &mut output,
            &calendar,
            &events,
            &request,
            "/caldav/cal-001",
            "0",
            "user-001",
        );

        assert!(
            result.is_ok(),
            "Failed to generate collection propfind: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(xml_str.contains("multistatus"), "Should have multistatus");
        assert!(xml_str.contains("Personal"), "Should have calendar name");
        // Depth 0 should NOT include individual event resources
    }

    #[test]
    fn test_generate_calendar_collection_propfind_depth_1() {
        let calendar = sample_calendar();
        let events = vec![sample_event()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendar_collection_propfind(
            &mut output,
            &calendar,
            &events,
            &request,
            "/caldav/cal-001",
            "1",
            "user-001",
        );

        assert!(
            result.is_ok(),
            "Failed to generate depth-1 propfind: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(xml_str.contains("multistatus"), "Should have multistatus");
        assert!(xml_str.contains("Personal"), "Should have calendar name");
        // Depth 1 should include event resources
        assert!(
            xml_str.contains("evt-001"),
            "Depth 1 should include event resources"
        );
    }

    #[test]
    fn test_owner_gets_write_privilege_but_non_owner_is_read_only() {
        // Regression for #480: the privilege gate previously compared owner_id
        // against the literal "current_user_id", so <D:write/> was never emitted
        // and every CalDAV client mounted calendars read-only.
        let calendar = sample_calendar(); // owner_id = "user-001"
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        // Owner → read + write.
        let mut owner_out = Vec::new();
        CalDavAdapter::generate_calendar_collection_propfind(
            &mut owner_out,
            &calendar,
            &[],
            &request,
            "/caldav/cal-001/",
            "0",
            "user-001",
        )
        .expect("owner propfind");
        let owner_xml = String::from_utf8(owner_out).expect("utf8");
        assert!(
            owner_xml.contains("D:write"),
            "Owner must be granted <D:write/>, got: {owner_xml}"
        );

        // A different caller (e.g. a read-only share) → read only, never write.
        let mut other_out = Vec::new();
        CalDavAdapter::generate_calendar_collection_propfind(
            &mut other_out,
            &calendar,
            &[],
            &request,
            "/caldav/cal-001/",
            "0",
            "a-different-user",
        )
        .expect("non-owner propfind");
        let other_xml = String::from_utf8(other_out).expect("utf8");
        assert!(other_xml.contains("D:read"), "Non-owner keeps <D:read/>");
        assert!(
            !other_xml.contains("D:write"),
            "Non-owner must NOT get <D:write/>, got: {other_xml}"
        );
    }

    // ========================
    // Calendar events response tests
    // ========================

    #[test]
    fn test_generate_calendar_events_response() {
        let events = vec![sample_event()];
        let report = CalDavReportType::CalendarQuery {
            time_range: None,
            props: vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "getetag".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:caldav".to_string(),
                    name: "calendar-data".to_string(),
                },
            ],
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendar_events_response(
            &mut output,
            &events,
            &report,
            "/caldav/cal-001",
            &[],
            None,
        );

        assert!(
            result.is_ok(),
            "Failed to generate events response: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(xml_str.contains("multistatus"), "Should have multistatus");
        assert!(xml_str.contains("evt-001"), "Should reference event ID");
        assert!(
            xml_str.contains("BEGIN:VCALENDAR"),
            "Should contain iCal data"
        );
        assert!(
            xml_str.contains("VEVENT"),
            "Should contain VEVENT component"
        );
        assert!(
            xml_str.contains("Team Meeting"),
            "Should contain event summary"
        );
    }

    #[test]
    fn test_generate_empty_events_response() {
        let events: Vec<CalendarEventDto> = vec![];
        let report = CalDavReportType::CalendarQuery {
            time_range: None,
            props: vec![],
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendar_events_response(
            &mut output,
            &events,
            &report,
            "/caldav/cal-001",
            &[],
            None,
        );

        assert!(
            result.is_ok(),
            "Empty events should still produce valid response"
        );
        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");
        assert!(
            xml_str.contains("multistatus"),
            "Should have multistatus even for empty"
        );
    }

    // ==============================================
    // Namespace resolution tests (issue #153 fixes)
    // ==============================================

    #[test]
    fn test_propfind_namespace_resolution_dav_and_caldav() {
        use crate::application::adapters::webdav_adapter::WebDavAdapter;

        // Simulates a real CalDAV client PROPFIND request with namespace prefixes
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:current-user-principal/>
    <C:calendar-home-set/>
    <D:resourcetype/>
  </D:prop>
</D:propfind>"#;

        let result = WebDavAdapter::parse_propfind(Cursor::new(xml));
        assert!(
            result.is_ok(),
            "Failed to parse PROPFIND: {:?}",
            result.err()
        );

        let request = result.unwrap();
        match request.prop_find_type {
            PropFindType::Prop(ref props) => {
                assert_eq!(props.len(), 3);

                // Verify namespace URIs are resolved, not prefix names
                assert_eq!(props[0].namespace, "DAV:");
                assert_eq!(props[0].name, "current-user-principal");

                assert_eq!(props[1].namespace, "urn:ietf:params:xml:ns:caldav");
                assert_eq!(props[1].name, "calendar-home-set");

                assert_eq!(props[2].namespace, "DAV:");
                assert_eq!(props[2].name, "resourcetype");
            }
            other => panic!("Expected Prop, got {:?}", other),
        }
    }

    #[test]
    fn test_propfind_namespace_resolution_with_custom_prefixes() {
        use crate::application::adapters::webdav_adapter::WebDavAdapter;

        // Some clients use different prefix names
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<A:propfind xmlns:A="DAV:" xmlns:B="urn:ietf:params:xml:ns:caldav">
  <A:prop>
    <A:current-user-principal/>
    <B:calendar-home-set/>
  </A:prop>
</A:propfind>"#;

        let result = WebDavAdapter::parse_propfind(Cursor::new(xml));
        assert!(result.is_ok());

        let request = result.unwrap();
        match request.prop_find_type {
            PropFindType::Prop(ref props) => {
                assert_eq!(props.len(), 2);
                assert_eq!(props[0].namespace, "DAV:");
                assert_eq!(props[0].name, "current-user-principal");
                assert_eq!(props[1].namespace, "urn:ietf:params:xml:ns:caldav");
                assert_eq!(props[1].name, "calendar-home-set");
            }
            other => panic!("Expected Prop, got {:?}", other),
        }
    }

    #[test]
    fn test_root_propfind_response_has_discovery_properties() {
        // Depth 0: only root entry, no calendars (simulates initial discovery)
        let calendars: Vec<CalendarDto> = vec![];
        let request = PropFindRequest {
            prop_find_type: PropFindType::Prop(vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "current-user-principal".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:caldav".to_string(),
                    name: "calendar-home-set".to_string(),
                },
            ]),
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_root_propfind_response(
            &mut output,
            &calendars,
            &request,
            "/caldav/",
            "testuser",
            "user-001",
        );
        assert!(
            result.is_ok(),
            "Failed to generate root propfind: {:?}",
            result.err()
        );

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");

        // Root response should contain populated current-user-principal
        assert!(
            xml_str.contains("/caldav/principals/testuser/"),
            "Should contain principal href, got: {}",
            xml_str
        );

        // Root response should contain populated calendar-home-set
        assert!(
            xml_str.contains("/caldav/testuser/"),
            "Should contain calendar home href, got: {}",
            xml_str
        );

        // Properties should NOT be empty self-closing elements
        assert!(
            !xml_str.contains("<D:current-user-principal/>"),
            "current-user-principal should not be empty"
        );
        assert!(
            !xml_str.contains("<C:calendar-home-set/>"),
            "calendar-home-set should not be empty"
        );
    }

    #[test]
    fn test_root_propfind_response_depth1_includes_calendars() {
        // Depth 1: root entry + calendars
        let calendars = vec![sample_calendar()];
        let request = PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_root_propfind_response(
            &mut output,
            &calendars,
            &request,
            "/caldav/",
            "testuser",
            "user-001",
        );
        assert!(result.is_ok());

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");

        // Should contain root entry with discovery properties
        assert!(xml_str.contains("/caldav/principals/testuser/"));
        // Should contain calendar entry
        assert!(xml_str.contains("cal-001"));
        assert!(xml_str.contains("Personal"));
    }

    #[test]
    fn test_principal_propfind_response() {
        let request = PropFindRequest {
            prop_find_type: PropFindType::Prop(vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "resourcetype".to_string(),
                },
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "displayname".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:caldav".to_string(),
                    name: "calendar-home-set".to_string(),
                },
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "current-user-principal".to_string(),
                },
            ]),
        };

        let mut output = Vec::new();
        let result =
            CalDavAdapter::generate_principal_propfind_response(&mut output, &request, "testuser");
        assert!(result.is_ok(), "Failed: {:?}", result.err());

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");

        // Should contain principal href
        assert!(xml_str.contains("/caldav/principals/testuser/"));
        // Should contain calendar-home-set
        assert!(xml_str.contains("/caldav/testuser/"));
        // Should contain principal in resourcetype
        assert!(xml_str.contains("D:principal"));
        // Should have displayname
        assert!(xml_str.contains("testuser"));
    }

    #[test]
    fn test_calendar_propfind_with_resolved_namespaces() {
        // Simulate what happens when a client asks for specific props
        // After namespace resolution fix, these should have proper URIs
        let calendar = sample_calendar();
        let request = PropFindRequest {
            prop_find_type: PropFindType::Prop(vec![
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "resourcetype".to_string(),
                },
                QualifiedName {
                    namespace: "DAV:".to_string(),
                    name: "displayname".to_string(),
                },
                QualifiedName {
                    namespace: "urn:ietf:params:xml:ns:caldav".to_string(),
                    name: "supported-calendar-component-set".to_string(),
                },
            ]),
        };

        let mut output = Vec::new();
        let result = CalDavAdapter::generate_calendar_collection_propfind(
            &mut output,
            &calendar,
            &[],
            &request,
            "/caldav/cal-001/",
            "0",
            "user-001",
        );
        assert!(result.is_ok(), "Failed: {:?}", result.err());

        let xml_str = String::from_utf8(output).expect("Invalid UTF-8");

        // resourcetype should have collection + calendar children
        assert!(
            xml_str.contains("D:collection"),
            "Should contain collection in resourcetype"
        );
        assert!(
            xml_str.contains("C:calendar"),
            "Should contain calendar in resourcetype"
        );

        // displayname should be populated
        assert!(xml_str.contains("Personal"), "Should contain calendar name");

        // supported-calendar-component-set should have VEVENT
        assert!(
            xml_str.contains("VEVENT"),
            "Should contain VEVENT component"
        );
    }

    #[test]
    fn test_report_namespace_resolution() {
        // Verify that REPORT parsing also resolves namespaces correctly
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:prop>
                <D:getetag/>
                <C:calendar-data/>
            </D:prop>
            <C:filter>
                <C:comp-filter name="VCALENDAR"/>
            </C:filter>
        </C:calendar-query>"#;

        let result = CalDavAdapter::parse_report(Cursor::new(xml));
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());

        match result.unwrap() {
            CalDavReportType::CalendarQuery { props, .. } => {
                assert_eq!(props.len(), 2);
                assert_eq!(props[0].namespace, "DAV:");
                assert_eq!(props[0].name, "getetag");
                assert_eq!(props[1].namespace, "urn:ietf:params:xml:ns:caldav");
                assert_eq!(props[1].name, "calendar-data");
            }
            other => panic!("Expected CalendarQuery, got {:?}", other),
        }
    }
}
