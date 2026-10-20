//! `refuse_tickets` effect: rules that make Validate / Create fault chosen tickets.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fakescan::profile::Profile;
use fakescan::state::World;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tower::ServiceExt;

fn world() -> Arc<RwLock<World>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let profile = Profile::load(&dir, "brother-wsd").expect("profile");
    Arc::new(RwLock::new(World::new(dir, profile)))
}

async fn send(
    w: &Arc<RwLock<World>>,
    method: &str,
    uri: &str,
    body: String,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "content-type",
            if uri.starts_with("/v1") {
                "application/json"
            } else {
                "application/soap+xml"
            },
        )
        .body(Body::from(body))
        .unwrap();
    let res = fakescan::server::device_router(w.clone(), true)
        .oneshot(req)
        .await
        .unwrap();
    let st = res.status();
    let b = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&b).into_owned())
}

fn ticket(op: &str, format: &str, color: &str, dpi: u32, source: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/{op}</wsa:Action>
    <wsa:MessageID>uuid:d1d1d1d1-d1d1-d1d1-d1d1-d1d1d1d1d1d1</wsa:MessageID>
  </soap:Header>
  <soap:Body><wscn:{op}Request><wscn:ScanTicket><wscn:DocumentParameters>
    <wscn:Format>{format}</wscn:Format>
    <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
    <wscn:InputSource>{source}</wscn:InputSource>
    <wscn:MediaSides><wscn:MediaFront>
      <wscn:ColorProcessing>{color}</wscn:ColorProcessing>
      <wscn:Resolution><wscn:Width>{dpi}</wscn:Width><wscn:Height>{dpi}</wscn:Height></wscn:Resolution>
    </wscn:MediaFront></wscn:MediaSides>
  </wscn:DocumentParameters></wscn:ScanTicket></wscn:{op}Request></soap:Body>
</soap:Envelope>"#
    )
}

async fn effects(w: &Arc<RwLock<World>>, body: &str) -> (StatusCode, String) {
    send(w, "POST", "/v1/effects", body.to_string()).await
}

async fn set_rules(w: &Arc<RwLock<World>>, rules: Value) {
    let (st, b) = effects(w, &json!({ "refuse_tickets": rules }).to_string()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// Is the ticket refused (a format fault) by both Validate and Create?
async fn refused(w: &Arc<RwLock<World>>, format: &str, color: &str, dpi: u32, src: &str) -> bool {
    let mut seen = Vec::new();
    for op in ["ValidateScanTicket", "CreateScanJob"] {
        let (_, b) = send(
            w,
            "POST",
            "/ScannerService",
            ticket(op, format, color, dpi, src),
        )
        .await;
        seen.push(b.contains("Fault"));
    }
    assert_eq!(
        seen[0], seen[1],
        "Validate/Create disagree for {format} {color} {dpi} {src}"
    );
    seen[0]
}

#[tokio::test]
async fn refuses_tiff_bw1_but_not_jfif_rgb() {
    let w = world();
    set_rules(
        &w,
        json!([{"format":"tiff-single-uncompressed","color":"BlackAndWhite1"}]),
    )
    .await;
    for op in ["ValidateScanTicket", "CreateScanJob"] {
        let (_, b) = send(
            &w,
            "POST",
            "/ScannerService",
            ticket(
                op,
                "tiff-single-uncompressed",
                "BlackAndWhite1",
                100,
                "Platen",
            ),
        )
        .await;
        assert!(
            b.contains("Fault") && b.contains("FormatNotSupported"),
            "{op}: {b}"
        );
    }
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
    assert!(!refused(&w, "tiff-single-uncompressed", "RGB24", 100, "Platen").await);
    assert!(!refused(&w, "jfif", "BlackAndWhite1", 100, "Platen").await);
    // The log records the refusals.
    let (_, t) = send(&w, "GET", "/v1/tickets", String::new()).await;
    let v: Value = serde_json::from_str(&t).unwrap();
    let outcomes: Vec<&str> = v["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes[0], "fault:wscn:ClientErrorFormatNotSupported",
        "{t}"
    );
    assert_eq!(
        outcomes[1], "refused:wscn:ClientErrorFormatNotSupported",
        "{t}"
    );
}

#[tokio::test]
async fn wildcards_and_all_fields() {
    let w = world();
    set_rules(&w, json!([{"color":"Grayscale8"}])).await;
    assert!(refused(&w, "jfif", "Grayscale8", 100, "Platen").await);
    assert!(refused(&w, "tiff-single-uncompressed", "Grayscale8", 200, "Platen").await);
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);

    set_rules(&w, json!([{"resolution":200}])).await;
    assert!(refused(&w, "jfif", "RGB24", 200, "Platen").await);
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);

    set_rules(&w, json!([{"input_source":"Platen","format":"jfif"}])).await;
    assert!(refused(&w, "jfif", "RGB24", 100, "Platen").await);
    assert!(!refused(&w, "tiff-single-uncompressed", "RGB24", 100, "Platen").await);
    // (Create over ADF faults for an empty feeder; check Validate only.)
    let (_, b) = send(
        &w,
        "POST",
        "/ScannerService",
        ticket("ValidateScanTicket", "jfif", "RGB24", 100, "ADF"),
    )
    .await;
    assert!(!b.contains("Fault"), "{b}");

    // An empty rule matches every ticket.
    set_rules(&w, json!([{}])).await;
    assert!(refused(&w, "jfif", "RGB24", 100, "Platen").await);
}

