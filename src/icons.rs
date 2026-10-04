//! Font Awesome SVG icons used by the native egui interface.

use std::sync::{LazyLock, OnceLock};

use eframe::egui::{self, Color32, Response, Stroke, Ui, Vec2, WidgetInfo, WidgetType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaIcon {
    Add,
    Brain,
    CaretDown,
    CaretLeft,
    CaretRight,
    CaretUp,
    Close,
    Code,
    Editor,
    File,
    Folder,
    FolderOpen,
    Info,
    Ingest,
    Lint,
    Output,
    Prompts,
    Preview,
    Query,
    Refresh,
    Save,
    Search,
    Split,
    Tools,
    Trash,
    Update,
}

impl FaIcon {
    #[cfg(test)]
    pub const ALL: [Self; 26] = [
        Self::Add,
        Self::Brain,
        Self::CaretDown,
        Self::CaretLeft,
        Self::CaretRight,
        Self::CaretUp,
        Self::Close,
        Self::Code,
        Self::Editor,
        Self::File,
        Self::Folder,
        Self::FolderOpen,
        Self::Info,
        Self::Ingest,
        Self::Lint,
        Self::Output,
        Self::Prompts,
        Self::Preview,
        Self::Query,
        Self::Refresh,
        Self::Save,
        Self::Search,
        Self::Split,
        Self::Tools,
        Self::Trash,
        Self::Update,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Brain => "LLM",
            Self::CaretDown => "Collapse down",
            Self::CaretLeft => "Collapse left",
            Self::CaretRight => "Expand right",
            Self::CaretUp => "Collapse up",
            Self::Close => "Close",
            Self::Code => "Markdown source",
            Self::Editor => "Markdown editor",
            Self::File => "File",
            Self::Folder => "Workspace",
            Self::FolderOpen => "Open folder",
            Self::Info => "Information",
            Self::Ingest => "Ingest",
            Self::Lint => "Lint",
            Self::Output => "Output",
            Self::Prompts => "Prompts",
            Self::Preview => "Preview",
            Self::Query => "AI Query",
            Self::Refresh => "Refresh",
            Self::Save => "Save",
            Self::Search => "Search",
            Self::Split => "Split view",
            Self::Tools => "Tools",
            Self::Trash => "Remove",
            Self::Update => "Update",
        }
    }

    const fn uri(self) -> &'static str {
        match self {
            Self::Add => "bytes://font-awesome/plus.svg",
            Self::Brain => "bytes://font-awesome/brain.svg",
            Self::CaretDown => "bytes://font-awesome/caret-down.svg",
            Self::CaretLeft => "bytes://font-awesome/caret-left.svg",
            Self::CaretRight => "bytes://font-awesome/caret-right.svg",
            Self::CaretUp => "bytes://font-awesome/caret-up.svg",
            Self::Close => "bytes://font-awesome/xmark.svg",
            Self::Code => "bytes://font-awesome/code.svg",
            Self::Editor => "bytes://font-awesome/file-pen.svg",
            Self::File => "bytes://font-awesome/file.svg",
            Self::Folder => "bytes://font-awesome/folder.svg",
            Self::FolderOpen => "bytes://font-awesome/folder-open.svg",
            Self::Info => "bytes://font-awesome/circle-info.svg",
            Self::Ingest => "bytes://font-awesome/database.svg",
            Self::Lint => "bytes://font-awesome/list-check.svg",
            Self::Output => "bytes://font-awesome/terminal.svg",
            Self::Prompts => "bytes://font-awesome/file-lines.svg",
            Self::Preview => "bytes://font-awesome/eye.svg",
            Self::Query => "bytes://font-awesome/comment-dots.svg",
            Self::Refresh => "bytes://font-awesome/arrows-rotate.svg",
            Self::Save => "bytes://font-awesome/floppy-disk.svg",
            Self::Search => "bytes://font-awesome/magnifying-glass.svg",
            Self::Split => "bytes://font-awesome/table-columns.svg",
            Self::Tools => "bytes://font-awesome/wrench.svg",
            Self::Trash => "bytes://font-awesome/trash.svg",
            Self::Update => "bytes://font-awesome/wand-magic-sparkles.svg",
        }
    }

    fn bytes(self) -> &'static [u8] {
        macro_rules! cached_svg {
            ($path:path) => {{
                static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
                BYTES
                    .get_or_init(|| pictogram::svg!($path).to_string().into_bytes())
                    .as_slice()
            }};
        }
        match self {
            Self::Add => cached_svg!(pictogram::font_awesome::plus::solid),
            Self::Brain => cached_svg!(pictogram::font_awesome::brain::solid),
            Self::CaretDown => cached_svg!(pictogram::font_awesome::caret_down::solid),
            Self::CaretLeft => cached_svg!(pictogram::font_awesome::caret_left::solid),
            Self::CaretRight => cached_svg!(pictogram::font_awesome::caret_right::solid),
            Self::CaretUp => cached_svg!(pictogram::font_awesome::caret_up::solid),
            Self::Close => cached_svg!(pictogram::font_awesome::xmark::solid),
            Self::Code => cached_svg!(pictogram::font_awesome::code::solid),
            Self::Editor => cached_svg!(pictogram::font_awesome::file_pen::solid),
            Self::File => cached_svg!(pictogram::font_awesome::file::regular),
            Self::Folder => cached_svg!(pictogram::font_awesome::folder::solid),
            Self::FolderOpen => cached_svg!(pictogram::font_awesome::folder_open::solid),
            Self::Info => cached_svg!(pictogram::font_awesome::circle_info::solid),
            Self::Ingest => cached_svg!(pictogram::font_awesome::database::solid),
            Self::Lint => cached_svg!(pictogram::font_awesome::list_check::solid),
            Self::Output => cached_svg!(pictogram::font_awesome::terminal::solid),
            Self::Prompts => cached_svg!(pictogram::font_awesome::file_lines::solid),
            Self::Preview => cached_svg!(pictogram::font_awesome::eye::solid),
            Self::Query => cached_svg!(pictogram::font_awesome::comment_dots::solid),
            Self::Refresh => cached_svg!(pictogram::font_awesome::arrows_rotate::solid),
            Self::Save => cached_svg!(pictogram::font_awesome::floppy_disk::solid),
            Self::Search => cached_svg!(pictogram::font_awesome::magnifying_glass::solid),
            Self::Split => cached_svg!(pictogram::font_awesome::table_columns::solid),
            Self::Tools => cached_svg!(pictogram::font_awesome::wrench::solid),
            Self::Trash => cached_svg!(pictogram::font_awesome::trash::solid),
            Self::Update => cached_svg!(pictogram::font_awesome::wand_magic_sparkles::solid),
        }
    }

    pub fn image(self, size: f32) -> egui::Image<'static> {
        egui::Image::from_bytes(self.uri(), self.bytes())
            .fit_to_exact_size(Vec2::splat(size))
            .alt_text(self.label())
    }

    /// Returns a white-backed SVG that egui can reliably tint to any UI color.
    pub fn tintable_image(self, size: f32) -> egui::Image<'static> {
        let uri = self
            .uri()
            .replace("font-awesome/", "font-awesome/tintable/");
        egui::Image::from_bytes(uri, self.tintable_bytes())
            .fit_to_exact_size(Vec2::splat(size))
            .alt_text(self.label())
    }

    fn tintable_bytes(self) -> &'static [u8] {
        static TINTABLE_BYTES: LazyLock<[OnceLock<Vec<u8>>; 27]> =
            LazyLock::new(|| std::array::from_fn(|_| OnceLock::new()));
        TINTABLE_BYTES[self as usize]
            .get_or_init(|| {
                String::from_utf8_lossy(self.bytes())
                    .replace("currentColor", "#ffffff")
                    .into_bytes()
            })
            .as_slice()
    }
}

