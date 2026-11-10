//! Clawd: the 9×3 block mascot the header draws.
//!
//! The sprite, its pose tables and its fifteen animations are extracted from
//! Claude Code 2.1.309 (its embedded build `1ce7e4ff2da8d4b0…`), where the
//! same component paints the wordmark's left column. Frames advance on the
//! TUI's own 80 ms tick rather than the source's 60 ms redraw: the frame index
//! is `elapsed / 80 ms`, so a frame whose tick runs late is skipped rather
//! than queued, and an extra draw repeats the current frame.
//!
//! Claude Code withholds the full sprite on Apple's Terminal.app and paints a
//! simplified one there instead; the verdict is read once from `TERM_PROGRAM`
//! at TUI startup and carried in as a bool. Both arms are ported: the
//! simplified sprite, and the source's filter that drops facing sequences and
//! animations whose first frame reads as the rest pose, when the simplified
//! arm is in use. Not ported: the source's `prefersReducedMotion` check, which
//! turns the animation off entirely, since clauth has no such setting.
//!
//! Idle motion is the short idle set — look, wink, boop, tap, sneeze — one
//! every ten seconds. The longer set — jump, celebrate, skip, spin, peekaboo,
//! drop, waddle, peek, turn, coin-hop — plays on launch and every thirty
//! minutes; `celebrate` is clauth's addition to Claude Code's pool. Poof and
//! shadow marks paint in the source's `inactive` colour.

use std::sync::LazyLock;

/// Sprite width in cells; the header gives it one more column of padding.
pub(crate) const COLS: usize = 9;
pub(crate) const ROWS: usize = 3;

const FRAME_MS: u64 = 80;
const SUBTLE_PERIOD_MS: u64 = 10_000;
const BIG_PERIOD_MS: u64 = 30 * 60 * 1000;

