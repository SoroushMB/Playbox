// ==============================================================================
//  QR Code Generator (100% std — no external crates)
//  Supports Version 1-6, Byte mode, ECC Level L
// ==============================================================================

pub fn print_qr_code(text: &str) {
    if let Some(modules) = qr_encode(text) {
        let size = modules.len();
        // Use Unicode half-block characters: upper half = top row, lower half = bottom row
        // ██ = both black, ▀▀ = top black / bottom white, ▄▄ = top white / bottom black, "  " = both white
        // QR spec: dark module = true. Terminal: we want dark=black on white bg or inverted.
        // Use inverted: dark module = █ (dark on terminal)
        let quiet = 2; // quiet zone modules
        let total = size + quiet * 2;

        // Process two rows at a time using half-block characters
        let get = |r: usize, c: usize| -> bool {
            if r < quiet || r >= size + quiet || c < quiet || c >= size + quiet {
                false // quiet zone is white (light)
            } else {
                modules[r - quiet][c - quiet]
            }
        };

        let mut row = 0;
        while row < total {
            print!("  "); // left indent
            for col in 0..total {
                let top = get(row, col);
                let bot = if row + 1 < total {
                    get(row + 1, col)
                } else {
                    false
                };
                // We want dark modules to be dark blocks on terminal
                // Using inverted rendering (dark = █):
                match (top, bot) {
                    (true, true) => print!("█"),
                    (true, false) => print!("▀"),
                    (false, true) => print!("▄"),
                    (false, false) => print!(" "),
                }
            }
            println!();
            row += 2;
        }
    } else {
        println!("  [QR generation failed — URL may be too long]");
    }
}

// QR Version capacities for Byte mode, ECC Level L (data codewords)
const QR_DATA_CAPACITY: &[(usize, usize)] = &[
    // (version, data_codewords)
    (1, 17),
    (2, 32),
    (3, 53),
    (4, 78),
    (5, 106),
    (6, 134),
];

// Total codewords per version
const QR_TOTAL_CODEWORDS: &[usize] = &[26, 44, 70, 100, 134, 172];

// ECC codewords per block for Level L
const QR_ECC_PER_BLOCK: &[usize] = &[7, 10, 15, 20, 26, 36];

// Number of ECC blocks for Level L
const QR_NUM_BLOCKS: &[usize] = &[1, 1, 1, 1, 1, 2];

// Alignment pattern center coords per version (version 1 has none)
const QR_ALIGN_CENTERS: &[&[usize]] = &[
    &[],           // v1
    &[6, 18],      // v2
    &[6, 22],      // v3
    &[6, 26],      // v4
    &[6, 30],      // v5
    &[6, 34],      // v6
];

