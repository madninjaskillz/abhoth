//! Ported from BenchmarkRunnerTests.cs, plus the argument checks of `HandleBenchmarkAsync`.
use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

const ALL_PRIMES: &str = "2\n3\n5\n7\n11\n13\n17\n19\n23\n29\n31\n37\n41\n43\n47\n53\n59\n61\n67\n71\n73\n79\n83\n89\n97\n";

#[test]
fn defaults_are_three_runs_of_the_current_model() {
    assert_eq!(
        parse_args(&[]),
        Ok(BenchmarkArgs {
            runs_per_model: 3,
            model_filter: None,
            mode_pair: false
        })
    );
}

#[test]
fn count_models_and_pair_mode_are_read() {
    let p = parse_args(&args(&["COUNT=5", "models=a, b,,c"])).unwrap();
    assert_eq!(p.runs_per_model, 5);
    assert_eq!(
        p.model_filter,
        Some(vec!["a".into(), "b".into(), "c".into()])
    );
    let p = parse_args(&args(&["models=ALL"])).unwrap();
    assert_eq!(p.model_filter, Some(vec![]));
    let p = parse_args(&args(&["mode=Pair"])).unwrap();
    assert!(p.mode_pair);
    assert_eq!(p.model_filter, Some(vec![]));
}

#[test]
fn bad_arguments_say_what_is_wrong() {
    let (console, out) = Console::buffer();
    for bad in ["count=0", "count=x", "models=,", "mode=solo", "fast"] {
        assert_eq!(parse_or_report(&console, &args(&[bad])), None);
    }
    let text = Console::buffer_text(&out);
    assert!(text.contains("Invalid count — must be a positive integer (e.g. count=5)."));
    assert!(
        text.contains("Invalid models — provide comma-separated names or 'all' (e.g. models=all).")
    );
    assert!(text.contains("Unknown mode: solo. Supported: pair"));
    assert!(text.contains("Unknown argument: fast"));
    assert!(text.contains("Usage: /benchmark [count=N] [models=all|name1,name2] [mode=pair]"));
    assert!(parse_or_report(&console, &args(&["count=2"])).is_some());
}

// ── ValidateOutput ──

#[test]
fn validate_output_all_primes_correct_returns_pass() {
    let (correct, notes) = validate_output(ALL_PRIMES);
    assert!(correct);
    assert!(notes.contains("25 primes correct"));
}

#[test]
fn validate_output_missing_primes_returns_fail() {
    let (correct, notes) = validate_output("2\n3\n5\n7\n11\n");
    assert!(!correct);
    assert!(notes.contains("Missing:"));
    assert!(notes.contains("97"));
}

#[test]
fn validate_output_extra_numbers_returns_fail() {
    let (correct, notes) = validate_output(&format!("{ALL_PRIMES}100\n"));
    assert!(!correct);
    assert!(notes.contains("Extra:"));
    assert!(notes.contains("100"));
    assert_eq!(notes, "Extra: 100");
}

#[test]
fn validate_output_empty_file_returns_fail() {
    let (correct, notes) = validate_output("");
    assert!(!correct);
    assert!(notes.contains("empty"));
}

#[test]
fn validate_output_whitespace_only_returns_fail() {
    let (correct, notes) = validate_output("   \n\n  ");
    assert!(!correct);
    assert!(notes.contains("empty"));
}

#[test]
fn validate_output_no_numbers_returns_fail() {
    let (correct, notes) = validate_output("hello\nworld\n");
    assert!(!correct);
    assert!(notes.contains("No numbers"));
}

#[test]
fn validate_output_windows_line_endings_works() {
    let (correct, _) = validate_output(&ALL_PRIMES.replace('\n', "\r\n"));
    assert!(correct);
}

#[test]
fn validate_output_lists_missing_and_extra_once_each() {
    let (_, notes) = validate_output("2\n2\n4\n4\n");
    assert!(notes.starts_with("Missing: 3, 5, 7"));
    assert!(notes.ends_with("; Extra: 4"));
}

