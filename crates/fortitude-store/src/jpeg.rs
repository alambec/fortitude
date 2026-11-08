//! Decoder of sequential DCT-based JPEG with Huffman coding (ITU-T T.81 processes 1, 2 and 4:
//! baseline and extended), with 8 or 12 bits per sample. jpeg-decoder, used by dicom-pixeldata,
//! decodes only 8-bit DCT-based JPEG: this one decodes JPEG Extended with 12-bit samples.
//!
//! The inverse DCT, chroma upsampling and YCbCr to RGB conversion are those of the IJG library
//! and libjpeg-turbo with their default settings (accurate integer IDCT, "fancy" upsampling),
//! so decoded samples are identical to theirs.

/// Natural (row-major) position of each coefficient in zigzag order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Most samples (all components, before upsampling) a frame may hold: 512 MiB of 16-bit
/// samples, far above any DICOM image, against frame headers announcing absurd dimensions.
const MAX_SAMPLES: usize = 1 << 28;

/// A decoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// 1 (grayscale) or 3 (color).
    pub components: usize,
    /// Bits per sample: 8 or 12.
    pub precision: u8,
    /// Samples of each pixel, row by row; interleaved for color images.
    pub samples: Vec<u16>,
}

/// The frame header of a JPEG stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The SOFn marker (0xC0 to 0xCF): the coding process.
    pub marker: u8,
    pub precision: u8,
    pub width: u16,
    pub height: u16,
    pub components: u8,
}

/// The frame header of `data`, a JPEG stream; None when it has none before its first scan.
pub fn header(data: &[u8]) -> Option<Header> {
    let mut reader = Reader { data, pos: 0 };
    if reader.marker().ok()? != Some(SOI) {
        return None;
    }
    loop {
        let marker = reader.marker().ok()??;
        if marker == SOS || marker == EOI {
            return None;
        }
        let body = reader.segment().ok()?;
        if is_frame_header(marker) && body.len() >= 6 {
            return Some(Header {
                marker,
                precision: body[0],
                height: u16::from_be_bytes([body[1], body[2]]),
                width: u16::from_be_bytes([body[3], body[4]]),
                components: body[5],
            });
        }
    }
}

/// Decode `data`, a JPEG stream. Color images are converted from YCbCr to RGB when `ycbcr`.
pub fn decode(data: &[u8], ycbcr: bool) -> Result<Image, String> {
    let mut decoder = Decoder::default();
    let mut reader = Reader { data, pos: 0 };
    if reader.marker()? != Some(SOI) {
        return Err("not a JPEG stream (no SOI marker)".into());
    }
    loop {
        // A missing EOI marker (truncated stream) is tolerated once a scan was decoded.
        let marker = match reader.marker()? {
            Some(EOI) | None if decoder.scans > 0 => break,
            Some(EOI) | None => return Err("no scan in the JPEG stream".into()),
            Some(marker) => marker,
        };
        match marker {
            SOF0 | SOF1 => decoder.frame(reader.segment()?)?,
            m if is_frame_header(m) => {
                return Err(format!(
                    "unsupported JPEG coding process (SOF{}): only sequential DCT with Huffman coding",
                    m - SOF0
                ));
            }
            DHT => decoder.huffman_tables(reader.segment()?)?,
            DQT => decoder.quantization_tables(reader.segment()?)?,
            DRI => {
                let body = reader.segment()?;
                let interval = body.get(..2).ok_or("invalid DRI segment")?;
                decoder.restart_interval =
                    usize::from(u16::from_be_bytes([interval[0], interval[1]]));
            }
            SOS => {
                let body = reader.segment()?;
                reader.pos = decoder.scan(body, data, reader.pos)?;
            }
            DNL => return Err("unsupported DNL marker".into()),
            // RSTn outside of a scan, and TEM: no segment.
            RST0..=RST7 | TEM => {}
            // APPn, COM and others: skipped.
            _ => {
                reader.segment()?;
            }
        }
    }
    decoder.image(ycbcr)
}

const SOF0: u8 = 0xC0;
const SOF1: u8 = 0xC1;
const DHT: u8 = 0xC4;
const RST0: u8 = 0xD0;
const RST7: u8 = 0xD7;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DNL: u8 = 0xDC;
const DRI: u8 = 0xDD;
const TEM: u8 = 0x01;

