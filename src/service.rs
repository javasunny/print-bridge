use crate::{
    ipp::*,
    store::{now, Bridge, ConnectionEvent, Job, SharedPrinter, ANONYMOUS_DEVICE, MAX_PDF},
};
use axum::{
    body::to_bytes,
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
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
            "请求超过大小上限（PDF 最多 100 MiB）",
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
        .with_state(Server {
            bridge: Arc::clone(&bridge),
            uploads: Arc::new(Semaphore::new(4)),
        })
        .layer(middleware::from_fn_with_state(bridge, record_connection))
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
                        Some(PRINT_JOB) => "PDF 打印任务已接收",
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

fn media_collection(bytes: &mut Vec<u8>, name: &str) {
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
    for (member, dimension) in [("x-dimension", 21000i32), ("y-dimension", 29700)] {
        collection_field(bytes, MEMBER_ATTR_NAME, "", member.as_bytes());
        collection_field(bytes, INTEGER, "", &dimension.to_be_bytes());
    }
    collection_field(bytes, END_COLLECTION, "", &[]);
    collection_field(bytes, END_COLLECTION, "", &[]);
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
    media_collection(&mut bytes, "media-col-default");
    media_collection(&mut bytes, "media-col-database");
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
    if message.code != PRINT_JOB && !message.document.is_empty() {
        return fail(CLIENT_ERROR_BAD_REQUEST, "Unexpected document");
    }
    match message.code {
        GET_PRINTER_ATTRIBUTES => {
            printer_reply(&message, &printer, &uri, bridge.authentication_required())
        }
        VALIDATE_JOB | PRINT_JOB => {
            if parts.len() != 4 {
                return fail(CLIENT_ERROR_BAD_REQUEST, "Expected printer route");
            }
            if !printer.enabled {
                return fail(SERVER_ERROR_SERVICE_UNAVAILABLE, "Printer disabled");
            }
            let (copies, sides) = match options(&message, &printer) {
                Ok(o) => o,
                Err((code, s)) => return fail(code, &s),
            };
            if message.code == VALIDATE_JOB {
                return reply(&message, SUCCESSFUL_OK, vec![], "");
            }
            match bridge.accept(device, &printer.id, copies, sides, message.document) {
                Ok(job) => reply(&message, SUCCESSFUL_OK, vec![job_group(&job, &uri)], ""),
                Err(error) => fail(CLIENT_ERROR_NOT_POSSIBLE, &error),
            }
        }
        GET_JOB_ATTRIBUTES | CANCEL_JOB => {
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
fn options<'a>(m: &'a Message<'_>, p: &SharedPrinter) -> Result<(u32, &'a str), (u16, String)> {
    for (name, tag) in [
        ("document-format", MIME_MEDIA_TYPE),
        ("sides", KEYWORD),
        ("media", KEYWORD),
    ] {
        if m.attribute(name)
            .is_some_and(|a| a.tag != tag || a.as_str().is_none())
        {
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
    if m.attribute("document-format").is_some()
        && !m
            .attributes("document-format")
            .any(|a| a.as_str() == Some("application/pdf"))
    {
        return Err((
            CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED,
            "Only application/pdf is supported".into(),
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
    let sides = text(m, "sides").unwrap_or("one-sided");
    for (name, tag, value) in [
        ("AP_ColorMatchingMode", NAME, "AP_ApplicationColorMatching"),
        ("AP_D_InputSlot", NAME, ""),
        ("ColorModel", NAME, "Gray"),
        ("Duplex", NAME, "None"),
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
    for (name, unsupported) in [
        (
            "Duplex",
            m.attribute("Duplex").is_some() && sides != "one-sided",
        ),
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
    if text(m, "media").is_some_and(|s| !matches!(s, "iso_a4_210x297mm" | "A4")) {
        return Err((
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Only A4 is supported".into(),
        ));
    }
    if m.attributes("PageSize")
        .any(|a| a.tag != NAME || a.as_str() != Some("A4"))
        || m.attributes("number-up")
            .any(|a| a.tag != INTEGER || a.as_i32() != Some(1))
        || m.attributes("finishings")
            .any(|a| a.tag != ENUM || a.as_i32() != Some(3))
    {
        return Err((
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Unsupported page size, number-up or finishing".into(),
        ));
    }
    for name in [
        "page-ranges",
        "orientation-requested",
        "print-color-mode",
        "media-col",
        "printer-resolution",
        "output-bin",
        "print-quality",
    ] {
        if m.attribute(name).is_some() {
            return Err((
                CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                format!("Unsupported print option: {name}"),
            ));
        }
    }
    Bridge::validate_options(p, copies, sides).map_err(|_| {
        (
            CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
            "Unsupported copies or sides".into(),
        )
    })?;
    Ok((copies, sides))
}
fn printer_group(p: &SharedPrinter, uri: &str, authentication_required: bool) -> Group {
    let mut a = vec![
        Attribute::text(URI, "printer-uri-supported", uri),
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
        Attribute::text(KEYWORD, "media-supported", "iso_a4_210x297mm"),
        Attribute::text(KEYWORD, "media-default", "iso_a4_210x297mm"),
        Attribute::integer(INTEGER, "media-bottom-margin-supported", 0),
        Attribute::integer(INTEGER, "media-left-margin-supported", 0),
        Attribute::integer(INTEGER, "media-right-margin-supported", 0),
        Attribute::integer(INTEGER, "media-top-margin-supported", 0),
        Attribute::text(KEYWORD, "sides-default", "one-sided"),
        Attribute::integer(INTEGER, "copies-default", 1),
        Attribute::boolean("multiple-document-jobs-supported", false),
        Attribute::text(KEYWORD, "compression-supported", "none"),
    ];
    a.push(Attribute {
        tag: RANGE_OF_INTEGER,
        name: "copies-supported".into(),
        value: [1i32.to_be_bytes(), (p.max_copies as i32).to_be_bytes()].concat(),
    });
    for op in [
        PRINT_JOB,
        VALIDATE_JOB,
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
        "processing" => (5, "job-printing"),
        "submitted" => (9, "queued-in-device"),
        "simulated" => (9, "job-completed-successfully"),
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
            assert_eq!(event.detail, "Only A4 is supported");
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
            (b"not a PDF".as_slice(), CLIENT_ERROR_NOT_POSSIBLE),
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
            assert_eq!(
                text(&response, "uri-authentication-supported"),
                Some("basic")
            );
            assert_eq!(response.attributes("sides-supported").count(), 3);
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
                    CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED,
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
                    Attribute::text(NAME, "ColorModel", "RGB"),
                    CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                ),
                (
                    Attribute::text(NAME, "Duplex", "DuplexNoTumble"),
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
            let bytes = call(&bridge, &device, "office", 0x0005, vec![], &[]).await;
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