pub fn qr_encode(text: &str) -> Option<Vec<Vec<bool>>> {
    let data_bytes = text.as_bytes();
    let data_len = data_bytes.len();

    // Find smallest version that fits
    let mut version = 0usize;
    let mut data_cap = 0usize;
    for &(v, cap) in QR_DATA_CAPACITY {
        // In byte mode: 4 bits mode + 8/16 bits count + data*8 bits + 4 bits terminator
        // For version 1-9: count indicator is 8 bits
        let overhead = 4 + 8 + 4; // mode(4) + count(8) + terminator(4) = 16 bits = 2 bytes
        let usable = if cap * 8 >= overhead {
            (cap * 8 - overhead) / 8
        } else {
            0
        };
        if data_len <= usable {
            version = v;
            data_cap = cap;
            break;
        }
    }
    if version == 0 {
        return None; // Too long
    }

    let vi = version - 1; // version index
    let size = 17 + version * 4; // module size
    let total_codewords = QR_TOTAL_CODEWORDS[vi];
    let ecc_per_block = QR_ECC_PER_BLOCK[vi];
    let num_blocks = QR_NUM_BLOCKS[vi];

    // === Step 1: Encode data into bit stream ===
    let mut bits: Vec<bool> = Vec::new();

    // Mode indicator: Byte mode = 0100
    push_bits(&mut bits, 0b0100, 4);

    // Character count indicator (8 bits for version 1-9)
    push_bits(&mut bits, data_len as u32, 8);

    // Data
    for &b in data_bytes {
        push_bits(&mut bits, b as u32, 8);
    }

    // Terminator (up to 4 zero bits, but don't exceed capacity)
    let cap_bits = data_cap * 8;
    let term_len = 4.min(cap_bits.saturating_sub(bits.len()));
    for _ in 0..term_len {
        bits.push(false);
    }

    // Pad to byte boundary
    while bits.len() % 8 != 0 && bits.len() < cap_bits {
        bits.push(false);
    }

    // Pad with alternating 0xEC, 0x11
    let pad_bytes = [0xECu8, 0x11u8];
    let mut pi = 0;
    while bits.len() < cap_bits {
        push_bits(&mut bits, pad_bytes[pi % 2] as u32, 8);
        pi += 1;
    }

    // Convert to bytes
    let mut data_codewords: Vec<u8> = Vec::new();
    for chunk in bits.chunks(8) {
        let mut byte = 0u8;
        for (i, &b) in chunk.iter().enumerate() {
            if b {
                byte |= 1 << (7 - i);
            }
        }
        data_codewords.push(byte);
    }
    data_codewords.truncate(data_cap);

    // === Step 2: Error correction ===
    let _ecc_total = ecc_per_block * num_blocks;
    let data_per_block_base = data_cap / num_blocks;
    let blocks_with_extra = data_cap % num_blocks;

    let mut data_blocks: Vec<Vec<u8>> = Vec::new();
    let mut ecc_blocks: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0;

    for i in 0..num_blocks {
        let block_data_len = data_per_block_base + if i >= num_blocks - blocks_with_extra { 1 } else { 0 };
        let block_data = data_codewords[offset..offset + block_data_len.min(data_codewords.len() - offset)].to_vec();
        offset += block_data.len();
        let ecc = rs_encode(&block_data, ecc_per_block);
        data_blocks.push(block_data);
        ecc_blocks.push(ecc);
    }

    // Interleave data blocks
    let max_data_len = data_blocks.iter().map(|b| b.len()).max().unwrap_or(0);
    let mut interleaved: Vec<u8> = Vec::with_capacity(total_codewords);
    for i in 0..max_data_len {
        for block in &data_blocks {
            if i < block.len() {
                interleaved.push(block[i]);
            }
        }
    }
    // Interleave ECC blocks
    for i in 0..ecc_per_block {
        for block in &ecc_blocks {
            if i < block.len() {
                interleaved.push(block[i]);
            }
        }
    }

    // === Step 3: Build matrix ===
    let mut modules = vec![vec![false; size]; size];
    let mut is_function = vec![vec![false; size]; size]; // marks function pattern areas

    // Place finder patterns
    place_finder_pattern(&mut modules, &mut is_function, 0, 0, size);
    place_finder_pattern(&mut modules, &mut is_function, 0, size - 7, size);
    place_finder_pattern(&mut modules, &mut is_function, size - 7, 0, size);

    // Place alignment patterns (version >= 2)
    let centers = QR_ALIGN_CENTERS[vi];
    if centers.len() >= 2 {
        for &r in centers {
            for &c in centers {
                // Skip if it overlaps with finder patterns
                if is_function[r][c] {
                    continue;
                }
                place_alignment_pattern(&mut modules, &mut is_function, r, c, size);
            }
        }
    }

    // Timing patterns
    for i in 8..size - 8 {
        let on = i % 2 == 0;
        if !is_function[6][i] {
            modules[6][i] = on;
            is_function[6][i] = true;
        }
        if !is_function[i][6] {
            modules[i][6] = on;
            is_function[i][6] = true;
        }
    }

    // Dark module
    modules[size - 8][8] = true;
    is_function[size - 8][8] = true;

    // Reserve format info areas
    for i in 0..9 {
        if i < size {
            is_function[8][i] = true;
            is_function[i][8] = true;
        }
    }
    for i in 0..8 {
        is_function[8][size - 1 - i] = true;
        is_function[size - 1 - i][8] = true;
    }

    // === Step 4: Place data bits ===
    let mut data_bits: Vec<bool> = Vec::new();
    for &byte in &interleaved {
        for j in (0..8).rev() {
            data_bits.push((byte >> j) & 1 == 1);
        }
    }
    // Pad remainder bits
    let remainder_bits = match version {
        2 | 3 | 4 | 5 | 6 => 7,
        _ => 0,
    };
    for _ in 0..remainder_bits {
        data_bits.push(false);
    }

    let mut bit_idx = 0;
    let mut right = size as isize - 1;
    while right >= 1 {
        if right == 6 {
            right -= 1; // skip timing column
        }
        // Traverse upward then downward alternately
        let going_up = ((size as isize - 1 - right) / 2) % 2 == 0;
        let rows: Vec<usize> = if going_up {
            (0..size).rev().collect()
        } else {
            (0..size).collect()
        };
        for row in rows {
            for &col_offset in &[0isize, -1isize] {
                let col = (right + col_offset) as usize;
                if col >= size {
                    continue;
                }
                if is_function[row][col] {
                    continue;
                }
                if bit_idx < data_bits.len() {
                    modules[row][col] = data_bits[bit_idx];
                    bit_idx += 1;
                }
            }
        }
        right -= 2;
    }

    // === Step 5: Apply masking (try all 8 masks, pick best) ===
    let mut _best_mask = 0u8;
    let mut best_penalty = u32::MAX;
    let mut best_matrix = modules.clone();

    for mask in 0..8u8 {
        let mut candidate = modules.clone();
        apply_mask(&mut candidate, &is_function, mask, size);
        write_format_info(&mut candidate, 0b01, mask, size); // ECC L = 01
        let penalty = evaluate_penalty(&candidate, size);
        if penalty < best_penalty {
            best_penalty = penalty;
            _best_mask = mask;
            best_matrix = candidate;
        }
    }

    Some(best_matrix)
}