#[tokio::test]
async fn two_rules_and_clearing() {
    let w = world();
    set_rules(
        &w,
        json!([{"format":"jfif","color":"RGB24"},{"color":"BlackAndWhite1","fault":"ClientErrorFormatNotSupported"}]),
    )
    .await;
    assert!(refused(&w, "jfif", "RGB24", 100, "Platen").await);
    assert!(
        refused(
            &w,
            "tiff-single-uncompressed",
            "BlackAndWhite1",
            100,
            "Platen"
        )
        .await
    );
    assert!(!refused(&w, "tiff-single-uncompressed", "RGB24", 100, "Platen").await);

    // status shows the rules
    let (_, s) = send(&w, "GET", "/v1/status", String::new()).await;
    let v: Value = serde_json::from_str(&s).unwrap();
    assert_eq!(
        v["effects"]["refuse_tickets"],
        json!([{"format":"jfif","color":"RGB24"},{"color":"BlackAndWhite1","fault":"ClientErrorFormatNotSupported"}]),
        "{s}"
    );

    // a preset keeps them
    let (st, _) = effects(&w, r#"{"preset":"clean"}"#).await;
    assert_eq!(st, StatusCode::OK);
    assert!(refused(&w, "jfif", "RGB24", 100, "Platen").await);

    // [] clears
    set_rules(&w, json!([])).await;
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
    // null clears
    set_rules(&w, json!([{}])).await;
    set_rules(&w, Value::Null).await;
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
    // `clear` resets
    set_rules(&w, json!([{}])).await;
    let (st, _) = effects(&w, r#"{"clear":true}"#).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
    // unrelated POST leaves rules alone
    set_rules(&w, json!([{}])).await;
    effects(&w, r#"{"seed":3}"#).await;
    assert!(refused(&w, "jfif", "RGB24", 100, "Platen").await);
}

#[tokio::test]
async fn invalid_rules_are_400_with_message() {
    let w = world();
    for (body, needle) in [
        (r#"{"refuse_tickets":"nope"}"#, "refuse_tickets"),
        (r#"{"refuse_tickets":[{"colour":"RGB24"}]}"#, "colour"),
        (r#"{"refuse_tickets":[{"color":"Purple"}]}"#, "Purple"),
        (r#"{"refuse_tickets":[{"fault":"Bogus"}]}"#, "Bogus"),
        (
            r#"{"refuse_tickets":[{"resolution":"x"}]}"#,
            "refuse_tickets",
        ),
        (r#"{"refuse_tickets":[{"input_source":"Tray"}]}"#, "Tray"),
        (r#"{"refuse_tickets":[{"#, ""),
    ] {
        let (st, b) = effects(&w, body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}: {b}");
        assert!(b.contains(needle), "{body}: {b}");
    }
    // nothing was applied
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
}

#[tokio::test]
async fn interplay_with_jfif_bw_reject() {
    let w = world();
    let (st, _) = effects(&w, r#"{"jfif_bw":"reject"}"#).await;
    assert_eq!(st, StatusCode::OK);
    set_rules(
        &w,
        json!([{"format":"tiff-single-uncompressed","color":"BlackAndWhite1"}]),
    )
    .await;
    // either may fault
    assert!(refused(&w, "jfif", "BlackAndWhite1", 100, "Platen").await);
    assert!(
        refused(
            &w,
            "tiff-single-uncompressed",
            "BlackAndWhite1",
            100,
            "Platen"
        )
        .await
    );
    assert!(!refused(&w, "jfif", "RGB24", 100, "Platen").await);
    // clearing the rules leaves jfif_bw=reject in force
    set_rules(&w, json!([])).await;
    assert!(refused(&w, "jfif", "BlackAndWhite1", 100, "Platen").await);
    assert!(
        !refused(
            &w,
            "tiff-single-uncompressed",
            "BlackAndWhite1",
            100,
            "Platen"
        )
        .await
    );
}
