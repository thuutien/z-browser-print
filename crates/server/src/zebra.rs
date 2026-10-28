//! Compatibility with Zebra's BrowserPrint JS library (BrowserPrint-3.x.js), so web
//! apps written for Zebra Browser Print work unchanged.
//!
//! The library calls http://127.0.0.1:9100/ with XMLHttpRequest (https://127.0.0.1:9101/
//! only on Safari, which Linux doesn't have):
//!   GET  /available            -> {"printer": [device, ...]}
//!   GET  /default?type=printer -> device, or "" when there is none
//!   GET  /config               -> {"application": {...}}
//!   POST /write   {device, data} as JSON text, or multipart "json" + "blob" (sendFile)
//!   POST /read    {device}       -> what the printer has said since, or ""
//! Only status 200 counts as success; for anything else the body text goes to the
//! page's error callback.
//!
//! Zebra's app keeps a connection open per device, so `device.sendThenRead("~HS")`
//! works as two calls. We get the same result by collecting the reply whenever a
//! write contains a query command, and handing it out on the next /read.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zprint_core::{Error, PrinterInfo, Transport};

use crate::{AppState, MAX_BODY};

/// Commands that make a Zebra printer answer: ~HS/~HI/~HQ..., ^HH, ^HW, ^HZ, ^HV, ^HT, SGD.
const QUERY_MARKERS: &[&[u8]] = &[b"~H", b"^HH", b"^HW", b"^HZ", b"^HV", b"^HT", b"getvar", b"{}{"];
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const MANUFACTURER: &str = "Zebra Technologies";
const PROVIDER: &str = "zprint";

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/available", get(available))
        .route("/default", get(default_device))
        .route("/config", get(config))
        .route("/write", post(write))
        .route("/read", post(read))
        .route("/convert", post(convert))
        .route("/convert/scan", post(convert))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    name: String,
    device_type: &'static str,
    connection: &'static str,
    uid: String,
    version: u32,
    provider: &'static str,
    manufacturer: &'static str,
}

impl Device {
    fn new(info: &PrinterInfo) -> Self {
        Self {
            name: info.name.clone(),
            device_type: "printer",
            // Zebra calls printers installed in the OS "driver" printers.
            connection: match info.connection {
                "cups" => "driver",
                other => other,
            },
            uid: info.id.clone(),
            version: 2,
            provider: PROVIDER,
            manufacturer: MANUFACTURER,
        }
    }
}

#[derive(Deserialize)]
struct DeviceRef {
    uid: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    device: Option<DeviceRef>,
    data: Option<String>,
    url: Option<String>,
}

/// Error with a plain-text body, which BrowserPrint hands to the error callback.
struct ZError(StatusCode, String);

impl IntoResponse for ZError {
    fn into_response(self) -> Response {
        tracing::warn!(error = %self.1, "BrowserPrint request failed");
        (self.0, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], self.1).into_response()
    }
}

impl From<Error> for ZError {
    fn from(e: Error) -> Self {
        ZError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad_request(msg: impl Into<String>) -> ZError {
    ZError(StatusCode::BAD_REQUEST, msg.into())
}

fn json_text(value: serde_json::Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], value.to_string()).into_response()
}

async fn available(State(st): State<Arc<AppState>>) -> Response {
    let printers: Vec<Device> = st.printers().list.iter().map(|p| Device::new(p.info())).collect();
    json_text(json!({ "printer": printers }))
}

async fn default_device(State(st): State<Arc<AppState>>) -> Response {
    match st.printers().default() {
        Some(p) => json_text(json!(Device::new(p.info()))),
        // The library treats an empty body as "no default printer".
        None => String::new().into_response(),
    }
}

async fn config() -> Response {
    json_text(json!({
        "application": {
            "version": format!("Z Browser Print {}", env!("CARGO_PKG_VERSION")),
            "build_number": 1,
            "api_level": 2,
            "platform": "linux",
            "supportedConversions": {},
        }
    }))
}

fn find(st: &AppState, device: Option<&DeviceRef>) -> Result<Arc<dyn Transport>, ZError> {
    let device = device.ok_or_else(|| bad_request("no device given"))?;
    let printers = st.printers();
    device
        .uid
        .as_deref()
        .and_then(|uid| printers.get(uid))
        .or_else(|| {
            let name = device.name.as_deref()?;
            printers.list.iter().find(|p| p.info().name == name).cloned()
        })
        .ok_or_else(|| {
            bad_request(format!(
                "unknown printer {:?}; pick it again (it may have been removed in Z Browser Print)",
                device.uid.as_deref().or(device.name.as_deref()).unwrap_or("")
            ))
        })
}

