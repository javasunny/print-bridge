//! Bounded IPP message encoding and decoding for the single-document bridge.

pub const PRINT_JOB: u16 = 0x0002;
pub const VALIDATE_JOB: u16 = 0x0004;
pub const CANCEL_JOB: u16 = 0x0008;
pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;
pub const GET_JOBS: u16 = 0x000a;
pub const GET_PRINTER_ATTRIBUTES: u16 = 0x000b;

pub const SUCCESSFUL_OK: u16 = 0x0000;
pub const CLIENT_ERROR_BAD_REQUEST: u16 = 0x0400;
pub const CLIENT_ERROR_FORBIDDEN: u16 = 0x0401;
pub const CLIENT_ERROR_NOT_AUTHENTICATED: u16 = 0x0402;
pub const CLIENT_ERROR_NOT_AUTHORIZED: u16 = 0x0403;
pub const CLIENT_ERROR_NOT_POSSIBLE: u16 = 0x0404;
pub const CLIENT_ERROR_NOT_FOUND: u16 = 0x0406;
pub const CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED: u16 = 0x040a;
pub const CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED: u16 = 0x040b;
pub const SERVER_ERROR_INTERNAL_ERROR: u16 = 0x0500;
pub const SERVER_ERROR_OPERATION_NOT_SUPPORTED: u16 = 0x0501;
pub const SERVER_ERROR_SERVICE_UNAVAILABLE: u16 = 0x0502;
pub const SERVER_ERROR_VERSION_NOT_SUPPORTED: u16 = 0x0503;
pub const SERVER_ERROR_BUSY: u16 = 0x0507;

pub const OPERATION_ATTRIBUTES: u8 = 0x01;
pub const JOB_ATTRIBUTES: u8 = 0x02;
pub const PRINTER_ATTRIBUTES: u8 = 0x04;
pub const UNSUPPORTED_ATTRIBUTES: u8 = 0x05;
pub const INTEGER: u8 = 0x21;
pub const BOOLEAN: u8 = 0x22;
pub const ENUM: u8 = 0x23;
pub const OCTET_STRING: u8 = 0x30;
pub const RANGE_OF_INTEGER: u8 = 0x33;
pub const TEXT: u8 = 0x41;
pub const NAME: u8 = 0x42;
pub const KEYWORD: u8 = 0x44;
pub const URI: u8 = 0x45;
pub const CHARSET: u8 = 0x47;
pub const NATURAL_LANGUAGE: u8 = 0x48;
pub const MIME_MEDIA_TYPE: u8 = 0x49;

pub const MAX_ATTRIBUTES: usize = 256;
pub const MAX_GROUPS: usize = 64;
pub const MAX_ATTRIBUTE_BYTES: usize = 64 * 1024;
pub const MAX_DOCUMENT_BYTES: usize = 100 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub tag: u8,
    pub name: String,
    pub value: Vec<u8>,
}

impl Attribute {
    pub fn text(tag: u8, name: &str, value: &str) -> Self {
        Self {
            tag,
            name: name.into(),
            value: value.as_bytes().to_vec(),
        }
    }

    pub fn integer(tag: u8, name: &str, value: i32) -> Self {
        Self {
            tag,
            name: name.into(),
            value: value.to_be_bytes().to_vec(),
        }
    }

    pub fn boolean(name: &str, value: bool) -> Self {
        Self {
            tag: BOOLEAN,
            name: name.into(),
            value: vec![u8::from(value)],
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.value).ok()
    }

    pub fn as_i32(&self) -> Option<i32> {
        let bytes: [u8; 4] = self.value.as_slice().try_into().ok()?;
        Some(i32::from_be_bytes(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub tag: u8,
    pub attributes: Vec<Attribute>,
}

impl Group {
    pub fn new(tag: u8, attributes: Vec<Attribute>) -> Self {
        Self { tag, attributes }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Message<'a> {
    pub version: (u8, u8),
    /// Operation ID for requests, status code for responses.
    pub code: u16,
    pub request_id: i32,
    pub groups: Vec<Group>,
    pub document: &'a [u8],
}

impl Message<'_> {
    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.groups
            .iter()
            .flat_map(|g| &g.attributes)
            .find(|a| a.name == name)
    }

    pub fn attributes<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Attribute> {
        self.groups
            .iter()
            .flat_map(|g| &g.attributes)
            .filter(move |a| a.name == name)
    }
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, count: usize) -> Result<&'a [u8], Error> {
    let end = cursor.checked_add(count).ok_or(Error("length overflow"))?;
    let value = bytes.get(*cursor..end).ok_or(Error("truncated message"))?;
    *cursor = end;
    Ok(value)
}

fn length(bytes: &[u8], cursor: &mut usize) -> Result<usize, Error> {
    let value = take(bytes, cursor, 2)?;
    let length = u16::from_be_bytes([value[0], value[1]]) as usize;
    if length > i16::MAX as usize {
        return Err(Error("negative IPP length"));
    }
    Ok(length)
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
        })
    {
        return Err(Error("invalid attribute name"));
    }
    Ok(())
}