/// SOF0 to SOF15, except DHT (0xC4), JPG (0xC8) and DAC (0xCC).
fn is_frame_header(marker: u8) -> bool {
    (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
}

/// Marker segments, outside of entropy-coded data.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// The next marker, skipping bytes before it and fill bytes; None at the end of the data.
    fn marker(&mut self) -> Result<Option<u8>, String> {
        loop {
            let Some(offset) = self.data[self.pos..].iter().position(|&b| b == 0xFF) else {
                self.pos = self.data.len();
                return Ok(None);
            };
            self.pos += offset;
            while self.data.get(self.pos) == Some(&0xFF) {
                self.pos += 1;
            }
            match self.data.get(self.pos) {
                None => return Ok(None),
                // A stuffed 0xFF: not a marker.
                Some(0) => self.pos += 1,
                Some(&marker) => {
                    self.pos += 1;
                    return Ok(Some(marker));
                }
            }
        }
    }

    /// The body of the segment starting here, after its length.
    fn segment(&mut self) -> Result<&'a [u8], String> {
        let length = self
            .data
            .get(self.pos..self.pos + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .filter(|&length| length >= 2)
            .ok_or("truncated or invalid marker segment")?;
        let body = self
            .data
            .get(self.pos + 2..self.pos + length)
            .ok_or("truncated marker segment")?;
        self.pos += length;
        Ok(body)
    }
}

#[derive(Default)]
struct Decoder {
    frame: Option<Frame>,
    /// Quantization tables, in natural order.
    quantization: [Option<[i32; 64]>; 4],
    dc_tables: [Option<Box<Huffman>>; 4],
    ac_tables: [Option<Box<Huffman>>; 4],
    /// MCUs between restart markers; 0 when there are none.
    restart_interval: usize,
    scans: usize,
}

struct Frame {
    precision: u8,
    width: usize,
    height: usize,
    max_h: usize,
    max_v: usize,
    /// MCUs of interleaved scans, across and down.
    mcus_across: usize,
    mcus_down: usize,
    components: Vec<Component>,
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    quantization_table: usize,
    /// The quantization table, as it was at the component's first scan.
    quantization: Option<[i32; 64]>,
    /// Size of the component, in samples, before upsampling.
    width: usize,
    height: usize,
    /// Samples of the component, in rows of `stride` samples: whole MCUs.
    stride: usize,
    plane: Vec<u16>,
}

