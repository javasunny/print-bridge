use jpeg_decoder::{Decoder as JpegDecoder, PixelFormat};
use lopdf::{dictionary, Document, Object, Stream};

const MAX_INPUT_BYTES: usize = 100 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 100 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = MAX_OUTPUT_BYTES - 2 * 1024 * 1024;
const MAX_TOTAL_DECODED_BYTES: usize = 512 * 1024 * 1024;
const MAX_PDF_STREAM_BYTES: usize = MAX_OUTPUT_BYTES - 2 * 1024 * 1024;
const MAX_PROGRESSIVE_JPEG_PIXELS: usize = 8 * 1024 * 1024;
const MAX_PAGE_COUNT: usize = 500;
const MAX_DIMENSION: usize = 20_000;
const RASTER_DPI: u32 = 300;
const MAX_PAGE_POINTS: f32 = 14_400.0;
const PWG_HEADER_LEN: usize = 1_796;
const URF_HEADER_LEN: usize = 32;

#[derive(Clone, Copy)]
enum ColorSpace {
    Gray,
    Rgb,
}

impl ColorSpace {
    fn channels(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }

    fn pdf_name(self) -> &'static [u8] {
        match self {
            Self::Gray => b"DeviceGray",
            Self::Rgb => b"DeviceRGB",
        }
    }
}

struct PageImage {
    width: usize,
    height: usize,
    x_dpi: u32,
    y_dpi: u32,
    color_space: ColorSpace,
    pixels: Vec<u8>,
}

struct PdfBuilder {
    document: Document,
    pages_id: lopdf::ObjectId,
    page_ids: Vec<Object>,
    decoded_bytes: usize,
    stream_bytes: usize,
}

impl PdfBuilder {
    fn new() -> Self {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        Self {
            document,
            pages_id,
            page_ids: Vec::new(),
            decoded_bytes: 0,
            stream_bytes: 0,
        }
    }

    fn reserve_page(&mut self, decoded_bytes: usize) -> Result<(), String> {
        if decoded_bytes > MAX_DECODED_BYTES {
            return Err("decoded page exceeds size limit".into());
        }
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(decoded_bytes)
            .filter(|sum| *sum <= MAX_TOTAL_DECODED_BYTES)
            .ok_or_else(|| "decoded document exceeds size limit".to_string())?;
        Ok(())
    }

