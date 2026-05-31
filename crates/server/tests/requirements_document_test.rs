mod support;

use support::{assert_contains_all, read_repo_file, requirement_lines};

#[test]
fn requirements_document_covers_all_server_requirement_sections() {
    let doc = read_repo_file("REQUIREMENTS.md");

    assert_contains_all(
        &doc,
        &[
            "# Phonolite Server Requirements",
            "## Runtime, Startup, and Process Model",
            "## Configuration and Defaults",
            "## Shared Data Model and Persistence",
            "## Library Discovery, Indexing, and Searchable Catalog Behavior",
            "## External Metadata Enrichment and Asset Handling",
            "## Authentication, Sessions, and Authorization",
            "## JSON API Surface and Contracts",
            "## Admin Console, HTML Flows, and Operator Actions",
            "## Logging, Activity, and Issue Tracking",
            "## QUIC Transport, Streaming Sessions, and Playback Stats",
            "## Transcoding, Raw Opus Format, and Stream Caching",
            "## Constraints and Current Implementation Limits",
            "## Suggested Next Traceability Layer",
        ],
    );
}

#[test]
fn requirements_document_has_depth_across_all_requirement_groups() {
    for (prefix, minimum) in [
        ("RUN-", 20usize),
        ("CFG-", 25),
        ("DATA-", 10),
        ("LIB-", 30),
        ("META-", 20),
        ("AUTH-", 20),
        ("API-", 45),
        ("ADMIN-", 45),
        ("LOG-", 15),
        ("STREAM-", 35),
        ("XCODE-", 25),
        ("LIM-", 10),
        ("TRACE-", 2),
    ] {
        let lines = requirement_lines(prefix);
        assert!(
            lines.len() >= minimum,
            "expected at least {} requirements for {}, found {}",
            minimum,
            prefix,
            lines.len()
        );
    }
}

#[test]
fn requirements_document_records_current_security_transport_and_cache_limits() {
    let doc = read_repo_file("REQUIREMENTS.md");

    assert_contains_all(
        &doc,
        &[
            "LIM-001: Password hashing currently uses plain SHA-256",
            "LIM-010: QUIC peer certificate verification is currently disabled",
            "STREAM-001: The server",
            "XCODE-021: The current memory-backed stream cache shall enforce a per-entry size limit of `256 MiB`.",
        ],
    );
}