impl Decoder {
    fn frame(&mut self, body: &[u8]) -> Result<(), String> {
        if self.frame.is_some() {
            return Err("more than one frame header".into());
        }
        let [precision, h1, h0, w1, w0, count, ref specs @ ..] = *body else {
            return Err("truncated frame header".into());
        };
        if precision != 8 && precision != 12 {
            return Err(format!("unsupported sample precision {precision}"));
        }
        let height = usize::from(u16::from_be_bytes([h1, h0]));
        let width = usize::from(u16::from_be_bytes([w1, w0]));
        if height == 0 {
            return Err("unsupported frame height defined by a DNL marker".into());
        }
        if width == 0 {
            return Err("frame width is 0".into());
        }
        let count = usize::from(count);
        if count != 1 && count != 3 {
            return Err(format!("unsupported number of components {count}"));
        }
        if specs.len() < count * 3 {
            return Err("truncated frame header".into());
        }
        let mut components = Vec::with_capacity(count);
        for spec in specs.chunks_exact(3).take(count) {
            let (h, v) = (usize::from(spec[1] >> 4), usize::from(spec[1] & 15));
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return Err(format!("invalid sampling factors {h}x{v}"));
            }
            if spec[2] > 3 {
                return Err(format!("invalid quantization table {}", spec[2]));
            }
            if components.iter().any(|c: &Component| c.id == spec[0]) {
                return Err(format!("duplicate component {}", spec[0]));
            }
            components.push(Component {
                id: spec[0],
                h,
                v,
                quantization_table: usize::from(spec[2]),
                quantization: None,
                width: 0,
                height: 0,
                stride: 0,
                plane: Vec::new(),
            });
        }
        let max_h = components.iter().map(|c| c.h).max().unwrap_or(1);
        let max_v = components.iter().map(|c| c.v).max().unwrap_or(1);
        let mcus_across = width.div_ceil(8 * max_h);
        let mcus_down = height.div_ceil(8 * max_v);
        let mut samples = 0;
        for c in &mut components {
            c.width = (width * c.h).div_ceil(max_h);
            c.height = (height * c.v).div_ceil(max_v);
            c.stride = mcus_across * c.h * 8;
            samples += c.stride * mcus_down * c.v * 8;
        }
        if samples > MAX_SAMPLES {
            return Err(format!("frame of {width}x{height} is too large"));
        }
        for c in &mut components {
            c.plane = vec![0; c.stride * mcus_down * c.v * 8];
        }
        self.frame = Some(Frame {
            precision,
            width,
            height,
            max_h,
            max_v,
            mcus_across,
            mcus_down,
            components,
        });
        Ok(())
    }

    fn huffman_tables(&mut self, mut body: &[u8]) -> Result<(), String> {
        while let [class_id, ref rest @ ..] = *body {
            let counts: [u8; 16] = rest
                .get(..16)
                .and_then(|c| c.try_into().ok())
                .ok_or("truncated DHT segment")?;
            let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
            let values = rest.get(16..16 + total).ok_or("truncated DHT segment")?;
            let (class, id) = (class_id >> 4, usize::from(class_id & 15));
            if class > 1 || id > 3 {
                return Err(format!("invalid Huffman table {class_id:#04x}"));
            }
            let table = Some(Box::new(Huffman::new(&counts, values)?));
            if class == 0 {
                self.dc_tables[id] = table;
            } else {
                self.ac_tables[id] = table;
            }
            body = &rest[16 + total..];
        }
        Ok(())
    }

    fn quantization_tables(&mut self, mut body: &[u8]) -> Result<(), String> {
        while let [precision_id, ref rest @ ..] = *body {
            let (precision, id) = (precision_id >> 4, usize::from(precision_id & 15));
            if precision > 1 || id > 3 {
                return Err(format!("invalid quantization table {precision_id:#04x}"));
            }
            let size = if precision == 0 { 64 } else { 128 };
            let values = rest.get(..size).ok_or("truncated DQT segment")?;
            let mut table = [0; 64];
            for (k, &position) in ZIGZAG.iter().enumerate() {
                table[position] = if precision == 0 {
                    i32::from(values[k])
                } else {
                    i32::from(u16::from_be_bytes([values[2 * k], values[2 * k + 1]]))
                };
            }
            self.quantization[id] = Some(table);
            body = &rest[size..];
        }
        Ok(())
    }

    /// Decode the scan whose header is `body` and whose entropy-coded data starts at `start`
    /// in `data`; returns the position after it.
    fn scan(&mut self, body: &[u8], data: &[u8], start: usize) -> Result<usize, String> {
        let frame = self.frame.as_mut().ok_or("scan before the frame header")?;
        let count = usize::from(*body.first().ok_or("truncated scan header")?);
        if !(1..=4).contains(&count) || body.len() < 1 + 2 * count + 3 {
            return Err("invalid scan header".into());
        }
        let mut scan = Vec::with_capacity(count);
        for spec in body[1..1 + 2 * count].chunks_exact(2) {
            let index = frame
                .components
                .iter()
                .position(|c| c.id == spec[0])
                .ok_or_else(|| format!("scan of unknown component {}", spec[0]))?;
            let (dc, ac) = (usize::from(spec[1] >> 4), usize::from(spec[1] & 15));
            let dc = self.dc_tables.get(dc).and_then(Option::as_deref);
            let ac = self.ac_tables.get(ac).and_then(Option::as_deref);
            let (Some(dc), Some(ac)) = (dc, ac) else {
                return Err("scan uses an undefined Huffman table".into());
            };
            let component = &mut frame.components[index];
            if component.quantization.is_none() {
                component.quantization = Some(
                    self.quantization[component.quantization_table]
                        .ok_or("component uses an undefined quantization table")?,
                );
            }
            scan.push(ScanComponent {
                index,
                dc,
                ac,
                prediction: 0,
            });
        }
        let selection = &body[1 + 2 * count..];
        if selection[..3] != [0, 63, 0] {
            return Err(
                "invalid spectral selection or successive approximation for a sequential scan"
                    .into(),
            );
        }
        let interleaved = count > 1;
        if interleaved
            && scan
                .iter()
                .map(|s| frame.components[s.index].h * frame.components[s.index].v)
                .sum::<usize>()
                > 10
        {
            return Err("more than 10 blocks per MCU".into());
        }
        let (mcus_across, mcus_down) = if interleaved {
            (frame.mcus_across, frame.mcus_down)
        } else {
            // A non-interleaved scan holds the component's blocks only, one per MCU.
            let c = &frame.components[scan[0].index];
            (c.width.div_ceil(8), c.height.div_ceil(8))
        };
        let (pass1_bits, max) = (
            if frame.precision == 8 { 2 } else { 1 },
            (1 << frame.precision) - 1,
        );
        let mut bits = Bits::new(data, start);
        let mut coefficients = [0i32; 64];
        for mcu in 0..mcus_across * mcus_down {
            if self.restart_interval > 0 && mcu > 0 && mcu % self.restart_interval == 0 {
                bits.restart();
                for s in &mut scan {
                    s.prediction = 0;
                }
            }
            let (row, column) = (mcu / mcus_across, mcu % mcus_across);
            for s in &mut scan {
                let c = &mut frame.components[s.index];
                let (blocks_h, blocks_v) = if interleaved { (c.h, c.v) } else { (1, 1) };
                let quantization = c.quantization.as_ref().ok_or("no quantization table")?;
                for v in 0..blocks_v {
                    for h in 0..blocks_h {
                        coefficients.fill(0);
                        let last = bits.block(s.dc, s.ac, &mut s.prediction, &mut coefficients)?;
                        let x = (column * blocks_h + h) * 8;
                        let y = (row * blocks_v + v) * 8;
                        let out = &mut c.plane[y * c.stride + x..];
                        if last == 0 {
                            dc_block(
                                coefficients[0] * quantization[0],
                                out,
                                c.stride,
                                pass1_bits,
                                max,
                            );
                        } else {
                            idct(&coefficients, quantization, out, c.stride, pass1_bits, max);
                        }
                    }
                }
            }
        }
        self.scans += 1;
        Ok(bits.pos)
    }

    fn image(&self, ycbcr: bool) -> Result<Image, String> {
        let frame = self.frame.as_ref().ok_or("no frame header")?;
        let planes: Vec<Vec<u16>> = frame
            .components
            .iter()
            .map(|c| upsample(frame, c))
            .collect::<Result<_, _>>()?;
        let pixels = frame.width * frame.height;
        let samples = match planes.as_slice() {
            [gray] => gray.clone(),
            [a, b, c] => {
                let max = (1i64 << frame.precision) - 1;
                let mut samples = Vec::with_capacity(pixels * 3);
                for i in 0..pixels {
                    if ycbcr {
                        samples.extend(ycbcr_to_rgb(a[i], b[i], c[i], max));
                    } else {
                        samples.extend([a[i], b[i], c[i]]);
                    }
                }
                samples
            }
            _ => return Err("unsupported number of components".into()),
        };
        Ok(Image {
            width: frame.width,
            height: frame.height,
            components: planes.len(),
            precision: frame.precision,
            samples,
        })
    }
}

