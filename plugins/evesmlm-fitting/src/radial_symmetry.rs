use augur_plugin_evesmlm_candidates::EveCluster;

use crate::FitEstimate;

pub(crate) fn fit(cluster: &EveCluster) -> Option<FitEstimate> {
    let width = usize::from(cluster.x_max - cluster.x_min) + 1;
    let height = usize::from(cluster.y_max - cluster.y_min) + 1;
    if width < 3 || height < 3 {
        return None;
    }

    let mut image = vec![0.0; width * height];
    for (x, y, positive, negative) in &cluster.pixel_histogram {
        let local_x = usize::from(*x - cluster.x_min);
        let local_y = usize::from(*y - cluster.y_min);
        image[local_y * width + local_x] = f64::from(*positive + *negative);
    }

    // Compute raw gradients via central differences.
    let grad_width = width - 2;
    let grad_height = height - 2;
    let grad_len = grad_width * grad_height;
    let mut raw_gx = vec![0.0; grad_len];
    let mut raw_gy = vec![0.0; grad_len];
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let gi = (y - 1) * grad_width + (x - 1);
            raw_gx[gi] = 0.5 * (image[y * width + x + 1] - image[y * width + x - 1]);
            raw_gy[gi] = 0.5 * (image[(y + 1) * width + x] - image[(y - 1) * width + x]);
        }
    }

    // Apply 3x3 uniform averaging filter to gradient components to suppress
    // shot-noise in low-count event histograms (matches the Parthasarathy 2012
    // reference algorithm used by the Python EVE implementation).
    let smooth_gx = smooth_3x3(&raw_gx, grad_width, grad_height);
    let smooth_gy = smooth_3x3(&raw_gy, grad_width, grad_height);

    let mut a00 = 0.0;
    let mut a01 = 0.0;
    let mut a11 = 0.0;
    let mut b0 = 0.0;
    let mut b1 = 0.0;
    let mut normals = Vec::new();

    for gy_idx in 0..grad_height {
        for gx_idx in 0..grad_width {
            let gi = gy_idx * grad_width + gx_idx;
            let gx = smooth_gx[gi];
            let gy = smooth_gy[gi];
            let weight = gx * gx + gy * gy;
            if weight <= 1e-9 {
                continue;
            }

            let x = gx_idx + 1; // back to image-local coordinates
            let y = gy_idx + 1;
            let nx = -gy;
            let ny = gx;
            let px = f64::from(cluster.x_min) + x as f64;
            let py = f64::from(cluster.y_min) + y as f64;
            let dot = nx * px + ny * py;

            a00 += weight * nx * nx;
            a01 += weight * nx * ny;
            a11 += weight * ny * ny;
            b0 += weight * nx * dot;
            b1 += weight * ny * dot;
            normals.push((nx, ny, px, py, weight));
        }
    }

    if normals.is_empty() {
        return None;
    }

    let determinant = a00 * a11 - a01 * a01;
    if determinant.abs() < 1e-9 {
        return None;
    }

    let x = (b0 * a11 - b1 * a01) / determinant;
    let y = (a00 * b1 - a01 * b0) / determinant;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    let residual = (normals
        .iter()
        .map(|(nx, ny, px, py, weight)| {
            let distance = nx * (x - px) + ny * (y - py);
            weight * distance * distance
        })
        .sum::<f64>()
        / normals
            .iter()
            .map(|(_, _, _, _, weight)| *weight)
            .sum::<f64>())
    .sqrt();

    Some(FitEstimate {
        x,
        y,
        sigma_x: 0.0,
        sigma_y: 0.0,
        residual,
    })
}

/// 3×3 uniform averaging filter with zero-padded boundary.
/// Matches `scipy.ndimage.convolve(data, np.ones((3,3))/9, mode='constant')`:
/// out-of-bounds samples are treated as 0, divisor is always 9.
fn smooth_3x3(input: &[f64], width: usize, height: usize) -> Vec<f64> {
    let mut output = vec![0.0; input.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx >= 0 && nx < width as i32 && ny >= 0 && ny < height as i32 {
                        sum += input[ny as usize * width + nx as usize];
                    }
                }
            }
            output[y * width + x] = sum / 9.0;
        }
    }
    output
}
