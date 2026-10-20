use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use fakescan::profile::Profile;
use fakescan::state::World;
use fakescan::wsd;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn world() -> Arc<RwLock<World>> {
    world_profile("brother-wsd")
}

fn world_profile(id: &str) -> Arc<RwLock<World>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let profile = Profile::load(&dir, id).expect("profile");
    Arc::new(RwLock::new(World::new(dir, profile)))
}

fn app(world: Arc<RwLock<World>>) -> axum::Router {
    axum::Router::new()
        .route("/ScannerService", axum::routing::post(wsd::scanner_service))
        .with_state(world)
}

async fn post_soap(app: axum::Router, xml: &str) -> (StatusCode, String) {
    post_soap_from(app, xml, "127.0.0.1:40000").await
}

/// SOAP oneshot from `peer`. A real client always has a TCP address; NotifyTo is stored only when
/// that address matches the sink host.
async fn post_soap_from(app: axum::Router, xml: &str, peer: &str) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(xml.to_string()))
        .unwrap();
    let addr: SocketAddr = peer.parse().expect("peer");
    req.extensions_mut().insert(ConnectInfo(addr));
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let mut problems = fakescan::wsd::soap::reply_problems(&body);
    problems.extend(fakescan::wsd::soap::body_problems(&body));
    // Requests are checked in `client_ticket_omits_fields_the_schema_requires`.
    // WIA omits JobDescription and DocumentDescription, sends InputSource Feeder,
    // and puts Resolution on DocumentParameters. The service still accepts those.
    // Lab reply deviations (Images*, JobToken ElementData, zero ImageInfo) are
    // allowed in `scan_schema`, not filtered here.
    assert!(problems.is_empty(), "schema: {problems:?}\n{body}");
    (status, body)
}

/// Like [`post_soap`], but returns `Err` when the response body stream aborts mid-send.
async fn post_soap_body_result(
    app: axum::Router,
    xml: &str,
) -> Result<(StatusCode, Vec<u8>), axum::Error> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(xml.to_string()))
        .unwrap();
    let addr: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024).await?;
    Ok((status, bytes.to_vec()))
}

#[tokio::test]
async fn get_scanner_elements_and_create_retrieve() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(1);
    }

    let elements = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:11111111-1111-1111-1111-111111111111</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetScannerElementsRequest>
      <wscn:RequestedElements>
        <wscn:Name>wscn:ScannerConfiguration</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w.clone()), elements).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("ScannerConfiguration"));
    assert!(body.contains("jfif"));
    assert!(
        body.contains("<wsa:RelatesTo>uuid:11111111-1111-1111-1111-111111111111</wsa:RelatesTo>")
    );
    assert!(body.contains("2006/01/wdp/scan"));

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:22222222-2222-2222-2222-222222222222</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:Resolution>
                <wscn:Width>200</wscn:Width>
                <wscn:Height>200</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("<wscn:JobId>"));
    assert!(body.contains("<wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>"));
    assert!(body.contains("<wscn:InputSource>Platen</wscn:InputSource>"));
    let job_id = body
        .split("<wscn:JobId>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();
    let token = body
        .split("<wscn:JobToken>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();

    let retrieve = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:33333333-3333-3333-3333-333333333333</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{job_id}</wscn:JobId>
      <wscn:JobToken>{token}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
    );

    let req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(retrieve))
        .unwrap();
    let res = app(w).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("multipart/related"), "{ct}");
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    assert!(bytes.windows(3).any(|w| w == b"\xff\xd8\xff"), "jpeg soi");
}

/// airscan-shaped walk: 2006/08 Action URIs, full RequestedElements, Platen ticket.
#[tokio::test]
async fn airscan_platen_flow_echoes_08_ns_and_jpeg() {
    let w = world();

    let elements = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:sca="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>urn:uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <sca:GetScannerElementsRequest>
      <sca:RequestedElements>
        <sca:Name>sca:ScannerConfiguration</sca:Name>
        <sca:Name>sca:ScannerDescription</sca:Name>
        <sca:Name>sca:DefaultScanTicket</sca:Name>
        <sca:Name>sca:ScannerStatus</sca:Name>
      </sca:RequestedElements>
    </sca:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w.clone()), elements).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("xmlns:wscn=\"http://schemas.microsoft.com/windows/2006/08/wdp/scan\""),
        "must echo airscan 08 namespace so fnmatch binds scan:"
    );
    assert!(body.contains("ScannerConfiguration"));
    assert!(body.contains("ScannerDescription"));
    assert!(body.contains("DefaultScanTicket"));
    assert!(body.contains("ScannerStatus"));
    assert!(body.contains("<wscn:Platen>"));
    assert!(body.contains("<wscn:ADF>"));
    assert!(body.contains("ImagesToTransfer"));
    assert!(body
        .contains("<wsa:RelatesTo>urn:uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa</wsa:RelatesTo>"));
    assert!(body.contains("/2006/08/wdp/scan/GetScannerElementsResponse"));

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:sca="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>urn:uuid:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <sca:CreateScanJobRequest>
      <sca:ScanTicket>
        <sca:JobDescription>
          <sca:JobName>scan</sca:JobName>
          <sca:JobOriginatingUserName>airscan</sca:JobOriginatingUserName>
        </sca:JobDescription>
        <sca:DocumentParameters>
          <sca:Format>jfif</sca:Format>
          <sca:ImagesToTransfer>1</sca:ImagesToTransfer>
          <sca:InputSource>Platen</sca:InputSource>
          <sca:ContentType>Auto</sca:ContentType>
          <sca:InputSize>
            <sca:InputMediaSize>
              <sca:Width>8500</sca:Width>
              <sca:Height>11000</sca:Height>
            </sca:InputMediaSize>
          </sca:InputSize>
          <sca:MediaSides>
            <sca:MediaFront>
              <sca:ColorProcessing>RGB24</sca:ColorProcessing>
              <sca:Resolution>
                <sca:Width>200</sca:Width>
                <sca:Height>200</sca:Height>
              </sca:Resolution>
            </sca:MediaFront>
          </sca:MediaSides>
        </sca:DocumentParameters>
      </sca:ScanTicket>
    </sca:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("2006/08/wdp/scan"));
    assert!(body.contains("<wscn:JobId>"));
    // Brother live jfif: BPL = PixelsPerLine×3 (optical 1648@200 → 4944), not MS compressed=0.
    assert!(
        body.contains("<wscn:PixelsPerLine>1648</wscn:PixelsPerLine>"),
        "ImageInformation uses Brother optical crop, not full letter: {body}"
    );
    assert!(
        body.contains("<wscn:BytesPerLine>4944</wscn:BytesPerLine>"),
        "jfif ImageInformation BytesPerLine must be RGB stride: {body}"
    );
    let job_id = body
        .split("<wscn:JobId>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();
    let token = body
        .split("<wscn:JobToken>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();

    let retrieve = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:sca="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>urn:uuid:cccccccc-cccc-cccc-cccc-cccccccccccc</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <sca:RetrieveImageRequest>
      <sca:JobId>{job_id}</sca:JobId>
      <sca:JobToken>{token}</sca:JobToken>
    </sca:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
    );

    let req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(retrieve))
        .unwrap();
    let res = app(w.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("multipart/related"), "{ct}");
    assert!(
        ct.contains("startinfo=application/soap+xml")
            && ct.contains("start-info=application/soap+xml"),
        "outer CT must emit MS startinfo and RFC start-info: {ct}"
    );
    assert!(
        res.headers()
            .get("mime-version")
            .and_then(|v| v.to_str().ok())
            == Some("1.0"),
        "MIME-Version: 1.0 required"
    );
    // Known-length body (not chunked) — wsdscdrv hung on chunked in early lab.
    let cl = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    assert!(res.headers().get("transfer-encoding").is_none());
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(cl, Some(bytes.len()), "Content-Length must match body");
    assert!(bytes.windows(3).any(|w| w == b"\xff\xd8\xff"), "jpeg soi");
    let soap_part = String::from_utf8_lossy(&bytes);
    assert!(soap_part.contains("2006/08/wdp/scan/RetrieveImage"));
    assert!(soap_part.contains("xop:Include"));
    // Brother live + WIA accept: 2004/08 XOP include NS (2003/12 → CancelJob after RetrieveImage).
    assert!(
        soap_part.contains("http://www.w3.org/2004/08/xop/include"),
        "XOP xmlns must be 2004/08 for wsdscdrv accept: {}",
        soap_part.lines().take(8).collect::<Vec<_>>().join(" ")
    );
    assert!(
        !soap_part.contains("http://www.w3.org/2003/12/xop/include"),
        "2003/12 XOP xmlns is rejected by inbox WSD"
    );
    // SOF must match ImageInformation optical crop (1648×2289 @200), not full letter.
    let jpeg_start = bytes.windows(3).position(|w| w == b"\xff\xd8\xff").unwrap();
    let sof = fakescan::image_gen::jpeg_sof0_dimensions(&bytes[jpeg_start..]);
    assert_eq!(
        sof,
        Some((1648, 2289)),
        "RetrieveImage JPEG SOF must match Create ImageInformation optical crop"
    );
}

