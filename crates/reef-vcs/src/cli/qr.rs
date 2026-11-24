//! A QR code (ISO/IEC 18004) of a short text for a terminal (R3.40): byte
//! mode, error correction level L, versions 1 to 5, which hold 106 bytes in
//! one block. A longer text has none.

/// `(data codewords, error correction codewords)` of versions 1 to 5 at L.
const VERSIONS: [(usize, usize); 5] = [(19, 7), (34, 10), (55, 15), (80, 20), (108, 26)];

/// The eight masks a code may be drawn with; a light module under one is
/// drawn dark, and a dark one light.
const MASKS: [fn(usize, usize) -> bool; 8] = [
    |row, col| (row + col).is_multiple_of(2),
    |row, _| row.is_multiple_of(2),
    |_, col| col.is_multiple_of(3),
    |row, col| (row + col).is_multiple_of(3),
    |row, col| (row / 2 + col / 3).is_multiple_of(2),
    |row, col| (row * col) % 2 + (row * col) % 3 == 0,
    |row, col| ((row * col) % 2 + (row * col) % 3).is_multiple_of(2),
    |row, col| ((row + col) % 2 + (row * col) % 3).is_multiple_of(2),
];

/// The light modules around a code that a reader needs.
const QUIET: usize = 4;

/// A code's modules, dark ones true.
#[derive(Clone, Debug)]
struct Qr {
    size: usize,
    dark: Vec<bool>,
    /// Modules that hold no data: the patterns and the format.
    fixed: Vec<bool>,
}

