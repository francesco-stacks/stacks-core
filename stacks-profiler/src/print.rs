//! Tree-formatted output for [`ProfileStats`].
//!
//! The module provides a [`TreeFormatter`] trait and a built-in
//! [`PrettyPrinter`] implementation that renders an ANSI-coloured tree
//! to any `fmt::Write` sink.  Custom formatters can be plugged in via
//! [`ProfileStats::print_with`].

use std::fmt::Write;

use crate::{ProfileStats, Tag};

/// ANSI escape sequences for terminal colouring.
struct Style;

#[allow(unused)]
impl Style {
    const RESET: &str = "\x1b[0m";
    const BOLD: &str = "\x1b[1m";
    const GRAY: &str = "\x1b[90m";
    const RED: &str = "\x1b[31m";
    const DIM: &str = "\x1b[2m";
    const GREEN: &str = "\x1b[32m";
    const YELLOW: &str = "\x1b[33m";
    const CYAN: &str = "\x1b[36m";
    const BLUE: &str = "\x1b[34m";
    const WHITE: &str = "\x1b[37m";
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tag::U64(v) => write!(f, "{}", v),
            Tag::I64(v) => write!(f, "{}", v),
            Tag::Usize(v) => write!(f, "{}", v),
            Tag::Str(v) => write!(f, "{}", v),
        }
    }
}

/// Context passed to the formatter for the current node being visited.
pub struct NodeContext<'a> {
    pub stats: &'a ProfileStats,
    pub depth: usize,
    pub is_last_sibling: bool,
    pub is_root: bool,
    /// The visual prefix string (e.g., "│   ├── ") built by the traversal logic.
    pub prefix: &'a str,
    /// The connector string (e.g., "├── " or "└── ") for this specific node.
    pub connector: &'a str,
}

/// Trait for customizing how the profile tree is rendered.
pub trait TreeFormatter {
    /// Called for every node in the tree during traversal.
    ///
    /// Implementors should write a single line representing the node to `writer`.
    fn format_node<W: Write>(&self, ctx: &NodeContext, writer: &mut W) -> std::fmt::Result;
}

/// Default formatter — produces an ANSI-coloured tree with wall, CPU, and
/// wait times in milliseconds, call counts, and source locations.
pub struct PrettyPrinter;

impl TreeFormatter for PrettyPrinter {
    fn format_node<W: Write>(&self, ctx: &NodeContext, writer: &mut W) -> std::fmt::Result {
        let reset = Style::RESET;
        let gray = Style::GRAY;
        let bold = Style::BOLD;
        let cyan = Style::CYAN;
        let dim = Style::DIM;
        let white = Style::WHITE;
        let red = Style::RED;

        let stats = ctx.stats;
        let name = stats.id.name;
        let file = stats.id.file;
        let line = stats.id.line;

        // 1. Icon & Name
        let name_icon = if ctx.is_root { "" } else { "▶" };

        // 2. Tag
        let tag_display = if let Some(t) = stats.tag {
            match t {
                Tag::U64(v) => format!(" {cyan}#{v}{reset}"),
                Tag::I64(v) => format!(" {cyan}#{v}{reset}"),
                Tag::Usize(v) => format!(" {cyan}#{v}{reset}"),
                Tag::Str(v) => format!(" {cyan}[{v}]{reset}"),
            }
        } else {
            String::new()
        };

        // 3. Metrics
        let wall_ms = stats.wall_time_ns as f64 / 1_000_000.0;
        let cpu_ms = stats.cpu_time_ns as f64 / 1_000_000.0;
        let wait_ns = stats.wait_time_ns();
        let wait_ms = wait_ns as f64 / 1_000_000.0;
        let count = stats.entered_count;

        let wait_color = if wait_ns > stats.cpu_time_ns {
            red
        } else {
            gray
        };

        let metrics = format!(
            "{reset}{dim}[total: {cyan}{wall_ms:.3}ms {reset}{dim}| busy: {cyan}{cpu_ms:.3}ms{reset} {dim}| wait: {reset}{wait_color}{wait_ms:.3}ms{reset}{dim}]{reset} {gray}(x{count}){reset}"
        );

        let source_loc = format!("{reset}{dim}{gray}@ {file}:{line}{reset}");

        // 4. Write Line
        if ctx.is_root {
            writeln!(
                writer,
                "{bold}{white}{name}{tag_display} {metrics} {source_loc}"
            )
        } else {
            writeln!(
                writer,
                "{gray}{}{}{reset}{gray}{name_icon}{reset} {bold}{white}{name}{tag_display} {metrics} {source_loc}",
                ctx.prefix, ctx.connector
            )
        }
    }
}

/// Render a [`ProfileStats`] tree to stdout using the given formatter.
pub fn print_tree<F: TreeFormatter>(stats: &ProfileStats, formatter: &F) {
    let mut buffer = String::new();
    // We ignore write errors to stdout/string buffer usually
    let _ = write_tree_recursive(stats, formatter, &mut buffer, "", "", true, 0);
    print!("{}", buffer);
}

fn write_tree_recursive<F: TreeFormatter, W: Write>(
    stats: &ProfileStats,
    formatter: &F,
    writer: &mut W,
    prefix: &str,
    connector: &str,
    is_root: bool,
    depth: usize,
) -> std::fmt::Result {
    // 1. Format current node
    let ctx = NodeContext {
        stats,
        depth,
        is_last_sibling: connector == "└── ", // Heuristic based on connector
        is_root,
        prefix,
        connector,
    };
    formatter.format_node(&ctx, writer)?;

    // 2. Recurse children
    let len = stats.children.len();
    for (i, child) in stats.children.iter().enumerate() {
        let is_last = i == len - 1;
        let child_connector = if is_last { "└── " } else { "├── " };

        // Calculate new prefix for the child
        // If we are root, we don't add prefix yet.
        // If we are not root:
        //   - If we were the last sibling, our children don't see our pipe "│"
        //   - If we were NOT the last sibling, our children see our pipe "│"
        let child_prefix_segment = if is_root {
            ""
        } else if connector == "└── " {
            "    "
        } else {
            "│   "
        };

        let child_prefix = format!("{}{}", prefix, child_prefix_segment);

        write_tree_recursive(
            child,
            formatter,
            writer,
            &child_prefix,
            child_connector,
            false,
            depth + 1,
        )?;
    }
    Ok(())
}
