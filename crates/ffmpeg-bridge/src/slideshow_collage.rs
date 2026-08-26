use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImageSize {
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cell {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

struct Layout {
    cells: Vec<Cell>,
    visible_area: f64,
}

pub(super) fn groups(sizes: &[ImageSize]) -> Vec<Vec<usize>> {
    let mut groups = Vec::new();
    let mut index = 0_usize;
    while index < sizes.len() {
        if !is_vertical(sizes[index]) {
            groups.push(vec![index]);
            index += 1;
            continue;
        }
        let run_start = index;
        while index < sizes.len() && is_vertical(sizes[index]) {
            index += 1;
        }
        let vertical_run = (run_start..index).collect::<Vec<_>>();
        if vertical_run.len() == 1 {
            let mut group = Vec::new();
            if index >= sizes.len()
                && groups.len() >= 2
                && groups[groups.len() - 1].len() == 1
                && groups[groups.len() - 2].len() == 1
                && !is_vertical(sizes[groups[groups.len() - 1][0]])
                && !is_vertical(sizes[groups[groups.len() - 2][0]])
            {
                group.extend(groups.remove(groups.len() - 2));
                group.extend(groups.pop().expect("checked final group"));
                group.push(vertical_run[0]);
                groups.push(group);
                continue;
            }
            if let Some(previous) =
                groups.pop_if(|last| last.len() == 1 && !is_vertical(sizes[last[0]]))
            {
                group.extend(previous);
            }
            group.push(vertical_run[0]);
            if index < sizes.len() && !is_vertical(sizes[index]) {
                group.push(index);
                index += 1;
            }
            groups.push(group);
            continue;
        }
        groups.extend(vertical_groups(&vertical_run));
    }
    groups
}

fn vertical_groups(indexes: &[usize]) -> Vec<Vec<usize>> {
    let mut groups = Vec::new();
    let mut index = 0_usize;
    let mut remaining = indexes.len();
    while remaining > 0 {
        let group_size = if remaining == 4 || remaining % 3 == 1 {
            2
        } else if remaining >= 3 {
            3
        } else {
            remaining
        };
        groups.push(indexes[index..index + group_size].to_vec());
        index += group_size;
        remaining -= group_size;
    }
    groups
}

pub(super) fn cells(sizes: &[ImageSize], output_width: u32, output_height: u32) -> Vec<Cell> {
    if sizes.len() == 3 {
        let vertical = sizes
            .iter()
            .enumerate()
            .filter_map(|(index, size)| is_vertical(*size).then_some(index))
            .collect::<Vec<_>>();
        if vertical.len() == 1 {
            return safe_cells(
                one_vertical_two_horizontal(sizes, vertical[0], output_width, output_height),
                output_width,
                output_height,
            );
        }
        if vertical.len() == 2 {
            return safe_cells(
                two_vertical_one_horizontal(
                    sizes,
                    (vertical[0], vertical[1]),
                    output_width,
                    output_height,
                ),
                output_width,
                output_height,
            );
        }
    }
    let mut candidates = vec![
        row_layout(sizes, output_width, output_height),
        column_layout(sizes, output_width, output_height),
    ];
    if sizes.len() == 3 {
        candidates.extend(three_photo_splits(sizes, output_width, output_height));
    }
    if sizes.len() == 4 {
        let cells = two_row_cells(sizes, output_width, output_height);
        candidates.push(layout(sizes, cells));
    }
    let best = candidates
        .into_iter()
        .max_by(|left, right| left.visible_area.total_cmp(&right.visible_area))
        .expect("row and column candidates always exist");
    safe_cells(best.cells, output_width, output_height)
}

pub(super) fn row_paste_cells(
    sizes: &[ImageSize],
    layout_cells: &[Cell],
    output_width: u32,
    output_height: u32,
) -> Option<Vec<Cell>> {
    if sizes.len() < 2 || sizes.len() != layout_cells.len() {
        return None;
    }
    let row_y = layout_cells[0].y;
    let row_height = layout_cells[0].height;
    if row_height == 0
        || row_y.saturating_add(row_height) > output_height
        || layout_cells
            .iter()
            .any(|cell| cell.y != row_y || cell.height != row_height)
    {
        return None;
    }

    let aspect_sum = sizes
        .iter()
        .map(|size| f64::from(size.width) / f64::from(size.height))
        .sum::<f64>();
    if !aspect_sum.is_finite() || aspect_sum <= 0.0 {
        return None;
    }
    let minimum_gap = if sizes.iter().all(|size| is_vertical(*size)) {
        0
    } else {
        24.max(output_width / 80)
    };
    let gap_count = u32::try_from(sizes.len() + 1).ok()?;
    let available_width = output_width
        .saturating_sub(minimum_gap.saturating_mul(gap_count))
        .max(1);
    let rounded_height = (f64::from(available_width) / aspect_sum)
        .round()
        .clamp(1.0, f64::from(row_height));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite, positive, rounded, and clamped to a u32 row height"
    )]
    let target_height = rounded_height as u32;
    let target_height = (target_height - target_height % 2).max(2);
    let widths = sizes
        .iter()
        .map(|size| {
            let width = scaled_width(*size, target_height);
            (width - width % 2).max(2)
        })
        .collect::<Vec<_>>();
    let total_width = widths.iter().copied().sum::<u32>();
    if total_width > output_width {
        return None;
    }
    let mut gap = (output_width - total_width) / gap_count;
    gap -= gap % 2;
    let mut x = gap;
    let mut result = Vec::with_capacity(widths.len());
    for width in widths {
        let mut y = row_y + (row_height - target_height) / 2;
        y -= y % 2;
        result.push(Cell {
            x,
            y,
            width,
            height: target_height,
        });
        x = x.saturating_add(width).saturating_add(gap);
    }
    Some(result)
}