    fn add_page(&mut self, page: PageImage) -> Result<(), String> {
        let width_points = page.width as f32 * 72.0 / page.x_dpi as f32;
        let height_points = page.height as f32 * 72.0 / page.y_dpi as f32;
        if !width_points.is_finite()
            || !height_points.is_finite()
            || width_points <= 0.0
            || height_points <= 0.0
            || width_points > MAX_PAGE_POINTS
            || height_points > MAX_PAGE_POINTS
        {
            return Err("raster page dimensions are out of range".into());
        }

        let name = format!("Im{}", self.page_ids.len());
        let mut image_dict = lopdf::Dictionary::new();
        image_dict.set("Type", Object::Name(b"XObject".to_vec()));
        image_dict.set("Subtype", Object::Name(b"Image".to_vec()));
        image_dict.set("Width", page.width as i64);
        image_dict.set("Height", page.height as i64);
        image_dict.set(
            "ColorSpace",
            Object::Name(page.color_space.pdf_name().to_vec()),
        );
        image_dict.set("BitsPerComponent", 8);
        let mut image_stream = Stream::new(image_dict, page.pixels);
        image_stream
            .compress()
            .map_err(|_| "unable to compress raster page".to_string())?;

        let content =
            format!("q\n{width_points:.6} 0 0 {height_points:.6} 0 0 cm\n/{name} Do\nQ\n");
        let content_bytes = content.into_bytes();
        let added_stream_bytes = image_stream
            .content
            .len()
            .checked_add(content_bytes.len())
            .and_then(|bytes| self.stream_bytes.checked_add(bytes))
            .filter(|sum| *sum <= MAX_PDF_STREAM_BYTES)
            .ok_or_else(|| "normalized PDF exceeds size limit".to_string())?;

        let image_id = self.document.add_object(image_stream);
        let content_id = self
            .document
            .add_object(Stream::new(lopdf::Dictionary::new(), content_bytes));
        let mut xobjects = lopdf::Dictionary::new();
        xobjects.set(name.as_bytes(), Object::Reference(image_id));
        let page_id = self.document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => self.pages_id,
            "MediaBox" => vec![0.into(), 0.into(), width_points.into(), height_points.into()],
            "Resources" => dictionary! { "XObject" => Object::Dictionary(xobjects) },
            "Contents" => content_id,
        });
        self.page_ids.push(Object::Reference(page_id));
        self.stream_bytes = added_stream_bytes;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, String> {
        if self.page_ids.is_empty() {
            return Err("empty document".into());
        }
        let page_count = self.page_ids.len() as i64;
        self.document.objects.insert(
            self.pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => self.page_ids,
                "Count" => page_count,
            }),
        );
        let catalog = self
            .document
            .add_object(dictionary! { "Type" => "Catalog", "Pages" => self.pages_id });
        self.document
            .trailer
            .set("Root", Object::Reference(catalog));

        let mut output = Vec::new();
        self.document
            .save_to(&mut output)
            .map_err(|_| "unable to build PDF document".to_string())?;
        if output.len() > MAX_OUTPUT_BYTES {
            return Err("normalized PDF exceeds size limit".into());
        }
        Ok(output)
    }
}

pub fn normalize(input: &[u8], mime: &str) -> Result<Vec<u8>, String> {
    if input.len() > MAX_INPUT_BYTES {
        return Err("document exceeds size limit".into());
    }

    match mime.trim().to_ascii_lowercase().as_str() {
        "application/pdf" => Ok(input.to_vec()),
        "image/jpeg" => jpeg_to_pdf(input),
        "image/urf" => urf_to_pdf(input),
        "image/pwg-raster" => pwg_to_pdf(input),
        _ => Err("unsupported document format".into()),
    }
}

fn jpeg_to_pdf(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = JpegDecoder::new(input);
    decoder.set_max_decoding_buffer_size(MAX_DECODED_BYTES);
    decoder
        .read_info()
        .map_err(|_| "invalid JPEG document".to_string())?;
    let info = decoder
        .info()
        .ok_or_else(|| "invalid JPEG document".to_string())?;
    let color_space = match info.pixel_format {
        PixelFormat::L8 => ColorSpace::Gray,
        PixelFormat::RGB24 => ColorSpace::Rgb,
        _ => return Err("unsupported JPEG color format".into()),
    };
    let width = usize::from(info.width);
    let height = usize::from(info.height);
    let pixel_count = validate_dimensions(width, height)?;
    if matches!(
        info.coding_process,
        jpeg_decoder::CodingProcess::DctProgressive
    ) && pixel_count > MAX_PROGRESSIVE_JPEG_PIXELS
    {
        return Err("progressive JPEG exceeds size limit".into());
    }
    let byte_count = pixel_count
        .checked_mul(color_space.channels())
        .filter(|size| *size <= MAX_DECODED_BYTES)
        .ok_or_else(|| "decoded image exceeds size limit".to_string())?;
    let mut pdf = PdfBuilder::new();
    pdf.reserve_page(byte_count)?;

    let pixels = decoder
        .decode()
        .map_err(|_| "invalid JPEG document".to_string())?;
    if pixels.len() != byte_count {
        return Err("invalid JPEG pixel data".into());
    }
    let orientation = decoder.exif_data().map(exif_orientation).unwrap_or(1);
    let (width, height, pixels) =
        orient_pixels(width, height, color_space.channels(), pixels, orientation)?;

    pdf.add_page(PageImage {
        width,
        height,
        x_dpi: 300,
        y_dpi: 300,
        color_space,
        pixels,
    })?;
    pdf.finish()
}

