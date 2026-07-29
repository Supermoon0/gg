//! Style-system performance harness.
//!
//! Answers one question: in `compute_styles`, does time go to *selector
//! matching* or to the *cascade* (the per-node HashMap<String,String>
//! churn)? The answer decides what is worth optimizing — compiling the
//! stylesheet into a matcher only pays if matching is the bottleneck.
//!
//! Run with:
//!   cargo test --release --no-default-features style_pipeline -- --nocapture

#[cfg(test)]
mod bench {
    use std::time::{Duration, Instant};

    use crate::dom::Document;
    use crate::style::{
        clear_stylesheet_cache, compute_styles, fill_ancestor_bloom,
        parse_sheets, RuleIndex,
    };

    const N_RULES: usize = 5000;
    const N_CLASSES: usize = 400;
    const N_ELEMENTS: usize = 2000;
    const TAGS: &[&str] = &[
        "div", "span", "p", "a", "li", "ul", "section", "h2", "button",
        "img",
    ];

    /// Deterministic PRNG — the workload must be byte-identical across
    /// runs or the numbers cannot be compared between commits.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    /// A stylesheet shaped like a real framework build: mostly utility
    /// classes, with a realistic tail of descendant/child/attr/pseudo
    /// selectors that force the matcher to walk the tree.
    fn gen_stylesheet() -> String {
        let mut s = String::with_capacity(N_RULES * 48);
        for i in 0..N_RULES {
            let c = i % N_CLASSES;
            match i % 20 {
                // 60% single-class utilities (Tailwind-shaped)
                0..=11 => s.push_str(&format!(
                    ".c{c}{{margin-top:{}px}}\n",
                    i % 40
                )),
                // 15% descendant — the expensive shape
                12..=14 => s.push_str(&format!(
                    ".c{} .c{c}{{color:#{:06x}}}\n",
                    (i * 7) % N_CLASSES,
                    (i * 977) % 0xff_ffff
                )),
                // 10% tag-qualified
                15..=16 => s.push_str(&format!(
                    "{}.c{c}{{padding-left:{}px}}\n",
                    TAGS[i % TAGS.len()],
                    i % 24
                )),
                17 => s.push_str(&format!(".c{c}:hover{{color:red}}\n")),
                18 => s.push_str(&format!(
                    "[data-k=\"{}\"]{{font-weight:bold}}\n",
                    i % 50
                )),
                _ => s.push_str(&format!(
                    ".c{} > .c{c}{{display:block}}\n",
                    (i * 13) % N_CLASSES
                )),
            }
        }
        s
    }

    /// A content-heavy page: ~2000 elements, 3-8 classes each drawn from
    /// the same pool the stylesheet targets (so rules actually match).
    fn gen_dom() -> Document {
        let mut doc = Document::with_capacity(N_ELEMENTS + 16);
        let mut rng = Lcg(0x2545_F491_4F6C_DD1D);
        let root = doc.new_element("html".into(), Vec::new(), None);
        doc.root = root;

        let mut level = vec![root];
        let mut created = 1usize;
        while created < N_ELEMENTS && !level.is_empty() {
            let mut next = Vec::new();
            for &p in &level {
                let kids = 2 + rng.below(2);
                for _ in 0..kids {
                    if created >= N_ELEMENTS {
                        break;
                    }
                    let tag = TAGS[rng.below(TAGS.len())];
                    let ncls = 3 + rng.below(6);
                    let mut cls = String::new();
                    for k in 0..ncls {
                        if k > 0 {
                            cls.push(' ');
                        }
                        cls.push_str(&format!("c{}", rng.below(N_CLASSES)));
                    }
                    let mut attrs = vec![("class".to_string(), cls)];
                    if rng.below(4) == 0 {
                        attrs.push((
                            "data-k".to_string(),
                            rng.below(50).to_string(),
                        ));
                    }
                    next.push(doc.new_element(
                        tag.to_string(),
                        attrs,
                        Some(p),
                    ));
                    created += 1;
                }
            }
            level = next;
        }
        doc
    }