fn push_bits(bits: &mut Vec<bool>, value: u32, count: usize) {
    for i in (0..count).rev() {
        bits.push((value >> i) & 1 == 1);
    }
}

// Reed-Solomon encoding over GF(256) with primitive polynomial 0x11D
fn rs_encode(data: &[u8], ecc_len: usize) -> Vec<u8> {
    let gen = rs_generator_poly(ecc_len);
    let mut msg = data.to_vec();
    msg.resize(data.len() + ecc_len, 0);

    for i in 0..data.len() {
        let coef = msg[i];
        if coef != 0 {
            for j in 0..gen.len() {
                msg[i + j] ^= gf_mul(gen[j], coef);
            }
        }
    }

    msg[data.len()..].to_vec()
}

fn rs_generator_poly(degree: usize) -> Vec<u8> {
    let mut gen = vec![1u8];
    for i in 0..degree {
        let mut new_gen = vec![0u8; gen.len() + 1];
        let factor = gf_exp(i as u8);
        for j in 0..gen.len() {
            new_gen[j] ^= gen[j];
            new_gen[j + 1] ^= gf_mul(gen[j], factor);
        }
        gen = new_gen;
    }
    gen
}

// GF(256) lookup tables
fn gf_exp(a: u8) -> u8 {
    GF_EXP_TABLE[a as usize]
}

#[allow(dead_code)]
fn gf_log(a: u8) -> u8 {
    GF_LOG_TABLE[a as usize]
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        let log_a = GF_LOG_TABLE[a as usize] as usize;
        let log_b = GF_LOG_TABLE[b as usize] as usize;
        GF_EXP_TABLE[(log_a + log_b) % 255]
    }
}

