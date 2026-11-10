use super::*;

/// Glyph rows of one arm. A claim about a cell's style reads `render`
/// directly, so the `CellKind` survives to the assertion.
fn rows(frame: &Frame, full_sprite: bool) -> Vec<String> {
    render(frame, full_sprite)
        .iter()
        .map(|row| row.iter().map(|(glyph, _)| *glyph).collect())
        .collect()
}

#[test]
fn rest_pose_matches_the_live_claude_code_capture() {
    assert_eq!(
        rows(&REST, true),
        [" ▐▛███▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
}

#[test]
fn animation_frame_counts_match_the_extracted_sequences() {
    let expected = [
        (Anim::Jump, 12),
        (Anim::Look, 11),
        (Anim::Celebrate, 15),
        (Anim::Skip, 14),
        (Anim::Spin, 10),
        (Anim::Peekaboo, 21),
        (Anim::Drop, 13),
        (Anim::Waddle, 19),
        (Anim::Peek, 20),
        (Anim::Wink, 6),
        (Anim::Boop, 11),
        (Anim::Tap, 14),
        (Anim::Sneeze, 12),
        (Anim::Turn, 19),
        (Anim::CoinHop, 14),
    ];
    for (anim, count) in expected {
        assert_eq!(anim.frames().len(), count, "{}", anim.name());
    }
}

#[test]
fn the_five_in_place_animations_are_the_idle_set() {
    assert_eq!(
        SUBTLE.map(Anim::name),
        ["look", "wink", "boop", "tap", "sneeze"]
    );
    assert_eq!(
        BIG.map(Anim::name),
        [
            "jump",
            "celebrate",
            "skip",
            "spin",
            "peekaboo",
            "drop",
            "waddle",
            "peek",
            "turn",
            "coin-hop",
        ]
    );
}

/// The startup gate: `Apple_Terminal` alone selects the simplified arm, and an
/// unset `TERM_PROGRAM` selects the full one.
#[test]
fn the_simplified_arm_is_apple_terminal_alone() {
    assert!(!full_sprite(Some("Apple_Terminal")));
    assert!(full_sprite(Some("tmux")));
    assert!(full_sprite(Some("iTerm.app")));
    assert!(full_sprite(None));
}

/// Every animation sits in the pool its kind names. The match is
/// wildcard-free, so a new `Anim` variant fails to compile here until it is
/// given a pool; the literal list beside it must then name it too, or the
/// variant is never checked.
#[test]
fn every_animation_sits_in_the_pool_its_kind_names() {
    fn pool_of(anim: Anim) -> &'static [Anim] {
        match anim {
            Anim::Look | Anim::Wink | Anim::Boop | Anim::Tap | Anim::Sneeze => SUBTLE.as_slice(),
            Anim::Jump
            | Anim::Celebrate
            | Anim::Skip
            | Anim::Spin
            | Anim::Peekaboo
            | Anim::Drop
            | Anim::Waddle
            | Anim::Peek
            | Anim::Turn
            | Anim::CoinHop => BIG.as_slice(),
        }
    }
    let every = [
        Anim::Look,
        Anim::Wink,
        Anim::Boop,
        Anim::Tap,
        Anim::Sneeze,
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
    for anim in every {
        assert!(
            pool_of(anim).contains(&anim),
            "{} sits in the pool its kind names",
            anim.name()
        );
    }
}

#[test]
fn launch_opens_with_a_big_animation_on_the_first_tick() {
    let mut clawd = Clawd::new();
    assert!(
        clawd.playing.is_none(),
        "construction must not start the launch animation"
    );
    assert!(clawd.frame(0).is_none(), "rest draws before the first tick");
    clawd.tick(0);
    let playing = match clawd.playing {
        Some(playing) => playing,
        None => panic!("no launch animation"),
    };
    assert!(BIG.contains(&playing.anim));
    assert_eq!(playing.start_ms, 0);
    assert!(clawd.frame(0).is_some());

    // A startup slow enough to miss the animation's whole length still gets
    // it: the clock starts at the first tick, not at construction.
    let mut slow = Clawd::new();
    slow.tick(5_000);
    let playing = slow.playing.expect("no launch animation on a slow start");
    assert!(BIG.contains(&playing.anim));
    assert_eq!(playing.start_ms, 5_000);
}

#[test]
fn each_ten_second_slot_opens_a_subtle_animation() {
    let mut clawd = Clawd::new();
    clawd.tick(0);
    clawd.tick(SUBTLE_PERIOD_MS);
    let playing = match clawd.playing {
        Some(playing) => playing,
        None => panic!("no animation at the first idle slot"),
    };
    assert!(SUBTLE.contains(&playing.anim));
    assert_eq!(playing.start_ms, SUBTLE_PERIOD_MS);
}

#[test]
fn each_thirty_minute_slot_opens_a_big_animation() {
    let mut clawd = Clawd::new();
    clawd.tick(0);
    clawd.tick(BIG_PERIOD_MS);
    let playing = match clawd.playing {
        Some(playing) => playing,
        None => panic!("no animation at the first big slot"),
    };
    assert!(BIG.contains(&playing.anim));
    assert_eq!(playing.start_ms, BIG_PERIOD_MS);
}

/// A big moment restarts the subtle rhythm: the next idle slot lands a full
/// period after it, not at the one it swallowed.
#[test]
fn a_big_animation_restarts_the_subtle_clock() {
    let mut clawd = Clawd::new();
    clawd.next_big_ms = BIG_PERIOD_MS;
    clawd.next_subtle_ms = BIG_PERIOD_MS;
    clawd.tick(BIG_PERIOD_MS);
    let big = clawd.playing.expect("no big animation at its slot");
    assert!(BIG.contains(&big.anim));
    assert_eq!(
        clawd.next_subtle_ms,
        BIG_PERIOD_MS + SUBTLE_PERIOD_MS,
        "a big moment pushes the next idle slot out by a full period"
    );

    clawd.tick(BIG_PERIOD_MS + frames_ms(big.anim));
    assert!(clawd.playing.is_none(), "the swallowed idle slot fired");
    assert_eq!(clawd.next_subtle_ms, BIG_PERIOD_MS + SUBTLE_PERIOD_MS);
}

#[test]
fn a_running_animation_is_never_cut_short() {
    let mut clawd = Clawd::new();
    clawd.tick(0);
    clawd.tick(SUBTLE_PERIOD_MS);
    let before = clawd
        .playing
        .map(|playing| (playing.anim, playing.start_ms));
    clawd.tick(SUBTLE_PERIOD_MS + FRAME_MS);
    assert_eq!(
        clawd
            .playing
            .map(|playing| (playing.anim, playing.start_ms)),
        before
    );
}

#[test]
fn frames_advance_once_per_tui_tick() {
    let frames = Anim::Wink.frames();
    let mut clawd = Clawd::new();
    clawd.playing = Some(Playing {
        anim: Anim::Wink,
        start_ms: 0,
    });
    assert_eq!(clawd.frame(0), Some(&frames[0]));
    assert_eq!(clawd.frame(FRAME_MS - 1), Some(&frames[0]));
    assert_eq!(clawd.frame(FRAME_MS), Some(&frames[1]));
    assert_eq!(clawd.frame(FRAME_MS * frames.len() as u64), None);
}

#[test]
fn the_eye_facing_shift_and_effect_art_matches_the_extracted_frames() {
    assert_eq!(
        rows(&Anim::Wink.frames()[0], true),
        [" ▐▛███▂█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Turn.frames()[0], true),
        [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Drop.frames()[0], true),
        ["         ", "         ", "         "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::CoinHop.frames()[2], true),
        ["▗▟▛███▛█▄", " ▜██████▘", " ▝▝▁▁▁▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Waddle.frames()[1], true),
        ["█▟       ", "██▀      ", "         "].map(String::from)
    );
}

#[test]
fn every_animation_opens_on_its_extracted_first_frame() {
    let expected = [
        (Anim::Jump, ["         ", " ▐▛███▛█ ", "·▜██████·"]),
        (Anim::Look, [" ▐█▟███▟ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Celebrate, ["         ", " ▐▛███▛█ ", "·▜██████·"]),
        (Anim::Skip, ["         ", "         ", "         "]),
        (Anim::Spin, [" ▐▟███▟█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Peekaboo, ["         ", "         ", "         "]),
        (Anim::Drop, ["         ", "         ", "         "]),
        (Anim::Waddle, ["         ", "         ", "         "]),
        (Anim::Peek, ["         ", "         ", "         "]),
        (Anim::Wink, [" ▐▛███▂█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Boop, ["         ", " ▐▂███▂█ ", "·▜██████·"]),
        (Anim::Tap, [" ▐▛███▛█ ", "▝▜██████▀", " ▝▝      "]),
        (Anim::Sneeze, ["▗▟▂███▂█▄", " ▜██████▘", " ▝▝   ▝▝ "]),
        (Anim::Turn, [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::CoinHop, ["         ", " ▐▛███▛█ ", "▝▜██████▀"]),
    ];
    for (anim, art) in expected {
        assert_eq!(
            rows(&anim.frames()[0], true),
            art.map(String::from),
            "{}",
            { anim.name() }
        );
    }
}

#[test]
fn every_facing_rotation_matches_its_extracted_art() {
    let expected = [
        (Facing::Right12, [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Facing::Right30, ["  █▛██▛▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Right55, ["  ▐█▛█▜  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Right75, ["   ██▛▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Edge, ["   ▐██   ", "   ▐██   ", "   ▝ ▝   "]),
        (Facing::Back105, ["   ███▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Back125, ["  ▐████  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Back150, ["  █████▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Back, [" ▐██████ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Facing::Left75, ["   ▛██▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Left55, ["  ▐▜▛██  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Left30, ["  ▛██▛█▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Left12, [" ▐▛███▜█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
    ];
    for (facing, art) in expected {
        let frame = Frame {
            pose: Pose::Facing(facing),
            ..REST
        };
        assert_eq!(rows(&frame, true), art.map(String::from), "{facing:?}");
    }
}

#[test]
fn a_finished_animation_clears_at_its_last_frame_boundary() {
    let frames = Anim::Wink.frames();
    let end = FRAME_MS * frames.len() as u64;
    let mut clawd = Clawd::new();
    clawd.tick(0);
    clawd.playing = Some(Playing {
        anim: Anim::Wink,
        start_ms: 0,
    });
    clawd.next_subtle_ms = end;
    clawd.tick(end);
    let playing = match clawd.playing {
        Some(playing) => playing,
        None => panic!("the finished slot never opened the due subtle animation"),
    };
    assert_eq!(playing.start_ms, end);
    assert!(SUBTLE.contains(&playing.anim));
}

/// Claude Code's simplified arm (`bt` in the source): a body edge either side
/// of a body-filled eye band, then the short feet row.
#[test]
fn the_simplified_sprite_paints_the_extracted_eye_band() {
    let want = [
        (Eyes::Open, "\u{2597} \u{2597}   \u{2596} \u{2596}"),
        (Eyes::Left, "\u{2597} \u{2598}   \u{2598} \u{2596}"),
        (Eyes::Right, "\u{2597} \u{259D}   \u{259D} \u{2596}"),
        (Eyes::Closed, "\u{2597} \u{2582}   \u{2582} \u{2596}"),
        (Eyes::Wink, "\u{2597} \u{2597}   \u{2582} \u{2596}"),
    ];
    for (eyes, row0) in want {
        let cell = frame(face(eyes, Arms::Down, Feet::Both), Opts::default());
        assert_eq!(
            rows(&cell, false),
            [row0, "         ", " \u{2598}\u{2598}   \u{259D}\u{259D} "].map(String::from),
            "{eyes:?}"
        );
    }
}

/// The band is the only body-background run in the simplified arm; the body
/// edges and the feet row are plain body glyphs.
#[test]
fn the_simplified_sprite_kinds_are_body_edges_over_a_lid_band() {
    use CellKind::{Body, Empty, Lid};
    let cell = frame(face(Eyes::Open, Arms::Down, Feet::Both), Opts::default());
    let expected = [
        [
            ('\u{2597}', Body),
            (' ', Lid),
            ('\u{2597}', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            ('\u{2596}', Lid),
            (' ', Lid),
            ('\u{2596}', Body),
        ],
        [
            (' ', Empty),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Empty),
        ],
        [
            (' ', Empty),
            ('\u{2598}', Body),
            ('\u{2598}', Body),
            (' ', Body),
            (' ', Body),
            (' ', Body),
            ('\u{259D}', Body),
            ('\u{259D}', Body),
            (' ', Empty),
        ],
    ];
    assert_eq!(render(&cell, false), expected);
}

/// Every turned pose paints as an open face in the simplified arm, because
/// the simplified sprite has no facings.
#[test]
fn the_simplified_sprite_paints_every_facing_as_an_open_face() {
    let open = ["▗ ▗   ▖ ▖", "         ", " ▘▘   ▝▝ "].map(String::from);
    const ALL: [Facing; 13] = [
        Facing::Right12,
        Facing::Right30,
        Facing::Right55,
        Facing::Right75,
        Facing::Edge,
        Facing::Back105,
        Facing::Back125,
        Facing::Back150,
        Facing::Back,
        Facing::Left75,
        Facing::Left55,
        Facing::Left30,
        Facing::Left12,
    ];
    for facing in ALL {
        let turned = Frame {
            pose: Pose::Facing(facing),
            ..REST
        };
        assert_eq!(rows(&turned, false), open, "{facing:?}");
    }
}

/// Claude Code filters its pools for the simplified sprite: no facing frame
/// anywhere, and the first frame must not be the open-eyed rest pose.
#[test]
fn the_simplified_pools_drop_facings_and_rest_pose_openings() {
    assert_eq!(
        subtle_pool(false),
        [Anim::Look, Anim::Wink, Anim::Boop, Anim::Sneeze].as_slice()
    );
    assert_eq!(
        big_pool(false),
        [
            Anim::Jump,
            Anim::Celebrate,
            Anim::Skip,
            Anim::Spin,
            Anim::Peekaboo,
            Anim::Drop,
            Anim::Waddle,
            Anim::Peek,
        ]
        .as_slice()
    );
}

/// Every facing cuts out exactly the column span Claude Code's facing table
/// names (2.1.310 binary, `eo`, copied by hand as `[from, to)` per row): the
/// cut-out cells of each row are that span and nothing else, and the feet row
/// is never cut.
#[test]
fn every_facing_cuts_the_span_the_source_table_names() {
    let table = [
        (Facing::Right12, (2, 8), (2, 7)),
        (Facing::Right30, (2, 7), (2, 7)),
        (Facing::Right55, (3, 7), (3, 7)),
        (Facing::Right75, (3, 6), (3, 6)),
        (Facing::Edge, (4, 6), (4, 6)),
        (Facing::Back105, (3, 6), (3, 6)),
        (Facing::Back125, (3, 7), (3, 7)),
        (Facing::Back150, (2, 7), (2, 7)),
        (Facing::Back, (2, 8), (2, 7)),
        (Facing::Left75, (3, 6), (3, 6)),
        (Facing::Left55, (3, 7), (3, 7)),
        (Facing::Left30, (2, 7), (2, 7)),
        (Facing::Left12, (2, 8), (2, 7)),
    ];
    for (facing, r0, r1) in table {
        let art = render(&frame(Pose::Facing(facing), Opts::default()), true);
        for (row, (from, to)) in [(0, r0), (1, r1), (2, (0, 0))] {
            let cut: Vec<usize> = (0..COLS)
                .filter(|&col| art[row][col].1 == CellKind::CutOut)
                .collect();
            assert_eq!(
                cut,
                (from..to).collect::<Vec<usize>>(),
                "{facing:?} row {row}"
            );
        }
    }
}
