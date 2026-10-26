use crate::{
    ipp::*,
    media::PaperSize,
    store::{now, Bridge, ConnectionEvent, Job, SharedPrinter, ANONYMOUS_DEVICE, MAX_PDF},
};
use axum::{
    body::to_bytes,
    extract::{ConnectInfo, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Server {
    bridge: Arc<Bridge>,
    uploads: Arc<Semaphore>,
}
#[derive(Clone)]
struct IppOutcome {
    operation: Option<u16>,
    status: u16,
    detail: String,
}
#[derive(Clone)]
struct HttpFailureDetail(&'static str);

fn body_error_response(error: axum::Error) -> Response {
    let too_large = std::error::Error::source(&error)
        .is_some_and(|source| source.is::<http_body_util::LengthLimitError>());
    let (status, detail) = if too_large {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "请求超过大小上限（文档最多 100 MiB）",
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "文档传输中断或请求体读取失败，请重新提交",
        )
    };
    let mut response = (status, detail).into_response();
    response.extensions_mut().insert(HttpFailureDetail(detail));
    response
}
pub(crate) fn router(bridge: Arc<Bridge>) -> Router {
    Router::new()
        .route("/ipp/printers/{printer}", post(handle))
        .route("/ipp/printers/{printer}/jobs/{job}", post(handle))
        .route("/ipp/printers/{printer}/driver.ppd", get(download_ppd))
        .with_state(Server {
            bridge: Arc::clone(&bridge),
            uploads: Arc::new(Semaphore::new(4)),
        })
        .layer(middleware::from_fn_with_state(bridge, record_connection))
}
async fn download_ppd(State(server): State<Server>, Path(id): Path<String>) -> Response {
    if !server.bridge.running.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(printer) = server.bridge.printer(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "application/vnd.cups-ppd".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"Print-Bridge-{id}.ppd\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        crate::ppd::generate(&printer),
    )
        .into_response()
}
async fn record_connection(
    State(bridge): State<Arc<Bridge>>,
    request: Request,
    next: Next,
) -> Response {
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let method = request.method().to_string();
    let path = request.uri().path().chars().take(256).collect();
    let timestamp = now();
    let response = next.run(request).await;
    let outcome = response.extensions().get::<IppOutcome>();
    let detail = match (outcome, response.extensions().get::<HttpFailureDetail>()) {
        (Some(outcome), _) => outcome.detail.clone(),
        (None, Some(failure)) => failure.0.to_string(),
        (None, None) => match response.status() {
            StatusCode::UNAUTHORIZED => "认证失败或缺少设备凭据",
            StatusCode::NOT_FOUND => "未找到队列地址，请检查 macOS 的 Queue",
            StatusCode::METHOD_NOT_ALLOWED => "该地址需要 IPP POST 请求",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "请求类型不是 application/ipp",
            StatusCode::PAYLOAD_TOO_LARGE => "请求超过大小限制",
            StatusCode::REQUEST_TIMEOUT => "读取请求超时",
            StatusCode::SERVICE_UNAVAILABLE => "服务未启动或当前繁忙",
            _ => response
                .status()
                .canonical_reason()
                .unwrap_or("未知 HTTP 状态"),
        }
        .to_string(),
    };
    bridge.record_connection_event(ConnectionEvent {
        timestamp,
        client_ip,
        method,
        path,
        operation: outcome.and_then(|outcome| outcome.operation),
        http_status: response.status().as_u16(),
        ipp_status: outcome.map(|outcome| outcome.status),
        detail: detail.chars().take(512).collect(),
    });
    response
}
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"Print Bridge\", charset=\"UTF-8\"",
        )],
        "Authentication required",
    )
        .into_response()
}
async fn handle(State(server): State<Server>, request: Request) -> Response {
    if !server.bridge.running.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let auth = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|b| String::from_utf8(b).ok());
    let credentials = auth.as_deref().and_then(|s| s.split_once(':'));
    let authentication_required = server.bridge.authentication_required();
    if authentication_required
        && request.headers().contains_key(header::AUTHORIZATION)
        && !credentials.is_some_and(|(device, token)| server.bridge.authenticate(device, token))
    {
        return unauthorized();
    }
    let device = if authentication_required {
        credentials.map(|(device, _)| device.to_string())
    } else {
        Some(ANONYMOUS_DEVICE.to_string())
    };
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|s| s.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or("").trim())
        != Some("application/ipp")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Ok(permit) = server.uploads.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let path = request.uri().path().to_string();
    if device.is_none() && path.split('/').count() != 4 {
        return unauthorized();
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
        })
        .unwrap_or("localhost")
        .to_string();
    let limit = if device.is_some() {
        MAX_PDF + MAX_ATTRIBUTE_BYTES + 8
    } else {
        MAX_ATTRIBUTE_BYTES + 8
    };
    let timeout = if device.is_some() { 300 } else { 10 };
    let body = match tokio::time::timeout(
        Duration::from_secs(timeout),
        to_bytes(request.into_body(), limit),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) if device.is_none() => return unauthorized(),
        Ok(Err(error)) => return body_error_response(error),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    if device.is_none()
        && !matches!(parse(&body), Ok(message) if message.code == GET_PRINTER_ATTRIBUTES && message.document.is_empty())
    {
        return unauthorized();
    }
    let bridge = server.bridge;
    let operation = body
        .get(2..4)
        .map(|code| u16::from_be_bytes([code[0], code[1]]));
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        dispatch(
            &bridge,
            device.as_deref().unwrap_or(""),
            &host,
            &path,
            &body,
        )
    })
    .await
    {
        Ok(bytes) => {
            let message = parse(&bytes).expect("encoded IPP response");
            let detail = text(&message, "status-message")
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if message.code >= CLIENT_ERROR_BAD_REQUEST {
                        return "IPP 请求被拒绝".into();
                    }
                    match operation {
                        Some(GET_PRINTER_ATTRIBUTES) => "打印机能力查询成功（尚未提交打印任务）",
                        Some(VALIDATE_JOB) => "打印参数验证通过（尚未提交文档）",
                        Some(PRINT_JOB) => "打印任务已接收",
                        Some(CREATE_JOB) => "已创建等待文档的任务",
                        Some(SEND_DOCUMENT) => "文档已接收",
                        Some(GET_JOB_ATTRIBUTES | GET_JOBS) => "任务状态查询成功",
                        Some(CANCEL_JOB) => "取消请求已处理",
                        _ => "IPP 请求已响应",
                    }
                    .into()
                });
            let outcome = IppOutcome {
                operation,
                status: message.code,
                detail,
            };
            let mut response = ([(header::CONTENT_TYPE, "application/ipp")], bytes).into_response();
            response.extensions_mut().insert(outcome);
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
fn reply(message: &Message<'_>, code: u16, extra: Vec<Group>, detail: &str) -> Vec<u8> {
    let mut op = operation_attributes();
    if !detail.is_empty() {
        op.attributes
            .push(Attribute::text(TEXT, "status-message", detail));
    }
    let mut groups = vec![op];
    groups.extend(extra);
    encode(message.version, code, message.request_id, &groups, &[]).expect("bounded response")
}

fn collection_field(bytes: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
    bytes.push(tag);
    bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
    bytes.extend_from_slice(value);
}

fn media_collection(bytes: &mut Vec<u8>, name: &str, paper: PaperSize) {
    collection_field(bytes, BEGIN_COLLECTION, name, &[]);
    for margin in [
        "media-bottom-margin",
        "media-left-margin",
        "media-right-margin",
        "media-top-margin",
    ] {
        collection_field(bytes, MEMBER_ATTR_NAME, "", margin.as_bytes());
        collection_field(bytes, INTEGER, "", &0i32.to_be_bytes());
    }
    collection_field(bytes, MEMBER_ATTR_NAME, "", b"media-size");
    collection_field(bytes, BEGIN_COLLECTION, "", &[]);
    let (width, height) = paper.dimensions();
    for (member, dimension) in [("x-dimension", width), ("y-dimension", height)] {
        collection_field(bytes, MEMBER_ATTR_NAME, "", member.as_bytes());
        collection_field(bytes, INTEGER, "", &dimension.to_be_bytes());
    }
    collection_field(bytes, END_COLLECTION, "", &[]);
    collection_field(bytes, END_COLLECTION, "", &[]);
}

#[derive(Debug)]
enum CollectionValue {
    Value(u8, Vec<u8>),
    Collection(Vec<(String, CollectionValue)>),
}

fn collection_length(bytes: &[u8], cursor: &mut usize) -> Option<usize> {
    let end = (*cursor).checked_add(2)?;
    let value = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(u16::from_be_bytes([value[0], value[1]]) as usize)
}

fn collection_take<'a>(bytes: &'a [u8], cursor: &mut usize, length: usize) -> Option<&'a [u8]> {
    let end = (*cursor).checked_add(length)?;
    let value = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(value)
}