fn one_vertical_two_horizontal(
    sizes: &[ImageSize],
    vertical_index: usize,
    output_width: u32,
    output_height: u32,
) -> Vec<Cell> {
    let gap = 24.max(output_width / 80);
    let horizontal = (0..3)
        .filter(|index| *index != vertical_index)
        .collect::<Vec<_>>();
    let vertical_width =
        scaled_width(sizes[vertical_index], output_height - 2 * gap).min((output_width / 2).max(1));
    let horizontal_width = (output_width - vertical_width - 3 * gap).max(1);
    let horizontal_height = ((output_height - 3 * gap) / 2).max(1);
    let vertical_height = output_height - 2 * gap;
    let vertical_x = if vertical_index == 2 {
        output_width - gap - vertical_width
    } else {
        gap
    };
    let horizontal_x = if vertical_index == 2 {
        gap
    } else {
        vertical_x + vertical_width + gap
    };
    let mut by_index = BTreeMap::new();
    by_index.insert(
        vertical_index,
        Cell {
            x: vertical_x,
            y: gap,
            width: vertical_width,
            height: vertical_height,
        },
    );
    for (offset, horizontal_index) in horizontal.into_iter().enumerate() {
        by_index.insert(
            horizontal_index,
            Cell {
                x: horizontal_x,
                y: gap + u32::try_from(offset).expect("two rows") * (horizontal_height + gap),
                width: horizontal_width,
                height: horizontal_height,
            },
        );
    }
    (0..3).map(|index| by_index[&index]).collect()
}

fn two_vertical_one_horizontal(
    sizes: &[ImageSize],
    vertical_indexes: (usize, usize),
    output_width: u32,
    output_height: u32,
) -> Vec<Cell> {
    let horizontal_index = (0..3)
        .find(|index| *index != vertical_indexes.0 && *index != vertical_indexes.1)
        .expect("one horizontal index");
    let vertical_height = output_height / 2;
    let horizontal_height = output_height - vertical_height;
    let vertical_sizes = [sizes[vertical_indexes.0], sizes[vertical_indexes.1]];
    let vertical_cells = row_cells(&vertical_sizes, output_width, vertical_height);
    let vertical_on_top = stable_orientation(sizes);
    let mut by_index = BTreeMap::new();
    if vertical_on_top {
        by_index.insert(vertical_indexes.0, vertical_cells[0]);
        by_index.insert(vertical_indexes.1, vertical_cells[1]);
        by_index.insert(
            horizontal_index,
            Cell {
                x: 0,
                y: vertical_height,
                width: output_width,
                height: horizontal_height,
            },
        );
    } else {
        by_index.insert(
            horizontal_index,
            Cell {
                x: 0,
                y: 0,
                width: output_width,
                height: horizontal_height,
            },
        );
        for (index, cell) in [vertical_indexes.0, vertical_indexes.1]
            .into_iter()
            .zip(vertical_cells)
        {
            by_index.insert(
                index,
                Cell {
                    x: cell.x,
                    y: horizontal_height,
                    width: cell.width,
                    height: vertical_height,
                },
            );
        }
    }
    (0..3).map(|index| by_index[&index]).collect()
}