    fn max_depth(doc: &Document) -> usize {
        let mut best = 0;
        let mut stack = vec![(doc.root, 1usize)];
        while let Some((i, d)) = stack.pop() {
            best = best.max(d);
            for &c in &doc.nodes[i].children {
                stack.push((c, d + 1));
            }
        }
        best
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    fn pct(part: f64, whole: f64) -> f64 {
        if whole <= 0.0 {
            0.0
        } else {
            part / whole * 100.0
        }
    }

    const REPS: usize = 7;

    /// One warmup pass, then the fastest of REPS runs. Minimum (not
    /// mean) is the right estimator here: every source of noise on this
    /// machine — scheduling, thermal throttling after a long LTO build,
    /// background work — only ever makes a run slower.
    fn best_of(mut f: impl FnMut()) -> Duration {
        f();
        let mut best = Duration::MAX;
        for _ in 0..REPS {
            let t = Instant::now();
            f();
            best = best.min(t.elapsed());
        }
        best
    }

    #[test]
    fn style_pipeline_breakdown() {
        let css = gen_stylesheet();
        let mut doc = gen_dom();
        let elements = (0..doc.nodes.len())
            .filter(|&i| doc.nodes[i].is_element())
            .count();
        let depth = max_depth(&doc);

        let sources = [String::new(), css.clone()];

        // 1. parse
        let t_parse = best_of(|| {
            std::hint::black_box(parse_sheets(&sources, 1280.0));
        });
        let (rules, _pseudo) = parse_sheets(&sources, 1280.0);

        // 2. build the rule index
        let t_index = best_of(|| {
            std::hint::black_box(RuleIndex::new(&rules));
        });
        let index = RuleIndex::new(&rules);

        // 3. building the ancestor bloom filter
        let t_bloom = best_of(|| fill_ancestor_bloom(&mut doc));

        // 4. matching the way the style pass calls it: one pair of
        //    buffers reused for the whole tree, ancestor filter live.
        let mut tested = 0usize;
        let mut hits = 0usize;
        let t_match = best_of(|| {
            let mut cand = Vec::new();
            let mut matched = Vec::new();
            let (mut n, mut h) = (0usize, 0usize);
            for i in 0..doc.nodes.len() {
                if doc.nodes[i].is_element() {
                    index.matching_into(&doc, i, &mut cand, &mut matched);
                    n += cand.len();
                    h += matched.len();
                }
            }
            tested = n;
            hits = h;
        });

        // 5. the same matching with the ancestor filter retired. A
        //    version mismatch is exactly how a stale filter is ignored,
        //    so this is the real before/after — measured in the same
        //    run, immune to whatever else the machine is doing.
        let saved = doc.ancestor_bloom_version;
        doc.ancestor_bloom_version = u64::MAX;
        let t_match_nobloom = best_of(|| {
            let mut cand = Vec::new();
            let mut matched = Vec::new();
            for i in 0..doc.nodes.len() {
                if doc.nodes[i].is_element() {
                    index.matching_into(&doc, i, &mut cand, &mut matched);
                }
            }
        });
        doc.ancestor_bloom_version = saved;

        // 6. matching with a fresh pair of Vecs per element — what it
        //    cost before the buffers were hoisted
        let t_match_alloc = best_of(|| {
            for i in 0..doc.nodes.len() {
                if doc.nodes[i].is_element() {
                    std::hint::black_box(index.matching(&doc, i));
                }
            }
        });

        // 5a. cold: first paint — parse the sheet, then style
        let t_cold = best_of(|| {
            clear_stylesheet_cache();
            compute_styles(&mut doc, &sources);
        });

        // 5b. warm: a restyle (hover, class toggle, animation frame) —
        //     the stylesheet is already parsed. This is the number that
        //     has to fit in a frame.
        let t_warm = best_of(|| compute_styles(&mut doc, &sources));

        let warm = ms(t_warm);
        let matching = ms(t_match);
        let cascade =
            (warm - ms(t_index) - ms(t_bloom) - matching).max(0.0);

        println!("\n=== ggcore style pipeline (best of {REPS}) ===");
        println!(
            "stylesheet   {} rules, {:.0} KB",
            rules.len(),
            css.len() as f64 / 1024.0
        );
        println!("DOM          {elements} elements, depth {depth}");
        println!("selectors    {tested} tested -> {hits} matched");
        println!("---");
        println!("{:<28}{:8.2} ms", "COLD (parse + style)", ms(t_cold));
        println!("{:<28}{warm:8.2} ms   <- restyle", "WARM (cached sheet)");
        println!("---");
        println!("  {:<26}{:8.2} ms", "CSS parse (cold only)", ms(t_parse));
        println!("  {:<26}{:8.2} ms", "rule index build", ms(t_index));
        println!("  {:<26}{:8.2} ms", "ancestor bloom build", ms(t_bloom));
        println!(
            "  {:<26}{matching:8.2} ms  {:5.1}% of warm",
            "selector matching",
            pct(matching, warm)
        );
        println!(
            "  {:<26}{cascade:8.2} ms  {:5.1}% of warm",
            "cascade + tree walk",
            pct(cascade, warm)
        );
        println!("--- same-run A/B on the matching phase ---");
        println!(
            "  {:<26}{:8.2} ms   ({:+.1}% vs filtered)",
            "ancestor filter OFF",
            ms(t_match_nobloom),
            (ms(t_match_nobloom) / matching - 1.0) * 100.0
        );
        println!(
            "  {:<26}{:8.2} ms   ({:+.1}% vs buffered)",
            "fresh Vecs per element",
            ms(t_match_alloc),
            (ms(t_match_alloc) / matching - 1.0) * 100.0
        );
        println!("---");
        println!(
            "{:<28}{:8.2} M selector-tests/sec\n",
            "throughput",
            tested as f64 / matching / 1000.0
        );
    }
}
