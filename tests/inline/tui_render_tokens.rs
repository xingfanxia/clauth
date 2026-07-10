//! Tokens-dashboard render tests: the height-aware bar charts, the full-width
//! loading spinner, the honest activity caption, and the granularity badges.

use super::{
    HOUR_TICKS, INDET_BLOCK, activity_lines, bar_chart, bar_chart_sqrt, determinate_bar,
    hour_lines, indeterminate_bar, model_lines,
};
use crate::pricing::{ModelRate, PriceTable};
use crate::profile::{AppConfig, AppState};
use crate::tokens::{DayActivity, DayTokens, ModelTokens, PeriodModel, TokenStats};
use crate::tui::app::{App, Tab, TokenPeriod};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Style;
use ratatui::text::Line;

/// Flatten a `Line`'s spans back into one string (pad spans included).
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Flatten a rendered `TestBackend` buffer to one string of cell symbols.
fn render_dashboard(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h as usize)
        .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
        .map(|(x, y)| buf.content[y * w as usize + x].symbol().to_owned())
        .collect()
}

fn populated_stats() -> TokenStats {
    let daily: Vec<DayTokens> = (0..21)
        .map(|i| DayTokens {
            date: format!("2026-06-{:02}", i + 1),
            tokens: 1_000_000 + (i as u64 % 7) * 3_000_000,
        })
        .collect();
    let activity: Vec<DayActivity> = (0..21)
        .map(|i| DayActivity {
            date: format!("2026-06-{:02}", i + 1),
            messages: 100 + (i as u64 % 5) * 400,
            sessions: 3 + (i as u64 % 4),
            tool_calls: 50 + (i as u64 % 6) * 200,
        })
        .collect();
    let mut hour_counts = [0u64; 24];
    for (h, c) in hour_counts.iter_mut().enumerate() {
        *c = (h as u64 * 7) % 90;
    }
    TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 30_000_000,
            output: 70_000_000,
            cache_read: 4_000_000_000,
            cache_create: 500_000_000,
        }],
        daily,
        activity,
        hour_counts,
        total_input: 100_000_000,
        total_output: 70_000_000,
        total_sessions: 1000,
        total_messages: 200_000,
        first_session_date: Some("2026-01-18T00:00:00Z".into()),
        ..Default::default()
    }
}

fn app_with_stats(period: TokenPeriod) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    app.token_period = period;
    app.token_stats = Some(populated_stats());
    app
}

// ── bar_chart ─────────────────────────────────────────────────────────────────

#[test]
fn bar_chart_peak_fills_the_full_height() {
    // Three columns, the max value (10) in column 0, at 4 rows tall.
    let lines = bar_chart(&[10, 0, 5], 3, 4, Style::default());
    assert_eq!(lines.len(), 4, "one line per chart row");
    for l in &lines {
        assert_eq!(
            line_text(l).chars().next().unwrap(),
            '█',
            "the peak column is a full block in every row"
        );
    }
    // A half-height column is filled only in its lower rows.
    assert_eq!(line_text(&lines[0]).chars().nth(2).unwrap(), ' ');
    assert_eq!(line_text(&lines[3]).chars().nth(2).unwrap(), '█');
}

#[test]
fn bar_chart_zero_series_renders_a_baseline() {
    let lines = bar_chart(&[0, 0, 0], 3, 3, Style::default());
    assert_eq!(lines.len(), 3);
    assert_eq!(
        line_text(&lines[2]),
        "▁▁▁",
        "an all-zero series shows a flat baseline on the bottom row"
    );
    assert_eq!(
        line_text(&lines[0]),
        "   ",
        "rows above the baseline are blank"
    );
}

#[test]
fn bar_chart_short_series_is_left_padded() {
    // Two full-height columns in a 6-wide chart → centered by a 2-cell left pad.
    let lines = bar_chart(&[5, 5], 6, 2, Style::default());
    assert!(
        line_text(&lines[0]).starts_with("  ██"),
        "a short series left-pads the bars, got {:?}",
        line_text(&lines[0])
    );
}

// ── sqrt scale ────────────────────────────────────────────────────────────────

