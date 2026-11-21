//! The application icon, drawn in code: a lemon with a leaf. The build
//! script embeds it in the executable at several sizes, and the window and
//! tray icon draw it at run time. No dependencies, so build.rs can include it.

/// The icon as `size` x `size` RGBA pixels, antialiased.
pub fn lemon_rgba(size: u32) -> Vec<u8> {
    const SUB: u32 = 4; // subsamples per axis
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;
    for y in 0..size {
        for x in 0..size {
            let mut color = [0f32; 4];
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUB as f32) / s;
                    let py = (y as f32 + (sy as f32 + 0.5) / SUB as f32) / s;
                    if let Some(c) = shade(px, py) {
                        for (acc, v) in color.iter_mut().zip(c) {
                            *acc += v;
                        }
                    }
                }
            }
            let n = (SUB * SUB) as f32;
            let alpha = color[3] / n;
            let i = ((y * size + x) * 4) as usize;
            if alpha > 0.0 {
                for channel in 0..3 {
                    // Colours were summed premultiplied; store them straight.
                    pixels[i + channel] = ((color[channel] / n / alpha) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
            pixels[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    pixels
}

/// The colour (premultiplied, with alpha) at a point of the unit square, or
/// nothing where the icon is transparent.
fn shade(x: f32, y: f32) -> Option<[f32; 4]> {
    // The leaf, at the top right, above the fruit.
    let (lx, ly) = rotate(x - 0.70, y - 0.20, 0.6);
    if (lx / 0.17).powi(2) + (ly / 0.075).powi(2) <= 1.0 {
        let vein = ly.abs() < 0.008 && lx.abs() < 0.15;
        return Some(if vein { [0.20, 0.45, 0.16, 1.0] } else { [0.33, 0.62, 0.22, 1.0] });
    }
    // The lemon: an ellipse tilted by 30 degrees, with pointed ends.
    let (ex, ey) = rotate(x - 0.48, y - 0.55, -0.52);
    let tip = |t: f32| (t.abs() - 0.36).max(0.0) * 1.6;
    let half = (0.30 - tip(ex)).max(1e-6); // half the height, narrowing to the tips
    let radius = (ex / 0.40).powi(2) + (ey / half).powi(2).min(1e9);
    if radius > 1.0 && !(ex.abs() <= 0.46 && ey.abs() <= 0.035 && ex.abs() > 0.38) {
        return None;
    }
    let edge = radius > 0.86;
    // Light from the top left.
    let light = (1.0 - ((x - 0.36).powi(2) + (y - 0.40).powi(2)).sqrt() * 2.2).clamp(0.0, 1.0);
    let base = if edge { [0.85, 0.62, 0.05] } else { [0.97, 0.80, 0.18] };
    let highlight = [1.0, 0.95, 0.60];
    let mix = |a: f32, b: f32| a + (b - a) * light * 0.55;
    Some([mix(base[0], highlight[0]), mix(base[1], highlight[1]), mix(base[2], highlight[2]), 1.0])
}

fn rotate(x: f32, y: f32, angle: f32) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    (x * cos - y * sin, x * sin + y * cos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_has_a_transparent_corner_and_an_opaque_middle() {
        let size = 32;
        let pixels = lemon_rgba(size);
        assert_eq!(pixels.len(), (size * size * 4) as usize);
        assert_eq!(pixels[3], 0, "corner");
        let middle = ((size / 2 * size + size / 2) * 4) as usize;
        assert_eq!(pixels[middle + 3], 255, "middle");
        assert!(pixels[middle] > 200 && pixels[middle + 2] < 150, "yellow");
    }
}
