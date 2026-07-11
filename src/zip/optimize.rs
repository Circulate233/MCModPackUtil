pub fn minify_json_bytes(input: &[u8]) -> serde_json::Result<Vec<u8>> {
    if input.iter().all(u8::is_ascii_whitespace) {
        return Ok(input.to_vec());
    }
    let value: serde_json::Value = serde_json::from_slice(input)?;
    serde_json::to_vec(&value)
}

pub fn optimize_png_bytes(input: &[u8]) -> oxipng::PngResult<Vec<u8>> {
    oxipng::optimize_from_memory(input, &oxipng::Options::from_preset(2))
}