struct ScanComponent<'a> {
    index: usize,
    dc: &'a Huffman,
    ac: &'a Huffman,
    prediction: i32,
}

/// Bits of a code looked up in one step.
const LOOKUP_BITS: u32 = 9;

struct Huffman {
    /// For each `LOOKUP_BITS`-bit prefix, the length (high byte) and value (low byte) of the
    /// code it starts with; 0 when the code is longer.
    lookup: [u16; 1 << LOOKUP_BITS],
    /// Largest code of each length; -1 when there is none.
    max_code: [i32; 17],
    /// Index in `values` of a code of each length, minus that code.
    offset: [i32; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: &[u8]) -> Result<Self, String> {
        let mut table = Huffman {
            lookup: [0; 1 << LOOKUP_BITS],
            max_code: [-1; 17],
            offset: [0; 17],
            values: values.to_vec(),
        };
        let (mut code, mut k) = (0i32, 0usize);
        for length in 1..=16 {
            let count = usize::from(counts[length - 1]);
            table.offset[length] = k as i32 - code;
            for _ in 0..count {
                if code >= 1 << length {
                    return Err("invalid Huffman table".into());
                }
                if length as u32 <= LOOKUP_BITS {
                    let shift = LOOKUP_BITS - length as u32;
                    let first = (code as usize) << shift;
                    let entry = ((length as u16) << 8) | u16::from(values[k]);
                    table.lookup[first..first + (1 << shift)].fill(entry);
                }
                code += 1;
                k += 1;
            }
            if count > 0 {
                table.max_code[length] = code - 1;
            }
            code <<= 1;
        }
        Ok(table)
    }
}