// Precomputed GF(256) tables (primitive poly 0x11D = x^8 + x^4 + x^3 + x^2 + 1)
const GF_EXP_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut val = 1u16;
    let mut i = 0;
    while i < 256 {
        table[i] = val as u8;
        val <<= 1;
        if val >= 256 {
            val ^= 0x11D;
        }
        i += 1;
    }
    table
};

const GF_LOG_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 255 {
        table[GF_EXP_TABLE[i] as usize] = i as u8;
        i += 1;
    }
    table
};

fn place_finder_pattern(
    modules: &mut Vec<Vec<bool>>,
    is_function: &mut Vec<Vec<bool>>,
    row: usize,
    col: usize,
    size: usize,
) {
    // 7x7 finder + 1 cell separator (white border)
    for dr in 0..9isize {
        for dc in 0..9isize {
            let r = row as isize + dr - 1;
            let c = col as isize + dc - 1;
            if r < 0 || r >= size as isize || c < 0 || c >= size as isize {
                continue;
            }
            let ru = r as usize;
            let cu = c as usize;
            // The actual 7x7 pattern
            let in_finder = dr >= 1 && dr <= 7 && dc >= 1 && dc <= 7;
            let on_border = dr == 1 || dr == 7 || dc == 1 || dc == 7;
            let in_inner = dr >= 3 && dr <= 5 && dc >= 3 && dc <= 5;

            is_function[ru][cu] = true;
            if in_finder && (on_border || in_inner) {
                modules[ru][cu] = true;
            } else {
                modules[ru][cu] = false;
            }
        }
    }
}

fn place_alignment_pattern(
    modules: &mut Vec<Vec<bool>>,
    is_function: &mut Vec<Vec<bool>>,
    center_row: usize,
    center_col: usize,
    size: usize,
) {
    for dr in -2i32..=2 {
        for dc in -2i32..=2 {
            let r = center_row as i32 + dr;
            let c = center_col as i32 + dc;
            if r < 0 || r >= size as i32 || c < 0 || c >= size as i32 {
                continue;
            }
            let ru = r as usize;
            let cu = c as usize;
            is_function[ru][cu] = true;
            modules[ru][cu] = dr.abs() == 2 || dc.abs() == 2 || (dr == 0 && dc == 0);
        }
    }
}

fn apply_mask(modules: &mut Vec<Vec<bool>>, is_function: &[Vec<bool>], mask: u8, size: usize) {
    for r in 0..size {
        for c in 0..size {
            if is_function[r][c] {
                continue;
            }
            let invert = match mask {
                0 => (r + c) % 2 == 0,
                1 => r % 2 == 0,
                2 => c % 3 == 0,
                3 => (r + c) % 3 == 0,
                4 => (r / 2 + c / 3) % 2 == 0,
                5 => (r * c) % 2 + (r * c) % 3 == 0,
                6 => ((r * c) % 2 + (r * c) % 3) % 2 == 0,
                7 => ((r + c) % 2 + (r * c) % 3) % 2 == 0,
                _ => false,
            };
            if invert {
                modules[r][c] = !modules[r][c];
            }
        }
    }
}

