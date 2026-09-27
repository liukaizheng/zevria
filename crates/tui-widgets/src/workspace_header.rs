//! Persistent workspace metadata shown above every session pane.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::{
    text::{display_width, truncate_display_width, truncate_leading_display_width},
    theme::theme,
};

const WORKSPACE_PREFIX: &str = " ";
const GIT_PREFIX: &str = " ";
const HEADER_GAP_WIDTH: usize = 1;
const MIN_GIT_STATUS_WIDTH: usize = 2;
const DETACHED_ID_WIDTH: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
enum GitState {
    Branch(String),
    Detached(String),
    None,
}

impl GitState {
    fn label(&self) -> Option<String> {
        match self {
            Self::Branch(branch) => Some(format!("{GIT_PREFIX}{branch}")),
            Self::Detached(id) => Some(format!("{GIT_PREFIX}detached@{id}")),
            Self::None => None,
        }
    }
}

#[derive(Debug)]
pub struct WorkspaceHeader {
    startup_workspace: PathBuf,
    workspace_display: String,
    git: GitState,
}

impl WorkspaceHeader {
    pub fn new(startup_workspace: PathBuf) -> Self {
        let startup_workspace = absolute_workspace(startup_workspace);
        let home = zevria_foundation::runtime_paths::home_dir().ok();
        let workspace_display = abbreviate_home(&startup_workspace, home.as_deref());
        let mut header = Self {
            startup_workspace,
            workspace_display,
            git: GitState::None,
        };
        header.refresh();
        header
    }

    /// Re-discover the repository and re-read HEAD outside the render path.
    pub fn refresh(&mut self) {
        self.git = discover_git_dir(&self.startup_workspace)
            .map_or(GitState::None, |git_dir| read_head(&git_dir));
    }

    /// Paint exactly one row, keeping the workspace on the left and Git on the
    /// right without ever wrapping or splitting a grapheme cluster.
    pub fn render(&self, buffer: &mut Buffer, area: Rect, accent: Color) {
        let area = area.intersection(buffer.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let row = Rect { height: 1, ..area };
        buffer.set_style(
            row,
            Style::new()
                .fg(theme().text.primary)
                .bg(theme().surfaces.canvas),
        );

        let accent_style = Style::new().fg(accent).add_modifier(Modifier::BOLD);
        let workspace_prefix_width = display_width(WORKSPACE_PREFIX);
        let prefix_width = if usize::from(row.width) > workspace_prefix_width {
            workspace_prefix_width
        } else {
            0
        };
        let (workspace, git) = self.fitted_segments(usize::from(row.width) - prefix_width);
        let (workspace_x, _) =
            buffer.set_stringn(row.x, row.y, WORKSPACE_PREFIX, prefix_width, accent_style);
        if !workspace.is_empty() {
            let prefix_end = workspace.rfind('/').map_or(0, |index| index + 1);
            let (parent, leaf) = workspace.split_at(prefix_end);
            let (leaf_x, _) = buffer.set_stringn(
                workspace_x,
                row.y,
                parent,
                display_width(parent),
                Style::new().fg(theme().text.muted),
            );
            buffer.set_stringn(leaf_x, row.y, leaf, display_width(leaf), accent_style);
        }
        if let Some(git) = git {
            let git_width = display_width(&git);
            let git_x = row
                .right()
                .saturating_sub(u16::try_from(git_width).unwrap_or(u16::MAX));
            buffer.set_stringn(git_x, row.y, &git, git_width, accent_style);
        }
        // Wide graphemes reset continuation cells, including their background.
        buffer.set_style(row, Style::new().bg(theme().surfaces.canvas));
    }

    fn fitted_segments(&self, max_width: usize) -> (String, Option<String>) {
        if max_width == 0 {
            return (String::new(), None);
        }

        let Some(git_label) = self.git.label() else {
            return (
                truncate_leading_display_width(&self.workspace_display, max_width),
                None,
            );
        };
        let min_git_label_width = display_width(GIT_PREFIX) + MIN_GIT_STATUS_WIDTH;
        if max_width
            < 1_usize
                .saturating_add(HEADER_GAP_WIDTH)
                .saturating_add(min_git_label_width)
        {
            return (
                truncate_leading_display_width(&self.workspace_display, max_width),
                None,
            );
        }

        let available = max_width.saturating_sub(HEADER_GAP_WIDTH);
        let workspace_width = display_width(&self.workspace_display);
        let git_width = display_width(&git_label);
        let (workspace_budget, git_budget) =
            if workspace_width.saturating_add(git_width) <= available {
                (workspace_width, git_width)
            } else {
                let balanced_git = available
                    .saturating_sub(available / 2)
                    .max(min_git_label_width)
                    .min(available.saturating_sub(1));
                let balanced_workspace = available.saturating_sub(balanced_git);
                if workspace_width <= balanced_workspace {
                    (workspace_width, available.saturating_sub(workspace_width))
                } else if git_width <= balanced_git {
                    (available.saturating_sub(git_width), git_width)
                } else {
                    (balanced_workspace, balanced_git)
                }
            };

        (
            truncate_leading_display_width(&self.workspace_display, workspace_budget),
            Some(truncate_display_width(&git_label, git_budget)),
        )
    }
}

fn absolute_workspace(workspace: PathBuf) -> PathBuf {
    if workspace.is_absolute() {
        return workspace;
    }
    std::env::current_dir().map_or(workspace.clone(), |current| current.join(workspace))
}

fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    let display = if let Some(home) = home.filter(|home| home.is_absolute())
        && let Ok(relative) = path.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        path.display().to_string()
    };
    // Normalize only presentation, after component-aware home matching. A
    // backslash is a filename character on Unix, not a path separator.
    if cfg!(windows) {
        display.replace('\\', "/")
    } else {
        display
    }
}