/// The startup gate: `false` selects the simplified sprite arm, the one
/// Claude Code paints on Apple's Terminal.app (see the module doc). Only
/// `TERM_PROGRAM` is consulted. Claude Code's own gate (as of 2.1.309)
/// checks other variables first, e.g. `CURSOR_TRACE_ID`,
/// `TERMINAL_EMULATOR=JetBrains-JediTerm` or a `kitty` `TERM`,
/// so a Terminal.app pane that exports one of them paints simplified here
/// where Claude Code paints full; the reverse cannot happen, since Claude Code
/// answers `Apple_Terminal` only from the raw `TERM_PROGRAM` read.
pub(crate) fn full_sprite(term_program: Option<&str>) -> bool {
    term_program != Some("Apple_Terminal")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eyes {
    Open,
    Left,
    Right,
    Closed,
    Wink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arms {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feet {
    Both,
    Left,
    Right,
}

/// Body orientations used by the turn/coin-hop sequences. Each names the
/// rotation the source art draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Facing {
    Right12,
    Right30,
    Right55,
    Right75,
    Edge,
    Back105,
    Back125,
    Back150,
    Back,
    Left75,
    Left55,
    Left30,
    Left12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pose {
    Face { eyes: Eyes, arms: Arms, feet: Feet },
    Facing(Facing),
}

const fn face(eyes: Eyes, arms: Arms, feet: Feet) -> Pose {
    Pose::Face { eyes, arms, feet }
}

const DEFAULT: Pose = face(Eyes::Open, Arms::Down, Feet::Both);
const ARMS_UP: Pose = face(Eyes::Open, Arms::Up, Feet::Both);
const LOOK_LEFT: Pose = face(Eyes::Left, Arms::Down, Feet::Both);
const LOOK_RIGHT: Pose = face(Eyes::Right, Arms::Down, Feet::Both);
const CLOSED: Pose = face(Eyes::Closed, Arms::Down, Feet::Both);
const CLOSED_UP: Pose = face(Eyes::Closed, Arms::Up, Feet::Both);
const WINK: Pose = face(Eyes::Wink, Arms::Down, Feet::Both);
const TAP_LEFT: Pose = face(Eyes::Open, Arms::Down, Feet::Left);
const TAP_RIGHT: Pose = face(Eyes::Open, Arms::Down, Feet::Right);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Poof {
    #[default]
    None,
    Dot,
    Wave,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Shadow {
    #[default]
    None,
    Wide,
    Narrow,
}

#[derive(Clone, Copy, Default)]
struct Opts {
    offset: i8,
    x: i8,
    poof: Poof,
    shadow: Shadow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pose: Pose,
    offset: i8,
    x: i8,
    poof: Poof,
    shadow: Shadow,
}

const REST: Frame = Frame {
    pose: DEFAULT,
    offset: 0,
    x: 0,
    poof: Poof::None,
    shadow: Shadow::None,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anim {
    Jump,
    Look,
    Celebrate,
    Skip,
    Spin,
    Peekaboo,
    Drop,
    Waddle,
    Peek,
    Wink,
    Boop,
    Tap,
    Sneeze,
    Turn,
    CoinHop,
}

impl Anim {
    #[cfg(test)]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Jump => "jump",
            Self::Look => "look",
            Self::Celebrate => "celebrate",
            Self::Skip => "skip",
            Self::Spin => "spin",
            Self::Peekaboo => "peekaboo",
            Self::Drop => "drop",
            Self::Waddle => "waddle",
            Self::Peek => "peek",
            Self::Wink => "wink",
            Self::Boop => "boop",
            Self::Tap => "tap",
            Self::Sneeze => "sneeze",
            Self::Turn => "turn",
            Self::CoinHop => "coin-hop",
        }
    }

    /// The animation's frames. A match, not an index into the table, so a new
    /// variant fails to compile here instead of panicking at runtime.
    fn frames(self) -> &'static [Frame] {
        let anims = &*ANIMS;
        match self {
            Self::Jump => &anims.jump,
            Self::Look => &anims.look,
            Self::Celebrate => &anims.celebrate,
            Self::Skip => &anims.skip,
            Self::Spin => &anims.spin,
            Self::Peekaboo => &anims.peekaboo,
            Self::Drop => &anims.drop,
            Self::Waddle => &anims.waddle,
            Self::Peek => &anims.peek,
            Self::Wink => &anims.wink,
            Self::Boop => &anims.boop,
            Self::Tap => &anims.tap,
            Self::Sneeze => &anims.sneeze,
            Self::Turn => &anims.turn,
            Self::CoinHop => &anims.coin_hop,
        }
    }
}

/// In-place animations: the subtle idle set, one per ten-second slot.
const SUBTLE: [Anim; 5] = [Anim::Look, Anim::Wink, Anim::Boop, Anim::Tap, Anim::Sneeze];

/// Travelling or turning animations: the bigger moments, on launch and every
/// thirty minutes.
const BIG: [Anim; 10] = [
    Anim::Jump,
    Anim::Celebrate,
    Anim::Skip,
    Anim::Spin,
    Anim::Peekaboo,
    Anim::Drop,
    Anim::Waddle,
    Anim::Peek,
    Anim::Turn,
    Anim::CoinHop,
];

/// The frame table, one named field per animation.
struct Anims {
    jump: Vec<Frame>,
    look: Vec<Frame>,
    celebrate: Vec<Frame>,
    skip: Vec<Frame>,
    spin: Vec<Frame>,
    peekaboo: Vec<Frame>,
    drop: Vec<Frame>,
    waddle: Vec<Frame>,
    peek: Vec<Frame>,
    wink: Vec<Frame>,
    boop: Vec<Frame>,
    tap: Vec<Frame>,
    sneeze: Vec<Frame>,
    turn: Vec<Frame>,
    coin_hop: Vec<Frame>,
}

static ANIMS: LazyLock<Anims> = LazyLock::new(build_anims);

fn frame(pose: Pose, opts: Opts) -> Frame {
    Frame {
        pose,
        offset: opts.offset,
        x: opts.x,
        poof: opts.poof,
        shadow: opts.shadow,
    }
}

fn push(frames: &mut Vec<Frame>, pose: Pose, count: usize, opts: Opts) {
    frames.extend(std::iter::repeat_n(frame(pose, opts), count));
}

fn push_poof_pair(frames: &mut Vec<Frame>) {
    frames.push(frame(
        DEFAULT,
        Opts {
            offset: 1,
            poof: Poof::Dot,
            ..Opts::default()
        },
    ));
    frames.push(frame(
        DEFAULT,
        Opts {
            offset: 1,
            poof: Poof::Wave,
            ..Opts::default()
        },
    ));
}

/// The eye-close + rest pair several sequences land on.
fn push_blink(frames: &mut Vec<Frame>) {
    push(frames, CLOSED, 1, Opts::default());
    frames.push(frame(DEFAULT, Opts::default()));
}

fn push_facings(frames: &mut Vec<Frame>, facings: &[Facing], shadow: Shadow) {
    for &facing in facings {
        frames.push(frame(
            Pose::Facing(facing),
            Opts {
                shadow,
                ..Opts::default()
            },
        ));
    }
}

fn jump_frames() -> Vec<Frame> {
    let mut frames = Vec::new();
    push_poof_pair(&mut frames);
    push(&mut frames, ARMS_UP, 3, Opts::default());
    push(&mut frames, DEFAULT, 1, Opts::default());
    push_poof_pair(&mut frames);
    push(&mut frames, ARMS_UP, 3, Opts::default());
    push(&mut frames, DEFAULT, 1, Opts::default());
    frames
}

fn build_anims() -> Anims {
    let jump = jump_frames();

    let mut look = Vec::new();
    push(&mut look, LOOK_RIGHT, 5, Opts::default());
    push(&mut look, LOOK_LEFT, 5, Opts::default());
    push(&mut look, DEFAULT, 1, Opts::default());

    let mut celebrate = jump.clone();
    push(
        &mut celebrate,
        DEFAULT,
        3,
        Opts {
            offset: 1,
            ..Opts::default()
        },
    );

    let mut skip = Vec::new();
    push(
        &mut skip,
        DEFAULT,
        1,
        Opts {
            offset: 1,
            x: -9,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        ARMS_UP,
        2,
        Opts {
            x: -6,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        DEFAULT,
        1,
        Opts {
            x: -6,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        DEFAULT,
        1,
        Opts {
            offset: 1,
            x: -6,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        ARMS_UP,
        2,
        Opts {
            x: -3,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        DEFAULT,
        1,
        Opts {
            x: -3,
            ..Opts::default()
        },
    );
    push(
        &mut skip,
        DEFAULT,
        1,
        Opts {
            offset: 1,
            x: -3,
            ..Opts::default()
        },
    );
    push(&mut skip, ARMS_UP, 2, Opts::default());
    push_poof_pair(&mut skip);
    push(&mut skip, DEFAULT, 1, Opts::default());

    let mut spin = Vec::new();
    push(&mut spin, LOOK_LEFT, 2, Opts::default());
    push(&mut spin, LOOK_RIGHT, 2, Opts::default());
    push(&mut spin, LOOK_LEFT, 2, Opts::default());
    push(&mut spin, ARMS_UP, 3, Opts::default());
    push(&mut spin, DEFAULT, 1, Opts::default());

    let mut peekaboo = Vec::new();
    push(
        &mut peekaboo,
        DEFAULT,
        1,
        Opts {
            offset: 3,
            ..Opts::default()
        },
    );
    push(
        &mut peekaboo,
        DEFAULT,
        3,
        Opts {
            offset: 2,
            ..Opts::default()
        },
    );
    push(
        &mut peekaboo,
        LOOK_RIGHT,
        3,
        Opts {
            offset: 2,
            ..Opts::default()
        },
    );
    push(
        &mut peekaboo,
        LOOK_LEFT,
        3,
        Opts {
            offset: 2,
            ..Opts::default()
        },
    );
    push(
        &mut peekaboo,
        DEFAULT,
        2,
        Opts {
            offset: 2,
            ..Opts::default()
        },
    );
    push(
        &mut peekaboo,
        DEFAULT,
        1,
        Opts {
            offset: 1,
            ..Opts::default()
        },
    );
    push(&mut peekaboo, ARMS_UP, 4, Opts::default());
    push(&mut peekaboo, DEFAULT, 2, Opts::default());
    push_blink(&mut peekaboo);

    let mut drop = Vec::new();
    push(
        &mut drop,
        ARMS_UP,
        1,
        Opts {
            offset: -3,
            ..Opts::default()
        },
    );
    push(
        &mut drop,
        ARMS_UP,
        2,
        Opts {
            offset: -2,
            ..Opts::default()
        },
    );
    push(
        &mut drop,
        ARMS_UP,
        2,
        Opts {
            offset: -1,
            ..Opts::default()
        },
    );
    push(&mut drop, ARMS_UP, 1, Opts::default());
    push_poof_pair(&mut drop);
    push(&mut drop, DEFAULT, 3, Opts::default());
    push_blink(&mut drop);

    let mut waddle = Vec::new();
    waddle.push(frame(
        LOOK_RIGHT,
        Opts {
            x: -9,
            ..Opts::default()
        },
    ));
    for x in [-6, -5, -4, -3, -2, -1] {
        let feet = if x % 2 == 0 { Feet::Left } else { Feet::Right };
        push(
            &mut waddle,
            face(Eyes::Right, Arms::Down, feet),
            2,
            Opts {
                x,
                ..Opts::default()
            },
        );
    }
    push(&mut waddle, LOOK_RIGHT, 2, Opts::default());
    push(&mut waddle, DEFAULT, 2, Opts::default());
    push_blink(&mut waddle);

    let mut peek = Vec::new();
    push(
        &mut peek,
        LOOK_RIGHT,
        1,
        Opts {
            x: -9,
            ..Opts::default()
        },
    );
    push(
        &mut peek,
        LOOK_RIGHT,
        5,
        Opts {
            x: -6,
            ..Opts::default()
        },
    );
    push(
        &mut peek,
        LOOK_RIGHT,
        3,
        Opts {
            x: -9,
            ..Opts::default()
        },
    );
    push(
        &mut peek,
        LOOK_RIGHT,
        4,
        Opts {
            x: -5,
            ..Opts::default()
        },
    );
    push(
        &mut peek,
        ARMS_UP,
        2,
        Opts {
            x: -3,
            ..Opts::default()
        },
    );
    push(&mut peek, ARMS_UP, 2, Opts::default());
    push_poof_pair(&mut peek);
    push(&mut peek, DEFAULT, 1, Opts::default());

    let mut wink = Vec::new();
    push(&mut wink, WINK, 5, Opts::default());
    push(&mut wink, DEFAULT, 1, Opts::default());

    let mut boop = Vec::new();
    push(
        &mut boop,
        CLOSED,
        1,
        Opts {
            offset: 1,
            poof: Poof::Dot,
            ..Opts::default()
        },
    );
    push(
        &mut boop,
        CLOSED,
        2,
        Opts {
            offset: 1,
            poof: Poof::Wave,
            ..Opts::default()
        },
    );
    push(&mut boop, LOOK_LEFT, 4, Opts::default());
    push(&mut boop, DEFAULT, 2, Opts::default());
    push_blink(&mut boop);

    let mut tap = Vec::new();
    push(&mut tap, TAP_LEFT, 2, Opts::default());
    push(&mut tap, TAP_RIGHT, 2, Opts::default());
    push(&mut tap, TAP_LEFT, 2, Opts::default());
    push(&mut tap, TAP_RIGHT, 2, Opts::default());
    push(&mut tap, TAP_LEFT, 1, Opts::default());
    push(&mut tap, TAP_RIGHT, 1, Opts::default());
    push(&mut tap, ARMS_UP, 3, Opts::default());
    push(&mut tap, DEFAULT, 1, Opts::default());

    let mut sneeze = Vec::new();
    push(&mut sneeze, CLOSED_UP, 4, Opts::default());
    push(
        &mut sneeze,
        CLOSED,
        2,
        Opts {
            offset: 1,
            poof: Poof::Wave,
            ..Opts::default()
        },
    );
    push(&mut sneeze, CLOSED, 2, Opts::default());
    push(&mut sneeze, DEFAULT, 2, Opts::default());
    push_blink(&mut sneeze);

    let mut turn = Vec::new();
    push_facings(
        &mut turn,
        &[
            Facing::Right12,
            Facing::Right30,
            Facing::Right55,
            Facing::Right75,
            Facing::Edge,
        ],
        Shadow::None,
    );
    push_facings(
        &mut turn,
        &[
            Facing::Back105,
            Facing::Back125,
            Facing::Back150,
            Facing::Back,
            Facing::Back,
        ],
        Shadow::None,
    );
    push_facings(
        &mut turn,
        &[
            Facing::Back150,
            Facing::Back125,
            Facing::Back105,
            Facing::Edge,
        ],
        Shadow::None,
    );
    push_facings(
        &mut turn,
        &[
            Facing::Left75,
            Facing::Left55,
            Facing::Left30,
            Facing::Left12,
        ],
        Shadow::None,
    );
    push(&mut turn, DEFAULT, 1, Opts::default());

    let mut coin_hop = Vec::new();
    push(
        &mut coin_hop,
        DEFAULT,
        2,
        Opts {
            offset: 1,
            ..Opts::default()
        },
    );
    push(
        &mut coin_hop,
        ARMS_UP,
        1,
        Opts {
            shadow: Shadow::Wide,
            ..Opts::default()
        },
    );
    push_facings(
        &mut coin_hop,
        &[Facing::Right55, Facing::Edge, Facing::Back125, Facing::Back],
        Shadow::Narrow,
    );
    push_facings(
        &mut coin_hop,
        &[Facing::Back125, Facing::Edge, Facing::Left55],
        Shadow::Narrow,
    );
    push(
        &mut coin_hop,
        ARMS_UP,
        1,
        Opts {
            shadow: Shadow::Wide,
            ..Opts::default()
        },
    );
    push_poof_pair(&mut coin_hop);
    push(&mut coin_hop, DEFAULT, 1, Opts::default());

    Anims {
        jump,
        look,
        celebrate,
        skip,
        spin,
        peekaboo,
        drop,
        waddle,
        peek,
        wink,
        boop,
        tap,
        sneeze,
        turn,
        coin_hop,
    }
}

/// One rendered cell: the glyph plus what its colours mean. Only the header
/// maps kinds to styles, so the sprite tables stay colour-free. A body glyph
/// and a facing body glyph share [`CellKind::Body`]; the source's
/// `clawd_background` cut-outs — an open eye span, the row-1 body block and a
/// facing's slice — are [`CellKind::CutOut`], which the header paints on
/// black.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CellKind {
    Empty,
    Body,
    /// Black glyph on the body colour, the reverse of [`CellKind::CutOut`] —
    /// closed lids, winks, and the simplified sprite's eye band and filled
    /// body block.
    Lid,
    /// Body-coloured glyph on the source's `clawd_background` black.
    CutOut,
    /// Poof and shadow marks, the source's `inactive` colour.
    Inactive,
}

pub(crate) type Art = [[(char, CellKind); COLS]; ROWS];

pub(crate) fn rest_art(full_sprite: bool) -> Art {
    render(&REST, full_sprite)
}

/// Draw one frame. `full_sprite` is the startup verdict: `false` paints the
/// simplified sprite instead (see the module doc).
pub(crate) fn render(frame: &Frame, full_sprite: bool) -> Art {
    let mut art = [[(' ', CellKind::Empty); COLS]; ROWS];
    match (frame.pose, full_sprite) {
        (Pose::Face { eyes, arms, feet }, true) => draw_face(&mut art, eyes, arms, feet),
        (Pose::Facing(facing), true) => draw_facing(&mut art, facing),
        (Pose::Face { eyes, .. }, false) => draw_simplified(&mut art, eyes),
        // The simplified sprite carries one eye pair and no facings, so every
        // turned pose paints as an open face, as the source does.
        (Pose::Facing(_), false) => draw_simplified(&mut art, Eyes::Open),
    }
    if frame.offset != 0 || frame.x != 0 {
        art = shift(&art, frame.x, frame.offset);
    }
    draw_effects(&mut art, frame);
    art
}

/// The source's simplified sprite: a body edge either side of a body-filled
/// eye band, a solid body row, then the short feet row. No arms, foot poses
/// or facings exist in this arm.
fn draw_simplified(art: &mut Art, eyes: Eyes) {
    let band = match eyes {
        Eyes::Open => " \u{2597}   \u{2596} ",
        Eyes::Left => " \u{2598}   \u{2598} ",
        Eyes::Right => " \u{259D}   \u{259D} ",
        Eyes::Closed => " \u{2582}   \u{2582} ",
        Eyes::Wink => " \u{2597}   \u{2582} ",
    };
    place(art, 0, 0, "\u{2597}", CellKind::Body);
    place(art, 1, 0, band, CellKind::Lid);
    place(art, 8, 0, "\u{2596}", CellKind::Body);
    for col in 1..8 {
        place_cell(art, col, 1, ' ', CellKind::Lid);
    }
    place(
        art,
        1,
        2,
        "\u{2598}\u{2598}   \u{259D}\u{259D}",
        CellKind::Body,
    );
}

fn draw_face(art: &mut Art, eyes: Eyes, arms: Arms, feet: Feet) {
    let arm = arm_art(arms);
    place(art, 0, 0, arm.r1l, CellKind::Body);
    let mut col = 2;
    for &(glyphs, lid) in eye_spans(eyes) {
        let kind = if lid { CellKind::Lid } else { CellKind::CutOut };
        for glyph in glyphs.chars() {
            place_cell(art, col, 0, glyph, kind);
            col += 1;
        }
    }
    place(art, 8, 0, arm.r1r, CellKind::Body);
    place(art, 0, 1, arm.r2l, CellKind::Body);
    place(
        art,
        2,
        1,
        "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}",
        CellKind::CutOut,
    );
    place(art, 7, 1, arm.r2r, CellKind::Body);
    place(art, 0, 2, feet_art(feet), CellKind::Body);
}

fn draw_facing(art: &mut Art, facing: Facing) {
    let cut = facing_cut(facing);
    for (row, &glyphs) in facing_rows(facing).iter().enumerate() {
        let (from, to) = cut[row];
        for (col, glyph) in glyphs.chars().enumerate() {
            let kind = if (from..to).contains(&col) {
                CellKind::CutOut
            } else {
                CellKind::Body
            };
            place_cell(art, col, row, glyph, kind);
        }
    }
}

/// The `[from, to)` slice of each row a facing cuts out in black
/// (`clawd_background`), as column indices into the 9-wide row. Row 2 (the
/// feet) is never cut. The ranges mirror Claude Code's facing table.
fn facing_cut(facing: Facing) -> [(usize, usize); 3] {
    use Facing::*;
    let (r0, r1) = match facing {
        Right12 | Back | Left12 => ((2, 8), (2, 7)),
        Right30 | Back150 | Left30 => ((2, 7), (2, 7)),
        Right55 | Back125 | Left55 => ((3, 7), (3, 7)),
        Right75 | Back105 | Left75 => ((3, 6), (3, 6)),
        Edge => ((4, 6), (4, 6)),
    };
    [r0, r1, (0, 0)]
}

fn draw_effects(art: &mut Art, frame: &Frame) {
    let poof = match frame.poof {
        Poof::None => None,
        Poof::Dot => Some('\u{00b7}'),
        Poof::Wave => Some('~'),
    };
    if let Some(glyph) = poof {
        art[2][0] = (glyph, CellKind::Inactive);
        art[2][COLS - 1] = (glyph, CellKind::Inactive);
    }
    match frame.shadow {
        Shadow::None => {}
        Shadow::Wide => {
            for (col, glyph) in "\u{2581}\u{2581}\u{2581}".chars().enumerate() {
                art[2][3 + col] = (glyph, CellKind::Inactive);
            }
        }
        Shadow::Narrow => art[2][4] = ('\u{2581}', CellKind::Inactive),
    }
}

fn shift(art: &Art, dx: i8, dy: i8) -> Art {
    let mut shifted = [[(' ', CellKind::Empty); COLS]; ROWS];
    for (row, cells) in art.iter().enumerate() {
        for (col, &(glyph, kind)) in cells.iter().enumerate() {
            if kind == CellKind::Empty {
                continue;
            }
            let x = col as isize + dx as isize;
            let y = row as isize + dy as isize;
            if (0..COLS as isize).contains(&x) && (0..ROWS as isize).contains(&y) {
                shifted[y as usize][x as usize] = (glyph, kind);
            }
        }
    }
    shifted
}

fn place(art: &mut Art, col: usize, row: usize, glyphs: &str, kind: CellKind) {
    for (offset, glyph) in glyphs.chars().enumerate() {
        place_cell(art, col + offset, row, glyph, kind);
    }
}

fn place_cell(art: &mut Art, col: usize, row: usize, glyph: char, kind: CellKind) {
    if let Some(cell) = art.get_mut(row).and_then(|cells| cells.get_mut(col)) {
        *cell = (glyph, kind);
    }
}

struct ArmArt {
    r1l: &'static str,
    r1r: &'static str,
    r2l: &'static str,
    r2r: &'static str,
}

fn arm_art(arms: Arms) -> ArmArt {
    match arms {
        Arms::Down => ArmArt {
            r1l: " \u{2590}",
            r1r: "",
            r2l: "\u{259d}\u{259c}",
            r2r: "\u{2588}\u{2580}",
        },
        Arms::Up => ArmArt {
            r1l: "\u{2597}\u{259f}",
            r1r: "\u{2584}",
            r2l: " \u{259c}",
            r2r: "\u{2588}\u{2598}",
        },
    }
}

fn eye_spans(eyes: Eyes) -> &'static [(&'static str, bool)] {
    const OPEN: &[(&str, bool)] = &[("\u{259b}\u{2588}\u{2588}\u{2588}\u{259b}\u{2588}", false)];
    const LEFT: &[(&str, bool)] = &[("\u{259f}\u{2588}\u{2588}\u{2588}\u{259f}\u{2588}", false)];
    const RIGHT: &[(&str, bool)] = &[("\u{2588}\u{259f}\u{2588}\u{2588}\u{2588}\u{259f}", false)];
    const CLOSED: &[(&str, bool)] = &[
        ("\u{2582}", true),
        ("\u{2588}\u{2588}\u{2588}", false),
        ("\u{2582}", true),
        ("\u{2588}", false),
    ];
    const WINK: &[(&str, bool)] = &[
        ("\u{259b}\u{2588}\u{2588}\u{2588}", false),
        ("\u{2582}", true),
        ("\u{2588}", false),
    ];
    match eyes {
        Eyes::Open => OPEN,
        Eyes::Left => LEFT,
        Eyes::Right => RIGHT,
        Eyes::Closed => CLOSED,
        Eyes::Wink => WINK,
    }
}

fn feet_art(feet: Feet) -> &'static str {
    match feet {
        Feet::Both => " \u{259d}\u{259d}   \u{259d}\u{259d} ",
        Feet::Left => " \u{259d}\u{259d}      ",
        Feet::Right => "      \u{259d}\u{259d} ",
    }
}

fn facing_rows(facing: Facing) -> &'static [&'static str] {
    const RIGHT_12: &[&str] = &[
        " \u{2590}\u{2588}\u{259c}\u{2588}\u{2588}\u{259b}\u{2588} ",
        "\u{259d}\u{259c}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2580}",
        " \u{259d}\u{259d}   \u{259d}\u{259d} ",
    ];
    const RIGHT_30: &[&str] = &[
        "  \u{2588}\u{259b}\u{2588}\u{2588}\u{259b}\u{258c} ",
        " \u{259d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{259b} ",
        "  \u{2598}\u{2598}  \u{2598}\u{2598} ",
    ];
    const RIGHT_55: &[&str] = &[
        "  \u{2590}\u{2588}\u{259b}\u{2588}\u{259c}  ",
        "  \u{2590}\u{2588}\u{2588}\u{2588}\u{2588}  ",
        "  \u{259d}\u{259d} \u{259d}\u{259d}  ",
    ];
    const RIGHT_75: &[&str] = &[
        "   \u{2588}\u{2588}\u{259b}\u{258c}  ",
        "   \u{2588}\u{2588}\u{2588}\u{258c}  ",
        "   \u{2598}  \u{2598}  ",
    ];
    const EDGE: &[&str] = &[
        "   \u{2590}\u{2588}\u{2588}   ",
        "   \u{2590}\u{2588}\u{2588}   ",
        "   \u{259d} \u{259d}   ",
    ];
    const BACK_105: &[&str] = &[
        "   \u{2588}\u{2588}\u{2588}\u{258c}  ",
        "   \u{2588}\u{2588}\u{2588}\u{258c}  ",
        "   \u{2598}  \u{2598}  ",
    ];
    const BACK_125: &[&str] = &[
        "  \u{2590}\u{2588}\u{2588}\u{2588}\u{2588}  ",
        "  \u{2590}\u{2588}\u{2588}\u{2588}\u{2588}  ",
        "  \u{259d}\u{259d} \u{259d}\u{259d}  ",
    ];
    const BACK_150: &[&str] = &[
        "  \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{258c} ",
        " \u{259d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{259b} ",
        "  \u{2598}\u{2598}  \u{2598}\u{2598} ",
    ];
    const BACK: &[&str] = &[
        " \u{2590}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588} ",
        "\u{259d}\u{259c}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2580}",
        " \u{259d}\u{259d}   \u{259d}\u{259d} ",
    ];
    const LEFT_75: &[&str] = &[
        "   \u{259b}\u{2588}\u{2588}\u{258c}  ",
        "   \u{2588}\u{2588}\u{2588}\u{258c}  ",
        "   \u{2598}  \u{2598}  ",
    ];
    const LEFT_55: &[&str] = &[
        "  \u{2590}\u{259c}\u{259b}\u{2588}\u{2588}  ",
        "  \u{2590}\u{2588}\u{2588}\u{2588}\u{2588}  ",
        "  \u{259d}\u{259d} \u{259d}\u{259d}  ",
    ];
    const LEFT_30: &[&str] = &[
        "  \u{259b}\u{2588}\u{2588}\u{259b}\u{2588}\u{258c} ",
        " \u{259d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{259b} ",
        "  \u{2598}\u{2598}  \u{2598}\u{2598} ",
    ];
    const LEFT_12: &[&str] = &[
        " \u{2590}\u{259b}\u{2588}\u{2588}\u{2588}\u{259c}\u{2588} ",
        "\u{259d}\u{259c}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2580}",
        " \u{259d}\u{259d}   \u{259d}\u{259d} ",
    ];
    match facing {
        Facing::Right12 => RIGHT_12,
        Facing::Right30 => RIGHT_30,
        Facing::Right55 => RIGHT_55,
        Facing::Right75 => RIGHT_75,
        Facing::Edge => EDGE,
        Facing::Back105 => BACK_105,
        Facing::Back125 => BACK_125,
        Facing::Back150 => BACK_150,
        Facing::Back => BACK,
        Facing::Left75 => LEFT_75,
        Facing::Left55 => LEFT_55,
        Facing::Left30 => LEFT_30,
        Facing::Left12 => LEFT_12,
    }
}

/// The pools the simplified sprite can animate: Claude Code keeps only the
/// animations that read as motion without a facing or a rest-pose opening.
static SUBTLE_SIMPLIFIED: LazyLock<Vec<Anim>> = LazyLock::new(|| simplified_pool(&SUBTLE));
static BIG_SIMPLIFIED: LazyLock<Vec<Anim>> = LazyLock::new(|| simplified_pool(&BIG));

fn simplified_pool(pool: &[Anim]) -> Vec<Anim> {
    pool.iter()
        .copied()
        .filter(|&anim| reads_as_motion(anim))
        .collect()
}

/// Claude Code's filter for the simplified sprite: it cannot show a facing,
/// and an animation whose first frame has open eyes and no offset reads as the
/// rest pose.
fn reads_as_motion(anim: Anim) -> bool {
    let frames = anim.frames();
    let Some(first) = frames.first() else {
        return false;
    };
    if frames
        .iter()
        .any(|frame| matches!(frame.pose, Pose::Facing(_)))
    {
        return false;
    }
    match first.pose {
        Pose::Face { eyes, .. } => eyes != Eyes::Open || first.offset != 0 || first.x != 0,
        Pose::Facing(_) => false,
    }
}

fn subtle_pool(full_sprite: bool) -> &'static [Anim] {
    if full_sprite {
        &SUBTLE
    } else {
        SUBTLE_SIMPLIFIED.as_slice()
    }
}

fn big_pool(full_sprite: bool) -> &'static [Anim] {
    if full_sprite {
        &BIG
    } else {
        BIG_SIMPLIFIED.as_slice()
    }
}

