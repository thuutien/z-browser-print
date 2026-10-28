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
//!   POST /write   {device, url, options}  (sendUrl: we download the file)
//!   POST /convert multipart "json" {device, options} + "blob" (convertAndSendFile, convert)
//! Images and PDFs sent any of these ways are converted to ZPL (see convert.rs).
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

use crate::convert::{self, Kind};
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
        .route("/convert/scan", post(scan_image))
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
    options: Option<serde_json::Value>,
}

impl Envelope {
    fn convert_options(&self) -> convert::Options {
        self.options
            .clone()
            .and_then(|o| serde_json::from_value(o).ok())
            .unwrap_or_default()
    }
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
            "supportedConversions": {
                "png": ["zpl"], "jpg": ["zpl"], "jpeg": ["zpl"], "gif": ["zpl"], "bmp": ["zpl"], "pdf": ["zpl"]
            },
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
        (None, Some(url)) => {
            let url = url.clone();
            blocking(move || convert::fetch(&url)).await?
        }
        (None, None) => return Err(bad_request("nothing to print")),
    };
    if data.is_empty() {
        return Err(bad_request("nothing to print"));
    }

    // A picture or PDF would come out as garbage; turn it into a ZPL graphic first.
    let data = match convert::sniff(&data) {
        Kind::Raw => data,
        _ => {
            let opts = envelope.convert_options();
            blocking(move || convert::to_zpl(&data, &opts)).await?.into_bytes()
        }
    };

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

/// Run slow work (downloads, image conversion) off the async threads.
async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, ZError> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| ZError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad_request)
}

/// convertAndSendFile() / convert(): image or PDF to ZPL, then print it or hand it back.
async fn convert(State(st): State<Arc<AppState>>, req: Request) -> Result<Response, ZError> {
    let (envelope, blob) = read_multipart(req, &st).await?;
    let data = blob.ok_or_else(|| bad_request("no file given"))?;
    let opts = envelope.convert_options();
    let action = opts.action.clone().unwrap_or_else(|| "print".into());
    let zpl = blocking(move || convert::to_zpl(&data, &opts)).await?;

    match action.as_str() {
        "return" => Ok(json_text(json!({ "data": zpl }))),
        "print" => {
            let printer = find(&st, envelope.device.as_ref())?;
            deliver(&st, printer.as_ref(), zpl.as_bytes()).await?;
            tracing::info!(printer = %printer.info().id, bytes = zpl.len(), "printed converted file (BrowserPrint)");
            Ok(json_text(json!({ "status": "printed" })))
        }
        other => Err(ZError(StatusCode::NOT_IMPLEMENTED, format!("convert action {other:?} is not supported"))),
    }
}

async fn scan_image() -> ZError {
    ZError(StatusCode::NOT_IMPLEMENTED, "scanImage is not supported by Z Browser Print".into())
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