/// Live 2026-10-12 (Windows 11): coercing a Grayscale8 ticket to RGB24 FinalParams
/// makes wsdscdrv skip RetrieveImage. Default FinalParams must stay Grayscale8, BPL = width.
#[tokio::test]
async fn create_scan_job_jfif_grayscale8_final_params_stay_gray() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(1);
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>urn:uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:JobDescription>
          <wscn:JobName>gray-jfif</wscn:JobName>
          <wscn:JobOriginatingUserName>lab</wscn:JobOriginatingUserName>
        </wscn:JobDescription>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:ColorProcessing>Grayscale8</wscn:ColorProcessing>
              <wscn:Resolution>
                <wscn:Width>200</wscn:Width>
                <wscn:Height>200</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains(">Grayscale8</wscn:ColorProcessing>"),
        "jfif FinalParams must echo Grayscale8: {body}"
    );
    assert!(
        !body.contains(">RGB24</wscn:ColorProcessing>"),
        "FinalParams must not coerce gray to RGB24: {body}"
    );
    let pixels: u32 = body
        .split("<wscn:PixelsPerLine>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .and_then(|s| s.parse().ok())
        .expect("PixelsPerLine");
    let bpl: u32 = body
        .split("<wscn:BytesPerLine>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .and_then(|s| s.parse().ok())
        .expect("BytesPerLine");
    assert_eq!(
        bpl, pixels,
        "gray jfif ImageInformation BPL must be width, got pixels={pixels} bpl={bpl}"
    );
}

/// Validate before Create must accept gray+jfif with the gray BytesPerLine (= width).
#[tokio::test]
async fn validate_scan_ticket_jfif_grayscale8_gray_stride() {
    let w = world();
    let validate = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/ValidateScanTicket</wsa:Action>
    <wsa:MessageID>urn:uuid:eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:ValidateScanTicketRequest>
      <wscn:ScanTicket>
        <wscn:JobDescription>
          <wscn:JobName>gray-validate</wscn:JobName>
          <wscn:JobOriginatingUserName>lab</wscn:JobOriginatingUserName>
        </wscn:JobDescription>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:ColorProcessing>Grayscale8</wscn:ColorProcessing>
              <wscn:Resolution>
                <wscn:Width>200</wscn:Width>
                <wscn:Height>200</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:ValidateScanTicketRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), validate).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ValidTicket>true</wscn:ValidTicket>"),
        "gray+jfif must ValidTicket=true: {body}"
    );
    let pixels: u32 = body
        .split("<wscn:PixelsPerLine>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .and_then(|s| s.parse().ok())
        .expect("PixelsPerLine");
    let bpl: u32 = body
        .split("<wscn:BytesPerLine>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .and_then(|s| s.parse().ok())
        .expect("BytesPerLine");
    assert_eq!(
        bpl, pixels,
        "Validate ImageInformation BPL must be width for gray jfif: pixels={pixels} bpl={bpl}"
    );
}

#[tokio::test]
async fn ws_transfer_get_includes_device_category_and_hosted_scan() {
    let w = world();
    let get = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/09/transfer/Get</wsa:Action>
    <wsa:MessageID>uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:MessageID>
  </soap:Header>
  <soap:Body/>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w), get).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("transfer/GetResponse"));
    assert!(
        body.contains("<wsa:RelatesTo>uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:RelatesTo>"),
        "RelatesTo must echo request MessageID: {body}"
    );
    assert!(body.contains("ThisDevice"));
    assert!(body.contains("ThisModel"));
    assert!(
        body.contains("Fake WSD scanner") || body.contains(">fakescan</wsdp:FriendlyName>"),
        "FriendlyName must look like fakescan: {body}"
    );
    assert!(body.contains("<pnpx:DeviceCategory>Scanners</pnpx:DeviceCategory>"));
    assert!(
        body.contains("<df:DeviceCategory>Imaging.Scanner</df:DeviceCategory>"),
        "df:DeviceCategory Imaging.Scanner required for Add-device: {body}"
    );
    assert!(body.contains("Relationship"));
    assert!(
        body.contains("<wsdp:Host>"),
        "DPWS Relationship must include Host (urn:uuid device EPR): {body}"
    );
    assert!(
        body.contains("wscn:ScanDeviceType"),
        "Host Types must advertise ScanDeviceType: {body}"
    );
    assert!(body.contains("<wsdp:Hosted>"));
    assert!(
        body.contains("wscn:ScannerServiceType"),
        "Hosted Types must use wscn:ScannerServiceType QName: {body}"
    );
    assert!(
        body.contains("http://schemas.microsoft.com/windows/2006/08/wdp/scan/ScannerServiceType"),
        "CompatibleId / xmlns must carry 2006/08 ScannerServiceType: {body}"
    );
    assert!(
        body.contains("<pnpx:CompatibleId"),
        "Hosted pnpx:CompatibleId required for WSDScan INF match: {body}"
    );
    assert!(
        body.contains("<pnpx:HardwareId"),
        "Hosted pnpx:HardwareId required with DeviceCategory: {body}"
    );
    assert!(
        body.contains("<wsa:Address>http://127.0.0.1:3920/ScannerService</wsa:Address>"),
        "Hosted EPR must point at ScannerService: {body}"
    );
    assert!(body.contains("/ScannerService"));
}