/// Entropy-coded data: bits, without stuffed bytes, until a marker.
struct Bits<'a> {
    data: &'a [u8],
    /// Next byte to read.
    pos: usize,
    /// Bits read, from the most significant one.
    buffer: u64,
    count: u32,
    /// Whether a marker (or the end of the data) was reached: zeros are read from then on.
    marker: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Bits {
            data,
            pos,
            buffer: 0,
            count: 0,
            marker: false,
        }
    }

    fn fill(&mut self) {
        while self.count <= 56 {
            let byte = if self.marker {
                0
            } else {
                match self.data.get(self.pos) {
                    Some(0xFF) if self.data.get(self.pos + 1) == Some(&0) => {
                        self.pos += 2;
                        0xFF
                    }
                    Some(0xFF) | None => {
                        self.marker = true;
                        0
                    }
                    Some(&byte) => {
                        self.pos += 1;
                        byte
                    }
                }
            };
            self.buffer |= u64::from(byte) << (56 - self.count);
            self.count += 8;
        }
    }

    fn consume(&mut self, n: u32) {
        self.buffer <<= n;
        self.count -= n;
    }

    /// Skip the bits left before a restart marker, and the marker.
    fn restart(&mut self) {
        self.buffer = 0;
        self.count = 0;
        self.marker = false;
        while self.data.get(self.pos) == Some(&0xFF) && self.data.get(self.pos + 1) == Some(&0xFF) {
            self.pos += 1;
        }
        if self.data.get(self.pos) == Some(&0xFF)
            && self
                .data
                .get(self.pos + 1)
                .is_some_and(|m| (RST0..=RST7).contains(m))
        {
            self.pos += 2;
        }
    }

    fn decode(&mut self, table: &Huffman) -> Result<u8, String> {
        if self.count < 16 {
            self.fill();
        }
        let entry = table.lookup[(self.buffer >> (64 - LOOKUP_BITS)) as usize];
        if entry != 0 {
            self.consume(u32::from(entry >> 8));
            return Ok(entry as u8);
        }
        for length in LOOKUP_BITS + 1..=16 {
            let code = (self.buffer >> (64 - length)) as i32;
            if code <= table.max_code[length as usize] {
                self.consume(length);
                return table
                    .values
                    .get((code + table.offset[length as usize]) as usize)
                    .copied()
                    .ok_or_else(|| "invalid Huffman table".into());
            }
        }
        Err("invalid Huffman code".into())
    }

    /// The `size`-bit value that follows, as a signed coefficient (T.81 F.2.2.1).
    fn receive_extend(&mut self, size: u8) -> i32 {
        let size = u32::from(size.min(16));
        if size == 0 {
            return 0;
        }
        if self.count < size {
            self.fill();
        }
        let value = (self.buffer >> (64 - size)) as i32;
        self.consume(size);
        if value < 1 << (size - 1) {
            value - (1 << size) + 1
        } else {
            value
        }
    }

    /// Decode a block's coefficients, in natural order, not dequantized; returns the zigzag
    /// index of the last one decoded (0 when the block only has its DC coefficient).
    fn block(
        &mut self,
        dc: &Huffman,
        ac: &Huffman,
        prediction: &mut i32,
        coefficients: &mut [i32; 64],
    ) -> Result<usize, String> {
        let size = self.decode(dc)?;
        *prediction += self.receive_extend(size);
        // Coefficients are 16-bit in the IJG library (JCOEF).
        coefficients[0] = i32::from(*prediction as i16);
        let (mut k, mut last) = (1, 0);
        while k < 64 {
            let run_size = self.decode(ac)?;
            let (run, size) = (usize::from(run_size >> 4), run_size & 15);
            if size == 0 {
                if run != 15 {
                    break; // end of block
                }
                k += 16;
                continue;
            }
            k += run;
            if k > 63 {
                return Err("invalid AC coefficient index".into());
            }
            coefficients[ZIGZAG[k]] = i32::from(self.receive_extend(size) as i16);
            last = k;
            k += 1;
        }
        Ok(last)
    }
}

/// Clamp a sample of the inverse DCT, before level shift, as the IJG range limit table does:
/// values out of range by more than twice the sample range wrap around.
fn limit(x: i64, max: i32) -> u16 {
    let range = max + 1;
    let v = (x as i32) & (4 * range - 1);
    let v = if v >= 2 * range { v - 4 * range } else { v };
    (v + range / 2).clamp(0, max) as u16
}