// ── FindPythonEntryPoint ──

#[test]
fn find_python_entry_point_main_py_exists_returns_main_py() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "print('hello')").unwrap();
    std::fs::write(dir.path().join("other.py"), "print('other')").unwrap();
    assert_eq!(
        find_python_entry_point(dir.path()),
        Some(dir.path().join("main.py"))
    );
}

#[test]
fn find_python_entry_point_single_py_file_returns_that_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "print('hello')").unwrap();
    assert!(
        find_python_entry_point(dir.path())
            .unwrap()
            .ends_with("app.py")
    );
}

#[test]
fn find_python_entry_point_multiple_candidates_returns_null() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "print('hello')").unwrap();
    std::fs::write(dir.path().join("utils.py"), "print('utils')").unwrap();
    assert_eq!(find_python_entry_point(dir.path()), None);
}

#[test]
fn find_python_entry_point_test_files_excluded() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("app.py", "print('hello')"),
        ("test.py", "assert True"),
        ("test_app.py", "assert True"),
        ("app_test.py", "assert True"),
        ("setup.py", "setup()"),
        ("conftest.py", ""),
    ] {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    assert!(
        find_python_entry_point(dir.path())
            .unwrap()
            .ends_with("app.py")
    );
}

#[test]
fn find_python_entry_point_no_py_files_returns_null() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();
    assert_eq!(find_python_entry_point(dir.path()), None);
}

// ── SanitizeModelName ──

#[test]
fn sanitize_model_name_replaces_invalid_chars() {
    for (input, expected) in [
        ("gpt-4", "gpt-4"),
        ("mistral-7b-instruct-v0.3", "mistral-7b-instruct-v0_3"),
        ("model/with/slashes", "model_with_slashes"),
        ("MODEL-Name", "model-name"),
    ] {
        assert_eq!(sanitize_model_name(input), expected);
    }
}

#[test]
fn sanitize_model_name_truncates_long_names() {
    assert_eq!(sanitize_model_name(&"a".repeat(100)).len(), 50);
}

// ── GenerateOrderedPairs ──

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| s.to_string()).collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

#[test]
fn generate_ordered_pairs_two_models_returns_two_pairs() {
    let pairs = generate_ordered_pairs(&names(&["A", "B"]));
    assert_eq!(pairs.len(), 2);
    assert!(pairs.contains(&pair("A", "B")));
    assert!(pairs.contains(&pair("B", "A")));
}

#[test]
fn generate_ordered_pairs_three_models_returns_six_pairs() {
    let pairs = generate_ordered_pairs(&names(&["A", "B", "C"]));
    assert_eq!(pairs.len(), 6);
    for (a, b) in [
        ("A", "B"),
        ("A", "C"),
        ("B", "A"),
        ("B", "C"),
        ("C", "A"),
        ("C", "B"),
    ] {
        assert!(pairs.contains(&pair(a, b)));
    }
}

#[test]
fn generate_ordered_pairs_four_models_returns_twelve_pairs() {
    let pairs = generate_ordered_pairs(&names(&["A", "B", "C", "D"]));
    assert_eq!(pairs.len(), 12);
    assert!(pairs.iter().all(|(a, b)| a != b));
}

#[test]
fn generate_ordered_pairs_single_model_returns_empty() {
    assert!(generate_ordered_pairs(&names(&["A"])).is_empty());
}

// PairBenchmarkRunResult_InheritsFromBenchmarkRunResult: no inheritance in Rust; the pair
// result holds a `RunResult`.
#[test]
fn pair_benchmark_run_result_holds_a_run_result() {
    let result = PairRunResult {
        primary_model: "model-a".into(),
        secondary_model: "model-b".into(),
        run: RunResult {
            llm_time_ms: 5000,
            python_exec_time_ms: 100,
            output_correct: Some(true),
            notes: Some("All 25 primes correct".into()),
        },
        pair_primary_wins: 2,
        ..Default::default()
    };
    assert_eq!(result.run.llm_time_ms, 5000);
    assert_eq!(result.primary_model, "model-a");
    assert_eq!(result.secondary_model, "model-b");
    assert_eq!(result.pair_primary_wins, 2);
}