fn discover_git_dir(workspace: &Path) -> Option<PathBuf> {
    for ancestor in workspace.ancestors() {
        let marker = ancestor.join(".git");
        match fs::metadata(&marker) {
            Ok(metadata) if metadata.is_dir() => return Some(marker),
            Ok(metadata) if metadata.is_file() => return parse_gitdir_marker(&marker),
            Ok(_) => return None,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

fn parse_gitdir_marker(marker: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(marker).ok()?;
    let mut lines = content.lines();
    let target = lines.next()?.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() || lines.any(|line| !line.trim().is_empty()) {
        return None;
    }
    let target = PathBuf::from(target);
    Some(if target.is_absolute() {
        target
    } else {
        marker.parent()?.join(target)
    })
}

fn read_head(git_dir: &Path) -> GitState {
    let Ok(content) = fs::read_to_string(git_dir.join("HEAD")) else {
        return GitState::None;
    };
    let mut lines = content.lines();
    let Some(head) = lines.next().map(str::trim) else {
        return GitState::None;
    };
    if head.is_empty() || lines.any(|line| !line.trim().is_empty()) {
        return GitState::None;
    }

    if let Some(reference) = head.strip_prefix("ref:").map(str::trim) {
        let Some(branch) = reference.strip_prefix("refs/heads/") else {
            return GitState::None;
        };
        if branch.is_empty() || branch.chars().any(char::is_whitespace) {
            return GitState::None;
        }
        return GitState::Branch(branch.to_string());
    }

    if matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return GitState::Detached(head.chars().take(DETACHED_ID_WIDTH).collect());
    }

    GitState::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use unicode_segmentation::UnicodeSegmentation as _;

    const ACCENT: Color = Color::Green;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture directory");
        }
        fs::write(path, content).expect("write fixture");
    }

    fn rendering_header(path: &str, git: GitState) -> WorkspaceHeader {
        WorkspaceHeader {
            startup_workspace: PathBuf::from(path),
            workspace_display: path.to_string(),
            git,
        }
    }

    fn render(header: &WorkspaceHeader, width: u16, height: u16, area: Rect) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        header.render(&mut buffer, area, ACCENT);
        buffer
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn icon_prefixes_with_trailing_spaces_each_occupy_two_cells() {
        for prefix in [WORKSPACE_PREFIX, GIT_PREFIX] {
            assert!(prefix.ends_with(' '), "prefix {prefix:?}");
            assert_eq!(display_width(prefix), 2, "prefix {prefix:?}");
        }
    }

    fn assert_formatted_path(path: &Path, home: Option<&Path>, expected: &str) {
        assert!(path.is_absolute(), "native absolute fixture: {path:?}");
        let workspace_display = abbreviate_home(path, home);
        assert_eq!(workspace_display, expected);
        let header = WorkspaceHeader {
            startup_workspace: path.to_path_buf(),
            workspace_display,
            git: GitState::None,
        };
        let width = u16::try_from(display_width(WORKSPACE_PREFIX) + display_width(expected))
            .expect("header width");
        let buffer = render(&header, width, 1, Rect::new(0, 0, width, 1));
        let parent_end = expected.rfind('/').map_or(0, |index| index + 1);
        let leaf_start = display_width(WORKSPACE_PREFIX) + display_width(&expected[..parent_end]);
        let mut x = 0;
        for grapheme in format!("{WORKSPACE_PREFIX}{expected}").graphemes(true) {
            let cell = &buffer[(u16::try_from(x).expect("cell offset"), 0)];
            assert_eq!(cell.symbol(), grapheme);
            if (display_width(WORKSPACE_PREFIX)..leaf_start).contains(&x) {
                assert_eq!(cell.fg, theme().text.muted);
                assert_eq!(cell.modifier, Modifier::empty());
            } else {
                assert_eq!(cell.fg, ACCENT);
                assert_eq!(cell.modifier, Modifier::BOLD);
            }
            let next = x + display_width(grapheme);
            for continuation in x + 1..next {
                assert_eq!(
                    buffer[(u16::try_from(continuation).unwrap(), 0)].symbol(),
                    " "
                );
            }
            x = next;
        }
        assert_eq!(x, usize::from(width));
        assert!((0..width).all(|x| buffer[(x, 0)].bg == theme().surfaces.canvas));
    }

    #[test]
    fn home_prefix_is_abbreviated() {
        let (root, root_display) = if cfg!(windows) {
            (Path::new(r"C:\"), "C:/")
        } else {
            (Path::new("/"), "/")
        };
        let home = root.join("home/alex");
        assert!(home.is_absolute());
        for (path, expected) in [
            (
                "home/alex/Documents/codes/zevria",
                "~/Documents/codes/zevria",
            ),
            ("home/alex/用户/🧑🏽‍💻", "~/用户/🧑🏽‍💻"),
            ("home/alex", "~"),
            ("home/alex/", "~"),
        ] {
            assert_formatted_path(&root.join(path), Some(&home), expected);
        }
        for path in ["home/alexandra/workspace", "workspace/project"] {
            assert_formatted_path(
                &root.join(path),
                Some(&home),
                &format!("{root_display}{path}"),
            );
        }
        for home in [None, Some(Path::new("")), Some(Path::new("home/alex"))] {
            assert_formatted_path(
                &root.join("home/alex/workspace"),
                home,
                &format!("{root_display}home/alex/workspace"),
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn drive_and_unc_paths_use_slashes_before_fitting_and_styling() {
        for (path, home, expected) in [
            (r"C:\work\zevria", None, "C:/work/zevria"),
            (r"C:\work/zevria", None, "C:/work/zevria"),
            (
                r"C:\Users\alex\work\zevria",
                Some(r"C:\Users\alex"),
                "~/work/zevria",
            ),
            (r"D:\work\zevria", Some(r"C:\work"), "D:/work/zevria"),
            (
                r"\\server\share\work\zevria",
                None,
                "//server/share/work/zevria",
            ),
            (
                r"\\server\share\home\alex\work\zevria",
                Some(r"\\server\share\home\alex"),
                "~/work/zevria",
            ),
        ] {
            assert_formatted_path(Path::new(path), home.map(Path::new), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn literal_backslashes_remain_filename_characters_in_display_and_styling() {
        let home = Path::new("/home/alex");
        let path = Path::new(r"/home/alex/work\space/zev\ria");
        assert_formatted_path(path, Some(home), r"~/work\space/zev\ria");
        assert_formatted_path(path, None, r"/home/alex/work\space/zev\ria");
        assert_formatted_path(
            Path::new(r"/home/alex\work/zevria"),
            Some(home),
            r"/home/alex\work/zevria",
        );
    }

    #[test]
    fn reads_symbolic_branch_from_normal_git_directory() {
        let workspace = tempdir().expect("workspace");
        write(
            &workspace.path().join(".git/HEAD"),
            "ref: refs/heads/main\n",
        );

        let header = WorkspaceHeader::new(workspace.path().to_path_buf());
        assert_eq!(header.startup_workspace, workspace.path());
        assert_eq!(header.git, GitState::Branch("main".to_string()));
        assert_eq!(header.git.label().as_deref(), Some(" main"));
    }

    #[test]
    fn discovers_repository_above_nested_workspace() {
        let repository = tempdir().expect("repository");
        write(
            &repository.path().join(".git/HEAD"),
            "ref: refs/heads/feature/nested\n",
        );
        let workspace = repository.path().join("one/two/workspace");
        fs::create_dir_all(&workspace).expect("nested workspace");

        let header = WorkspaceHeader::new(workspace);
        assert_eq!(header.git, GitState::Branch("feature/nested".to_string()));
    }

    #[test]
    fn resolves_relative_gitdir_marker() {
        let fixture = tempdir().expect("fixture");
        let workspace = fixture.path().join("worktree");
        let git_dir = fixture.path().join("metadata/worktrees/topic");
        fs::create_dir_all(&workspace).expect("worktree");
        write(
            &workspace.join(".git"),
            "gitdir: ../metadata/worktrees/topic\n",
        );
        write(&git_dir.join("HEAD"), "ref: refs/heads/worktree-topic\n");

        let header = WorkspaceHeader::new(workspace);
        assert_eq!(header.git, GitState::Branch("worktree-topic".to_string()));
    }

    #[test]
    fn shortens_detached_head() {
        let workspace = tempdir().expect("workspace");
        write(
            &workspace.path().join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        );

        let header = WorkspaceHeader::new(workspace.path().to_path_buf());
        assert_eq!(header.git, GitState::Detached("01234567".to_string()));
        assert_eq!(header.git.label().as_deref(), Some(" detached@01234567"));
    }

    #[test]
    fn missing_and_malformed_metadata_fail_silently() {
        let missing = tempdir().expect("missing repository");
        assert_eq!(
            WorkspaceHeader::new(missing.path().to_path_buf()).git,
            GitState::None
        );

        let malformed_head = tempdir().expect("malformed HEAD");
        write(
            &malformed_head.path().join(".git/HEAD"),
            "ref: refs/heads/\n",
        );
        assert_eq!(
            WorkspaceHeader::new(malformed_head.path().to_path_buf()).git,
            GitState::None
        );

        let malformed_marker = tempdir().expect("malformed marker");
        write(&malformed_marker.path().join(".git"), "not a gitdir\n");
        assert_eq!(
            WorkspaceHeader::new(malformed_marker.path().to_path_buf()).git,
            GitState::None
        );
    }

    #[test]
    fn refresh_observes_head_changes() {
        let workspace = tempdir().expect("workspace");
        let head = workspace.path().join(".git/HEAD");
        write(&head, "ref: refs/heads/main\n");
        let mut header = WorkspaceHeader::new(workspace.path().to_path_buf());
        assert_eq!(header.git, GitState::Branch("main".to_string()));

        write(&head, "ref: refs/heads/rebased\n");
        header.refresh();
        assert_eq!(header.git, GitState::Branch("rebased".to_string()));
    }

    #[test]
    fn full_width_header_aligns_and_styles_both_segments() {
        let header = rendering_header("/workspace/project", GitState::Branch("main".to_string()));
        let buffer = render(&header, 40, 1, Rect::new(0, 0, 40, 1));
        let row = row_text(&buffer, 0);
        assert_eq!(row, format!(" /workspace/project{} main", " ".repeat(14)));
        for x in (0..2).chain(13..20).chain(34..40) {
            assert_eq!(buffer[(x, 0)].fg, ACCENT);
            assert_eq!(buffer[(x, 0)].modifier, Modifier::BOLD);
        }
        for x in 2..13 {
            assert_eq!(buffer[(x, 0)].fg, theme().text.muted);
            assert_eq!(buffer[(x, 0)].modifier, Modifier::empty());
        }
        for x in 20..34 {
            assert_eq!(buffer[(x, 0)].symbol(), " ");
            assert_eq!(buffer[(x, 0)].fg, theme().text.primary);
            assert_eq!(buffer[(x, 0)].modifier, Modifier::empty());
        }
        assert!((0..40).all(|x| buffer[(x, 0)].bg == theme().surfaces.canvas));
    }

    #[test]
    fn narrow_header_preserves_a_path_cell_and_compact_git_label() {
        let header = rendering_header(
            "/very/long/workspace",
            GitState::Branch("very-long-branch".to_string()),
        );
        for (width, expected, gap_x) in [
            (7, " …pace", None),
            (8, " …  v…", Some(3)),
            (9, " …e  v…", Some(4)),
            (10, " …ce  v…", Some(5)),
        ] {
            let buffer = render(&header, width, 1, Rect::new(0, 0, width, 1));
            assert_eq!(row_text(&buffer, 0), expected, "width {width}");
            for x in 0..width {
                let cell = &buffer[(x, 0)];
                if Some(x) == gap_x {
                    assert_eq!(cell.symbol(), " ");
                    assert_eq!(cell.fg, theme().text.primary);
                    assert_eq!(cell.modifier, Modifier::empty());
                } else {
                    assert_eq!(cell.fg, ACCENT);
                    assert_eq!(cell.modifier, Modifier::BOLD);
                }
                assert_eq!(cell.bg, theme().surfaces.canvas);
            }
        }
    }

    #[test]
    fn zero_and_tiny_headers_only_show_the_home_icon_with_a_workspace_cell() {
        let header = rendering_header("/very/long/workspace", GitState::Branch("main".to_string()));
        let zero = render(&header, 0, 1, Rect::new(0, 0, 0, 1));
        assert_eq!(zero.area.width, 0);

        for (width, expected) in [
            (1, "…"),
            (2, "…e"),
            (3, " …"),
            (4, " …e"),
            (5, " …ce"),
            (6, " …ace"),
        ] {
            let buffer = render(&header, width, 1, Rect::new(0, 0, width, 1));
            assert_eq!(row_text(&buffer, 0), expected);
            assert!((0..width).all(|x| {
                buffer[(x, 0)].fg == ACCENT
                    && buffer[(x, 0)].modifier == Modifier::BOLD
                    && buffer[(x, 0)].bg == theme().surfaces.canvas
            }));
        }

        let empty = render(&header, 8, 1, Rect::new(0, 0, 8, 0));
        assert_eq!(row_text(&empty, 0), "        ");
    }

    #[test]
    fn unicode_path_truncation_keeps_complete_trailing_graphemes() {
        let header = rendering_header("/用户/🧑🏽‍💻/workspace", GitState::None);
        let (path, git) = header.fitted_segments(10);
        assert_eq!(path, "…workspace");
        assert_eq!(git, None);
        assert!(display_width(&path) <= 10);

        let buffer = render(&header, 12, 1, Rect::new(0, 0, 12, 1));
        assert_eq!(row_text(&buffer, 0), " …workspace");
        assert!(
            (2..12).all(|x| {
                buffer[(x, 0)].fg == ACCENT && buffer[(x, 0)].modifier == Modifier::BOLD
            })
        );
    }

    #[test]
    fn unicode_parent_and_leaf_keep_graphemes_and_cell_offsets() {
        let header = rendering_header("~/用户/🧑🏽‍💻/e\u{301}cho", GitState::None);
        let buffer = render(&header, 20, 1, Rect::new(0, 0, 20, 1));
        assert_eq!(buffer[(4, 0)].symbol(), "用");
        assert_eq!(buffer[(6, 0)].symbol(), "户");
        assert_eq!(buffer[(9, 0)].symbol(), "🧑🏽‍💻");
        for x in [2, 3, 4, 6, 8, 9, 11] {
            assert_eq!(buffer[(x, 0)].fg, theme().text.muted);
            assert_eq!(buffer[(x, 0)].modifier, Modifier::empty());
        }
        assert_eq!(buffer[(12, 0)].symbol(), "e\u{301}");
        for x in 12..16 {
            assert_eq!(buffer[(x, 0)].fg, ACCENT);
            assert_eq!(buffer[(x, 0)].modifier, Modifier::BOLD);
        }
        assert!((0..20).all(|x| buffer[(x, 0)].bg == theme().surfaces.canvas));

        let header = rendering_header("/very/long/用户/e\u{301}cho", GitState::None);
        let (path, _) = header.fitted_segments(10);
        assert_eq!(path, "…用户/e\u{301}cho");
        let buffer = render(&header, 12, 1, Rect::new(0, 0, 12, 1));
        assert_eq!(buffer[(2, 0)].symbol(), "…");
        assert_eq!(buffer[(2, 0)].fg, theme().text.muted);
        assert_eq!(buffer[(8, 0)].symbol(), "e\u{301}");
        assert_eq!(buffer[(8, 0)].fg, ACCENT);
        assert_eq!(buffer[(8, 0)].modifier, Modifier::BOLD);
    }

    #[test]
    fn long_branch_truncates_at_the_right_and_remains_right_aligned() {
        let header = rendering_header(
            "/w",
            GitState::Branch("feature/super-long-branch-name".to_string()),
        );
        let segment_width = 24 - display_width(WORKSPACE_PREFIX);
        let (path, git) = header.fitted_segments(segment_width);
        let git = git.expect("git label");
        assert_eq!(path, "/w");
        assert_eq!(git, " feature/super-lo…");
        assert!(display_width(&path) + HEADER_GAP_WIDTH + display_width(&git) <= segment_width);

        let buffer = render(&header, 24, 1, Rect::new(0, 0, 24, 1));
        assert!(row_text(&buffer, 0).ends_with(&git));
        let x = 24 - u16::try_from(display_width(&git)).expect("git width");
        assert_eq!(buffer[(x, 0)].fg, ACCENT);
        assert_eq!(buffer[(x, 0)].modifier, Modifier::BOLD);
    }

    #[test]
    fn unicode_branch_truncation_keeps_complete_graphemes_and_alignment() {
        let header = rendering_header("/w", GitState::Branch("e\u{301}/用户/🧑🏽‍💻".to_string()));
        for (width, expected_git) in [
            (8, " e\u{301}…"),
            (9, " e\u{301}…"),
            (10, " e\u{301}/…"),
            (12, " e\u{301}/用…"),
            (14, " e\u{301}/用户…"),
            (15, " e\u{301}/用户/…"),
            (16, " e\u{301}/用户/🧑🏽‍💻"),
        ] {
            let (path, git) =
                header.fitted_segments(usize::from(width) - display_width(WORKSPACE_PREFIX));
            assert_eq!(git.as_deref(), Some(expected_git), "width {width}");
            let buffer = render(&header, width, 1, Rect::new(0, 0, width, 1));
            let mut x = width - u16::try_from(display_width(expected_git)).expect("Git width");
            let workspace_end = display_width(WORKSPACE_PREFIX) + display_width(&path);
            assert!(workspace_end + HEADER_GAP_WIDTH <= usize::from(x));
            assert_eq!(buffer[(x - 1, 0)].symbol(), " ");
            for grapheme in expected_git.graphemes(true) {
                assert_eq!(buffer[(x, 0)].symbol(), grapheme, "width {width}, cell {x}");
                assert_eq!(buffer[(x, 0)].fg, ACCENT);
                assert_eq!(buffer[(x, 0)].modifier, Modifier::BOLD);
                x += u16::try_from(display_width(grapheme)).expect("grapheme width");
            }
            assert_eq!(x, width);
            assert!((0..width).all(|x| buffer[(x, 0)].bg == theme().surfaces.canvas));
        }
    }

    #[test]
    fn detached_and_non_git_labels_render_as_expected() {
        let detached = rendering_header("/workspace", GitState::Detached("01234567".to_string()));
        let detached_buffer = render(&detached, 40, 1, Rect::new(0, 0, 40, 1));
        assert!(row_text(&detached_buffer, 0).starts_with(" /workspace"));
        assert!(row_text(&detached_buffer, 0).ends_with(" detached@01234567"));
        assert!((22..40).all(|x| {
            detached_buffer[(x, 0)].fg == ACCENT
                && detached_buffer[(x, 0)].modifier == Modifier::BOLD
        }));
        assert!((0..40).all(|x| detached_buffer[(x, 0)].bg == theme().surfaces.canvas));

        let plain = rendering_header("/workspace", GitState::None);
        let plain_buffer = render(&plain, 20, 1, Rect::new(0, 0, 20, 1));
        assert_eq!(row_text(&plain_buffer, 0), " /workspace        ");
        assert_eq!(plain_buffer[(2, 0)].fg, theme().text.muted);
        for x in (0..2).chain(3..12) {
            assert_eq!(plain_buffer[(x, 0)].fg, ACCENT);
            assert_eq!(plain_buffer[(x, 0)].modifier, Modifier::BOLD);
        }
        assert!((0..20).all(|x| plain_buffer[(x, 0)].bg == theme().surfaces.canvas));
    }

    #[test]
    fn unicode_headers_stay_inside_clipped_rectangles_at_every_width() {
        for git in [
            GitState::None,
            GitState::Branch("用户/e\u{301}/🧑🏽‍💻/long-branch".to_string()),
            GitState::Detached("01234567".to_string()),
        ] {
            let header = rendering_header("/very/long/用户/🧑🏽‍💻/e\u{301}cho", git);
            let source = format!(
                "{WORKSPACE_PREFIX}{} {}…·",
                header.workspace_display,
                header.git.label().unwrap_or_default()
            );
            let graphemes = source.graphemes(true).collect::<Vec<_>>();
            for width in 0..=64 {
                let mut buffer = Buffer::empty(Rect::new(0, 0, 64, 3));
                for y in 0..3 {
                    for x in 0..64 {
                        buffer[(x, y)]
                            .set_char('·')
                            .set_style(Style::default().fg(Color::Blue));
                    }
                }
                let before = buffer.clone();
                let requested = Rect::new(3, 1, width, 2);
                let clipped = requested.intersection(buffer.area);
                header.render(&mut buffer, requested, ACCENT);

                let row_width = usize::from(clipped.width);
                let prefix_width = if row_width > display_width(WORKSPACE_PREFIX) {
                    display_width(WORKSPACE_PREFIX)
                } else {
                    0
                };
                let (path, git) = header.fitted_segments(row_width - prefix_width);
                let workspace_end = prefix_width + display_width(&path);
                assert!(workspace_end <= row_width);
                if let Some(git) = git {
                    let git_start = row_width - display_width(&git);
                    assert!(workspace_end + HEADER_GAP_WIDTH <= git_start);
                    let x = clipped.x + u16::try_from(git_start).expect("Git offset");
                    assert_eq!(buffer[(x, clipped.y)].symbol(), "");
                }

                for y in 0..3 {
                    for x in 0..64 {
                        let cell = &buffer[(x, y)];
                        if y == clipped.y && (clipped.x..clipped.right()).contains(&x) {
                            assert!(
                                graphemes.contains(&cell.symbol()),
                                "partial grapheme at width {width}, cell ({x}, {y}): {cell:?}"
                            );
                            assert_eq!(cell.bg, theme().surfaces.canvas);
                        } else {
                            assert_eq!(cell, &before[(x, y)], "width {width}, cell ({x}, {y})");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn renderer_touches_only_the_first_row_inside_a_non_zero_offset_area() {
        let header = rendering_header("/workspace", GitState::Branch("main".to_string()));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 32, 3));
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                buffer
                    .cell_mut((x, y))
                    .expect("buffer cell")
                    .set_char('·')
                    .set_style(Style::default().fg(Color::Blue));
            }
        }

        header.render(&mut buffer, Rect::new(3, 1, 26, 2), ACCENT);

        for y in [0, 2] {
            assert!((0..buffer.area.width).all(|x| buffer[(x, y)].symbol() == "·"));
        }
        for x in (0..3).chain(29..32) {
            assert_eq!(buffer[(x, 1)].symbol(), "·");
            assert_eq!(buffer[(x, 1)].fg, Color::Blue);
        }
        let rendered = (3..29).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
        assert!(rendered.starts_with(" /workspace"));
        assert!(rendered.ends_with(" main"));
        assert_eq!(buffer[(5, 1)].fg, theme().text.muted);
        for x in (3..5).chain(6..15).chain(23..29) {
            assert_eq!(buffer[(x, 1)].fg, ACCENT);
            assert_eq!(buffer[(x, 1)].modifier, Modifier::BOLD);
        }
        assert!((3..29).all(|x| buffer[(x, 1)].bg == theme().surfaces.canvas));
    }
}