#[derive(Clone, Copy)]
struct Playing {
    anim: Anim,
    start_ms: u64,
}

/// The header mascot's playback state: which animation runs, and when the
/// next idle or big moment is due. The render path reads [`Clawd::frame`] and
/// never mutates it; [`Clawd::tick`] moves the schedule.
pub(crate) struct Clawd {
    playing: Option<Playing>,
    next_subtle_ms: u64,
    next_big_ms: u64,
    seed: u64,
    /// The startup verdict on `TERM_PROGRAM`. False paints the simplified arm
    /// and restricts the animation pools to what still reads there.
    pub(crate) full_sprite: bool,
}

impl Clawd {
    pub(crate) fn new() -> Self {
        Self {
            playing: None,
            next_subtle_ms: SUBTLE_PERIOD_MS,
            // Zero, so the first tick (always after the first paint) opens the
            // launch animation; startup work before it cannot clip it.
            next_big_ms: 0,
            seed: random_seed(),
            full_sprite: true,
        }
    }

    pub(crate) fn tick(&mut self, now_ms: u64) {
        if let Some(playing) = self.playing {
            let elapsed = now_ms.saturating_sub(playing.start_ms);
            if elapsed < frames_ms(playing.anim) {
                return;
            }
            self.playing = None;
        }
        if now_ms >= self.next_big_ms {
            let pool = big_pool(self.full_sprite);
            let anim = pool[self.pick(pool.len())];
            self.playing = Some(Playing {
                anim,
                start_ms: now_ms,
            });
            self.next_big_ms = now_ms.saturating_add(BIG_PERIOD_MS);
            // A big moment restarts the subtle rhythm, so the next small one
            // lands a full period after it rather than in its shadow.
            self.next_subtle_ms = now_ms.saturating_add(SUBTLE_PERIOD_MS);
        } else if now_ms >= self.next_subtle_ms {
            let pool = subtle_pool(self.full_sprite);
            let anim = pool[self.pick(pool.len())];
            self.playing = Some(Playing {
                anim,
                start_ms: now_ms,
            });
            self.next_subtle_ms = now_ms.saturating_add(SUBTLE_PERIOD_MS);
        }
    }