fn stable_orientation(sizes: &[ImageSize]) -> bool {
    sizes.iter().fold(0_u64, |hash, size| {
        hash.wrapping_mul(1_099_511_628_211)
            .wrapping_add(u64::from(size.width) << 32 | u64::from(size.height))
    }) & 1
        == 0
}

fn row_cells(sizes: &[ImageSize], output_width: u32, output_height: u32) -> Vec<Cell> {
    let mut widths = sizes
        .iter()
        .map(|size| scaled_width(*size, output_height))
        .collect::<Vec<_>>();
    let total = widths.iter().copied().sum::<u32>();
    if total > output_width {
        for width in &mut widths {
            *width = ((u64::from(*width) * u64::from(output_width)) / u64::from(total))
                .max(1)
                .try_into()
                .expect("bounded by output width");
        }
    }
    for width in &mut widths {
        *width -= *width % 2;
    }
    let total = widths.iter().copied().sum::<u32>();
    let mut gap = if !widths.is_empty() && sizes.iter().all(|size| is_vertical(*size)) {
        (output_width - total) / (u32::try_from(widths.len()).expect("image count") + 1)
    } else {
        0
    };
    gap -= gap % 2;
    let mut x = if gap == 0 {
        (output_width - total) / 2
    } else {
        gap
    };
    x -= x % 2;
    let last = widths.len().saturating_sub(1);
    widths
        .into_iter()
        .enumerate()
        .map(|(index, mut width)| {
            if index == last {
                width = width.min(output_width - x);
            }
            let cell = Cell {
                x,
                y: 0,
                width,
                height: output_height,
            };
            x += width + gap;
            cell
        })
        .collect()
}

fn row_layout(sizes: &[ImageSize], width: u32, height: u32) -> Layout {
    layout(sizes, row_cells(sizes, width, height))
}

fn column_layout(sizes: &[ImageSize], width: u32, height: u32) -> Layout {
    let mut heights = sizes
        .iter()
        .map(|size| scaled_height(*size, width))
        .collect::<Vec<_>>();
    let total = heights.iter().copied().sum::<u32>();
    if total > height {
        for item in &mut heights {
            *item = ((u64::from(*item) * u64::from(height)) / u64::from(total))
                .max(1)
                .try_into()
                .expect("bounded by output height");
        }
    }
    let total = heights.iter().copied().sum::<u32>();
    let mut y = (height - total) / 2;
    let last = heights.len().saturating_sub(1);
    let cells = heights
        .into_iter()
        .enumerate()
        .map(|(index, mut cell_height)| {
            if index == last {
                cell_height = cell_height.min(height - y);
            }
            let cell = Cell {
                x: 0,
                y,
                width,
                height: cell_height,
            };
            y += cell_height;
            cell
        })
        .collect();
    layout(sizes, cells)
}

fn three_photo_splits(sizes: &[ImageSize], width: u32, height: u32) -> Vec<Layout> {
    let mut layouts = Vec::new();
    for single in 0..3 {
        let pair = (0..3).filter(|index| *index != single).collect::<Vec<_>>();
        let single_height = scaled_height(sizes[single], width).min((height * 2 / 3).max(1));
        let pair_height = height - single_height;
        if pair_height > 0 {
            for single_on_top in [true, false] {
                let mut cells = vec![
                    Cell {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1
                    };
                    3
                ];
                cells[single] = Cell {
                    x: 0,
                    y: if single_on_top { 0 } else { pair_height },
                    width,
                    height: single_height,
                };
                let pair_y = if single_on_top { single_height } else { 0 };
                for (source, cell) in pair.iter().copied().zip(row_cells(
                    &[sizes[pair[0]], sizes[pair[1]]],
                    width,
                    pair_height,
                )) {
                    cells[source] = Cell { y: pair_y, ..cell };
                }
                layouts.push(layout(sizes, cells));
            }
        }
        let single_width = scaled_width(sizes[single], height).min((width * 2 / 3).max(1));
        let pair_width = width - single_width;
        if pair_width > 0 {
            for single_on_left in [true, false] {
                let mut cells = vec![
                    Cell {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1
                    };
                    3
                ];
                cells[single] = Cell {
                    x: if single_on_left { 0 } else { pair_width },
                    y: 0,
                    width: single_width,
                    height,
                };
                let pair_x = if single_on_left { single_width } else { 0 };
                let pair_layout =
                    column_layout(&[sizes[pair[0]], sizes[pair[1]]], pair_width, height);
                for (source, cell) in pair.iter().copied().zip(pair_layout.cells) {
                    cells[source] = Cell { x: pair_x, ..cell };
                }
                layouts.push(layout(sizes, cells));
            }
        }
    }
    layouts
}

