use crate::model::Node;
const MIN_EDGE: f32 = 0.5;
pub const DIRECTORY_HEADER: f32 = 18.0;
pub const DIRECTORY_PADDING: f32 = 2.0;
const MIN_HEADER_WIDTH: f32 = 38.0;
const DISPLAY_DEPTH: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Bounds {
    pub const fn from_size(width: f32, height: f32) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    pub fn area(self) -> f32 {
        (self.width * self.height).max(0.0)
    }

    pub fn is_within(self, outer: Self) -> bool {
        self.x >= outer.x - 0.01
            && self.y >= outer.y - 0.01
            && self.x + self.width <= outer.x + outer.width + 0.01
            && self.y + self.height <= outer.y + outer.height + 0.01
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedTile {
    pub path: Vec<usize>,
    pub bounds: Bounds,
    pub content: Bounds,
    pub is_directory: bool,
    pub render_children: bool,
}

/// Normalize by sibling sums because deduped children may exceed their parent's total.
pub fn layout(root: &Node, bounds: Bounds) -> Vec<OwnedTile> {
    layout_with_depth(root, bounds, DISPLAY_DEPTH)
}

pub fn layout_with_depth(root: &Node, bounds: Bounds, max_depth: usize) -> Vec<OwnedTile> {
    let mut tiles = Vec::new();
    visit(root, bounds, &mut Vec::new(), max_depth, &mut tiles);
    tiles
}

fn visit(
    node: &Node,
    bounds: Bounds,
    path: &mut Vec<usize>,
    depth_left: usize,
    tiles: &mut Vec<OwnedTile>,
) {
    if bounds.width < MIN_EDGE || bounds.height < MIN_EDGE {
        return;
    }
    let is_directory = node.is_directory;
    let render_children = is_directory && depth_left > 0;
    let content = if render_children
        && bounds.width >= MIN_HEADER_WIDTH + 2.0 * DIRECTORY_PADDING
        && bounds.height >= DIRECTORY_HEADER + 2.0 * DIRECTORY_PADDING + MIN_EDGE
    {
        Bounds {
            x: bounds.x + DIRECTORY_PADDING,
            y: bounds.y + DIRECTORY_HEADER + DIRECTORY_PADDING,
            width: bounds.width - 2.0 * DIRECTORY_PADDING,
            height: bounds.height - DIRECTORY_HEADER - 2.0 * DIRECTORY_PADDING,
        }
    } else {
        bounds
    };

    tiles.push(OwnedTile {
        path: path.clone(),
        bounds,
        content,
        is_directory,
        render_children,
    });
    if !render_children {
        return;
    }

    let mut remaining = node
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| (index, child.size as f64))
        .filter(|(_, size)| *size > 0.0)
        .collect::<Vec<_>>();
    remaining.sort_by(|(left_index, left), (right_index, right)| {
        right
            .total_cmp(left)
            .then_with(|| left_index.cmp(right_index))
    });

    let total = remaining.iter().map(|(_, size)| *size).sum::<f64>();
    if total == 0.0 {
        return;
    }
    for (_, size) in &mut remaining {
        *size = *size / total * content.area() as f64;
    }

    for (index, child_bounds) in squarify(remaining, content) {
        path.push(index);
        visit(
            &node.children[index],
            child_bounds,
            path,
            depth_left - 1,
            tiles,
        );
        path.pop();
    }
}

fn squarify(items: Vec<(usize, f64)>, mut bounds: Bounds) -> Vec<(usize, Bounds)> {
    let mut placed = Vec::new();
    let mut offset = 0;
    while offset < items.len() && bounds.width >= MIN_EDGE && bounds.height >= MIN_EDGE {
        let row = take_row(&items[offset..], bounds);
        let row_area = items[offset..offset + row]
            .iter()
            .map(|(_, area)| area)
            .sum();
        let (strip, rest) = take_strip(bounds, row_area);
        place_row(
            &items[offset..offset + row],
            strip,
            bounds.width >= bounds.height,
            &mut placed,
        );
        bounds = rest;
        offset += row;
    }
    placed
}

fn take_row(items: &[(usize, f64)], bounds: Bounds) -> usize {
    let short_side = bounds.width.min(bounds.height) as f64;
    let mut best_worst = worst_ratio(&items[..1], short_side);
    let mut count = 1;
    for next in 2..=items.len() {
        let candidate = worst_ratio(&items[..next], short_side);
        if candidate > best_worst {
            break;
        }
        best_worst = candidate;
        count = next;
    }
    count
}

fn worst_ratio(row: &[(usize, f64)], short_side: f64) -> f64 {
    let sum = row.iter().map(|(_, area)| area).sum::<f64>();
    let max = row.iter().map(|(_, area)| *area).fold(0.0_f64, f64::max);
    let min = row
        .iter()
        .map(|(_, area)| *area)
        .fold(f64::INFINITY, f64::min);
    (short_side * short_side * max / (sum * sum)).max(sum * sum / (short_side * short_side * min))
}

fn take_strip(bounds: Bounds, area: f64) -> (Bounds, Bounds) {
    if bounds.width >= bounds.height {
        let width = (area / bounds.height as f64) as f32;
        (
            Bounds { width, ..bounds },
            Bounds {
                x: bounds.x + width,
                width: (bounds.width - width).max(0.0),
                ..bounds
            },
        )
    } else {
        let height = (area / bounds.width as f64) as f32;
        (
            Bounds { height, ..bounds },
            Bounds {
                y: bounds.y + height,
                height: (bounds.height - height).max(0.0),
                ..bounds
            },
        )
    }
}

fn place_row(
    row: &[(usize, f64)],
    strip: Bounds,
    vertical: bool,
    placed: &mut Vec<(usize, Bounds)>,
) {
    let extent = if vertical { strip.height } else { strip.width };
    let origin = if vertical { strip.y } else { strip.x };
    let thickness = if vertical { strip.width } else { strip.height };
    let mut start = 0.0_f32;
    for (position, (index, area)) in row.iter().enumerate() {
        let end = if position + 1 == row.len() {
            extent
        } else {
            start + (*area / thickness as f64) as f32
        };
        let bounds = if vertical {
            Bounds {
                x: strip.x,
                y: origin + start,
                width: strip.width,
                height: (end - start).max(0.0),
            }
        } else {
            Bounds {
                x: origin + start,
                y: strip.y,
                width: (end - start).max(0.0),
                height: strip.height,
            }
        };
        placed.push((*index, bounds));
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::{Bounds, DIRECTORY_HEADER, DIRECTORY_PADDING, layout, layout_with_depth, squarify};
    use crate::model::Node;

    #[test]
    fn leaf_areas_fill_bounds_and_tracks_sizes() {
        let tree = Node::new(
            "root",
            0,
            vec![Node::leaf("large", 800), Node::leaf("small", 200)],
        );
        let bounds = Bounds::from_size(400.0, 300.0);
        let tiles = layout(&tree, bounds);
        let leaves = tiles
            .iter()
            .filter(|tile| !tile.path.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(leaves.len(), 2);
        for tile in &tiles {
            assert!(tile.bounds.is_within(bounds));
        }
        let area = leaves.iter().map(|tile| tile.bounds.area()).sum::<f32>();
        assert!(
            (area - tiles[0].content.area()).abs() < 0.1,
            "leaf area was {area}"
        );
        assert!(leaves[0].bounds.area() > leaves[1].bounds.area() * 3.9);
        assert!(leaves[0].bounds.area() < leaves[1].bounds.area() * 4.1);
    }

    #[test]
    fn sibling_sum_not_parent_size_controls_the_partition() {
        let tree = Node {
            name: "deduplicated".into(),
            size: 100,
            children: vec![Node::leaf("a", 700), Node::leaf("b", 300)],
            is_directory: true,
            needs_sub_scan: false,
            btrfs: None,
        };
        let tiles = layout(&tree, Bounds::from_size(100.0, 100.0));
        let total = tiles[1..]
            .iter()
            .map(|tile| tile.bounds.area())
            .sum::<f32>();
        assert!((total - tiles[0].content.area()).abs() < 0.1);
    }

    #[test]
    fn empty_and_zero_sized_trees_do_not_panic() {
        let tree = Node::new("root", 0, vec![Node::leaf("empty", 0)]);
        let tiles = layout(&tree, Bounds::from_size(0.0, 0.0));
        assert!(tiles.is_empty());
        let tiles = layout(&tree, Bounds::from_size(100.0, 100.0));
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].path, Vec::<usize>::new());
    }

    #[test]
    fn nested_nodes_stay_inside_their_parent_tile() {
        let tree = Node::new(
            "root",
            0,
            vec![
                Node::new("left", 0, vec![Node::leaf("a", 60), Node::leaf("b", 40)]),
                Node::leaf("right", 100),
            ],
        );
        let bounds = Bounds::from_size(800.0, 500.0);
        let tiles = layout(&tree, bounds);
        for tile in &tiles {
            assert!(tile.bounds.is_within(bounds));
            for ancestor in 1..tile.path.len() {
                let parent = tiles
                    .iter()
                    .find(|candidate| candidate.path == tile.path[..ancestor])
                    .expect("parent is emitted before children");
                assert!(tile.bounds.is_within(parent.bounds));
            }
        }
    }

    #[test]
    fn recursion_limit_makes_deeper_directories_drilldown_targets() {
        let tree = Node::new(
            "root",
            0,
            vec![Node::new(
                "directory",
                0,
                vec![Node::new("nested", 0, vec![Node::leaf("file", 100)])],
            )],
        );
        let tiles = layout_with_depth(&tree, Bounds::from_size(400.0, 300.0), 1);
        assert_eq!(tiles.len(), 2);
        assert!(!tiles[1].render_children);
        assert_eq!(tiles[1].path, vec![0]);
    }

    #[test]
    fn children_leave_a_directory_frame_visible() {
        let tree = Node::new(
            "root",
            0,
            vec![Node::new("directory", 0, vec![Node::leaf("file", 100)])],
        );
        let tiles = layout(&tree, Bounds::from_size(400.0, 300.0));
        assert!(tiles[0].is_directory && tiles[1].is_directory && !tiles[2].is_directory);
        assert_eq!(tiles[1].content.x, tiles[1].bounds.x + DIRECTORY_PADDING);
        assert_eq!(
            tiles[1].content.y,
            tiles[1].bounds.y + DIRECTORY_HEADER + DIRECTORY_PADDING
        );
        assert!(tiles[2].bounds.is_within(tiles[1].content));

        let cutoff = layout_with_depth(&tree, Bounds::from_size(400.0, 300.0), 1);
        assert!(cutoff[1].is_directory);
        assert!(!cutoff[1].render_children);
    }

    #[test]
    fn row_orientation_matches_the_selected_strip() {
        let tiles = squarify(
            vec![(0, 10_000.0), (1, 10_000.0), (2, 1.0)],
            Bounds::from_size(400.0, 300.0),
        );
        let (_, first) = tiles[0];
        let (_, second) = tiles[1];
        assert!((first.width - 20000.0 / 300.0).abs() < 0.01);
        assert!((first.height - 150.0).abs() < 0.01);
        assert_eq!(second.width, first.width);
        assert_eq!(second.height, first.height);
    }
}