fn ticket_envelope(action: &str, format: &str, dpi: u32) -> String {
    ticket_envelope_honor(action, format, dpi, false)
}

fn ticket_envelope_honor(action: &str, format: &str, dpi: u32, dpi_must_honor: bool) -> String {
    let res_attr = if dpi_must_honor {
        r#" wscn:MustHonor="true""#
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/{action}</wsa:Action>
    <wsa:MessageID>uuid:eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:{action}Request>
      <wscn:ScanTicket>
        <wscn:JobDescription>
          <wscn:JobName>fakescan</wscn:JobName>
          <wscn:JobOriginatingUserName>fakescan</wscn:JobOriginatingUserName>
        </wscn:JobDescription>
        <wscn:DocumentParameters>
          <wscn:Format>{format}</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:Resolution{res_attr}>
                <wscn:Width>{dpi}</wscn:Width>
                <wscn:Height>{dpi}</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:{action}Request>
  </soap:Body>
</soap:Envelope>"#
    )
}

#[tokio::test]
async fn validate_scan_ticket_refuses_unknown_dpi_and_format() {
    let w = world();

    let (st, body) = post_soap(
        app(w.clone()),
        &ticket_envelope("ValidateScanTicket", "jfif", 200),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ValidTicket>true</wscn:ValidTicket>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ImageInformation>") && body.contains("<wscn:PixelsPerLine>"),
        "WIA needs ImageInformation in ValidationInfo: {body}"
    );

    // MustHonor absent → substitute nearest profile dpi (ValidTicket=true).
    let (st, body) = post_soap(
        app(w.clone()),
        &ticket_envelope("ValidateScanTicket", "jfif", 999),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ValidTicket>true</wscn:ValidTicket>"),
        "dpi miss without MustHonor substitutes: {body}"
    );
    assert!(
        body.contains("<wscn:PixelsPerLine>")
            && !body.contains("<wscn:PixelsPerLine>0</wscn:PixelsPerLine>"),
        "substituted dpi still reports ImageInformation: {body}"
    );

    // MustHonor=true → ValidTicket=false + ValidScanTicket rewrite to nearest.
    let (st, body) = post_soap(
        app(w.clone()),
        &ticket_envelope_honor("ValidateScanTicket", "jfif", 999, true),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ValidTicket>false</wscn:ValidTicket>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:PixelsPerLine>0</wscn:PixelsPerLine>")
            && body.contains("<wscn:NumberOfLines>0</wscn:NumberOfLines>")
            && body.contains("<wscn:BytesPerLine>0</wscn:BytesPerLine>"),
        "Learn invalid uses zero ImageInformation: {body}"
    );
    assert!(
        body.contains("<wscn:ValidScanTicket>") && body.contains("<wscn:Width>300</wscn:Width>"),
        "invalid dpi must rewrite ValidScanTicket to nearest profile dpi: {body}"
    );

    // Format miss is soft-accepted (ValidTicket=true) so WIA still reaches CreateScanJob.
    let (st, body) = post_soap(
        app(w.clone()),
        &ticket_envelope("ValidateScanTicket", "png", 200),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ValidTicket>true</wscn:ValidTicket>"),
        "format soft-accept: {body}"
    );
    assert!(
        !body.contains("<wscn:ValidScanTicket>"),
        "soft-accept must not emit ValidScanTicket rewrite: {body}"
    );
}

#[tokio::test]
async fn create_scan_job_refuses_unknown_dpi() {
    let w = world();
    // Absent MustHonor → Create substitutes nearest dpi (Override on FinalParameters).
    let (st, body) = post_soap(
        app(w.clone()),
        &ticket_envelope("CreateScanJob", "jfif", 999),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("CreateScanJobResponse") && body.contains(r#"wscn:Override="true""#),
        "dpi substitute marks Override: {body}"
    );

    // MustHonor=true → refuse.
    let (st, body) = post_soap(
        app(w),
        &ticket_envelope_honor("CreateScanJob", "jfif", 999, true),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body.contains("invalid scan ticket") || body.contains("dpi") || body.contains("Fault"),
        "{body}"
    );
}

#[tokio::test]
async fn client_ticket_omits_fields_the_schema_requires() {
    let w = world();
    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>200</wscn:Width>
            <wscn:Height>200</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let request = fakescan::wsd::soap::body_problems(create);
    assert!(
        request.iter().any(|p| p.contains("missing JobDescription")),
        "{request:?}"
    );
    assert!(
        request
            .iter()
            .any(|p| p.contains("unexpected child Resolution")),
        "{request:?}"
    );
    let (st, body) = post_soap(app(w), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("<wscn:JobId>"), "{body}");
}

#[tokio::test]
async fn advertises_encodable_jfif_and_tiff() {
    let w = world();
    let elements = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:ffffffff-ffff-ffff-ffff-ffffffffffff</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetScannerElementsRequest>
      <wscn:RequestedElements>
        <wscn:Name>wscn:ScannerConfiguration</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), elements).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:FormatValue>jfif</wscn:FormatValue>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:FormatValue>tiff-single-uncompressed</wscn:FormatValue>"),
        "brother-wsd lists tiff and we encode it: {body}"
    );
    // brother-wsd is tiff+jfif only (Brother GSE live); dib lives on accept-dib / wia-rich.
    assert!(
        !body.contains("<wscn:FormatValue>dib</wscn:FormatValue>"),
        "brother-wsd must not advertise dib: {body}"
    );
}

#[tokio::test]
async fn wia_rich_advertises_every_shipped_format() {
    use fakescan::state::SHIPPED_WSD_FORMATS;
    let w = world_profile("wia-rich");
    let elements = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetScannerElementsRequest>
      <wscn:RequestedElements>
        <wscn:Name>wscn:ScannerConfiguration</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), elements).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    for fmt in SHIPPED_WSD_FORMATS {
        assert!(
            body.contains(&format!("<wscn:FormatValue>{fmt}</wscn:FormatValue>")),
            "missing {fmt} in: {body}"
        );
    }
    assert!(
        !body.contains("jbig"),
        "must not advertise unencodable jbig: {body}"
    );
}

