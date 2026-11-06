use clojurecraft_edn::{Class, NodeKind, Severity, highlight, line_col, parse, parse_value, tokenize};

fn errors(src: &str) -> Vec<(String, (usize, usize))> {
    parse_value(src)
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| (d.message, line_col(src, d.span.start)))
        .collect()
}

fn single(src: &str) -> NodeKind {
    let p = parse_value(src);
    assert!(p.diagnostics.is_empty(), "{src}: {:?}", p.diagnostics);
    assert_eq!(p.values.len(), 1, "{src}");
    p.values[0].kind.clone()
}

#[test]
fn reads_the_edn_value_space() {
    let src = r#"{:id 1, :ratio 3/4, :big 12N, :dec 1.5M, :exp -2e10, :hex 0xFF, :inf ##Inf
      :s "tab\there \u00e9 \"q\"", :c [\a \newline \u0041], :k :user/name, :sym foo.bar/baz
      :nil nil, :t true, :list (1 2), :set #{1 2}, :when #inst "2024-05-01T12:00:00.5+02:00"
      :uuid #uuid "3b2a9f30-6f0e-4bd8-9e64-0c6f1c1b4a52" :user #:user{:id 7 :_/plain 1}
      :skip #_ ignored 1 :kept 2}"#;
    let NodeKind::Map(entries) = single(src) else {
        panic!("expected a map")
    };
    assert_eq!(entries.len(), 20);
    let value_of = |key: &str| {
        &entries
            .iter()
            .find(|(k, _)| k.text(src) == key)
            .unwrap_or_else(|| panic!("{key}"))
            .1
    };
    assert_eq!(value_of(":ratio").kind, NodeKind::Ratio);
    assert_eq!(value_of(":dec").kind, NodeKind::Float);
    assert_eq!(value_of(":hex").kind, NodeKind::Integer);
    assert_eq!(value_of(":s").kind, NodeKind::String("tab\there é \"q\"".into()));
    assert!(matches!(&value_of(":c").kind, NodeKind::Vector(v) if v.len() == 3));
    assert!(matches!(&value_of(":when").kind, NodeKind::Tagged { tag, .. } if tag == "inst"));
    assert_eq!(value_of(":kept").text(src), "2");
}

#[test]
fn delimiter_errors_point_at_the_culprit() {
    assert_eq!(
        errors("{:a [1 2}"),
        [("expected `]` to close `[` but found `}`".into(), (1, 9))]
    );
    assert_eq!(
        errors("[1 2\n 3"),
        [("`[` is never closed: add a matching `]`".into(), (1, 1))]
    );
    assert_eq!(
        errors("[1 2]]"),
        [("unexpected `]` with no matching opening bracket".into(), (1, 6))]
    );
}

#[test]
fn recovery_keeps_reading_after_a_mismatch() {
    // The inner vector is closed by the map's brace; the map itself still parses.
    let p = parse_value("{:a [1 2} ");
    assert_eq!(p.values.len(), 1);
    let NodeKind::Map(entries) = &p.values[0].kind else {
        panic!()
    };
    assert_eq!(entries.len(), 1);
    assert!(matches!(&entries[0].1.kind, NodeKind::Vector(v) if v.len() == 2));
}

#[test]
fn map_and_set_semantics_follow_clojure_equality() {
    let e = errors("{:a 1 :b 2 :a 3}");
    assert_eq!(e.len(), 1);
    assert!(
        e[0].0
            .starts_with("duplicate map key `:a` (also at line 1, column 2)")
    );
    assert_eq!(e[0].1, (1, 12));
    // 1 and 1N are equal; [1] and (1) are equal; 1 and 1.0 are not.
    assert_eq!(errors("#{1 1N}").len(), 1);
    assert_eq!(errors("{[1] :x (1) :y}").len(), 1);
    assert!(errors("#{1 1.0}").is_empty());
    // A namespaced map qualifies its plain keyword keys.
    assert_eq!(errors("#:u{:id 1 :u/id 2}").len(), 1);
    let e = errors("{:a 1 :b}");
    assert_eq!(
        e[0].0,
        "map key `:b` has no value (a map needs an even number of forms)"
    );
}

