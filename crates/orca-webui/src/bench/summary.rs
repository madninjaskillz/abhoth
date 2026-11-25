//! `BenchSummary`: the summary table, every run of one task on one arm (and one target when the
//! suite compares targets). Scored runs reached the checker; passes and medians are over those.
//! A run that was stopped or errored first counts in `runs` only.

use serde::Serialize;
use serde_json::Value;

use super::result_row::BenchResultRow;
use super::time::round;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchSummaryRow {
    pub task: String,
    pub arm: String,
    pub target: Option<String>,
    pub runs: i32,
    pub scored: i32,
    pub passes: i32,
    pub median_wall_seconds: Option<f64>,
    pub median_steps: Option<f64>,
    pub median_prompt_tokens: Option<f64>,
    pub median_output_tokens: Option<f64>,
    pub median_reasoning_tokens: Option<f64>,
    pub median_tool_tokens: Option<f64>,
    pub median_content_tokens: Option<f64>,
}

/// Results rows grouped by task × arm (× target when `by_target`), in order of first
/// appearance. `batch` / `suite` narrow the rows first.
pub fn compute(
    rows: &[Value],
    batch: Option<&str>,
    suite: Option<&str>,
    by_target: bool,
) -> Vec<BenchSummaryRow> {
    type Group = (String, String, Option<String>, Vec<BenchResultRow>);
    let mut groups: Vec<Group> = Vec::new();
    for el in rows {
        let Some(r) = BenchResultRow::parse(el) else {
            continue;
        };
        if batch.is_some_and(|b| r.batch.as_deref() != Some(b))
            || suite.is_some_and(|s| r.suite != s)
        {
            continue;
        }
        let target = if by_target { r.target.clone() } else { None };
        match groups
            .iter_mut()
            .find(|g| g.0 == r.task && g.1 == r.arm && g.2 == target)
        {
            Some(g) => g.3.push(r),
            None => groups.push((r.task.clone(), r.arm.clone(), target, vec![r])),
        }
    }
    groups
        .into_iter()
        .map(|(task, arm, target, rows)| {
            let scored: Vec<&BenchResultRow> = rows.iter().filter(|r| r.pass.is_some()).collect();
            let m = |f: &dyn Fn(&BenchResultRow) -> Option<f64>| {
                median(scored.iter().map(|r| f(r)).collect())
            };
            BenchSummaryRow {
                task,
                arm,
                target,
                runs: rows.len() as i32,
                scored: scored.len() as i32,
                passes: scored.iter().filter(|r| r.pass == Some(true)).count() as i32,
                median_wall_seconds: m(&|r| r.wall_seconds).map(|w| round(w, 1)),
                median_steps: m(&|r| r.requests.map(|v| v as f64)),
                median_prompt_tokens: m(&|r| r.prompt_tokens.map(|v| v as f64)),
                median_output_tokens: m(&|r| r.output_tokens.map(|v| v as f64)),
                median_reasoning_tokens: m(&|r| r.reasoning_tokens.map(|v| v as f64)),
                median_tool_tokens: m(&|r| r.tool_tokens.map(|v| v as f64)),
                median_content_tokens: m(&|r| r.content_tokens.map(|v| v as f64)),
            }
        })
        .collect()
}

/// The middle value (the mean of the two middle ones for an even count); `None` when there
/// are none.
pub fn median(values: Vec<Option<f64>>) -> Option<f64> {
    let mut v: Vec<f64> = values.into_iter().flatten().collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::too_many_arguments)]
    fn row(
        task: &str,
        arm: &str,
        status: &str,
        pass: Option<bool>,
        wall: Option<f64>,
        steps: Option<i64>,
        reasoning: Option<i64>,
        output: Option<i64>,
        batch: &str,
    ) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), json!(crate::util::short_id(32)));
        m.insert("suite".into(), json!("s"));
        m.insert("task".into(), json!(task));
        m.insert("arm".into(), json!(arm));
        m.insert("rep".into(), json!(1));
        m.insert("status".into(), json!(status));
        m.insert("batch".into(), json!(batch));
        for (k, v) in [
            ("pass", pass.map(|p| json!(p))),
            ("wall_s", wall.map(|p| json!(p))),
            ("requests", steps.map(|p| json!(p))),
            ("reasoning_tokens", reasoning.map(|p| json!(p))),
            ("output_tokens", output.map(|p| json!(p))),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), v);
            }
        }
        Value::Object(m)
    }

    #[test]
    fn summary_counts_passes_and_takes_medians() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![None, None]), None);
        assert_eq!(median(vec![Some(5.0)]), Some(5.0));
        assert_eq!(median(vec![Some(9.0), Some(1.0), Some(3.0)]), Some(3.0));
        assert_eq!(
            median(vec![Some(4.0), None, Some(1.0), Some(2.0), Some(3.0)]),
            Some(2.5)
        );

        let rows = vec![
            row(
                "t3",
                "base",
                "passed",
                Some(true),
                Some(100.0),
                Some(10),
                Some(5000),
                Some(9000),
                "b1",
            ),
            row(
                "t3",
                "cap",
                "passed",
                Some(true),
                Some(80.0),
                Some(12),
                Some(2000),
                Some(6000),
                "b1",
            ),
            row(
                "t3",
                "base",
                "failed",
                Some(false),
                Some(300.0),
                Some(30),
                Some(9000),
                Some(20000),
                "b1",
            ),
            row(
                "t3",
                "base",
                "passed",
                Some(true),
                Some(200.0),
                Some(20),
                Some(7000),
                Some(10000),
                "b1",
            ),
            row(
                "t3",
                "base",
                "stopped",
                None,
                Some(40.0),
                None,
                None,
                None,
                "b1",
            ), // counted, not scored
            row(
                "gems",
                "base",
                "timeout",
                Some(false),
                Some(1200.0),
                Some(44),
                Some(100),
                Some(200),
                "b1",
            ),
            row(
                "t3",
                "base",
                "passed",
                Some(true),
                Some(1.0),
                Some(1),
                Some(1),
                Some(1),
                "b0",
            ), // another batch
            json!("not a row"),
        ];
        let s = compute(&rows, Some("b1"), None, false);
        let keys: Vec<(&str, &str)> = s
            .iter()
            .map(|x| (x.task.as_str(), x.arm.as_str()))
            .collect();
        assert_eq!(keys, [("t3", "base"), ("t3", "cap"), ("gems", "base")]);
        let t3 = &s[0];
        assert_eq!((t3.runs, t3.scored, t3.passes), (4, 3, 2));
        assert_eq!(t3.median_wall_seconds, Some(200.0)); // the stopped run's 40 s is left out
        assert_eq!(t3.median_steps, Some(20.0));
        assert_eq!(t3.median_reasoning_tokens, Some(7000.0));
        assert_eq!(t3.median_output_tokens, Some(10000.0));
        assert_eq!(t3.median_tool_tokens, None);
        assert_eq!((s[2].runs, s[2].scored, s[2].passes), (1, 1, 0));
        assert_eq!(compute(&rows, None, Some("s"), false)[0].runs, 5);
    }
}