fn validate_value(tag: u8, value: &[u8]) -> Result<(), Error> {
    match tag {
        0x34 | 0x37 | 0x4a => Err(Error("collection attributes are not supported")),
        0x7f => Err(Error("extended value tags are not supported")),
        INTEGER | ENUM if value.len() != 4 => Err(Error("invalid integer value length")),
        BOOLEAN if value.len() != 1 || value[0] > 1 => Err(Error("invalid boolean value")),
        RANGE_OF_INTEGER if value.len() != 8 => Err(Error("invalid integer range length")),
        0x31 if value.len() != 11 => Err(Error("invalid date-time length")),
        0x32 if value.len() != 9 => Err(Error("invalid resolution length")),
        0x10..=0x1f if !value.is_empty() => Err(Error("out-of-band value must be empty")),
        0x10..=0x7e => Ok(()),
        _ => Err(Error("invalid value tag")),
    }
}

pub fn parse(bytes: &[u8]) -> Result<Message<'_>, Error> {
    if bytes.len() > MAX_DOCUMENT_BYTES + MAX_ATTRIBUTE_BYTES + 8 {
        return Err(Error("message exceeds size limit"));
    }
    let mut cursor = 0;
    let header = take(bytes, &mut cursor, 8)?;
    let request_id = i32::from_be_bytes(header[4..8].try_into().unwrap());
    if header[0] == 0 || request_id <= 0 {
        return Err(Error("invalid version or request ID"));
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut attribute_count = 0;
    loop {
        if cursor - 8 >= MAX_ATTRIBUTE_BYTES {
            return Err(Error("attributes exceed size limit"));
        }
        let tag = take(bytes, &mut cursor, 1)?[0];
        if tag == 0x03 {
            if groups.first().map(|g| g.tag) != Some(OPERATION_ATTRIBUTES) {
                return Err(Error("operation attributes must be first"));
            }
            let document = &bytes[cursor..];
            if document.len() > MAX_DOCUMENT_BYTES {
                return Err(Error("document exceeds size limit"));
            }
            return Ok(Message {
                version: (header[0], header[1]),
                code: u16::from_be_bytes([header[2], header[3]]),
                request_id,
                groups,
                document,
            });
        }
        if matches!(
            tag,
            OPERATION_ATTRIBUTES | JOB_ATTRIBUTES | PRINTER_ATTRIBUTES | UNSUPPORTED_ATTRIBUTES
        ) {
            if groups.len() == MAX_GROUPS || (tag == OPERATION_ATTRIBUTES && !groups.is_empty()) {
                return Err(Error("invalid or excessive attribute groups"));
            }
            groups.push(Group::new(tag, Vec::new()));
            continue;
        }
        if tag < 0x10 {
            return Err(Error("unsupported group tag"));
        }
        let group = groups.last_mut().ok_or(Error("attribute before group"))?;
        attribute_count += 1;
        if attribute_count > MAX_ATTRIBUTES {
            return Err(Error("too many attributes"));
        }
        let name_length = length(bytes, &mut cursor)?;
        if name_length > MAX_NAME_BYTES {
            return Err(Error("attribute name too long"));
        }
        let name = if name_length == 0 {
            let previous = group
                .attributes
                .last()
                .ok_or(Error("additional value without attribute"))?;
            if previous.tag != tag {
                return Err(Error("additional value has different tag"));
            }
            previous.name.clone()
        } else {
            let raw = take(bytes, &mut cursor, name_length)?;
            let name =
                std::str::from_utf8(raw).map_err(|_| Error("invalid attribute name encoding"))?;
            validate_name(name)?;
            name.to_owned()
        };
        let value_length = length(bytes, &mut cursor)?;
        let value = take(bytes, &mut cursor, value_length)?;
        validate_value(tag, value)?;
        if cursor - 8 >= MAX_ATTRIBUTE_BYTES {
            return Err(Error("attributes exceed size limit"));
        }
        group.attributes.push(Attribute {
            tag,
            name,
            value: value.to_vec(),
        });
    }
}

