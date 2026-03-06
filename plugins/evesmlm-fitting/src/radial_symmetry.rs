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

    let mut a00 = 0.0;
    let mut a01 = 0.0;
    let mut a11 = 0.0;
    let mut b0 = 0.0;
    let mut b1 = 0.0;
    let mut normals = Vec::new();

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let gx = 0.5 * (image[y * width + x + 1] - image[y * width + x - 1]);
            let gy = 0.5 * (image[(y + 1) * width + x] - image[(y - 1) * width + x]);
            let weight = gx * gx + gy * gy;
            if weight <= 1e-9 {
                continue;
            }

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