// ReplState_ResetPairStats_ClearsAllCounters: the tallies live in `AgentSession.pair_stats`
// (a plain struct reset with `Default`), covered by the pair benchmark path.

// ── Model selection and tables ──

#[test]
fn select_models_matches_case_insensitively_and_reports_misses() {
    let (console, out) = Console::buffer();
    let available = names(&["Model-A", "model-b"]);
    assert_eq!(
        select_models(&console, &available, &names(&["model-a", "zzz"]), 1),
        Some(names(&["Model-A"]))
    );
    assert_eq!(
        select_models(&console, &available, &names(&["zzz"]), 1),
        None
    );
    assert_eq!(
        select_models(&console, &available, &names(&["model-a"]), 2),
        None
    );
    assert_eq!(select_models(&console, &[], &[], 1), None);
    assert_eq!(select_models(&console, &available[..1], &[], 2), None);
    assert_eq!(
        select_models(&console, &available, &[], 2),
        Some(available.clone())
    );
    let text = Console::buffer_text(&out);
    assert!(text.contains("Model not found: zzz — skipping"));
    assert!(text.contains("None of the requested models are available."));
    assert!(text.contains("Not enough matched models for this benchmark mode."));
    assert!(text.contains("Available: Model-A, model-b"));
    assert!(
        text.contains(
            "No models found on the server. Is the LLM server running with models loaded?"
        )
    );
    assert!(text.contains("Requires at least 2 models. Found: 1"));
    assert!(text.contains("Benchmarking 2 model(s): Model-A, model-b"));
}

#[test]
fn results_tables_show_runs_averages_and_the_best_pair() {
    let (console, out) = Console::buffer();
    let pass = RunResult {
        llm_time_ms: 2500,
        python_exec_time_ms: 40,
        output_correct: Some(true),
        notes: Some("All 25 primes correct".into()),
    };
    let fail = RunResult {
        llm_time_ms: 3500,
        python_exec_time_ms: 0,
        output_correct: Some(false),
        notes: Some("output.txt not found".into()),
    };
    render_results_table(
        &console,
        &[ModelResult {
            model_name: "m1".into(),
            runs: vec![pass.clone(), fail.clone()],
        }],
    );
    render_pair_results_table(
        &console,
        &[
            PairRunResult {
                primary_model: "a".into(),
                secondary_model: "b".into(),
                run: fail,
                pair_backup_failures: 1,
                ..Default::default()
            },
            PairRunResult {
                primary_model: "b".into(),
                secondary_model: "a".into(),
                run: pass,
                pair_primary_wins: 3,
                ..Default::default()
            },
        ],
    );
    let text = Console::buffer_text(&out);
    assert!(text.contains("Benchmark Results"));
    assert!(text.contains("2.5s") && text.contains("3.5s") && text.contains("40ms"));
    assert!(text.contains("3.0s"), "average LLM time: {text}");
    assert!(text.contains("1/2"));
    assert!(text.contains("PASS") && text.contains("FAIL"));
    assert!(text.contains("Pair Benchmark Results"));
    assert!(text.contains("0/0/0 (1 fail)"));
    assert!(text.contains("3/0/0"));
    assert!(text.contains("Best pair: b + a (2.5s)"));
}

#[test]
fn rule_puts_the_title_left_or_centred() {
    let left = crate::ui::widgets::rule_line("Title", true, 20);
    assert_eq!(left, "── Title ───────────");
    let centred = crate::ui::widgets::rule_line("Title", false, 20);
    assert_eq!(centred, "────── Title ───────");
}
