use augur_plugin_evesmlm_fitting::EveLocalizationResults;

pub fn estimate_correction_shift(
    reference_points: &[(f64, f64)],
    moving_points: &[(f64, f64)],
    max_shift_px: i32,
) -> (f64, f64) {
    if reference_points.is_empty() || moving_points.is_empty() {
        return (0.0, 0.0);
    }

    let min_x = reference_points
        .iter()
        .chain(moving_points.iter())
        .map(|(x, _)| x.floor() as isize)
        .min()
        .unwrap_or(0)
        - max_shift_px as isize
        - 2;
    let max_x = reference_points
        .iter()
        .chain(moving_points.iter())
        .map(|(x, _)| x.ceil() as isize)
        .max()
        .unwrap_or(0)
        + max_shift_px as isize
        + 2;
    let min_y = reference_points
        .iter()
        .chain(moving_points.iter())
        .map(|(_, y)| y.floor() as isize)
        .min()
        .unwrap_or(0)
        - max_shift_px as isize
        - 2;
    let max_y = reference_points
        .iter()
        .chain(moving_points.iter())
        .map(|(_, y)| y.ceil() as isize)
        .max()
        .unwrap_or(0)
        + max_shift_px as isize
        + 2;

    let width = (max_x - min_x + 1).max(1) as usize;
    let height = (max_y - min_y + 1).max(1) as usize;
    let reference_image = render_points(reference_points, min_x, min_y, width, height);
    let moving_image = render_points(moving_points, min_x, min_y, width, height);

    let mut best_shift = (0, 0);
    let mut best_score = f64::NEG_INFINITY;
    for shift_y in -max_shift_px..=max_shift_px {
        for shift_x in -max_shift_px..=max_shift_px {
            let score = correlation_score(
                &reference_image,
                &moving_image,
                width,
                height,
                shift_x,
                shift_y,
            );
            if score > best_score {
                best_score = score;
                best_shift = (shift_x, shift_y);
            }
        }
    }

    let best_x = best_shift.0;
    let best_y = best_shift.1;
    let offset_x = parabolic_offset(
        correlation_score(
            &reference_image,
            &moving_image,
            width,
            height,
            best_x - 1,
            best_y,
        ),
        best_score,
        correlation_score(
            &reference_image,
            &moving_image,
            width,
            height,
            best_x + 1,
            best_y,
        ),
    );
    let offset_y = parabolic_offset(
        correlation_score(
            &reference_image,
            &moving_image,
            width,
            height,
            best_x,
            best_y - 1,
        ),
        best_score,
        correlation_score(
            &reference_image,
            &moving_image,
            width,
            height,
            best_x,
            best_y + 1,
        ),
    );

    (best_x as f64 + offset_x, best_y as f64 + offset_y)
}

pub fn apply_correction(
    results: &EveLocalizationResults,
    correction_x: f64,
    correction_y: f64,
) -> EveLocalizationResults {
    let mut corrected = results.clone();
    for localization in &mut corrected.localizations {
        localization.x -= correction_x;
        localization.y -= correction_y;
    }
    corrected
}

fn render_points(
    points: &[(f64, f64)],
    origin_x: isize,
    origin_y: isize,
    width: usize,
    height: usize,
) -> Vec<f64> {
    let mut image = vec![0.0; width * height];
    for (x, y) in points {
        let pixel_x = x.round() as isize - origin_x;
        let pixel_y = y.round() as isize - origin_y;
        if pixel_x < 0 || pixel_x >= width as isize || pixel_y < 0 || pixel_y >= height as isize {
            continue;
        }
        image[pixel_y as usize * width + pixel_x as usize] += 1.0;
    }
    image
}

fn correlation_score(
    reference: &[f64],
    moving: &[f64],
    width: usize,
    height: usize,
    correction_x: i32,
    correction_y: i32,
) -> f64 {
    let mut score = 0.0;
    for y in 0..height {
        for x in 0..width {
            let moving_x = x as isize - correction_x as isize;
            let moving_y = y as isize - correction_y as isize;
            if moving_x < 0
                || moving_x >= width as isize
                || moving_y < 0
                || moving_y >= height as isize
            {
                continue;
            }
            score +=
                reference[y * width + x] * moving[moving_y as usize * width + moving_x as usize];
        }
    }
    score
}

fn parabolic_offset(left: f64, center: f64, right: f64) -> f64 {
    let denominator = left - 2.0 * center + right;
    if denominator.abs() < 1e-9 {
        0.0
    } else {
        (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
    }
}
