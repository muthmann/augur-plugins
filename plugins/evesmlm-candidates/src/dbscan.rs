use std::collections::{HashMap, VecDeque};

use crate::EveEvent;

pub fn cluster_event_indices(
    events: &[EveEvent],
    epsilon_px: f64,
    min_events: usize,
) -> Vec<Vec<usize>> {
    if events.is_empty() {
        return Vec::new();
    }

    let epsilon_px = epsilon_px.max(1.0);
    let min_events = min_events.max(1);
    let cell_size = epsilon_px.ceil() as i32;

    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        grid.entry(cell_key(event, cell_size))
            .or_default()
            .push(index);
    }

    let mut visited = vec![false; events.len()];
    let mut assigned = vec![false; events.len()];
    let mut clusters = Vec::new();

    for seed_index in 0..events.len() {
        if visited[seed_index] {
            continue;
        }

        visited[seed_index] = true;
        let seed_neighbors = region_query(seed_index, events, &grid, cell_size, epsilon_px);
        if seed_neighbors.len() < min_events {
            continue;
        }

        let mut queue: VecDeque<usize> = seed_neighbors.into();
        let mut cluster = Vec::new();

        while let Some(index) = queue.pop_front() {
            if !visited[index] {
                visited[index] = true;
                let neighbors = region_query(index, events, &grid, cell_size, epsilon_px);
                if neighbors.len() >= min_events {
                    for neighbor in neighbors {
                        queue.push_back(neighbor);
                    }
                }
            }

            if !assigned[index] {
                assigned[index] = true;
                cluster.push(index);
            }
        }

        cluster.sort_unstable();
        clusters.push(cluster);
    }

    clusters
}

fn region_query(
    event_index: usize,
    events: &[EveEvent],
    grid: &HashMap<(i32, i32), Vec<usize>>,
    cell_size: i32,
    epsilon_px: f64,
) -> Vec<usize> {
    let epsilon2 = epsilon_px * epsilon_px;
    let event = events[event_index];
    let (cell_x, cell_y) = cell_key(&event, cell_size);
    let mut neighbors = Vec::new();

    for dy in -1..=1 {
        for dx in -1..=1 {
            let key = (cell_x + dx, cell_y + dy);
            let Some(indices) = grid.get(&key) else {
                continue;
            };
            for &candidate_index in indices {
                let candidate = events[candidate_index];
                let ddx = f64::from(candidate.x) - f64::from(event.x);
                let ddy = f64::from(candidate.y) - f64::from(event.y);
                if ddx * ddx + ddy * ddy <= epsilon2 {
                    neighbors.push(candidate_index);
                }
            }
        }
    }

    neighbors
}

fn cell_key(event: &EveEvent, cell_size: i32) -> (i32, i32) {
    (
        i32::from(event.x) / cell_size.max(1),
        i32::from(event.y) / cell_size.max(1),
    )
}