fn collection_entries(bytes: &[u8], cursor: &mut usize) -> Option<Vec<(String, CollectionValue)>> {
    let mut entries = Vec::new();
    loop {
        let tag = *collection_take(bytes, cursor, 1)?.first()?;
        let name_length = collection_length(bytes, cursor)?;
        let name = collection_take(bytes, cursor, name_length)?;
        let value_length = collection_length(bytes, cursor)?;
        let value = collection_take(bytes, cursor, value_length)?;
        if tag == END_COLLECTION {
            if !name.is_empty() || !value.is_empty() {
                return None;
            }
            return Some(entries);
        }
        if tag != MEMBER_ATTR_NAME || !name.is_empty() {
            return None;
        }
        let member = std::str::from_utf8(value).ok()?.to_owned();
        if member.is_empty() {
            return None;
        }
        let tag = *collection_take(bytes, cursor, 1)?.first()?;
        if collection_length(bytes, cursor)? != 0 {
            return None;
        }
        let value_length = collection_length(bytes, cursor)?;
        let value = collection_take(bytes, cursor, value_length)?;
        let value = if tag == BEGIN_COLLECTION {
            if !value.is_empty() {
                return None;
            }
            CollectionValue::Collection(collection_entries(bytes, cursor)?)
        } else {
            CollectionValue::Value(tag, value.to_vec())
        };
        if entries.iter().any(|(name, _)| name == &member) {
            return None;
        }
        entries.push((member, value));
    }
}

fn media_col_paper(attribute: &Attribute) -> Result<PaperSize, String> {
    if attribute.tag != BEGIN_COLLECTION {
        return Err("media-col must be a collection".into());
    }
    let mut cursor = 0;
    let entries = collection_entries(&attribute.value, &mut cursor)
        .filter(|_| cursor == attribute.value.len())
        .ok_or_else(|| "Invalid media-col collection".to_string())?;
    let mut named = None;
    let mut dimensions = None;
    for (name, value) in entries {
        match (name.as_str(), value) {
            ("media-size-name", CollectionValue::Value(KEYWORD | NAME, bytes)) => {
                let value = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(PaperSize::parse)
                    .ok_or_else(|| "Unsupported media-size-name".to_string())?;
                named = Some(value);
            }
            ("media-size", CollectionValue::Collection(fields)) => {
                let mut width = None;
                let mut height = None;
                if fields
                    .iter()
                    .any(|(name, value)| match (name.as_str(), value) {
                        ("x-dimension", CollectionValue::Value(INTEGER, bytes))
                            if bytes.len() == 4 =>
                        {
                            width = Some(i32::from_be_bytes(bytes.as_slice().try_into().unwrap()));
                            false
                        }
                        ("y-dimension", CollectionValue::Value(INTEGER, bytes))
                            if bytes.len() == 4 =>
                        {
                            height = Some(i32::from_be_bytes(bytes.as_slice().try_into().unwrap()));
                            false
                        }
                        _ => true,
                    })
                {
                    return Err("Unsupported media-size member".into());
                }
                let (width, height) = (width, height);
                let size = crate::media::PAPER_SIZES
                    .into_iter()
                    .find(|paper| Some(paper.dimensions()) == width.zip(height))
                    .ok_or_else(|| "Unsupported media-size dimensions".to_string())?;
                dimensions = Some(size);
            }
            (
                "media-bottom-margin"
                | "media-left-margin"
                | "media-right-margin"
                | "media-top-margin",
                CollectionValue::Value(INTEGER, bytes),
            ) if bytes.as_slice() == 0i32.to_be_bytes().as_slice() => {}
            _ => return Err(format!("Unsupported media-col member: {name}")),
        }
    }
    match (named, dimensions) {
        (Some(name), Some(size)) if name != size => Err("Conflicting media-size members".into()),
        (Some(name), _) => Ok(name),
        (_, Some(size)) => Ok(size),
        _ => Err("media-col is missing media-size".into()),
    }
}

fn resolution_300_dpi(attribute: &Attribute) -> bool {
    let mut expected = Vec::with_capacity(9);
    expected.extend_from_slice(&300i32.to_be_bytes());
    expected.extend_from_slice(&300i32.to_be_bytes());
    expected.push(3);
    attribute.tag == RESOLUTION && attribute.value == expected
}

fn resolution_attribute(name: &str) -> Attribute {
    let mut value = Vec::with_capacity(9);
    value.extend_from_slice(&300i32.to_be_bytes());
    value.extend_from_slice(&300i32.to_be_bytes());
    value.push(3);
    Attribute {
        tag: RESOLUTION,
        name: name.into(),
        value,
    }
}

fn document_mime<'a>(message: &'a Message<'_>) -> &'a str {
    let mut attributes = message.attributes("document-format");
    let Some(first) = attributes.next() else {
        return "application/pdf";
    };
    if first
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
    {
        return attributes
            .find_map(|attribute| {
                attribute
                    .as_str()
                    .filter(|value| !value.eq_ignore_ascii_case("application/octet-stream"))
            })
            .unwrap_or("application/octet-stream");
    }
    first.as_str().unwrap_or("application/pdf")
}

fn supported_document_mime(mime: &str) -> bool {
    [
        "application/pdf",
        "image/jpeg",
        "image/urf",
        "image/pwg-raster",
    ]
    .iter()
    .any(|supported| mime.eq_ignore_ascii_case(supported))
}