fn two_row_cells(sizes: &[ImageSize], width: u32, height: u32) -> Vec<Cell> {
    let top_height = height / 2;
    let bottom_height = height - top_height;
    let mut cells = row_cells(&sizes[..2], width, top_height);
    cells.extend(
        row_cells(&sizes[2..], width, bottom_height)
            .into_iter()
            .map(|cell| Cell {
                y: top_height,
                ..cell
            }),
    );
    cells
}

fn layout(sizes: &[ImageSize], cells: Vec<Cell>) -> Layout {
    let visible_area = sizes
        .iter()
        .zip(&cells)
        .map(|(size, cell)| {
            let scale = (f64::from(cell.width) / f64::from(size.width))
                .min(f64::from(cell.height) / f64::from(size.height));
            f64::from(size.width) * scale * f64::from(size.height) * scale
        })
        .sum();
    Layout {
        cells,
        visible_area,
    }
}

fn safe_cells(cells: Vec<Cell>, output_width: u32, output_height: u32) -> Vec<Cell> {
    cells
        .into_iter()
        .map(|cell| {
            let x = (cell.x - cell.x % 2).min(output_width - 2);
            let y = (cell.y - cell.y % 2).min(output_height - 2);
            let mut width = (cell.width - cell.width % 2).max(2).min(output_width - x);
            let mut height = (cell.height - cell.height % 2)
                .max(2)
                .min(output_height - y);
            width -= width % 2;
            height -= height % 2;
            Cell {
                x,
                y,
                width: width.max(2),
                height: height.max(2),
            }
        })
        .collect()
}

fn scaled_width(size: ImageSize, height: u32) -> u32 {
    rounded_ratio(height, size.width, size.height).max(1)
}

fn scaled_height(size: ImageSize, width: u32) -> u32 {
    rounded_ratio(width, size.height, size.width).max(1)
}

fn rounded_ratio(base: u32, numerator: u32, denominator: u32) -> u32 {
    ((u64::from(base) * u64::from(numerator) + u64::from(denominator) / 2)
        / u64::from(denominator.max(1)))
    .try_into()
    .unwrap_or(u32::MAX)
}

const fn is_vertical(size: ImageSize) -> bool {
    size.height > size.width
}

#[cfg(test)]
mod tests {
    use super::{ImageSize, cells, groups, row_paste_cells};

    const H: ImageSize = ImageSize {
        width: 16,
        height: 9,
    };
    const V: ImageSize = ImageSize {
        width: 9,
        height: 16,
    };

    #[test]
    fn matches_python_vertical_run_grouping() {
        assert_eq!(groups(&[V, V, V, V]), vec![vec![0, 1], vec![2, 3]]);
        assert_eq!(
            groups(&[V, V, V, V, V, V, V]),
            vec![vec![0, 1], vec![2, 3, 4], vec![5, 6]]
        );
    }

    #[test]
    fn folds_a_single_vertical_between_horizontal_photos() {
        assert_eq!(groups(&[H, V, H]), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn produces_even_bounded_cells() {
        let cells = cells(&[V, V, V], 1920, 1080);
        assert_eq!(cells.len(), 3);
        assert!(cells.iter().all(|cell| {
            cell.x % 2 == 0
                && cell.y % 2 == 0
                && cell.width % 2 == 0
                && cell.height % 2 == 0
                && cell.x + cell.width <= 1920
                && cell.y + cell.height <= 1080
        }));
    }

    #[test]
    fn row_paste_preserves_aspect_and_balances_gaps() {
        let sizes = [V, V];
        let layout = cells(&sizes, 1920, 1080);
        let pasted = row_paste_cells(&sizes, &layout, 1920, 1080).expect("one row");
        assert_eq!(pasted[0].width, 608);
        assert_eq!(pasted[0].height, 1080);
        assert_eq!(pasted[0].x, 234);
        assert_eq!(pasted[1].x, 1076);
    }

    #[test]
    fn row_paste_reserves_gaps_for_mixed_orientations() {
        let sizes = [H, V];
        let layout = vec![
            super::Cell {
                x: 0,
                y: 0,
                width: 960,
                height: 1080,
            },
            super::Cell {
                x: 960,
                y: 0,
                width: 960,
                height: 1080,
            },
        ];
        let pasted = row_paste_cells(&sizes, &layout, 1920, 1080).expect("one row");
        assert!(pasted[0].x >= 24);
        assert!(pasted[1].x > pasted[0].x + pasted[0].width);
        assert!(pasted[1].x + pasted[1].width < 1920);
    }
}
