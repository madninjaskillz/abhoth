//! Port of tests/OpenOrca.Tools.Tests/PiTestSummaryTests.cs.

use orca_tools_pi::test_summary;

#[test]
fn unittest_names_failures_and_errors_with_the_result_line() {
    let output = [
        "..F.E",
        "======",
        "FAIL: test_total (tests.test_sales.SalesTest.test_total)",
        "Traceback ...",
        "ERROR: test_load (tests.test_io.IoTest)",
        "Traceback ...",
        "FAIL: test_total (tests.test_sales.SalesTest.test_total)",
        "----",
        "Ran 12 tests in 0.042s",
        "",
        "FAILED (failures=1, errors=1)",
    ]
    .join("\n");
    assert_eq!(
        test_summary::from(&output).as_deref(),
        Some(
            "[Test summary (whole output): FAILED (failures=1, errors=1) after Ran 12 tests in 0.042s. \
             Failing: test_total (tests.test_sales.SalesTest.test_total), test_load (tests.test_io.IoTest)]"
        )
    );
}

#[test]
fn unittest_passing() {
    assert_eq!(
        test_summary::from("...\r\n----\r\nRan 3 tests in 0.001s\r\n\r\nOK\r\n").as_deref(),
        Some("[Test summary (whole output): OK after Ran 3 tests in 0.001s]")
    );
}

#[test]
fn pytest_short_summary_and_verbose_lines() {
    let output = [
        "tests/test_a.py::test_one PASSED",
        "tests/test_a.py::test_two FAILED",
        "=== short test summary info ===",
        "FAILED tests/test_a.py::test_two - assert 1 == 2",
        "ERROR tests/test_b.py::test_setup",
        "===== 1 failed, 1 passed, 1 error in 0.12s =====",
    ]
    .join("\n");
    assert_eq!(
        test_summary::from(&output).as_deref(),
        Some(
            "[Test summary (whole output): 1 failed, 1 passed, 1 error in 0.12s. Failing: tests/test_a.py::test_two, tests/test_b.py::test_setup]"
        )
    );
}

#[test]
fn many_failures_are_capped() {
    let mut lines: Vec<String> = (1..=20).map(|i| format!("FAIL: test_{i} (t.T)")).collect();
    lines.push("Ran 20 tests in 1s".into());
    lines.push("FAILED (failures=20)".into());
    let s = test_summary::from(&lines.join("\n")).expect("a summary");
    assert!(s.contains("test_12 (t.T) (+8 more)]"), "{s}");
    assert!(!s.contains("test_13"), "{s}");
}

#[test]
fn not_a_test_run_gives_null() {
    for output in [
        "1\n2\n3",
        "OK", // no "Ran N tests" before it
        "FAIL: test_a (t.T)\nstill running",
    ] {
        assert_eq!(test_summary::from(output), None, "{output:?}");
    }
}
