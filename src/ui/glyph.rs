//! The kyouko glyph: a mountain and its echo — computed, never shipped.
//! Geometry is defined in a 16×16 logical space but can be rendered at any
//! resolution; the tray icon supersamples at 4× (64×64) for smooth edges,
//! and the terminal prints the 16×16 view (`icon` command).
//! Pure math, no platform types.

const APEX_X: f32 = 5.0;
const APEX_Y: f32 = 2.0;
const BASE_Y: f32 = 13.0;
const BASE_HALF_W: f32 = 4.0;
/// Echo arc radii and thickness, in logical units.
const ARC_RADII: [f32; 2] = [6.5, 9.5];
const ARC_THICKNESS: f32 = 0.8;

/// `size`×`size` coverage mask. Pixel centers sample the shape, so higher
/// resolutions are true supersamples of the same geometry.
pub fn glyph_mask_at(size: usize) -> Vec<Vec<bool>> {
    let s = size as f32 / 16.0;
    let (ax, ay) = (APEX_X * s, APEX_Y * s);
    let (base_y, half_w) = (BASE_Y * s, BASE_HALF_W * s);

    let mut m = vec![vec![false; size]; size];
    for (py, row) in m.iter_mut().enumerate() {
        let y = py as f32 + 0.5;
        // mountain: filled triangle, apex (5,2), base y=13
        if y >= ay && y <= base_y {
            let half = (y - ay) / (base_y - ay) * half_w;
            for (px, lit) in row.iter_mut().enumerate() {
                let x = px as f32 + 0.5;
                if (x - ax).abs() <= half {
                    *lit = true;
                }
            }
        }
    }
    // echo: arcs centered on the apex, sweeping rightward in a ±70° cone,
    // clipped out of the mountain silhouette.
    for r_logical in ARC_RADII {
        let r = r_logical * s;
        let th = ARC_THICKNESS * s;
        for (py, row) in m.iter_mut().enumerate() {
            let y = py as f32 + 0.5;
            for (px, lit) in row.iter_mut().enumerate() {
                if *lit {
                    continue;
                }
                let x = px as f32 + 0.5;
                let dx = x - ax;
                let dy = y - ay;
                if dx <= 0.5 * s || dy.abs() > 2.75 * dx {
                    continue;
                }
                let d = (dx * dx + dy * dy).sqrt();
                if (d - r).abs() >= th {
                    continue;
                }
                if y >= ay && y <= base_y {
                    let tri_right = ax + (y - ay) / (base_y - ay) * half_w;
                    if x <= tri_right + s {
                        continue; // don't draw the echo over the mountain
                    }
                }
                *lit = true;
            }
        }
    }
    m
}

/// The 16×16 logical view.
pub fn glyph_mask() -> [[bool; 16]; 16] {
    let m = glyph_mask_at(16);
    let mut out = [[false; 16]; 16];
    for (y, row) in m.iter().enumerate() {
        for (x, lit) in row.iter().enumerate() {
            out[y][x] = *lit;
        }
    }
    out
}

/// ASCII view of the glyph, row 0 = top (debug: `icon` command).
pub fn glyph_ascii() -> String {
    let m = glyph_mask();
    let mut out = String::with_capacity(16 * 17 + 4);
    out.push_str("+  kyouko tray glyph  +\n");
    for row in m {
        for lit in row {
            out.push(if lit { '#' } else { '.' });
        }
        out.push('\n');
    }
    out
}