    pub(crate) fn frame(&self, now_ms: u64) -> Option<&'static Frame> {
        let playing = self.playing?;
        let index = now_ms.saturating_sub(playing.start_ms) / FRAME_MS;
        playing.anim.frames().get(usize::try_from(index).ok()?)
    }

    /// Render tests pin one animation instead of sampling the random launch
    /// choice. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) fn play(&mut self, anim: Anim, start_ms: u64) {
        self.playing = Some(Playing { anim, start_ms });
    }

    fn pick(&mut self, len: usize) -> usize {
        (self.next_random() % len as u64) as usize
    }

    fn next_random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
}

fn frames_ms(anim: Anim) -> u64 {
    anim.frames().len() as u64 * FRAME_MS
}

#[cfg(not(test))]
fn random_seed() -> u64 {
    let mut bytes = [0u8; 8];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes) | 1,
        // Cosmetic choice only: a fixed pick is a fine fallback for a failed
        // entropy read, and the seed must stay non-zero for the xorshift.
        Err(_) => 1,
    }
}

/// Render tests sample specific animations; a fixed seed keeps every run on
/// the same choice.
#[cfg(test)]
fn random_seed() -> u64 {
    0x9E37_79B9_7F4A_7C15
}

#[cfg(test)]
#[path = "../../tests/inline/tui_clawd.rs"]
mod tests;