pub fn text_button(icon: FaIcon, text: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::image_and_text(icon.image(13.0), text).image_tint_follows_text_color(true)
}

pub fn icon_button(ui: &mut Ui, id: &str, icon: FaIcon, label: &str) -> Response {
    let response = ui
        .push_id(id, |ui| {
            ui.add(
                egui::Button::image(icon.image(13.0))
                    .image_tint_follows_text_color(true)
                    .frame(false)
                    .min_size(Vec2::new(24.0, 22.0)),
            )
        })
        .inner;
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    response.on_hover_text(label)
}

pub fn rail_button(
    ui: &mut Ui,
    icon: FaIcon,
    label: &str,
    selected: bool,
    accent: Color32,
) -> Response {
    let border = ui.visuals().widgets.inactive.bg_stroke.color;
    let icon_color = if selected {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    let button = egui::Button::image(icon.image(16.0).tint(icon_color))
        .min_size(Vec2::splat(34.0))
        .fill(if selected {
            accent
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(1.0_f32, if selected { accent } else { border }));
    let response = ui.add(button);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    response.on_hover_text(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_font_awesome_icon_has_embedded_svg_data() {
        for icon in FaIcon::ALL {
            let svg = std::str::from_utf8(icon.bytes()).expect("SVG must be UTF-8");
            assert!(svg.contains("<svg"), "missing SVG for {icon:?}");
            assert!(svg.contains("viewBox"), "missing viewBox for {icon:?}");
        }
    }

    #[test]
    fn tintable_font_awesome_icons_have_a_white_svg_source() {
        for icon in FaIcon::ALL {
            let svg = std::str::from_utf8(icon.tintable_bytes()).expect("SVG must be UTF-8");
            assert!(!svg.contains("currentColor"), "untintable SVG for {icon:?}");
            assert!(svg.contains("#ffffff"), "missing white source for {icon:?}");
        }
    }
}