const CONST_BITS: u32 = 13;
const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;

fn descale(x: i64, n: u32) -> i64 {
    (x + (1 << (n - 1))) >> n
}

/// The samples of a block whose only coefficient is the (dequantized) DC one.
fn dc_block(dc: i32, out: &mut [u16], stride: usize, pass1_bits: u32, max: i32) {
    let value = limit(descale(i64::from(dc) << pass1_bits, pass1_bits + 3), max);
    for row in 0..8 {
        out[row * stride..row * stride + 8].fill(value);
    }
}

/// The 1-D inverse DCT of the IJG library's accurate integer method (jidctint.c, after the
/// LL&M algorithm): the even and odd parts of 8 inputs.
#[inline(always)]
fn idct_1d(i: [i64; 8]) -> ([i64; 4], [i64; 4]) {
    let z1 = (i[2] + i[6]) * FIX_0_541196100;
    let tmp2 = z1 - i[6] * FIX_1_847759065;
    let tmp3 = z1 + i[2] * FIX_0_765366865;
    let tmp0 = (i[0] + i[4]) << CONST_BITS;
    let tmp1 = (i[0] - i[4]) << CONST_BITS;
    let even = [tmp0 + tmp3, tmp1 + tmp2, tmp1 - tmp2, tmp0 - tmp3];

    let (t0, t1, t2, t3) = (i[7], i[5], i[3], i[1]);
    let z1 = t0 + t3;
    let z2 = t1 + t2;
    let z3 = t0 + t2;
    let z4 = t1 + t3;
    let z5 = (z3 + z4) * FIX_1_175875602;
    let t0 = t0 * FIX_0_298631336;
    let t1 = t1 * FIX_2_053119869;
    let t2 = t2 * FIX_3_072711026;
    let t3 = t3 * FIX_1_501321110;
    let z1 = -z1 * FIX_0_899976223;
    let z2 = -z2 * FIX_2_562915447;
    let z3 = -z3 * FIX_1_961570560 + z5;
    let z4 = -z4 * FIX_0_390180644 + z5;
    // Pairs with even[0..4]: outputs 0/7, 1/6, 2/5, 3/4.
    let odd = [t3 + z1 + z4, t2 + z2 + z3, t1 + z2 + z4, t0 + z1 + z3];
    (even, odd)
}

/// Dequantize and inverse transform a block into 8 rows of `out`.
fn idct(
    coefficients: &[i32; 64],
    quantization: &[i32; 64],
    out: &mut [u16],
    stride: usize,
    pass1_bits: u32,
    max: i32,
) {
    let mut workspace = [0i32; 64];
    // Columns.
    for column in 0..8 {
        let input: [i64; 8] = std::array::from_fn(|row| {
            i64::from(coefficients[row * 8 + column]) * i64::from(quantization[row * 8 + column])
        });
        if input[1..].iter().all(|&v| v == 0) {
            let dc = (input[0] << pass1_bits) as i32;
            for row in 0..8 {
                workspace[row * 8 + column] = dc;
            }
            continue;
        }
        let (even, odd) = idct_1d(input);
        let shift = CONST_BITS - pass1_bits;
        for k in 0..4 {
            workspace[k * 8 + column] = descale(even[k] + odd[k], shift) as i32;
            workspace[(7 - k) * 8 + column] = descale(even[k] - odd[k], shift) as i32;
        }
    }
    // Rows.
    let shift = CONST_BITS + pass1_bits + 3;
    for row in 0..8 {
        let input: [i64; 8] = std::array::from_fn(|k| i64::from(workspace[row * 8 + k]));
        let line = &mut out[row * stride..row * stride + 8];
        if input[1..].iter().all(|&v| v == 0) {
            line.fill(limit(descale(input[0], pass1_bits + 3), max));
            continue;
        }
        let (even, odd) = idct_1d(input);
        for k in 0..4 {
            line[k] = limit(descale(even[k] + odd[k], shift), max);
            line[7 - k] = limit(descale(even[k] - odd[k], shift), max);
        }
    }
}