#[tokio::test]
async fn retrieve_image_honors_tiff_format() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(1);
    }
    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:b1b1b1b1-b1b1-b1b1-b1b1-b1b1b1b1b1b1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>tiff-single-uncompressed</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:ColorProcessing>RGB24</wscn:ColorProcessing>
              <wscn:Resolution>
                <wscn:Width>100</wscn:Width>
                <wscn:Height>100</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let job_id = body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");
    let token = body
        .split("<wscn:JobToken>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobToken");
    let retrieve = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:c2c2c2c2-c2c2-c2c2-c2c2-c2c2c2c2c2c2</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{job_id}</wscn:JobId>
      <wscn:JobToken>{token}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
    );
    let req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(retrieve))
        .unwrap();
    let res = app(w).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.contains("multipart") || ct.contains("related"), "{ct}");
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    // TIFF magic II*\0 or MM\0*
    let has_tiff = bytes
        .windows(4)
        .any(|w| (w == [0x49, 0x49, 0x2A, 0x00]) || (w == [0x4D, 0x4D, 0x00, 0x2A]));
    assert!(has_tiff, "MTOM body should include TIFF payload");
    assert!(
        String::from_utf8_lossy(&bytes).contains("image/tiff"),
        "MTOM part Content-Type should be image/tiff"
    );
}

#[tokio::test]
async fn adf_empty_after_last_page_returns_no_images_while_job_alive() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(1);
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:a0a0a0a0-a0a0-a0a0-a0a0-a0a0a0a0a0a0</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:Resolution>
                <wscn:Width>200</wscn:Width>
                <wscn:Height>200</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;

    let (st, body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let job_id = body
        .split("<wscn:JobId>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();
    let token = body
        .split("<wscn:JobToken>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_string();

    let retrieve = |job_id: &str, token: &str| {
        format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:b0b0b0b0-b0b0-b0b0-b0b0-b0b0b0b0b0b0</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{job_id}</wscn:JobId>
      <wscn:JobToken>{token}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
        )
    };

    let (st, _) = post_soap(app(w.clone()), &retrieve(&job_id, &token)).await;
    assert_eq!(st, StatusCode::OK);

    let (st, body) = post_soap(app(w.clone()), &retrieve(&job_id, &token)).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body.contains("ClientErrorNoImagesAvailable"),
        "expected NoImagesAvailable, got: {body}"
    );
    assert!(
        !body.contains("JobIdNotFound"),
        "must keep job alive: {body}"
    );

    let job_elems = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetJobElements</wsa:Action>
    <wsa:MessageID>uuid:c0c0c0c0-c0c0-c0c0-c0c0-c0c0c0c0c0c0</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetJobElementsRequest>
      <wscn:JobId>{job_id}</wscn:JobId>
      <wscn:RequestedElements>
        <wscn:Name>wscn:JobStatus</wscn:Name>
        <wscn:Name>wscn:JobToken</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetJobElementsRequest>
  </soap:Body>
</soap:Envelope>"#
    );
    let (st, body) = post_soap(app(w), &job_elems).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains(&format!("<wscn:JobId>{job_id}</wscn:JobId>")),
        "{body}"
    );
    assert!(
        body.contains(r#"Name="wscn:JobToken" Valid="false""#),
        "JobToken is not a JobElements entry: {body}"
    );
    assert!(
        body.contains("<wscn:JobState>Completed</wscn:JobState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScansCompleted>1</wscn:ScansCompleted>"),
        "{body}"
    );
}

#[tokio::test]
async fn get_active_jobs_includes_state_reason() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(1);
    }
    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:a1a1a1a1-a1a1-a1a1-a1a1-a1a1a1a1a1a1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:Resolution>
                <wscn:Width>100</wscn:Width>
                <wscn:Height>100</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, create_body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{create_body}");
    let job_id = create_body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");

    let active = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetActiveJobs</wsa:Action>
    <wsa:MessageID>uuid:b1b1b1b1-b1b1-b1b1-b1b1-b1b1b1b1b1b1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetActiveJobsRequest/>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), active).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains(&format!("<wscn:JobId>{job_id}</wscn:JobId>")),
        "{body}"
    );
    assert!(body.contains("<wscn:JobStateReason>"), "{body}");
    assert!(
        body.contains("<wscn:ScansCompleted>0</wscn:ScansCompleted>"),
        "{body}"
    );
}

#[tokio::test]
async fn temporary_fault_maps_xsd_subcode() {
    let w = world();
    {
        let mut g = w.write().await;
        g.apply_faults(&fakescan::state::FaultBody {
            temporary: Some(true),
            ..Default::default()
        });
    }
    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:c1c1c1c1-c1c1-c1c1-c1c1-c1c1c1c1c1c1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:MediaSides>
            <wscn:MediaFront>
              <wscn:Resolution>
                <wscn:Width>100</wscn:Width>
                <wscn:Height>100</wscn:Height>
              </wscn:Resolution>
            </wscn:MediaFront>
          </wscn:MediaSides>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), create).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body.contains("ServerErrorTemporaryError"),
        "expected Temporary subcode, got: {body}"
    );
}

#[tokio::test]
async fn get_job_history_stub() {
    let w = world();
    let xml = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetJobHistory</wsa:Action>
    <wsa:MessageID>uuid:d0d0d0d0-d0d0-d0d0-d0d0-d0d0d0d0d0d0</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetJobHistoryRequest/>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), xml).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("GetJobHistoryResponse"), "{body}");
    assert!(body.contains("JobHistory"), "{body}");
}

fn scanner_status_request() -> &'static str {
    r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:e0e0e0e0-e0e0-e0e0-e0e0-e0e0e0e0e0e0</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetScannerElementsRequest>
      <wscn:RequestedElements>
        <wscn:Name>wscn:ScannerStatus</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#
}

#[tokio::test]
async fn scanner_status_idle_reason_none_when_feeder_empty() {
    // Empty ADF must stay Idle/None — AttentionRequired makes WIA treat glass as offline.
    let w = world();
    {
        let mut g = w.write().await;
        g.set_feeder(false);
        g.set_jam(false);
        g.set_cover_open(false);
    }
    let (st, body) = post_soap(app(w), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ScannerState>Idle</wscn:ScannerState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
        "expected None reason: {body}"
    );
    assert!(
        !body.contains("AttentionRequired"),
        "feeder_ready=false must not emit AttentionRequired: {body}"
    );
}

#[tokio::test]
async fn scanner_status_jam_is_media_jam() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_jam(true);
        g.set_cover_open(false);
    }
    let (st, body) = post_soap(app(w), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>MediaJam</wscn:ScannerStateReason>"),
        "{body}"
    );
    assert!(!body.contains("CoverOpen"), "{body}");
    assert!(!body.contains("AttentionRequired"), "{body}");
}

#[tokio::test]
async fn scanner_status_cover_open_is_cover_open() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_jam(false);
        g.set_cover_open(true);
    }
    let (st, body) = post_soap(app(w), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
        "{body}"
    );
    assert!(!body.contains("MediaJam"), "{body}");
    assert!(!body.contains("AttentionRequired"), "{body}");
}

#[tokio::test]
async fn scanner_status_reflects_jam_and_cover() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_jam(true);
        g.set_cover_open(true);
    }
    let (st, body) = post_soap(app(w), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>MediaJam</wscn:ScannerStateReason>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
        "{body}"
    );
}