fn printer_reply(
    message: &Message<'_>,
    printer: &SharedPrinter,
    uri: &str,
    authentication_required: bool,
) -> Vec<u8> {
    let mut bytes = reply(
        message,
        SUCCESSFUL_OK,
        vec![printer_group(printer, uri, authentication_required)],
        "",
    );
    assert_eq!(bytes.pop(), Some(0x03));
    media_collection(&mut bytes, "media-col-default", printer.default_paper);
    for (index, paper) in printer.paper_sizes.iter().enumerate() {
        media_collection(
            &mut bytes,
            if index == 0 { "media-col-database" } else { "" },
            *paper,
        );
    }
    bytes.push(0x03);
    bytes
}
fn dispatch(bridge: &Bridge, device: &str, host: &str, path: &str, bytes: &[u8]) -> Vec<u8> {
    let message = match parse(bytes) {
        Ok(m) => m,
        Err(error) => {
            let id = bytes
                .get(4..8)
                .and_then(|s| s.try_into().ok())
                .map(i32::from_be_bytes)
                .filter(|n| *n > 0)
                .unwrap_or(1);
            return encode(
                (1, 1),
                CLIENT_ERROR_BAD_REQUEST,
                id,
                &[Group::new(OPERATION_ATTRIBUTES, {
                    let mut attributes = operation_attributes().attributes;
                    attributes.push(Attribute::text(TEXT, "status-message", &error.to_string()));
                    attributes
                })],
                &[],
            )
            .unwrap();
        }
    };
    let fail = |code, detail: &str| reply(&message, code, vec![], detail);
    if !matches!(message.version, (1, 1) | (2, 0)) {
        return fail(
            SERVER_ERROR_VERSION_NOT_SUPPORTED,
            "Only IPP 1.1 and 2.0 are supported",
        );
    }
    if text(&message, "attributes-charset") != Some("utf-8")
        || text(&message, "attributes-natural-language").is_none()
    {
        return fail(CLIENT_ERROR_BAD_REQUEST, "Missing charset or language");
    }
    for name in [
        "printer-uri",
        "job-uri",
        "job-id",
        "copies",
        "sides",
        "media",
        "document-format",
        "media-col",
        "printer-resolution",
        "print-color-mode",
        "ColorModel",
        "print-quality",
        "last-document",
    ] {
        let attributes: Vec<_> = message.attributes(name).collect();
        // macOS appends an octet-stream fallback; accept only this PDF pair and still validate the document.
        let macos_pdf_format = name == "document-format"
            && attributes.len() == 2
            && attributes.iter().all(|a| a.tag == MIME_MEDIA_TYPE)
            && attributes
                .iter()
                .any(|a| a.as_str() == Some("application/pdf"))
            && attributes
                .iter()
                .any(|a| a.as_str() == Some("application/octet-stream"));
        if attributes.len() > 1 && !macos_pdf_format {
            return fail(
                CLIENT_ERROR_BAD_REQUEST,
                "Duplicate single-valued attribute",
            );
        }
    }
    for (name, tag) in [
        ("printer-uri", URI),
        ("job-uri", URI),
        ("attributes-charset", CHARSET),
        ("attributes-natural-language", NATURAL_LANGUAGE),
        ("which-jobs", KEYWORD),
    ] {
        if message
            .attribute(name)
            .is_some_and(|a| a.tag != tag || a.as_str().is_none())
        {
            return fail(CLIENT_ERROR_BAD_REQUEST, "Invalid attribute type");
        }
    }
    for name in ["job-id", "limit"] {
        if message
            .attribute(name)
            .is_some_and(|a| a.tag != INTEGER || a.as_i32().is_none_or(|n| n <= 0))
        {
            return fail(CLIENT_ERROR_BAD_REQUEST, "Invalid integer attribute");
        }
    }
    if message.attribute("last-document").is_some_and(|attribute| {
        attribute.tag != BOOLEAN || attribute.value.len() != 1 || attribute.value[0] > 1
    }) {
        return fail(CLIENT_ERROR_BAD_REQUEST, "Invalid last-document attribute");
    }
    let parts: Vec<_> = path.split('/').collect();
    let Some(printer_id) = parts.get(3) else {
        return fail(CLIENT_ERROR_NOT_FOUND, "Unknown printer");
    };
    let printer = match bridge.printer(printer_id) {
        Ok(p) => p,
        Err(_) => return fail(CLIENT_ERROR_NOT_FOUND, "Unknown printer"),
    };
    let printer_path = format!("/ipp/printers/{}", printer.id);
    let uri = format!("ipp://{host}{printer_path}");
    if let Some(value) = text(&message, "printer-uri") {
        if uri_path(value) != Some(printer_path.as_str()) {
            return fail(CLIENT_ERROR_BAD_REQUEST, "Printer URI does not match route");
        }
    } else if text(&message, "job-uri").is_none() {
        return fail(CLIENT_ERROR_BAD_REQUEST, "Missing target URI");
    }
    if !matches!(message.code, PRINT_JOB | SEND_DOCUMENT) && !message.document.is_empty() {
        return fail(CLIENT_ERROR_BAD_REQUEST, "Unexpected document");
    }
    match message.code {
        GET_PRINTER_ATTRIBUTES => {
            printer_reply(&message, &printer, &uri, bridge.authentication_required())
        }
        VALIDATE_JOB | PRINT_JOB | CREATE_JOB => {
            if parts.len() != 4 {
                return fail(CLIENT_ERROR_BAD_REQUEST, "Expected printer route");
            }
            if !printer.enabled {
                return fail(SERVER_ERROR_SERVICE_UNAVAILABLE, "Printer disabled");
            }
            let (copies, sides, paper, color_mode) = match options(&message, &printer) {
                Ok(o) => o,
                Err((code, s)) => return fail(code, &s),
            };
            if message.code == VALIDATE_JOB {
                return reply(&message, SUCCESSFUL_OK, vec![], "");
            }
            if message.code == CREATE_JOB {
                return match bridge.create_ipp_job(
                    device,
                    &printer.id,
                    copies,
                    sides,
                    paper,
                    &color_mode,
                ) {
                    Ok(job) => reply(&message, SUCCESSFUL_OK, vec![job_group(&job, &uri)], ""),
                    Err(error) => fail(CLIENT_ERROR_NOT_POSSIBLE, &error),
                };
            }
            match bridge.accept_format(
                device,
                &printer.id,
                copies,
                sides,
                paper,
                &color_mode,
                document_mime(&message),
                message.document,
            ) {
                Ok(job) => reply(&message, SUCCESSFUL_OK, vec![job_group(&job, &uri)], ""),
                Err(error) => fail(
                    if error.contains("文档格式错误") {
                        CLIENT_ERROR_DOCUMENT_FORMAT_ERROR
                    } else {
                        CLIENT_ERROR_NOT_POSSIBLE
                    },
                    &error,
                ),
            }
        }
        SEND_DOCUMENT | GET_JOB_ATTRIBUTES | CANCEL_JOB => {
            let attr_id = message
                .attribute("job-id")
                .and_then(|a| a.as_i32())
                .map(i64::from);
            let uri_id = text(&message, "job-uri")
                .and_then(uri_path)
                .and_then(|p| p.strip_prefix(&format!("{printer_path}/jobs/")))
                .and_then(|s| s.parse::<i64>().ok());
            let route_id = parts.get(5).and_then(|s| s.parse::<i64>().ok());
            let Some(id) = attr_id.or(uri_id).or(route_id) else {
                return fail(CLIENT_ERROR_BAD_REQUEST, "Missing job ID");
            };
            if attr_id.is_some_and(|n| n != id)
                || uri_id.is_some_and(|n| n != id)
                || route_id.is_some_and(|n| n != id)
                || text(&message, "job-uri").is_some() && uri_id.is_none()
            {
                return fail(CLIENT_ERROR_BAD_REQUEST, "Conflicting job target");
            }
            let job = match bridge.job(id) {
                Ok(j) if j.device_id == device && j.printer_id == printer.id => j,
                _ => return fail(CLIENT_ERROR_NOT_FOUND, "Unknown job"),
            };
            if message.code == CANCEL_JOB {
                return match bridge.cancel_job(id) {
                    Ok(()) => reply(&message, SUCCESSFUL_OK, vec![], ""),
                    Err(_) => fail(
                        CLIENT_ERROR_NOT_POSSIBLE,
                        "Only pending jobs can be canceled",
                    ),
                };
            }
            if message.code == SEND_DOCUMENT {
                if parts.len() != 4 && parts.len() != 6 {
                    return fail(CLIENT_ERROR_BAD_REQUEST, "Expected job route");
                }
                let last_document = message
                    .attribute("last-document")
                    .map(|attribute| attribute.value.as_slice() == [1])
                    .unwrap_or(false);
                if message.attribute("last-document").is_none() {
                    return fail(CLIENT_ERROR_BAD_REQUEST, "Missing last-document");
                }
                if !supported_document_mime(document_mime(&message)) {
                    return fail(
                        CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED,
                        "Unsupported document-format",
                    );
                }
                if text(&message, "compression").is_some_and(|value| value != "none") {
                    return fail(
                        CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                        "Compression not supported",
                    );
                }
                if !printer.enabled {
                    return fail(SERVER_ERROR_SERVICE_UNAVAILABLE, "Printer disabled");
                }
                return match bridge.send_ipp_document(
                    device,
                    id,
                    message.document,
                    document_mime(&message),
                    last_document,
                ) {
                    Ok(job) => reply(&message, SUCCESSFUL_OK, vec![job_group(&job, &uri)], ""),
                    Err(error) => {
                        let status = if error.contains("不支持一个任务包含多个文档") {
                            SERVER_ERROR_MULTIPLE_DOCUMENT_JOBS_NOT_SUPPORTED
                        } else if error.contains("文档格式错误") {
                            CLIENT_ERROR_DOCUMENT_FORMAT_ERROR
                        } else if error == "任务不存在" {
                            CLIENT_ERROR_NOT_FOUND
                        } else {
                            CLIENT_ERROR_NOT_POSSIBLE
                        };
                        fail(status, &error)
                    }
                };
            }
            reply(&message, SUCCESSFUL_OK, vec![job_group(&job, &uri)], "")
        }
        GET_JOBS => {
            let which = text(&message, "which-jobs").unwrap_or("not-completed");
            if !matches!(which, "all" | "completed" | "not-completed") {
                return fail(
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                    "Unsupported which-jobs",
                );
            }
            let limit = message
                .attribute("limit")
                .and_then(|a| a.as_i32())
                .unwrap_or(20);
            if limit <= 0 {
                return fail(
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                    "Invalid limit",
                );
            }
            let jobs = match bridge.device_jobs(device, &printer.id, which, limit) {
                Ok(j) => j,
                Err(_) => return fail(SERVER_ERROR_INTERNAL_ERROR, "Storage unavailable"),
            };
            let groups = jobs.into_iter().map(|j| job_group(&j, &uri)).collect();
            reply(&message, SUCCESSFUL_OK, groups, "")
        }
        _ => fail(
            SERVER_ERROR_OPERATION_NOT_SUPPORTED,
            "Operation not supported",
        ),
    }
}
fn text<'a>(m: &'a Message<'_>, name: &str) -> Option<&'a str> {
    m.attribute(name).and_then(Attribute::as_str)
}
fn uri_path(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("ipp://")?;
    let (_, path) = rest.split_once('/')?;
    Some(&rest[rest.len() - path.len() - 1..])
}
fn options<'a>(
    m: &'a Message<'_>,
    p: &SharedPrinter,
) -> Result<(u32, &'a str, PaperSize, String), (u16, String)> {
    for (name, tag) in [
        ("document-format", MIME_MEDIA_TYPE),
        ("sides", KEYWORD),
        ("media", KEYWORD),
        ("media-col", BEGIN_COLLECTION),
        ("printer-resolution", RESOLUTION),
        ("print-color-mode", KEYWORD),
        ("print-quality", ENUM),
    ] {
        if m.attribute(name).is_some_and(|attribute| {
            attribute.tag != tag || tag != BEGIN_COLLECTION && attribute.as_str().is_none()
        }) {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                "Invalid print attribute type".into(),
            ));
        }
    }
    if text(m, "compression").is_some_and(|s| s != "none") {
        return Err((
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Compression not supported".into(),
        ));
    }
    for group in &m.groups {
        if group.tag == JOB_ATTRIBUTES {
            if let Some(attribute) = group.attributes.iter().find(|a| {
                // macOS rendering tickets are client metadata; standard IPP options control the gateway.
                !a.name.starts_with("com.apple.print.")
                    && !matches!(
                        a.name.as_str(),
                        "copies"
                            | "sides"
                            | "media"
                            | "media-col"
                            | "printer-resolution"
                            | "print-color-mode"
                            | "print-quality"
                            | "PageSize"
                            | "number-up"
                            | "finishings"
                            | "document-name-supplied"
                            | "job-name"
                            | "job-originating-host-name"
                            | "job-uuid"
                            | "AP_ColorMatchingMode"
                            | "AP_D_InputSlot"
                            | "ColorModel"
                            | "Duplex"
                            | "collate"
                            | "pserrorhandler-requested"
                            | "job-priority"
                            | "job-hold-until"
                            | "job-sheets"
                            | "PaperInfoIsSuggested..b."
                            | "PDFIsProtected..b."
                            | "DestinationPrinterID"
                    )
            }) {
                return Err((
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                    format!("Unsupported job attribute: {}", attribute.name),
                ));
            }
        }
    }
    if !supported_document_mime(document_mime(m)) {
        return Err((
            CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED,
            "Unsupported document-format".into(),
        ));
    }
    let copies = match m.attribute("copies") {
        Some(a) if a.tag == INTEGER => a.as_i32().filter(|n| *n > 0).ok_or((
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Invalid copies".into(),
        ))? as u32,
        Some(_) => {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                "Invalid copies".into(),
            ))
        }
        None => 1,
    };
    let unsupported = |detail: &str| {
        (
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            detail.to_owned(),
        )
    };
    let duplex_sides = |value| match value {
        "None" => Some("one-sided"),
        "DuplexNoTumble" => Some("two-sided-long-edge"),
        "DuplexTumble" => Some("two-sided-short-edge"),
        _ => None,
    };
    let native_sides = text(m, "Duplex")
        .map(|value| duplex_sides(value).ok_or_else(|| unsupported("Unsupported value for Duplex")))
        .transpose()?;
    let sides = text(m, "sides").or(native_sides).unwrap_or("one-sided");
    if m.attributes("Duplex")
        .any(|a| a.tag != NAME || a.as_str().and_then(duplex_sides) != Some(sides))
    {
        return Err(unsupported("Unsupported or conflicting value for Duplex"));
    }
    let media = text(m, "media")
        .map(|value| PaperSize::parse(value).ok_or_else(|| unsupported("Unsupported media")))
        .transpose()?;
    let page_size = text(m, "PageSize")
        .map(|value| PaperSize::parse(value).ok_or_else(|| unsupported("Unsupported PageSize")))
        .transpose()?;
    let paper = media.or(page_size).unwrap_or(p.default_paper);
    if media.is_some() && page_size.is_some() && media != page_size {
        return Err(unsupported("Conflicting media and PageSize"));
    }
    if m.attributes("PageSize")
        .any(|a| a.tag != NAME || a.as_str().and_then(PaperSize::parse) != Some(paper))
    {
        return Err(unsupported("Unsupported or conflicting PageSize"));
    }
    let media_col = m
        .attribute("media-col")
        .map(media_col_paper)
        .transpose()
        .map_err(|detail| unsupported(&detail))?;
    if (media.is_some() || page_size.is_some()) && media_col.is_some_and(|value| value != paper) {
        return Err(unsupported("Conflicting media-col and media"));
    }
    let paper = media_col.unwrap_or(paper);
    for (name, tag, value) in [
        ("AP_ColorMatchingMode", NAME, "AP_ApplicationColorMatching"),
        ("AP_D_InputSlot", NAME, ""),
        ("pserrorhandler-requested", NAME, "standard"),
        ("job-hold-until", KEYWORD, "no-hold"),
    ] {
        if m.attributes(name)
            .any(|a| a.tag != tag || a.as_str() != Some(value))
        {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                format!("Unsupported value for {name}"),
            ));
        }
    }
    let native_color_mode = text(m, "ColorModel")
        .map(|value| match value {
            "Gray" => Ok("monochrome"),
            "RGB" => Ok("color"),
            _ => Err(unsupported("Unsupported value for ColorModel")),
        })
        .transpose()?;
    if m.attributes("ColorModel").any(|attribute| {
        attribute.tag != NAME || !matches!(attribute.as_str(), Some("Gray") | Some("RGB"))
    }) {
        return Err(unsupported("Unsupported value for ColorModel"));
    }
    for (name, unsupported) in [
        (
            "collate",
            m.attributes("collate")
                .any(|a| a.tag != BOOLEAN || copies != 1),
        ),
        (
            "job-priority",
            m.attributes("job-priority")
                .any(|a| a.tag != INTEGER || a.as_i32() != Some(50)),
        ),
        (
            "job-sheets",
            m.attributes("job-sheets")
                .any(|a| !matches!(a.tag, NAME | KEYWORD) || a.as_str() != Some("none")),
        ),
        (
            "PaperInfoIsSuggested..b.",
            m.attributes("PaperInfoIsSuggested..b.")
                .any(|a| a.tag != BOOLEAN),
        ),
        (
            "PDFIsProtected..b.",
            m.attributes("PDFIsProtected..b.")
                .any(|a| a.tag != BOOLEAN || a.value.as_slice() != [0]),
        ),
        (
            "DestinationPrinterID",
            m.attributes("DestinationPrinterID")
                .any(|a| a.tag != NAME || a.as_str().is_none()),
        ),
    ] {
        if unsupported {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                format!("Unsupported value for {name}"),
            ));
        }
    }
    if m.attributes("number-up")
        .any(|a| a.tag != INTEGER || a.as_i32() != Some(1))
        || m.attributes("finishings")
            .any(|a| a.tag != ENUM || a.as_i32() != Some(3))
    {
        return Err((
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Unsupported page size, number-up or finishing".into(),
        ));
    }
    if m.attributes("printer-resolution")
        .any(|attribute| !resolution_300_dpi(attribute))
    {
        return Err(unsupported("Unsupported printer-resolution"));
    }
    if m.attributes("print-color-mode").any(|attribute| {
        !matches!(
            attribute.as_str(),
            Some("auto") | Some("color") | Some("monochrome")
        )
    }) {
        return Err(unsupported("Unsupported print-color-mode"));
    }
    let ipp_color_mode = text(m, "print-color-mode");
    if ipp_color_mode.is_some()
        && native_color_mode.is_some_and(|native| Some(native) != ipp_color_mode)
    {
        return Err(unsupported("Conflicting print-color-mode and ColorModel"));
    }
    let color_mode = ipp_color_mode
        .or(native_color_mode)
        .unwrap_or("auto")
        .to_owned();
    if m.attributes("print-quality")
        .any(|attribute| attribute.as_i32() != Some(4))
    {
        return Err(unsupported("Unsupported print-quality"));
    }
    for name in ["page-ranges", "orientation-requested", "output-bin"] {
        if m.attribute(name).is_some() {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                format!("Unsupported print option: {name}"),
            ));
        }
    }
    Bridge::validate_options(p, copies, sides, paper, &color_mode).map_err(|error| {
        (
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            format!("Unsupported copies, sides or media: {error}"),
        )
    })?;
    Ok((copies, sides, paper, color_mode))
}
fn printer_group(p: &SharedPrinter, uri: &str, authentication_required: bool) -> Group {
    let uuid = format!("urn:uuid:{}", crate::discovery::printer_uuid(&p.id));
    let mut a = vec![
        Attribute::text(URI, "printer-uri-supported", uri),
        Attribute::text(URI, "printer-uuid", &uuid),
        Attribute::text(
            KEYWORD,
            "uri-authentication-supported",
            if authentication_required {
                "basic"
            } else {
                "none"
            },
        ),
        Attribute::text(KEYWORD, "uri-security-supported", "none"),
        Attribute::text(NAME, "printer-name", &p.name),
        Attribute::text(TEXT, "printer-info", &p.name),
        Attribute::text(TEXT, "printer-make-and-model", "Print Bridge PDF gateway"),
        Attribute::integer(ENUM, "printer-state", if p.enabled { 3 } else { 5 }),
        Attribute::text(
            KEYWORD,
            "printer-state-reasons",
            if p.enabled { "none" } else { "paused" },
        ),
        Attribute::boolean("printer-is-accepting-jobs", p.enabled),
        Attribute::text(KEYWORD, "ipp-versions-supported", "1.1"),
        Attribute::text(KEYWORD, "ipp-versions-supported", "2.0"),
        Attribute::text(CHARSET, "charset-configured", "utf-8"),
        Attribute::text(CHARSET, "charset-supported", "utf-8"),
        Attribute::text(NATURAL_LANGUAGE, "natural-language-configured", "en"),
        Attribute::text(
            NATURAL_LANGUAGE,
            "generated-natural-language-supported",
            "en",
        ),
        Attribute::text(
            MIME_MEDIA_TYPE,
            "document-format-supported",
            "application/pdf",
        ),
        Attribute::text(
            MIME_MEDIA_TYPE,
            "document-format-default",
            "application/pdf",
        ),
        Attribute::text(KEYWORD, "media-default", p.default_paper.keyword()),
        Attribute::integer(INTEGER, "media-bottom-margin-supported", 0),
        Attribute::integer(INTEGER, "media-left-margin-supported", 0),
        Attribute::integer(INTEGER, "media-right-margin-supported", 0),
        Attribute::integer(INTEGER, "media-top-margin-supported", 0),
        Attribute::text(KEYWORD, "sides-default", "one-sided"),
        Attribute::integer(INTEGER, "copies-default", 1),
        Attribute::boolean("multiple-document-jobs-supported", false),
        Attribute::integer(INTEGER, "multiple-operation-time-out", 120),
        Attribute::text(KEYWORD, "print-color-mode-default", "auto"),
        Attribute::text(KEYWORD, "print-color-mode-supported", "auto"),
        Attribute::text(KEYWORD, "print-color-mode-supported", "color"),
        Attribute::text(KEYWORD, "print-color-mode-supported", "monochrome"),
        Attribute::boolean("color-supported", true),
        Attribute::integer(ENUM, "print-quality-default", 4),
        Attribute::integer(ENUM, "print-quality-supported", 4),
        Attribute::text(KEYWORD, "compression-supported", "none"),
        Attribute::text(KEYWORD, "pwg-raster-document-type-supported", "sgray_8"),
        Attribute::text(KEYWORD, "pwg-raster-document-type-supported", "srgb_8"),
    ];
    let format_default = a
        .iter()
        .position(|attribute| attribute.name == "document-format-default")
        .unwrap();
    let formats = ["image/jpeg", "image/urf", "image/pwg-raster"]
        .into_iter()
        .map(|format| Attribute::text(MIME_MEDIA_TYPE, "document-format-supported", format))
        .collect::<Vec<_>>();
    a.splice(format_default..format_default, formats);
    for format in urf_tokens(p.duplex) {
        a.push(Attribute::text(KEYWORD, "urf-supported", format));
    }
    if p.duplex {
        a.push(Attribute::text(
            KEYWORD,
            "pwg-raster-document-sheet-back",
            "normal",
        ));
    }
    for member in [
        "media-size",
        "media-size-name",
        "media-bottom-margin",
        "media-left-margin",
        "media-right-margin",
        "media-top-margin",
    ] {
        a.push(Attribute::text(KEYWORD, "media-col-supported", member));
    }
    a.push(resolution_attribute("printer-resolution-default"));
    a.push(resolution_attribute("printer-resolution-supported"));
    a.push(resolution_attribute(
        "pwg-raster-document-resolution-supported",
    ));
    for paper in &p.paper_sizes {
        a.push(Attribute::text(KEYWORD, "media-supported", paper.keyword()));
    }
    a.push(Attribute {
        tag: RANGE_OF_INTEGER,
        name: "copies-supported".into(),
        value: [1i32.to_be_bytes(), (p.max_copies as i32).to_be_bytes()].concat(),
    });
    for op in [
        PRINT_JOB,
        VALIDATE_JOB,
        CREATE_JOB,
        SEND_DOCUMENT,
        CANCEL_JOB,
        GET_JOB_ATTRIBUTES,
        GET_JOBS,
        GET_PRINTER_ATTRIBUTES,
    ] {
        a.push(Attribute::integer(ENUM, "operations-supported", op as i32));
    }
    a.push(Attribute::text(KEYWORD, "sides-supported", "one-sided"));
    if p.duplex {
        for s in ["two-sided-long-edge", "two-sided-short-edge"] {
            a.push(Attribute::text(KEYWORD, "sides-supported", s));
        }
    }
    Group::new(PRINTER_ATTRIBUTES, a)
}
fn job_group(j: &Job, printer_uri: &str) -> Group {
    let (state, reason) = match j.state.as_str() {
        "pending" => (3, "none"),
        "pending-document" | "pending-final-document" => (3, "job-incoming"),
        "processing" => (5, "job-printing"),
        "submitted" => (9, "queued-in-device"),
        "simulated" => (9, "job-completed-successfully"),
        "completed" => (9, "job-completed-successfully"),
        "failed" => (8, "aborted-by-system"),
        "uncertain" => (6, "job-stopped"),
        "canceled" => (7, "job-canceled-by-user"),
        _ => (6, "job-stopped"),
    };
    Group::new(
        JOB_ATTRIBUTES,
        vec![
            Attribute::integer(INTEGER, "job-id", j.id as i32),
            Attribute::text(URI, "job-uri", &format!("{printer_uri}/jobs/{}", j.id)),
            Attribute::text(URI, "job-printer-uri", printer_uri),
            Attribute::integer(ENUM, "job-state", state),
            Attribute::text(KEYWORD, "job-state-reasons", reason),
            Attribute::text(TEXT, "job-state-message", &j.detail),
            Attribute::text(NAME, "job-originating-user-name", &j.device_id),
            Attribute::integer(INTEGER, "copies", j.copies as i32),
            Attribute::text(KEYWORD, "sides", &j.sides),
            Attribute::text(KEYWORD, "media", j.paper.keyword()),
            Attribute::text(KEYWORD, "print-color-mode", &j.color_mode),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        tests::{fixture, pdf_bytes},
        CreatedDevice,
    };
    use axum::body::{Body, Bytes};
    use std::{
        convert::Infallible,
        pin::Pin,
        task::{Context, Poll},
    };
    use tower::ServiceExt;

    struct UnreadBody;
    impl http_body::Body for UnreadBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            panic!("authentication must complete before reading the body")
        }
    }

    struct InterruptedBody;
    impl http_body::Body for InterruptedBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated interrupted upload",
            ))))
        }
    }

    #[test]
    fn interrupted_upload_is_not_reported_as_a_size_limit_error() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::new(InterruptedBody))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let event = bridge.connection_events().remove(0);
            assert_eq!(event.http_status, 400);
            assert!(event.detail.contains("传输中断"));
            assert!(!event.detail.contains("大小"));
            assert!(bridge.list_jobs().unwrap().is_empty());
            let error = to_bytes(Body::from(vec![0; 9]), 8).await.unwrap_err();
            let response = body_error_response(error);
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert!(response
                .extensions()
                .get::<HttpFailureDetail>()
                .unwrap()
                .0
                .contains("100 MiB"));
        });
    }

    fn request_bytes(operation: u16, printer: &str, extra: Vec<Attribute>, pdf: &[u8]) -> Vec<u8> {
        let mut group = operation_attributes();
        group.attributes.push(Attribute::text(
            URI,
            "printer-uri",
            &format!("ipp://localhost/ipp/printers/{printer}"),
        ));
        group.attributes.extend(extra);
        encode((1, 1), operation, 17, &[group], pdf).unwrap()
    }

    fn media_col(paper: PaperSize) -> Attribute {
        let mut bytes = Vec::new();
        collection_field(&mut bytes, MEMBER_ATTR_NAME, "", b"media-size");
        collection_field(&mut bytes, BEGIN_COLLECTION, "", &[]);
        let (width, height) = paper.dimensions();
        for (name, value) in [
            ("x-dimension", width.to_be_bytes()),
            ("y-dimension", height.to_be_bytes()),
        ] {
            collection_field(&mut bytes, MEMBER_ATTR_NAME, "", name.as_bytes());
            collection_field(&mut bytes, INTEGER, "", &value);
        }
        collection_field(&mut bytes, END_COLLECTION, "", &[]);
        collection_field(&mut bytes, END_COLLECTION, "", &[]);
        Attribute {
            tag: BEGIN_COLLECTION,
            name: "media-col".into(),
            value: bytes,
        }
    }

    #[test]
    fn native_and_ipp_paper_and_duplex_options_agree_and_persist() {
        let (_directory, bridge, device) = fixture();
        let mut printer = bridge.printer("office").unwrap();
        printer.paper_sizes = crate::media::PAPER_SIZES.to_vec();
        printer.default_paper = PaperSize::Letter;
        bridge.save_printer(printer.clone()).unwrap();
        let defaults = request_bytes(VALIDATE_JOB, "office", vec![], &[]);
        let message = parse(&defaults).unwrap();
        assert_eq!(
            options(&message, &printer).unwrap(),
            (1, "one-sided", PaperSize::Letter, "auto".into())
        );
        let media_col_request =
            request_bytes(VALIDATE_JOB, "office", vec![media_col(PaperSize::A5)], &[]);
        assert_eq!(
            options(&parse(&media_col_request).unwrap(), &printer).unwrap(),
            (1, "one-sided", PaperSize::A5, "auto".into())
        );
        for paper in crate::media::PAPER_SIZES {
            for (sides, native) in [
                ("one-sided", "None"),
                ("two-sided-long-edge", "DuplexNoTumble"),
                ("two-sided-short-edge", "DuplexTumble"),
            ] {
                for attributes in [
                    vec![
                        Attribute::text(KEYWORD, "media", paper.keyword()),
                        Attribute::text(KEYWORD, "sides", sides),
                    ],
                    vec![
                        Attribute::text(NAME, "PageSize", paper.name()),
                        Attribute::text(NAME, "Duplex", native),
                    ],
                    vec![
                        Attribute::text(KEYWORD, "media", paper.name()),
                        Attribute::text(NAME, "PageSize", paper.name()),
                        Attribute::text(KEYWORD, "sides", sides),
                        Attribute::text(NAME, "Duplex", native),
                    ],
                ] {
                    let bytes = request_bytes(VALIDATE_JOB, "office", attributes, &[]);
                    assert_eq!(
                        options(&parse(&bytes).unwrap(), &printer).unwrap(),
                        (1, sides, paper, "auto".into())
                    );
                }
            }
        }
        let request = request_bytes(
            PRINT_JOB,
            "office",
            vec![
                Attribute::text(KEYWORD, "media", "na_letter_8.5x11in"),
                Attribute::text(NAME, "PageSize", "Letter"),
                Attribute::text(KEYWORD, "sides", "two-sided-short-edge"),
                Attribute::text(NAME, "Duplex", "DuplexTumble"),
                Attribute::integer(INTEGER, "copies", 2),
                Attribute::text(KEYWORD, "print-color-mode", "color"),
            ],
            &pdf_bytes(),
        );
        let bytes = dispatch(
            &bridge,
            &device.device.id,
            "localhost",
            "/ipp/printers/office",
            &request,
        );
        let response = parse(&bytes).unwrap();
        assert_eq!(response.code, SUCCESSFUL_OK);
        assert_eq!(text(&response, "media"), Some(PaperSize::Letter.keyword()));
        let job = bridge.list_jobs().unwrap().remove(0);
        assert_eq!(job.paper, PaperSize::Letter);
        assert_eq!(job.sides, "two-sided-short-edge");
        assert_eq!(job.copies, 2);
        assert_eq!(job.color_mode, "color");
        assert_eq!(text(&response, "print-color-mode"), Some("color"));
    }

    #[test]
    fn validates_standard_and_legacy_ppd_color_modes() {
        let (_directory, bridge, _) = fixture();
        let printer = bridge.printer("office").unwrap();
        for mode in ["auto", "color", "monochrome"] {
            let bytes = request_bytes(
                VALIDATE_JOB,
                "office",
                vec![Attribute::text(KEYWORD, "print-color-mode", mode)],
                &[],
            );
            assert_eq!(options(&parse(&bytes).unwrap(), &printer).unwrap().3, mode);
        }
        let gray = request_bytes(
            VALIDATE_JOB,
            "office",
            vec![Attribute::text(NAME, "ColorModel", "Gray")],
            &[],
        );
        assert_eq!(
            options(&parse(&gray).unwrap(), &printer).unwrap().3,
            "monochrome"
        );
        let rgb = request_bytes(
            VALIDATE_JOB,
            "office",
            vec![Attribute::text(NAME, "ColorModel", "RGB")],
            &[],
        );
        assert_eq!(options(&parse(&rgb).unwrap(), &printer).unwrap().3, "color");
        for attributes in [
            vec![
                Attribute::text(KEYWORD, "print-color-mode", "color"),
                Attribute::text(NAME, "ColorModel", "Gray"),
            ],
            vec![
                Attribute::text(KEYWORD, "print-color-mode", "auto"),
                Attribute::text(NAME, "ColorModel", "RGB"),
            ],
            vec![Attribute::text(KEYWORD, "print-color-mode", "draft")],
        ] {
            assert!(options(
                &parse(&request_bytes(VALIDATE_JOB, "office", attributes, &[])).unwrap(),
                &printer
            )
            .is_err());
        }
    }

    #[test]
    fn rejects_conflicting_unknown_or_unconfigured_paper_and_duplex() {
        let (_directory, bridge, _) = fixture();
        let mut printer = bridge.printer("office").unwrap();
        for attributes in [
            vec![
                Attribute::text(KEYWORD, "media", "A4"),
                Attribute::text(NAME, "PageSize", "A5"),
            ],
            vec![Attribute::text(NAME, "PageSize", "A5")],
            vec![Attribute::text(KEYWORD, "media", "custom_10x20mm")],
            vec![
                Attribute::text(NAME, "PageSize", "A4"),
                Attribute::text(NAME, "PageSize", "Letter"),
            ],
            vec![Attribute::integer(INTEGER, "PageSize", 1)],
            vec![
                Attribute::text(KEYWORD, "sides", "one-sided"),
                Attribute::text(NAME, "Duplex", "DuplexNoTumble"),
            ],
            vec![
                Attribute::text(NAME, "Duplex", "None"),
                Attribute::text(NAME, "Duplex", "DuplexTumble"),
            ],
            vec![Attribute::text(KEYWORD, "Duplex", "None")],
        ] {
            let bytes = request_bytes(VALIDATE_JOB, "office", attributes, &[]);
            assert_eq!(
                options(&parse(&bytes).unwrap(), &printer).unwrap_err().0,
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED
            );
        }
        printer.duplex = false;
        for attributes in [
            vec![Attribute::text(KEYWORD, "sides", "two-sided-long-edge")],
            vec![Attribute::text(NAME, "Duplex", "DuplexTumble")],
        ] {
            let bytes = request_bytes(VALIDATE_JOB, "office", attributes, &[]);
            assert!(options(&parse(&bytes).unwrap(), &printer).is_err());
        }
        assert!(bridge.list_jobs().unwrap().is_empty());
    }

    #[test]
    fn ppd_download_matches_capabilities_without_credentials() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, _) = fixture();
            let mut printer = bridge.printer("office").unwrap();
            printer.paper_sizes = vec![PaperSize::A5, PaperSize::Letter];
            printer.default_paper = PaperSize::A5;
            for duplex in [false, true] {
                printer.duplex = duplex;
                bridge.save_printer(printer.clone()).unwrap();
                let response = router(Arc::clone(&bridge))
                    .oneshot(
                        Request::builder()
                            .uri("/ipp/printers/office/driver.ppd")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "application/vnd.cups-ppd"
                );
                let content = to_bytes(response.into_body(), 65536).await.unwrap();
                let ppd = std::str::from_utf8(&content).unwrap();
                assert!(ppd.contains("*DefaultPageSize: A5"));
                assert!(ppd.contains("*PageSize Letter:"));
                assert!(!ppd.contains("*PageSize A4:"));
                assert_eq!(ppd.contains("*Duplex DuplexTumble/"), duplex);
                assert!(!ppd.contains(&printer.queue));
            }
            let response = router(Arc::clone(&bridge))
                .oneshot(
                    Request::builder()
                        .uri("/ipp/printers/missing/driver.ppd")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        });
    }

    async fn call(
        bridge: &Arc<Bridge>,
        device: &CreatedDevice,
        printer: &str,
        operation: u16,
        attributes: Vec<Attribute>,
        pdf: &[u8],
    ) -> Vec<u8> {
        let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
        let request = Request::builder()
            .method("POST")
            .uri(format!("/ipp/printers/{printer}"))
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/ipp")
            .header(header::AUTHORIZATION, format!("Basic {auth}"))
            .body(Body::from(request_bytes(
                operation, printer, attributes, pdf,
            )))
            .unwrap();
        let response = router(Arc::clone(bridge)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/ipp");
        to_bytes(response.into_body(), MAX_PDF)
            .await
            .unwrap()
            .to_vec()
    }

    #[test]
    fn permits_unauthenticated_capability_queries_only() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, _) = fixture();
            for operation in [GET_PRINTER_ATTRIBUTES, PRINT_JOB] {
                let request = Request::builder()
                    .method("POST")
                    .uri("/ipp/printers/office")
                    .header(header::HOST, "localhost")
                    .header(header::CONTENT_TYPE, "application/ipp")
                    .body(Body::from(request_bytes(operation, "office", vec![], &[])))
                    .unwrap();
                let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
                if operation == PRINT_JOB {
                    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                    assert!(bridge.list_jobs().unwrap().is_empty());
                } else {
                    assert_eq!(response.status(), StatusCode::OK);
                    let bytes = to_bytes(response.into_body(), MAX_ATTRIBUTE_BYTES)
                        .await
                        .unwrap();
                    let message = parse(&bytes).unwrap();
                    assert_eq!(message.code, SUCCESSFUL_OK);
                    assert_eq!(
                        text(&message, "document-format-supported"),
                        Some("application/pdf")
                    );
                }
            }
        });
    }

    #[test]
    fn records_ipp_rejection_with_client_ip_without_credentials_or_document_metadata() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let authorization = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office?access_token=private-query-value")
                .extension(ConnectInfo(
                    "192.0.2.10:54321".parse::<SocketAddr>().unwrap(),
                ))
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {authorization}"))
                .body(Body::from(request_bytes(
                    PRINT_JOB,
                    "office",
                    vec![
                        Attribute::text(KEYWORD, "media", "na_letter_8.5x11in"),
                        Attribute::text(NAME, "job-name", "private-document-title"),
                    ],
                    &pdf_bytes(),
                )))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let events = bridge.connection_events();
            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert_eq!(event.client_ip.as_deref(), Some("192.0.2.10"));
            assert_eq!(event.path, "/ipp/printers/office");
            assert_eq!(event.operation, Some(PRINT_JOB));
            assert_eq!(event.http_status, 200);
            assert_eq!(
                event.ipp_status,
                Some(CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED)
            );
            assert!(event.detail.contains("media"));
            let serialized = serde_json::to_string(&events).unwrap();
            for secret in [
                &device.token,
                &authorization,
                "private-query-value",
                "private-document-title",
            ] {
                assert!(!serialized.contains(secret));
            }
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }

    #[test]
    fn records_wrong_routes_and_methods_without_reading_request_body() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, _) = fixture();
            for (method, path, status) in [
                (
                    "GET",
                    "/ipp/printers/office",
                    StatusCode::METHOD_NOT_ALLOWED,
                ),
                ("POST", "/wrong-queue-path", StatusCode::NOT_FOUND),
            ] {
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::new(UnreadBody))
                    .unwrap();
                let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
                assert_eq!(response.status(), status);
                let event = bridge.connection_events().remove(0);
                assert_eq!(event.http_status, status.as_u16());
                assert_eq!(event.path, path);
                assert_eq!(event.ipp_status, None);
            }
            assert_eq!(bridge.connection_events().len(), 2);
        });
    }

    #[test]
    fn accepts_authenticated_ipp_2_pdf_job() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let mut bytes = request_bytes(PRINT_JOB, "office", vec![], &pdf_bytes());
            bytes[0..2].copy_from_slice(&[2, 0]);
            let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::from(bytes))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), MAX_ATTRIBUTE_BYTES)
                .await
                .unwrap();
            let message = parse(&bytes).unwrap();
            assert_eq!(message.version, (2, 0));
            assert_eq!(message.code, SUCCESSFUL_OK);
            assert_eq!(bridge.list_jobs().unwrap().len(), 1);
        });
    }

    #[test]
    fn disabled_authentication_accepts_anonymous_pdf_and_ignores_cached_credentials() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, _) = fixture();
            bridge.running.store(false, Ordering::SeqCst);
            bridge.set_authentication_required(false).unwrap();
            bridge.running.store(true, Ordering::SeqCst);
            for auth in [None, Some("Basic invalid")] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/ipp/printers/office")
                    .header(header::HOST, "localhost")
                    .header(header::CONTENT_TYPE, "application/ipp");
                if let Some(auth) = auth {
                    request = request.header(header::AUTHORIZATION, auth);
                }
                let response = router(Arc::clone(&bridge))
                    .oneshot(
                        request
                            .body(Body::from(request_bytes(
                                PRINT_JOB,
                                "office",
                                vec![],
                                &pdf_bytes(),
                            )))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = to_bytes(response.into_body(), MAX_ATTRIBUTE_BYTES)
                    .await
                    .unwrap();
                assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            }
            let jobs = bridge.list_jobs().unwrap();
            assert_eq!(jobs.len(), 2);
            assert!(jobs.iter().all(|job| job.device_id == ANONYMOUS_DEVICE));
            let query = request_bytes(GET_PRINTER_ATTRIBUTES, "office", vec![], &[]);
            let bytes = dispatch(
                &bridge,
                ANONYMOUS_DEVICE,
                "localhost",
                "/ipp/printers/office",
                &query,
            );
            assert_eq!(
                text(&parse(&bytes).unwrap(), "uri-authentication-supported"),
                Some("none")
            );
            bridge.running.store(false, Ordering::SeqCst);
            bridge.set_authentication_required(true).unwrap();
            bridge.running.store(true, Ordering::SeqCst);
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::CONTENT_TYPE, "application/ipp")
                .body(Body::from(request_bytes(
                    PRINT_JOB,
                    "office",
                    vec![],
                    &pdf_bytes(),
                )))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(bridge.list_jobs().unwrap().len(), 2);
        });
    }

    #[test]
    fn native_macos_defaults_accept_pdf_but_still_validate_document_bytes() {
        let (_directory, bridge, device) = fixture();
        let mut operation = operation_attributes();
        operation.attributes.extend([
            Attribute::text(URI, "printer-uri", "ipp://localhost/ipp/printers/office"),
            Attribute::text(MIME_MEDIA_TYPE, "document-format", "application/pdf"),
            Attribute::text(
                MIME_MEDIA_TYPE,
                "document-format",
                "application/octet-stream",
            ),
        ]);
        let job = Group::new(
            JOB_ATTRIBUTES,
            vec![
                Attribute::text(NAME, "document-name-supplied", "test.pdf"),
                Attribute::integer(ENUM, "finishings", 3),
                Attribute::text(NAME, "job-originating-host-name", "localhost"),
                Attribute::text(URI, "job-uuid", "urn:uuid:compatibility-test"),
                Attribute::text(KEYWORD, "media", "A4"),
                Attribute::integer(INTEGER, "number-up", 1),
                Attribute::text(NAME, "PageSize", "A4"),
                Attribute::text(KEYWORD, "sides", "one-sided"),
                Attribute::text(NAME, "AP_ColorMatchingMode", "AP_ApplicationColorMatching"),
                Attribute::text(NAME, "AP_D_InputSlot", ""),
                Attribute::text(NAME, "ColorModel", "Gray"),
                Attribute::text(NAME, "Duplex", "None"),
                Attribute::boolean("collate", false),
                Attribute::text(NAME, "pserrorhandler-requested", "standard"),
                Attribute::integer(INTEGER, "job-priority", 50),
                Attribute::text(KEYWORD, "job-hold-until", "no-hold"),
                Attribute::text(NAME, "job-sheets", "none"),
                Attribute::boolean("PaperInfoIsSuggested..b.", true),
                Attribute::boolean("PDFIsProtected..b.", false),
                Attribute::text(NAME, "DestinationPrinterID", "local-printer"),
                Attribute::text(NAME, "com.apple.print.PrintSettings.PMCopies..n.", "1"),
                Attribute::text(
                    NAME,
                    "com.apple.print.JobInfo.PMJobName",
                    "private-test-title",
                ),
            ],
        );
        let mut groups = [operation, job];
        let pdf = pdf_bytes();
        for (document, expected) in [
            (pdf.as_slice(), SUCCESSFUL_OK),
            (b"not a PDF".as_slice(), CLIENT_ERROR_DOCUMENT_FORMAT_ERROR),
        ] {
            let bytes = encode((2, 0), PRINT_JOB, 17, &groups, document).unwrap();
            let response = dispatch(
                &bridge,
                &device.device.id,
                "localhost",
                "/ipp/printers/office",
                &bytes,
            );
            assert_eq!(parse(&response).unwrap().code, expected);
        }
        assert_eq!(bridge.list_jobs().unwrap().len(), 1);
        groups[0].attributes.last_mut().unwrap().value = b"application/postscript".to_vec();
        let bytes = encode((2, 0), PRINT_JOB, 18, &groups, &pdf).unwrap();
        let response = dispatch(
            &bridge,
            &device.device.id,
            "localhost",
            "/ipp/printers/office",
            &bytes,
        );
        assert_eq!(parse(&response).unwrap().code, CLIENT_ERROR_BAD_REQUEST);
    }

    #[test]
    fn rejects_wrong_and_revoked_credentials_without_reading_body() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let auth = STANDARD.encode(format!("{}:wrong", device.device.id));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::new(UnreadBody))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
            bridge.revoke_device(&device.device.id).unwrap();
            let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::new(UnreadBody))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(bridge.list_jobs().unwrap().is_empty());
            let events = bridge.connection_events();
            assert_eq!(events.len(), 2);
            assert!(events
                .iter()
                .all(|event| event.http_status == 401 && event.ipp_status.is_none()));
        });
    }

    #[test]
    fn capabilities_and_validate_job_advertise_only_supported_operations() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let bytes = call(
                &bridge,
                &device,
                "office",
                GET_PRINTER_ATTRIBUTES,
                vec![],
                &[],
            )
            .await;
            let response = parse(&bytes).unwrap();
            assert_eq!(response.code, SUCCESSFUL_OK);
            assert_eq!(response.request_id, 17);
            let mut operations: Vec<_> = response
                .attributes("operations-supported")
                .map(|a| a.as_i32().unwrap() as u16)
                .collect();
            operations.sort();
            assert_eq!(
                operations,
                vec![
                    PRINT_JOB,
                    VALIDATE_JOB,
                    CREATE_JOB,
                    SEND_DOCUMENT,
                    CANCEL_JOB,
                    GET_JOB_ATTRIBUTES,
                    GET_JOBS,
                    GET_PRINTER_ATTRIBUTES
                ]
            );
            assert_eq!(
                text(&response, "document-format-supported"),
                Some("application/pdf")
            );
            assert!(
                text(&response, "printer-uuid").is_some_and(|uuid| uuid.starts_with("urn:uuid:"))
            );
            let mut formats: Vec<_> = response
                .attributes("document-format-supported")
                .map(|attribute| attribute.as_str().unwrap())
                .collect();
            formats.sort_unstable();
            assert_eq!(
                formats,
                vec![
                    "application/pdf",
                    "image/jpeg",
                    "image/pwg-raster",
                    "image/urf"
                ]
            );
            assert_eq!(
                text(&response, "uri-authentication-supported"),
                Some("basic")
            );
            assert_eq!(response.attributes("sides-supported").count(), 3);
            assert_eq!(
                response
                    .attributes("print-color-mode-supported")
                    .filter_map(Attribute::as_str)
                    .collect::<Vec<_>>(),
                vec!["auto", "color", "monochrome"]
            );
            assert_eq!(response.attribute("color-supported").unwrap().value, [1]);
            assert_eq!(
                response
                    .attributes("urf-supported")
                    .filter_map(Attribute::as_str)
                    .collect::<Vec<_>>(),
                vec!["W8", "SRGB24", "RS300", "DM1"]
            );
            assert_eq!(
                response.attributes("printer-resolution-supported").count(),
                1
            );
            assert_eq!(
                response
                    .attribute("printer-resolution-default")
                    .unwrap()
                    .tag,
                RESOLUTION
            );
            assert_eq!(
                text(&response, "pwg-raster-document-sheet-back"),
                Some("normal")
            );
            assert_eq!(response.attributes("media-col-supported").count(), 6);
            assert_eq!(
                response.attribute("media-col-default").unwrap().tag,
                BEGIN_COLLECTION
            );
            assert_eq!(response.attributes("media-col-database").count(), 1);
            for name in [
                b"document-format-supported".as_slice(),
                b"media-col-database".as_slice(),
                b"urf-supported".as_slice(),
            ] {
                assert_eq!(
                    bytes
                        .windows(name.len())
                        .filter(|window| *window == name)
                        .count(),
                    1
                );
            }
            // Multi-valued attributes must use one name followed by zero-name values.
            assert_eq!(
                bytes
                    .windows(b"sides-supported".len())
                    .filter(|s| *s == b"sides-supported")
                    .count(),
                1
            );
            let bytes = call(
                &bridge,
                &device,
                "office",
                VALIDATE_JOB,
                vec![Attribute::integer(INTEGER, "copies", 2)],
                &[],
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }

    #[test]
    fn submits_queries_and_cancels_pending_job_without_printing() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let bytes = call(
                &bridge,
                &device,
                "office",
                PRINT_JOB,
                vec![Attribute::integer(INTEGER, "copies", 2)],
                &pdf_bytes(),
            )
            .await;
            let submitted = parse(&bytes).unwrap();
            assert_eq!(submitted.code, SUCCESSFUL_OK);
            let id = submitted.attribute("job-id").unwrap().as_i32().unwrap();
            assert_eq!(submitted.attribute("job-state").unwrap().as_i32(), Some(3));
            let bytes = call(
                &bridge,
                &device,
                "office",
                GET_JOB_ATTRIBUTES,
                vec![Attribute::integer(INTEGER, "job-id", id)],
                &[],
            )
            .await;
            assert_eq!(
                parse(&bytes).unwrap().attribute("copies").unwrap().as_i32(),
                Some(2)
            );
            let bytes = call(&bridge, &device, "office", GET_JOBS, vec![], &[]).await;
            assert_eq!(parse(&bytes).unwrap().attributes("job-id").count(), 1);
            let bytes = call(
                &bridge,
                &device,
                "office",
                CANCEL_JOB,
                vec![Attribute::integer(INTEGER, "job-id", id)],
                &[],
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            assert_eq!(bridge.job(id as i64).unwrap().state, "canceled");
            let bytes = call(
                &bridge,
                &device,
                "office",
                CANCEL_JOB,
                vec![Attribute::integer(INTEGER, "job-id", id)],
                &[],
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_NOT_POSSIBLE);
        });
    }

    #[test]
    fn create_job_waits_for_send_document_before_becoming_printable() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let bytes = call(
                &bridge,
                &device,
                "office",
                CREATE_JOB,
                vec![Attribute::integer(INTEGER, "copies", 2)],
                &[],
            )
            .await;
            let created = parse(&bytes).unwrap();
            let id = created.attribute("job-id").unwrap().as_i32().unwrap();
            assert_eq!(created.code, SUCCESSFUL_OK);
            assert_eq!(created.attribute("job-state").unwrap().as_i32(), Some(3));
            assert_eq!(bridge.job(id as i64).unwrap().state, "pending-document");

            let job_uri = format!("ipp://localhost/ipp/printers/office/jobs/{id}");
            let attrs = vec![
                Attribute::text(URI, "job-uri", &job_uri),
                Attribute::text(MIME_MEDIA_TYPE, "document-format", "application/pdf"),
                Attribute::boolean("last-document", false),
            ];
            let bytes = call(
                &bridge,
                &device,
                "office",
                SEND_DOCUMENT,
                attrs,
                &pdf_bytes(),
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            assert_eq!(
                bridge.job(id as i64).unwrap().state,
                "pending-final-document"
            );

            let attrs = vec![
                Attribute::text(URI, "job-uri", &job_uri),
                Attribute::boolean("last-document", true),
            ];
            let bytes = call(&bridge, &device, "office", SEND_DOCUMENT, attrs, &[]).await;
            assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            assert_eq!(bridge.job(id as i64).unwrap().state, "pending");

            let other = bridge.create_device("Other Mac".into()).unwrap();
            let attrs = vec![
                Attribute::text(URI, "job-uri", &job_uri),
                Attribute::boolean("last-document", true),
            ];
            let bytes = call(&bridge, &other, "office", SEND_DOCUMENT, attrs, &[]).await;
            assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_NOT_FOUND);
            assert_eq!(bridge.job(id as i64).unwrap().state, "pending");
            bridge.cancel_job(id as i64).unwrap();
        });
    }

    #[test]
    fn isolates_queries_and_cancellation_by_device_and_printer() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let other = bridge.create_device("Other Mac".into()).unwrap();
            let mut printer = bridge.printer("office").unwrap();
            printer.id = "warehouse".into();
            printer.queue = "Queue B".into();
            bridge.save_printer(printer).unwrap();
            let bytes = call(&bridge, &device, "office", PRINT_JOB, vec![], &pdf_bytes()).await;
            let id = parse(&bytes)
                .unwrap()
                .attribute("job-id")
                .unwrap()
                .as_i32()
                .unwrap();
            for operation in [GET_JOB_ATTRIBUTES, CANCEL_JOB] {
                for (who, printer) in [(&other, "office"), (&device, "warehouse")] {
                    let bytes = call(
                        &bridge,
                        who,
                        printer,
                        operation,
                        vec![Attribute::integer(INTEGER, "job-id", id)],
                        &[],
                    )
                    .await;
                    assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_NOT_FOUND);
                }
            }
            let bytes = call(
                &bridge,
                &other,
                "office",
                GET_JOBS,
                vec![Attribute::text(KEYWORD, "which-jobs", "all")],
                &[],
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().attributes("job-id").count(), 0);
            assert_eq!(bridge.job(id as i64).unwrap().state, "pending");
        });
    }

    #[test]
    fn rejects_formats_options_unknown_operations_and_conflicting_uris() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            for (attribute, status) in [
                (
                    Attribute::text(MIME_MEDIA_TYPE, "document-format", "image/pwg-raster"),
                    CLIENT_ERROR_DOCUMENT_FORMAT_ERROR,
                ),
                (
                    Attribute::integer(INTEGER, "copies", 0),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(KEYWORD, "media", "na_letter_8.5x11in"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(KEYWORD, "sides", "invalid"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::integer(INTEGER, "number-up", 2),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(NAME, "PageSize", "Letter"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::integer(ENUM, "finishings", 4),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(NAME, "AP_D_InputSlot", "Tray2"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(NAME, "Duplex", "InvalidDuplex"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::integer(INTEGER, "job-priority", 75),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
            ] {
                let bytes = call(
                    &bridge,
                    &device,
                    "office",
                    PRINT_JOB,
                    vec![attribute],
                    &pdf_bytes(),
                )
                .await;
                assert_eq!(parse(&bytes).unwrap().code, status);
            }
            let bytes = call(&bridge, &device, "office", 0x0007, vec![], &[]).await;
            assert_eq!(
                parse(&bytes).unwrap().code,
                SERVER_ERROR_OPERATION_NOT_SUPPORTED
            );
            let bytes = call(
                &bridge,
                &device,
                "office",
                GET_JOB_ATTRIBUTES,
                vec![
                    Attribute::integer(INTEGER, "job-id", 1),
                    Attribute::text(URI, "job-uri", "ipp://localhost/ipp/printers/office/jobs/2"),
                ],
                &[],
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_BAD_REQUEST);
            let bytes = call(
                &bridge,
                &device,
                "office",
                PRINT_JOB,
                vec![],
                b"%PDF-1.7 broken",
            )
            .await;
            assert_ne!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }

    #[test]
    fn intentional_identical_submissions_remain_distinct_jobs() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let pdf = pdf_bytes();
            let first = call(&bridge, &device, "office", PRINT_JOB, vec![], &pdf).await;
            let second = call(&bridge, &device, "office", PRINT_JOB, vec![], &pdf).await;
            let first = parse(&first)
                .unwrap()
                .attribute("job-id")
                .unwrap()
                .as_i32()
                .unwrap();
            let second = parse(&second)
                .unwrap()
                .attribute("job-id")
                .unwrap()
                .as_i32()
                .unwrap();
            assert_ne!(first, second);
            assert_eq!(bridge.list_jobs().unwrap().len(), 2);
        });
    }

    #[test]
    fn rejects_wrong_tags_and_invalid_utf8_in_explicit_attributes() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            for attribute in [
                Attribute {
                    tag: KEYWORD,
                    name: "sides".into(),
                    value: vec![0xff],
                },
                Attribute::integer(INTEGER, "sides", 1),
                Attribute::text(NAME, "media", "iso_a4_210x297mm"),
                Attribute::text(KEYWORD, "document-format", "application/pdf"),
                Attribute::text(KEYWORD, "copies", "1"),
            ] {
                let bytes = call(
                    &bridge,
                    &device,
                    "office",
                    PRINT_JOB,
                    vec![attribute],
                    &pdf_bytes(),
                )
                .await;
                assert_eq!(
                    parse(&bytes).unwrap().code,
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED
                );
            }
            for (operation, attribute) in [
                (GET_JOB_ATTRIBUTES, Attribute::text(KEYWORD, "job-id", "1")),
                (GET_JOBS, Attribute::text(NAME, "which-jobs", "all")),
                (GET_JOBS, Attribute::text(KEYWORD, "limit", "1")),
            ] {
                let bytes = call(&bridge, &device, "office", operation, vec![attribute], &[]).await;
                assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_BAD_REQUEST);
            }
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }

    #[test]
    fn validates_without_a_document_but_rejects_unknown_job_attributes() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            let bytes = call(&bridge, &device, "office", VALIDATE_JOB, vec![], &[]).await;
            assert_eq!(parse(&bytes).unwrap().code, SUCCESSFUL_OK);
            let bytes = call(
                &bridge,
                &device,
                "office",
                VALIDATE_JOB,
                vec![],
                &pdf_bytes(),
            )
            .await;
            assert_eq!(parse(&bytes).unwrap().code, CLIENT_ERROR_BAD_REQUEST);
            let mut operation = operation_attributes();
            operation.attributes.push(Attribute::text(
                URI,
                "printer-uri",
                "ipp://localhost/ipp/printers/office",
            ));
            let job = Group::new(
                JOB_ATTRIBUTES,
                vec![Attribute::text(
                    KEYWORD,
                    "unsupported-job-setting",
                    "enabled",
                )],
            );
            let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::from(
                    encode((1, 1), PRINT_JOB, 18, &[operation, job], &pdf_bytes()).unwrap(),
                ))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), MAX_PDF).await.unwrap();
            assert_eq!(
                parse(&bytes).unwrap().code,
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED
            );
            let expected = "Unsupported job attribute: unsupported-job-setting";
            assert_eq!(
                text(&parse(&bytes).unwrap(), "status-message"),
                Some(expected)
            );
            assert_eq!(bridge.connection_events()[0].detail, expected);
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }

    #[test]
    fn stopped_service_rejects_new_requests_without_reading_the_document() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (_directory, bridge, device) = fixture();
            bridge.stop().unwrap();
            let auth = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
            let request = Request::builder()
                .method("POST")
                .uri("/ipp/printers/office")
                .header(header::CONTENT_TYPE, "application/ipp")
                .header(header::AUTHORIZATION, format!("Basic {auth}"))
                .body(Body::new(UnreadBody))
                .unwrap();
            let response = router(Arc::clone(&bridge)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(bridge.list_jobs().unwrap().is_empty());
        });
    }
}