/// Rows of a column occupied by any glyph (full or partial), bottom-up.
fn col_cells(lines: &[Line<'_>], col: usize) -> usize {
    lines
        .iter()
        .filter(|l| line_text(l).chars().nth(col) != Some(' '))
        .count()
}

#[test]
fn bar_chart_sqrt_peak_alone_fills_the_full_height() {
    // One 100× outlier next to quiet days: only the peak column reaches the
    // top row — no p95-style wall of identical full-height columns.
    let mut vals = vec![9_u64; 19];
    vals.push(100);
    let lines = bar_chart_sqrt(&vals, 20, 8, Style::default());
    assert_eq!(
        line_text(&lines[0]).chars().nth(19).unwrap(),
        '█',
        "the peak column reaches the top row"
    );
    assert_eq!(
        line_text(&lines[0]).chars().filter(|&c| c != ' ').count(),
        1,
        "no other column joins the peak at the top"
    );
}

#[test]
fn bar_chart_sqrt_lifts_quiet_days_above_linear() {
    // 9% of the peak: linear leaves it inside the bottom cell; sqrt gives it
    // ~30% of the height so months of normal use stay readable.
    let vals = [9_u64, 100];
    let linear = bar_chart(&vals, 2, 8, Style::default());
    let sqrt = bar_chart_sqrt(&vals, 2, 8, Style::default());
    assert_eq!(col_cells(&linear, 0), 1, "linear flattens the quiet column");
    assert!(
        col_cells(&sqrt, 0) >= 2,
        "sqrt keeps the quiet column readable, got {} cells",
        col_cells(&sqrt, 0)
    );
}

#[test]
fn bar_chart_nonzero_keeps_the_floor_cell() {
    // 1/10_000 of the peak rounds to zero cells; a real day still shows the
    // ▁ floor instead of vanishing, while a true zero day stays blank.
    let lines = bar_chart(&[1, 0, 10_000], 3, 2, Style::default());
    assert_eq!(
        line_text(&lines[1]),
        "▁ █",
        "nonzero floors at ▁, zero stays blank"
    );
}

// ── dashboard width clamp + TOTAL card ────────────────────────────────────────

#[test]
fn dashboard_reflows_to_two_columns_on_big_terminals() {
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 160, 40);
    let row = |y: usize| -> String { out.chars().skip(y * 160).take(160).collect() };
    assert_eq!(
        row(0).chars().next(),
        Some('╭'),
        "the card column starts at the left edge (no centering margin)"
    );
    assert!(
        row(0).contains(" DAILY "),
        "the trend card shares the top row"
    );
    assert!(
        !row(0).contains(" TOTAL "),
        "total stacks under the first card instead of sitting beside it"
    );
    assert!(
        row(6).contains(" TOTAL "),
        "total is the second left-column card"
    );
    let act_row = (0..40).find(|&y| row(y).contains(" ACTIVITY "));
    assert!(
        act_row.is_some_and(|y| y >= 20),
        "activity sits in the lower right half, got row {act_row:?}"
    );
}

#[test]
fn dashboard_clamps_to_a_centered_band_on_wide_short_terminals() {
    // Wide but under the 30-row reflow gate → the single-column centered band.
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 160, 24);
    let row0: String = out.chars().take(160).collect();
    assert!(
        row0.starts_with(&" ".repeat(20)),
        "the left margin outside the 120-col band stays blank"
    );
    assert_eq!(
        row0.chars().nth(20),
        Some('╭'),
        "the first card's border opens at the band edge"
    );
    assert!(
        row0.trim_end().chars().count() <= 140,
        "the right margin outside the band stays blank"
    );
}

#[test]
fn total_card_groups_kv_rows_and_carries_the_range_meta() {
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 120, 24);
    assert!(
        out.contains("jan 18 → jun 21"),
        "lifetime date range rides the title-right meta"
    );
    assert!(out.contains("sessions"), "sessions is a spelled-out kv key");
    assert!(out.contains("1,000"), "session count is comma-grouped");
}

// ── model rows: unpriced cost dash ────────────────────────────────────────────

#[test]
fn model_lines_dash_unpriced_models_when_a_table_is_loaded() {
    let rows: Vec<PeriodModel> = [("claude-opus-4-8", 900_u64), ("glm-5.2", 800)]
        .iter()
        .map(|&(id, tokens)| {
            PeriodModel::from_full(&ModelTokens {
                model: id.into(),
                input: tokens,
                output: 0,
                cache_read: 0,
                cache_create: 0,
            })
        })
        .collect();
    let mut rates = std::collections::HashMap::new();
    rates.insert(
        "claude-opus-4-8".to_string(),
        ModelRate {
            input: 5e-6,
            output: 25e-6,
            cache_read: 5e-7,
            cache_write: 6e-6,
        },
    );
    let prices = PriceTable::from_rates(rates);

    let lines = model_lines(&rows, 60, 5, true, Some(&prices), "no model usage yet");
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert!(
        texts[0].contains('$'),
        "the priced model shows a cost, got {:?}",
        texts[0]
    );
    assert!(
        texts[1].trim_end().ends_with('—'),
        "the unpriced model shows the no-value dash, got {:?}",
        texts[1]
    );

    // No price table at all → the whole cost column stays hidden.
    let bare = model_lines(&rows, 60, 5, true, None, "no model usage yet");
    assert!(
        bare.iter().map(line_text).all(|t| !t.contains('—')),
        "no table → no dash column"
    );
}

// ── hour-of-day ticks ─────────────────────────────────────────────────────────