#[tokio::test]
async fn subscribe_returns_destination_tokens() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/08/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:11111111-1111-1111-1111-111111111111</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT1H</wse:Expires>
      <wscn:ScanDestinations>
        <wscn:ScanDestination>
          <wscn:ClientDisplayName>Lab PC</wscn:ClientDisplayName>
          <wscn:ClientContext>ctx-lab</wscn:ClientContext>
        </wscn:ScanDestination>
      </wscn:ScanDestinations>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), sub).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("eventing/SubscribeResponse"),
        "action must be SubscribeResponse: {body}"
    );
    assert!(body.contains("<wse:SubscribeResponse"), "{body}");
    assert!(body.contains("<wse:SubscriptionManager>"), "{body}");
    // SubscriptionManager EPR is the ScannerService URL (not Device) + Identifier.
    assert!(
        body.contains("<wsa:Address>http://127.0.0.1:3920/ScannerService</wsa:Address>"),
        "SubscriptionManager Address must be ScannerService: {body}"
    );
    assert!(
        !body.contains("<wsa:Address>http://127.0.0.1:3920/WebServices/Device</wsa:Address>"),
        "SubscriptionManager must not use Device EPR: {body}"
    );
    assert!(
        body.contains("<wse:Identifier>uuid:"),
        "SubscriptionManager ReferenceParameters Identifier: {body}"
    );
    assert!(body.contains("<wse:Expires>PT1H</wse:Expires>"), "{body}");
    assert!(
        body.contains("<wscn:DestinationResponses>"),
        "DestinationResponses wrapper required: {body}"
    );
    assert!(
        body.contains("<wscn:DestinationResponse>"),
        "DestinationResponse per ScanDestination: {body}"
    );
    assert!(
        body.contains("<wscn:ClientContext>ctx-lab</wscn:ClientContext>"),
        "echo ClientContext: {body}"
    );
    // Token must sit inside DestinationResponse with a non-empty value.
    let token_open = body.find("<wscn:DestinationToken>").unwrap_or_else(|| {
        panic!("DestinationToken required when ScanDestinations present: {body}")
    });
    let token_close = body[token_open..]
        .find("</wscn:DestinationToken>")
        .unwrap_or_else(|| panic!("DestinationToken close tag: {body}"));
    let token = body[token_open + "<wscn:DestinationToken>".len()..token_open + token_close].trim();
    assert!(
        !token.is_empty(),
        "DestinationToken must be non-empty: {body}"
    );
    // ClientContext echo and DestinationToken share one DestinationResponse.
    let resp_start = body
        .find("<wscn:DestinationResponse>")
        .unwrap_or_else(|| panic!("DestinationResponse: {body}"));
    let resp_end = body[resp_start..]
        .find("</wscn:DestinationResponse>")
        .unwrap_or_else(|| panic!("DestinationResponse close: {body}"));
    let resp = &body[resp_start..resp_start + resp_end];
    assert!(
        resp.contains("<wscn:ClientContext>ctx-lab</wscn:ClientContext>"),
        "ClientContext must be inside DestinationResponse: {resp}"
    );
    assert!(
        resp.contains("<wscn:DestinationToken>"),
        "DestinationToken must be inside DestinationResponse: {resp}"
    );
}

#[tokio::test]
async fn subscribe_without_scan_destinations_omits_destination_responses() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:22222222-2222-2222-2222-222222222222</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT2H</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), sub).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("eventing/SubscribeResponse"),
        "action must be SubscribeResponse: {body}"
    );
    assert!(body.contains("<wse:SubscribeResponse"), "{body}");
    assert!(body.contains("<wse:SubscriptionManager>"), "{body}");
    assert!(body.contains("<wse:Expires>PT2H</wse:Expires>"), "{body}");
    assert!(
        !body.contains("DestinationResponses") && !body.contains("DestinationResponse"),
        "no DestinationResponses when ScanDestinations omitted: {body}"
    );
    assert!(
        !body.contains("DestinationToken"),
        "no DestinationToken when ScanDestinations omitted: {body}"
    );
}

#[tokio::test]
async fn resubscribe_replaces_single_subscription() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = |msg: &str, sink: &str| {
        format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>{msg}</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>{sink}</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT1H</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#
        )
    };
    let (st, body1) = post_soap(
        app(w.clone()),
        &sub(
            "uuid:resub-1-0000-0000-000000000001",
            "http://127.0.0.1:9/sink-a",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body1}");
    let id1 = body1
        .split("<wse:Identifier>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("first Identifier")
        .to_string();
    let (st, body2) = post_soap(
        app(w.clone()),
        &sub(
            "uuid:resub-2-0000-0000-000000000002",
            "http://127.0.0.1:9/sink-b",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body2}");
    let id2 = body2
        .split("<wse:Identifier>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("second Identifier")
        .to_string();
    assert_ne!(id1, id2, "re-Subscribe must mint a new Identifier");
    {
        let g = w.read().await;
        assert_eq!(g.subscription_id(), Some(id2.as_str()));
        assert_eq!(g.notify_to(), Some("http://127.0.0.1:9/sink-b"));
    }
    // Prior Identifier is gone — Renew with id1 faults.
    let renew = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Renew</wsa:Action>
    <wsa:MessageID>uuid:resub-renew-old-0000-000000000001</wsa:MessageID>
    <wsa:ReferenceParameters>
      <wse:Identifier>{id1}</wse:Identifier>
    </wsa:ReferenceParameters>
  </soap:Header>
  <soap:Body>
    <wse:Renew><wse:Expires>PT2H</wse:Expires></wse:Renew>
  </soap:Body>
</soap:Envelope>"#
    );
    let (st, body) = post_soap(app(w), &renew).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("InvalidMessage"), "{body}");
}

#[tokio::test]
async fn subscribe_from_another_host_does_not_store_notify_to() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:foreign-sub-0000-0000-000000000001</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT1H</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#;
    // Subscribe still succeeds (a fault looks like the device is offline), but the sink is not
    // kept: the TCP peer is not the NotifyTo host.
    let (st, body) = post_soap_from(app(w.clone()), sub, "192.168.1.20:40000").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("eventing/SubscribeResponse"), "{body}");
    assert!(w.read().await.notify_to().is_none());
    assert!(w.read().await.subscription_id().is_some());
}

#[tokio::test]
async fn renew_with_matching_identifier_ok() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:renew-ok-sub-0000-0000-000000000001</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT1H</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w.clone()), sub).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let identifier = body
        .split("<wse:Identifier>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("wse:Identifier in SubscribeResponse")
        .to_string();
    assert!(
        identifier.starts_with("uuid:"),
        "expected uuid: Identifier, got {identifier}"
    );

    let renew = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Renew</wsa:Action>
    <wsa:MessageID>uuid:renew-ok-ren-0000-0000-000000000001</wsa:MessageID>
    <wsa:To>http://127.0.0.1:3920/ScannerService</wsa:To>
    <wsa:ReferenceParameters>
      <wse:Identifier>{identifier}</wse:Identifier>
    </wsa:ReferenceParameters>
  </soap:Header>
  <soap:Body>
    <wse:Renew>
      <wse:Expires>PT2H</wse:Expires>
    </wse:Renew>
  </soap:Body>
</soap:Envelope>"#
    );
    let (st, body) = post_soap(app(w), &renew).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("eventing/RenewResponse"),
        "action must be RenewResponse: {body}"
    );
    assert!(body.contains("<wse:RenewResponse"), "{body}");
    assert!(body.contains("<wse:Expires>PT2H</wse:Expires>"), "{body}");
}