fn urf_to_pdf(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() < 12 || &input[..8] != b"UNIRAST\0" {
        return Err("invalid URF document header".into());
    }
    let declared_pages = read_u32_be(&input[8..12])? as usize;
    if declared_pages > MAX_PAGE_COUNT {
        return Err("document has too many pages".into());
    }

    let mut cursor = 12;
    let mut pdf = PdfBuilder::new();
    while cursor < input.len() && (declared_pages == 0 || pdf.page_ids.len() < declared_pages) {
        if pdf.page_ids.len() == MAX_PAGE_COUNT {
            return Err("document has too many pages".into());
        }
        let header = take(input, &mut cursor, URF_HEADER_LEN)?;
        let bits_per_pixel = header[0];
        let color_space = match (header[1], bits_per_pixel) {
            (0 | 4, 8) => ColorSpace::Gray,
            (1 | 5, 24) => ColorSpace::Rgb,
            _ => return Err("unsupported URF color format".into()),
        };
        let width = read_u32_be(&header[12..16])? as usize;
        let height = read_u32_be(&header[16..20])? as usize;
        let dpi = read_u32_be(&header[20..24])?;
        let pixel_count = validate_dimensions(width, height)?;
        validate_dpi(dpi)?;
        let page_bytes = pixel_count
            .checked_mul(color_space.channels())
            .filter(|size| *size <= MAX_DECODED_BYTES)
            .ok_or_else(|| "raster page exceeds size limit".to_string())?;
        pdf.reserve_page(page_bytes)?;
        let row_bytes = width
            .checked_mul(color_space.channels())
            .ok_or_else(|| "raster row is too large".to_string())?;
        let pixels = decode_raster_rows(
            input,
            &mut cursor,
            width,
            height,
            row_bytes,
            color_space.channels(),
        )?;

        pdf.add_page(PageImage {
            width,
            height,
            x_dpi: dpi,
            y_dpi: dpi,
            color_space,
            pixels,
        })?;
    }

    if pdf.page_ids.is_empty()
        || (declared_pages != 0 && pdf.page_ids.len() != declared_pages)
        || cursor != input.len()
    {
        return Err("incomplete URF document".into());
    }
    pdf.finish()
}

fn pwg_to_pdf(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() < 4 || &input[..4] != b"RaS2" {
        return Err("invalid PWG Raster document header".into());
    }

    let mut cursor = 4;
    let mut pdf = PdfBuilder::new();
    while cursor < input.len() {
        if pdf.page_ids.len() == MAX_PAGE_COUNT {
            return Err("document has too many pages".into());
        }
        let header = take(input, &mut cursor, PWG_HEADER_LEN)?;
        if &header[..9] != b"PwgRaster"
            || header[9] != 0
            || header[10..64].iter().any(|byte| *byte != 0)
        {
            return Err("invalid PWG Raster page header".into());
        }

        let width = read_u32_be(&header[372..376])? as usize;
        let height = read_u32_be(&header[376..380])? as usize;
        let bits_per_color = read_u32_be(&header[384..388])?;
        let bits_per_pixel = read_u32_be(&header[388..392])?;
        let bytes_per_line = read_u32_be(&header[392..396])? as usize;
        let color_order = read_u32_be(&header[396..400])?;
        let color_code = read_u32_be(&header[400..404])?;
        let num_colors = read_u32_be(&header[420..424])?;
        let color_space = match (color_code, bits_per_color, bits_per_pixel, num_colors) {
            (18, 8, 8, 1) => ColorSpace::Gray,
            (1 | 19, 8, 24, 3) => ColorSpace::Rgb,
            _ => return Err("unsupported PWG Raster color format".into()),
        };
        if color_order != 0 {
            return Err("unsupported PWG Raster color order".into());
        }

        let x_dpi = read_u32_be(&header[276..280])?;
        let y_dpi = read_u32_be(&header[280..284])?;
        let pixel_count = validate_dimensions(width, height)?;
        validate_dpi(x_dpi)?;
        validate_dpi(y_dpi)?;
        let row_bytes = width
            .checked_mul(color_space.channels())
            .ok_or_else(|| "raster row is too large".to_string())?;
        if bytes_per_line < row_bytes
            || bytes_per_line - row_bytes > 16
            || bytes_per_line % color_space.channels() != 0
        {
            return Err("invalid PWG Raster line length".into());
        }
        let page_bytes = pixel_count
            .checked_mul(color_space.channels())
            .filter(|size| *size <= MAX_DECODED_BYTES)
            .ok_or_else(|| "raster page exceeds size limit".to_string())?;
        pdf.reserve_page(page_bytes)?;
        let pixels = decode_raster_rows(
            input,
            &mut cursor,
            width,
            height,
            bytes_per_line,
            color_space.channels(),
        )?;
        pdf.add_page(PageImage {
            width,
            height,
            x_dpi,
            y_dpi,
            color_space,
            pixels,
        })?;
    }

    if pdf.page_ids.is_empty() {
        return Err("empty PWG Raster document".into());
    }
    pdf.finish()
}

