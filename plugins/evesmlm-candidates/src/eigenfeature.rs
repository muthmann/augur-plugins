use nalgebra::Matrix2;

use crate::EveEvent;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterEigenInfo {
    pub lambda_1: f64,
    pub lambda_2: f64,
    pub angle_rad: f64,
}

pub fn filter_clusters(
    events: &[EveEvent],
    clusters: Vec<Vec<usize>>,
    max_spatial_extent_px: f64,
    min_isotropy: f64,
) -> Vec<Vec<usize>> {
    let max_variance = max_spatial_extent_px.max(0.0).powi(2);

    clusters
        .into_iter()
        .filter(|indices| {
            let Some(info) = cluster_eigen_info(events, indices) else {
                return false;
            };
            let isotropy = if info.lambda_1 <= 1e-9 {
                1.0
            } else {
                info.lambda_2 / info.lambda_1
            };
            info.lambda_1 <= max_variance && isotropy >= min_isotropy
        })
        .collect()
}

pub fn cluster_eigen_info(events: &[EveEvent], indices: &[usize]) -> Option<ClusterEigenInfo> {
    if indices.len() < 2 {
        return None;
    }

    let n = indices.len() as f64;
    let mean_x = indices
        .iter()
        .map(|&index| f64::from(events[index].x))
        .sum::<f64>()
        / n;
    let mean_y = indices
        .iter()
        .map(|&index| f64::from(events[index].y))
        .sum::<f64>()
        / n;

    let mut covariance = Matrix2::zeros();
    for &index in indices {
        let dx = f64::from(events[index].x) - mean_x;
        let dy = f64::from(events[index].y) - mean_y;
        covariance[(0, 0)] += dx * dx;
        covariance[(0, 1)] += dx * dy;
        covariance[(1, 0)] += dx * dy;
        covariance[(1, 1)] += dy * dy;
    }
    covariance /= n.max(1.0);

    let eigen = covariance.symmetric_eigen();
    let major_index = if eigen.eigenvalues[0] >= eigen.eigenvalues[1] {
        0
    } else {
        1
    };
    let minor_index = 1 - major_index;
    let major_vector = eigen.eigenvectors.column(major_index);

    Some(ClusterEigenInfo {
        lambda_1: eigen.eigenvalues[major_index],
        lambda_2: eigen.eigenvalues[minor_index],
        angle_rad: major_vector[1].atan2(major_vector[0]),
    })
}

pub fn cluster_eigenvalues(events: &[EveEvent], indices: &[usize]) -> Option<(f64, f64)> {
    cluster_eigen_info(events, indices).map(|info| (info.lambda_1, info.lambda_2))
}