/// Consecutive attributes with the same name encode as additional values.
pub fn encode(
    version: (u8, u8),
    code: u16,
    request_id: i32,
    groups: &[Group],
    document: &[u8],
) -> Result<Vec<u8>, Error> {
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(Error("document exceeds size limit"));
    }
    if groups.len() > MAX_GROUPS
        || groups.iter().map(|g| g.attributes.len()).sum::<usize>() > MAX_ATTRIBUTES
    {
        return Err(Error("too many groups or attributes"));
    }
    if groups.iter().any(|group| {
        !matches!(
            group.tag,
            OPERATION_ATTRIBUTES | JOB_ATTRIBUTES | PRINTER_ATTRIBUTES | UNSUPPORTED_ATTRIBUTES
        )
    }) {
        return Err(Error("unsupported group tag"));
    }
    let mut bytes = vec![version.0, version.1];
    bytes.extend_from_slice(&code.to_be_bytes());
    bytes.extend_from_slice(&request_id.to_be_bytes());
    for group in groups {
        bytes.push(group.tag);
        let mut previous: Option<&Attribute> = None;
        for attribute in &group.attributes {
            validate_name(&attribute.name)?;
            validate_value(attribute.tag, &attribute.value)?;
            if attribute.value.len() > i16::MAX as usize {
                return Err(Error("attribute value too long"));
            }
            bytes.push(attribute.tag);
            let additional =
                previous.is_some_and(|a| a.name == attribute.name && a.tag == attribute.tag);
            let name = if additional { "" } else { &attribute.name };
            bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(attribute.value.len() as u16).to_be_bytes());
            bytes.extend_from_slice(&attribute.value);
            if bytes.len() - 8 >= MAX_ATTRIBUTE_BYTES {
                return Err(Error("attributes exceed size limit"));
            }
            previous = Some(attribute);
        }
    }
    bytes.push(0x03);
    // Validate before appending a potentially large document.
    parse(&bytes)?;
    bytes.extend_from_slice(document);
    Ok(bytes)
}

pub fn operation_attributes() -> Group {
    Group::new(
        OPERATION_ATTRIBUTES,
        vec![
            Attribute::text(CHARSET, "attributes-charset", "utf-8"),
            Attribute::text(NATURAL_LANGUAGE, "attributes-natural-language", "en"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pdf_and_multiple_values() {
        let groups = vec![
            operation_attributes(),
            Group::new(
                JOB_ATTRIBUTES,
                vec![
                    Attribute::text(KEYWORD, "sides-supported", "one-sided"),
                    Attribute::text(KEYWORD, "sides-supported", "two-sided-long-edge"),
                    Attribute::integer(INTEGER, "copies", 2),
                    Attribute::boolean("my-jobs", true),
                ],
            ),
        ];
        let bytes = encode((1, 1), PRINT_JOB, 9, &groups, b"%PDF-1.7\n").unwrap();
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.groups, groups);
        assert_eq!(parsed.document, b"%PDF-1.7\n");
        assert_eq!(parsed.attribute("copies").unwrap().as_i32(), Some(2));
        assert_eq!(parsed.attributes("sides-supported").count(), 2);
        assert_eq!(parsed.request_id, 9);
    }

    #[test]
    fn rejects_every_truncated_prefix() {
        let bytes = encode(
            (1, 1),
            GET_PRINTER_ATTRIBUTES,
            1,
            &[operation_attributes()],
            &[],
        )
        .unwrap();
        for end in 0..bytes.len() {
            assert!(
                parse(&bytes[..end]).is_err(),
                "accepted prefix of {end} bytes"
            );
        }
    }

    #[test]
    fn rejects_malformed_attributes() {
        let prefix = [1, 1, 0, 2, 0, 0, 0, 1, 1];
        for suffix in [
            vec![KEYWORD, 0, 0, 0, 1, b'x', 3],
            vec![KEYWORD, 0xff, 0xff, 3],
            vec![INTEGER, 0, 1, b'x', 0, 1, 1, 3],
            vec![BOOLEAN, 0, 1, b'x', 0, 1, 2, 3],
            vec![0x34, 0, 1, b'x', 0, 0, 3],
            vec![0x0f, 3],
        ] {
            let mut bytes = prefix.to_vec();
            bytes.extend(suffix);
            assert!(parse(&bytes).is_err());
        }
    }

    #[test]
    fn limits_attribute_count_and_groups() {
        let mut group = operation_attributes();
        group
            .attributes
            .extend((0..MAX_ATTRIBUTES).map(|_| Attribute::text(KEYWORD, "x", "y")));
        assert!(encode((1, 1), PRINT_JOB, 1, &[group], &[]).is_err());
        let mut groups = vec![operation_attributes()];
        groups.extend((0..MAX_GROUPS).map(|_| Group::new(JOB_ATTRIBUTES, vec![])));
        assert!(encode((1, 1), PRINT_JOB, 1, &groups, &[]).is_err());
    }

    #[test]
    fn validates_header_and_group_order() {
        assert!(encode((1, 1), PRINT_JOB, 0, &[operation_attributes()], &[]).is_err());
        assert!(encode((0, 0), PRINT_JOB, 1, &[operation_attributes()], &[]).is_err());
        assert!(encode(
            (1, 1),
            PRINT_JOB,
            1,
            &[Group::new(JOB_ATTRIBUTES, vec![])],
            &[]
        )
        .is_err());
        assert!(encode(
            (1, 1),
            PRINT_JOB,
            1,
            &[operation_attributes(), operation_attributes()],
            &[]
        )
        .is_err());
    }
}