fn decode_raster_rows(
    input: &[u8],
    cursor: &mut usize,
    width: usize,
    height: usize,
    bytes_per_line: usize,
    channels: usize,
) -> Result<Vec<u8>, String> {
    let row_bytes = width
        .checked_mul(channels)
        .ok_or_else(|| "raster row is too large".to_string())?;
    let total_bytes = row_bytes
        .checked_mul(height)
        .ok_or_else(|| "raster page is too large".to_string())?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(total_bytes)
        .map_err(|_| "unable to allocate raster page".to_string())?;
    let mut line = Vec::new();
    line.try_reserve_exact(bytes_per_line)
        .map_err(|_| "unable to allocate raster line".to_string())?;
    line.resize(bytes_per_line, 0xff);

    let mut decoded_rows = 0usize;
    while decoded_rows < height {
        let repeated_rows = usize::from(read_byte(input, cursor)?) + 1;
        if repeated_rows > height - decoded_rows {
            return Err("raster line repeat exceeds page height".into());
        }
        decode_packbits_row(input, cursor, &mut line, channels)?;
        for _ in 0..repeated_rows {
            pixels.extend_from_slice(&line[..row_bytes]);
        }
        decoded_rows += repeated_rows;
    }
    Ok(pixels)
}

fn decode_packbits_row(
    input: &[u8],
    cursor: &mut usize,
    row: &mut [u8],
    pixel_bytes: usize,
) -> Result<(), String> {
    let mut output = 0usize;
    while output < row.len() {
        let code = read_byte(input, cursor)?;
        if code == 128 {
            row[output..].fill(0xff);
            break;
        }

        if code < 128 {
            let count = usize::from(code) + 1;
            let value = take(input, cursor, pixel_bytes)?;
            let bytes = count
                .checked_mul(pixel_bytes)
                .ok_or_else(|| "invalid raster run".to_string())?;
            if bytes > row.len() - output {
                return Err("raster run exceeds line length".into());
            }
            for pixel in row[output..output + bytes].chunks_exact_mut(pixel_bytes) {
                pixel.copy_from_slice(value);
            }
            output += bytes;
        } else {
            let count = 257usize - usize::from(code);
            let bytes = count
                .checked_mul(pixel_bytes)
                .ok_or_else(|| "invalid raster literal".to_string())?;
            if bytes > row.len() - output {
                return Err("raster literal exceeds line length".into());
            }
            let literal = take(input, cursor, bytes)?;
            row[output..output + bytes].copy_from_slice(literal);
            output += bytes;
        }
    }
    Ok(())
}