/// `a` times `b` in the code's field, GF(256) over 0x11D.
fn times(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b > 0 {
        if b & 1 == 1 {
            product ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    product
}

/// The `count` Reed-Solomon codewords that follow `data`.
fn correction(data: &[u8], count: usize) -> Vec<u8> {
    // The generator polynomial's coefficients, highest power first, without
    // its leading 1.
    let mut generator = vec![0; count];
    generator[count - 1] = 1;
    let mut root = 1;
    for _ in 0..count {
        for index in 0..count {
            generator[index] = times(generator[index], root)
                ^ generator.get(index + 1).copied().unwrap_or_default();
        }
        root = times(root, 2);
    }
    let mut remainder = vec![0; count];
    for byte in data {
        let factor = byte ^ remainder.remove(0);
        remainder.push(0);
        for (slot, coefficient) in remainder.iter_mut().zip(&generator) {
            *slot ^= times(*coefficient, factor);
        }
    }
    remainder
}

/// The text's data codewords, filled to `capacity`: byte mode, its length,
/// the bytes, a terminator, and the padding bytes.
fn codewords(text: &[u8], length: u8, capacity: usize) -> Vec<u8> {
    let mut words = Vec::with_capacity(capacity);
    let mut carry = 0b0100;
    for byte in std::iter::once(&length).chain(text) {
        words.push(carry << 4 | byte >> 4);
        carry = byte & 0x0F;
    }
    words.push(carry << 4);
    for pad in [0xEC, 0x11].into_iter().cycle() {
        if words.len() >= capacity {
            break;
        }
        words.push(pad);
    }
    words
}

/// The 15 format bits for level L and `mask`, highest first.
fn format_bits(mask: u32) -> u32 {
    let data = 0b01 << 3 | mask;
    let mut remainder = data;
    for _ in 0..10 {
        remainder = (remainder << 1) ^ ((remainder >> 9) * 0b101_0011_0111);
    }
    (data << 10 | remainder) ^ 0b101_0100_0001_0010
}

impl Qr {
    fn at(&self, row: usize, col: usize) -> bool {
        self.dark[row * self.size + col]
    }

    fn set(&mut self, row: usize, col: usize, dark: bool) {
        self.dark[row * self.size + col] = dark;
        self.fixed[row * self.size + col] = true;
    }

    /// A finder or an alignment pattern's rings around (`row`, `col`), out to
    /// `radius`; a ring off the code is not drawn.
    fn rings(&mut self, row: usize, col: usize, radius: usize, light: &[usize]) {
        let range = |centre: usize| centre.saturating_sub(radius)..=centre + radius;
        for r in range(row) {
            for c in range(col) {
                if r < self.size && c < self.size {
                    let ring = r.abs_diff(row).max(c.abs_diff(col));
                    self.set(r, c, !light.contains(&ring));
                }
            }
        }
    }

    /// Everything but the data: finders, alignment, timing, and room for the
    /// format.
    fn patterns(version: usize) -> Qr {
        let size = 17 + 4 * version;
        let mut qr = Qr {
            size,
            dark: vec![false; size * size],
            fixed: vec![false; size * size],
        };
        for index in 0..size {
            qr.set(6, index, index % 2 == 0);
            qr.set(index, 6, index % 2 == 0);
        }
        for (row, col) in [(3, 3), (3, size - 4), (size - 4, 3)] {
            qr.rings(row, col, 4, &[2, 4]);
        }
        if version > 1 {
            qr.rings(size - 7, size - 7, 2, &[1]);
        }
        qr.write_format(0);
        qr
    }

    fn write_format(&mut self, mask: u32) {
        let bits = format_bits(mask);
        let size = self.size;
        for index in 0..15 {
            let dark = bits >> index & 1 == 1;
            let (row, col) = match index {
                0..=5 => (index, 8),
                6 => (7, 8),
                7 => (8, 8),
                8 => (8, 7),
                _ => (8, 14 - index),
            };
            self.set(row, col, dark);
            if index < 8 {
                self.set(8, size - 1 - index, dark);
            } else {
                self.set(size - 15 + index, 8, dark);
            }
        }
        self.set(size - 8, 8, true);
    }

    /// The modules that hold data, in order: two columns at a time from the
    /// right, upward then downward, around the timing column.
    fn data_path(&self) -> Vec<(usize, usize)> {
        let mut path = Vec::new();
        let mut upward = true;
        for right in (1..self.size).rev().step_by(2) {
            let right = if right <= 6 { right - 1 } else { right };
            for step in 0..self.size {
                let row = if upward { self.size - 1 - step } else { step };
                for col in [right, right - 1] {
                    if !self.fixed[row * self.size + col] {
                        path.push((row, col));
                    }
                }
            }
            upward = !upward;
        }
        path
    }

    /// How hard the code is to read, as the standard scores it: long runs,
    /// blocks, finder look-alikes, and an uneven share of dark modules.
    fn penalty(&self) -> usize {
        const LOOKALIKE: [bool; 11] = [
            true, false, true, true, true, false, true, false, false, false, false,
        ];
        let mut score = 0;
        for line in 0..self.size {
            for by_row in [true, false] {
                let modules: Vec<bool> = (0..self.size)
                    .map(|index| {
                        if by_row {
                            self.at(line, index)
                        } else {
                            self.at(index, line)
                        }
                    })
                    .collect();
                for run in modules.chunk_by(|a, b| a == b) {
                    if run.len() >= 5 {
                        score += run.len() - 2;
                    }
                }
                for window in modules.windows(LOOKALIKE.len()) {
                    if window == LOOKALIKE || window.iter().rev().eq(LOOKALIKE.iter()) {
                        score += 40;
                    }
                }
            }
        }
        for row in 1..self.size {
            for col in 1..self.size {
                let dark = self.at(row, col);
                if self.at(row - 1, col) == dark
                    && self.at(row, col - 1) == dark
                    && self.at(row - 1, col - 1) == dark
                {
                    score += 3;
                }
            }
        }
        let dark = self.dark.iter().filter(|dark| **dark).count();
        score + (dark * 20).abs_diff(self.dark.len() * 10) / self.dark.len() * 10
    }

    fn encode(text: &str) -> Option<Qr> {
        let length = u8::try_from(text.len()).ok()?;
        // 12 bits of the first codewords are the mode and the length.
        let index = VERSIONS
            .iter()
            .position(|(data, _)| text.len() + 2 <= *data)?;
        let (data, ecc) = VERSIONS[index];
        let mut words = codewords(text.as_bytes(), length, data);
        words.extend(correction(&words, ecc));
        let blank = Qr::patterns(index + 1);
        let path = blank.data_path();
        (0_u32..8)
            .zip(MASKS)
            .map(|(number, mask)| {
                let mut qr = blank.clone();
                for (bit, (row, col)) in path.iter().enumerate() {
                    let dark = words
                        .get(bit / 8)
                        .is_some_and(|word| word >> (7 - bit % 8) & 1 == 1);
                    qr.dark[row * qr.size + col] = dark != mask(*row, *col);
                }
                qr.write_format(number);
                qr
            })
            .min_by_key(Qr::penalty)
    }

    /// The code in half blocks, two modules to a line, black on white
    /// whatever the terminal's colors are.
    fn render(&self) -> String {
        let side = self.size + 2 * QUIET;
        let dark = |row: usize, col: usize| {
            row.checked_sub(QUIET)
                .zip(col.checked_sub(QUIET))
                .is_some_and(|(row, col)| row < self.size && col < self.size && self.at(row, col))
        };
        let mut text = String::new();
        for row in (0..side).step_by(2) {
            text.push_str("\x1b[38;5;16;48;5;231m");
            for col in 0..side {
                text.push(match (dark(row, col), dark(row + 1, col)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            text.push_str("\x1b[0m\n");
        }
        text
    }
}

/// `text` as a QR code for a terminal; `None` when it is over 106 bytes.
pub(super) fn terminal(text: &str) -> Option<String> {
    Qr::encode(text).map(|qr| qr.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a code back from its modules alone: the format, the mask, the
    /// codewords, their error correction, and the text.
    fn decode(qr: &Qr) -> String {
        let size = qr.size;
        let mut bits = 0_u32;
        for col in [0, 1, 2, 3, 4, 5, 7, 8] {
            bits = bits << 1 | u32::from(qr.at(8, col));
        }
        for row in [7, 5, 4, 3, 2, 1, 0] {
            bits = bits << 1 | u32::from(qr.at(row, 8));
        }
        let mut copy = 0_u32;
        for row in (size - 7..size).rev() {
            copy = copy << 1 | u32::from(qr.at(row, 8));
        }
        for col in size - 8..size {
            copy = copy << 1 | u32::from(qr.at(8, col));
        }
        assert_eq!(bits, copy, "the two copies of the format");
        let mask = (bits ^ 0b101_0100_0001_0010) >> 10 & 0b111;
        assert_eq!(bits, format_bits(mask));
        let version = (size - 17) / 4;
        let (data, ecc) = VERSIONS[version - 1];
        let modules: Vec<bool> = Qr::patterns(version)
            .data_path()
            .into_iter()
            .map(|(row, col)| qr.at(row, col) != MASKS[mask as usize](row, col))
            .collect();
        let words: Vec<u8> = modules
            .as_chunks::<8>()
            .0
            .iter()
            .map(|byte| byte.iter().fold(0, |word, bit| word << 1 | u8::from(*bit)))
            .take(data + ecc)
            .collect();
        // Every root of the generator is a root of a valid codeword.
        let mut root = 1;
        for _ in 0..ecc {
            let value = words
                .iter()
                .fold(0, |value, word| times(value, root) ^ word);
            assert_eq!(value, 0, "a syndrome is not zero");
            root = times(root, 2);
        }
        assert_eq!(words[0] >> 4, 0b0100, "byte mode");
        let length = usize::from(words[0] << 4 | words[1] >> 4);
        let bytes: Vec<u8> = (0..length)
            .map(|index| words[index + 1] << 4 | words[index + 2] >> 4)
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    // R3.40 15.
    #[test]
    fn a_code_decodes_to_its_text() {
        for text in [
            "https://a.example/",
            "https://mac.example.ts.net:7470/",
            "https://a-machine-with-a-long-name.tail1234.ts.net:7470/",
            &format!("https://{}.example/", "a".repeat(60)),
            &"b".repeat(106),
        ] {
            let qr = Qr::encode(text).unwrap();
            assert_eq!(decode(&qr), text);
            let rendered = terminal(text).unwrap();
            assert_eq!(rendered.lines().count(), (qr.size + 2 * QUIET).div_ceil(2));
        }
        assert!(terminal(&"b".repeat(107)).is_none());
    }

    // The codewords of the standard's own example, "01234567" at 1-M, whose
    // error correction it lists.
    #[test]
    fn error_correction_as_the_standard_computes_it() {
        let data = [
            0x10, 0x20, 0x0C, 0x56, 0x61, 0x80, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11,
            0xEC, 0x11,
        ];
        assert_eq!(
            correction(&data, 10),
            [0xA5, 0x24, 0xD4, 0xC1, 0xED, 0x36, 0xC7, 0x87, 0x2C, 0x55]
        );
    }

    // Two of the format strings the standard lists for level L.
    #[test]
    fn format_bits_as_the_standard_lists_them() {
        assert_eq!(format_bits(0), 0b111_0111_1100_0100);
        assert_eq!(format_bits(4), 0b110_0110_0010_1111);
    }
}