async fn write(State(st): State<Arc<AppState>>, req: Request) -> Result<Response, ZError> {
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/form-data"));

    let (envelope, data) = if is_multipart {
        read_multipart(req, &st).await?
    } else {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY)
            .await
            .map_err(|e| bad_request(format!("could not read request: {e}")))?;
        let envelope: Envelope = serde_json::from_slice(&body)
            .map_err(|e| bad_request(format!("request is not BrowserPrint JSON: {e}")))?;
        let data = envelope.data.clone().map(String::into_bytes);
        (envelope, data)
    };

    let printer = find(&st, envelope.device.as_ref())?;
    let data = match (data, &envelope.url) {
        (Some(d), _) => d,
        (None, Some(_)) => {
            return Err(ZError(
                StatusCode::NOT_IMPLEMENTED,
                "sendUrl is not supported by Z Browser Print; fetch the file in the page and use send()".into(),
            ))
        }
        (None, None) => return Err(bad_request("nothing to print")),
    };
    if data.is_empty() {
        return Err(bad_request("nothing to print"));
    }

    deliver(&st, printer.as_ref(), &data).await?;
    tracing::info!(printer = %printer.info().id, bytes = data.len(), "printed (BrowserPrint)");
    Ok(StatusCode::OK.into_response())
}

/// sendFile(): a "json" part with the device and a "blob" part with the data.
async fn read_multipart(req: Request, st: &Arc<AppState>) -> Result<(Envelope, Option<Vec<u8>>), ZError> {
    let mut form = Multipart::from_request(req, st)
        .await
        .map_err(|e| bad_request(format!("bad upload: {e}")))?;
    let mut envelope = None;
    let mut blob = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| bad_request(format!("bad upload: {e}")))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let bytes: Bytes = field
            .bytes()
            .await
            .map_err(|e| bad_request(format!("bad upload: {e}")))?;
        match name.as_str() {
            "json" => {
                envelope = Some(
                    serde_json::from_slice::<Envelope>(&bytes)
                        .map_err(|e| bad_request(format!("bad upload json: {e}")))?,
                )
            }
            "blob" => blob = Some(bytes.to_vec()),
            _ => {}
        }
    }
    let envelope = envelope.ok_or_else(|| bad_request("upload has no json part"))?;
    Ok((envelope, blob))
}

/// Send `data`; when it asks the printer something, keep the answer for /read.
async fn deliver(st: &AppState, printer: &dyn Transport, data: &[u8]) -> Result<(), ZError> {
    if is_query(data) {
        match printer.read(data, REPLY_TIMEOUT).await {
            Ok(reply) => {
                st.replies
                    .lock()
                    .unwrap()
                    .entry(printer.info().id.clone())
                    .or_default()
                    .extend_from_slice(&reply);
                return Ok(());
            }
            // Sent, but the printer had nothing to say.
            Err(Error::Timeout) => return Ok(()),
            // CUPS can't read back; just print.
            Err(Error::Unsupported(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    printer.write(data).await?;
    Ok(())
}

fn is_query(data: &[u8]) -> bool {
    QUERY_MARKERS
        .iter()
        .any(|m| data.windows(m.len()).any(|w| w.eq_ignore_ascii_case(m)))
}

async fn read(State(st): State<Arc<AppState>>, body: Bytes) -> Result<Response, ZError> {
    let envelope: Envelope = serde_json::from_slice(&body)
        .map_err(|e| bad_request(format!("request is not BrowserPrint JSON: {e}")))?;
    let printer = find(&st, envelope.device.as_ref())?;
    let reply = st
        .replies
        .lock()
        .unwrap()
        .remove(&printer.info().id)
        .unwrap_or_default();
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        String::from_utf8_lossy(&reply).into_owned(),
    )
        .into_response())
}

async fn convert() -> ZError {
    ZError(
        StatusCode::NOT_IMPLEMENTED,
        "image and PDF conversion is not supported by Z Browser Print; send ZPL instead".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::is_query;

    #[test]
    fn spots_queries() {
        assert!(is_query(b"~HS"));
        assert!(is_query(b"^XA^HH^XZ"));
        assert!(is_query(b"! U1 getvar \"device.languages\"\r\n"));
        assert!(is_query(b"~hqes"));
        assert!(!is_query(b"^XA^FO50,50^FDHello^FS^XZ"));
    }
}
