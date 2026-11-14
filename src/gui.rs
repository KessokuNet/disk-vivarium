use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Align2, Color32, FontId, Sense, Stroke, StrokeKind, Vec2};
use parallel_disk_usage::bytes_format::BytesFormat;

use crate::{
    Node,
    scan::{Scan, ScanOptions, ScanOutcome, SizeMode},
    treemap::{Bounds, DIRECTORY_HEADER, DIRECTORY_PADDING, OwnedTile, layout_with_depth},
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const TREE_REFRESH: Duration = Duration::from_millis(300);
const SUB_SCAN_DEPTH: u64 = 2;

pub struct Vivarium {
    root_text: String,
    options: ScanOptions,
    render_depth: usize,
    running: Option<Scan>,
    outcome: Option<Arc<ScanOutcome>>,
    partial: Option<Arc<Node>>,
    last_tree_refresh: Option<Instant>,
    outcome_root: PathBuf,
    focus: Vec<usize>,
    selected: Vec<usize>,
    error: Option<String>,
    started: Option<Instant>,
    elapsed: Option<Duration>,
    generation: u64,
    show_errors: bool,
    layout_key: Option<LayoutKey>,
    tiles: Vec<OwnedTile>,
}

#[derive(Debug, Clone, PartialEq)]
struct LayoutKey {
    generation: u64,
    focus: Vec<usize>,
    bounds: Bounds,
    render_depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectoryAction {
    Focus(Vec<usize>),
    Scan(PathBuf),
}

impl Vivarium {
    pub fn new(root: PathBuf) -> Self {
        let mut app = Self {
            root_text: root.to_string_lossy().into_owned(),
            options: ScanOptions::new(root.clone()),
            render_depth: 2,
            running: None,
            outcome: None,
            partial: None,
            last_tree_refresh: None,
            outcome_root: root,
            focus: Vec::new(),
            selected: Vec::new(),
            error: None,
            started: None,
            elapsed: None,
            generation: 0,
            show_errors: false,
            layout_key: None,
            tiles: Vec::new(),
        };
        app.options.max_depth = SUB_SCAN_DEPTH;
        app.start_scan();
        app
    }

    fn start_scan(&mut self) {
        let root = PathBuf::from(self.root_text.trim());
        if root.as_os_str().is_empty() {
            self.error = Some("choose a directory to scan".into());
            return;
        }
        self.options.root = root;
        match Scan::spawn(self.options.clone()) {
            Ok(scan) => {
                self.running = Some(scan);
                self.started = Some(Instant::now());
                self.partial = None;
                self.outcome = None;
                self.last_tree_refresh = None;
                self.focus.clear();
                self.selected.clear();
                self.generation += 1;
                self.layout_key = None;
                self.error = None;
                self.show_errors = false;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(running) = &mut self.running else {
            return;
        };
        ctx.request_repaint_after(POLL_INTERVAL);
        if self
            .last_tree_refresh
            .is_none_or(|last| last.elapsed() >= TREE_REFRESH)
        {
            if let Some(tree) = running.tree_snapshot() {
                self.partial = Some(Arc::new(tree));
                self.generation += 1;
                self.layout_key = None;
            }
            self.last_tree_refresh = Some(Instant::now());
        }
        let Some(result) = running.try_finish() else {
            return;
        };
        self.running = None;
        self.elapsed = self.started.map(|started| started.elapsed());
        match result {
            Ok(outcome) => {
                self.focus.clear();
                self.selected.clear();
                self.generation += 1;
                self.layout_key = None;
                self.show_errors = !outcome.errors.is_empty();
                self.outcome_root = self.options.root.clone();
                self.outcome = Some(Arc::new(outcome));
                self.partial = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn node_at(&self, path: &[usize]) -> Option<&Node> {
        let tree = self
            .outcome
            .as_ref()
            .map(|outcome| &outcome.tree)
            .or(self.partial.as_deref())?;
        let mut node = tree;
        for &index in path {
            node = node.children.get(index)?;
        }
        Some(node)
    }

    fn node_path(&self, path: &[usize]) -> String {
        let mut full = self.current_root().to_path_buf();
        let Some(mut node) = self.node_at(&[]) else {
            return full.display().to_string();
        };
        for &index in path {
            let Some(child) = node.children.get(index) else {
                break;
            };
            node = child;
            full.push(&node.name);
        }
        full.display().to_string()
    }

    fn current_root(&self) -> &Path {
        if self.running.is_some() {
            &self.options.root
        } else {
            &self.outcome_root
        }
    }

    fn directory_action(&self, path: &[usize]) -> Option<DirectoryAction> {
        let node = self.node_at(path)?;
        if !node.is_directory {
            return None;
        }
        if node.needs_sub_scan {
            self.running
                .is_none()
                .then(|| DirectoryAction::Scan(PathBuf::from(self.node_path(path))))
        } else {
            Some(DirectoryAction::Focus(path.to_vec()))
        }
    }

    fn open_directory(&mut self, path: &[usize]) {
        if let Some(action) = self.directory_action(path) {
            self.navigate(action);
        }
    }

    fn open_entry(&mut self, path: &[usize]) {
        let Some(node) = self.node_at(path) else {
            return;
        };
        if node.is_directory {
            self.open_directory(path);
        } else if let Some((_, parent)) = path.split_last() {
            self.open_directory(parent);
        } else if self.running.is_none() {
            let parent = self.current_root().parent().map(Path::to_path_buf);
            if let Some(parent) = parent {
                self.navigate(DirectoryAction::Scan(parent));
            }
        }
    }

    fn navigate(&mut self, action: DirectoryAction) {
        match action {
            DirectoryAction::Focus(path) => {
                self.focus = path.clone();
                self.selected = path;
            }
            DirectoryAction::Scan(path) => {
                self.root_text = path.to_string_lossy().into_owned();
                self.start_scan();
            }
        }
    }

    fn toolbar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(root, |ui| {
            ui.horizontal(|ui| {
                let enabled = self.running.is_none();
                if ui
                    .add_enabled(enabled, egui::Button::new("Folder…"))
                    .clicked()
                {
                    let dialog = rfd::FileDialog::new().set_directory(&self.options.root);
                    if let Some(root) = dialog.pick_folder() {
                        self.root_text = root.to_string_lossy().into_owned();
                        self.start_scan();
                    }
                }
                if ui.add_enabled(enabled, egui::Button::new("Scan")).clicked() {
                    self.start_scan();
                }
                let edit = egui::TextEdit::singleline(&mut self.root_text)
                    .desired_width(380.0)
                    .hint_text("directory to scan");
                if ui.add_enabled(enabled, edit).lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                {
                    self.start_scan();
                }
            });
            ui.horizontal_wrapped(|ui| {
                let mut stay = !self.options.cross_device;
                ui.checkbox(&mut stay, "Stay on one filesystem");
                self.options.cross_device = !stay;
                ui.checkbox(
                    &mut self.options.dedupe_hardlinks,
                    "Count hardlinked files once",
                );
                let btrfs = ui.checkbox(
                    &mut self.options.btrfs,
                    "Exact Btrfs compression and reflinks",
                );
                btrfs.on_hover_text("Requires root or CAP_SYS_ADMIN; falls back to block counts.");
                let mut apparent = self.options.size_mode == SizeMode::Apparent;
                ui.checkbox(&mut apparent, "Apparent size instead of allocated");
                self.options.size_mode = if apparent {
                    SizeMode::Apparent
                } else {
                    SizeMode::Allocated
                };
                ui.add(egui::Slider::new(&mut self.render_depth, 1..=4).text("Display levels"));
                if let Some(node) = self.node_at(&[]) {
                    ui.separator();
                    ui.label(format!("{} here", format_bytes(node.size)));
                }
            });
        });
    }

    fn accounting_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("btrfs-accounting").show(root, |ui| {
            if let Some(outcome) = &self.outcome {
                if let Some(report) = &outcome.btrfs {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "Btrfs · {} files · {} unique extents · {}",
                            report.files,
                            report.unique_extents,
                            btrfs_usage(report.total)
                        ));
                        for (kind, usage) in &report.by_compression {
                            ui.label(format!("{}: {}", kind.label(), percentage(usage.ratio())))
                                .on_hover_text(btrfs_usage(*usage));
                        }
                    });
                }
                if let Some(reason) = &outcome.btrfs_unavailable {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Using block counts instead of Btrfs extents: {reason}"),
                    );
                }
            } else if let Some(usage) = self.partial.as_deref().and_then(|tree| tree.btrfs) {
                ui.label(format!("Btrfs, partial scan · {}", btrfs_usage(usage)));
            }
        });
    }

    fn breadcrumbs(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("breadcrumbs").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut action = None;
                let mut up = self.focus.clone();
                let can_go_up = up.pop().is_some() || self.current_root().parent().is_some();
                if ui.add_enabled(can_go_up, egui::Button::new("Up")).clicked() {
                    action = if up.len() < self.focus.len() {
                        Some(DirectoryAction::Focus(up))
                    } else {
                        self.current_root()
                            .parent()
                            .map(|path| DirectoryAction::Scan(path.to_path_buf()))
                    };
                }
                ui.separator();

                let mut ancestors = self
                    .current_root()
                    .ancestors()
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>();
                ancestors.reverse();
                for (index, path) in ancestors.iter().enumerate() {
                    if index > 0 && ancestors[index - 1].parent().is_some() {
                        ui.label("/");
                    }
                    let label = path
                        .file_name()
                        .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy());
                    let is_current_root = path.as_path() == self.current_root();
                    let enabled = self.running.is_none() || is_current_root;
                    let crumb = ui.add_enabled(enabled, egui::Button::new(label));
                    if crumb.clicked() {
                        action = Some(if is_current_root {
                            DirectoryAction::Focus(Vec::new())
                        } else {
                            DirectoryAction::Scan(path.clone())
                        });
                    }
                }
                for depth in 1..=self.focus.len() {
                    let path = &self.focus[..depth];
                    if let Some(node) = self.node_at(path) {
                        ui.label("/");
                        if ui.small_button(&node.name).clicked() {
                            action = Some(DirectoryAction::Focus(path.to_vec()));
                        }
                    }
                }
                if let Some(action) = action {
                    self.navigate(action);
                }
                if let Some(node) = self.node_at(&self.focus) {
                    ui.separator();
                    ui.label(format!(
                        "{} here · double-click a directory to open it",
                        format_bytes(node.size)
                    ));
                }
            });
        });
    }

    fn status_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(running) = &self.running {
                    let progress = running.progress();
                    ui.spinner();
                    ui.label(format!(
                        "{} entries · {} counted · {} unreadable",
                        progress.entries,
                        format_bytes(progress.bytes),
                        progress.errors
                    ));
                    if let Some(started) = self.started {
                        ui.label(format!("{:.1?}", started.elapsed()));
                    }
                } else if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                } else if let Some(outcome) = &self.outcome {
                    let details = self
                        .node_at(&self.selected)
                        .map(|node| {
                            format!(
                                "{} entries · {}",
                                node.node_count(),
                                format_bytes(node.size)
                            )
                        })
                        .unwrap_or_default();
                    ui.label(self.node_path(&self.selected));
                    ui.separator();
                    ui.label(details);
                    if let Some(duration) = self.elapsed {
                        ui.separator();
                        ui.label(format!("scanned in {duration:.1?}"));
                    }
                    if !outcome.errors.is_empty() {
                        let label = format!("{} unreadable", outcome.progress.errors);
                        if ui.selectable_label(self.show_errors, label).clicked() {
                            self.show_errors = !self.show_errors;
                        }
                    }
                } else {
                    ui.label("Choose a directory and press Scan.");
                }
            });
        });
    }

    fn errors_panel(&mut self, root: &mut egui::Ui) {
        if !self.show_errors {
            return;
        }
        let Some(outcome) = &self.outcome else {
            return;
        };
        egui::Panel::right("errors")
            .default_size(340.0)
            .show(root, |ui| {
                ui.heading("Unreadable entries");
                ui.label(format!(
                    "Showing the first {} messages.",
                    outcome.errors.len()
                ));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for error in &outcome.errors {
                        ui.label(error);
                    }
                });
            });
    }

    fn treemap(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            if self.outcome.is_none() && self.partial.is_none() {
                ui.centered_and_justified(|ui| ui.label("Scan a directory to see its treemap."));
                return;
            }
            let available = ui.available_size().max(Vec2::new(1.0, 1.0));
            let (rect, _) = ui.allocate_exact_size(available, Sense::hover());
            let bounds = Bounds {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.width(),
                height: rect.height(),
            };
            self.update_layout(bounds);
            let painter = ui.painter_at(rect);
            let stroke = if ui.visuals().dark_mode {
                Color32::from_gray(15)
            } else {
                Color32::WHITE
            };

            let mut selection = None;
            let mut drill_into = None;
            for index in 0..self.tiles.len() {
                let tile = self.tiles[index].clone();
                let Some(node) = self.node_at(&tile.path) else {
                    continue;
                };
                let tile_rect = to_egui(tile.bounds);
                if !painter.clip_rect().intersects(tile_rect) {
                    continue;
                }
                let depth = tile.path.len();
                painter.rect_filled(tile_rect, 0.0, color(node, depth));

                if tile.is_directory {
                    let header_height = if tile.render_children {
                        tile.content.y - tile.bounds.y - DIRECTORY_PADDING
                    } else {
                        DIRECTORY_HEADER
                    };
                    if header_height > 0.0 {
                        let header = egui::Rect::from_min_size(
                            tile_rect.min,
                            Vec2::new(tile_rect.width(), header_height.min(tile_rect.height())),
                        );
                        painter.rect_filled(header, 0.0, Color32::from_black_alpha(85));
                        painter.with_clip_rect(header).text(
                            header.min + Vec2::new(4.0, 2.0),
                            Align2::LEFT_TOP,
                            format!("{} · {}", node.name, format_bytes(node.size)),
                            FontId::proportional(12.0),
                            Color32::WHITE,
                        );
                    }
                    painter.rect_stroke(
                        tile_rect,
                        0.0,
                        Stroke::new(2.0, stroke),
                        StrokeKind::Inside,
                    );
                } else {
                    painter.rect_stroke(
                        tile_rect,
                        0.0,
                        Stroke::new(0.6, stroke),
                        StrokeKind::Inside,
                    );
                }

                let path = self.node_path(&tile.path);
                let mut tip = format!(
                    "{path}\n{} · {}% of the current view",
                    format_bytes(node.size),
                    percent(node.size, self.node_at(&self.focus).map(|node| node.size))
                );
                if let Some(usage) = node.btrfs {
                    tip.push_str(&format!("\n{}", btrfs_usage(usage)));
                }
                if self.running.is_some() {
                    tip.push_str("\nScan in progress; sizes will change.");
                }
                let response = ui
                    .interact(
                        tile_rect,
                        ui.scope_id().with(("tile", index)),
                        Sense::click(),
                    )
                    .on_hover_ui(|ui| {
                        ui.label(tip);
                    });
                if response.hovered() || self.selected == tile.path {
                    painter.rect_stroke(
                        tile_rect,
                        0.0,
                        Stroke::new(1.5, Color32::from_rgb(255, 215, 64)),
                        StrokeKind::Inside,
                    );
                }
                if response.clicked() {
                    selection = Some(tile.path.clone());
                }
                if response.double_clicked() {
                    drill_into = Some(tile.path.clone());
                }
                if !tile.is_directory && tile_rect.width() >= 54.0 && tile_rect.height() >= 16.0 {
                    let label_painter = painter.with_clip_rect(tile_rect);
                    label_painter.text(
                        tile_rect.min + Vec2::new(3.0, 2.0),
                        Align2::LEFT_TOP,
                        &node.name,
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                    if tile_rect.height() >= 32.0 {
                        label_painter.text(
                            tile_rect.min + Vec2::new(3.0, 17.0),
                            Align2::LEFT_TOP,
                            format_bytes(node.size),
                            FontId::proportional(11.0),
                            Color32::from_white_alpha(220),
                        );
                    }
                }
            }
            if let Some(selection) = selection {
                self.selected = selection;
            }
            if let Some(path) = drill_into {
                self.open_entry(&path);
            }
        });
    }

    fn update_layout(&mut self, bounds: Bounds) {
        let key = LayoutKey {
            generation: self.generation,
            focus: self.focus.clone(),
            bounds,
            render_depth: self.render_depth,
        };
        if self.layout_key.as_ref() == Some(&key) {
            return;
        }
        self.tiles = self
            .node_at(&self.focus)
            .map(|node| layout_with_depth(node, bounds, self.render_depth))
            .unwrap_or_default();
        self.layout_key = Some(key);
    }
}