fn validate_dimensions(width: usize, height: usize) -> Result<usize, String> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err("raster page dimensions are out of range".into());
    }
    width
        .checked_mul(height)
        .ok_or_else(|| "raster page dimensions overflow".to_string())
}

fn validate_dpi(dpi: u32) -> Result<(), String> {
    if dpi != RASTER_DPI {
        return Err("raster resolution is out of range".into());
    }
    Ok(())
}

fn exif_orientation(exif: &[u8]) -> u16 {
    if exif.len() < 8 {
        return 1;
    }
    let little_endian = match &exif[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return 1,
    };
    let read_u16 = |bytes: &[u8]| {
        let value = [bytes[0], bytes[1]];
        if little_endian {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        }
    };
    let read_u32 = |bytes: &[u8]| {
        let value = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if little_endian {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        }
    };
    if read_u16(&exif[2..4]) != 42 {
        return 1;
    }
    let Some(offset) = usize::try_from(read_u32(&exif[4..8])).ok() else {
        return 1;
    };
    let Some(count_bytes) = exif.get(offset..offset.saturating_add(2)) else {
        return 1;
    };
    let entries = usize::from(read_u16(count_bytes));
    let Some(entries_len) = entries.checked_mul(12) else {
        return 1;
    };
    let Some(table) = exif.get(offset + 2..offset.saturating_add(2).saturating_add(entries_len))
    else {
        return 1;
    };
    for entry in table.chunks_exact(12) {
        if read_u16(&entry[..2]) == 0x0112
            && read_u16(&entry[2..4]) == 3
            && read_u32(&entry[4..8]) == 1
        {
            let orientation = read_u16(&entry[8..10]);
            return if (1..=8).contains(&orientation) {
                orientation
            } else {
                1
            };
        }
    }
    1
}

fn orient_pixels(
    width: usize,
    height: usize,
    channels: usize,
    pixels: Vec<u8>,
    orientation: u16,
) -> Result<(usize, usize, Vec<u8>), String> {
    if orientation == 1 {
        return Ok((width, height, pixels));
    }
    let swaps_dimensions = matches!(orientation, 5..=8);
    let (output_width, output_height) = if swaps_dimensions {
        (height, width)
    } else {
        (width, height)
    };
    let mut output = Vec::new();
    output
        .try_reserve_exact(pixels.len())
        .map_err(|_| "unable to allocate JPEG image".to_string())?;
    output.resize(pixels.len(), 0);
    for y in 0..output_height {
        for x in 0..output_width {
            let (source_x, source_y) = match orientation {
                2 => (width - 1 - x, y),
                3 => (width - 1 - x, height - 1 - y),
                4 => (x, height - 1 - y),
                5 => (y, x),
                6 => (y, height - 1 - x),
                7 => (width - 1 - y, height - 1 - x),
                8 => (width - 1 - y, x),
                _ => return Ok((width, height, pixels)),
            };
            let source = (source_y * width + source_x) * channels;
            let destination = (y * output_width + x) * channels;
            output[destination..destination + channels]
                .copy_from_slice(&pixels[source..source + channels]);
        }
    }
    Ok((output_width, output_height, output))
}