/// A component's samples at the frame's size, upsampled as libjpeg-turbo does by default
/// (jdsample.c): "fancy" (triangle filter) upsampling by 2 horizontally, vertically or both,
/// sample replication otherwise. Edge samples are replicated.
fn upsample(frame: &Frame, c: &Component) -> Result<Vec<u16>, String> {
    let (width, height) = (frame.width, frame.height);
    if !frame.max_h.is_multiple_of(c.h) || !frame.max_v.is_multiple_of(c.v) {
        return Err(format!(
            "unsupported sampling factors {}x{} with {}x{}",
            c.h, c.v, frame.max_h, frame.max_v
        ));
    }
    let (h_expand, v_expand) = (frame.max_h / c.h, frame.max_v / c.v);
    let at = |x: usize, y: usize| -> i32 {
        i32::from(c.plane[y.min(c.height - 1) * c.stride + x.min(c.width - 1)])
    };
    let mut out = Vec::with_capacity(width * height);
    match (h_expand, v_expand) {
        (1, 1) => {
            for y in 0..height {
                out.extend_from_slice(&c.plane[y * c.stride..y * c.stride + width]);
            }
        }
        (2, 1) if c.width > 2 => {
            for y in 0..height {
                for x in 0..width {
                    let (sx, right) = (x / 2, x % 2 == 1);
                    let this = at(sx, y) * 3;
                    out.push(if right {
                        (this + at(sx + 1, y) + 2) >> 2
                    } else {
                        (this + at(sx.saturating_sub(1), y) + 1) >> 2
                    } as u16);
                }
            }
        }
        (1, 2) => {
            for y in 0..height {
                let (sy, below) = (y / 2, y % 2 == 1);
                let (near, bias) = if below {
                    (sy + 1, 2)
                } else {
                    (sy.saturating_sub(1), 1)
                };
                for x in 0..width {
                    out.push(((at(x, sy) * 3 + at(x, near) + bias) >> 2) as u16);
                }
            }
        }
        (2, 2) if c.width > 2 => {
            for y in 0..height {
                let (sy, below) = (y / 2, y % 2 == 1);
                let near = if below { sy + 1 } else { sy.saturating_sub(1) };
                let column = |sx: usize| at(sx, sy) * 3 + at(sx, near);
                for x in 0..width {
                    let (sx, right) = (x / 2, x % 2 == 1);
                    let this = column(sx) * 3;
                    out.push(if right {
                        (this + column(sx + 1) + 7) >> 4
                    } else {
                        (this + column(sx.saturating_sub(1)) + 8) >> 4
                    } as u16);
                }
            }
        }
        _ => {
            for y in 0..height {
                for x in 0..width {
                    out.push(c.plane[(y / v_expand) * c.stride + x / h_expand]);
                }
            }
        }
    }
    Ok(out)
}