#[tokio::test]
async fn renew_with_wrong_identifier_faults() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    let sub = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:renew-bad-sub-0000-0000-000000000001</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>PT1H</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w.clone()), sub).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("<wse:Identifier>"), "{body}");

    let renew = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Renew</wsa:Action>
    <wsa:MessageID>uuid:renew-bad-ren-0000-0000-000000000001</wsa:MessageID>
    <wsa:ReferenceParameters>
      <wse:Identifier>uuid:deadbeef</wse:Identifier>
    </wsa:ReferenceParameters>
  </soap:Header>
  <soap:Body>
    <wse:Renew>
      <wse:Expires>PT2H</wse:Expires>
    </wse:Renew>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), renew).await;
    // FaultOut maps to HTTP 500 with a SOAP Fault body.
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("Fault") || body.contains("fault"), "{body}");
    assert!(
        body.contains("InvalidMessage"),
        "expected InvalidMessage fault: {body}"
    );
    assert!(
        body.contains("mismatched") || body.contains("unknown"),
        "expected mismatched/unknown Identifier reason: {body}"
    );
}

#[tokio::test]
async fn get_scanner_elements_condition_history_after_clear() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        // Arm feeder so derived InputTrayEmpty (Id 9) does not appear idle.
        g.set_pages(1);
        g.apply_faults(&FaultBody {
            jam: Some(true),
            ..Default::default()
        });
        g.apply_faults(&FaultBody {
            jam: Some(false),
            ..Default::default()
        });
    }

    let (st, body) = post_soap(app(w), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains("<wscn:ConditionHistoryEntry"),
        "cleared jam must leave ConditionHistoryEntry: {body}"
    );
    assert!(body.contains("MediaJam"), "{body}");
    assert!(
        body.contains("<wscn:ClearTime>"),
        "ConditionHistoryEntry needs ClearTime: {body}"
    );
    assert!(
        body.contains("<wscn:ActiveConditions/>"),
        "ActiveConditions must be empty/self-closing after clear: {body}"
    );
    assert!(
        !body.contains("<wscn:DeviceCondition"),
        "no active DeviceCondition after clear: {body}"
    );
}

#[tokio::test]
async fn subscribe_echoes_absolute_expires() {
    let w = world();
    {
        let mut g = w.write().await;
        g.set_identity(
            "http://127.0.0.1:3920/WebServices/Device".into(),
            "http://127.0.0.1:3920/ScannerService".into(),
            uuid::Uuid::nil(),
        );
    }
    // ~2 minutes ahead — echo only; no deadline sleep in this test.
    let expires = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + 120;
        fakescan::timeutil::format_unix_secs_utc(secs)
    };
    let sub = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe</wsa:Action>
    <wsa:MessageID>uuid:abs-exp-sub-0000-0000-000000000001</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Subscribe>
      <wse:Delivery>
        <wse:NotifyTo>
          <wsa:Address>http://127.0.0.1:9/sink</wsa:Address>
        </wse:NotifyTo>
      </wse:Delivery>
      <wse:Expires>{expires}</wse:Expires>
    </wse:Subscribe>
  </soap:Body>
</soap:Envelope>"#
    );
    let (st, body) = post_soap(app(w), &sub).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body.contains(&format!("<wse:Expires>{expires}</wse:Expires>")),
        "SubscribeResponse must echo absolute Expires: {body}"
    );
}

#[tokio::test]
async fn retrieve_image_jam_after_n_returns_media_jam_subcode() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(3);
        g.apply_faults(&FaultBody {
            jam_after_n: Some(1),
            ..Default::default()
        });
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>100</wscn:Width>
            <wscn:Height>100</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, create_body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{create_body}");
    let job_id = create_body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");
    let token = create_body
        .split("<wscn:JobToken>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobToken");

    let retrieve = |id: &str, tok: &str| {
        format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{id}</wscn:JobId>
      <wscn:JobToken>{tok}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
        )
    };

    let (st0, body0) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st0, StatusCode::OK, "{body0}");
    assert!(!body0.contains("Fault"), "{body0}");

    let (st1, body1) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st1, StatusCode::INTERNAL_SERVER_ERROR, "{body1}");
    assert!(
        body1.contains("ClientErrorMediaJam"),
        "expected MediaJam SOAP subcode: {body1}"
    );
    assert!(w.read().await.jam(), "jam sticky after trip");
}

#[tokio::test]
async fn sticky_offline_refuses_create_with_not_accepting_jobs() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        g.apply_faults(&FaultBody {
            offline: Some(true),
            ..Default::default()
        });
    }

    let status = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:cccccccc-cccc-cccc-cccc-cccccccccccc</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:GetScannerElementsRequest>
      <wscn:RequestedElements>
        <wscn:Name>wscn:ScannerStatus</wscn:Name>
      </wscn:RequestedElements>
    </wscn:GetScannerElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w.clone()), status).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("AttentionRequired"), "{body}");
    assert!(body.contains("Stopped"), "{body}");

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>100</wscn:Width>
            <wscn:Height>100</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, body) = post_soap(app(w), create).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body.contains("ServerErrorNotAcceptingJobs") || body.contains("NotAcceptingJobs"),
        "expected NotAcceptingJobs: {body}"
    );
}

#[tokio::test]
async fn retrieve_image_offline_after_n_returns_not_accepting_jobs() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(3);
        g.apply_faults(&FaultBody {
            offline_after_n: Some(1),
            ..Default::default()
        });
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>100</wscn:Width>
            <wscn:Height>100</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, create_body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{create_body}");
    let job_id = create_body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");
    let token = create_body
        .split("<wscn:JobToken>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobToken");

    let retrieve = |id: &str, tok: &str| {
        format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:ffffffff-ffff-ffff-ffff-ffffffffffff</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{id}</wscn:JobId>
      <wscn:JobToken>{tok}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
        )
    };

    let (st0, body0) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st0, StatusCode::OK, "{body0}");
    assert!(!body0.contains("Fault"), "{body0}");

    let (st1, body1) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st1, StatusCode::INTERNAL_SERVER_ERROR, "{body1}");
    assert!(
        body1.contains("ServerErrorNotAcceptingJobs") || body1.contains("NotAcceptingJobs"),
        "expected NotAcceptingJobs after offline_after_n: {body1}"
    );
    assert!(w.read().await.offline(), "offline sticky after trip");
}