#[test]
fn hour_lines_carry_baseline_ticks_only_when_tall_enough() {
    let mut hours = [0u64; 24];
    hours[12] = 10;
    let tall: Vec<String> = hour_lines(&hours, 30, 5).iter().map(line_text).collect();
    assert!(
        tall.iter().any(|t| t.contains(HOUR_TICKS)),
        "a tall chart carries the 0/6/12/18 tick row"
    );
    // Ticks sit directly above the caption row.
    assert!(tall[tall.len() - 2].contains(HOUR_TICKS));
    assert!(tall[tall.len() - 1].contains("peak 12:00"));

    let short: Vec<String> = hour_lines(&hours, 30, 2).iter().map(line_text).collect();
    assert!(
        !short.iter().any(|t| t.contains(HOUR_TICKS)),
        "the 2-row floor drops the ticks, not the chart"
    );
}

// ── determinate_bar ───────────────────────────────────────────────────────────

#[test]
fn determinate_bar_is_bare_with_a_trailing_label() {
    let t = line_text(&determinate_bar(1, 2, 10, "scanning session logs 1/2"));
    assert!(
        !t.contains('[') && !t.contains(']'),
        "determinate bars are bare — the [ ] frame is the indeterminate tell"
    );
    assert_eq!(t.matches('█').count(), 5, "half done fills half the track");
    assert_eq!(t.matches('░').count(), 5, "the rest stays track");
    assert!(t.ends_with("scanning session logs 1/2"));
}

// ── indeterminate_bar ─────────────────────────────────────────────────────────

#[test]
fn indeterminate_bar_is_bracketed_with_a_bouncing_block() {
    let t = line_text(&indeterminate_bar(0, 12, "reading ~/.claude…"));
    assert!(t.starts_with('['), "opens with the [ frame");
    assert!(t.contains(']'), "closes with the ] frame");
    assert_eq!(t.matches('█').count(), INDET_BLOCK, "a 4-cell block");
    assert!(t.contains("reading ~/.claude…"), "label trails the bar");

    // The block advances with the tick (bounces), so consecutive frames differ.
    let a = line_text(&indeterminate_bar(0, 12, "x"));
    let b = line_text(&indeterminate_bar(1, 12, "x"));
    assert_ne!(a, b, "the block position advances one cell per tick");
}

// ── activity caption ──────────────────────────────────────────────────────────

#[test]
fn activity_caption_reports_the_busiest_bucket_only() {
    let stats = TokenStats {
        activity: vec![
            DayActivity {
                date: "2026-06-01".into(),
                messages: 50,
                sessions: 2,
                tool_calls: 10,
            },
            DayActivity {
                date: "2026-06-02".into(),
                messages: 116_000,
                sessions: 334,
                tool_calls: 41_000,
            },
            DayActivity {
                date: "2026-06-03".into(),
                messages: 80,
                sessions: 900,
                tool_calls: 90_000,
            },
        ],
        ..Default::default()
    };
    let lines = activity_lines(&stats, 48, 3, TokenPeriod::Lifetime);
    let caption = line_text(lines.last().unwrap());
    // The peak-message bucket's OWN three figures (its 334 sess / 41.0K tools),
    // not the other buckets' higher session/tool maxima.
    assert!(
        caption.contains("peak day: 116K msgs   334 sess   41.0K tools"),
        "caption must report one bucket's real figures, got {caption:?}"
    );
}

#[test]
fn activity_caption_names_the_granularity() {
    let stats = populated_stats();
    let wk = activity_lines(&stats, 48, 3, TokenPeriod::Weekly);
    assert!(
        line_text(wk.last().unwrap()).contains("peak wk:"),
        "weekly lens says `peak wk:`"
    );
    let mo = activity_lines(&stats, 48, 3, TokenPeriod::Monthly);
    assert!(
        line_text(mo.last().unwrap()).contains("peak mo:"),
        "monthly lens says `peak mo:`"
    );
}

// ── granularity badges (rendered) ─────────────────────────────────────────────

#[test]
fn trend_and_activity_badges_read_by_week_and_by_month() {
    let weekly = render_dashboard(&app_with_stats(TokenPeriod::Weekly), 100, 44);
    assert!(weekly.contains("BY WEEK"), "trend title reads `by week`");
    assert!(weekly.contains("by week"), "activity meta reads `by week`");

    let monthly = render_dashboard(&app_with_stats(TokenPeriod::Monthly), 100, 44);
    assert!(monthly.contains("BY MONTH"), "trend title reads `by month`");
    assert!(
        monthly.contains("by month"),
        "activity meta reads `by month`"
    );
}

// ── pre-first-paint placeholder ───────────────────────────────────────────────

#[test]
fn placeholder_shows_the_full_width_bouncing_bar() {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    // token_stats stays None, tokens_failed false → the indeterminate spinner.
    let out = render_dashboard(&app, 100, 10);
    assert!(
        out.contains("parsing stats-cache.json"),
        "the stage-1 loading label renders"
    );
    assert!(out.contains('['), "the bracketed spinner frame renders");
    assert!(out.contains('█'), "the bouncing block renders");
}