#[test]
fn clojure_code_syntax_is_rejected_once_per_occurrence() {
    for (src, needle) in [
        ("'(1 2)", "quote"),
        ("#(inc %)", "anonymous function"),
        ("@x", "deref"),
        ("^:m {}", "metadata"),
        ("#\"a+\"", "regular expressions"),
        ("::local", "auto-resolved"),
        ("#=(+ 1 2)", "read-eval"),
    ] {
        let e = errors(src);
        assert_eq!(e.len(), 1, "{src}: {e:?}");
        assert!(e[0].0.contains(needle), "{src}: {}", e[0].0);
    }
}

#[test]
fn scalar_errors_are_specific() {
    for (src, needle) in [
        ("12abc", "not a valid number"),
        ("08", "not a valid number"),
        ("\"abc", "never closed"),
        ("\"a\\qb\"", "not a valid escape"),
        ("\\foo", "not a valid character"),
        ("#inst \"yesterday\"", "RFC 3339"),
        ("#uuid \"nope\"", "#uuid"),
        ("#money 12", "unknown tag `#money`"),
        ("a//b", "single `/`"),
        (":", "keyword name"),
        ("#_", "followed by the value"),
        ("[#_]", "followed by the value"),
    ] {
        let e = errors(src);
        assert!(e.iter().any(|(m, _)| m.contains(needle)), "{src}: {e:?}");
    }
}

#[test]
fn a_value_field_holds_exactly_one_value() {
    assert!(parse_value("").values.is_empty());
    assert!(parse_value("  ; just a comment\n").diagnostics.is_empty());
    assert!(
        errors("1 2 3")[0]
            .0
            .starts_with("expected a single value but found 3")
    );
    // Several top-level forms are fine where a sequence of forms is expected.
    assert!(parse("1 2 3").diagnostics.is_empty());
}

#[test]
fn positions_count_characters_not_bytes() {
    let src = "{:name \"Zoë\"\n :x ]}";
    let e = errors(src);
    assert_eq!(e[0].1, (2, 5));
}

#[test]
fn deep_nesting_is_reported_not_overflowed() {
    let src = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let p = parse_value(&src);
    assert!(p.has_errors());
    assert!(p.diagnostics[0].message.contains("nested more than"));
}

#[test]
fn large_nested_data_parses_quickly_without_diagnostics() {
    let mut src = String::from("[");
    for i in 0..20_000 {
        src.push_str(&format!(
            "{{:id {i} :name \"customer-{i}\" :tags #{{:a :b}} :orders [{{:sku \"x\" :qty {i}}}]}}\n"
        ));
    }
    src.push(']');
    let started = std::time::Instant::now();
    let p = parse_value(&src);
    let elapsed = started.elapsed();
    assert!(p.diagnostics.is_empty());
    assert_eq!(p.values[0].child_count(), Some(20_000));
    // Generous bound that still catches accidental quadratic behaviour in debug builds.
    assert!(elapsed.as_secs() < 5, "parsing took {elapsed:?}");
}

#[test]
fn diagnostics_are_bounded() {
    let src = "]".repeat(10_000);
    let p = parse_value(&src);
    assert_eq!(p.diagnostics.len(), clojurecraft_edn::parse::MAX_DIAGNOSTICS);
    assert_eq!(p.error_count(), 10_000);
}

#[test]
fn lexing_covers_any_input_and_parsing_never_panics() {
    let samples = [
        r#"{:a [1 2.5M 3/4 \u00e9 "s\"x" #{:k} #inst "2024" #_x (f 'y @z ^m #(inc %))]}"#,
        "#:ns{:a 1} ##-Inf ~@x `y #?(:clj 1) #'v #\"re\" \\ # ;c",
        "\u{1F600} :ü/ö \"unterminated",
    ];
    for sample in samples {
        let mut boundaries: Vec<usize> = sample.char_indices().map(|(i, _)| i).collect();
        boundaries.push(sample.len());
        for &end in &boundaries {
            let prefix = &sample[..end];
            let tokens = tokenize(prefix);
            let mut covered = 0;
            for t in &tokens {
                assert_eq!(t.span.start, covered, "gap in {prefix:?}");
                covered = t.span.end;
            }
            assert_eq!(covered, prefix.len(), "{prefix:?}");
            let _ = parse(prefix);
            let _ = parse_value(prefix);
        }
    }
}

#[test]
fn highlighting_tracks_delimiter_depth() {
    let classes: Vec<Class> = highlight("{:a [1]}")
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| *c != Class::Plain)
        .collect();
    assert_eq!(
        classes,
        [
            Class::Delimiter(0),
            Class::Keyword,
            Class::Delimiter(1),
            Class::Number,
            Class::Delimiter(1),
            Class::Delimiter(0),
        ]
    );
}
