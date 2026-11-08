//! JPEG streams encoded by libjpeg-turbo, with the samples its decoder gives
//! (`data/jpeg/generate.sh`): 12-bit (JPEG Extended) and 8-bit, grayscale and color, with
//! every chroma subsampling, restart markers, 16-bit quantization tables and one scan per
//! component.

/// Fixture names: `<precision>-<color space>[-<variant>]`.
pub const FIXTURES: &[&str] = &[
    "12-gray",
    "12-gray-restart",
    "12-ycbcr-420",
    "12-ycbcr-422",
    "12-ycbcr-440",
    "12-ycbcr-411",
    "12-ycbcr-444",
    "12-ycbcr-scans",
    "12-ycbcr-420-narrow",
    "12-rgb",
    "8-ycbcr-420",
];

/// Samples decoded by libjpeg-turbo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub width: usize,
    pub height: usize,
    /// 1 (grayscale) or 3 (RGB, converted from YCbCr unless the stream is RGB).
    pub components: usize,
    /// Samples of each pixel, row by row, interleaved.
    pub samples: Vec<u16>,
}

impl Decoded {
    /// The samples as little endian 16-bit words, as in native DICOM pixel data.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        self.samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
}

/// The JPEG stream of fixture `name`, and its samples decoded by libjpeg-turbo.
pub fn fixture(name: &str) -> (Vec<u8>, Decoded) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data/jpeg");
    let read = |file: String| {
        std::fs::read(format!("{dir}/{file}")).unwrap_or_else(|e| panic!("{file}: {e}"))
    };
    let jpeg = read(format!("{name}.jpg"));
    let decoded = if name.contains("gray") {
        pnm(&read(format!("{name}.pgm")))
    } else {
        pnm(&read(format!("{name}.ppm")))
    };
    (jpeg, decoded)
}

/// A binary PGM (P5) or PPM (P6) image.
fn pnm(data: &[u8]) -> Decoded {
    let mut fields = Vec::new();
    let mut pos = 0;
    while fields.len() < 4 {
        while data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while !data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&data[start..pos]).expect("PNM header"));
    }
    let raster = &data[pos + 1..];
    let components = match fields[0] {
        "P5" => 1,
        "P6" => 3,
        magic => panic!("not a binary PGM or PPM: {magic}"),
    };
    let number = |field: &str| field.parse::<usize>().expect("PNM header number");
    let (width, height, max) = (number(fields[1]), number(fields[2]), number(fields[3]));
    let samples: Vec<u16> = if max > 255 {
        raster
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect()
    } else {
        raster.iter().map(|&b| u16::from(b)).collect()
    };
    assert_eq!(
        samples.len(),
        width * height * components,
        "PNM raster size"
    );
    Decoded {
        width,
        height,
        components,
        samples,
    }
}