#[tokio::test]
async fn retrieve_image_multi_feed_after_n_returns_multiple_feed_subcode() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(3);
        g.apply_faults(&FaultBody {
            multi_feed_after_n: Some(1),
            ..Default::default()
        });
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>100</wscn:Width>
            <wscn:Height>100</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, create_body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{create_body}");
    let job_id = create_body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");
    let token = create_body
        .split("<wscn:JobToken>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobToken");

    let retrieve = |id: &str, tok: &str| {
        format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbb1</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{id}</wscn:JobId>
      <wscn:JobToken>{tok}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
        )
    };

    let (st0, body0) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st0, StatusCode::OK, "{body0}");

    let (st1, body1) = post_soap(app(w.clone()), &retrieve(job_id, token)).await;
    assert_eq!(st1, StatusCode::INTERNAL_SERVER_ERROR, "{body1}");
    assert!(
        body1.contains("ClientErrorMultipleFeed"),
        "expected MultipleFeed SOAP subcode: {body1}"
    );
    assert!(w.read().await.multi_feed(), "multi_feed sticky after trip");
}

#[tokio::test]
async fn retrieve_image_abort_retrieve_cuts_mtom_stream() {
    use fakescan::state::FaultBody;

    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(2);
        g.apply_faults(&FaultBody {
            abort_retrieve: Some(true),
            abort_retrieve_bytes: Some(256),
            ..Default::default()
        });
    }

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/CreateScanJob</wsa:Action>
    <wsa:MessageID>uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa2</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:CreateScanJobRequest>
      <wscn:ScanTicket>
        <wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution>
            <wscn:Width>100</wscn:Width>
            <wscn:Height>100</wscn:Height>
          </wscn:Resolution>
        </wscn:DocumentParameters>
      </wscn:ScanTicket>
    </wscn:CreateScanJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, create_body) = post_soap(app(w.clone()), create).await;
    assert_eq!(st, StatusCode::OK, "{create_body}");
    let job_id = create_body
        .split("<wscn:JobId>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobId");
    let token = create_body
        .split("<wscn:JobToken>")
        .nth(1)
        .and_then(|s| s.split('<').next())
        .expect("JobToken");

    let retrieve = format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/RetrieveImage</wsa:Action>
    <wsa:MessageID>uuid:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbb2</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wscn:RetrieveImageRequest>
      <wscn:JobId>{job_id}</wscn:JobId>
      <wscn:JobToken>{token}</wscn:JobToken>
    </wscn:RetrieveImageRequest>
  </soap:Body>
</soap:Envelope>"#
    );

    let err = post_soap_body_result(app(w.clone()), &retrieve)
        .await
        .expect_err("MTOM stream should abort mid-send");
    let msg = err.to_string();
    assert!(
        msg.contains("abort_retrieve") || msg.contains("Connection reset") || msg.contains("error"),
        "unexpected abort error: {msg}"
    );
    assert!(w.read().await.abort_retrieve(), "abort stays sticky");
}

// ---- fault plan (mid-scan interruptions) ------------------------------------------------

fn soap_env(action: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/{action}</wsa:Action>
    <wsa:MessageID>uuid:dddddddd-dddd-dddd-dddd-dddddddddddd</wsa:MessageID>
  </soap:Header>
  <soap:Body>{body}</soap:Body>
</soap:Envelope>"#
    )
}

async fn plan_create_job(w: &Arc<RwLock<World>>) -> (String, String) {
    let create = soap_env(
        "CreateScanJob",
        r#"<wscn:CreateScanJobRequest><wscn:ScanTicket><wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format><wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
          </wscn:DocumentParameters></wscn:ScanTicket></wscn:CreateScanJobRequest>"#,
    );
    let (st, b) = post_soap(app(w.clone()), &create).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let pick = |tag: &str| {
        b.split(&format!("<wscn:{tag}>"))
            .nth(1)
            .and_then(|s| s.split('<').next())
            .expect(tag)
            .to_string()
    };
    (pick("JobId"), pick("JobToken"))
}

fn retrieve_xml(id: &str, tok: &str) -> String {
    soap_env(
        "RetrieveImage",
        &format!(
            "<wscn:RetrieveImageRequest><wscn:JobId>{id}</wscn:JobId><wscn:JobToken>{tok}</wscn:JobToken></wscn:RetrieveImageRequest>"
        ),
    )
}

