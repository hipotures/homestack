//! Account quota observations, independent of project and workstation token accounting.
use crate::{
    datasets::{Availability, Dataset, Selection, TimeRange, hash},
    prometheus::{Prometheus, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
const SELECTOR: &str = "{__name__=~\"agent_account_limit_(used_percent|reset_timestamp_seconds|window_minutes|exhausted)|agent_account_(enabled|measurement_stale|last_success_timestamp_seconds)\"}";
pub async fn read(
    prom: &Prometheus,
    selection: &Selection,
    range: &TimeRange,
    timestamp: u64,
) -> Result<(Dataset, Vec<Arc<Source>>), String> {
    let step = range.step.max(300).max((range.end - range.start) / 500);
    let provider = if selection.agent == "all" {
        String::new()
    } else {
        format!(r#"{{provider="{}"}}"#, selection.agent)
    };
    let current_query = if selection.agent == "all" {
        SELECTOR.to_owned()
    } else {
        format!(r#"{},provider="{}"}}"#, SELECTOR.trim_end_matches('}'), selection.agent)
    };
    // Preserve short exhaustion observations when a long view uses coarse buckets.
    let history_query = ["used_percent", "exhausted", "reset_timestamp_seconds", "window_minutes"]
        .into_iter()
        .map(|name| {
            let metric = format!("agent_account_limit_{name}");
            let aggregate = if matches!(name, "used_percent" | "exhausted") {
                "max_over_time"
            } else {
                "last_over_time"
            };
            format!(r#"label_replace({aggregate}({metric}{provider}[{step}s]),"__name__","{metric}","","")"#)
        })
        .collect::<Vec<_>>()
        .join(" or ");
    let history = prom
        .read(
            "/api/v1/query_range",
            vec![
                ("query".into(), history_query),
                ("start".into(), range.start.to_string()),
                ("end".into(), timestamp.to_string()),
                ("step".into(), step.to_string()),
                ("timeout".into(), "20s".into()),
            ],
            30,
        )
        .await?;
    let current = prom
        .read(
            "/api/v1/query",
            vec![
                ("query".into(), current_query),
                ("time".into(), timestamp.to_string()),
                ("timeout".into(), "20s".into()),
            ],
            30,
        )
        .await?;
    let mut windows = BTreeMap::<(String, String, String), Value>::new();
    let mut accounts = BTreeMap::<(String, String), BTreeMap<String, f64>>::new();
    for (source, live) in [(&history, false), (&current, true)] {
        for row in source.data["result"].as_array().into_iter().flatten() {
            let labels = &row["metric"];
            let provider = labels["provider"].as_str().unwrap_or("");
            if !["claude", "codex", "antigravity"].contains(&provider)
                || (selection.agent != "all" && selection.agent != provider)
            {
                continue;
            }
            let profile = labels["profile_id"].as_str().unwrap_or("");
            let metric = labels["__name__"].as_str().unwrap_or("");
            let points: Vec<(f64, f64)> = if live {
                row.get("value").into_iter().filter_map(pair).collect()
            } else {
                row["values"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(pair)
                    .collect()
            };
            if !metric.starts_with("agent_account_limit_") {
                if live && let Some((_, v)) = points.last() {
                    accounts
                        .entry((provider.into(), profile.into()))
                        .or_default()
                        .insert(metric.into(), *v);
                }
                continue;
            }
            let window = labels["window"].as_str().unwrap_or("");
            let value=windows.entry((provider.into(),profile.into(),window.into())).or_insert_with(||json!({"agent":provider,"profile":profile,"window":window,"label":labels["window_label"].as_str().unwrap_or(window),"group":labels["model"].as_str().unwrap_or(""),"history":{},"current":{}}));
            let name = metric.trim_start_matches("agent_account_limit_");
            if live {
                if let Some((_, v)) = points.last() {
                    value["current"][name] = json!(v);
                }
            } else {
                value["history"][name] = json!(points);
            }
        }
    }
    for ((provider, profile, _), value) in &mut windows {
        let account = accounts.get(&(provider.clone(), profile.clone()));
        let fresh = account.is_some_and(|a| {
            a.get("agent_account_enabled") == Some(&1.0)
                && a.get("agent_account_measurement_stale") == Some(&0.0)
                && a.get("agent_account_last_success_timestamp_seconds")
                    .is_some_and(|t| *t <= timestamp as f64 && timestamp as f64 - *t <= 900.0)
        });
        if !fresh {
            value["current"] = Value::Null;
        }
    }
    let mut dataset=Dataset{id:"all.limits".into(),measurement:"limits".into(),agent:"all".into(),unit:"percent".into(),freshness_key:"limits".into(),scope:selection.clone(),range:range.clone(),state:if windows.is_empty(){Availability::Missing}else{Availability::Available},series:vec![],total:None,details:json!({"windows":windows.into_values().collect::<Vec<_>>(),"sample_step":step,"account_scope":true}),warnings:vec!["Account-wide quota observations are independent of host and project filters. Exhaustion markers show the first exhausted observation, not the exact time of a rejected request. Missing telemetry is not zero.".into()],version:String::new()};
    if serde_json::to_vec(&dataset).map_err(|e| e.to_string())?.len() > 192 * 1024 {
        return Err("Quota history exceeds the bounded overlay budget; select one agent or a shorter range".into());
    }
    dataset.seal();
    Ok((dataset, vec![history, current]))
}
fn pair(v: &Value) -> Option<(f64, f64)> {
    let t = v[0].as_f64()?;
    let n = v[1].as_str()?.parse::<f64>().ok()?;
    (t.is_finite() && n.is_finite()).then_some((t, n))
}
pub fn unavailable(selection: &Selection, range: &TimeRange, error: String) -> Dataset {
    let mut d = Dataset {
        id: "all.limits".into(),
        measurement: "limits".into(),
        agent: "all".into(),
        unit: "percent".into(),
        freshness_key: "limits".into(),
        scope: selection.clone(),
        range: range.clone(),
        state: Availability::Unavailable,
        series: vec![],
        total: None,
        details: json!({"windows":[]}),
        warnings: vec![error],
        version: hash(&"unavailable"),
    };
    d.seal();
    d
}