fn write_format_info(modules: &mut Vec<Vec<bool>>, ecc_level: u8, mask: u8, size: usize) {
    let data = ((ecc_level as u16) << 3) | (mask as u16);
    let mut rem = data << 10;
    // Generator polynomial for format info: x^10 + x^8 + x^5 + x^4 + x^2 + x + 1 = 0x537
    let gen = 0x537u16;
    for i in (0..=4).rev() {
        if rem & (1 << (i + 10)) != 0 {
            rem ^= gen << i;
        }
    }
    let format_bits = ((data << 10) | rem) ^ 0x5412; // XOR with mask pattern

    // Place format bits around finder patterns
    // Horizontal: left side of top-left finder, then right side
    let format_positions_h: [(usize, usize); 15] = [
        (8, 0), (8, 1), (8, 2), (8, 3), (8, 4), (8, 5), (8, 7), (8, 8),
        (8, size - 8), (8, size - 7), (8, size - 6), (8, size - 5),
        (8, size - 4), (8, size - 3), (8, size - 2),
    ];
    let format_positions_v: [(usize, usize); 15] = [
        (0, 8), (1, 8), (2, 8), (3, 8), (4, 8), (5, 8), (7, 8), (8, 8),
        (size - 7, 8), (size - 6, 8), (size - 5, 8), (size - 4, 8),
        (size - 3, 8), (size - 2, 8), (size - 1, 8),
    ];
    for i in 0..15 {
        let bit = (format_bits >> (14 - i)) & 1 == 1;
        let (r, c) = format_positions_h[i];
        modules[r][c] = bit;
        let (r2, c2) = format_positions_v[i];
        modules[r2][c2] = bit;
    }
}

fn evaluate_penalty(modules: &[Vec<bool>], size: usize) -> u32 {
    let mut penalty = 0u32;

    // Rule 1: runs of same color (5+ in a row/col)
    for r in 0..size {
        let mut run = 1u32;
        for c in 1..size {
            if modules[r][c] == modules[r][c - 1] {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }
    for c in 0..size {
        let mut run = 1u32;
        for r in 1..size {
            if modules[r][c] == modules[r - 1][c] {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }

    // Rule 2: 2x2 blocks of same color
    for r in 0..size - 1 {
        for c in 0..size - 1 {
            let color = modules[r][c];
            if color == modules[r][c + 1]
                && color == modules[r + 1][c]
                && color == modules[r + 1][c + 1]
            {
                penalty += 3;
            }
        }
    }

    // Rule 3: finder-like patterns (1011101 with 4 white)
    let pattern_a: [bool; 11] = [true, false, true, true, true, false, true, false, false, false, false];
    let pattern_b: [bool; 11] = [false, false, false, false, true, false, true, true, true, false, true];
    for r in 0..size {
        for c in 0..size.saturating_sub(10) {
            let mut match_a = true;
            let mut match_b = true;
            for i in 0..11 {
                if modules[r][c + i] != pattern_a[i] {
                    match_a = false;
                }
                if modules[r][c + i] != pattern_b[i] {
                    match_b = false;
                }
            }
            if match_a || match_b {
                penalty += 40;
            }
        }
    }
    for c in 0..size {
        for r in 0..size.saturating_sub(10) {
            let mut match_a = true;
            let mut match_b = true;
            for i in 0..11 {
                if modules[r + i][c] != pattern_a[i] {
                    match_a = false;
                }
                if modules[r + i][c] != pattern_b[i] {
                    match_b = false;
                }
            }
            if match_a || match_b {
                penalty += 40;
            }
        }
    }

    // Rule 4: proportion of dark modules
    let total = (size * size) as u32;
    let dark: u32 = modules.iter().flatten().filter(|&&m| m).count() as u32;
    let pct = (dark * 100) / total;
    let prev5 = (pct / 5) * 5;
    let next5 = prev5 + 5;
    let dev_prev = if prev5 >= 50 { prev5 - 50 } else { 50 - prev5 };
    let dev_next = if next5 >= 50 { next5 - 50 } else { 50 - next5 };
    penalty += (dev_prev.min(dev_next) / 5) * 10;

    penalty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_encode_basic() {
        let text = "http://192.168.1.100:8080";
        let result = qr_encode(text);
        assert!(result.is_some());
        let matrix = result.unwrap();
        assert!(!matrix.is_empty());
        assert_eq!(matrix.len(), matrix[0].len());
    }

    #[test]
    fn test_qr_encode_empty() {
        let result = qr_encode("");
        assert!(result.is_some());
    }
}