/// YCbCr to RGB (JFIF), with the fixed-point arithmetic of the IJG library (jdcolor.c).
fn ycbcr_to_rgb(y: u16, cb: u16, cr: u16, max: i64) -> [u16; 3] {
    const SCALE_BITS: u32 = 16;
    const ONE_HALF: i64 = 1 << (SCALE_BITS - 1);
    const FIX_1_40200: i64 = 91881;
    const FIX_1_77200: i64 = 116130;
    const FIX_0_71414: i64 = 46802;
    const FIX_0_34414: i64 = 22554;
    let center = (max + 1) / 2;
    let (y, cb, cr) = (i64::from(y), i64::from(cb) - center, i64::from(cr) - center);
    let red = (FIX_1_40200 * cr + ONE_HALF) >> SCALE_BITS;
    let blue = (FIX_1_77200 * cb + ONE_HALF) >> SCALE_BITS;
    let green = (-FIX_0_34414 * cb + ONE_HALF - FIX_0_71414 * cr) >> SCALE_BITS;
    [y + red, y + green, y + blue].map(|v| v.clamp(0, max) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fortitude_testkit::jpeg::{FIXTURES, fixture};

    /// Every fixture, encoded and decoded by libjpeg-turbo, decodes to the same samples.
    #[test]
    fn matches_libjpeg_turbo() {
        for &name in FIXTURES {
            let (jpeg, expected) = fixture(name);
            // RGB streams are not converted, as DICOM's RGB photometric interpretation says.
            let ycbcr = !name.contains("rgb");
            let image = decode(&jpeg, ycbcr).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                (image.width, image.height, image.components),
                (expected.width, expected.height, expected.components),
                "{name}"
            );
            assert_eq!(
                image.precision,
                if name.starts_with("8-") { 8 } else { 12 },
                "{name}"
            );
            let differences = image
                .samples
                .iter()
                .zip(&expected.samples)
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                differences, 0,
                "{name}: samples differ from libjpeg-turbo's"
            );
        }
    }

    #[test]
    fn reads_the_frame_header() {
        let (jpeg, _) = fixture("12-gray");
        let header = header(&jpeg).expect("frame header");
        assert_eq!(header.marker, SOF1);
        assert_eq!(header.precision, 12);
        assert_eq!(
            (header.width, header.height, header.components),
            (61, 45, 1)
        );
        assert_eq!(super::header(b"not a JPEG"), None);
    }

    /// Without the conversion, color samples are those of the stream (YCbCr).
    #[test]
    fn keeps_ycbcr() {
        let (jpeg, expected) = fixture("12-ycbcr-444");
        let rgb = decode(&jpeg, true).expect("decoded");
        let ycbcr = decode(&jpeg, false).expect("decoded");
        assert_eq!(rgb.samples, expected.samples);
        let max = (1 << 12) - 1;
        for (pixel, rgb) in ycbcr
            .samples
            .chunks_exact(3)
            .zip(rgb.samples.chunks_exact(3))
        {
            assert_eq!(ycbcr_to_rgb(pixel[0], pixel[1], pixel[2], max), rgb);
        }
    }

    #[test]
    fn tolerates_a_missing_eoi() {
        let (jpeg, expected) = fixture("12-gray");
        assert_eq!(&jpeg[jpeg.len() - 2..], [0xFF, 0xD9]);
        let image = decode(&jpeg[..jpeg.len() - 2], true).expect("decoded");
        assert_eq!(image.samples, expected.samples);
    }

    #[test]
    fn rejects_invalid_streams() {
        let (jpeg, _) = fixture("12-gray");
        // Truncated in the middle of its headers.
        assert!(decode(&jpeg[..40], true).is_err());
        assert!(decode(b"\xFF\xD8\xFF\xD9", true).is_err());
        assert!(decode(b"", true).is_err());
        // Progressive (SOF2) instead of extended sequential.
        let sof = jpeg
            .windows(2)
            .position(|w| w == [0xFF, SOF1])
            .expect("SOF1");
        let mut progressive = jpeg.clone();
        progressive[sof + 1] = 0xC2;
        let error = decode(&progressive, true).expect_err("progressive");
        assert!(error.contains("SOF2"), "{error}");
        // Every truncation fails cleanly or decodes (missing data reads as zeros).
        for length in 0..jpeg.len() {
            let _ = decode(&jpeg[..length], true);
        }
    }

    #[test]
    fn rejects_huge_frames() {
        // A frame header announcing 65535x65535 pixels, in 3 components.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xC1, 0, 17, 12, 0xFF, 0xFF, 0xFF, 0xFF, 3];
        for id in 1..=3 {
            jpeg.extend([id, 0x11, 0]);
        }
        let error = decode(&jpeg, true).expect_err("too large");
        assert!(error.contains("too large"), "{error}");
    }

    #[test]
    fn idct_of_a_dc_block() {
        // The general IDCT of a block with only a DC coefficient gives the shortcut's values.
        let mut coefficients = [0; 64];
        let quantization = [3; 64];
        for (dc, precision) in [(-700, 12), (300, 12), (0, 12), (-100, 8), (40, 8)] {
            coefficients[0] = dc;
            let (pass1_bits, max) = (if precision == 8 { 2 } else { 1 }, (1 << precision) - 1);
            let (mut general, mut shortcut) = ([0u16; 64], [0u16; 64]);
            idct(
                &coefficients,
                &quantization,
                &mut general,
                8,
                pass1_bits,
                max,
            );
            dc_block(dc * 3, &mut shortcut, 8, pass1_bits, max);
            assert_eq!(general, shortcut, "DC {dc} at {precision} bits");
        }
    }

    #[test]
    fn range_limit() {
        let max = 4095;
        assert_eq!(limit(0, max), 2048);
        assert_eq!(limit(-2048, max), 0);
        assert_eq!(limit(2047, max), 4095);
        assert_eq!(limit(5000, max), 4095);
        assert_eq!(limit(-5000, max), 0);
        // Wrapped around, as libjpeg's table does.
        assert_eq!(limit(4 * 4096, max), 2048);
    }
}