impl eframe::App for Vivarium {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan(root.ctx());
        self.toolbar(root);
        if self.outcome.is_some() || self.partial.is_some() {
            self.breadcrumbs(root);
        }
        self.accounting_bar(root);
        self.status_bar(root);
        self.errors_panel(root);
        self.treemap(root);
    }
}

fn to_egui(bounds: Bounds) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(bounds.x, bounds.y),
        egui::vec2(bounds.width, bounds.height),
    )
}

fn format_bytes(bytes: u64) -> String {
    BytesFormat::BinaryUnits.format(bytes).to_string()
}

fn percentage(ratio: Option<f64>) -> String {
    ratio.map_or_else(|| "—".into(), |ratio| format!("{:.1}%", ratio * 100.0))
}

fn btrfs_usage(usage: crate::BtrfsUsage) -> String {
    format!(
        "disk {} · uncompressed {} · referenced {} · {} ratio",
        format_bytes(usage.disk),
        format_bytes(usage.uncompressed),
        format_bytes(usage.referenced),
        percentage(usage.ratio())
    )
}

fn percent(size: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!("{:.1}", size as f64 / total as f64 * 100.0),
        _ => "—".into(),
    }
}

const PALETTE: [Color32; 12] = [
    Color32::from_rgb(54, 111, 196),
    Color32::from_rgb(40, 146, 122),
    Color32::from_rgb(176, 107, 44),
    Color32::from_rgb(145, 77, 165),
    Color32::from_rgb(180, 70, 78),
    Color32::from_rgb(69, 132, 51),
    Color32::from_rgb(43, 138, 172),
    Color32::from_rgb(157, 90, 59),
    Color32::from_rgb(106, 96, 183),
    Color32::from_rgb(179, 121, 37),
    Color32::from_rgb(56, 141, 151),
    Color32::from_rgb(160, 75, 122),
];
fn color(node: &Node, depth: usize) -> Color32 {
    let category = if !node.is_directory {
        Path::new(&node.name)
            .extension()
            .map_or("file".into(), |extension| {
                extension.to_string_lossy().into_owned().to_lowercase()
            })
    } else {
        "directory".into()
    };
    let mut hasher = DefaultHasher::new();
    category.hash(&mut hasher);
    node.name.hash(&mut hasher);
    let color = PALETTE[(hasher.finish() as usize) % PALETTE.len()];
    color.gamma_multiply((1.0 - depth as f32 * 0.04).max(0.65))
}