/// Returns (status, declared Content-Length, bytes received, stream ended with an error).
async fn retrieve_stream(
    w: &Arc<RwLock<World>>,
    xml: &str,
) -> (StatusCode, Option<usize>, usize, bool) {
    use futures_util::StreamExt;
    let req = Request::builder()
        .method("POST")
        .uri("/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(Body::from(xml.to_string()))
        .unwrap();
    let res = app(w.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let declared = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    let mut stream = res.into_body().into_data_stream();
    let (mut got, mut errored) = (0usize, false);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => got += b.len(),
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    (status, declared, got, errored)
}

async fn scanner_status_xml(w: &Arc<RwLock<World>>) -> String {
    let req = soap_env(
        "GetScannerElements",
        "<wscn:GetScannerElementsRequest><wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements></wscn:GetScannerElementsRequest>",
    );
    post_soap(app(w.clone()), &req).await.1
}

#[tokio::test]
async fn soap_cover_open_after_n_serves_exactly_n_pages_then_faults() {
    use fakescan::state::FaultBody;
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(4);
        g.apply_faults(&FaultBody {
            cover_open_after_n: Some(2),
            ..Default::default()
        });
    }
    let (id, tok) = plan_create_job(&w).await;
    for n in 1..=2 {
        let (st, declared, got, errored) = retrieve_stream(&w, &retrieve_xml(&id, &tok)).await;
        assert_eq!(st, StatusCode::OK, "page {n}");
        assert!(!errored && declared == Some(got), "page {n} intact");
    }
    let (st, body) = post_soap(app(w.clone()), &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("ClientErrorCoverOpen"), "{body}");
    let status = scanner_status_xml(&w).await;
    assert!(
        status.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
        "{status}"
    );
    assert!(
        status.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
        "{status}"
    );
    assert!(
        status.contains("<wscn:Name>CoverOpen</wscn:Name>"),
        "{status}"
    );
}

#[tokio::test]
async fn soap_mid_page_cover_cut_truncates_page_two_and_keeps_page_one() {
    use fakescan::state::PlanBody;
    let w = world();
    {
        let mut g = w.write().await;
        g.set_pages(4);
        let body: PlanBody = serde_json::from_value(serde_json::json!({
            "faults": [{"kind": "cover_open", "when": {"page": 2, "fraction": 0.5}}]
        }))
        .unwrap();
        g.add_fault_plan(body).unwrap();
    }
    let (id, tok) = plan_create_job(&w).await;
    let (st, declared, got, errored) = retrieve_stream(&w, &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !errored && declared == Some(got),
        "page 1 complete: {declared:?} vs {got}"
    );
    let full = got;

    let (st, declared, got, errored) = retrieve_stream(&w, &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(errored, "stream must error mid-page");
    let declared = declared.expect("Content-Length still advertises the whole page");
    assert!(
        got > 0 && got < declared,
        "truncated: got {got} of {declared}"
    );
    assert!(
        got.abs_diff(declared / 2) <= declared / 10 + 8,
        "cut at about 50%: got {got} of {declared} (page 1 was {full})"
    );
    {
        let g = w.read().await;
        assert!(g.cover_open(), "condition set");
        assert_eq!(
            g.get_job(id.parse().unwrap()).unwrap().page_index,
            1,
            "only page 1 counted"
        );
    }
    let status = scanner_status_xml(&w).await;
    assert!(
        status.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
        "{status}"
    );
    let (st, body) = post_soap(app(w.clone()), &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("ClientErrorCoverOpen"), "{body}");
}

#[tokio::test]
async fn soap_live_jam_now_faults_next_retrieve_of_running_job() {
    use fakescan::state::PlanBody;
    let w = world();
    {
        w.write().await.set_pages(3);
    }
    let (id, tok) = plan_create_job(&w).await;
    let (st, _, _, errored) = retrieve_stream(&w, &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!errored);
    {
        let mut g = w.write().await;
        let body: PlanBody =
            serde_json::from_value(serde_json::json!({"kind": "jam", "when": "now"})).unwrap();
        g.add_fault_plan(body).unwrap();
    }
    let (st, body) = post_soap(app(w.clone()), &retrieve_xml(&id, &tok)).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("ClientErrorMediaJam"), "{body}");
}

#[tokio::test]
async fn soap_timed_cover_shows_in_status_without_any_retrieve() {
    use fakescan::state::PlanBody;
    let w = world();
    {
        w.write().await.set_pages(2);
    }
    let (_id, _tok) = plan_create_job(&w).await;
    {
        let mut g = w.write().await;
        let body: PlanBody = serde_json::from_value(
            serde_json::json!({"kind": "cover_open", "when": {"after_ms": 3_600_000}}),
        )
        .unwrap();
        g.add_fault_plan(body).unwrap();
    }
    let before = scanner_status_xml(&w).await;
    assert!(!before.contains("CoverOpen"), "{before}");
    w.write()
        .await
        .advance_plan_clock(std::time::Duration::from_secs(3_700));
    let after = scanner_status_xml(&w).await;
    assert!(
        after.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
        "{after}"
    );
}

// ---- empty_feeder_brother (real Brother with an empty feeder) -----------------------------

async fn create_with_source(w: &Arc<RwLock<World>>, source: &str) -> (StatusCode, String) {
    let create = soap_env(
        "CreateScanJob",
        &format!(
            r#"<wscn:CreateScanJobRequest><wscn:ScanTicket><wscn:DocumentParameters>
          <wscn:Format>jfif</wscn:Format><wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>{source}</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
          </wscn:DocumentParameters></wscn:ScanTicket></wscn:CreateScanJobRequest>"#
        ),
    );
    post_soap(app(w.clone()), &create).await
}

async fn arm_empty_feeder(w: &Arc<RwLock<World>>, delay_ms: u64) {
    w.write().await.apply_faults(&fakescan::state::FaultBody {
        empty_feeder_brother: Some(true),
        empty_feeder_delay_ms: Some(delay_ms),
        ..Default::default()
    });
}

fn assert_idle_no_conditions(body: &str) {
    assert!(
        body.contains("<wscn:ScannerState>Idle</wscn:ScannerState>"),
        "{body}"
    );
    assert!(
        body.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
        "{body}"
    );
    assert!(!body.contains("InputTrayEmpty"), "{body}");
    assert!(!body.contains("<wscn:DeviceCondition>"), "{body}");
}

#[tokio::test]
async fn empty_feeder_brother_refuses_feeder_job_busy_after_delay() {
    let w = world();
    w.write().await.set_feeder(false);
    arm_empty_feeder(&w, 150).await;
    let (st, body) = post_soap(app(w.clone()), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_idle_no_conditions(&body);

    let t0 = std::time::Instant::now();
    let (st, body) = create_with_source(&w, "Feeder").await;
    assert!(t0.elapsed() >= std::time::Duration::from_millis(150));
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("ServerErrorNotAcceptingJobs"), "{body}");
    assert_eq!(w.read().await.status().active_jobs, 0);

    // Status after the refusal is still Idle with no conditions.
    let (_, body) = post_soap(app(w.clone()), scanner_status_request()).await;
    assert_idle_no_conditions(&body);
}

#[tokio::test]
async fn empty_feeder_brother_does_not_block_other_requests_during_delay() {
    let w = world();
    arm_empty_feeder(&w, 400).await;
    let w2 = w.clone();
    let slow = tokio::spawn(async move { create_with_source(&w2, "Feeder").await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let t0 = std::time::Instant::now();
    let (st, body) = post_soap(app(w.clone()), scanner_status_request()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(250),
        "status poll blocked by the delay"
    );
    let (st, _) = slow.await.unwrap();
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn empty_feeder_brother_with_sheets_scans_normally() {
    let w = world();
    arm_empty_feeder(&w, 150).await;
    w.write().await.set_pages(2);
    let t0 = std::time::Instant::now();
    let (st, body) = create_with_source(&w, "Feeder").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(t0.elapsed() < std::time::Duration::from_millis(150));
}

#[tokio::test]
async fn empty_feeder_brother_glass_job_is_normal() {
    let w = world();
    arm_empty_feeder(&w, 150).await;
    let t0 = std::time::Instant::now();
    let (st, body) = create_with_source(&w, "Platen").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(t0.elapsed() < std::time::Duration::from_millis(150));
}

#[tokio::test]
async fn empty_feeder_unarmed_keeps_quick_generic_failure() {
    let w = world();
    w.write().await.set_feeder(false);
    let t0 = std::time::Instant::now();
    let (st, body) = create_with_source(&w, "Feeder").await;
    assert!(t0.elapsed() < std::time::Duration::from_millis(100));
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("feeder empty"), "{body}");
    assert!(!body.contains("ServerErrorNotAcceptingJobs"), "{body}");
}

#[tokio::test]
async fn empty_feeder_brother_reset_clears_and_status_field_present() {
    let w = world();
    arm_empty_feeder(&w, 150).await;
    let st = w.read().await.status();
    assert!(st.empty_feeder_brother);
    assert_eq!(st.empty_feeder_delay_ms, 150);
    let json = serde_json::to_value(&st).unwrap();
    assert_eq!(json["empty_feeder_brother"], true);
    assert_eq!(json["empty_feeder_delay_ms"], 150);
    w.write().await.reset();
    let json = serde_json::to_value(w.read().await.status()).unwrap();
    assert_eq!(json["empty_feeder_brother"], false);
    assert_eq!(json["empty_feeder_delay_ms"], 10000);
    let (st, body) = create_with_source(&w, "Feeder").await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body.contains("feeder empty"), "{body}");
}