fn read_u32_be(bytes: &[u8]) -> Result<u32, String> {
    let value = bytes
        .get(..4)
        .ok_or_else(|| "truncated document header".to_string())?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_byte(input: &[u8], cursor: &mut usize) -> Result<u8, String> {
    let byte = *input
        .get(*cursor)
        .ok_or_else(|| "truncated raster data".to_string())?;
    *cursor += 1;
    Ok(byte)
}

fn take<'a>(input: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| "document offset overflow".to_string())?;
    let bytes = input
        .get(*cursor..end)
        .ok_or_else(|| "truncated document data".to_string())?;
    *cursor = end;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    const JPEG: &str = "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAAqADAAQAAAABAAAAAQAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8AAEQgAAQACAwERAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAABAgMEBQYHCAkKC//EALUQAAIBAwMCBAMFBQQEAAABfQECAwAEEQUSITFBBhNRYQcicRQygZGhCCNCscEVUtHwJDNicoIJChYXGBkaJSYnKCkqNDU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6g4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2drh4uPk5ebn6Onq8fLz9PX29/j5+v/EAB8BAAMBAQEBAQEBAQEAAAAAAAABAgMEBQYHCAkKC//EALURAAIBAgQEAwQHBQQEAAECdwABAgMRBAUhMQYSQVEHYXETIjKBCBRCkaGxwQkjM1LwFWJy0QoWJDThJfEXGBkaJicoKSo1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoKDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uLj5OXm5+jp6vLz9PX29/j5+v/bAEMAAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAf/bAEMBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAf/dAAQAAf/aAAwDAQACEQMRAD8A9m+DH/JHvhR/2TXwL/6i+lV/zEcc/wDJbcYf9lTxB/6tsWf8yP7UX/lZl+0T/wC06vpcf+v/APEA/9k=";

    fn pwg_page_header(width: u32, height: u32, color_code: u32, channels: u32) -> Vec<u8> {
        let mut header = vec![0; PWG_HEADER_LEN];
        header[..9].copy_from_slice(b"PwgRaster");
        for (offset, value) in [
            (276, 300),
            (280, 300),
            (372, width),
            (376, height),
            (384, 8),
            (388, 8 * channels),
            (392, width * channels),
            (396, 0),
            (400, color_code),
            (420, channels),
        ] {
            header[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        header
    }

    fn encoded_pdf(input: &[u8]) -> Document {
        let bytes = normalize(input, "image/pwg-raster").unwrap();
        Document::load_mem(&bytes).unwrap()
    }

    #[test]
    fn pwg_srgb_rle_becomes_a_single_page_pdf_with_original_pixels() {
        let mut raster = b"RaS2".to_vec();
        raster.extend_from_slice(&pwg_page_header(2, 1, 19, 3));
        raster.extend_from_slice(&[0, 255, 255, 0, 0, 0, 255, 0]);

        let document = encoded_pdf(&raster);
        assert_eq!(document.get_pages().len(), 1);
        assert!(document.objects.values().any(|object| match object {
            Object::Stream(stream) => {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                    == Some(b"Image".as_slice())
                    && stream.get_plain_content().unwrap() == [255, 0, 0, 0, 255, 0]
            }
            _ => false,
        }));
    }

    #[test]
    fn pwg_pages_are_compressed_and_appended_incrementally() {
        let mut raster = b"RaS2".to_vec();
        raster.extend_from_slice(&pwg_page_header(1, 1, 18, 1));
        raster.extend_from_slice(&[0, 0, 64]);
        raster.extend_from_slice(&pwg_page_header(1, 1, 19, 3));
        raster.extend_from_slice(&[0, 0, 10, 20, 30]);

        let document = encoded_pdf(&raster);
        assert_eq!(document.get_pages().len(), 2);
        let image_data = document
            .objects
            .values()
            .filter_map(|object| match object {
                Object::Stream(stream)
                    if stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                        == Some(b"Image".as_slice()) =>
                {
                    Some(stream.get_plain_content().unwrap())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(image_data, [vec![64], vec![10, 20, 30]]);
    }

    #[test]
    fn urf_gray_decodes_line_repeats_and_white_fill() {
        let mut raster = b"UNIRAST\0".to_vec();
        raster.extend_from_slice(&1u32.to_be_bytes());
        let mut header = [0; URF_HEADER_LEN];
        header[0] = 8;
        header[1] = 0;
        header[12..16].copy_from_slice(&2u32.to_be_bytes());
        header[16..20].copy_from_slice(&2u32.to_be_bytes());
        header[20..24].copy_from_slice(&300u32.to_be_bytes());
        raster.extend_from_slice(&header);
        raster.extend_from_slice(&[1, 0, 0, 128]);

        let pdf = normalize(&raster, "image/urf").unwrap();
        let document = Document::load_mem(&pdf).unwrap();
        assert_eq!(document.get_pages().len(), 1);
        assert!(document.objects.values().any(|object| match object {
            Object::Stream(stream) => {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                    == Some(b"Image".as_slice())
                    && stream.get_plain_content().unwrap() == [0, 255, 0, 255]
            }
            _ => false,
        }));
    }

    #[test]
    fn jpeg_is_fully_decoded_before_pdf_creation() {
        let jpeg = STANDARD.decode(JPEG).unwrap();
        let pdf = normalize(&jpeg, "image/jpeg").unwrap();
        let document = Document::load_mem(&pdf).unwrap();
        assert_eq!(document.get_pages().len(), 1);
        assert!(document.objects.values().any(|object| match object {
            Object::Stream(stream) =>
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
                    == Some(b"Image".as_slice()),
            _ => false,
        }));
    }

    #[test]
    fn rejects_unknown_raster_modes_truncation_and_invalid_jpeg() {
        assert!(normalize(b"not a document", "image/jpeg").is_err());
        assert!(normalize(b"RaS2", "image/pwg-raster").is_err());
        let mut unsupported = b"RaS2".to_vec();
        unsupported.extend_from_slice(&pwg_page_header(1, 1, 6, 4));
        assert!(normalize(&unsupported, "image/pwg-raster").is_err());
        assert!(normalize(b"anything", "application/x-unknown").is_err());
    }

    #[test]
    fn raster_headers_and_runs_are_rejected_before_out_of_bounds_allocation() {
        for (offset, value) in [(372, 20_001u32), (376, 0), (276, 600), (280, 0)] {
            let mut header = pwg_page_header(1, 1, 18, 1);
            header[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            let mut raster = b"RaS2".to_vec();
            raster.extend_from_slice(&header);
            raster.extend_from_slice(&[0, 0, 64]);
            assert!(normalize(&raster, "image/pwg-raster").is_err());
        }
        for encoded in [&[0, 127, 64][..], &[255, 0, 64], &[0, 255, 64]] {
            let mut raster = b"RaS2".to_vec();
            raster.extend_from_slice(&pwg_page_header(1, 1, 18, 1));
            raster.extend_from_slice(encoded);
            assert!(normalize(&raster, "image/pwg-raster").is_err());
        }
        let jpeg = STANDARD.decode(JPEG).unwrap();
        assert!(normalize(&jpeg[..jpeg.len() / 2], "image/jpeg").is_err());
    }

    #[test]
    fn urf_declared_page_count_and_trailing_data_must_match() {
        let mut raster = b"UNIRAST\0".to_vec();
        raster.extend_from_slice(&2u32.to_be_bytes());
        let mut header = [0; URF_HEADER_LEN];
        header[0] = 8;
        header[12..16].copy_from_slice(&1u32.to_be_bytes());
        header[16..20].copy_from_slice(&1u32.to_be_bytes());
        header[20..24].copy_from_slice(&300u32.to_be_bytes());
        raster.extend_from_slice(&header);
        raster.extend_from_slice(&[0, 0, 64]);
        assert!(normalize(&raster, "image/urf").is_err());
        raster[8..12].copy_from_slice(&1u32.to_be_bytes());
        assert!(normalize(&raster, "image/urf").is_ok());
        raster.push(0);
        assert!(normalize(&raster, "image/urf").is_err());
    }

    #[test]
    fn applies_exif_orientation_with_transposed_dimensions() {
        let (width, height, pixels) = orient_pixels(2, 1, 1, vec![1, 2], 6).unwrap();
        assert_eq!((width, height), (1, 2));
        assert_eq!(pixels, [1, 2]);
    }
}
