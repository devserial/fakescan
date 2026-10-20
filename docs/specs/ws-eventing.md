# WS-Eventing

How a client **registers for notifications** from the Scan Service (status changes, ScanAvailableEvent, end-of-job, …).

## Spec status

| Question | Answer |
|----------|--------|
| IETF RFC? | **No** |
| Wire Windows uses | [WS-Eventing W3C Member Submission](https://www.w3.org/Submission/WS-Eventing/) — namespace `http://schemas.xmlsoap.org/ws/2004/08/eventing` |
| Later | [W3C WS-Eventing Recommendation](https://www.w3.org/TR/ws-eventing/) (different element names in places, e.g. `GrantedExpires`) |
| DPWS | Hosted services that event MUST support Eventing ([dpws.md](dpws.md) §5) |

**Use the 2004/08 Submission shapes for WSDScan compatibility** (`wse:Expires` on SubscribeResponse, not only `GrantedExpires`).

## Why fakescan cares

Lab (2026-10-11): WIA open sequence hit `/ScannerService` with:

1. `GetScannerElements` → **ok**
2. **`Subscribe`** → fakescan faulted `action not implemented`
3. pdfcraft / WIA reported **`WIA_ERROR_OFFLINE` (0x80210005)**

So Eventing is on the **critical path for “device online”**, not an optional push-scan feature. Subscribe is accepted now; that October log is why it stays on the online path.

## Operations (2004/08)

| Action short | Purpose |
|--------------|---------|
| **Subscribe** | Create subscription; returns SubscriptionManager EPR + Expires |
| **Renew** | Extend Expires |
| **Unsubscribe** | Tear down |
| **GetStatus** | Query remaining lifetime |

Subscribe request (conceptual):

```xml
<wse:Subscribe>
  <wse:EndTo>…optional SubscriptionEnd sink…</wse:EndTo>
  <wse:Delivery Mode="…/DeliveryModes/Push">
    <wse:NotifyTo>…client event sink EPR…</wse:NotifyTo>
  </wse:Delivery>
  <wse:Expires>PT1H</wse:Expires>   <!-- xs:duration or dateTime -->
  <wse:Filter Dialect="…">…</wse:Filter>
</wse:Subscribe>
```

Successful **SubscribeResponse**:

```xml
<wse:SubscribeResponse>
  <wse:SubscriptionManager>
    <wsa:Address>http://device/ScannerService</wsa:Address>
    <wsa:ReferenceParameters>
      <wse:Identifier>uuid:…</wse:Identifier>
    </wsa:ReferenceParameters>
  </wse:SubscriptionManager>
  <wse:Expires>PT1H</wse:Expires>
</wse:SubscribeResponse>
```

Action:

```text
http://schemas.xmlsoap.org/ws/2004/08/eventing/SubscribeResponse
```

## Scanner events (Scan Service)

The Scan Service Definition layers **event notifications** (e.g. ScannerElementsChangeEvent, JobStatusEvent, **ScanAvailableEvent** for walk-up scan) on top of this subscription. Push delivery POSTs notification SOAP to `NotifyTo`.

### ScanDestinations extension (Windows / WIA)

**Implemented** in fakescan (`wsd/mod.rs`): when the Subscribe body carries **`wscn:ScanDestinations`**, the SubscribeResponse includes matching **`wscn:DestinationResponses`** / **`DestinationToken`** per `ClientContext` ([DestinationToken](https://learn.microsoft.com/en-us/windows-hardware/drivers/image/destinationtoken)). That token is later presented on device-push `CreateScanJob`; omitting the response can leave WIA treating the scanner as not fully online.

### Footgun: `AttentionRequired` for empty feeder

Do **not** report `ScannerStateReason=AttentionRequired` merely because the ADF is empty. That reason means human intervention; WIA/WSDScan can treat the device as offline and refuse glass acquire. Empty feeder → **Idle** + **None** (jam/cover stay real Attention/Stopped paths). See scanner status mapping in `wsd/elements.rs`.

```xml
<!-- in Subscribe body -->
<wscn:ScanDestinations>
  <wscn:ScanDestination>
    <wscn:ClientDisplayName>Lab PC</wscn:ClientDisplayName>
    <wscn:ClientContext>…</wscn:ClientContext>
  </wscn:ScanDestination>
</wscn:ScanDestinations>

<!-- in SubscribeResponse -->
<wscn:DestinationResponses>
  <wscn:DestinationResponse>
    <wscn:ClientContext>…</wscn:ClientContext>
    <wscn:DestinationToken>fakescan-dest-…</wscn:DestinationToken>
  </wscn:DestinationResponse>
</wscn:DestinationResponses>
```

## fakescan mapping

| Op | Status |
|----|--------|
| Subscribe / Renew / Unsubscribe / GetStatus | **done** — Accept; Identifier + Expires (`PT…` / `P1D` / `P1DT…` / UTC `Z`/`+00:00`); Renew/GetStatus/Unsubscribe check Identifier when present (absent = lab-accept); expired → clear + fault; Filter/@Dialect logged; SubscriptionManager EPR = ScannerService Address + Identifier; single active sub (re-Subscribe replaces) |
| DestinationResponses when ScanDestinations present | **implemented** (stub tokens echoing ClientContext) |
| Notification push to NotifyTo | **done** — JobStatus, JobEnd, condition, status summary, elements change, and ScanAvailableEvent POST when Subscribe stored a NotifyTo; otherwise a no-op. The Address must be `http://` with an IP host equal to the TCP peer that subscribed (IPv4-mapped forms match). Anything else is not stored and not posted; Subscribe still succeeds so WIA does not go offline |
| ScanAvailableEvent → device-push CreateScanJob | **implemented** — `ctl push_scan` mints the identifier and POSTs; CreateScanJob checks ScanIdentifier + DestinationToken |

Filter dialects: DPWS prefers Action filter; Windows may send scan-specific filters — ignore unknown filters in the stub rather than faulting when possible. `soap::eventing_filter_dialect` extracts `@Dialect` for Subscribe logs (Brother compare); the filter body is not evaluated.