#[cfg(test)]
mod tests {
    use super::{DirectoryAction, Vivarium};
    use crate::{
        Node,
        scan::{ScanOptions, ScanOutcome},
    };
    use std::{path::PathBuf, sync::Arc};

    fn app_with_tree(tree: Node) -> Vivarium {
        let root = PathBuf::from("/");
        Vivarium {
            root_text: root.to_string_lossy().into_owned(),
            options: ScanOptions::new(&root),
            render_depth: 2,
            running: None,
            outcome: Some(Arc::new(ScanOutcome {
                tree,
                progress: Default::default(),
                errors: Vec::new(),
                btrfs: None,
                btrfs_unavailable: None,
            })),
            partial: None,
            last_tree_refresh: None,
            outcome_root: root,
            focus: Vec::new(),
            selected: Vec::new(),
            error: None,
            started: None,
            elapsed: None,
            generation: 0,
            show_errors: false,
            layout_key: None,
            tiles: Vec::new(),
        }
    }

    #[test]
    fn opening_a_root_file_scans_its_parent() {
        let tempdir = tempfile::tempdir().unwrap();
        let file = tempdir.path().join("file");
        std::fs::write(&file, "data").unwrap();
        let mut app = app_with_tree(Node::leaf("file", 4));
        app.root_text = file.to_string_lossy().into_owned();
        app.options.root = file.clone();
        app.outcome_root = file;

        app.open_entry(&[]);

        let outcome = app
            .running
            .take()
            .expect("a root file scans its parent")
            .wait()
            .expect("the parent scan completes");
        assert_eq!(outcome.tree.name, tempdir.path().to_string_lossy().as_ref());
        assert_eq!(outcome.tree.children.len(), 1);
    }

    #[test]
    fn opening_a_file_focuses_its_parent() {
        let tree = Node::new(
            "root",
            0,
            vec![Node::new("folder", 0, vec![Node::leaf("file", 4096)])],
        );
        let mut app = app_with_tree(tree);
        app.open_entry(&[0, 0]);
        assert_eq!(app.focus, vec![0]);
        assert_eq!(app.selected, vec![0]);
        assert!(app.running.is_none());
    }

    #[test]
    fn opening_a_pruned_directory_starts_its_sub_scan() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        std::fs::create_dir_all(root.join("one/two")).expect("nested directory");
        let mut app = Vivarium::new(root.to_path_buf());
        let first = app
            .running
            .take()
            .expect("initial scan")
            .wait()
            .expect("initial result");
        let child = first
            .tree
            .children
            .iter()
            .position(|node| node.name == "one");
        let child = child.expect("directory entry");
        assert!(first.tree.children[child].needs_sub_scan);
        app.outcome_root = root.to_path_buf();
        app.outcome = Some(Arc::new(first));

        assert_eq!(
            app.directory_action(&[child]),
            Some(DirectoryAction::Scan(root.join("one")))
        );
        app.open_directory(&[child]);
        let second = app
            .running
            .take()
            .expect("subdirectory scan")
            .wait()
            .expect("subdirectory result");
        assert_eq!(second.tree.name, root.join("one").to_string_lossy());
        assert_eq!(app.root_text, root.join("one").to_string_lossy());
        assert!(second.tree.children[0].needs_sub_scan);
    }
}
